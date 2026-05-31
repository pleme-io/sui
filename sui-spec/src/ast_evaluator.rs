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
    /// Closure value — produced by evaluating a `Lambda` node.
    /// Captures the param name + body AST id + closure env at
    /// construction time. Applied later via `Apply` evaluation.
    /// Note: closures are *not* meaningfully serializable across
    /// processes (the body_node_id is only valid for the originating
    /// AstGraph). When persisted via `serde_json::to_vec`, they
    /// round-trip as opaque sentinels — the original closure is lost.
    /// That's acceptable today because closures don't normally land
    /// in `EnvSnapshot.config`; they're invoked to produce a primitive
    /// value before reaching env.
    Closure {
        param: String,
        body_node_id: NodeId,
        /// Snapshot of the env at closure creation. Stored as a
        /// flat map; reapplied at call time as the closure scope.
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
            // Capture the env in a flat map. Pattern-args (full
            // formal-args destructuring `{a, b, ...}: ...`) aren't
            // typed as a single `param` name — they only land here
            // when the lambda's wrapping isn't NixOS-module-shaped.
            // For those, we surface Opaque so the future VM
            // integration handles the long tail.
            match param {
                crate::ast_graph::LambdaParam::Ident(name) => {
                    Ok(EvalValue::Closure {
                        param: name.clone(),
                        body_node_id: *body,
                        captured_env: env.bindings.clone(),
                    })
                }
                crate::ast_graph::LambdaParam::Pattern { .. } => {
                    Ok(EvalValue::Opaque {
                        kind: "Lambda-Pattern".to_string(),
                        node_id: id,
                    })
                }
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

    // Closure invocation.
    let func_value = eval_at(ast, cursor, env, depth + 1);
    if let Ok(EvalValue::Closure {
        param,
        body_node_id,
        captured_env,
    }) = func_value
    {
        // Apply each argument in sequence (curried).
        let mut arg_iter = args.into_iter();
        let first_arg = match arg_iter.next() {
            Some(a) => eval_at(ast, a, env, depth + 1)?,
            None => return Ok(EvalValue::Null),
        };
        let mut call_env = EvalEnv {
            bindings: captured_env,
        };
        call_env.bindings.insert(param, first_arg);
        let mut result = eval_at(ast, body_node_id, &call_env, depth + 1)?;
        // Process remaining args (curried application).
        for next_arg in arg_iter {
            let arg_val = eval_at(ast, next_arg, env, depth + 1)?;
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
                    result = eval_at(ast, body_node_id, &next_env, depth + 1)?;
                }
                _ => {
                    return Ok(EvalValue::Opaque {
                        kind: "Apply-non-callable-result".to_string(),
                        node_id: function,
                    });
                }
            }
        }
        return Ok(result);
    }

    Ok(EvalValue::Opaque {
        kind: "Apply".to_string(),
        node_id: function,
    })
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

        _ => Ok(None),
    }
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
        // Cppnix's toString on attrsets calls the `__toString` field if
        // present; otherwise errors. We approximate: empty string.
        EvalValue::AttrSet(_) | EvalValue::Closure { .. }
        | EvalValue::Builtin { .. } | EvalValue::Opaque { .. } => {
            EvalValue::Str(String::new())
        }
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
        EvalValue::Closure { .. } => "Closure",
        EvalValue::Builtin { .. } => "Builtin",
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
