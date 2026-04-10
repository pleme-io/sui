//! Attrset builtins: attrNames, attrValues, hasAttr, getAttr, intersectAttrs,
//! mapAttrs, listToAttrs, catAttrs, removeAttrs, filterAttrs, zipAttrsWith.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    // attrNames: iterates BTreeMap keys (sorted). String clone per key
    // (typically small interned identifiers).
    register_builtin(builtins, "attrNames", |args| {
        let attrs = args[0].to_attrs()?;
        Ok(Value::List(Rc::new(attrs.keys().map(|k| Value::string(k.clone())).collect())))
    });
    // attrValues: iterates BTreeMap values. Each `.cloned()` is an Rc
    // bump for heap-backed Value variants (no deep copy).
    register_builtin(builtins, "attrValues", |args| {
        let attrs = args[0].to_attrs()?;
        Ok(Value::List(Rc::new(attrs.values().cloned().collect())))
    });
    register_builtin(builtins, "hasAttr", |args| {
        let name = args[0].as_string()?.to_string();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "hasAttr<partial>",
            func: Rc::new(move |args2| {
                let attrs = args2[0].to_attrs()?;
                Ok(Value::Bool(attrs.contains_key(&name)))
            }),
        })))
    });
    register_builtin(builtins, "getAttr", |args| {
        let name = args[0].as_string()?.to_string();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "getAttr<partial>",
            func: Rc::new(move |args2| {
                let attrs = args2[0].to_attrs()?;
                attrs.get(&name).cloned().ok_or_else(|| EvalError::AttrNotFound(name.clone()))
            }),
        })))
    });
    register_builtin(builtins, "intersectAttrs", |args| {
        let a = args[0].to_attrs()?.clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "intersectAttrs<partial>",
            func: Rc::new(move |args2| {
                let b = args2[0].to_attrs()?;
                let mut result = NixAttrs::new();
                for (k, v) in b.iter() {
                    if a.contains_key(&k) {
                        result.insert(k.clone(), v.clone());
                    }
                }
                Ok(Value::Attrs(Rc::new(result)))
            }),
        })))
    });

    // filterAttrs
    register_builtin(builtins, "filterAttrs", |args| {
        let pred = args[0].clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "filterAttrs<partial>",
            func: Rc::new(move |args2| {
                let attrs = args2[0].to_attrs()?;
                let mut result = NixAttrs::new();
                for (k, v) in attrs.iter() {
                    let partial = crate::eval::apply(pred.clone(), Value::string(k.clone()))?;
                    if crate::eval::apply_and_force(partial, v.clone())?.as_bool()? {
                        result.insert(k.clone(), v.clone());
                    }
                }
                Ok(Value::Attrs(Rc::new(result)))
            }),
        })))
    });

    // Attrset higher-order operations
    register_builtin(builtins, "mapAttrs", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "mapAttrs<partial>",
            func: Rc::new(move |args2| {
                let attrs = args2[0].to_attrs()?;
                let mut result = NixAttrs::new();
                for (k, v) in attrs.iter() {
                    let f = func.clone();
                    let key = k.clone();
                    let val = v.clone();
                    let thunk = Thunk::new_native(move || {
                        let partial = crate::eval::apply(f, Value::string(key))?;
                        crate::eval::apply(partial, val)
                    });
                    result.insert(k.clone(), Value::Thunk(thunk));
                }
                Ok(Value::Attrs(Rc::new(result)))
            }),
        })))
    });
    register_builtin(builtins, "listToAttrs", |args| {
        let list = args[0].as_list()?;
        let mut attrs = NixAttrs::new();
        for item in list {
            let item_attrs = item.to_attrs()?;
            let name = item_attrs.get("name")
                .ok_or_else(|| EvalError::AttrNotFound("name".to_string()))?
                .to_str()?;
            let value = item_attrs.get("value")
                .ok_or_else(|| EvalError::AttrNotFound("value".to_string()))?
                .clone();
            attrs.insert(name, value);
        }
        Ok(Value::Attrs(Rc::new(attrs)))
    });
    register_builtin(builtins, "catAttrs", |args| {
        let name = args[0].as_string()?.to_string();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "catAttrs<partial>",
            func: Rc::new(move |args2| {
                let list = args2[0].as_list()?;
                let mut result = Vec::new();
                for item in list {
                    if let Ok(attrs) = item.to_attrs()
                        && let Some(v) = attrs.get(&name) {
                            result.push(v.clone());
                        }
                }
                Ok(Value::List(Rc::new(result)))
            }),
        })))
    });
    register_builtin(builtins, "removeAttrs", |args| {
        let set = args[0].to_attrs()?.clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "removeAttrs<partial>",
            func: Rc::new(move |args2| {
                let names = args2[0].as_list()?;
                let remove: Vec<String> = names.iter()
                    .filter_map(|v| v.as_string().ok().map(|s| s.to_string()))
                    .collect();
                let mut result = set.clone();
                for name in &remove {
                    result.remove(name);
                }
                Ok(Value::Attrs(Rc::new(result)))
            }),
        })))
    });

    // zipAttrsWith — zip attrsets with a combining function
    register_builtin(builtins, "zipAttrsWith", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "zipAttrsWith<partial>",
            func: Rc::new(move |args2| {
                let list = args2[0].as_list()?;
                // Collect all keys and their values across all attrsets
                let mut collected: std::collections::BTreeMap<String, Vec<Value>> =
                    std::collections::BTreeMap::new();
                for item in list {
                    let attrs = item.to_attrs()?;
                    for (k, v) in attrs.iter() {
                        collected.entry(k.clone()).or_default().push(v.clone());
                    }
                }
                let mut result = NixAttrs::new();
                for (k, vs) in collected {
                    let partial = crate::eval::apply(
                        func.clone(),
                        Value::string(k.clone()),
                    )?;
                    let val = crate::eval::apply(partial, Value::List(Rc::new(vs)))?;
                    result.insert(k, val);
                }
                Ok(Value::Attrs(Rc::new(result)))
            }),
        })))
    });
}
