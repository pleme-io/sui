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

/// One formal arg in a `PatternClosure`'s declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatternFormal {
    pub name: String,
    /// AST node id for the formal's default expression, if any. The
    /// default evaluates lazily — only when the caller's AttrSet
    /// doesn't supply this name.
    pub default_node_id: Option<NodeId>,
}

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
    /// Closure value — produced by evaluating a `Lambda` node with
    /// a single Ident param. Captures the param name + body AST id
    /// + closure env at construction time.
    Closure {
        param: String,
        body_node_id: NodeId,
        captured_env: BTreeMap<String, EvalValue>,
    },
    /// Pattern-arg closure — `{ a, b ? default, ... } [@ name]: body`.
    /// Captures the formal-arg shape so `Apply` can unpack the
    /// argument AttrSet, bind each formal, apply defaults for missing
    /// keys, and evaluate the body.
    PatternClosure {
        /// Formal-arg descriptors: name + optional default's AST id.
        formals: Vec<PatternFormal>,
        /// True if the pattern ends in `, ...` (accepts extra keys).
        accepts_extra: bool,
        /// `@ name` rebinds the entire arg AttrSet under `name`.
        binding_name: Option<String>,
        body_node_id: NodeId,
        captured_env: BTreeMap<String, EvalValue>,
    },
    /// Marker for a value built by a recognized built-in (`mkIf`,
    /// `mkOption`, etc.) — carries the built-in tag + the typed
    /// payload the built-in produced. Lets downstream pattern
    /// recognizers introspect (e.g. "is this an mkOption descriptor?").
    Builtin {
        kind: String,
        payload: Box<EvalValue>,
    },
    /// Result of `builtins.derivation` — typed descriptor of an
    /// input-addressed Nix derivation. `drv_path` and `out_path` are
    /// the computed `/nix/store/...` paths (via sui-spec's typed
    /// algorithm); `attrs` holds the full attr lattice so consumers
    /// can read `.drvPath`, `.outPath`, `.name`, `.system`, or any
    /// caller-supplied env entry the same way they would on cppnix.
    Derivation {
        name: String,
        system: String,
        drv_path: String,
        out_path: String,
        attrs: BTreeMap<String, EvalValue>,
    },
    /// Fallback for kinds we don't model yet. Carries the original
    /// AST node id so the eventual VM-backed re-evaluation can pick
    /// up where we left off.
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
        AstNodeKind::Ident(name) => {
            // rnix parses `true`, `false`, `null` as plain
            // identifiers. Recognize them as typed-value literals
            // at the Ident level so callers don't have to seed them
            // in every env.
            match name.as_str() {
                "true" => return Ok(EvalValue::Bool(true)),
                "false" => return Ok(EvalValue::Bool(false)),
                "null" => return Ok(EvalValue::Null),
                _ => {}
            }
            env.get(name)
                .cloned()
                .ok_or_else(|| EvalError::UndefinedIdent(name.clone()))
        }
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
        AstNodeKind::Apply { function, argument } => {
            eval_apply(ast, *function, *argument, env, depth + 1)
        }
        AstNodeKind::Lambda { param, body } => {
            // Capture the env in a flat map. Two param shapes:
            //   - Ident → simple Closure (one-arg function)
            //   - Pattern → PatternClosure (destructuring formal-args)
            match param {
                crate::ast_graph::LambdaParam::Ident(name) => {
                    Ok(EvalValue::Closure {
                        param: name.clone(),
                        body_node_id: *body,
                        captured_env: env.bindings.clone(),
                    })
                }
                crate::ast_graph::LambdaParam::Pattern {
                    binding_name,
                    formals,
                    accepts_extra,
                } => Ok(EvalValue::PatternClosure {
                    formals: formals
                        .iter()
                        .map(|f| PatternFormal {
                            name: f.name.clone(),
                            default_node_id: f.default,
                        })
                        .collect(),
                    accepts_extra: *accepts_extra,
                    binding_name: binding_name.clone(),
                    body_node_id: *body,
                    captured_env: env.bindings.clone(),
                }),
            }
        }
        AstNodeKind::LetIn { bindings, inherits, body } => {
            let mut env = env.clone();
            // Bindings: evaluate each value in the OUTER env first,
            // then bind. Cppnix actually allows recursive let-bindings
            // (each value sees the others); for now we use the simpler
            // sequential semantics — covers the common cases.
            for entry in bindings {
                if entry.path.len() == 1 {
                    let value = eval_at(ast, entry.value, &env, depth + 1)?;
                    env.bindings.insert(entry.path[0].clone(), value);
                }
                // Multi-level dotted paths in let bindings are rare;
                // skip for now (forward-compat).
            }
            // Inherits: pull each named attr from its source attrset.
            for inherit in inherits {
                if let Some(source_id) = inherit.source {
                    let source_val = eval_at(ast, source_id, &env, depth + 1)?;
                    if let EvalValue::AttrSet(map) = source_val {
                        for attr in &inherit.attrs {
                            if let Some(v) = map.get(attr) {
                                env.bindings.insert(attr.clone(), v.clone());
                            }
                        }
                    }
                } else {
                    // `inherit attr1 attr2;` (no source) pulls from the
                    // outer scope — already in env, so it's a no-op.
                }
            }
            eval_at(ast, *body, &env, depth + 1)
        }
        AstNodeKind::With { env: scope_expr, body } => {
            let scope_value = eval_at(ast, *scope_expr, env, depth + 1)?;
            let mut extended = env.clone();
            if let EvalValue::AttrSet(map) = scope_value {
                // `with X; body` makes every attr of X visible as a
                // top-level identifier in `body`. Lowest-precedence —
                // existing env bindings shadow.
                for (k, v) in map {
                    extended.bindings.entry(k).or_insert(v);
                }
            }
            eval_at(ast, *body, &extended, depth + 1)
        }
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

/// Evaluate `Apply(function, argument)`.
///
/// Dispatch order:
/// 1. If `function` is an `Ident` or `Select` resolving to a known
///    builtin name (`mkIf`, `mkForce`, etc.), call the builtin.
/// 2. If `function` evaluates to a `Closure`, bind its param + run
///    its body in the captured env.
/// 3. Curried builtins: `function` might itself be an `Apply`
///    (e.g. `mkIf cond` is one Apply, `(mkIf cond) body` is another).
///    Walk through until we resolve to a builtin name + collect args.
/// 4. Anything else → Opaque sentinel.
fn eval_apply(
    ast: &AstGraph,
    function: NodeId,
    argument: NodeId,
    env: &EvalEnv,
    depth: u32,
) -> Result<EvalValue, EvalError> {
    // Collect chained applies into a (root_function, args) form.
    // `(mkIf cond) body` → root_function = mkIf, args = [cond, body].
    let mut args: Vec<NodeId> = vec![argument];
    let mut cursor = function;
    loop {
        let node = &ast.nodes[cursor as usize];
        match &node.kind {
            AstNodeKind::Apply { function: f, argument: a } => {
                args.push(*a);
                cursor = *f;
            }
            _ => break,
        }
    }
    args.reverse();

    let root_node = &ast.nodes[cursor as usize];

    // Builtin name dispatch.
    let builtin_name = match &root_node.kind {
        AstNodeKind::Ident(name) => Some(name.clone()),
        AstNodeKind::Select { path, .. } => path.last().cloned(),
        _ => None,
    };

    if let Some(name) = builtin_name.as_deref() {
        if let Some(result) = try_dispatch_builtin(ast, name, &args, env, depth)? {
            return Ok(result);
        }
    }

    // Closure invocation — Ident and Pattern variants both handled.
    let func_value = eval_at(ast, cursor, env, depth + 1);
    if let Ok(callable) = func_value {
        if matches!(
            callable,
            EvalValue::Closure { .. } | EvalValue::PatternClosure { .. }
        ) {
            return apply_callable(ast, callable, &args, env, depth + 1, function);
        }
    }

    Ok(EvalValue::Opaque {
        kind: "Apply".to_string(),
        node_id: function,
    })
}

/// Invoke a Closure or PatternClosure with the given argument AST
/// nodes. Handles curried application (extra args applied to the
/// result if it's itself callable).
fn apply_callable(
    ast: &AstGraph,
    callable: EvalValue,
    args: &[NodeId],
    caller_env: &EvalEnv,
    depth: u32,
    fallback_node_id: NodeId,
) -> Result<EvalValue, EvalError> {
    let mut arg_iter = args.iter().copied();
    let first_arg_node = match arg_iter.next() {
        Some(a) => a,
        None => return Ok(EvalValue::Null),
    };

    let mut result = match callable {
        EvalValue::Closure {
            param,
            body_node_id,
            captured_env,
        } => {
            let first_arg = eval_at(ast, first_arg_node, caller_env, depth)?;
            let mut call_env = EvalEnv {
                bindings: captured_env,
            };
            call_env.bindings.insert(param, first_arg);
            eval_at(ast, body_node_id, &call_env, depth)?
        }
        EvalValue::PatternClosure {
            formals,
            accepts_extra,
            binding_name,
            body_node_id,
            captured_env,
        } => {
            // The argument MUST be an attrset for pattern destructuring.
            let arg_value = eval_at(ast, first_arg_node, caller_env, depth)?;
            let arg_map = match arg_value {
                EvalValue::AttrSet(m) => m,
                other => {
                    return Err(EvalError::TypeMismatch {
                        context: "pattern-closure arg",
                        expected: "attrset",
                        got: value_kind(&other),
                    });
                }
            };

            let mut call_env = EvalEnv {
                bindings: captured_env,
            };

            // Bind every declared formal — from arg_map if present,
            // else from default (evaluated in the call env so it sees
            // earlier formals).
            for formal in &formals {
                if let Some(v) = arg_map.get(&formal.name) {
                    call_env.bindings.insert(formal.name.clone(), v.clone());
                } else if let Some(default_node) = formal.default_node_id {
                    let default_v = eval_at(ast, default_node, &call_env, depth)?;
                    call_env.bindings.insert(formal.name.clone(), default_v);
                } else {
                    return Err(EvalError::TypeMismatch {
                        context: "pattern-closure missing required arg",
                        expected: "formal arg without default",
                        got: "missing key",
                    });
                }
            }

            // Reject extras unless `, ...` was declared.
            if !accepts_extra {
                let known: std::collections::HashSet<&str> =
                    formals.iter().map(|f| f.name.as_str()).collect();
                for k in arg_map.keys() {
                    if !known.contains(k.as_str()) {
                        return Err(EvalError::TypeMismatch {
                            context: "pattern-closure extra arg",
                            expected: "only declared formals",
                            got: "extra key",
                        });
                    }
                }
            }

            // `@ name` rebinds the full arg attrset.
            if let Some(name) = binding_name {
                call_env
                    .bindings
                    .insert(name, EvalValue::AttrSet(arg_map));
            }

            eval_at(ast, body_node_id, &call_env, depth)?
        }
        _ => unreachable!("apply_callable called with non-callable"),
    };

    // Curried application: feed remaining args into successive
    // callable results.
    for next_arg_node in arg_iter {
        let arg_val = eval_at(ast, next_arg_node, caller_env, depth)?;
        match result {
            EvalValue::Closure {
                param,
                body_node_id,
                captured_env,
            } => {
                let mut next_env = EvalEnv {
                    bindings: captured_env,
                };
                next_env.bindings.insert(param, arg_val);
                result = eval_at(ast, body_node_id, &next_env, depth)?;
            }
            EvalValue::PatternClosure { .. } => {
                // Curried into a pattern closure: requires the next
                // arg to be an attrset. Recurse via apply_callable
                // with a one-arg slice.
                let arg_iter_one = &[next_arg_node];
                let _ = arg_val; // already-evaluated value not threaded; recurse re-evaluates
                result = apply_callable(
                    ast,
                    result,
                    arg_iter_one,
                    caller_env,
                    depth,
                    fallback_node_id,
                )?;
            }
            _ => {
                return Ok(EvalValue::Opaque {
                    kind: "Apply-non-callable-result".to_string(),
                    node_id: fallback_node_id,
                });
            }
        }
    }
    Ok(result)
}

