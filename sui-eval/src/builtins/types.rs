//! Type-checking builtins: typeOf, isAttrs, isList, isString, isInt, isFloat,
//! isBool, isFunction, isPath, isNull.

use super::*;

/// Type-checking builtins, registered from a declarative table.
const TYPE_CHECK_BUILTINS: &[BuiltinSpec] = &[
    BuiltinSpec { name: "typeOf", func: |args| Ok(Value::string(args[0].type_name())) },
    BuiltinSpec { name: "isNull", func: |args| Ok(Value::Bool(matches!(args[0], Value::Null))) },
    BuiltinSpec { name: "isInt",  func: |args| Ok(Value::Bool(matches!(args[0], Value::Int(_)))) },
    BuiltinSpec { name: "isFloat", func: |args| Ok(Value::Bool(matches!(args[0], Value::Float(_)))) },
    BuiltinSpec { name: "isBool", func: |args| Ok(Value::Bool(matches!(args[0], Value::Bool(_)))) },
    BuiltinSpec { name: "isString", func: |args| Ok(Value::Bool(matches!(args[0], Value::String(_)))) },
    BuiltinSpec { name: "isList", func: |args| Ok(Value::Bool(matches!(args[0], Value::List(_)))) },
    BuiltinSpec { name: "isAttrs", func: |args| Ok(Value::Bool(matches!(args[0], Value::Attrs(_)))) },
    BuiltinSpec { name: "isFunction", func: |args| Ok(Value::Bool(matches!(args[0], Value::Lambda(_) | Value::Builtin(_)))) },
    BuiltinSpec { name: "isPath", func: |args| Ok(Value::Bool(matches!(args[0], Value::Path(_)))) },
];

pub(crate) fn register(builtins: &mut NixAttrs) {
    for spec in TYPE_CHECK_BUILTINS {
        register_builtin(builtins, spec.name, spec.func);
    }
}
