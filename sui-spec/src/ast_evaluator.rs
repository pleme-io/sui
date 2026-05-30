//! Minimum-viable tree-walking evaluator over [`AstGraph`] — the
//! engine that drives the module-system solver's
//! [`BodyEvaluator`](crate::module_solver::BodyEvaluator) impl today.
//!
//! ## Scope
//!
//! Handles the AST kinds needed to fully evaluate **setter body
//! expressions** in typical NixOS modules: literals, identifier
//! lookups via env, dotted attrset selects rooted at `config`, the
//! standard binary + unary operators, conditional expressions, lists,
//! and attrset construction.
//!
//! What this does **not** cover (intentional — those land when the
//! sui-eval bytecode VM integration replaces this minimum-viable
//! engine):
//!
//! * `Apply` (function calls) — `lib.mkOption {...}`, `pkgs.callPackage
//!   ...`, etc. Module bodies that compute setter values via library
//!   calls fall back to [`EvalValue::Opaque`] today (a sentinel that
//!   carries the AST node id for the eventual VM-backed re-evaluation).
//! * `Lambda` and `LetIn` — closures and local bindings. Reported as
//!   `Opaque` for the same reason.
//! * `With` — runtime scope manipulation. Same.
//! * Full string interpolation — segments evaluated to interpolated
//!   sub-values are concatenated when all parts evaluate strict-ly to
//!   strings; otherwise the whole string is `Opaque`.
//! * Type coercions (Int↔Float, Path↔String) — only when both sides
//!   are the same type. Mixed-type arithmetic returns
//!   `EvalError::TypeMismatch`.
//!
//! ## Why this exists separately from sui-eval
//!
//! The sui-eval crate's tree-walker + bytecode VM both take **source
//! text** as input (rnix CST → typed AST → bytecode). The L4 substrate
//! works in the opposite direction: it has the typed AST already (in
//! AstGraph form) and needs to evaluate sub-expressions of it directly,
//! WITHOUT round-tripping back through source. That's structurally a
//! different shape: walk the AstNodeKind variants, with the env as a
//! BTreeMap of pre-evaluated values.
//!
//! Future ship: sui-eval grows an AstGraph-input mode (its tree-walker
//! gains a constructor that takes a pre-built typed AST), and this
//! module either becomes a thin wrapper around that or is sunset in
//! favor of the unified evaluator.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ast_graph::{AstGraph, AstNodeKind, BinaryOp, NodeId, StrSegment, UnaryOp};

/// Typed value the evaluator produces. Mirrors the Nix value lattice
/// for the kinds we support today.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EvalValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    Str(String),
    Path(String),
    List(Vec<EvalValue>),
    AttrSet(BTreeMap<String, EvalValue>),
    /// Fallback for kinds we don't model yet (Apply, Lambda, LetIn,
    /// With, complex Select chains). Carries the original AST node id
    /// so the eventual VM-backed re-evaluation can pick up where we
    /// left off. `kind` is the [`AstNodeKind`] variant name (`"Apply"`,
    /// `"Lambda"`, …) — owned so EvalValue is `DeserializeOwned`.
    Opaque {
        kind: String,
        node_id: NodeId,
    },
}

/// Errors the evaluator surfaces.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("type mismatch in {context}: expected {expected}, got {got}")]
    TypeMismatch {
        context: &'static str,
        expected: &'static str,
        got: &'static str,
    },

    #[error("division by zero")]
    DivisionByZero,

    #[error("undefined identifier: {0}")]
    UndefinedIdent(String),

    #[error("config select walked off the env at {path:?}")]
    ConfigMiss { path: Vec<String> },

    #[error("attempted to recurse past max depth ({0})")]
    DepthExceeded(u32),
}

/// Read-only environment threaded through the walker. Maps identifier
/// → its already-evaluated [`EvalValue`]. Setter bodies that reference
/// `config` look it up here; the caller seeds `config` from the
/// solver's [`crate::module_solver::EnvSnapshot`].
#[derive(Debug, Default, Clone)]
pub struct EvalEnv {
    pub bindings: BTreeMap<String, EvalValue>,
}

impl EvalEnv {
    /// New empty env.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind one identifier to a value.
    #[must_use]
    pub fn with_binding(mut self, name: impl Into<String>, value: EvalValue) -> Self {
        self.bindings.insert(name.into(), value);
        self
    }

