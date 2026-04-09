//! Version builtins: compareVersions, parseDrvName, splitVersion.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    // compareVersions — compare version strings
    register_builtin(builtins, "compareVersions", |args| {
        let a = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "compareVersions<partial>",
            func: Rc::new(move |args2| {
                let b = args2[0].as_string()?;
                let result = compare_versions(&a, b);
                Ok(Value::Int(result))
            }),
        }))
    });

    // parseDrvName — parse "name-version" from package name
    register_builtin(builtins, "parseDrvName", |args| {
        let s = args[0].as_string()?;
        let (name, version) = parse_drv_name(s);
        let mut result = NixAttrs::new();
        result.insert("name".to_string(), Value::string(name));
        result.insert("version".to_string(), Value::string(version));
        Ok(Value::Attrs(result))
    });

    // splitVersion
    register_builtin(builtins, "splitVersion", |args| {
        let s = args[0].as_string()?;
        let parts = split_version(s);
        Ok(Value::List(parts.into_iter().map(Value::string).collect()))
    });
}
