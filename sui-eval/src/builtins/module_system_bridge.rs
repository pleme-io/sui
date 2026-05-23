//! Bridge between sui-eval's lazy `Value` type and
//! `sui_spec::module_system::eval_modules`.
//!
//! Exposes `builtins.sui.evalModules` — a Nix-callable surface
//! that takes a list of module attrsets, drives them through the
//! M2.1 minimal interpreter, and returns the merged config.  This
//! is the first integration bridge: substrate primitives crossing
//! into the sui-eval surface that flakes can actually invoke.
//!
//! ## M3.1 scope
//!
//! - Modules are attrsets shaped `{ options ?, config ?, imports ? }`.
//! - Option declarations are attrsets `{ type, default ?, description ? }`
//!   where `type` is a STRING naming the OptionTypeSpec
//!   (`"bool"`, `"int"`, `"str"`, `"path"`, `"listOf"`, ...).
//!   Real cppnix uses `lib.types.<X>` typed objects — that shim
//!   lands in M3.2.
//! - Config paths are flat dotted-strings in the attrset keys
//!   (`"services.foo.enable" = true;`).  Recursive shorthand
//!   (`services.foo.enable = true;` without quotes) is the
//!   parser's job and works equivalently.
//! - `imports` is M3.2 — for M3.1 the caller pre-flattens.
//! - `mkIf` / `mkForce` / `mkDefault` wrappers are M3.2.  M3.1
//!   accepts only bare definitions at the normal priority (100).
//!
//! Once this bridge is in place, future M3.x ratchets extend it
//! to handle the real cppnix authoring shapes without breaking
//! the M3.1 contract — M3.1 modules continue to evaluate
//! unchanged.

use std::collections::HashMap;
use std::rc::Rc;

use super::*;
use sui_spec::module_system::{
    self, Definition, Module, NixValue, OptionDecl,
};

/// Register the bridge builtin under `builtins.sui.evalModules`.
///
/// The caller wires this from `sui_ext::register` (or directly
/// from `mod.rs`) so it lands at `builtins.sui.evalModules`.
pub(crate) fn register(sui_ext: &mut NixAttrs) {
    register_builtin(sui_ext, "evalModules", |args| {
        eval_modules_builtin(&args[0])
    });
}

/// The actual builtin implementation.  Takes one arg: a list of
/// module attrsets.
fn eval_modules_builtin(modules_arg: &Value) -> Result<Value, EvalError> {
    let forced = crate::eval::force_value(modules_arg)?;
    let list = match forced {
        Value::List(l) => l,
        other => {
            return Err(EvalError::type_error(format!(
                "builtins.sui.evalModules: expected a list of module attrsets, got {}",
                other.type_name(),
            )));
        }
    };

    let mut modules: Vec<Module> = Vec::with_capacity(list.len());
    for (i, m) in list.iter().enumerate() {
        modules.push(parse_module(m, i)?);
    }

    let registry = module_system::load_canonical()
        .map_err(|e| EvalError::type_error(format!(
            "builtins.sui.evalModules: registry load: {e:?}",
        )))?
        .types;

    let config = module_system::eval_modules(&modules, &registry)
        .map_err(|e| EvalError::type_error(format!(
            "builtins.sui.evalModules: {e:?}",
        )))?;

    Ok(config_to_value(config))
}