    /// Read a binding by name.
    pub fn get(&self, name: &str) -> Option<&EvalValue> {
        self.bindings.get(name)
    }
}

/// Hard cap on recursion depth — guards against pathological inputs.
/// Real-world module bodies are shallow (single digits typical); 256
/// is more than enough.
const MAX_DEPTH: u32 = 256;

/// Evaluate an AST node id in the given env.
///
/// # Errors
///
/// See [`EvalError`].
pub fn eval_node(ast: &AstGraph, id: NodeId, env: &EvalEnv) -> Result<EvalValue, EvalError> {
    eval_at(ast, id, env, 0)
}

fn eval_at(
    ast: &AstGraph,
    id: NodeId,
    env: &EvalEnv,
    depth: u32,
) -> Result<EvalValue, EvalError> {
    if depth >= MAX_DEPTH {
        return Err(EvalError::DepthExceeded(MAX_DEPTH));
    }
    let node = &ast.nodes[id as usize];
    match &node.kind {
        AstNodeKind::Int(n) => Ok(EvalValue::Int(*n)),
        AstNodeKind::Float(f) => Ok(EvalValue::Float(*f)),
        AstNodeKind::Bool(b) => Ok(EvalValue::Bool(*b)),
        AstNodeKind::Null => Ok(EvalValue::Null),
        AstNodeKind::Path(p) => Ok(EvalValue::Path(p.clone())),
        AstNodeKind::Str { segments } | AstNodeKind::IndentedStr { segments } => {
            eval_string_segments(ast, segments, env, depth + 1, id)
        }
        AstNodeKind::Ident(name) => env
            .get(name)
            .cloned()
            .ok_or_else(|| EvalError::UndefinedIdent(name.clone())),
        AstNodeKind::Select {
            target,
            path,
            fallback,
        } => eval_select(ast, *target, path, *fallback, env, depth + 1),
        AstNodeKind::HasAttr { target, path } => {
            eval_has_attr(ast, *target, path, env, depth + 1)
        }
        AstNodeKind::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(eval_at(ast, *item, env, depth + 1)?);
            }
            Ok(EvalValue::List(out))
        }
        AstNodeKind::AttrSet { entries, .. } => {
            let mut out: BTreeMap<String, EvalValue> = BTreeMap::new();
            for entry in entries {
                let value = eval_at(ast, entry.value, env, depth + 1)?;
                insert_at_path(&mut out, &entry.path, value);
            }
            Ok(EvalValue::AttrSet(out))
        }
        AstNodeKind::IfThenElse {
            condition,
            then_branch,
            else_branch,
        } => {
            let c = eval_at(ast, *condition, env, depth + 1)?;
            match c {
                EvalValue::Bool(true) => eval_at(ast, *then_branch, env, depth + 1),
                EvalValue::Bool(false) => eval_at(ast, *else_branch, env, depth + 1),
                other => Err(EvalError::TypeMismatch {
                    context: "if-then-else condition",
                    expected: "bool",
                    got: value_kind(&other),
                }),
            }
        }
        AstNodeKind::BinOp { op, left, right } => {
            let l = eval_at(ast, *left, env, depth + 1)?;
            let r = eval_at(ast, *right, env, depth + 1)?;
            eval_binop(*op, l, r)
        }
        AstNodeKind::UnaryOp { op, operand } => {
            let v = eval_at(ast, *operand, env, depth + 1)?;
            eval_unaryop(*op, v)
        }
        // Kinds we don't fully model yet — surface as Opaque carrying
        // the AST node id so the future VM-backed evaluator can pick up.
        AstNodeKind::Apply { .. } => Ok(EvalValue::Opaque {
            kind: "Apply".to_string(),
            node_id: id,
        }),
        AstNodeKind::Lambda { .. } => Ok(EvalValue::Opaque {
            kind: "Lambda".to_string(),
            node_id: id,
        }),
        AstNodeKind::LetIn { .. } => Ok(EvalValue::Opaque {
            kind: "LetIn".to_string(),
            node_id: id,
        }),
        AstNodeKind::With { .. } => Ok(EvalValue::Opaque {
            kind: "With".to_string(),
            node_id: id,
        }),
        AstNodeKind::Assert { body, .. } => {
            // Assertion: ignored today (would need to evaluate the
            // condition and throw on false). Just evaluate the body.
            eval_at(ast, *body, env, depth + 1)
        }
        AstNodeKind::Unknown { kind, .. } => Ok(EvalValue::Opaque {
            kind: kind.clone(),
            node_id: id,
        }),
    }
}

