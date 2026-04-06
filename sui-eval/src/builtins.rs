//! Core Nix builtins.

use std::sync::Arc;

use crate::value::*;

/// Register all builtins into the environment.
pub fn register(env: &mut Env) {
    let mut builtins_set = NixAttrs::new();

    // Type checking
    register_builtin(&mut builtins_set, "typeOf", |args| {
        Ok(Value::String(args[0].type_name().to_string()))
    });
    register_builtin(&mut builtins_set, "isNull", |args| {
        Ok(Value::Bool(matches!(args[0], Value::Null)))
    });
    register_builtin(&mut builtins_set, "isInt", |args| {
        Ok(Value::Bool(matches!(args[0], Value::Int(_))))
    });
    register_builtin(&mut builtins_set, "isFloat", |args| {
        Ok(Value::Bool(matches!(args[0], Value::Float(_))))
    });
    register_builtin(&mut builtins_set, "isBool", |args| {
        Ok(Value::Bool(matches!(args[0], Value::Bool(_))))
    });
    register_builtin(&mut builtins_set, "isString", |args| {
        Ok(Value::Bool(matches!(args[0], Value::String(_))))
    });
    register_builtin(&mut builtins_set, "isList", |args| {
        Ok(Value::Bool(matches!(args[0], Value::List(_))))
    });
    register_builtin(&mut builtins_set, "isAttrs", |args| {
        Ok(Value::Bool(matches!(args[0], Value::Attrs(_))))
    });
    register_builtin(&mut builtins_set, "isFunction", |args| {
        Ok(Value::Bool(matches!(args[0], Value::Lambda(_) | Value::Builtin(_))))
    });
    register_builtin(&mut builtins_set, "isPath", |args| {
        Ok(Value::Bool(matches!(args[0], Value::Path(_))))
    });

    // Arithmetic
    register_curried(&mut builtins_set, "add", |a, b| {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x + y)),
            (Value::Float(x), Value::Float(y)) => Ok(Value::Float(x + y)),
            (Value::Int(x), Value::Float(y)) => Ok(Value::Float(*x as f64 + y)),
            (Value::Float(x), Value::Int(y)) => Ok(Value::Float(x + *y as f64)),
            _ => Err(EvalError::TypeError("add: expected numbers".to_string())),
        }
    });
    register_curried(&mut builtins_set, "sub", |a, b| {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x - y)),
            _ => Err(EvalError::TypeError("sub: expected ints".to_string())),
        }
    });
    register_curried(&mut builtins_set, "mul", |a, b| {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x * y)),
            _ => Err(EvalError::TypeError("mul: expected ints".to_string())),
        }
    });
    register_curried(&mut builtins_set, "div", |a, b| {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => {
                if *y == 0 { return Err(EvalError::DivisionByZero); }
                Ok(Value::Int(x / y))
            }
            _ => Err(EvalError::TypeError("div: expected ints".to_string())),
        }
    });

    // List operations
    register_builtin(&mut builtins_set, "length", |args| {
        Ok(Value::Int(args[0].as_list()?.len() as i64))
    });
    register_builtin(&mut builtins_set, "head", |args| {
        let list = args[0].as_list()?;
        list.first()
            .cloned()
            .ok_or_else(|| EvalError::TypeError("head: empty list".to_string()))
    });
    register_builtin(&mut builtins_set, "tail", |args| {
        let list = args[0].as_list()?;
        if list.is_empty() {
            return Err(EvalError::TypeError("tail: empty list".to_string()));
        }
        Ok(Value::List(list[1..].to_vec()))
    });
    register_builtin(&mut builtins_set, "elemAt", |args| {
        // Curried: builtins.elemAt list index
        let list = args[0].as_list()?.to_vec();
        Ok(Value::Builtin(BuiltinFn {
            name: "elemAt<partial>",
            func: Arc::new(move |args2| {
                let idx = args2[0].as_int()? as usize;
                list.get(idx)
                    .cloned()
                    .ok_or_else(|| EvalError::TypeError(format!("elemAt: index {idx} out of bounds")))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "elem", |args| {
        let needle = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "elem<partial>",
            func: Arc::new(move |args2| {
                let haystack = args2[0].as_list()?;
                Ok(Value::Bool(haystack.iter().any(|v| *v == needle)))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "genList", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "genList<partial>",
            func: Arc::new(move |args2| {
                let n = args2[0].as_int()?;
                let mut result = Vec::new();
                for i in 0..n {
                    result.push(crate::eval::apply(func.clone(), Value::Int(i))?);
                }
                Ok(Value::List(result))
            }),
        }))
    });

    // Attrset operations
    register_builtin(&mut builtins_set, "attrNames", |args| {
        let attrs = args[0].as_attrs()?;
        Ok(Value::List(attrs.keys().map(|k| Value::String(k.clone())).collect()))
    });
    register_builtin(&mut builtins_set, "attrValues", |args| {
        let attrs = args[0].as_attrs()?;
        Ok(Value::List(attrs.iter().map(|(_, v)| v.clone()).collect()))
    });
    register_builtin(&mut builtins_set, "hasAttr", |args| {
        let name = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "hasAttr<partial>",
            func: Arc::new(move |args2| {
                let attrs = args2[0].as_attrs()?;
                Ok(Value::Bool(attrs.contains_key(&name)))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "getAttr", |args| {
        let name = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "getAttr<partial>",
            func: Arc::new(move |args2| {
                let attrs = args2[0].as_attrs()?;
                attrs.get(&name).cloned().ok_or_else(|| EvalError::AttrNotFound(name.clone()))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "intersectAttrs", |args| {
        let a = args[0].as_attrs()?.clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "intersectAttrs<partial>",
            func: Arc::new(move |args2| {
                let b = args2[0].as_attrs()?;
                let mut result = NixAttrs::new();
                for (k, v) in b.iter() {
                    if a.contains_key(k) {
                        result.insert(k.clone(), v.clone());
                    }
                }
                Ok(Value::Attrs(result))
            }),
        }))
    });

    // String operations
    register_builtin(&mut builtins_set, "toString", |args| {
        Ok(Value::String(match &args[0] {
            Value::String(s) => s.clone(),
            Value::Int(n) => n.to_string(),
            Value::Float(f) => format!("{f}"),
            Value::Bool(true) => "1".to_string(),
            Value::Bool(false) => String::new(),
            Value::Null => String::new(),
            Value::Path(p) => p.clone(),
            Value::List(_) => return Err(EvalError::TypeError("toString: cannot convert list".to_string())),
            Value::Attrs(attrs) => {
                // __toString protocol: call __toString with self
                if let Some(to_str) = attrs.get("__toString") {
                    let result = crate::eval::apply(to_str.clone(), args[0].clone())?;
                    match result {
                        Value::String(s) => return Ok(Value::String(s)),
                        _ => return Err(EvalError::TypeError("__toString must return a string".to_string())),
                    }
                }
                return Err(EvalError::TypeError("toString: cannot convert set".to_string()));
            }
            Value::Lambda(_) | Value::Builtin(_) => return Err(EvalError::TypeError("toString: cannot convert function".to_string())),
        }))
    });
    register_builtin(&mut builtins_set, "stringLength", |args| {
        Ok(Value::Int(args[0].as_string()?.len() as i64))
    });
    register_builtin(&mut builtins_set, "substring", |args| {
        let start = args[0].as_int()? as usize;
        Ok(Value::Builtin(BuiltinFn {
            name: "substring<p1>",
            func: Arc::new(move |args2| {
                let len = args2[0].as_int()? as usize;
                Ok(Value::Builtin(BuiltinFn {
                    name: "substring<p2>",
                    func: Arc::new(move |args3| {
                        let s = args3[0].as_string()?;
                        let end = (start + len).min(s.len());
                        let start = start.min(s.len());
                        Ok(Value::String(s[start..end].to_string()))
                    }),
                }))
            }),
        }))
    });

    // Conversion
    register_builtin(&mut builtins_set, "toJSON", |args| {
        Ok(Value::String(serde_json::to_string(&args[0].to_json())
            .unwrap_or_else(|_| "null".to_string())))
    });
    register_builtin(&mut builtins_set, "fromJSON", |args| {
        let s = args[0].as_string()?;
        let json: serde_json::Value = serde_json::from_str(s)
            .map_err(|e| EvalError::TypeError(format!("fromJSON: {e}")))?;
        Ok(json_to_value(&json))
    });

    // Higher-order list operations (critical for nixpkgs)
    register_builtin(&mut builtins_set, "map", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "map<partial>",
            func: Arc::new(move |args2| {
                let list = args2[0].as_list()?;
                let result: Result<Vec<_>, _> = list.iter()
                    .map(|v| crate::eval::apply(func.clone(), v.clone()))
                    .collect();
                Ok(Value::List(result?))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "filter", |args| {
        let pred = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "filter<partial>",
            func: Arc::new(move |args2| {
                let list = args2[0].as_list()?;
                let mut result = Vec::new();
                for v in list {
                    if crate::eval::apply(pred.clone(), v.clone())?.as_bool()? {
                        result.push(v.clone());
                    }
                }
                Ok(Value::List(result))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "foldl'", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "foldl'<p1>",
            func: Arc::new(move |args2| {
                let init = args2[0].clone();
                let func2 = func.clone();
                Ok(Value::Builtin(BuiltinFn {
                    name: "foldl'<p2>",
                    func: Arc::new(move |args3| {
                        let list = args3[0].as_list()?;
                        let mut acc = init.clone();
                        for v in list {
                            let partial = crate::eval::apply(func2.clone(), acc)?;
                            acc = crate::eval::apply(partial, v.clone())?;
                        }
                        Ok(acc)
                    }),
                }))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "concatMap", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "concatMap<partial>",
            func: Arc::new(move |args2| {
                let list = args2[0].as_list()?;
                let mut result = Vec::new();
                for v in list {
                    let mapped = crate::eval::apply(func.clone(), v.clone())?;
                    result.extend_from_slice(mapped.as_list()?);
                }
                Ok(Value::List(result))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "concatLists", |args| {
        let lists = args[0].as_list()?;
        let mut result = Vec::new();
        for l in lists {
            result.extend_from_slice(l.as_list()?);
        }
        Ok(Value::List(result))
    });
    register_builtin(&mut builtins_set, "sort", |args| {
        let cmp = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "sort<partial>",
            func: Arc::new(move |args2| {
                let mut list = args2[0].as_list()?.to_vec();
                // Simple insertion sort using the comparison function
                for i in 1..list.len() {
                    let mut j = i;
                    while j > 0 {
                        let less = crate::eval::apply(
                            crate::eval::apply(cmp.clone(), list[j].clone())?,
                            list[j - 1].clone(),
                        )?.as_bool()?;
                        if less {
                            list.swap(j, j - 1);
                            j -= 1;
                        } else {
                            break;
                        }
                    }
                }
                Ok(Value::List(list))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "all", |args| {
        let pred = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "all<partial>",
            func: Arc::new(move |args2| {
                let list = args2[0].as_list()?;
                for v in list {
                    if !crate::eval::apply(pred.clone(), v.clone())?.as_bool()? {
                        return Ok(Value::Bool(false));
                    }
                }
                Ok(Value::Bool(true))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "any", |args| {
        let pred = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "any<partial>",
            func: Arc::new(move |args2| {
                let list = args2[0].as_list()?;
                for v in list {
                    if crate::eval::apply(pred.clone(), v.clone())?.as_bool()? {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }),
        }))
    });

    // Attrset higher-order operations
    register_builtin(&mut builtins_set, "mapAttrs", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "mapAttrs<partial>",
            func: Arc::new(move |args2| {
                let attrs = args2[0].as_attrs()?;
                let mut result = NixAttrs::new();
                for (k, v) in attrs.iter() {
                    let partial = crate::eval::apply(func.clone(), Value::String(k.clone()))?;
                    let mapped = crate::eval::apply(partial, v.clone())?;
                    result.insert(k.clone(), mapped);
                }
                Ok(Value::Attrs(result))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "listToAttrs", |args| {
        let list = args[0].as_list()?;
        let mut attrs = NixAttrs::new();
        for item in list {
            let item_attrs = item.as_attrs()?;
            let name = item_attrs.get("name")
                .ok_or_else(|| EvalError::AttrNotFound("name".to_string()))?
                .as_string()?.to_string();
            let value = item_attrs.get("value")
                .ok_or_else(|| EvalError::AttrNotFound("value".to_string()))?
                .clone();
            attrs.insert(name, value);
        }
        Ok(Value::Attrs(attrs))
    });
    register_builtin(&mut builtins_set, "catAttrs", |args| {
        let name = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "catAttrs<partial>",
            func: Arc::new(move |args2| {
                let list = args2[0].as_list()?;
                let mut result = Vec::new();
                for item in list {
                    if let Ok(attrs) = item.as_attrs() {
                        if let Some(v) = attrs.get(&name) {
                            result.push(v.clone());
                        }
                    }
                }
                Ok(Value::List(result))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "removeAttrs", |args| {
        let set = args[0].as_attrs()?.clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "removeAttrs<partial>",
            func: Arc::new(move |args2| {
                let names = args2[0].as_list()?;
                let remove: Vec<String> = names.iter()
                    .filter_map(|v| v.as_string().ok().map(|s| s.to_string()))
                    .collect();
                let mut result = set.clone();
                for name in &remove {
                    result.0.remove(name);
                }
                Ok(Value::Attrs(result))
            }),
        }))
    });

    // String operations (additional)
    register_builtin(&mut builtins_set, "replaceStrings", |args| {
        let from = args[0].as_list()?.iter()
            .map(|v| v.as_string().map(|s| s.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Value::Builtin(BuiltinFn {
            name: "replaceStrings<p1>",
            func: Arc::new(move |args2| {
                let to = args2[0].as_list()?.iter()
                    .map(|v| v.as_string().map(|s| s.to_string()))
                    .collect::<Result<Vec<_>, _>>()?;
                let from2 = from.clone();
                Ok(Value::Builtin(BuiltinFn {
                    name: "replaceStrings<p2>",
                    func: Arc::new(move |args3| {
                        let mut s = args3[0].as_string()?.to_string();
                        for (f, t) in from2.iter().zip(to.iter()) {
                            if !f.is_empty() {
                                s = s.replace(f.as_str(), t);
                            }
                        }
                        Ok(Value::String(s))
                    }),
                }))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "concatStringsSep", |args| {
        let sep = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "concatStringsSep<partial>",
            func: Arc::new(move |args2| {
                let list = args2[0].as_list()?;
                let strings: Result<Vec<_>, _> = list.iter()
                    .map(|v| v.as_string().map(|s| s.to_string()))
                    .collect();
                Ok(Value::String(strings?.join(&sep)))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "hasPrefix", |args| {
        let prefix = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "hasPrefix<partial>",
            func: Arc::new(move |args2| {
                let s = args2[0].as_string()?;
                Ok(Value::Bool(s.starts_with(&prefix)))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "hasSuffix", |args| {
        let suffix = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "hasSuffix<partial>",
            func: Arc::new(move |args2| {
                let s = args2[0].as_string()?;
                Ok(Value::Bool(s.ends_with(&suffix)))
            }),
        }))
    });

    // concatStrings — concat without separator
    register_builtin(&mut builtins_set, "concatStrings", |args| {
        let list = args[0].as_list()?;
        let mut result = String::new();
        for v in list {
            result.push_str(v.as_string()?);
        }
        Ok(Value::String(result))
    });

    // partition — split list by predicate into { right, wrong }
    register_builtin(&mut builtins_set, "partition", |args| {
        let pred = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "partition<partial>",
            func: Arc::new(move |args2| {
                let list = args2[0].as_list()?;
                let mut right = Vec::new();
                let mut wrong = Vec::new();
                for v in list {
                    if crate::eval::apply(pred.clone(), v.clone())?.as_bool()? {
                        right.push(v.clone());
                    } else {
                        wrong.push(v.clone());
                    }
                }
                let mut result = NixAttrs::new();
                result.insert("right".to_string(), Value::List(right));
                result.insert("wrong".to_string(), Value::List(wrong));
                Ok(Value::Attrs(result))
            }),
        }))
    });

    // groupBy — group list elements by key function
    register_builtin(&mut builtins_set, "groupBy", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "groupBy<partial>",
            func: Arc::new(move |args2| {
                let list = args2[0].as_list()?;
                let mut groups: std::collections::BTreeMap<String, Vec<Value>> =
                    std::collections::BTreeMap::new();
                for v in list {
                    let key = crate::eval::apply(func.clone(), v.clone())?;
                    let key_str = key.as_string()?.to_string();
                    groups.entry(key_str).or_default().push(v.clone());
                }
                let mut result = NixAttrs::new();
                for (k, vs) in groups {
                    result.insert(k, Value::List(vs));
                }
                Ok(Value::Attrs(result))
            }),
        }))
    });

    // zipAttrsWith — zip attrsets with a combining function
    register_builtin(&mut builtins_set, "zipAttrsWith", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "zipAttrsWith<partial>",
            func: Arc::new(move |args2| {
                let list = args2[0].as_list()?;
                // Collect all keys and their values across all attrsets
                let mut collected: std::collections::BTreeMap<String, Vec<Value>> =
                    std::collections::BTreeMap::new();
                for item in list {
                    let attrs = item.as_attrs()?;
                    for (k, v) in attrs.iter() {
                        collected.entry(k.clone()).or_default().push(v.clone());
                    }
                }
                let mut result = NixAttrs::new();
                for (k, vs) in collected {
                    let partial = crate::eval::apply(
                        func.clone(),
                        Value::String(k.clone()),
                    )?;
                    let val = crate::eval::apply(partial, Value::List(vs))?;
                    result.insert(k, val);
                }
                Ok(Value::Attrs(result))
            }),
        }))
    });

    // compareVersions — compare version strings
    register_builtin(&mut builtins_set, "compareVersions", |args| {
        let a = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "compareVersions<partial>",
            func: Arc::new(move |args2| {
                let b = args2[0].as_string()?;
                let result = compare_versions(&a, b);
                Ok(Value::Int(result))
            }),
        }))
    });

    // parseDrvName — parse "name-version" from package name
    register_builtin(&mut builtins_set, "parseDrvName", |args| {
        let s = args[0].as_string()?;
        let (name, version) = parse_drv_name(s);
        let mut result = NixAttrs::new();
        result.insert("name".to_string(), Value::String(name));
        result.insert("version".to_string(), Value::String(version));
        Ok(Value::Attrs(result))
    });

    // baseNameOf — extract filename from path
    register_builtin(&mut builtins_set, "baseNameOf", |args| {
        let s = match &args[0] {
            Value::String(s) => s.clone(),
            Value::Path(p) => p.clone(),
            _ => return Err(EvalError::TypeError("baseNameOf: expected string or path".to_string())),
        };
        let base = s.rsplit('/').next().unwrap_or(&s);
        Ok(Value::String(base.to_string()))
    });

    // dirOf — extract directory from path
    register_builtin(&mut builtins_set, "dirOf", |args| {
        let (s, is_path) = match &args[0] {
            Value::String(s) => (s.clone(), false),
            Value::Path(p) => (p.clone(), true),
            _ => return Err(EvalError::TypeError("dirOf: expected string or path".to_string())),
        };
        let dir = match s.rfind('/') {
            Some(0) => "/".to_string(),
            Some(i) => s[..i].to_string(),
            None => ".".to_string(),
        };
        if is_path {
            Ok(Value::Path(dir))
        } else {
            Ok(Value::String(dir))
        }
    });

    // readFile — read file contents to string
    register_builtin(&mut builtins_set, "readFile", |args| {
        let path = match &args[0] {
            Value::Path(p) => p.clone(),
            Value::String(s) => s.clone(),
            _ => return Err(EvalError::TypeError("readFile: expected path or string".to_string())),
        };
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| EvalError::TypeError(format!("readFile: {e}")))?;
        Ok(Value::String(contents))
    });

    // addErrorContext — wraps an expression with error context (passthrough in our impl)
    register_builtin(&mut builtins_set, "addErrorContext", |args| {
        let _ctx = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "addErrorContext<partial>",
            func: Arc::new(|args2| Ok(args2[0].clone())),
        }))
    });

    // Numeric
    register_builtin(&mut builtins_set, "ceil", |args| {
        Ok(Value::Int(args[0].as_float()?.ceil() as i64))
    });
    register_builtin(&mut builtins_set, "floor", |args| {
        Ok(Value::Int(args[0].as_float()?.floor() as i64))
    });

    // Misc
    register_builtin(&mut builtins_set, "tryEval", |args| {
        // In our implementation, args are already evaluated, so tryEval
        // just wraps the value. Real lazy eval would catch thunk errors.
        let mut result = NixAttrs::new();
        result.insert("success".to_string(), Value::Bool(true));
        result.insert("value".to_string(), args[0].clone());
        Ok(Value::Attrs(result))
    });
    register_builtin(&mut builtins_set, "trace", |args| {
        let msg = args[0].clone();
        tracing::debug!("trace: {msg}");
        Ok(Value::Builtin(BuiltinFn {
            name: "trace<partial>",
            func: Arc::new(|args2| Ok(args2[0].clone())),
        }))
    });
    register_builtin(&mut builtins_set, "functionArgs", |args| {
        match &args[0] {
            Value::Lambda(closure) => {
                let mut result = NixAttrs::new();
                match &closure.param {
                    rnix::ast::Param::Pattern(pat) => {
                        for entry in pat.pat_entries() {
                            let name = entry.ident().unwrap().to_string();
                            let has_default = entry.default().is_some();
                            result.insert(name, Value::Bool(has_default));
                        }
                    }
                    _ => {}
                }
                Ok(Value::Attrs(result))
            }
            Value::Builtin(_) => Ok(Value::Attrs(NixAttrs::new())),
            _ => Err(EvalError::TypeError("functionArgs: expected function".to_string())),
        }
    });

    // Misc
    register_builtin(&mut builtins_set, "throw", |args| {
        let msg = args[0].as_string()?;
        Err(EvalError::TypeError(format!("throw: {msg}")))
    });
    register_builtin(&mut builtins_set, "abort", |args| {
        let msg = args[0].as_string()?;
        Err(EvalError::TypeError(format!("abort: {msg}")))
    });
    register_builtin(&mut builtins_set, "seq", |args| {
        let _forced = args[0].clone(); // force first arg
        Ok(Value::Builtin(BuiltinFn {
            name: "seq<partial>",
            func: Arc::new(|args2| Ok(args2[0].clone())),
        }))
    });
    register_builtin(&mut builtins_set, "deepSeq", |args| {
        let _forced = args[0].clone(); // force first arg (deep in real impl)
        Ok(Value::Builtin(BuiltinFn {
            name: "deepSeq<partial>",
            func: Arc::new(|args2| Ok(args2[0].clone())),
        }))
    });

    // ── Tier 1: hashString, match, split (regex-based) ────

    register_curried(&mut builtins_set, "hashString", |algo, s| {
        let algo_str = algo.as_string()?;
        let input = s.as_string()?;
        let hex = match algo_str {
            "sha256" => {
                use sha2::{Sha256, Digest};
                format!("{:x}", Sha256::digest(input.as_bytes()))
            }
            "sha512" => {
                use sha2::{Sha512, Digest};
                format!("{:x}", Sha512::digest(input.as_bytes()))
            }
            _ => return Err(EvalError::TypeError(format!("hashString: unsupported algorithm: {algo_str}"))),
        };
        Ok(Value::String(hex))
    });

    register_curried(&mut builtins_set, "match", |pattern, s| {
        let pat = pattern.as_string()?;
        let input = s.as_string()?;
        let re = regex::Regex::new(&format!("^{pat}$"))
            .map_err(|e| EvalError::TypeError(format!("match: invalid regex: {e}")))?;
        match re.captures(input) {
            Some(caps) => {
                let groups: Vec<Value> = (1..caps.len())
                    .map(|i| match caps.get(i) {
                        Some(m) => Value::String(m.as_str().to_string()),
                        None => Value::Null,
                    })
                    .collect();
                Ok(Value::List(groups))
            }
            None => Ok(Value::Null),
        }
    });

    // Regex-based split per Nix spec: alternates non-match strings and match group lists.
    register_curried(&mut builtins_set, "split", |pattern, s| {
        let pat = pattern.as_string()?;
        let input = s.as_string()?;
        let re = regex::Regex::new(pat)
            .map_err(|e| EvalError::TypeError(format!("split: invalid regex: {e}")))?;
        let mut result: Vec<Value> = Vec::new();
        let mut last_end = 0;
        for m in re.find_iter(input) {
            // Add the non-matching part before this match
            result.push(Value::String(input[last_end..m.start()].to_string()));
            // Add the match groups as a list
            if let Some(caps) = re.captures(&input[m.start()..]) {
                let groups: Vec<Value> = (1..caps.len())
                    .map(|i| match caps.get(i) {
                        Some(g) => Value::String(g.as_str().to_string()),
                        None => Value::Null,
                    })
                    .collect();
                // If no capture groups, wrap the whole match in a list
                if groups.is_empty() {
                    result.push(Value::List(vec![Value::String(m.as_str().to_string())]));
                } else {
                    result.push(Value::List(groups));
                }
            }
            last_end = m.end();
        }
        // Add trailing non-matching part
        result.push(Value::String(input[last_end..].to_string()));
        Ok(Value::List(result))
    });

    // ── Tier 2: readDir, toPath, storePath, placeholder ────

    register_builtin(&mut builtins_set, "readDir", |args| {
        let path_str = match &args[0] {
            Value::Path(p) => p.clone(),
            Value::String(s) => s.clone(),
            _ => return Err(EvalError::TypeError("readDir: expected path".into())),
        };
        let mut attrs = NixAttrs::new();
        for entry in std::fs::read_dir(&path_str)
            .map_err(|e| EvalError::TypeError(format!("readDir: {e}")))?
        {
            let entry = entry.map_err(|e| EvalError::TypeError(format!("readDir: {e}")))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let ft = entry.file_type().map_err(|e| EvalError::TypeError(format!("readDir: {e}")))?;
            let type_str = if ft.is_dir() {
                "directory"
            } else if ft.is_symlink() {
                "symlink"
            } else {
                "regular"
            };
            attrs.insert(name, Value::String(type_str.to_string()));
        }
        Ok(Value::Attrs(attrs))
    });

    register_builtin(&mut builtins_set, "toPath", |args| {
        let s = args[0].as_string()?;
        if !s.starts_with('/') {
            return Err(EvalError::TypeError(format!("toPath: path must be absolute: {s}")));
        }
        Ok(Value::Path(s.to_string()))
    });

    register_builtin(&mut builtins_set, "storePath", |args| {
        let s = args[0].as_string()?;
        if !s.starts_with("/nix/store/") {
            return Err(EvalError::TypeError(format!("storePath: not a store path: {s}")));
        }
        Ok(Value::Path(s.to_string()))
    });

    register_builtin(&mut builtins_set, "placeholder", |args| {
        let output = args[0].as_string()?;
        use sha2::{Sha256, Digest};
        let hash = Sha256::digest(format!("nix-output:{output}").as_bytes());
        let hash_str = format!("{:x}", hash);
        Ok(Value::String(format!("/placeholder-{}", &hash_str[..32])))
    });

    // ── import ─────────────────────────────────────────────

    register_builtin(&mut builtins_set, "import", |args| {
        let path = match &args[0] {
            Value::Path(p) => p.clone(),
            Value::String(s) => s.clone(),
            _ => return Err(EvalError::TypeError("import: expected path".into())),
        };
        let source = std::fs::read_to_string(&path)
            .map_err(|e| EvalError::TypeError(format!("import {path}: {e}")))?;
        crate::eval::eval(&source)
    });

    // ── derivation stub ────────────────────────────────────

    register_builtin(&mut builtins_set, "derivation", |args| {
        let input_attrs = args[0].as_attrs()?;
        let name = input_attrs
            .get("name")
            .ok_or(EvalError::AttrNotFound("name".into()))?
            .as_string()?
            .to_string();
        let _system = input_attrs
            .get("system")
            .ok_or(EvalError::AttrNotFound("system".into()))?
            .as_string()?;
        let _builder = input_attrs
            .get("builder")
            .ok_or(EvalError::AttrNotFound("builder".into()))?
            .as_string()?;

        let mut result = input_attrs.clone();
        result.insert("type".to_string(), Value::String("derivation".to_string()));
        result.insert(
            "drvPath".to_string(),
            Value::String(format!("/nix/store/stub-{name}.drv")),
        );
        result.insert(
            "outPath".to_string(),
            Value::String(format!("/nix/store/stub-{name}")),
        );
        Ok(Value::Attrs(result))
    });

    // ── fetchurl ───────────────────────────────────────────
    //
    // Accepts a string URL or an attrset { url, sha256? }.
    // Downloads the URL and writes it to a temp file, returning the path.

    register_builtin(&mut builtins_set, "fetchurl", |args| {
        let (url, expected_sha256) = match &args[0] {
            Value::String(s) => (s.clone(), None),
            Value::Attrs(a) => {
                let u = a
                    .get("url")
                    .ok_or_else(|| EvalError::AttrNotFound("url".into()))?
                    .as_string()?
                    .to_string();
                let sha = a
                    .get("sha256")
                    .map(|v| v.as_string().map(|s| s.to_string()))
                    .transpose()?;
                (u, sha)
            }
            _ => {
                return Err(EvalError::TypeError(
                    "fetchurl: expected string or attrset".into(),
                ))
            }
        };
        let bytes = fetch_url_bytes(&url)
            .map_err(|e| EvalError::TypeError(format!("fetchurl: {e}")))?;
        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(&bytes));
        if let Some(ref expected) = expected_sha256 {
            if *expected != hash {
                return Err(EvalError::TypeError(format!(
                    "fetchurl: sha256 mismatch: expected {expected}, got {hash}"
                )));
            }
        }
        let dir = std::env::temp_dir().join("sui-fetchurl");
        std::fs::create_dir_all(&dir)
            .map_err(|e| EvalError::TypeError(format!("fetchurl: {e}")))?;
        let path = dir.join(&hash);
        std::fs::write(&path, &bytes)
            .map_err(|e| EvalError::TypeError(format!("fetchurl: {e}")))?;
        Ok(Value::Path(path.to_string_lossy().to_string()))
    });

    // ── fetchTarball ──────────────────────────────────────
    //
    // Accepts a string URL or an attrset { url, sha256? }.
    // Downloads the tarball, extracts it to a temp directory, and returns
    // the path to the extracted contents.

    register_builtin(&mut builtins_set, "fetchTarball", |args| {
        let (url, expected_sha256) = match &args[0] {
            Value::String(s) => (s.clone(), None),
            Value::Attrs(a) => {
                let u = a
                    .get("url")
                    .ok_or_else(|| EvalError::AttrNotFound("url".into()))?
                    .as_string()?
                    .to_string();
                let sha = a
                    .get("sha256")
                    .map(|v| v.as_string().map(|s| s.to_string()))
                    .transpose()?;
                (u, sha)
            }
            _ => {
                return Err(EvalError::TypeError(
                    "fetchTarball: expected string or attrset".into(),
                ))
            }
        };
        let bytes = fetch_url_bytes(&url)
            .map_err(|e| EvalError::TypeError(format!("fetchTarball: {e}")))?;
        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(&bytes));
        if let Some(ref expected) = expected_sha256 {
            if *expected != hash {
                return Err(EvalError::TypeError(format!(
                    "fetchTarball: sha256 mismatch: expected {expected}, got {hash}"
                )));
            }
        }
        let base_dir = std::env::temp_dir().join("sui-fetchTarball");
        let extract_dir = base_dir.join(&hash);
        if !extract_dir.exists() {
            std::fs::create_dir_all(&extract_dir)
                .map_err(|e| EvalError::TypeError(format!("fetchTarball: {e}")))?;
            let decoder = flate2::read::GzDecoder::new(&bytes[..]);
            let mut archive = tar::Archive::new(decoder);
            archive
                .unpack(&extract_dir)
                .map_err(|e| EvalError::TypeError(format!("fetchTarball: {e}")))?;
        }
        Ok(Value::Path(extract_dir.to_string_lossy().to_string()))
    });

    // ── getFlake ──────────────────────────────────────────
    //
    // Minimal implementation: supports path-based flake references only.
    // Reads flake.nix from the given path and evaluates it.

    register_builtin(&mut builtins_set, "getFlake", |args| {
        let flake_ref = args[0].as_string()?.to_string();
        let flake_dir = if flake_ref.starts_with('/') || flake_ref.starts_with('.') {
            std::path::PathBuf::from(&flake_ref)
        } else {
            return Err(EvalError::NotImplemented(format!(
                "getFlake: only path-based flakes supported, got: {flake_ref}"
            )));
        };
        let flake_nix = flake_dir.join("flake.nix");
        let source = std::fs::read_to_string(&flake_nix).map_err(|e| {
            EvalError::TypeError(format!(
                "getFlake: cannot read {}: {e}",
                flake_nix.display()
            ))
        })?;
        crate::eval::eval(&source)
    });

    // ── path ──────────────────────────────────────────────
    //
    // builtins.path { path; name?; sha256?; recursive?; }
    // Hashes the path contents and returns a synthetic store path.

    register_builtin(&mut builtins_set, "path", |args| {
        let attrs = args[0].as_attrs()?;
        let path_val = attrs
            .get("path")
            .ok_or_else(|| EvalError::AttrNotFound("path".into()))?;
        let path_str = match path_val {
            Value::Path(p) => p.clone(),
            Value::String(s) => s.clone(),
            _ => return Err(EvalError::TypeError("path: expected path".into())),
        };
        let name = attrs
            .get("name")
            .map(|v| v.as_string().map(|s| s.to_string()))
            .transpose()?
            .unwrap_or_else(|| {
                std::path::Path::new(&path_str)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        let p = std::path::Path::new(&path_str);
        if p.is_file() {
            let content = std::fs::read(p)
                .map_err(|e| EvalError::TypeError(format!("path: {e}")))?;
            hasher.update(&content);
        } else if p.is_dir() {
            // Hash the directory name for deterministic output
            hasher.update(path_str.as_bytes());
        } else {
            hasher.update(path_str.as_bytes());
        }
        if let Some(expected) = attrs.get("sha256") {
            let expected_str = expected.as_string()?;
            let actual = format!("{:x}", hasher.clone().finalize());
            if expected_str != actual {
                return Err(EvalError::TypeError(format!(
                    "path: sha256 mismatch: expected {expected_str}, got {actual}"
                )));
            }
        }
        let hash = format!("{:x}", hasher.finalize());
        let store_path = format!("/nix/store/{}-{}", &hash[..32], name);
        Ok(Value::Path(store_path))
    });

    // true/false/null as builtins
    builtins_set.insert("true".to_string(), Value::Bool(true));
    builtins_set.insert("false".to_string(), Value::Bool(false));
    builtins_set.insert("null".to_string(), Value::Null);
    builtins_set.insert("nixVersion".to_string(), Value::String("sui-0.1.0".to_string()));
    builtins_set.insert("currentSystem".to_string(), Value::String(current_system().to_string()));
    builtins_set.insert("langVersion".to_string(), Value::Int(6));

    env.bind("builtins".to_string(), Value::Attrs(builtins_set.clone()));

    // Also bind common builtins at top level (Nix does this)
    for name in ["true", "false", "null", "throw", "abort", "toString", "import"] {
        if let Some(v) = builtins_set.get(name) {
            env.bind(name.to_string(), v.clone());
        }
    }
}

fn register_builtin(
    attrs: &mut NixAttrs,
    name: &'static str,
    func: impl Fn(&[Value]) -> Result<Value, EvalError> + 'static,
) {
    attrs.insert(
        name.to_string(),
        Value::Builtin(BuiltinFn {
            name,
            func: Arc::new(func),
        }),
    );
}

fn register_curried(
    attrs: &mut NixAttrs,
    name: &'static str,
    func: impl Fn(&Value, &Value) -> Result<Value, EvalError> + Clone + 'static,
) {
    let f = func.clone();
    attrs.insert(
        name.to_string(),
        Value::Builtin(BuiltinFn {
            name,
            func: Arc::new(move |args| {
                let a = args[0].clone();
                let f2 = f.clone();
                Ok(Value::Builtin(BuiltinFn {
                    name: "curried<partial>",
                    func: Arc::new(move |args2| f2(&a, &args2[0])),
                }))
            }),
        }),
    );
}

/// Fetch bytes from a URL. Supports `file://` scheme for local files and
/// delegates to `reqwest::blocking` for HTTP(S).
fn fetch_url_bytes(url: &str) -> Result<Vec<u8>, String> {
    if let Some(path) = url.strip_prefix("file://") {
        std::fs::read(path).map_err(|e| format!("{e}"))
    } else {
        let resp = reqwest::blocking::get(url).map_err(|e| format!("{e}"))?;
        let bytes = resp.bytes().map_err(|e| format!("{e}"))?;
        Ok(bytes.to_vec())
    }
}

fn json_to_value(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(arr) => {
            Value::List(arr.iter().map(json_to_value).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut attrs = NixAttrs::new();
            for (k, v) in obj {
                attrs.insert(k.clone(), json_to_value(v));
            }
            Value::Attrs(attrs)
        }
    }
}

fn current_system() -> &'static str {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "aarch64-darwin"
        } else {
            "x86_64-darwin"
        }
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-linux"
    } else {
        "x86_64-linux"
    }
}

/// Compare two version strings, returning -1, 0, or 1.
///
/// Splits on `.`, `-`, AND digit/letter boundaries (matching Nix behavior).
/// Compares components numerically where possible, lexicographically otherwise.
/// The special component `"pre"` is less than everything except itself and empty.
fn compare_versions(a: &str, b: &str) -> i64 {
    let split = |s: &str| -> Vec<String> {
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut prev_digit: Option<bool> = None;
        for ch in s.chars() {
            if ch == '.' || ch == '-' {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                    prev_digit = None;
                }
            } else {
                let is_digit = ch.is_ascii_digit();
                if let Some(was_digit) = prev_digit {
                    if is_digit != was_digit && !current.is_empty() {
                        parts.push(std::mem::take(&mut current));
                    }
                }
                current.push(ch);
                prev_digit = Some(is_digit);
            }
        }
        if !current.is_empty() {
            parts.push(current);
        }
        parts
    };
    let pa = split(a);
    let pb = split(b);
    let max_len = pa.len().max(pb.len());
    for i in 0..max_len {
        let ca = pa.get(i).map(|s| s.as_str()).unwrap_or("");
        let cb = pb.get(i).map(|s| s.as_str()).unwrap_or("");
        // Try numeric comparison first
        let ord = match (ca.parse::<i64>(), cb.parse::<i64>()) {
            (Ok(na), Ok(nb)) => na.cmp(&nb),
            _ => {
                // Nix: "pre" is less than everything except itself and empty
                match (ca, cb) {
                    ("pre", "pre") => std::cmp::Ordering::Equal,
                    ("pre", _) => std::cmp::Ordering::Less,
                    (_, "pre") => std::cmp::Ordering::Greater,
                    _ => ca.cmp(cb),
                }
            }
        };
        if ord != std::cmp::Ordering::Equal {
            return if ord == std::cmp::Ordering::Less { -1 } else { 1 };
        }
    }
    0
}

/// Parse a derivation name into (name, version).
///
/// The version starts at the last `-` followed by a digit.
/// e.g. "hello-2.10" => ("hello", "2.10"), "openssl-1.1.1k" => ("openssl", "1.1.1k")
fn parse_drv_name(s: &str) -> (String, String) {
    // Find the last '-' that is followed by a digit
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).rev() {
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            return (s[..i].to_string(), s[i + 1..].to_string());
        }
    }
    (s.to_string(), String::new())
}

#[cfg(test)]
mod tests {
    use crate::eval::eval;
    use crate::value::Value;

    fn ev(input: &str) -> Value {
        eval(input).unwrap()
    }

    #[test]
    fn builtins_gen_list_generates_correct_list() {
        // genList (x: x * 2) 4 => [0 2 4 6]
        let v = ev("builtins.genList (x: x * 2) 4");
        assert_eq!(
            v,
            Value::List(vec![
                Value::Int(0),
                Value::Int(2),
                Value::Int(4),
                Value::Int(6),
            ]),
        );
    }

    #[test]
    fn builtins_gen_list_zero_length() {
        let v = ev("builtins.genList (x: x) 0");
        assert_eq!(v, Value::List(vec![]));
    }

    #[test]
    fn builtins_elem_finds_element() {
        assert_eq!(ev("builtins.elem 2 [1 2 3]"), Value::Bool(true));
    }

    #[test]
    fn builtins_elem_missing_element() {
        assert_eq!(ev("builtins.elem 5 [1 2 3]"), Value::Bool(false));
    }

    #[test]
    fn builtins_throw_produces_error() {
        let result = eval(r#"builtins.throw "kaboom""#);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("kaboom"));
    }

    #[test]
    fn builtins_seq_forces_first_arg() {
        // seq evaluates first arg then returns second
        assert_eq!(ev("builtins.seq 1 42"), Value::Int(42));
        assert_eq!(ev(r#"builtins.seq "forced" true"#), Value::Bool(true));
    }

    #[test]
    fn builtins_current_system_valid_string() {
        let v = ev("builtins.currentSystem");
        if let Value::String(s) = v {
            // Should match one of the known system strings
            assert!(
                ["aarch64-darwin", "x86_64-darwin", "aarch64-linux", "x86_64-linux"]
                    .contains(&s.as_str()),
                "unexpected system string: {s}",
            );
        } else {
            panic!("expected string for currentSystem");
        }
    }

    #[test]
    fn builtins_lang_version_is_int() {
        let v = ev("builtins.langVersion");
        assert!(matches!(v, Value::Int(_)));
    }

    #[test]
    fn builtins_nix_version_is_string() {
        let v = ev("builtins.nixVersion");
        assert!(matches!(v, Value::String(_)));
    }

    #[test]
    fn builtins_is_function() {
        assert_eq!(ev("builtins.isFunction (x: x)"), Value::Bool(true));
        assert_eq!(ev("builtins.isFunction builtins.head"), Value::Bool(true));
        assert_eq!(ev("builtins.isFunction 42"), Value::Bool(false));
    }

    #[test]
    fn builtins_is_path() {
        assert_eq!(ev("builtins.isPath ./foo"), Value::Bool(true));
        assert_eq!(ev("builtins.isPath 42"), Value::Bool(false));
    }

    #[test]
    fn builtins_elem_at() {
        assert_eq!(ev("builtins.elemAt [10 20 30] 1"), Value::Int(20));
    }

    #[test]
    fn builtins_has_attr() {
        assert_eq!(ev(r#"builtins.hasAttr "a" { a = 1; }"#), Value::Bool(true));
        assert_eq!(ev(r#"builtins.hasAttr "b" { a = 1; }"#), Value::Bool(false));
    }

    #[test]
    fn builtins_get_attr() {
        assert_eq!(ev(r#"builtins.getAttr "a" { a = 42; }"#), Value::Int(42));
    }

    // ── New builtins tests ───────────────────────────────

    #[test]
    fn builtins_map() {
        assert_eq!(
            ev("builtins.map (x: x * 2) [1 2 3]"),
            Value::List(vec![Value::Int(2), Value::Int(4), Value::Int(6)]),
        );
    }

    #[test]
    fn builtins_map_empty() {
        assert_eq!(ev("builtins.map (x: x) []"), Value::List(vec![]));
    }

    #[test]
    fn builtins_filter() {
        assert_eq!(
            ev("builtins.filter (x: x > 2) [1 2 3 4 5]"),
            Value::List(vec![Value::Int(3), Value::Int(4), Value::Int(5)]),
        );
    }

    #[test]
    fn builtins_filter_empty() {
        assert_eq!(ev("builtins.filter (x: false) [1 2 3]"), Value::List(vec![]));
    }

    #[test]
    fn builtins_foldl() {
        assert_eq!(ev("builtins.foldl' (a: b: a + b) 0 [1 2 3 4]"), Value::Int(10));
    }

    #[test]
    fn builtins_foldl_empty() {
        assert_eq!(ev("builtins.foldl' (a: b: a + b) 0 []"), Value::Int(0));
    }

    #[test]
    fn builtins_concat_map() {
        assert_eq!(
            ev("builtins.concatMap (x: [x (x * 2)]) [1 2 3]"),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(2), Value::Int(4), Value::Int(3), Value::Int(6)]),
        );
    }

    #[test]
    fn builtins_concat_lists() {
        assert_eq!(
            ev("builtins.concatLists [[1 2] [3] [4 5]]"),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4), Value::Int(5)]),
        );
    }

    #[test]
    fn builtins_all() {
        assert_eq!(ev("builtins.all (x: x > 0) [1 2 3]"), Value::Bool(true));
        assert_eq!(ev("builtins.all (x: x > 2) [1 2 3]"), Value::Bool(false));
    }

    #[test]
    fn builtins_any() {
        assert_eq!(ev("builtins.any (x: x > 2) [1 2 3]"), Value::Bool(true));
        assert_eq!(ev("builtins.any (x: x > 5) [1 2 3]"), Value::Bool(false));
    }

    #[test]
    fn builtins_map_attrs() {
        let v = ev(r#"builtins.mapAttrs (name: value: value * 2) { a = 1; b = 2; }"#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("a"), Some(&Value::Int(2)));
            assert_eq!(a.get("b"), Some(&Value::Int(4)));
        } else { panic!("expected attrs"); }
    }

    #[test]
    fn builtins_list_to_attrs() {
        let v = ev(r#"builtins.listToAttrs [{ name = "a"; value = 1; } { name = "b"; value = 2; }]"#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("a"), Some(&Value::Int(1)));
            assert_eq!(a.get("b"), Some(&Value::Int(2)));
        } else { panic!("expected attrs"); }
    }

    #[test]
    fn builtins_remove_attrs() {
        let v = ev(r#"builtins.removeAttrs { a = 1; b = 2; c = 3; } ["b" "c"]"#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.len(), 1);
            assert_eq!(a.get("a"), Some(&Value::Int(1)));
        } else { panic!("expected attrs"); }
    }

    #[test]
    fn builtins_concat_strings_sep() {
        assert_eq!(
            ev(r#"builtins.concatStringsSep ", " ["a" "b" "c"]"#),
            Value::String("a, b, c".to_string()),
        );
    }

    #[test]
    fn builtins_has_prefix() {
        assert_eq!(ev(r#"builtins.hasPrefix "foo" "foobar""#), Value::Bool(true));
        assert_eq!(ev(r#"builtins.hasPrefix "bar" "foobar""#), Value::Bool(false));
    }

    #[test]
    fn builtins_has_suffix() {
        assert_eq!(ev(r#"builtins.hasSuffix "bar" "foobar""#), Value::Bool(true));
        assert_eq!(ev(r#"builtins.hasSuffix "foo" "foobar""#), Value::Bool(false));
    }

    #[test]
    fn builtins_replace_strings() {
        assert_eq!(
            ev(r#"builtins.replaceStrings ["foo" "bar"] ["FOO" "BAR"] "foobar""#),
            Value::String("FOOBAR".to_string()),
        );
    }

    #[test]
    fn builtins_ceil_floor() {
        assert_eq!(ev("builtins.ceil 3.2"), Value::Int(4));
        assert_eq!(ev("builtins.floor 3.8"), Value::Int(3));
    }

    #[test]
    fn builtins_try_eval() {
        let v = ev("builtins.tryEval 42");
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("success"), Some(&Value::Bool(true)));
            assert_eq!(a.get("value"), Some(&Value::Int(42)));
        } else { panic!("expected attrs"); }
    }

    #[test]
    fn builtins_trace() {
        assert_eq!(ev(r#"builtins.trace "debug msg" 42"#), Value::Int(42));
    }

    #[test]
    fn builtins_function_args() {
        let v = ev("builtins.functionArgs ({ a, b ? 1 }: a)");
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("a"), Some(&Value::Bool(false)));
            assert_eq!(a.get("b"), Some(&Value::Bool(true)));
        } else { panic!("expected attrs"); }
    }

    #[test]
    fn builtins_sort() {
        assert_eq!(
            ev("builtins.sort (a: b: a < b) [3 1 2]"),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn builtins_cat_attrs() {
        assert_eq!(
            ev(r#"builtins.catAttrs "a" [{ a = 1; } { b = 2; } { a = 3; }]"#),
            Value::List(vec![Value::Int(1), Value::Int(3)]),
        );
    }

    // ── New builtins: concatStrings ─────────────────────────

    #[test]
    fn builtins_concat_strings() {
        assert_eq!(
            ev(r#"builtins.concatStrings ["hello" " " "world"]"#),
            Value::String("hello world".to_string()),
        );
    }

    #[test]
    fn builtins_concat_strings_empty() {
        assert_eq!(
            ev(r#"builtins.concatStrings []"#),
            Value::String("".to_string()),
        );
    }

    // ── New builtins: partition ──────────────────────────────

    #[test]
    fn builtins_partition_basic() {
        let v = ev("builtins.partition (x: x > 2) [1 2 3 4 5]");
        if let Value::Attrs(a) = v {
            assert_eq!(
                a.get("right"),
                Some(&Value::List(vec![Value::Int(3), Value::Int(4), Value::Int(5)])),
            );
            assert_eq!(
                a.get("wrong"),
                Some(&Value::List(vec![Value::Int(1), Value::Int(2)])),
            );
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_partition_all_right() {
        let v = ev("builtins.partition (x: true) [1 2 3]");
        if let Value::Attrs(a) = v {
            assert_eq!(
                a.get("right"),
                Some(&Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])),
            );
            assert_eq!(a.get("wrong"), Some(&Value::List(vec![])));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_partition_empty() {
        let v = ev("builtins.partition (x: true) []");
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("right"), Some(&Value::List(vec![])));
            assert_eq!(a.get("wrong"), Some(&Value::List(vec![])));
        } else {
            panic!("expected attrs");
        }
    }

    // ── New builtins: groupBy ───────────────────────────────

    #[test]
    fn builtins_group_by_basic() {
        let v = ev(r#"builtins.groupBy (x: x) ["a" "b" "a" "c" "b"]"#);
        if let Value::Attrs(a) = v {
            assert_eq!(
                a.get("a"),
                Some(&Value::List(vec![
                    Value::String("a".to_string()),
                    Value::String("a".to_string()),
                ])),
            );
            assert_eq!(
                a.get("b"),
                Some(&Value::List(vec![
                    Value::String("b".to_string()),
                    Value::String("b".to_string()),
                ])),
            );
            assert_eq!(
                a.get("c"),
                Some(&Value::List(vec![Value::String("c".to_string())])),
            );
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_group_by_empty() {
        let v = ev(r#"builtins.groupBy (x: x) []"#);
        if let Value::Attrs(a) = v {
            assert!(a.is_empty());
        } else {
            panic!("expected attrs");
        }
    }

    // ── New builtins: zipAttrsWith ──────────────────────────

    #[test]
    fn builtins_zip_attrs_with_basic() {
        // zipAttrsWith (name: values: values) [{ a = 1; } { a = 2; b = 3; }]
        let v = ev("builtins.zipAttrsWith (name: values: values) [{ a = 1; } { a = 2; b = 3; }]");
        if let Value::Attrs(a) = v {
            assert_eq!(
                a.get("a"),
                Some(&Value::List(vec![Value::Int(1), Value::Int(2)])),
            );
            assert_eq!(
                a.get("b"),
                Some(&Value::List(vec![Value::Int(3)])),
            );
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_zip_attrs_with_sum() {
        // Sum values for each key
        let v = ev(r#"builtins.zipAttrsWith (name: values: builtins.foldl' (a: b: a + b) 0 values) [{ x = 1; } { x = 2; } { x = 3; }]"#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("x"), Some(&Value::Int(6)));
        } else {
            panic!("expected attrs");
        }
    }

    // ── New builtins: compareVersions ─────────��─────────────

    #[test]
    fn builtins_compare_versions_equal() {
        assert_eq!(ev(r#"builtins.compareVersions "1.2.3" "1.2.3""#), Value::Int(0));
    }

    #[test]
    fn builtins_compare_versions_less() {
        assert_eq!(ev(r#"builtins.compareVersions "1.2.3" "1.2.4""#), Value::Int(-1));
        assert_eq!(ev(r#"builtins.compareVersions "1.2" "1.3""#), Value::Int(-1));
    }

    #[test]
    fn builtins_compare_versions_greater() {
        assert_eq!(ev(r#"builtins.compareVersions "1.3.0" "1.2.9""#), Value::Int(1));
    }

    #[test]
    fn builtins_compare_versions_pre() {
        // "pre" is less than anything except itself
        assert_eq!(ev(r#"builtins.compareVersions "1.0pre1" "1.0.1""#), Value::Int(-1));
    }

    // ── New builtins: parseDrvName ──────────────────────────

    #[test]
    fn builtins_parse_drv_name_basic() {
        let v = ev(r#"builtins.parseDrvName "hello-2.10""#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("name"), Some(&Value::String("hello".to_string())));
            assert_eq!(a.get("version"), Some(&Value::String("2.10".to_string())));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_parse_drv_name_no_version() {
        let v = ev(r#"builtins.parseDrvName "hello""#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("name"), Some(&Value::String("hello".to_string())));
            assert_eq!(a.get("version"), Some(&Value::String("".to_string())));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_parse_drv_name_complex() {
        let v = ev(r#"builtins.parseDrvName "openssl-1.1.1k""#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("name"), Some(&Value::String("openssl".to_string())));
            assert_eq!(a.get("version"), Some(&Value::String("1.1.1k".to_string())));
        } else {
            panic!("expected attrs");
        }
    }

    // ── New builtins: baseNameOf / dirOf ────────────────────

    #[test]
    fn builtins_base_name_of() {
        assert_eq!(
            ev(r#"builtins.baseNameOf "/nix/store/abc-hello""#),
            Value::String("abc-hello".to_string()),
        );
        assert_eq!(
            ev(r#"builtins.baseNameOf "hello.txt""#),
            Value::String("hello.txt".to_string()),
        );
    }

    #[test]
    fn builtins_dir_of_string() {
        assert_eq!(
            ev(r#"builtins.dirOf "/nix/store/abc""#),
            Value::String("/nix/store".to_string()),
        );
        assert_eq!(
            ev(r#"builtins.dirOf "/foo""#),
            Value::String("/".to_string()),
        );
    }

    #[test]
    fn builtins_dir_of_path() {
        assert_eq!(
            ev("builtins.dirOf /nix/store/abc"),
            Value::Path("/nix/store".to_string()),
        );
    }

    // ── New builtins: readFile ──────────────────────────────

    #[test]
    fn builtins_read_file() {
        // Create a temp file and read it
        let dir = std::env::temp_dir();
        let path = dir.join("sui_eval_test_read_file.txt");
        std::fs::write(&path, "hello from test").unwrap();
        let expr = format!(r#"builtins.readFile "{}""#, path.display());
        let v = eval(&expr).unwrap();
        if let Value::String(s) = v {
            assert_eq!(s, "hello from test");
        } else {
            panic!("expected string");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn builtins_read_file_missing() {
        let result = eval(r#"builtins.readFile "/nonexistent/path/file.txt""#);
        assert!(result.is_err());
    }

    // ── New builtins: addErrorContext ────────────────────────

    #[test]
    fn builtins_add_error_context_passthrough() {
        // addErrorContext just passes through the value
        assert_eq!(
            ev(r#"builtins.addErrorContext "context msg" 42"#),
            Value::Int(42),
        );
    }

    // ── __functor protocol ──────────────────────────────────

    #[test]
    fn functor_basic() {
        assert_eq!(
            ev("let s = { __functor = self: x: self.value + x; value = 10; }; in s 5"),
            Value::Int(15),
        );
    }

    #[test]
    fn functor_nested() {
        // The functor can return another functor
        assert_eq!(
            ev("let s = { __functor = self: x: x * 2; }; in s 21"),
            Value::Int(42),
        );
    }

    #[test]
    fn functor_with_update() {
        // Common pattern: { __functor = ...; } // { value = ...; }
        assert_eq!(
            ev(r#"
                let
                    base = { __functor = self: x: self.v + x; v = 0; };
                    extended = base // { v = 100; };
                in extended 5
            "#),
            Value::Int(105),
        );
    }

    // ── __toString protocol ─────────────────────────────────

    #[test]
    fn to_string_protocol_interpolation() {
        assert_eq!(
            ev(r#"let s = { __toString = self: "hello"; }; in "${s}""#),
            Value::String("hello".to_string()),
        );
    }

    #[test]
    fn to_string_protocol_with_self() {
        assert_eq!(
            ev(r#"let s = { __toString = self: self.name; name = "world"; }; in "${s}""#),
            Value::String("world".to_string()),
        );
    }

    #[test]
    fn to_string_protocol_via_builtin() {
        assert_eq!(
            ev(r#"builtins.toString { __toString = self: "custom"; }"#),
            Value::String("custom".to_string()),
        );
    }

    // ── Ignored tests for features needing major work ───────

    #[test]
    fn builtins_hash_string_sha256() {
        let v = ev(r#"builtins.hashString "sha256" "hello""#);
        if let Value::String(s) = v {
            assert_eq!(s.len(), 64); // SHA-256 hex is 64 chars
            assert_eq!(s, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        } else {
            panic!("expected string");
        }
    }

    #[test]
    fn builtins_hash_string_sha512() {
        let v = ev(r#"builtins.hashString "sha512" "hello""#);
        if let Value::String(s) = v {
            assert_eq!(s.len(), 128); // SHA-512 hex is 128 chars
        } else {
            panic!("expected string");
        }
    }

    #[test]
    fn builtins_match_regex() {
        // match returns null on no match, list of groups on match
        assert_eq!(
            ev(r#"builtins.match "([0-9]+)\\.([0-9]+)" "1.23""#),
            Value::List(vec![Value::String("1".to_string()), Value::String("23".to_string())]),
        );
        assert_eq!(
            ev(r#"builtins.match "([0-9]+)" "abc""#),
            Value::Null,
        );
    }

    #[test]
    fn builtins_match_full_string() {
        // match anchors to full string
        assert_eq!(
            ev(r#"builtins.match "([0-9]+)" "42""#),
            Value::List(vec![Value::String("42".to_string())]),
        );
        // Partial match should return null (anchored)
        assert_eq!(
            ev(r#"builtins.match "([0-9]+)" "abc42def""#),
            Value::Null,
        );
    }

    #[test]
    fn builtins_import_file() {
        // Create a temp file and import it
        let dir = std::env::temp_dir();
        let path = dir.join("sui_eval_test_import.nix");
        std::fs::write(&path, "{ x = 42; }").unwrap();
        let expr = format!(r#"(builtins.import "{}").x"#, path.display());
        let v = eval(&expr).unwrap();
        assert_eq!(v, Value::Int(42));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn builtins_import_expr() {
        // Import a file that returns a simple expression
        let dir = std::env::temp_dir();
        let path = dir.join("sui_eval_test_import_expr.nix");
        std::fs::write(&path, "1 + 2").unwrap();
        let expr = format!(r#"builtins.import "{}""#, path.display());
        let v = eval(&expr).unwrap();
        assert_eq!(v, Value::Int(3));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn builtins_derivation_stub() {
        let v = eval(r#"builtins.derivation { name = "test"; system = "x86_64-linux"; builder = "/bin/sh"; }"#).unwrap();
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("type"), Some(&Value::String("derivation".to_string())));
            assert_eq!(a.get("drvPath"), Some(&Value::String("/nix/store/stub-test.drv".to_string())));
            assert_eq!(a.get("outPath"), Some(&Value::String("/nix/store/stub-test".to_string())));
            assert_eq!(a.get("name"), Some(&Value::String("test".to_string())));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_fetchurl_exists_as_builtin() {
        // Verify fetchurl is registered and callable.
        // Test with a file:// URL served from a temp file to avoid network.
        let dir = std::env::temp_dir().join("sui_eval_test_fetchurl");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = b"fetchurl-test-content";
        let file = dir.join("payload.txt");
        std::fs::write(&file, payload).unwrap();
        let file_url = format!("file://{}", file.display());
        let expr = format!(r#"builtins.fetchurl "{}""#, file_url);
        let v = eval(&expr).unwrap();
        if let Value::Path(p) = v {
            let content = std::fs::read_to_string(&p).unwrap();
            assert_eq!(content, "fetchurl-test-content");
        } else {
            panic!("expected path, got {v}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtins_fetchurl_attrset_form() {
        // Test the attrset form: { url, sha256? }
        let dir = std::env::temp_dir().join("sui_eval_test_fetchurl_attr");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = b"attr-form-content";
        let file = dir.join("payload.txt");
        std::fs::write(&file, payload).unwrap();
        let file_url = format!("file://{}", file.display());
        let expr = format!(
            r#"builtins.fetchurl {{ url = "{}"; }}"#,
            file_url
        );
        let v = eval(&expr).unwrap();
        assert!(matches!(v, Value::Path(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtins_fetchurl_bad_type_errors() {
        let result = eval("builtins.fetchurl 42");
        assert!(result.is_err());
    }

    #[test]
    fn builtins_read_dir() {
        let dir = std::env::temp_dir().join("sui_eval_test_readdir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.txt"), "content").unwrap();
        std::fs::create_dir(dir.join("subdir")).unwrap();
        let expr = format!(r#"builtins.readDir "{}""#, dir.display());
        let v = eval(&expr).unwrap();
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("file.txt"), Some(&Value::String("regular".to_string())));
            assert_eq!(a.get("subdir"), Some(&Value::String("directory".to_string())));
        } else {
            panic!("expected attrs");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtins_read_dir_empty() {
        let dir = std::env::temp_dir().join("sui_eval_test_readdir_empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let expr = format!(r#"builtins.readDir "{}""#, dir.display());
        let v = eval(&expr).unwrap();
        if let Value::Attrs(a) = v {
            assert!(a.is_empty());
        } else {
            panic!("expected attrs");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtins_path_with_file() {
        // builtins.path on a real file returns a /nix/store/... path
        let dir = std::env::temp_dir().join("sui_eval_test_builtins_path");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        std::fs::write(&file, "hello world").unwrap();
        let expr = format!(
            r#"builtins.path {{ path = "{}"; name = "test"; }}"#,
            file.display()
        );
        let v = eval(&expr).unwrap();
        if let Value::Path(p) = v {
            assert!(p.starts_with("/nix/store/"));
            assert!(p.ends_with("-test"));
        } else {
            panic!("expected path, got {v}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtins_path_default_name() {
        // Without explicit name, uses the file name component
        let dir = std::env::temp_dir().join("sui_eval_test_builtins_path_dn");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("myfile.txt");
        std::fs::write(&file, "content").unwrap();
        let expr = format!(
            r#"builtins.path {{ path = "{}"; }}"#,
            file.display()
        );
        let v = eval(&expr).unwrap();
        if let Value::Path(p) = v {
            assert!(p.starts_with("/nix/store/"));
            assert!(p.ends_with("-myfile.txt"));
        } else {
            panic!("expected path, got {v}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtins_placeholder() {
        let v = ev(r#"builtins.placeholder "out""#);
        if let Value::String(s) = v {
            assert!(s.starts_with("/placeholder-"));
            assert_eq!(s.len(), "/placeholder-".len() + 32);
        } else {
            panic!("expected string");
        }
    }

    #[test]
    fn builtins_get_flake_path_based() {
        // getFlake with a path-based flake reference reads and evaluates flake.nix
        let dir = std::env::temp_dir().join("sui_eval_test_getflake");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("flake.nix"),
            r#"{ description = "test flake"; outputs = { self }: { value = 42; }; }"#,
        )
        .unwrap();
        let expr = format!(r#"(builtins.getFlake "{}").description"#, dir.display());
        let v = eval(&expr).unwrap();
        assert_eq!(v, Value::String("test flake".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtins_get_flake_rejects_registry_refs() {
        // Non-path flake references are not yet supported
        let result = eval(r#"builtins.getFlake "nixpkgs""#);
        assert!(result.is_err());
    }

    #[test]
    fn builtins_to_path() {
        let v = ev(r#"builtins.toPath "/foo/bar""#);
        assert_eq!(v, Value::Path("/foo/bar".to_string()));
    }

    #[test]
    fn builtins_to_path_rejects_relative() {
        let result = eval(r#"builtins.toPath "relative/path""#);
        assert!(result.is_err());
    }

    #[test]
    fn builtins_store_path() {
        let v = ev(r#"builtins.storePath "/nix/store/abc-hello""#);
        assert_eq!(v, Value::Path("/nix/store/abc-hello".to_string()));
    }

    #[test]
    fn builtins_store_path_rejects_non_store() {
        let result = eval(r#"builtins.storePath "/tmp/not-store""#);
        assert!(result.is_err());
    }

    #[test]
    fn builtins_fetch_tarball_from_file() {
        // Create a .tar.gz in a temp dir, fetch it via file:// URL
        let dir = std::env::temp_dir().join("sui_eval_test_fetchtarball");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Build a small tarball in memory
        let tar_gz_path = dir.join("archive.tar.gz");
        {
            let file = std::fs::File::create(&tar_gz_path).unwrap();
            let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar_builder = tar::Builder::new(enc);
            let data = b"hello tarball";
            let mut header = tar::Header::new_gnu();
            header.set_path("hello.txt").unwrap();
            header.set_size(data.len() as u64);
            header.set_cksum();
            tar_builder.append(&header, &data[..]).unwrap();
            tar_builder.finish().unwrap();
        }

        let file_url = format!("file://{}", tar_gz_path.display());
        let expr = format!(r#"builtins.fetchTarball "{}""#, file_url);
        let v = eval(&expr).unwrap();
        if let Value::Path(p) = v {
            // The extracted directory should exist
            assert!(
                std::path::Path::new(&p).exists(),
                "extracted dir should exist: {p}",
            );
        } else {
            panic!("expected path, got {v}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtins_fetch_tarball_bad_type_errors() {
        let result = eval("builtins.fetchTarball 42");
        assert!(result.is_err());
    }
}
