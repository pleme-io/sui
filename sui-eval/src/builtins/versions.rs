//! Version builtins: compareVersions, parseDrvName, splitVersion.
//!
//! Delegates to [`sui_compat::versions`] — the single typed
//! implementation shared with the bytecode VM.  Previous tree-walker-
//! local copies have been removed to eliminate engine drift.

use super::*;
use sui_compat::versions::{compare_versions, parse_drv_name, split_version};

pub(crate) fn register(builtins: &mut NixAttrs) {
    // compareVersions — compare version strings
    register_builtin(builtins, "compareVersions", |args| {
        let a = args[0].as_string()?.to_string();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "compareVersions<partial>",
            func: Rc::new(move |args2| {
                let b = args2[0].as_string()?;
                Ok(Value::Int(compare_versions(&a, b)))
            }),
        })))
    });

    // parseDrvName — parse "name-version" from package name
    register_builtin(builtins, "parseDrvName", |args| {
        let s = args[0].as_string()?;
        let (name, version) = parse_drv_name(s);
        let mut result = NixAttrs::new();
        result.insert("name".to_string(), Value::string(name));
        result.insert("version".to_string(), Value::string(version));
        Ok(Value::Attrs(Rc::new(result)))
    });

    // splitVersion
    register_builtin(builtins, "splitVersion", |args| {
        let s = args[0].as_string()?;
        let parts = split_version(s);
        Ok(Value::List(Rc::new(parts.into_iter().map(Value::string).collect())))
    });
}