fn eval_string_segments(
    ast: &AstGraph,
    segments: &[StrSegment],
    env: &EvalEnv,
    depth: u32,
    fallback_id: NodeId,
) -> Result<EvalValue, EvalError> {
    let mut out = String::new();
    for s in segments {
        match s {
            StrSegment::Literal(t) => out.push_str(t),
            StrSegment::Interpolation(child) => {
                let v = eval_at(ast, *child, env, depth)?;
                match v {
                    EvalValue::Str(s) => out.push_str(&s),
                    EvalValue::Int(n) => out.push_str(&n.to_string()),
                    EvalValue::Float(f) => out.push_str(&f.to_string()),
                    EvalValue::Bool(b) => out.push_str(if b { "1" } else { "" }),
                    EvalValue::Path(p) => out.push_str(&p),
                    // Complex values inside string interpolation
                    // become opaque — fall back to the whole string
                    // being opaque so the eventual VM-backed reval
                    // can recompute correctly.
                    _ => {
                        return Ok(EvalValue::Opaque {
                            kind: "Str-with-complex-interp".to_string(),
                            node_id: fallback_id,
                        });
                    }
                }
            }
        }
    }
    Ok(EvalValue::Str(out))
}

fn eval_select(
    ast: &AstGraph,
    target: NodeId,
    path: &[String],
    fallback: Option<NodeId>,
    env: &EvalEnv,
    depth: u32,
) -> Result<EvalValue, EvalError> {
    let base = eval_at(ast, target, env, depth)?;
    let result = follow_path(&base, path);
    match result {
        Some(v) => Ok(v),
        None => {
            if let Some(fb) = fallback {
                eval_at(ast, fb, env, depth)
            } else {
                Err(EvalError::ConfigMiss { path: path.to_vec() })
            }
        }
    }
}

fn eval_has_attr(
    ast: &AstGraph,
    target: NodeId,
    path: &[String],
    env: &EvalEnv,
    depth: u32,
) -> Result<EvalValue, EvalError> {
    let base = eval_at(ast, target, env, depth)?;
    Ok(EvalValue::Bool(follow_path(&base, path).is_some()))
}

fn follow_path(value: &EvalValue, path: &[String]) -> Option<EvalValue> {
    let mut cursor = value.clone();
    for step in path {
        match cursor {
            EvalValue::AttrSet(map) => match map.get(step) {
                Some(v) => cursor = v.clone(),
                None => return None,
            },
            _ => return None,
        }
    }
    Some(cursor)
}

fn insert_at_path(
    out: &mut BTreeMap<String, EvalValue>,
    path: &[String],
    value: EvalValue,
) {
    if path.is_empty() {
        return;
    }
    if path.len() == 1 {
        out.insert(path[0].clone(), value);
        return;
    }
    let head = &path[0];
    let tail = &path[1..];
    let entry = out
        .entry(head.clone())
        .or_insert_with(|| EvalValue::AttrSet(BTreeMap::new()));
    if let EvalValue::AttrSet(inner) = entry {
        insert_at_path(inner, tail, value);
    }
}