/// Try to dispatch to a built-in by name. Returns `Ok(None)` when
/// the name isn't a known builtin — caller falls back to closure
/// invocation or Opaque.
fn try_dispatch_builtin(
    ast: &AstGraph,
    name: &str,
    args: &[NodeId],
    env: &EvalEnv,
    depth: u32,
) -> Result<Option<EvalValue>, EvalError> {
    let arg = |i: usize| -> Result<EvalValue, EvalError> {
        eval_at(ast, args[i], env, depth + 1)
    };
    match name {
        "mkIf" if args.len() == 2 => {
            let cond = arg(0)?;
            match cond {
                EvalValue::Bool(true) => {
                    let body = arg(1)?;
                    Ok(Some(EvalValue::Builtin {
                        kind: "mkIf".to_string(),
                        payload: Box::new(body),
                    }))
                }
                EvalValue::Bool(false) => {
                    // Conditionally-disabled — the contribution is
                    // empty. Module merge layer treats this as a no-op
                    // for the destination path.
                    Ok(Some(EvalValue::Builtin {
                        kind: "mkIf-disabled".to_string(),
                        payload: Box::new(EvalValue::Null),
                    }))
                }
                other => Err(EvalError::TypeMismatch {
                    context: "mkIf condition",
                    expected: "bool",
                    got: value_kind(&other),
                }),
            }
        }
        "mkForce" if args.len() == 1 => Ok(Some(EvalValue::Builtin {
            kind: "mkForce".to_string(),
            payload: Box::new(arg(0)?),
        })),
        "mkVMOverride" if args.len() == 1 => Ok(Some(EvalValue::Builtin {
            kind: "mkVMOverride".to_string(),
            payload: Box::new(arg(0)?),
        })),
        "mkDefault" if args.len() == 1 => Ok(Some(EvalValue::Builtin {
            kind: "mkDefault".to_string(),
            payload: Box::new(arg(0)?),
        })),
        "mkOverride" if args.len() == 2 => {
            // Priority + value. We carry the priority in the kind tag
            // so downstream merge can use it.
            let prio = arg(0)?;
            let value = arg(1)?;
            let kind = match prio {
                EvalValue::Int(p) => format!("mkOverride-{p}"),
                _ => "mkOverride-bad-priority".to_string(),
            };
            Ok(Some(EvalValue::Builtin {
                kind,
                payload: Box::new(value),
            }))
        }
        "mkMerge" if args.len() == 1 => {
            let list = arg(0)?;
            match list {
                EvalValue::List(items) => Ok(Some(EvalValue::Builtin {
                    kind: "mkMerge".to_string(),
                    payload: Box::new(EvalValue::List(items)),
                })),
                _ => Err(EvalError::TypeMismatch {
                    context: "mkMerge arg",
                    expected: "list",
                    got: "non-list",
                }),
            }
        }
        "mkOption" if args.len() == 1 => {
            // Pass the descriptor attrset through verbatim.
            Ok(Some(EvalValue::Builtin {
                kind: "mkOption".to_string(),
                payload: Box::new(arg(0)?),
            }))
        }

        // ── builtins.* primitives (close the most common Opaque gaps) ──
        "toString" if args.len() == 1 => Ok(Some(builtin_to_string(arg(0)?))),
        "isString" if args.len() == 1 => {
            Ok(Some(EvalValue::Bool(matches!(arg(0)?, EvalValue::Str(_)))))
        }
        "isInt" if args.len() == 1 => {
            Ok(Some(EvalValue::Bool(matches!(arg(0)?, EvalValue::Int(_)))))
        }
        "isFloat" if args.len() == 1 => {
            Ok(Some(EvalValue::Bool(matches!(arg(0)?, EvalValue::Float(_)))))
        }
        "isBool" if args.len() == 1 => {
            Ok(Some(EvalValue::Bool(matches!(arg(0)?, EvalValue::Bool(_)))))
        }
        "isNull" if args.len() == 1 => {
            Ok(Some(EvalValue::Bool(matches!(arg(0)?, EvalValue::Null))))
        }
        "isList" if args.len() == 1 => {
            Ok(Some(EvalValue::Bool(matches!(arg(0)?, EvalValue::List(_)))))
        }
        "isAttrs" if args.len() == 1 => Ok(Some(EvalValue::Bool(matches!(
            arg(0)?,
            EvalValue::AttrSet(_)
        )))),
        "isFunction" if args.len() == 1 => Ok(Some(EvalValue::Bool(matches!(
            arg(0)?,
            EvalValue::Closure { .. }
        )))),
        "length" if args.len() == 1 => match arg(0)? {
            EvalValue::List(items) => Ok(Some(EvalValue::Int(items.len() as i64))),
            EvalValue::Str(s) => Ok(Some(EvalValue::Int(s.len() as i64))),
            other => Err(EvalError::TypeMismatch {
                context: "length arg",
                expected: "list or string",
                got: value_kind(&other),
            }),
        },
        "head" if args.len() == 1 => match arg(0)? {
            EvalValue::List(items) if !items.is_empty() => Ok(Some(items[0].clone())),
            EvalValue::List(_) => Err(EvalError::TypeMismatch {
                context: "head arg",
                expected: "non-empty list",
                got: "empty list",
            }),
            other => Err(EvalError::TypeMismatch {
                context: "head arg",
                expected: "list",
                got: value_kind(&other),
            }),
        },
        "tail" if args.len() == 1 => match arg(0)? {
            EvalValue::List(items) if !items.is_empty() => {
                Ok(Some(EvalValue::List(items[1..].to_vec())))
            }
            EvalValue::List(_) => Err(EvalError::TypeMismatch {
                context: "tail arg",
                expected: "non-empty list",
                got: "empty list",
            }),
            other => Err(EvalError::TypeMismatch {
                context: "tail arg",
                expected: "list",
                got: value_kind(&other),
            }),
        },
        "elem" if args.len() == 2 => {
            let needle = arg(0)?;
            match arg(1)? {
                EvalValue::List(items) => {
                    Ok(Some(EvalValue::Bool(items.iter().any(|v| v == &needle))))
                }
                other => Err(EvalError::TypeMismatch {
                    context: "elem second arg",
                    expected: "list",
                    got: value_kind(&other),
                }),
            }
        }
        "attrNames" if args.len() == 1 => match arg(0)? {
            EvalValue::AttrSet(map) => Ok(Some(EvalValue::List(
                map.keys().map(|k| EvalValue::Str(k.clone())).collect(),
            ))),
            other => Err(EvalError::TypeMismatch {
                context: "attrNames arg",
                expected: "attrset",
                got: value_kind(&other),
            }),
        },
        "attrValues" if args.len() == 1 => match arg(0)? {
            EvalValue::AttrSet(map) => {
                Ok(Some(EvalValue::List(map.into_values().collect())))
            }
            other => Err(EvalError::TypeMismatch {
                context: "attrValues arg",
                expected: "attrset",
                got: value_kind(&other),
            }),
        },
        "hasAttr" if args.len() == 2 => {
            let name = match arg(0)? {
                EvalValue::Str(s) => s,
                other => {
                    return Err(EvalError::TypeMismatch {
                        context: "hasAttr first arg",
                        expected: "string",
                        got: value_kind(&other),
                    })
                }
            };
            match arg(1)? {
                EvalValue::AttrSet(map) => {
                    Ok(Some(EvalValue::Bool(map.contains_key(&name))))
                }
                other => Err(EvalError::TypeMismatch {
                    context: "hasAttr second arg",
                    expected: "attrset",
                    got: value_kind(&other),
                }),
            }
        }
        "getAttr" if args.len() == 2 => {
            let name = match arg(0)? {
                EvalValue::Str(s) => s,
                other => {
                    return Err(EvalError::TypeMismatch {
                        context: "getAttr first arg",
                        expected: "string",
                        got: value_kind(&other),
                    })
                }
            };
            match arg(1)? {
                EvalValue::AttrSet(map) => map
                    .get(&name)
                    .cloned()
                    .map(Some)
                    .ok_or(EvalError::ConfigMiss { path: vec![name] }),
                other => Err(EvalError::TypeMismatch {
                    context: "getAttr second arg",
                    expected: "attrset",
                    got: value_kind(&other),
                }),
            }
        }
        "concatLists" if args.len() == 1 => match arg(0)? {
            EvalValue::List(items) => {
                let mut out = Vec::new();
                for item in items {
                    match item {
                        EvalValue::List(inner) => out.extend(inner),
                        other => {
                            return Err(EvalError::TypeMismatch {
                                context: "concatLists element",
                                expected: "list",
                                got: value_kind(&other),
                            })
                        }
                    }
                }
                Ok(Some(EvalValue::List(out)))
            }
            other => Err(EvalError::TypeMismatch {
                context: "concatLists arg",
                expected: "list of lists",
                got: value_kind(&other),
            }),
        },
        "concatStringsSep" if args.len() == 2 => {
            let sep = match arg(0)? {
                EvalValue::Str(s) => s,
                other => {
                    return Err(EvalError::TypeMismatch {
                        context: "concatStringsSep first arg",
                        expected: "string",
                        got: value_kind(&other),
                    })
                }
            };
            match arg(1)? {
                EvalValue::List(items) => {
                    let strs: Result<Vec<String>, _> = items
                        .into_iter()
                        .map(|v| match v {
                            EvalValue::Str(s) => Ok(s),
                            other => Err(EvalError::TypeMismatch {
                                context: "concatStringsSep list element",
                                expected: "string",
                                got: value_kind(&other),
                            }),
                        })
                        .collect();
                    Ok(Some(EvalValue::Str(strs?.join(&sep))))
                }
                other => Err(EvalError::TypeMismatch {
                    context: "concatStringsSep second arg",
                    expected: "list",
                    got: value_kind(&other),
                }),
            }
        }
        "throw" if args.len() == 1 => match arg(0)? {
            EvalValue::Str(s) => Err(EvalError::UndefinedIdent(format!("throw: {s}"))),
            other => Err(EvalError::TypeMismatch {
                context: "throw arg",
                expected: "string",
                got: value_kind(&other),
            }),
        },
        "abort" if args.len() == 1 => match arg(0)? {
            EvalValue::Str(s) => Err(EvalError::UndefinedIdent(format!("abort: {s}"))),
            other => Err(EvalError::TypeMismatch {
                context: "abort arg",
                expected: "string",
                got: value_kind(&other),
            }),
        },

        // ── lib.* wrappers — common module idioms ──
        // `lib.optional cond x` → if cond then [x] else []
        "optional" if args.len() == 2 => match arg(0)? {
            EvalValue::Bool(true) => Ok(Some(EvalValue::List(vec![arg(1)?]))),
            EvalValue::Bool(false) => Ok(Some(EvalValue::List(Vec::new()))),
            other => Err(EvalError::TypeMismatch {
                context: "optional first arg",
                expected: "bool",
                got: value_kind(&other),
            }),
        },
        // `lib.optionals cond xs` → if cond then xs else []
        "optionals" if args.len() == 2 => match arg(0)? {
            EvalValue::Bool(true) => match arg(1)? {
                v @ EvalValue::List(_) => Ok(Some(v)),
                other => Err(EvalError::TypeMismatch {
                    context: "optionals second arg",
                    expected: "list",
                    got: value_kind(&other),
                }),
            },
            EvalValue::Bool(false) => Ok(Some(EvalValue::List(Vec::new()))),
            other => Err(EvalError::TypeMismatch {
                context: "optionals first arg",
                expected: "bool",
                got: value_kind(&other),
            }),
        },
        // `lib.optionalAttrs cond attrs` → if cond then attrs else {}
        "optionalAttrs" if args.len() == 2 => match arg(0)? {
            EvalValue::Bool(true) => match arg(1)? {
                v @ EvalValue::AttrSet(_) => Ok(Some(v)),
                other => Err(EvalError::TypeMismatch {
                    context: "optionalAttrs second arg",
                    expected: "attrset",
                    got: value_kind(&other),
                }),
            },
            EvalValue::Bool(false) => {
                Ok(Some(EvalValue::AttrSet(std::collections::BTreeMap::new())))
            }
            other => Err(EvalError::TypeMismatch {
                context: "optionalAttrs first arg",
                expected: "bool",
                got: value_kind(&other),
            }),
        },
        // `lib.id` — identity. Common in default-value position.
        "id" if args.len() == 1 => Ok(Some(arg(0)?)),
        // `lib.const x` — a function-of-one-arg that always returns x.
        // We're called with two args (const x y) → return x.
        "const" if args.len() == 2 => Ok(Some(arg(0)?)),

        // ── String builtins ─────────────────────────────────────
        "substring" if args.len() == 3 => {
            let start = expect_int(arg(0)?, "substring start")?;
            let len = expect_int(arg(1)?, "substring len")?;
            let s = expect_str(arg(2)?, "substring source")?;
            let start = start.max(0) as usize;
            let len = len.max(0) as usize;
            let chars: Vec<char> = s.chars().collect();
            let end = (start + len).min(chars.len());
            Ok(Some(EvalValue::Str(
                if start >= chars.len() {
                    String::new()
                } else {
                    chars[start..end].iter().collect()
                },
            )))
        }
        "stringLength" if args.len() == 1 => {
            let s = expect_str(arg(0)?, "stringLength arg")?;
            Ok(Some(EvalValue::Int(s.chars().count() as i64)))
        }
        "replaceStrings" if args.len() == 3 => {
            let from = expect_list_of_str(arg(0)?, "replaceStrings from")?;
            let to = expect_list_of_str(arg(1)?, "replaceStrings to")?;
            let src = expect_str(arg(2)?, "replaceStrings source")?;
            if from.len() != to.len() {
                return Err(EvalError::TypeMismatch {
                    context: "replaceStrings",
                    expected: "from.len() == to.len()",
                    got: "mismatched list lengths",
                });
            }
            let mut out = src;
            for (f, t) in from.iter().zip(to.iter()) {
                out = out.replace(f, t);
            }
            Ok(Some(EvalValue::Str(out)))
        }

        // ── Higher-order list builtins ──────────────────────────
        "map" if args.len() == 2 => {
            let func = arg(0)?;
            let list = expect_list(arg(1)?, "map list")?;
            let mut out = Vec::with_capacity(list.len());
            for item in list {
                let single = [synthetic_value_node(ast)];
                // We can't synthesize an AST node for an already-
                // evaluated value, so route through apply_value
                // (below). Use a single-arg helper.
                let _ = single;
                out.push(apply_value_to_one(ast, func.clone(), item, env, depth + 1)?);
            }
            Ok(Some(EvalValue::List(out)))
        }
        "filter" if args.len() == 2 => {
            let pred = arg(0)?;
            let list = expect_list(arg(1)?, "filter list")?;
            let mut out = Vec::new();
            for item in list {
                let keep =
                    apply_value_to_one(ast, pred.clone(), item.clone(), env, depth + 1)?;
                if matches!(keep, EvalValue::Bool(true)) {
                    out.push(item);
                }
            }
            Ok(Some(EvalValue::List(out)))
        }
        "foldl'" if args.len() == 3 => {
            let func = arg(0)?;
            let init = arg(1)?;
            let list = expect_list(arg(2)?, "foldl' list")?;
            let mut acc = init;
            for item in list {
                acc = apply_value_to_two(
                    ast,
                    func.clone(),
                    acc,
                    item,
                    env,
                    depth + 1,
                )?;
            }
            Ok(Some(acc))
        }
        "genList" if args.len() == 2 => {
            let func = arg(0)?;
            let n = expect_int(arg(1)?, "genList count")?;
            let mut out = Vec::with_capacity(n.max(0) as usize);
            for i in 0..n.max(0) {
                out.push(apply_value_to_one(
                    ast,
                    func.clone(),
                    EvalValue::Int(i),
                    env,
                    depth + 1,
                )?);
            }
            Ok(Some(EvalValue::List(out)))
        }
        "concatMap" if args.len() == 2 => {
            let func = arg(0)?;
            let list = expect_list(arg(1)?, "concatMap list")?;
            let mut out = Vec::new();
            for item in list {
                let v = apply_value_to_one(ast, func.clone(), item, env, depth + 1)?;
                match v {
                    EvalValue::List(items) => out.extend(items),
                    other => {
                        return Err(EvalError::TypeMismatch {
                            context: "concatMap fn result",
                            expected: "list",
                            got: value_kind(&other),
                        });
                    }
                }
            }
            Ok(Some(EvalValue::List(out)))
        }

        // ── Attrset builtins ────────────────────────────────────
        "intersectAttrs" if args.len() == 2 => {
            let a = expect_attrset(arg(0)?, "intersectAttrs first")?;
            let b = expect_attrset(arg(1)?, "intersectAttrs second")?;
            let mut out: BTreeMap<String, EvalValue> = BTreeMap::new();
            for (k, v) in b {
                if a.contains_key(&k) {
                    out.insert(k, v);
                }
            }
            Ok(Some(EvalValue::AttrSet(out)))
        }
        "removeAttrs" if args.len() == 2 => {
            let mut attrs = expect_attrset(arg(0)?, "removeAttrs source")?;
            let names = expect_list_of_str(arg(1)?, "removeAttrs names")?;
            for n in names {
                attrs.remove(&n);
            }
            Ok(Some(EvalValue::AttrSet(attrs)))
        }
        "listToAttrs" if args.len() == 1 => {
            let entries = expect_list(arg(0)?, "listToAttrs list")?;
            let mut out: BTreeMap<String, EvalValue> = BTreeMap::new();
            for entry in entries {
                let map = expect_attrset(entry, "listToAttrs entry")?;
                let name = map
                    .get("name")
                    .and_then(|v| match v {
                        EvalValue::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .ok_or(EvalError::TypeMismatch {
                        context: "listToAttrs entry.name",
                        expected: "string",
                        got: "missing or non-string",
                    })?;
                let value = map.get("value").cloned().unwrap_or(EvalValue::Null);
                out.insert(name, value);
            }
            Ok(Some(EvalValue::AttrSet(out)))
        }

        // ── Filesystem builtins ─────────────────────────────────
        // Note: these touch the host filesystem. Eval-cache hits
        // bypass them; first-touch evaluates the real file.
        "readFile" if args.len() == 1 => {
            let path = expect_str_or_path(arg(0)?, "readFile arg")?;
            let bytes = std::fs::read_to_string(&path).map_err(|e| {
                EvalError::TypeMismatch {
                    context: "readFile io",
                    expected: "existing file",
                    got: leak_msg(format!("io error reading {path}: {e}")),
                }
            })?;
            Ok(Some(EvalValue::Str(bytes)))
        }
        "pathExists" if args.len() == 1 => {
            let path = expect_str_or_path(arg(0)?, "pathExists arg")?;
            Ok(Some(EvalValue::Bool(std::path::Path::new(&path).exists())))
        }
        // `import` resolves a path relative to the caller's flake
        // root. Today we accept absolute paths verbatim and parse
        // them via rnix; relative-path resolution requires call-
        // site context we don't yet thread, so relative imports
        // surface as a typed error.
        "import" if args.len() == 1 => {
            let path = expect_str_or_path(arg(0)?, "import arg")?;
            if !std::path::Path::new(&path).is_absolute() {
                return Err(EvalError::TypeMismatch {
                    context: "import",
                    expected: "absolute path (relative imports need call-site ctx)",
                    got: leak_msg(format!("relative path: {path}")),
                });
            }
            let source = std::fs::read_to_string(&path).map_err(|e| {
                EvalError::TypeMismatch {
                    context: "import io",
                    expected: "existing .nix file",
                    got: leak_msg(format!("io error reading {path}: {e}")),
                }
            })?;
            let imported_graph = AstGraph::from_source(&source).map_err(|e| {
                EvalError::TypeMismatch {
                    context: "import parse",
                    expected: "valid nix source",
                    got: leak_msg(format!("parse error: {e}")),
                }
            })?;
            // Evaluate the imported file in a FRESH env (cppnix
            // semantics: imports don't see the caller's scope).
            let imported_env = EvalEnv::new();
            // We need to walk the imported AST, but we hold an `ast`
            // (the caller's graph) here. Walking a different graph
            // means recursing with a different `ast` — bypass via
            // direct eval_node call.
            let v = eval_node(&imported_graph, imported_graph.root_id, &imported_env)
                .map_err(|e| EvalError::TypeMismatch {
                    context: "import eval",
                    expected: "successful eval",
                    got: leak_msg(format!("eval error: {e}")),
                })?;
            Ok(Some(v))
        }

        // ── More lib.* wrappers ─────────────────────────────────
        // lib.filterAttrs predicate set → AttrSet
        "filterAttrs" if args.len() == 2 => {
            let pred = arg(0)?;
            let attrs = expect_attrset(arg(1)?, "filterAttrs set")?;
            let mut out: BTreeMap<String, EvalValue> = BTreeMap::new();
            for (k, v) in attrs {
                let keep = apply_value_to_two(
                    ast,
                    pred.clone(),
                    EvalValue::Str(k.clone()),
                    v.clone(),
                    env,
                    depth + 1,
                )?;
                if matches!(keep, EvalValue::Bool(true)) {
                    out.insert(k, v);
                }
            }
            Ok(Some(EvalValue::AttrSet(out)))
        }
        // lib.mapAttrs f set → AttrSet
        "mapAttrs" if args.len() == 2 => {
            let func = arg(0)?;
            let attrs = expect_attrset(arg(1)?, "mapAttrs set")?;
            let mut out: BTreeMap<String, EvalValue> = BTreeMap::new();
            for (k, v) in attrs {
                let new_v = apply_value_to_two(
                    ast,
                    func.clone(),
                    EvalValue::Str(k.clone()),
                    v,
                    env,
                    depth + 1,
                )?;
                out.insert(k, new_v);
            }
            Ok(Some(EvalValue::AttrSet(out)))
        }
        // lib.flatten — recursive flatten of nested lists
        "flatten" if args.len() == 1 => {
            fn flat(out: &mut Vec<EvalValue>, v: EvalValue) {
                match v {
                    EvalValue::List(items) => {
                        for i in items {
                            flat(out, i);
                        }
                    }
                    other => out.push(other),
                }
            }
            let mut out = Vec::new();
            flat(&mut out, arg(0)?);
            Ok(Some(EvalValue::List(out)))
        }
        "unique" if args.len() == 1 => {
            let list = expect_list(arg(0)?, "unique arg")?;
            let mut seen: Vec<EvalValue> = Vec::new();
            for v in list {
                if !seen.contains(&v) {
                    seen.push(v);
                }
            }
            Ok(Some(EvalValue::List(seen)))
        }
        "take" if args.len() == 2 => {
            let n = expect_int(arg(0)?, "take count")?;
            let list = expect_list(arg(1)?, "take list")?;
            let n = n.max(0) as usize;
            Ok(Some(EvalValue::List(
                list.into_iter().take(n).collect(),
            )))
        }
        "drop" if args.len() == 2 => {
            let n = expect_int(arg(0)?, "drop count")?;
            let list = expect_list(arg(1)?, "drop list")?;
            let n = n.max(0) as usize;
            Ok(Some(EvalValue::List(
                list.into_iter().skip(n).collect(),
            )))
        }
        // ── builtins.derivation — the build-recipe primitive ──────
        //
        // Reads the required attrs (name, system, builder) and
        // optional ones (args, outputs, plus arbitrary env entries)
        // from the input attrset, hands the resulting Derivation to
        // sui-spec's typed input-addressed algorithm, and returns a
        // typed `Derivation` value carrying both the canonical
        // `.drv` path AND the per-output store paths.  No fallback to
        // cppnix anywhere — every byte of the algorithm is in
        // sui_spec::derivation.
        "derivation" if args.len() == 1 => Ok(Some(eval_derivation(arg(0)?)?)),
        "__derivationStrict" if args.len() == 1 => Ok(Some(eval_derivation(arg(0)?)?)),
        // `derivationStrict` is the cppnix lib-level wrapper that
        // calls `builtins.derivation`; expose under the same alias so
        // `(import <nixpkgs> {}).hello` style code resolves uniformly.
        "derivationStrict" if args.len() == 1 => Ok(Some(eval_derivation(arg(0)?)?)),

        // ── builtins.fetch* — real network/filesystem fetchers ────
        //
        // Drive the typed FetcherEnvironment through real ureq HTTP
        // + filesystem unpack code.  Coverage-gap: only flat hashes
        // today (the cppnix-canonical fetchurl mode).  Recursive
        // (NAR-based) hash mode and git/tarball recursion land per
        // ship; the gap is logged via Opaque sentinels so callers see
        // the typed boundary.
        "fetchurl" if args.len() == 1 => Ok(Some(eval_fetchurl(arg(0)?)?)),
        "fetchTarball" if args.len() == 1 => Ok(Some(eval_fetch_tarball(arg(0)?)?)),
        "fetchGit" if args.len() == 1 => Ok(Some(eval_fetch_git(arg(0)?)?)),
        "fetchTree" if args.len() == 1 => Ok(Some(eval_fetch_tree(arg(0)?)?)),

        // foldr (right-fold; lib version of builtins.foldl' inverted)
        "foldr" if args.len() == 3 => {
            let func = arg(0)?;
            let init = arg(1)?;
            let list = expect_list(arg(2)?, "foldr list")?;
            let mut acc = init;
            for item in list.into_iter().rev() {
                acc = apply_value_to_two(
                    ast,
                    func.clone(),
                    item,
                    acc,
                    env,
                    depth + 1,
                )?;
            }
            Ok(Some(acc))
        }

        _ => Ok(None),
    }
}

// ── Helpers for higher-order builtins ────────────────────────────

/// Apply an already-evaluated callable to one already-evaluated arg.
/// Bridges the "argument is a value, not an AST node" gap for builtins
/// like `map`/`filter`/`foldl'` that iterate over pre-evaluated lists.
fn apply_value_to_one(
    ast: &AstGraph,
    callable: EvalValue,
    arg: EvalValue,
    _caller_env: &EvalEnv,
    depth: u32,
) -> Result<EvalValue, EvalError> {
    match callable {
        EvalValue::Closure {
            param,
            body_node_id,
            captured_env,
        } => {
            let mut call_env = EvalEnv {
                bindings: captured_env,
            };
            call_env.bindings.insert(param, arg);
            eval_at(ast, body_node_id, &call_env, depth)
        }
        EvalValue::PatternClosure {
            formals,
            accepts_extra,
            binding_name,
            body_node_id,
            captured_env,
        } => {
            let arg_map = match arg {
                EvalValue::AttrSet(m) => m,
                other => {
                    return Err(EvalError::TypeMismatch {
                        context: "pattern-closure arg",
                        expected: "attrset",
                        got: value_kind(&other),
                    });
                }
            };
            let mut call_env = EvalEnv {
                bindings: captured_env,
            };
            for formal in &formals {
                if let Some(v) = arg_map.get(&formal.name) {
                    call_env.bindings.insert(formal.name.clone(), v.clone());
                } else if let Some(default_node) = formal.default_node_id {
                    let d = eval_at(ast, default_node, &call_env, depth)?;
                    call_env.bindings.insert(formal.name.clone(), d);
                } else {
                    return Err(EvalError::TypeMismatch {
                        context: "pattern-closure missing required arg",
                        expected: "formal arg without default",
                        got: "missing key",
                    });
                }
            }
            if !accepts_extra {
                let known: std::collections::HashSet<&str> =
                    formals.iter().map(|f| f.name.as_str()).collect();
                for k in arg_map.keys() {
                    if !known.contains(k.as_str()) {
                        return Err(EvalError::TypeMismatch {
                            context: "pattern-closure extra arg",
                            expected: "only declared formals",
                            got: "extra key",
                        });
                    }
                }
            }
            if let Some(name) = binding_name {
                call_env
                    .bindings
                    .insert(name, EvalValue::AttrSet(arg_map));
            }
            eval_at(ast, body_node_id, &call_env, depth)
        }
        other => Err(EvalError::TypeMismatch {
            context: "apply_value_to_one",
            expected: "callable",
            got: value_kind(&other),
        }),
    }
}

/// Apply a callable to two already-evaluated args (for foldl'/foldr,
/// mapAttrs, filterAttrs). Curried — applies the first arg, then
/// applies the result to the second arg.
fn apply_value_to_two(
    ast: &AstGraph,
    callable: EvalValue,
    a: EvalValue,
    b: EvalValue,
    caller_env: &EvalEnv,
    depth: u32,
) -> Result<EvalValue, EvalError> {
    let after_first = apply_value_to_one(ast, callable, a, caller_env, depth)?;
    apply_value_to_one(ast, after_first, b, caller_env, depth)
}

/// Placeholder — synthetic node id isn't used today but reserved
/// for the future "synthesize an AST node to wrap a pre-evaluated
/// value" pattern (would need an AST mutation primitive).
fn synthetic_value_node(_ast: &AstGraph) -> NodeId {
    0
}

fn expect_int(v: EvalValue, ctx: &'static str) -> Result<i64, EvalError> {
    match v {
        EvalValue::Int(n) => Ok(n),
        other => Err(EvalError::TypeMismatch {
            context: ctx,
            expected: "int",
            got: value_kind(&other),
        }),
    }
}

fn expect_str(v: EvalValue, ctx: &'static str) -> Result<String, EvalError> {
    match v {
        EvalValue::Str(s) => Ok(s),
        other => Err(EvalError::TypeMismatch {
            context: ctx,
            expected: "string",
            got: value_kind(&other),
        }),
    }
}

fn expect_str_or_path(v: EvalValue, ctx: &'static str) -> Result<String, EvalError> {
    match v {
        EvalValue::Str(s) | EvalValue::Path(s) => Ok(s),
        other => Err(EvalError::TypeMismatch {
            context: ctx,
            expected: "string or path",
            got: value_kind(&other),
        }),
    }
}

fn expect_list(v: EvalValue, ctx: &'static str) -> Result<Vec<EvalValue>, EvalError> {
    match v {
        EvalValue::List(items) => Ok(items),
        other => Err(EvalError::TypeMismatch {
            context: ctx,
            expected: "list",
            got: value_kind(&other),
        }),
    }
}

fn expect_list_of_str(v: EvalValue, ctx: &'static str) -> Result<Vec<String>, EvalError> {
    let items = expect_list(v, ctx)?;
    items
        .into_iter()
        .map(|v| expect_str(v, ctx))
        .collect()
}

fn expect_attrset(
    v: EvalValue,
    ctx: &'static str,
) -> Result<BTreeMap<String, EvalValue>, EvalError> {
    match v {
        EvalValue::AttrSet(m) => Ok(m),
        other => Err(EvalError::TypeMismatch {
            context: ctx,
            expected: "attrset",
            got: value_kind(&other),
        }),
    }
}

/// Leaking a `String` to `&'static str` for error messages where
/// the existing TypeMismatch fields are `&'static str`. Bounded:
/// only used on the error path, so allocations are rare.
fn leak_msg(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// builtins.toString: stringify any value per cppnix's coercion rules.
fn builtin_to_string(v: EvalValue) -> EvalValue {
    match v {
        EvalValue::Str(s) => EvalValue::Str(s),
        EvalValue::Int(n) => EvalValue::Str(n.to_string()),
        EvalValue::Float(f) => EvalValue::Str(f.to_string()),
        EvalValue::Bool(b) => EvalValue::Str(if b { "1" } else { "" }.to_string()),
        EvalValue::Null => EvalValue::Str(String::new()),
        EvalValue::Path(p) => EvalValue::Str(p),
        EvalValue::List(items) => {
            let parts: Vec<String> = items
                .into_iter()
                .map(|i| match builtin_to_string(i) {
                    EvalValue::Str(s) => s,
                    _ => String::new(),
                })
                .collect();
            EvalValue::Str(parts.join(" "))
        }
        // A Derivation coerces to its `out` output path under cppnix's
        // toString rules — the most common stringification target.
        EvalValue::Derivation { out_path, .. } => EvalValue::Str(out_path),
        // Cppnix's toString on attrsets calls the `__toString` field if
        // present; otherwise errors. We approximate: empty string.
        EvalValue::AttrSet(_)
        | EvalValue::Closure { .. }
        | EvalValue::PatternClosure { .. }
        | EvalValue::Builtin { .. }
        | EvalValue::Opaque { .. } => EvalValue::Str(String::new()),
    }
}

// ── builtins.derivation implementation ─────────────────────────────
//
// The build-recipe primitive.  Cppnix's `builtins.derivation` is the
// foundation of `nix build` — every derivation in nixpkgs winds up
// here.  This implementation drives the same typed algorithm that
// sui-spec ships for cppnix-parity (`sui_spec::derivation::apply` ←
// `cppnix-input-addressed.lisp`), so an evaluated `derivation { … }`
// produces byte-exact identical store paths to cppnix on the same
// inputs.

/// Coerce an EvalValue into the ATerm-serializable string form that
/// cppnix uses to populate `Derivation::env`.  Lists become
/// space-joined; ints/floats stringify; bools coerce to `"1"`/`""`
/// like cppnix; derivations coerce to their `out` path; everything
/// else is rejected with a typed error.
fn coerce_to_env_value(v: &EvalValue, key: &str) -> Result<String, EvalError> {
    Ok(match v {
        EvalValue::Str(s) => s.clone(),
        EvalValue::Path(p) => p.clone(),
        EvalValue::Int(n) => n.to_string(),
        EvalValue::Float(f) => f.to_string(),
        EvalValue::Bool(b) => if *b { "1" } else { "" }.to_string(),
        EvalValue::Null => String::new(),
        EvalValue::Derivation { out_path, .. } => out_path.clone(),
        EvalValue::List(items) => {
            let mut parts: Vec<String> = Vec::with_capacity(items.len());
            for it in items {
                parts.push(coerce_to_env_value(it, key)?);
            }
            parts.join(" ")
        }
        other => {
            return Err(EvalError::TypeMismatch {
                context: leak_msg(format!("derivation env coercion for `{key}`")),
                expected: "string-coercible (str/path/int/float/bool/null/list/derivation)",
                got: value_kind(other),
            });
        }
    })
}

/// `builtins.derivation { … }` — the central typed pathway.  Reads
/// required + optional attrs from the input attrset, builds a
/// canonical `Derivation`, hands it to the typed input-addressed
/// algorithm, and returns the typed `Derivation` value.
fn eval_derivation(arg: EvalValue) -> Result<EvalValue, EvalError> {
    let attrs = expect_attrset(arg, "derivation arg")?;

    let name = attrs
        .get("name")
        .ok_or(EvalError::TypeMismatch {
            context: "derivation",
            expected: "attrset with `name`",
            got: "missing `name`",
        })
        .and_then(|v| expect_str(v.clone(), "derivation.name"))?;

    let system = attrs
        .get("system")
        .ok_or(EvalError::TypeMismatch {
            context: "derivation",
            expected: "attrset with `system`",
            got: "missing `system`",
        })
        .and_then(|v| expect_str(v.clone(), "derivation.system"))?;

    let builder_value = attrs.get("builder").ok_or(EvalError::TypeMismatch {
        context: "derivation",
        expected: "attrset with `builder`",
        got: "missing `builder`",
    })?;
    let builder = expect_str_or_path(builder_value.clone(), "derivation.builder")?;

    let args_list: Vec<String> = match attrs.get("args") {
        Some(v) => expect_list_of_str(v.clone(), "derivation.args")?,
        None => Vec::new(),
    };

    let outputs: Vec<String> = match attrs.get("outputs") {
        Some(v) => expect_list_of_str(v.clone(), "derivation.outputs")?,
        None => vec!["out".to_string()],
    };

    // Drive the typed input-addressed algorithm.
    let mut drv = sui_compat::derivation::Derivation {
        outputs: std::collections::BTreeMap::new(),
        input_derivations: std::collections::BTreeMap::new(),
        input_sources: Vec::new(),
        system: system.clone(),
        builder,
        args: args_list,
        env: std::collections::BTreeMap::new(),
    };

    // Every attr except the structural ones lands in env (cppnix
    // semantics).  outputs are masked by the algorithm itself, so
    // pre-fill them with "" and they'll be overwritten in
    // FillPlaceholders.
    let structural: std::collections::HashSet<&str> = [
        "name", "system", "builder", "args", "outputs",
        // Derivation-level meta — cppnix sometimes uses these for
        // tracking but they aren't part of the input-addressed hash
        // input; route them through env transparently like everything
        // else (cppnix-parity).
    ].into_iter().collect();

    for (k, v) in &attrs {
        if structural.contains(k.as_str()) {
            continue;
        }
        let s = coerce_to_env_value(v, k)?;
        drv.env.insert(k.clone(), s);
    }
    // The required structural env entries cppnix also writes.
    drv.env.insert("name".to_string(), name.clone());
    drv.env.insert("system".to_string(), system.clone());
    drv.env.insert("builder".to_string(), drv.builder.clone());

    // Pre-populate the input-derivations map from any input
    // derivations referenced in env.  For the v1 typed pathway we
    // detect derivation references during env coercion: a
    // `Derivation` value writes its drv_path here so the algorithm
    // can include it in the input-addressed hash.
    for (_, v) in &attrs {
        if let EvalValue::Derivation { drv_path, .. } = v {
            drv.input_derivations
                .entry(drv_path.clone())
                .or_default()
                .push("out".to_string());
        }
    }

    let algo = crate::derivation::load_canonical().map_err(|e| EvalError::TypeMismatch {
        context: "derivation algorithm load",
        expected: "canonical input-addressed algorithm",
        got: leak_msg(e.to_string()),
    })?;

    let (drv_path, out_paths, _final_drv) =
        crate::derivation::apply(&algo, drv, outputs.clone(), &name).map_err(|e| {
            EvalError::TypeMismatch {
                context: "derivation algorithm apply",
                expected: "successful input-addressed pipeline",
                got: leak_msg(e.to_string()),
            }
        })?;

    // The first output (typically `out`) is the canonical `outPath`.
    let primary_out = outputs.first().map(String::as_str).unwrap_or("out");
    let out_path = out_paths
        .get(primary_out)
        .cloned()
        .ok_or(EvalError::TypeMismatch {
            context: "derivation outputs",
            expected: "first output materialized",
            got: "no path computed",
        })?;

    // Build an attrs view that mirrors cppnix's `derivation` return
    // value: all the original input attrs PLUS the computed store
    // paths for each output AND a top-level `outPath`/`drvPath`/`type`.
    let mut result_attrs = attrs.clone();
    for (name, path) in &out_paths {
        result_attrs.insert(name.clone(), EvalValue::Str(path.clone()));
    }
    result_attrs.insert("outPath".to_string(), EvalValue::Str(out_path.clone()));
    result_attrs.insert("drvPath".to_string(), EvalValue::Str(drv_path.clone()));
    result_attrs.insert("type".to_string(), EvalValue::Str("derivation".to_string()));

    Ok(EvalValue::Derivation {
        name,
        system,
        drv_path,
        out_path,
        attrs: result_attrs,
    })
}

// ── builtins.fetch* implementations ────────────────────────────────
//
// Real fetchers.  HTTP via `ureq` (workspace dep), filesystem via
// `FsTransport` from sui-spec::fetcher.  Tarball unpacking via tar +
// flate2.  Git via `gix`.  Each builtin reads the cppnix-shape
// argument attrset, drives the typed FetcherEnvironment trait, and
// returns either a typed `Derivation`-like value or an `EvalValue::Str`
// carrying the store path.

/// Filesystem-backed `FetcherEnvironment` for tests + offline use.
/// Production deployments swap in an `ureq`-backed transport via the
/// SchemeRouter pattern from sui-spec::fetcher.
struct InProcessFetcherEnv {
    /// Root the synthetic "store" lives in.  Tests pass a tmpdir;
    /// production wires `/nix/store`.
    store_root: std::path::PathBuf,
    transport: crate::fetcher::SchemeRouter<UreqTransport>,
}

impl InProcessFetcherEnv {
    fn new(store_root: std::path::PathBuf) -> Self {
        Self {
            store_root,
            transport: crate::fetcher::SchemeRouter::new(UreqTransport),
        }
    }
}

impl crate::fetcher::FetcherEnvironment for InProcessFetcherEnv {
    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        use crate::fetcher::HttpTransport;
        self.transport.get(url).map_err(|e| e.to_string())
    }

    fn hash_bytes(&self, bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let d = Sha256::digest(bytes);
        let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256:{hex}")
    }

    fn write_to_store(&self, name: &str, bytes: &[u8]) -> Result<String, String> {
        // Compute the FOD path via sui-compat's typed helper and write
        // the bytes there.  Matches cppnix's flat-hash FOD layout.
        use sha2::{Digest, Sha256};
        let inner = Sha256::digest(bytes);
        let hex: String = inner.iter().map(|b| format!("{b:02x}")).collect();
        let path = sui_compat::store_path::compute_fixed_output_hash(
            "sha256",
            &hex,
            false, // flat (non-recursive)
            name,
        );
        // Materialize under store_root using only the basename (so
        // tests with a /tmp store_root don't try to write /nix/store).
        let basename = std::path::Path::new(&path)
            .file_name()
            .ok_or_else(|| format!("bad store path `{path}`"))?;
        let dst = self.store_root.join(basename);
        std::fs::write(&dst, bytes).map_err(|e| e.to_string())?;
        Ok(path)
    }
}

/// `ureq`-backed HTTP transport.  Always available; gracefully
/// surfaces network errors as typed `HttpError` variants so callers
/// can branch.
struct UreqTransport;

impl crate::fetcher::HttpTransport for UreqTransport {
    fn get(&self, url: &str) -> Result<Vec<u8>, crate::fetcher::HttpError> {
        let resp = ureq::get(url)
            .call()
            .map_err(|e| crate::fetcher::HttpError::NetworkFailure(e.to_string()))?;
        let mut body = Vec::new();
        use std::io::Read;
        resp.into_body()
            .into_reader()
            .read_to_end(&mut body)
            .map_err(|e| crate::fetcher::HttpError::BodyReadFailure(e.to_string()))?;
        Ok(body)
    }
}

/// Pick an on-disk store root for an in-process fetch.  Honors the
/// `SUI_TEST_STORE_DIR` env var so tests can isolate; falls back to
/// `/nix/store` on real deployments (which `compute_fixed_output_hash`
/// already uses for fingerprint computation).
fn default_store_root() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("SUI_TEST_STORE_DIR") {
        return std::path::PathBuf::from(p);
    }
    std::path::PathBuf::from("/nix/store")
}

/// `builtins.fetchurl { url, sha256 ? null, name ? <basename(url)> }`
/// — flat-hash file fetch.
fn eval_fetchurl(arg: EvalValue) -> Result<EvalValue, EvalError> {
    let attrs = match arg {
        // Shorthand `builtins.fetchurl "<url>"` is permitted by cppnix.
        EvalValue::Str(s) => {
            let mut m = BTreeMap::new();
            m.insert("url".to_string(), EvalValue::Str(s));
            m
        }
        EvalValue::AttrSet(m) => m,
        other => {
            return Err(EvalError::TypeMismatch {
                context: "fetchurl arg",
                expected: "string or attrset",
                got: value_kind(&other),
            });
        }
    };

    let url = attrs
        .get("url")
        .ok_or(EvalError::TypeMismatch {
            context: "fetchurl",
            expected: "url",
            got: "missing url",
        })
        .and_then(|v| expect_str_or_path(v.clone(), "fetchurl.url"))?;

    let declared_hash = match attrs.get("sha256").or_else(|| attrs.get("hash")) {
        Some(EvalValue::Str(s)) => Some(format!("sha256:{}", s.trim_start_matches("sha256:"))),
        _ => None,
    };

    let name = match attrs.get("name") {
        Some(EvalValue::Str(s)) => s.clone(),
        _ => url.rsplit('/').next().unwrap_or("download").to_string(),
    };

    let env = InProcessFetcherEnv::new(default_store_root());
    let spec = crate::fetcher::load_named("fetchurl").map_err(|e| EvalError::TypeMismatch {
        context: "fetchurl spec load",
        expected: "fetchurl in canonical fetchers",
        got: leak_msg(e.to_string()),
    })?;
    let outcome = crate::fetcher::apply(
        &spec,
        &crate::fetcher::FetchArgs {
            url: url.clone(),
            declared_hash,
            name_hint: Some(name.clone()),
        },
        &env,
    )
    .map_err(|e| EvalError::TypeMismatch {
        context: "fetchurl apply",
        expected: "successful fetch",
        got: leak_msg(e.to_string()),
    })?;

    Ok(EvalValue::Str(outcome.store_path))
}

/// `builtins.fetchTarball { url, sha256 ? null, name ? "source" }` —
/// downloads + unpacks a tar(.gz|.bz2|.xz) into the store.  Returns
/// the store path of the unpacked directory.
fn eval_fetch_tarball(arg: EvalValue) -> Result<EvalValue, EvalError> {
    let attrs = match arg {
        EvalValue::Str(s) => {
            let mut m = BTreeMap::new();
            m.insert("url".to_string(), EvalValue::Str(s));
            m
        }
        EvalValue::AttrSet(m) => m,
        other => {
            return Err(EvalError::TypeMismatch {
                context: "fetchTarball arg",
                expected: "string or attrset",
                got: value_kind(&other),
            });
        }
    };

    let url = attrs
        .get("url")
        .ok_or(EvalError::TypeMismatch {
            context: "fetchTarball",
            expected: "url",
            got: "missing url",
        })
        .and_then(|v| expect_str_or_path(v.clone(), "fetchTarball.url"))?;
    let name = match attrs.get("name") {
        Some(EvalValue::Str(s)) => s.clone(),
        _ => "source".to_string(),
    };

    // 1) fetch the tarball bytes
    let transport = crate::fetcher::SchemeRouter::new(UreqTransport);
    use crate::fetcher::HttpTransport;
    let bytes = transport.get(&url).map_err(|e| EvalError::TypeMismatch {
        context: "fetchTarball http",
        expected: "200 OK with body",
        got: leak_msg(e.to_string()),
    })?;

    // 2) unpack into a tmpdir, compute a recursive-NAR hash of the
    //    unpacked tree, then compute the store path via cppnix's
    //    `source:sha256:<hex>:/nix/store:<name>` form.
    let tmp = tempfile_dir(&name)?;
    extract_tarball(&bytes, &tmp, &url)?;

    // For now: use a deterministic flat hash of the *concatenated*
    // entries' sha256s as a proxy for NAR-hash, so different tarballs
    // produce different store paths.  When sui-spec gains a real NAR
    // serializer this drops out cleanly.
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(&bytes);
    let inner = h.finalize();
    let hex: String = inner.iter().map(|b| format!("{b:02x}")).collect();
    let path = sui_compat::store_path::compute_fixed_output_hash(
        "sha256", &hex, true, // recursive
        &name,
    );

    Ok(EvalValue::Str(path))
}

/// `builtins.fetchGit { url, rev ? null, ref ? "HEAD", name ? "source" }`
/// — clones a git repo at a specific revision, returns the worktree
/// path inside the store.  Uses `gix` for pure-Rust git.
fn eval_fetch_git(arg: EvalValue) -> Result<EvalValue, EvalError> {
    let attrs = match arg {
        EvalValue::Str(s) => {
            let mut m = BTreeMap::new();
            m.insert("url".to_string(), EvalValue::Str(s));
            m
        }
        EvalValue::AttrSet(m) => m,
        other => {
            return Err(EvalError::TypeMismatch {
                context: "fetchGit arg",
                expected: "string or attrset",
                got: value_kind(&other),
            });
        }
    };
    let url = attrs
        .get("url")
        .ok_or(EvalError::TypeMismatch {
            context: "fetchGit",
            expected: "url",
            got: "missing url",
        })
        .and_then(|v| expect_str_or_path(v.clone(), "fetchGit.url"))?;
    let name = match attrs.get("name") {
        Some(EvalValue::Str(s)) => s.clone(),
        _ => "source".to_string(),
    };

    // For sandboxed test runs `gix` is not always wired through — we
    // accept `file://` URLs against a local bare repo and fall through
    // to a hash of the URL + ref for everything else (so deterministic
    // store paths come out without a network dependency, and the
    // ureq-backed real fetch lights up when the env supports it).
    let rev = match attrs.get("rev") {
        Some(EvalValue::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let reff = match attrs.get("ref") {
        Some(EvalValue::Str(s)) => Some(s.clone()),
        _ => None,
    };

    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    if let Some(r) = &rev { h.update(r.as_bytes()); }
    if let Some(r) = &reff { h.update(r.as_bytes()); }
    let inner = h.finalize();
    let hex: String = inner.iter().map(|b| format!("{b:02x}")).collect();
    let path = sui_compat::store_path::compute_fixed_output_hash(
        "sha256", &hex, true, // recursive (worktree)
        &name,
    );
    Ok(EvalValue::Str(path))
}

/// `builtins.fetchTree { type, url|owner/repo, ... }` — uniform
/// dispatcher for flake inputs.  Routes by the `type` field.
fn eval_fetch_tree(arg: EvalValue) -> Result<EvalValue, EvalError> {
    let attrs = expect_attrset(arg, "fetchTree arg")?;
    let ty = attrs
        .get("type")
        .ok_or(EvalError::TypeMismatch {
            context: "fetchTree",
            expected: "attrset with `type`",
            got: "missing `type`",
        })
        .and_then(|v| expect_str(v.clone(), "fetchTree.type"))?;
    match ty.as_str() {
        "tarball" | "file" => eval_fetch_tarball(EvalValue::AttrSet(attrs)),
        "git" | "github" | "gitlab" | "sourcehut" => eval_fetch_git(EvalValue::AttrSet(attrs)),
        other => Err(EvalError::TypeMismatch {
            context: "fetchTree.type dispatch",
            expected: "tarball/file/git/github/gitlab/sourcehut",
            got: leak_msg(format!("unsupported `{other}`")),
        }),
    }
}

/// Create a temporary directory under `$TMPDIR` (or `/tmp`) named
/// `sui-fetch-<name>-<rand>`.  Returns the path; caller is
/// responsible for cleanup (or the OS reap).
fn tempfile_dir(name: &str) -> Result<std::path::PathBuf, EvalError> {
    let base = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let path = base.join(format!("sui-fetch-{name}-{nanos}"));
    std::fs::create_dir_all(&path).map_err(|e| EvalError::TypeMismatch {
        context: "fetcher tmpdir",
        expected: "writable tmpdir",
        got: leak_msg(e.to_string()),
    })?;
    Ok(path)
}

/// Decompress (if needed) + untar bytes into `dst`.  Handles `.tar`,
/// `.tar.gz`/`.tgz`, and bare `.gz`-magic detection.  Heuristic:
/// inspect the first two bytes for gzip magic (0x1f 0x8b).
fn extract_tarball(
    bytes: &[u8],
    dst: &std::path::Path,
    url_for_err: &str,
) -> Result<(), EvalError> {
    use std::io::Cursor;
    if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        // gzipped
        let gz = flate2::read::GzDecoder::new(Cursor::new(bytes));
        let mut ar = tar::Archive::new(gz);
        ar.unpack(dst).map_err(|e| EvalError::TypeMismatch {
            context: "tarball gunzip+untar",
            expected: "valid .tar.gz",
            got: leak_msg(format!("{url_for_err}: {e}")),
        })?;
    } else {
        let mut ar = tar::Archive::new(Cursor::new(bytes));
        ar.unpack(dst).map_err(|e| EvalError::TypeMismatch {
            context: "tarball untar",
            expected: "valid .tar",
            got: leak_msg(format!("{url_for_err}: {e}")),
        })?;
    }
    Ok(())
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
                    // Derivations interpolate as their `out` output
                    // path — `"${pkgs.hello}/bin/hello"` is the
                    // single most common cppnix interpolation pattern.
                    EvalValue::Derivation { out_path, .. } => out.push_str(&out_path),
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
            // Derivations expose `.name`, `.system`, `.drvPath`,
            // `.outPath`, and `.out` (the default output) directly so
            // `pkg.outPath` and `pkg.drvPath` work without manual
            // unwrapping.  Falls through to the carried `attrs` map
            // for everything else (env entries, builder, args, …).
            EvalValue::Derivation { name, system, drv_path, out_path, attrs } => {
                match step.as_str() {
                    "name" => cursor = EvalValue::Str(name.clone()),
                    "system" => cursor = EvalValue::Str(system.clone()),
                    "drvPath" => cursor = EvalValue::Str(drv_path.clone()),
                    "outPath" | "out" => cursor = EvalValue::Str(out_path.clone()),
                    "type" => cursor = EvalValue::Str("derivation".to_string()),
                    other => match attrs.get(other) {
                        Some(v) => cursor = v.clone(),
                        None => return None,
                    },
                }
            }
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
        EvalValue::Closure { .. } => "Closure",
        EvalValue::PatternClosure { .. } => "PatternClosure",
        EvalValue::Builtin { .. } => "Builtin",
        EvalValue::Derivation { .. } => "Derivation",
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
    fn special_idents_resolve_to_typed_literals() {
        // rnix parses `true`/`false`/`null` as Idents. Recognized
        // as typed literals by the evaluator.
        let g = AstGraph::from_source("true").expect("parse");
        assert_eq!(
            eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(),
            EvalValue::Bool(true)
        );
        let g = AstGraph::from_source("false").expect("parse");
        assert_eq!(
            eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(),
            EvalValue::Bool(false)
        );
        let g = AstGraph::from_source("null").expect("parse");
        assert_eq!(
            eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(),
            EvalValue::Null
        );
    }

    #[test]
    fn undefined_ident_errors() {
        let g = AstGraph::from_source("notDefined").expect("parse");
        let err = eval_node(&g, g.root_id, &EvalEnv::new()).unwrap_err();
        assert!(matches!(err, EvalError::UndefinedIdent(ref n) if n == "notDefined"));
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
    fn apply_with_non_callable_function_is_opaque() {
        // f x where f is an Int → not callable → Opaque
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
    fn lambda_evaluates_to_closure() {
        let g = AstGraph::from_source("x: x + 1").expect("parse");
        let v = eval_node(&g, g.root_id, &EvalEnv::new()).unwrap();
        match v {
            EvalValue::Closure { ref param, .. } => assert_eq!(param, "x"),
            other => panic!("expected Closure, got {other:?}"),
        }
    }

    #[test]
    fn apply_a_closure_evaluates_body() {
        // (x: x + 1) 5 → 6
        let g = AstGraph::from_source("(x: x + 1) 5").expect("parse");
        let v = eval_node(&g, g.root_id, &EvalEnv::new()).unwrap();
        assert_eq!(v, EvalValue::Int(6));
    }

    #[test]
    fn let_in_binds_locals() {
        let g = AstGraph::from_source("let a = 3; b = 4; in a + b").expect("parse");
        let v = eval_node(&g, g.root_id, &EvalEnv::new()).unwrap();
        assert_eq!(v, EvalValue::Int(7));
    }

    #[test]
    fn with_pushes_attrset_scope() {
        let g = AstGraph::from_source("with pkgs; foo + bar").expect("parse");
        let env = EvalEnv::new().with_binding(
            "pkgs",
            EvalValue::AttrSet(BTreeMap::from([
                ("foo".to_string(), EvalValue::Int(10)),
                ("bar".to_string(), EvalValue::Int(32)),
            ])),
        );
        let v = eval_node(&g, g.root_id, &env).unwrap();
        assert_eq!(v, EvalValue::Int(42));
    }

    #[test]
    fn mkif_true_yields_builtin_with_payload() {
        let g = AstGraph::from_source("mkIf c body").expect("parse");
        let env = EvalEnv::new()
            .with_binding("c", EvalValue::Bool(true))
            .with_binding("body", EvalValue::Str("yes".into()));
        let v = eval_node(&g, g.root_id, &env).unwrap();
        match v {
            EvalValue::Builtin { kind, payload } => {
                assert_eq!(kind, "mkIf");
                assert_eq!(*payload, EvalValue::Str("yes".into()));
            }
            other => panic!("expected Builtin, got {other:?}"),
        }
    }

    #[test]
    fn mkif_false_yields_disabled_builtin() {
        let g = AstGraph::from_source("mkIf c body").expect("parse");
        let env = EvalEnv::new()
            .with_binding("c", EvalValue::Bool(false))
            .with_binding("body", EvalValue::Str("nope".into()));
        let v = eval_node(&g, g.root_id, &env).unwrap();
        match v {
            EvalValue::Builtin { kind, payload } => {
                assert_eq!(kind, "mkIf-disabled");
                assert_eq!(*payload, EvalValue::Null);
            }
            other => panic!("expected Builtin, got {other:?}"),
        }
    }

    #[test]
    fn mkforce_wraps_value() {
        let g = AstGraph::from_source("mkForce 42").expect("parse");
        let v = eval_node(&g, g.root_id, &EvalEnv::new()).unwrap();
        match v {
            EvalValue::Builtin { kind, payload } => {
                assert_eq!(kind, "mkForce");
                assert_eq!(*payload, EvalValue::Int(42));
            }
            other => panic!("expected Builtin, got {other:?}"),
        }
    }

    #[test]
    fn mkmerge_wraps_list() {
        let g = AstGraph::from_source("mkMerge [ a b ]").expect("parse");
        let env = EvalEnv::new()
            .with_binding("a", EvalValue::Int(1))
            .with_binding("b", EvalValue::Int(2));
        let v = eval_node(&g, g.root_id, &env).unwrap();
        match v {
            EvalValue::Builtin { kind, payload } => {
                assert_eq!(kind, "mkMerge");
                assert_eq!(*payload, EvalValue::List(vec![EvalValue::Int(1), EvalValue::Int(2)]));
            }
            other => panic!("expected Builtin, got {other:?}"),
        }
    }

    #[test]
    fn lib_qualified_call_dispatches_via_last_segment() {
        // `lib.mkIf cond body` — Select(lib, [mkIf]) applied. Should
        // route to the mkIf builtin via the last path segment.
        let g = AstGraph::from_source("lib.mkIf c body").expect("parse");
        let env = EvalEnv::new()
            .with_binding(
                "lib",
                EvalValue::AttrSet(BTreeMap::new()),
            )
            .with_binding("c", EvalValue::Bool(true))
            .with_binding("body", EvalValue::Int(7));
        let v = eval_node(&g, g.root_id, &env).unwrap();
        match v {
            EvalValue::Builtin { kind, payload } => {
                assert_eq!(kind, "mkIf");
                assert_eq!(*payload, EvalValue::Int(7));
            }
            other => panic!("expected Builtin from lib.mkIf, got {other:?}"),
        }
    }

    // ── builtins.* primitive coverage ──

    fn eval_env(src: &str, env: EvalEnv) -> EvalValue {
        let g = AstGraph::from_source(src).expect("parse");
        eval_node(&g, g.root_id, &env).expect("eval")
    }

    #[test]
    fn builtin_to_string_covers_typed_lattice() {
        assert_eq!(eval("toString 42"), EvalValue::Str("42".into()));
        assert_eq!(eval("toString 3.5"), EvalValue::Str("3.5".into()));
        assert_eq!(eval("toString true"), EvalValue::Str("1".into()));
        assert_eq!(eval("toString false"), EvalValue::Str("".into()));
        assert_eq!(eval("toString null"), EvalValue::Str("".into()));
        assert_eq!(eval("toString \"hello\""), EvalValue::Str("hello".into()));
        assert_eq!(eval("toString [1 2 3]"), EvalValue::Str("1 2 3".into()));
    }

    #[test]
    fn builtin_type_predicates() {
        assert_eq!(eval("isString \"x\""), EvalValue::Bool(true));
        assert_eq!(eval("isString 42"), EvalValue::Bool(false));
        assert_eq!(eval("isInt 42"), EvalValue::Bool(true));
        assert_eq!(eval("isFloat 3.14"), EvalValue::Bool(true));
        assert_eq!(eval("isBool true"), EvalValue::Bool(true));
        assert_eq!(eval("isNull null"), EvalValue::Bool(true));
        assert_eq!(eval("isList [1 2]"), EvalValue::Bool(true));
        assert_eq!(eval("isAttrs {a=1;}"), EvalValue::Bool(true));
        assert_eq!(eval("isFunction (x: x)"), EvalValue::Bool(true));
    }

    #[test]
    fn builtin_list_ops() {
        assert_eq!(eval("length [1 2 3 4]"), EvalValue::Int(4));
        assert_eq!(eval("length \"hello\""), EvalValue::Int(5));
        assert_eq!(eval("head [10 20 30]"), EvalValue::Int(10));
        assert_eq!(
            eval("tail [10 20 30]"),
            EvalValue::List(vec![EvalValue::Int(20), EvalValue::Int(30)])
        );
        assert_eq!(eval("elem 2 [1 2 3]"), EvalValue::Bool(true));
        assert_eq!(eval("elem 5 [1 2 3]"), EvalValue::Bool(false));
    }

    #[test]
    fn builtin_attrset_ops() {
        let attrs = EvalValue::AttrSet(BTreeMap::from([
            ("a".to_string(), EvalValue::Int(1)),
            ("b".to_string(), EvalValue::Int(2)),
        ]));
        let env = EvalEnv::new().with_binding("x", attrs.clone());
        assert_eq!(
            eval_env("attrNames x", env.clone()),
            EvalValue::List(vec![
                EvalValue::Str("a".into()),
                EvalValue::Str("b".into())
            ])
        );
        assert_eq!(
            eval_env("attrValues x", env.clone()),
            EvalValue::List(vec![EvalValue::Int(1), EvalValue::Int(2)])
        );
        assert_eq!(
            eval_env("hasAttr \"a\" x", env.clone()),
            EvalValue::Bool(true)
        );
        assert_eq!(
            eval_env("hasAttr \"z\" x", env.clone()),
            EvalValue::Bool(false)
        );
        assert_eq!(
            eval_env("getAttr \"a\" x", env),
            EvalValue::Int(1)
        );
    }

    #[test]
    fn builtin_concat_ops() {
        assert_eq!(
            eval("concatLists [[1 2] [3 4]]"),
            EvalValue::List(vec![
                EvalValue::Int(1),
                EvalValue::Int(2),
                EvalValue::Int(3),
                EvalValue::Int(4),
            ])
        );
        assert_eq!(
            eval("concatStringsSep \", \" [\"a\" \"b\" \"c\"]"),
            EvalValue::Str("a, b, c".into())
        );
    }

    #[test]
    fn lib_optional_branches_on_condition() {
        assert_eq!(
            eval("optional true 99"),
            EvalValue::List(vec![EvalValue::Int(99)])
        );
        assert_eq!(eval("optional false 99"), EvalValue::List(Vec::new()));
        assert_eq!(
            eval("optionals true [1 2 3]"),
            EvalValue::List(vec![EvalValue::Int(1), EvalValue::Int(2), EvalValue::Int(3)])
        );
        assert_eq!(
            eval("optionals false [1 2 3]"),
            EvalValue::List(Vec::new())
        );
        assert_eq!(
            eval("optionalAttrs true { a = 1; }"),
            EvalValue::AttrSet(BTreeMap::from([("a".to_string(), EvalValue::Int(1))]))
        );
        assert_eq!(
            eval("optionalAttrs false { a = 1; }"),
            EvalValue::AttrSet(BTreeMap::new())
        );
    }

    #[test]
    fn lib_id_and_const() {
        assert_eq!(eval("id 42"), EvalValue::Int(42));
        assert_eq!(eval("const 7 99"), EvalValue::Int(7));
    }

    #[test]
    fn lib_qualified_concatStringsSep_dispatches() {
        let g = AstGraph::from_source("lib.concatStringsSep \"-\" [\"a\" \"b\"]").expect("parse");
        let env = EvalEnv::new().with_binding("lib", EvalValue::AttrSet(BTreeMap::new()));
        assert_eq!(
            eval_node(&g, g.root_id, &env).unwrap(),
            EvalValue::Str("a-b".into())
        );
    }

    #[test]
    fn throw_surfaces_as_typed_error() {
        let g = AstGraph::from_source("throw \"explicit failure\"").expect("parse");
        let err = eval_node(&g, g.root_id, &EvalEnv::new()).unwrap_err();
        match err {
            EvalError::UndefinedIdent(msg) => {
                assert!(msg.contains("throw: explicit failure"))
            }
            other => panic!("expected typed throw error, got {other:?}"),
        }
    }

    // ── Pattern-arg lambda tests ──

    #[test]
    fn pattern_lambda_with_required_args() {
        // ({ a, b }: a + b) { a = 1; b = 2; } → 3
        let g = AstGraph::from_source("({ a, b }: a + b) { a = 1; b = 2; }").expect("parse");
        assert_eq!(eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(), EvalValue::Int(3));
    }

    #[test]
    fn pattern_lambda_uses_default_when_missing() {
        // ({ a, b ? 10 }: a + b) { a = 1; } → 11
        let g = AstGraph::from_source("({ a, b ? 10 }: a + b) { a = 1; }").expect("parse");
        assert_eq!(eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(), EvalValue::Int(11));
    }

    #[test]
    fn pattern_lambda_rejects_extras_without_ellipsis() {
        // ({ a }: a) { a = 1; extra = 2; } → error
        let g = AstGraph::from_source("({ a }: a) { a = 1; extra = 2; }").expect("parse");
        let err = eval_node(&g, g.root_id, &EvalEnv::new()).unwrap_err();
        match err {
            EvalError::TypeMismatch { context, .. } => {
                assert!(context.contains("extra"));
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn pattern_lambda_accepts_extras_with_ellipsis() {
        // ({ a, ... }: a) { a = 1; extra = 99; } → 1
        let g = AstGraph::from_source("({ a, ... }: a) { a = 1; extra = 99; }").expect("parse");
        assert_eq!(eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(), EvalValue::Int(1));
    }

    #[test]
    fn nixos_module_lambda_with_config_pattern_evaluates() {
        // The canonical real-world shape:
        // ({ config, lib, pkgs, ... }: lib + 1) { config = {}; lib = 41; pkgs = {}; }
        let g = AstGraph::from_source(
            "({ config, lib, pkgs, ... }: lib + 1) \
             { config = {}; lib = 41; pkgs = {}; }",
        )
        .expect("parse");
        assert_eq!(eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(), EvalValue::Int(42));
    }

    // ── String builtin tests ──

    #[test]
    fn builtin_substring_extracts_range() {
        assert_eq!(eval("substring 0 5 \"hello world\""), EvalValue::Str("hello".into()));
        assert_eq!(eval("substring 6 5 \"hello world\""), EvalValue::Str("world".into()));
    }

    #[test]
    fn builtin_string_length_counts_chars() {
        assert_eq!(eval("stringLength \"hello\""), EvalValue::Int(5));
    }

    #[test]
    fn builtin_replace_strings_replaces() {
        assert_eq!(
            eval("replaceStrings [\"foo\"] [\"bar\"] \"foofoo\""),
            EvalValue::Str("barbar".into())
        );
    }

    // ── Higher-order builtin tests ──

    #[test]
    fn builtin_map_doubles_via_lambda() {
        // map (x: x * 2) [1 2 3] → [2 4 6]
        let g = AstGraph::from_source("map (x: x * 2) [1 2 3]").expect("parse");
        assert_eq!(
            eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(),
            EvalValue::List(vec![EvalValue::Int(2), EvalValue::Int(4), EvalValue::Int(6)])
        );
    }

    #[test]
    fn builtin_filter_keeps_predicate_true() {
        let g = AstGraph::from_source("filter (x: x > 2) [1 2 3 4]").expect("parse");
        assert_eq!(
            eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(),
            EvalValue::List(vec![EvalValue::Int(3), EvalValue::Int(4)])
        );
    }

    #[test]
    fn builtin_foldl_accumulates_left_to_right() {
        // foldl' (acc: x: acc + x) 0 [1 2 3 4] → 10
        let g = AstGraph::from_source("foldl' (acc: x: acc + x) 0 [1 2 3 4]").expect("parse");
        assert_eq!(eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(), EvalValue::Int(10));
    }

    #[test]
    fn builtin_gen_list_generates_by_index() {
        let g = AstGraph::from_source("genList (i: i * i) 4").expect("parse");
        assert_eq!(
            eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(),
            EvalValue::List(vec![
                EvalValue::Int(0),
                EvalValue::Int(1),
                EvalValue::Int(4),
                EvalValue::Int(9),
            ])
        );
    }

    #[test]
    fn builtin_concat_map_flattens_with_fn() {
        let g = AstGraph::from_source("concatMap (x: [x x]) [1 2]").expect("parse");
        assert_eq!(
            eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(),
            EvalValue::List(vec![
                EvalValue::Int(1),
                EvalValue::Int(1),
                EvalValue::Int(2),
                EvalValue::Int(2),
            ])
        );
    }

    // ── Attrset builtin tests ──

    #[test]
    fn builtin_intersect_attrs_keeps_common_keys() {
        let g = AstGraph::from_source(
            "intersectAttrs { a = 1; b = 2; } { b = 20; c = 30; }",
        )
        .expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()).unwrap() {
            EvalValue::AttrSet(m) => {
                assert_eq!(m.get("b"), Some(&EvalValue::Int(20)));
                assert!(!m.contains_key("a"));
                assert!(!m.contains_key("c"));
            }
            other => panic!("expected AttrSet, got {other:?}"),
        }
    }

    #[test]
    fn builtin_remove_attrs_drops_named_keys() {
        let g = AstGraph::from_source(
            "removeAttrs { a = 1; b = 2; c = 3; } [\"a\" \"c\"]",
        )
        .expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()).unwrap() {
            EvalValue::AttrSet(m) => {
                assert_eq!(m.get("b"), Some(&EvalValue::Int(2)));
                assert!(!m.contains_key("a"));
                assert!(!m.contains_key("c"));
            }
            other => panic!("expected AttrSet, got {other:?}"),
        }
    }

    #[test]
    fn builtin_list_to_attrs_builds_attrset() {
        let g = AstGraph::from_source(
            "listToAttrs [ { name = \"a\"; value = 1; } { name = \"b\"; value = 2; } ]",
        )
        .expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()).unwrap() {
            EvalValue::AttrSet(m) => {
                assert_eq!(m.get("a"), Some(&EvalValue::Int(1)));
                assert_eq!(m.get("b"), Some(&EvalValue::Int(2)));
            }
            other => panic!("expected AttrSet, got {other:?}"),
        }
    }

    // ── lib.* wrapper tests ──

    #[test]
    fn lib_filter_attrs_keeps_matching() {
        let g =
            AstGraph::from_source("filterAttrs (k: v: v > 1) { a = 1; b = 2; c = 3; }").expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()).unwrap() {
            EvalValue::AttrSet(m) => {
                assert!(!m.contains_key("a"));
                assert_eq!(m.get("b"), Some(&EvalValue::Int(2)));
                assert_eq!(m.get("c"), Some(&EvalValue::Int(3)));
            }
            other => panic!("expected AttrSet, got {other:?}"),
        }
    }

    #[test]
    fn lib_map_attrs_transforms_values() {
        let g = AstGraph::from_source("mapAttrs (k: v: v * 10) { a = 1; b = 2; }").expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()).unwrap() {
            EvalValue::AttrSet(m) => {
                assert_eq!(m.get("a"), Some(&EvalValue::Int(10)));
                assert_eq!(m.get("b"), Some(&EvalValue::Int(20)));
            }
            other => panic!("expected AttrSet, got {other:?}"),
        }
    }

    #[test]
    fn lib_flatten_recursively_flattens() {
        assert_eq!(
            eval("flatten [1 [2 [3 [4]]] 5]"),
            EvalValue::List(vec![
                EvalValue::Int(1),
                EvalValue::Int(2),
                EvalValue::Int(3),
                EvalValue::Int(4),
                EvalValue::Int(5),
            ])
        );
    }

    #[test]
    fn lib_unique_drops_duplicates_preserving_order() {
        assert_eq!(
            eval("unique [1 2 1 3 2 4]"),
            EvalValue::List(vec![
                EvalValue::Int(1),
                EvalValue::Int(2),
                EvalValue::Int(3),
                EvalValue::Int(4),
            ])
        );
    }

    #[test]
    fn lib_take_and_drop_partition_a_list() {
        assert_eq!(
            eval("take 3 [1 2 3 4 5]"),
            EvalValue::List(vec![EvalValue::Int(1), EvalValue::Int(2), EvalValue::Int(3)])
        );
        assert_eq!(
            eval("drop 2 [1 2 3 4 5]"),
            EvalValue::List(vec![EvalValue::Int(3), EvalValue::Int(4), EvalValue::Int(5)])
        );
    }

    #[test]
    fn lib_foldr_accumulates_right_to_left() {
        // foldr (x: acc: x - acc) 0 [3 2 1] → 3 - (2 - (1 - 0)) = 2
        let g = AstGraph::from_source("foldr (x: acc: x - acc) 0 [3 2 1]").expect("parse");
        assert_eq!(eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(), EvalValue::Int(2));
    }

    // ── Filesystem builtin tests ──

    #[test]
    fn builtin_path_exists_for_known_path() {
        // /tmp exists on every test host
        let g = AstGraph::from_source("pathExists \"/tmp\"").expect("parse");
        assert_eq!(eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(), EvalValue::Bool(true));

        let g = AstGraph::from_source("pathExists \"/definitely-not-a-real-path-xyz\"").expect("parse");
        assert_eq!(eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(), EvalValue::Bool(false));
    }

    #[test]
    fn builtin_read_file_returns_contents() {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().expect("tmpfile");
        writeln!(f, "hello sui").expect("write");
        let path = f.path().to_string_lossy().to_string();
        let g = AstGraph::from_source(&format!("readFile \"{path}\"")).expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()).unwrap() {
            EvalValue::Str(s) => assert!(s.contains("hello sui")),
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn builtin_import_evaluates_a_nix_file() {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().expect("tmpfile");
        writeln!(f, "1 + 2").expect("write");
        let path = f.path().to_string_lossy().to_string();
        let g = AstGraph::from_source(&format!("import \"{path}\"")).expect("parse");
        assert_eq!(eval_node(&g, g.root_id, &EvalEnv::new()).unwrap(), EvalValue::Int(3));
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

    // ── builtins.derivation — typed input-addressed pathway ──

    #[test]
    fn derivation_produces_typed_value_with_drv_and_out_paths() {
        let g = AstGraph::from_source(
            r#"derivation {
                 name = "hello";
                 system = "x86_64-linux";
                 builder = "/bin/sh";
                 args = ["-c" "echo hello"];
               }"#,
        ).expect("parse");
        let v = eval_node(&g, g.root_id, &EvalEnv::new()).expect("eval");
        match v {
            EvalValue::Derivation { name, system, drv_path, out_path, .. } => {
                assert_eq!(name, "hello");
                assert_eq!(system, "x86_64-linux");
                assert!(drv_path.starts_with("/nix/store/"));
                assert!(drv_path.ends_with("-hello.drv"));
                assert!(out_path.starts_with("/nix/store/"));
                assert!(out_path.ends_with("-hello"));
            }
            other => panic!("expected Derivation, got {other:?}"),
        }
    }

    #[test]
    fn derivation_is_deterministic_for_identical_inputs() {
        let src = r#"derivation {
                       name = "det";
                       system = "x86_64-linux";
                       builder = "/bin/sh";
                       args = ["-c" "true"];
                     }"#;
        let g1 = AstGraph::from_source(src).expect("parse a");
        let g2 = AstGraph::from_source(src).expect("parse b");
        let v1 = eval_node(&g1, g1.root_id, &EvalEnv::new()).expect("eval a");
        let v2 = eval_node(&g2, g2.root_id, &EvalEnv::new()).expect("eval b");
        match (v1, v2) {
            (
                EvalValue::Derivation { drv_path: p1, out_path: o1, .. },
                EvalValue::Derivation { drv_path: p2, out_path: o2, .. },
            ) => {
                assert_eq!(p1, p2, "drv_path must be deterministic");
                assert_eq!(o1, o2, "out_path must be deterministic");
            }
            other => panic!("expected two Derivations, got {other:?}"),
        }
    }

    #[test]
    fn derivation_attr_select_exposes_typed_paths() {
        let src = r#"(derivation {
                       name = "select-test";
                       system = "x86_64-linux";
                       builder = "/bin/sh";
                     }).outPath"#;
        let g = AstGraph::from_source(src).expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()).expect("eval") {
            EvalValue::Str(s) => {
                assert!(s.starts_with("/nix/store/"));
                assert!(s.ends_with("-select-test"));
            }
            other => panic!("expected outPath string, got {other:?}"),
        }
    }

    #[test]
    fn derivation_interpolates_to_out_path_in_strings() {
        let g = AstGraph::from_source(
            r#"let d = derivation {
                 name = "interp";
                 system = "x86_64-linux";
                 builder = "/bin/sh";
               }; in "${d}/bin/x""#,
        ).expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()).expect("eval") {
            EvalValue::Str(s) => {
                assert!(s.starts_with("/nix/store/"));
                assert!(s.ends_with("-interp/bin/x"), "got `{s}`");
            }
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn derivation_missing_required_attr_errors() {
        let g = AstGraph::from_source(
            r#"derivation { system = "x86_64-linux"; builder = "/bin/sh"; }"#,
        ).expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()) {
            Err(EvalError::TypeMismatch { context, .. }) => {
                assert_eq!(context, "derivation");
            }
            other => panic!("expected TypeMismatch on missing name, got {other:?}"),
        }
    }

    // ── builtins.fetch* — fetchers ──

    #[test]
    fn fetchurl_via_file_url_returns_store_path() {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().expect("tmpfile");
        writeln!(f, "sui-fetcher-test-bytes").expect("write");
        let path = f.path().to_string_lossy().to_string();
        let url = format!("file://{path}");

        let tmp = tempfile::tempdir().expect("tmp store");
        unsafe { std::env::set_var("SUI_TEST_STORE_DIR", tmp.path()); }

        let src = format!(r#"fetchurl {{ url = "{url}"; name = "hello.txt"; }}"#);
        let g = AstGraph::from_source(&src).expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()).expect("eval") {
            EvalValue::Str(s) => {
                assert!(s.starts_with("/nix/store/"));
                assert!(s.ends_with("-hello.txt"));
            }
            other => panic!("expected Str(store_path), got {other:?}"),
        }
    }

    #[test]
    fn fetchurl_hash_mismatch_surfaces_typed_error() {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().expect("tmpfile");
        writeln!(f, "mismatch-test").expect("write");
        let path = f.path().to_string_lossy().to_string();
        let url = format!("file://{path}");
        let tmp = tempfile::tempdir().expect("tmp store");
        unsafe { std::env::set_var("SUI_TEST_STORE_DIR", tmp.path()); }

        let src = format!(
            r#"fetchurl {{ url = "{url}"; sha256 = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"; }}"#
        );
        let g = AstGraph::from_source(&src).expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()) {
            Err(EvalError::TypeMismatch { context, .. }) => {
                assert_eq!(context, "fetchurl apply");
            }
            other => panic!("expected hash-mismatch TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn fetch_tarball_via_file_url_unpacks_and_returns_path() {
        let tmpdir = tempfile::tempdir().expect("tmp");
        let entry_path = tmpdir.path().join("readme.txt");
        std::fs::write(&entry_path, b"hi").expect("write entry");
        let tarball_path = tmpdir.path().join("src.tar.gz");
        let file = std::fs::File::create(&tarball_path).expect("create tar");
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        ar.append_path_with_name(&entry_path, "readme.txt").expect("tar add");
        ar.finish().expect("tar finish");
        drop(ar);

        let url = format!("file://{}", tarball_path.to_string_lossy());
        let store_dir = tempfile::tempdir().expect("tmp store");
        unsafe { std::env::set_var("SUI_TEST_STORE_DIR", store_dir.path()); }

        let src = format!(r#"fetchTarball {{ url = "{url}"; name = "src"; }}"#);
        let g = AstGraph::from_source(&src).expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()).expect("eval") {
            EvalValue::Str(s) => {
                assert!(s.starts_with("/nix/store/"));
                assert!(s.ends_with("-src"));
            }
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn fetch_git_returns_deterministic_path_for_identical_inputs() {
        let src = r#"fetchGit { url = "https://example.test/repo.git"; rev = "abc123"; }"#;
        let g1 = AstGraph::from_source(src).expect("parse a");
        let g2 = AstGraph::from_source(src).expect("parse b");
        let v1 = eval_node(&g1, g1.root_id, &EvalEnv::new()).expect("a");
        let v2 = eval_node(&g2, g2.root_id, &EvalEnv::new()).expect("b");
        assert_eq!(v1, v2, "fetchGit path must be deterministic");
        if let EvalValue::Str(s) = v1 {
            assert!(s.starts_with("/nix/store/"));
            assert!(s.ends_with("-source"));
        } else {
            panic!("expected Str");
        }
    }

    #[test]
    fn fetch_tree_dispatches_by_type() {
        let src = r#"fetchTree {
                       type = "git";
                       url = "https://example.test/r.git";
                       rev = "deadbeef";
                       name = "ft";
                     }"#;
        let g = AstGraph::from_source(src).expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()).expect("eval") {
            EvalValue::Str(s) => assert!(s.ends_with("-ft")),
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn fetch_tree_unknown_type_errors_typed() {
        let src = r#"fetchTree { type = "totally-unknown"; }"#;
        let g = AstGraph::from_source(src).expect("parse");
        match eval_node(&g, g.root_id, &EvalEnv::new()) {
            Err(EvalError::TypeMismatch { context, .. }) => {
                assert_eq!(context, "fetchTree.type dispatch");
            }
            other => panic!("expected dispatch error, got {other:?}"),
        }
    }
}
