//! Control flow builtins: tryEval, throw, abort, trace, warn, seq, deepSeq,
//! addErrorContext, break, traceVerbose.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_builtin(builtins, "tryEval", |args| {
        match crate::eval::force_value(&args[0]) {
            Ok(v) => {
                let mut result = NixAttrs::new();
                result.insert("success".to_string(), Value::Bool(true));
                result.insert("value".to_string(), v);
                Ok(Value::Attrs(Rc::new(result)))
            }
            Err(_) => {
                let mut result = NixAttrs::new();
                result.insert("success".to_string(), Value::Bool(false));
                result.insert("value".to_string(), Value::Bool(false));
                Ok(Value::Attrs(Rc::new(result)))
            }
        }
    });
    register_builtin(builtins, "trace", |args| {
        let msg = args[0].clone();
        tracing::debug!("trace: {msg}");
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
        Err(EvalError::Throw(format!("abort: {msg}")))
    });
    register_builtin(builtins, "seq", |args| {
        let _forced = args[0].clone(); // force first arg
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "seq<partial>",
            func: Rc::new(|args2| Ok(args2[0].clone())),
        })))
    });
    register_builtin(builtins, "deepSeq", |args| {
        let _forced = args[0].clone(); // force first arg (deep in real impl)
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