fn eval_binop(op: BinaryOp, l: EvalValue, r: EvalValue) -> Result<EvalValue, EvalError> {
    use EvalValue::*;
    match (op, &l, &r) {
        // Arithmetic — integer/integer
        (BinaryOp::Add, Int(a), Int(b)) => Ok(Int(a + b)),
        (BinaryOp::Sub, Int(a), Int(b)) => Ok(Int(a - b)),
        (BinaryOp::Mul, Int(a), Int(b)) => Ok(Int(a * b)),
        (BinaryOp::Div, Int(_), Int(0)) => Err(EvalError::DivisionByZero),
        (BinaryOp::Div, Int(a), Int(b)) => Ok(Int(a / b)),
        // Arithmetic — float/float
        (BinaryOp::Add, Float(a), Float(b)) => Ok(Float(a + b)),
        (BinaryOp::Sub, Float(a), Float(b)) => Ok(Float(a - b)),
        (BinaryOp::Mul, Float(a), Float(b)) => Ok(Float(a * b)),
        (BinaryOp::Div, Float(a), Float(b)) => Ok(Float(a / b)),
        // String concatenation
        (BinaryOp::Add, Str(a), Str(b)) => Ok(Str(format!("{a}{b}"))),
        // Equality / inequality (structural)
        (BinaryOp::Eq, _, _) => Ok(Bool(l == r)),
        (BinaryOp::NotEq, _, _) => Ok(Bool(l != r)),
        // Comparisons
        (BinaryOp::Lt, Int(a), Int(b)) => Ok(Bool(a < b)),
        (BinaryOp::Le, Int(a), Int(b)) => Ok(Bool(a <= b)),
        (BinaryOp::Gt, Int(a), Int(b)) => Ok(Bool(a > b)),
        (BinaryOp::Ge, Int(a), Int(b)) => Ok(Bool(a >= b)),
        (BinaryOp::Lt, Float(a), Float(b)) => Ok(Bool(a < b)),
        (BinaryOp::Le, Float(a), Float(b)) => Ok(Bool(a <= b)),
        (BinaryOp::Gt, Float(a), Float(b)) => Ok(Bool(a > b)),
        (BinaryOp::Ge, Float(a), Float(b)) => Ok(Bool(a >= b)),
        // Logical (short-circuit semantics not preserved — both sides
        // already evaluated; that's fine for the side-effect-free
        // subset we cover).
        (BinaryOp::And, Bool(a), Bool(b)) => Ok(Bool(*a && *b)),
        (BinaryOp::Or, Bool(a), Bool(b)) => Ok(Bool(*a || *b)),
        (BinaryOp::Implies, Bool(a), Bool(b)) => Ok(Bool(!a || *b)),
        // List concatenation
        (BinaryOp::Concat, List(a), List(b)) => {
            let mut out = a.clone();
            out.extend_from_slice(b);
            Ok(List(out))
        }
        // Attrset update
        (BinaryOp::Update, AttrSet(a), AttrSet(b)) => {
            let mut merged = a.clone();
            for (k, v) in b {
                merged.insert(k.clone(), v.clone());
            }
            Ok(AttrSet(merged))
        }
        // Anything else: type mismatch surface (the eventual VM-
        // backed eval handles the long tail).
        _ => Err(EvalError::TypeMismatch {
            context: "binop",
            expected: "numeric / string / list / attrset / bool match",
            got: value_kind(&l),
        }),
    }
}

fn eval_unaryop(op: UnaryOp, v: EvalValue) -> Result<EvalValue, EvalError> {
    match (op, &v) {
        (UnaryOp::Neg, EvalValue::Int(n)) => Ok(EvalValue::Int(-n)),
        (UnaryOp::Neg, EvalValue::Float(f)) => Ok(EvalValue::Float(-f)),
        (UnaryOp::Not, EvalValue::Bool(b)) => Ok(EvalValue::Bool(!b)),
        _ => Err(EvalError::TypeMismatch {
            context: "unary op",
            expected: "numeric (for -) or bool (for !)",
            got: value_kind(&v),
        }),
    }
}