/// Convert one Nix attrset (the module shape) into a typed Module.
fn parse_module(value: &Value, idx: usize) -> Result<Module, EvalError> {
    let forced = crate::eval::force_value(value)?;
    let attrs = match forced {
        Value::Attrs(a) => a,
        other => {
            return Err(EvalError::type_error(format!(
                "builtins.sui.evalModules: module[{idx}] must be an attrset, got {}",
                other.type_name(),
            )));
        }
    };

    let mut module = Module::default();

    // Walk the `options` attrset → HashMap<String, OptionDecl>.
    if let Some(opts) = attrs.get("options") {
        let forced_opts = crate::eval::force_value(opts)?;
        let opts_attrs = match forced_opts {
            Value::Attrs(a) => a,
            other => {
                return Err(EvalError::type_error(format!(
                    "builtins.sui.evalModules: module[{idx}].options must be an attrset, got {}",
                    other.type_name(),
                )));
            }
        };
        for (path, decl_val) in opts_attrs.iter() {
            module
                .options
                .insert(path.to_string(), parse_option_decl(decl_val, &path)?);
        }
    }

    // Walk the `config` attrset → Vec<Definition>.
    if let Some(cfg) = attrs.get("config") {
        let forced_cfg = crate::eval::force_value(cfg)?;
        let cfg_attrs = match forced_cfg {
            Value::Attrs(a) => a,
            other => {
                return Err(EvalError::type_error(format!(
                    "builtins.sui.evalModules: module[{idx}].config must be an attrset, got {}",
                    other.type_name(),
                )));
            }
        };
        for (path, value) in cfg_attrs.iter() {
            module.config.push(Definition {
                path: path.to_string(),
                value: crate::eval::force_value(value)?.to_json(),
                priority: 100, // M3.1: only normal-priority defs (mkIf/mkForce land in M3.2)
                cond: None,
            });
        }
    }

    // `imports` — M3.1 accepts the field but ignores it (caller pre-flattens).
    // Validation: it must be a list of strings if present.
    if let Some(imports) = attrs.get("imports") {
        let forced = crate::eval::force_value(imports)?;
        match forced {
            Value::List(l) => {
                for item in l.iter() {
                    let forced_item = crate::eval::force_value(item)?;
                    if !matches!(forced_item, Value::String(_)) {
                        return Err(EvalError::type_error(format!(
                            "builtins.sui.evalModules: module[{idx}].imports \
                             items must be strings (M3.1); attrset/function imports land in M3.2",
                        )));
                    }
                }
            }
            _ => {
                return Err(EvalError::type_error(format!(
                    "builtins.sui.evalModules: module[{idx}].imports must be a list",
                )));
            }
        }
    }

    Ok(module)
}

/// Parse one option-declaration attrset.
fn parse_option_decl(decl_val: &Value, path: &str) -> Result<OptionDecl, EvalError> {
    let forced = crate::eval::force_value(decl_val)?;
    let attrs = match forced {
        Value::Attrs(a) => a,
        other => {
            return Err(EvalError::type_error(format!(
                "builtins.sui.evalModules: option `{path}` declaration must be an \
                 attrset, got {}",
                other.type_name(),
            )));
        }
    };

    let type_name = match attrs.get("type") {
        Some(v) => match crate::eval::force_value(v)? {
            Value::String(s) => s.chars.to_string(),
            other => {
                return Err(EvalError::type_error(format!(
                    "builtins.sui.evalModules: option `{path}`.type must be a \
                     string (e.g. \"bool\", \"int\", \"str\"); got {}",
                    other.type_name(),
                )));
            }
        },
        None => {
            return Err(EvalError::type_error(format!(
                "builtins.sui.evalModules: option `{path}` missing required `type` field",
            )));
        }
    };

    let default = match attrs.get("default") {
        Some(v) => Some(crate::eval::force_value(v)?.to_json()),
        None => None,
    };

    let description = match attrs.get("description") {
        Some(v) => match crate::eval::force_value(v)? {
            Value::String(s) => s.chars.to_string(),
            _ => String::new(),
        },
        None => String::new(),
    };

    Ok(OptionDecl {
        type_name,
        default,
        description,
        submodule: None,
    })
}

