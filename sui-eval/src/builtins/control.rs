//! Control flow builtins: tryEval, throw, abort, trace, warn, seq, deepSeq,
//! addErrorContext, break, traceVerbose.

use super::*;

/// Recursively force a value and all nested values (attrset values, list elements).
///
/// **Cycle-safe.** A self-referential structure — e.g. `let as = { x = 123; y = as; };
/// in builtins.deepSeq as 456` — is finite here because every attrset/list is
/// recorded by its `Rc` identity in `seen` and never descended into twice. This
/// mirrors cppnix's `forceValueDeep` `std::set<const Value*> seen`. Without it,
/// `deepSeq` on a cyclic value recurses forever; `stacker::maybe_grow` turns that
/// infinite recursion into an unbounded stack-grow HANG rather than a prompt
/// overflow (so it presents as a >60s wedge, not a crash).
///
/// Never-remove-from-`seen` is correct AND an optimization: a value shared across
/// two branches (a DAG, not a cycle) is deep-forced once — forcing the shared `Rc`
/// once forces it everywhere. cppnix does the same.
///
/// (2026-07-22: surfaced by the vendored CppNix `eval-okay-deepseq` corpus fixture
/// — nix returns 456, sui hung. STRATOSPHERE M3 test-expansion caught it.)
fn deep_force(val: &Value) -> Result<(), EvalError> {
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    deep_force_seen(val, &mut seen)
}

fn deep_force_seen(val: &Value, seen: &mut std::collections::HashSet<usize>) -> Result<(), EvalError> {
    stacker::maybe_grow(64 * 1024, 2 * 1024 * 1024, || {
        let forced = crate::eval::force_value(val)?;
        match &forced {
            Value::Attrs(attrs) => {
                // Break cycles: an attrset already being descended into is skipped.
                if !seen.insert(Rc::as_ptr(attrs) as usize) {
                    return Ok(());
                }
                for (_k, v) in attrs.iter() {
                    deep_force_seen(v, seen)?;
                }
            }
            Value::List(list) => {
                if !seen.insert(Rc::as_ptr(list) as usize) {
                    return Ok(());
                }
                for v in list.iter() {
                    deep_force_seen(v, seen)?;
                }
            }
            _ => {}
        }
        Ok(())
    })
}

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_builtin(builtins, "tryEval", |args| {
        // CppNix: tryEval ONLY catches `throw` and `assert` — NOT `abort`
        // (uncatchable) and NOT evaluation errors like AttrNotFound,
        // TypeMismatch, InfiniteRecursion, etc.
        // Catching all errors breaks the nixpkgs module system which uses
        // tryEval to detect if an option value throws, NOT to swallow
        // real evaluation errors.
        match crate::eval::force_value(&args[0]) {
            Ok(v) => {
                let mut result = NixAttrs::new();
                result.insert("success".to_string(), Value::Bool(true));
                result.insert("value".to_string(), v);
                Ok(Value::Attrs(Rc::new(result)))
            }
            Err(EvalError::Throw(_)) | Err(EvalError::AssertionFailed(_)) => {
                let mut result = NixAttrs::new();
                result.insert("success".to_string(), Value::Bool(false));
                result.insert("value".to_string(), Value::Bool(false));
                Ok(Value::Attrs(Rc::new(result)))
            }
            Err(e) => Err(e), // Propagate real evaluation errors
        }
    });
    register_builtin(builtins, "trace", |args| {
        let msg = crate::eval::force_value(&args[0])?;
        let msg_str = match &msg {
            Value::String(s) => s.chars.to_string(),
            other => format!("{other}"),
        };
        eprintln!("trace: {msg_str}");
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "trace<partial>",
            func: Rc::new(|args2| Ok(args2[0].clone())),
        })))
    });
    register_builtin(builtins, "warn", |args| {
        let msg = args[0].as_string()?.to_string();
        eprintln!("evaluation warning: {msg}");
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "warn<partial>",
            func: Rc::new(|args2| Ok(args2[0].clone())),
        })))
    });
    register_builtin(builtins, "traceVerbose", |args| {
        let msg = args[0].clone();
        if std::env::var("SUI_TRACE_VERBOSE").ok().as_deref() == Some("1") {
            eprintln!("trace: {msg}");
        }
        tracing::trace!("traceVerbose: {msg}");
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "traceVerbose<partial>",
            func: Rc::new(|args2| Ok(args2[0].clone())),
        })))
    });
    register_builtin(builtins, "break", |args| {
        tracing::debug!("break: {}", args[0]);
        Ok(args[0].clone())
    });
    register_builtin(builtins, "throw", |args| {
        let msg = args[0].as_string()?;
        Err(EvalError::Throw(format!("throw: {msg}")))
    });
    register_builtin(builtins, "abort", |args| {
        let msg = args[0].as_string()?;
        // `abort` is UNCATCHABLE — a distinct variant from `throw` so
        // `tryEval` does not swallow it (CppNix parity).
        Err(EvalError::Abort(format!("evaluation aborted with the following error message: '{msg}'")))
    });
    // seq: force first arg to WHNF, return second arg unchanged.
    // First arg is already forced by apply's force_value.
    register_builtin(builtins, "seq", |_args| {
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "seq<partial>",
            func: Rc::new(|args2| Ok(args2[0].clone())),
        })))
    });
    // deepSeq: recursively force first arg (all nested values), return second.
    // First arg is forced to WHNF by apply; we need to DEEPLY force it.
    register_builtin(builtins, "deepSeq", |args| {
        deep_force(&args[0])?;
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "deepSeq<partial>",
            func: Rc::new(|args2| Ok(args2[0].clone())),
        })))
    });

    // addErrorContext — wraps an expression with error context (passthrough in our impl).
    // CRITICAL: Do NOT force the context string eagerly. CppNix only
    // stringifies it when an error actually occurs. Eagerly forcing it
    // breaks nixpkgs lib/modules.nix where context strings reference
    // module config that isn't yet fully initialized (null attrs).
    register_builtin(builtins, "addErrorContext", |args| {
        let _ctx = &args[0]; // captured but not forced
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "addErrorContext<partial>",
            func: Rc::new(|args2| Ok(args2[0].clone())),
        })))
    });
}