fn value_kind(v: &EvalValue) -> &'static str {
    match v {
        EvalValue::Int(_) => "Int",
        EvalValue::Float(_) => "Float",
        EvalValue::Bool(_) => "Bool",
        EvalValue::Null => "Null",
        EvalValue::Str(_) => "Str",
        EvalValue::Path(_) => "Path",
        EvalValue::List(_) => "List",
        EvalValue::AttrSet(_) => "AttrSet",
        EvalValue::Opaque { .. } => "Opaque",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast_graph::AstGraph;
    use pretty_assertions::assert_eq;

    fn eval(source: &str) -> EvalValue {
        let g = AstGraph::from_source(source).expect("parse");
        eval_node(&g, g.root_id, &EvalEnv::new()).expect("eval")
    }

    fn try_eval(source: &str) -> Result<EvalValue, EvalError> {
        let g = AstGraph::from_source(source).expect("parse");
        eval_node(&g, g.root_id, &EvalEnv::new())
    }

    #[test]
    fn int_and_float_literals() {
        assert_eq!(eval("42"), EvalValue::Int(42));
        assert_eq!(eval("3.14"), EvalValue::Float(3.14));
    }

    // Note on `true`/`false`/`null`: rnix parses these as plain
    // identifiers (`Ident("true")`); the evaluator that needs to
    // recognize them as Bool literals lives in the eval-engine
    // surface (sui-eval gives `true`/`false`/`null` special status).
    // Our minimum-viable walker treats them as Ident lookups; the
    // `undefined_ident_errors` test proves the no-binding case raises
    // UndefinedIdent. The env-keyed tests below prove we can wire
    // boolean values explicitly via `EvalEnv::with_binding`.

    #[test]
    fn undefined_ident_errors() {
        let g = AstGraph::from_source("true").expect("parse");
        let err = eval_node(&g, g.root_id, &EvalEnv::new()).unwrap_err();
        assert!(matches!(err, EvalError::UndefinedIdent(ref n) if n == "true"));
    }

    #[test]
    fn env_binding_resolves_ident() {
        let g = AstGraph::from_source("x").expect("parse");
        let env = EvalEnv::new().with_binding("x", EvalValue::Int(7));
        assert_eq!(
            eval_node(&g, g.root_id, &env).unwrap(),
            EvalValue::Int(7)
        );
    }

    #[test]
    fn arithmetic() {
        assert_eq!(eval("1 + 2"), EvalValue::Int(3));
        assert_eq!(eval("5 - 2"), EvalValue::Int(3));
        assert_eq!(eval("4 * 3"), EvalValue::Int(12));
        assert_eq!(eval("12 / 4"), EvalValue::Int(3));
    }

    #[test]
    fn division_by_zero_is_typed_error() {
        let err = try_eval("1 / 0").unwrap_err();
        assert!(matches!(err, EvalError::DivisionByZero));
    }

    #[test]
    fn comparison_and_equality() {
        assert_eq!(eval("1 == 1"), EvalValue::Bool(true));
        assert_eq!(eval("1 == 2"), EvalValue::Bool(false));
        assert_eq!(eval("1 != 2"), EvalValue::Bool(true));
        assert_eq!(eval("1 < 2"), EvalValue::Bool(true));
        assert_eq!(eval("2 <= 2"), EvalValue::Bool(true));
        assert_eq!(eval("3 > 2"), EvalValue::Bool(true));
        assert_eq!(eval("3 >= 3"), EvalValue::Bool(true));
    }

    #[test]
    fn string_literal_and_concat() {
        assert_eq!(eval("\"hello\""), EvalValue::Str("hello".into()));
        assert_eq!(
            eval("\"hello \" + \"world\""),
            EvalValue::Str("hello world".into())
        );
    }

    #[test]
    fn list_construction_and_concat() {
        assert_eq!(
            eval("[1 2 3]"),
            EvalValue::List(vec![EvalValue::Int(1), EvalValue::Int(2), EvalValue::Int(3)])
        );
        assert_eq!(
            eval("[1] ++ [2 3]"),
            EvalValue::List(vec![EvalValue::Int(1), EvalValue::Int(2), EvalValue::Int(3)])
        );
    }

    #[test]
    fn attrset_construction_via_dotted_paths() {
        let v = eval("{ a.b = 1; a.c = 2; }");
        match v {
            EvalValue::AttrSet(map) => {
                if let Some(EvalValue::AttrSet(inner)) = map.get("a") {
                    assert_eq!(inner.get("b"), Some(&EvalValue::Int(1)));
                    assert_eq!(inner.get("c"), Some(&EvalValue::Int(2)));
                } else {
                    panic!("expected nested attrset under 'a'");
                }
            }
            _ => panic!("expected attrset"),
        }
    }

    #[test]
    fn attrset_select_with_dotted_path() {
        let g = AstGraph::from_source("x.a.b").expect("parse");
        let env = EvalEnv::new().with_binding(
            "x",
            EvalValue::AttrSet(BTreeMap::from([(
                "a".to_string(),
                EvalValue::AttrSet(BTreeMap::from([(
                    "b".to_string(),
                    EvalValue::Int(42),
                )])),
            )])),
        );
        assert_eq!(
            eval_node(&g, g.root_id, &env).unwrap(),
            EvalValue::Int(42)
        );
    }

    #[test]
    fn select_with_fallback_when_missing() {
        let g = AstGraph::from_source("x.missing or 99").expect("parse");
        let env = EvalEnv::new().with_binding(
            "x",
            EvalValue::AttrSet(BTreeMap::from([("present".to_string(), EvalValue::Int(1))])),
        );
        assert_eq!(
            eval_node(&g, g.root_id, &env).unwrap(),
            EvalValue::Int(99)
        );
    }

    #[test]
    fn has_attr() {
        let g = AstGraph::from_source("x ? a").expect("parse");
        let env = EvalEnv::new().with_binding(
            "x",
            EvalValue::AttrSet(BTreeMap::from([("a".to_string(), EvalValue::Int(1))])),
        );
        assert_eq!(
            eval_node(&g, g.root_id, &env).unwrap(),
            EvalValue::Bool(true)
        );

        let env = EvalEnv::new().with_binding(
            "x",
            EvalValue::AttrSet(BTreeMap::from([("b".to_string(), EvalValue::Int(1))])),
        );
        assert_eq!(
            eval_node(&g, g.root_id, &env).unwrap(),
            EvalValue::Bool(false)
        );
    }

    #[test]
    fn if_then_else() {
        let g = AstGraph::from_source("if c then 1 else 2").expect("parse");
        let env_t = EvalEnv::new().with_binding("c", EvalValue::Bool(true));
        let env_f = EvalEnv::new().with_binding("c", EvalValue::Bool(false));
        assert_eq!(eval_node(&g, g.root_id, &env_t).unwrap(), EvalValue::Int(1));
        assert_eq!(eval_node(&g, g.root_id, &env_f).unwrap(), EvalValue::Int(2));
    }

    #[test]
    fn unary_neg_and_not() {
        let g = AstGraph::from_source("-x").expect("parse");
        let env = EvalEnv::new().with_binding("x", EvalValue::Int(7));
        assert_eq!(eval_node(&g, g.root_id, &env).unwrap(), EvalValue::Int(-7));

        let g = AstGraph::from_source("!b").expect("parse");
        let env = EvalEnv::new().with_binding("b", EvalValue::Bool(true));
        assert_eq!(
            eval_node(&g, g.root_id, &env).unwrap(),
            EvalValue::Bool(false)
        );
    }

    #[test]
    fn apply_is_opaque() {
        // f x → Apply variant → Opaque sentinel
        let g = AstGraph::from_source("f x").expect("parse");
        let env = EvalEnv::new()
            .with_binding("f", EvalValue::Int(1))
            .with_binding("x", EvalValue::Int(2));
        let v = eval_node(&g, g.root_id, &env).unwrap();
        match v {
            EvalValue::Opaque { ref kind, .. } => assert_eq!(kind, "Apply"),
            other => panic!("expected Opaque, got {other:?}"),
        }
    }

    #[test]
    fn lambda_is_opaque() {
        let g = AstGraph::from_source("x: x + 1").expect("parse");
        let v = eval_node(&g, g.root_id, &EvalEnv::new()).unwrap();
        match v {
            EvalValue::Opaque { ref kind, .. } => assert_eq!(kind, "Lambda"),
            other => panic!("expected Opaque, got {other:?}"),
        }
    }

    #[test]
    fn string_with_interpolated_int() {
        let g = AstGraph::from_source("\"value: ${n}\"").expect("parse");
        let env = EvalEnv::new().with_binding("n", EvalValue::Int(42));
        assert_eq!(
            eval_node(&g, g.root_id, &env).unwrap(),
            EvalValue::Str("value: 42".into())
        );
    }

    #[test]
    fn config_dotted_select_evaluates() {
        // Setter body pattern: `config.networking.hostName == "rio"`
        let g = AstGraph::from_source(
            "config.networking.hostName == \"rio\"",
        )
        .expect("parse");
        let mut inner = BTreeMap::new();
        inner.insert(
            "hostName".to_string(),
            EvalValue::Str("rio".to_string()),
        );
        let mut outer = BTreeMap::new();
        outer.insert("networking".to_string(), EvalValue::AttrSet(inner));
        let env = EvalEnv::new().with_binding("config", EvalValue::AttrSet(outer));
        assert_eq!(
            eval_node(&g, g.root_id, &env).unwrap(),
            EvalValue::Bool(true)
        );
    }
}