/// Convert the typed Config back into a `Value::Attrs`.
fn config_to_value(config: HashMap<String, NixValue>) -> Value {
    let mut attrs = NixAttrs::new();
    for (k, v) in config {
        attrs.insert(k, json_to_value(&v));
    }
    Value::Attrs(Rc::new(attrs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny module-list Value (the input shape for the
    /// builtin) directly via Value constructors.
    fn module_list(modules: Vec<Value>) -> Value {
        Value::List(Rc::new(modules))
    }

    fn attrs_of(pairs: &[(&str, Value)]) -> Value {
        let mut a = NixAttrs::new();
        for (k, v) in pairs {
            a.insert(k.to_string(), v.clone());
        }
        Value::Attrs(Rc::new(a))
    }

    #[test]
    fn trivial_bool_evaluates_through_the_bridge() {
        let opt_decl = attrs_of(&[("type", Value::string("bool"))]);
        let options = attrs_of(&[("enable", opt_decl)]);
        let config = attrs_of(&[("enable", Value::Bool(true))]);
        let module = attrs_of(&[("options", options), ("config", config)]);
        let result = eval_modules_builtin(&module_list(vec![module])).unwrap();
        let attrs = match result {
            Value::Attrs(a) => a,
            _ => panic!("expected attrs result"),
        };
        match attrs.get("enable") {
            Some(Value::Bool(b)) => assert!(*b),
            other => panic!("expected enable=true, got {other:?}"),
        }
    }

    #[test]
    fn default_surfaces_when_undefined() {
        let opt_decl = attrs_of(&[
            ("type", Value::string("int")),
            ("default", Value::Int(80)),
        ]);
        let options = attrs_of(&[("port", opt_decl)]);
        // No config — default should kick in.
        let module = attrs_of(&[("options", options)]);
        let result = eval_modules_builtin(&module_list(vec![module])).unwrap();
        let attrs = match result {
            Value::Attrs(a) => a,
            _ => panic!("expected attrs"),
        };
        match attrs.get("port") {
            Some(Value::Int(n)) => assert_eq!(*n, 80),
            other => panic!("expected port=80, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_list_arg() {
        let bogus = Value::Bool(true);
        let err = eval_modules_builtin(&bogus).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("list of module attrsets"));
    }

    #[test]
    fn rejects_module_with_non_attrset() {
        let result = eval_modules_builtin(&module_list(vec![Value::Int(42)]));
        assert!(result.is_err());
    }

    #[test]
    fn rejects_option_with_missing_type() {
        let opt_decl = attrs_of(&[]);  // no `type` field
        let options = attrs_of(&[("foo", opt_decl)]);
        let module = attrs_of(&[("options", options)]);
        let err = eval_modules_builtin(&module_list(vec![module])).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("missing required `type`"));
    }

    #[test]
    fn rejects_option_with_typed_object_type_field() {
        // M3.1 requires bare-string `type`; cppnix typed-object
        // surface lands in M3.2.
        let typed_obj = attrs_of(&[("__type", Value::string("bool"))]);
        let opt_decl = attrs_of(&[("type", typed_obj)]);
        let options = attrs_of(&[("foo", opt_decl)]);
        let module = attrs_of(&[("options", options)]);
        let err = eval_modules_builtin(&module_list(vec![module])).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("must be a"));
        assert!(msg.contains("string"));
    }

    #[test]
    fn type_mismatch_surfaces_through_bridge() {
        // bool option, but config gives an int — the underlying
        // eval_modules type-check should surface.
        let opt_decl = attrs_of(&[("type", Value::string("bool"))]);
        let options = attrs_of(&[("enable", opt_decl)]);
        let config = attrs_of(&[("enable", Value::Int(42))]);
        let module = attrs_of(&[("options", options), ("config", config)]);
        let err = eval_modules_builtin(&module_list(vec![module])).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("type-check") || msg.contains("bool"));
    }

    #[test]
    fn list_of_concatenates_across_modules() {
        let opt_decl = attrs_of(&[("type", Value::string("listOf"))]);
        let options = attrs_of(&[("xs", opt_decl)]);

        let cfg1 = attrs_of(&[("xs", Value::list(vec![Value::Int(1), Value::Int(2)]))]);
        let mod1 = attrs_of(&[("options", options), ("config", cfg1)]);

        let cfg2 = attrs_of(&[("xs", Value::list(vec![Value::Int(3), Value::Int(4)]))]);
        let mod2 = attrs_of(&[("config", cfg2)]);

        let result = eval_modules_builtin(&module_list(vec![mod1, mod2])).unwrap();
        let attrs = match result {
            Value::Attrs(a) => a,
            _ => panic!("expected attrs"),
        };
        let list_val = attrs.get("xs").expect("xs must resolve");
        let items = match list_val {
            Value::List(l) => l,
            _ => panic!("expected list, got {list_val:?}"),
        };
        assert_eq!(items.len(), 4);
    }
}
