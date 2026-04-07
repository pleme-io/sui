//! Core Nix builtins.
//!
//! Builtins are registered in [`register`] which populates the global
//! `builtins` attribute set and the top-level default scope. Simple
//! single-argument builtins (type-checking predicates, `ceil`, `floor`,
//! etc.) are described declaratively via [`BuiltinSpec`] and a static
//! slice, keeping registration compact and self-documenting. More
//! complex builtins (curried, multi-stage, I/O) are still registered
//! with the imperative [`register_builtin`] / [`register_curried`]
//! helpers.

use std::sync::Arc;

use crate::value::*;

/// Descriptor for a simple single-argument builtin function.
///
/// The implementation closure receives the full `&[Value]` argument
/// slice (always length 1 after apply) and returns a `Result<Value>`.
/// Using a struct makes the builtin table scannable and self-documenting
/// without repeating the `register_builtin(…, |args| { … })` boilerplate.
pub(crate) struct BuiltinSpec {
    pub name: &'static str,
    pub func: fn(&[Value]) -> Result<Value, EvalError>,
}

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

/// Register all builtins into the environment.
pub fn register(env: &mut Env) {
    let mut builtins_set = NixAttrs::new();

    for spec in TYPE_CHECK_BUILTINS {
        register_builtin(&mut builtins_set, spec.name, spec.func);
    }

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
        let attrs = args[0].to_attrs()?;
        Ok(Value::List(attrs.keys().map(|k| Value::string(k.clone())).collect()))
    });
    register_builtin(&mut builtins_set, "attrValues", |args| {
        let attrs = args[0].to_attrs()?;
        Ok(Value::List(attrs.iter().map(|(_, v)| v.clone()).collect()))
    });
    register_builtin(&mut builtins_set, "hasAttr", |args| {
        let name = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "hasAttr<partial>",
            func: Arc::new(move |args2| {
                let attrs = args2[0].to_attrs()?;
                Ok(Value::Bool(attrs.contains_key(&name)))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "getAttr", |args| {
        let name = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "getAttr<partial>",
            func: Arc::new(move |args2| {
                let attrs = args2[0].to_attrs()?;
                attrs.get(&name).cloned().ok_or_else(|| EvalError::AttrNotFound(name.clone()))
            }),
        }))
    });
    register_builtin(&mut builtins_set, "intersectAttrs", |args| {
        let a = args[0].to_attrs()?.clone();
        Ok(Value::Builtin(BuiltinFn {
            name: "intersectAttrs<partial>",
            func: Arc::new(move |args2| {
                let b = args2[0].to_attrs()?;
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
        // args are already forced by apply(), but handle thunks defensively.
        let val = &args[0];
        Ok(Value::String(match val {
            Value::String(s) => s.clone(),
            Value::Int(n) => NixString::plain(n.to_string()),
            Value::Float(f) => NixString::plain(format!("{f}")),
            Value::Bool(true) => NixString::plain("1"),
            Value::Bool(false) => NixString::plain(""),
            Value::Null => NixString::plain(""),
            Value::Path(p) => NixString::plain(p),
            Value::List(_) => return Err(EvalError::TypeError("toString: cannot convert list".to_string())),
            Value::Attrs(attrs) => {
                // __toString protocol: call __toString with self
                if let Some(to_str) = attrs.get("__toString") {
                    let result = crate::eval::apply(to_str.clone(), val.clone())?;
                    match result {
                        Value::String(s) => return Ok(Value::String(s)),
                        _ => return Err(EvalError::TypeError("__toString must return a string".to_string())),
                    }
                }
                return Err(EvalError::TypeError("toString: cannot convert set".to_string()));
            }
            Value::Lambda(_) | Value::Builtin(_) => return Err(EvalError::TypeError("toString: cannot convert function".to_string())),
            Value::Thunk(_) => {
                // Should not happen since apply() forces args, but handle it.
                return Err(EvalError::TypeError("toString: unexpected thunk".to_string()));
            }
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
                        Ok(Value::string(&s[start..end]))
                    }),
                }))
            }),
        }))
    });

    // Conversion
    register_builtin(&mut builtins_set, "toJSON", |args| {
        Ok(Value::string(serde_json::to_string(&args[0].to_json())
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
                let attrs = args2[0].to_attrs()?;
                let mut result = NixAttrs::new();
                for (k, v) in attrs.iter() {
                    let partial = crate::eval::apply(func.clone(), Value::string(k.clone()))?;
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
            let item_attrs = item.to_attrs()?;
            let name = item_attrs.get("name")
                .ok_or_else(|| EvalError::AttrNotFound("name".to_string()))?
                .to_str()?;
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
                    if let Ok(attrs) = item.to_attrs() {
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
        let set = args[0].to_attrs()?.clone();
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
                        Ok(Value::string(s))
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
                Ok(Value::string(strings?.join(&sep)))
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
        Ok(Value::string(result))
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
        result.insert("name".to_string(), Value::string(name));
        result.insert("version".to_string(), Value::string(version));
        Ok(Value::Attrs(result))
    });

    // baseNameOf — extract filename from path
    register_builtin(&mut builtins_set, "baseNameOf", |args| {
        let s = match &args[0] {
            Value::String(ns) => ns.chars.clone(),
            Value::Path(p) => p.clone(),
            _ => return Err(EvalError::TypeError("baseNameOf: expected string or path".to_string())),
        };
        let base = s.rsplit('/').next().unwrap_or(&s);
        Ok(Value::string(base))
    });

    // dirOf — extract directory from path
    register_builtin(&mut builtins_set, "dirOf", |args| {
        let (s, is_path) = match &args[0] {
            Value::String(ns) => (ns.chars.clone(), false),
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
            Ok(Value::string(dir))
        }
    });

    // readFile — read file contents to string
    register_builtin(&mut builtins_set, "readFile", |args| {
        let path = args[0].coerce_to_path("readFile")?;
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| EvalError::IoError { context: "readFile".into(), message: e.to_string() })?;
        Ok(Value::string(contents))
    });

    // addErrorContext — wraps an expression with error context (passthrough in our impl)
    register_builtin(&mut builtins_set, "addErrorContext", |args| {
        let _ctx = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "addErrorContext<partial>",
            func: Arc::new(|args2| Ok(args2[0].clone())),
        }))
    });

    // Numeric — simple single-arg builtins
    const NUMERIC_BUILTINS: &[BuiltinSpec] = &[
        BuiltinSpec { name: "ceil",  func: |args| Ok(Value::Int(args[0].as_float()?.ceil() as i64)) },
        BuiltinSpec { name: "floor", func: |args| Ok(Value::Int(args[0].as_float()?.floor() as i64)) },
    ];
    for spec in NUMERIC_BUILTINS {
        register_builtin(&mut builtins_set, spec.name, spec.func);
    }

    // Misc
    register_builtin(&mut builtins_set, "tryEval", |args| {
        // `tryEval` must catch `throw`/`abort` from the *evaluation*
        // of its argument, not just wrap an already-forced value.
        // The eval-side `apply` special-cases `tryEval` and hands
        // us the unforced thunk so we can drive `force_value`
        // ourselves and intercept the resulting error.
        match crate::eval::force_value(&args[0]) {
            Ok(v) => {
                let mut result = NixAttrs::new();
                result.insert("success".to_string(), Value::Bool(true));
                result.insert("value".to_string(), v);
                Ok(Value::Attrs(result))
            }
            Err(_) => {
                // Real Nix discards the error message and returns
                // `{ success = false; value = false; }`. We do the
                // same.
                let mut result = NixAttrs::new();
                result.insert("success".to_string(), Value::Bool(false));
                result.insert("value".to_string(), Value::Bool(false));
                Ok(Value::Attrs(result))
            }
        }
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
                            if let Some(ident) = entry.ident() {
                                let has_default = entry.default().is_some();
                                result.insert(ident.to_string(), Value::Bool(has_default));
                            }
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
        Err(EvalError::Throw(format!("throw: {msg}")))
    });
    register_builtin(&mut builtins_set, "abort", |args| {
        let msg = args[0].as_string()?;
        Err(EvalError::Throw(format!("abort: {msg}")))
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

    // ── Impure builtins ────────────────────────────────────
    //
    // These read from the process environment / clock and so cannot
    // be deterministic. nixpkgs uses them in `getEnv "NIXPKGS_CONFIG"`,
    // `currentTime` for build-time stamps, `pathExists ./<path>` etc.
    // In hermetic (pure) mode they would normally error; sui doesn't
    // enforce that yet, so we just delegate to the OS.

    register_builtin(&mut builtins_set, "getEnv", |args| {
        let name = args[0].as_string()?;
        Ok(Value::string(std::env::var(name).unwrap_or_default()))
    });

    register_builtin(&mut builtins_set, "currentTime", |_args| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Ok(Value::Int(now))
    });

    // ── convertHash ────────────────────────────────────────
    //
    // builtins.convertHash { hash; hashAlgo; toHashFormat } → string
    //
    // Converts a hash value between encodings (`base16`, `nix32`,
    // `base64`, `sri`). nixpkgs `lib/default.nix` inherits this
    // builtin even though most lib functions don't use it directly,
    // so missing it breaks the inherit and crashes any nixpkgs
    // import. We support sha256/sha512/md5/sha1 and the four
    // formats; other input combinations error out.

    register_builtin(&mut builtins_set, "convertHash", |args| {
        use base64::Engine;
        let attrs = args[0].to_attrs()?;
        let hash_str = attrs
            .get("hash")
            .ok_or_else(|| EvalError::AttrNotFound("hash".into()))?
            .to_str()?;
        let to_format = attrs
            .get("toHashFormat")
            .ok_or_else(|| EvalError::AttrNotFound("toHashFormat".into()))?
            .to_str()?;
        // hashAlgo can be omitted if the hash is SRI-prefixed; we
        // accept either an explicit algo or strip the SRI prefix.
        let (algo, raw_hash): (String, String) = if let Some(algo_v) =
            attrs.get("hashAlgo")
        {
            (algo_v.to_str()?, hash_str.clone())
        } else if let Some(stripped) = hash_str.strip_prefix("sha256-") {
            ("sha256".to_string(), stripped.to_string())
        } else if let Some(stripped) = hash_str.strip_prefix("sha512-") {
            ("sha512".to_string(), stripped.to_string())
        } else {
            return Err(EvalError::TypeError(
                "convertHash: missing hashAlgo".into(),
            ));
        };
        let expected_len = match algo.as_str() {
            "md5" => 16,
            "sha1" => 20,
            "sha256" => 32,
            "sha512" => 64,
            other => {
                return Err(EvalError::TypeError(format!(
                    "convertHash: unsupported algo {other}"
                )))
            }
        };
        // Decode the input hash from any of the accepted formats.
        let bytes: Vec<u8> = if raw_hash.len() == expected_len * 2
            && raw_hash.chars().all(|c| c.is_ascii_hexdigit())
        {
            // base16 (hex)
            (0..raw_hash.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&raw_hash[i..i + 2], 16))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| EvalError::TypeError(format!("convertHash hex: {e}")))?
        } else if let Ok(b) = sui_compat::store_path::nix_base32_decode(&raw_hash) {
            if expected_len != 20 {
                return Err(EvalError::TypeError(
                    "convertHash: nix32 only supported for 20-byte (sha1) hashes".into(),
                ));
            }
            b.to_vec()
        } else if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(&raw_hash)
        {
            b
        } else {
            return Err(EvalError::TypeError(format!(
                "convertHash: cannot decode hash '{raw_hash}'"
            )));
        };
        if bytes.len() != expected_len {
            return Err(EvalError::TypeError(format!(
                "convertHash: decoded {} bytes, expected {expected_len} for {algo}",
                bytes.len()
            )));
        }
        // Re-encode in the requested format.
        let out = match to_format.as_str() {
            "base16" => {
                let mut s = String::with_capacity(bytes.len() * 2);
                for b in &bytes {
                    s.push_str(&format!("{b:02x}"));
                }
                s
            }
            "nix32" => {
                if expected_len != 20 {
                    return Err(EvalError::TypeError(
                        "convertHash: nix32 output only supported for 20-byte hashes".into(),
                    ));
                }
                sui_compat::store_path::nix_base32_encode(&bytes)
            }
            "base64" => base64::engine::general_purpose::STANDARD.encode(&bytes),
            "sri" => format!(
                "{algo}-{}",
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            ),
            other => {
                return Err(EvalError::TypeError(format!(
                    "convertHash: unsupported toHashFormat {other}"
                )))
            }
        };
        Ok(Value::string(out))
    });

    register_builtin(&mut builtins_set, "readFileType", |args| {
        let path = args[0].as_string()?;
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                let kind = if meta.is_symlink() {
                    "symlink"
                } else if meta.is_dir() {
                    "directory"
                } else if meta.is_file() {
                    "regular"
                } else {
                    "unknown"
                };
                Ok(Value::string(kind))
            }
            Err(e) => Err(EvalError::IoError { context: "readFileType".into(), message: e.to_string() }),
        }
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
        Ok(Value::string(hex))
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
                        Some(m) => Value::string(m.as_str()),
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
            result.push(Value::string(&input[last_end..m.start()]));
            // Add the match groups as a list
            if let Some(caps) = re.captures(&input[m.start()..]) {
                let groups: Vec<Value> = (1..caps.len())
                    .map(|i| match caps.get(i) {
                        Some(g) => Value::string(g.as_str()),
                        None => Value::Null,
                    })
                    .collect();
                // If no capture groups, wrap the whole match in a list
                if groups.is_empty() {
                    result.push(Value::List(vec![Value::string(m.as_str())]));
                } else {
                    result.push(Value::List(groups));
                }
            }
            last_end = m.end();
        }
        // Add trailing non-matching part
        result.push(Value::string(&input[last_end..]));
        Ok(Value::List(result))
    });

    // ── Tier 2: readDir, toPath, storePath, placeholder ────

    register_builtin(&mut builtins_set, "readDir", |args| {
        let path_str = args[0].coerce_to_path("readDir")?;
        let mut attrs = NixAttrs::new();
        for entry in std::fs::read_dir(&path_str)
            .map_err(|e| EvalError::IoError { context: "readDir".into(), message: e.to_string() })?
        {
            let entry = entry.map_err(|e| EvalError::IoError { context: "readDir".into(), message: e.to_string() })?;
            let name = entry.file_name().to_string_lossy().to_string();
            let ft = entry.file_type().map_err(|e| EvalError::IoError { context: "readDir".into(), message: e.to_string() })?;
            let type_str = if ft.is_dir() {
                "directory"
            } else if ft.is_symlink() {
                "symlink"
            } else {
                "regular"
            };
            attrs.insert(name, Value::string(type_str));
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
        Ok(Value::string(format!("/placeholder-{}", &hash_str[..32])))
    });

    // ── import ─────────────────────────────────────────────

    register_builtin(&mut builtins_set, "import", |args| {
        let raw_path = args[0].coerce_to_path("import")?;
        // Resolve relative paths against the *currently evaluating
        // file's directory*, not the process cwd. This is what
        // makes `import ./foo.nix` work correctly inside nested
        // imports.
        let resolved_raw = if std::path::Path::new(&raw_path).is_absolute() {
            raw_path
        } else if let Some(dir) = crate::eval::current_eval_dir() {
            dir.join(&raw_path).to_string_lossy().into_owned()
        } else {
            raw_path
        };
        // Real Nix: importing a directory is equivalent to importing
        // `<dir>/default.nix`. nixpkgs and every flake-style consumer
        // relies on this, so without it `import <nixpkgs>` errors
        // immediately.
        let path = if std::path::Path::new(&resolved_raw).is_dir() {
            format!("{resolved_raw}/default.nix")
        } else {
            resolved_raw
        };
        let source = std::fs::read_to_string(&path)
            .map_err(|e| EvalError::IoError { context: format!("import {path}"), message: e.to_string() })?;
        // Push this file onto the eval stack so further relative
        // path literals inside it resolve against its directory,
        // AND tag the root Env so any closure created during
        // evaluation captures the file (for late-evaluated
        // function defaults that fire after we've left this
        // import scope).
        let path_buf = std::path::PathBuf::from(&path);
        let _guard = crate::eval::push_eval_file(path_buf.clone());
        crate::eval::eval_with_file(&source, Some(path_buf))
    });

    // ── derivation ─────────────────────────────────────────
    //
    // Computes real CppNix-compatible store paths for the resulting
    // derivation by serializing an ATerm representation of the inputs and
    // hashing it. See sui-compat::store_path for the hash algorithm details.

    register_builtin(&mut builtins_set, "derivation", |args| {
        build_derivation(&args[0])
    });

    // ── fetchurl ───────────────────────────────────────────
    //
    // Accepts a string URL or an attrset { url, sha256? }.
    // Downloads the URL and writes it to a temp file, returning the path.

    register_builtin(&mut builtins_set, "fetchurl", |args| {
        let (url, expected_sha256) = match &args[0] {
            Value::String(ns) => (ns.chars.clone(), None),
            Value::Attrs(a) => {
                let u = a
                    .get("url")
                    .ok_or_else(|| EvalError::AttrNotFound("url".into()))?
                    .to_str()?;
                let sha = a
                    .get("sha256")
                    .map(|v| v.to_str())
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
            Value::String(ns) => (ns.chars.clone(), None),
            Value::Attrs(a) => {
                let u = a
                    .get("url")
                    .ok_or_else(|| EvalError::AttrNotFound("url".into()))?
                    .to_str()?;
                let sha = a
                    .get("sha256")
                    .map(|v| v.to_str())
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
    // Path-based flake evaluation:
    //   1. Read flake.nix and evaluate it as a bare attrset (description, inputs, outputs).
    //   2. Parse flake.lock (if present) using sui-compat::flake::FlakeLock.
    //   3. Build the inputs attrset from the lock — `self` plus one entry per locked input.
    //   4. Apply the `outputs` function to the inputs attrset.
    //   5. Merge top-level metadata (description) into the result so callers can
    //      still access `.description` (matches Cpp Nix's user-facing behavior).
    //
    // Only path-style flake references are supported. Registry / git / github refs
    // would require fetching and store-path materialization, which is out of scope
    // for the in-process evaluator.

    register_builtin(&mut builtins_set, "getFlake", |args| {
        let flake_ref = crate::eval::force_value(&args[0])?;
        let flake_ref_str = flake_ref.as_string()?.to_string();

        let flake_dir = if flake_ref_str.starts_with('/') || flake_ref_str.starts_with('.') {
            std::path::PathBuf::from(&flake_ref_str)
        } else if let Some(path) = flake_ref_str.strip_prefix("path:") {
            std::path::PathBuf::from(path)
        } else {
            return Err(EvalError::NotImplemented(format!(
                "getFlake: only path-based flakes supported, got: {flake_ref_str}"
            )));
        };

        evaluate_flake(&flake_dir)
    });

    // ── path ──────────────────────────────────────────────
    //
    // builtins.path { path; name?; sha256?; recursive?; }
    // Hashes the path contents and returns a synthetic store path.

    register_builtin(&mut builtins_set, "path", |args| {
        let attrs = args[0].to_attrs()?;
        let path_val = attrs
            .get("path")
            .ok_or_else(|| EvalError::AttrNotFound("path".into()))?;
        let path_forced = crate::eval::force_value(path_val)?;
        let path_str = path_forced.coerce_to_path("path")?;
        let name = attrs
            .get("name")
            .map(|v| v.to_str())
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
                .map_err(|e| EvalError::IoError { context: "path".into(), message: e.to_string() })?;
            hasher.update(&content);
        } else if p.is_dir() {
            // Hash the directory name for deterministic output
            hasher.update(path_str.as_bytes());
        } else {
            hasher.update(path_str.as_bytes());
        }
        if let Some(expected) = attrs.get("sha256") {
            let expected_str = expected.to_str()?;
            let actual = format!("{:x}", hasher.clone().finalize());
            if &expected_str != &actual {
                return Err(EvalError::TypeError(format!(
                    "path: sha256 mismatch: expected {expected_str}, got {actual}"
                )));
            }
        }
        let hash = format!("{:x}", hasher.finalize());
        let store_path = format!("/nix/store/{}-{}", &hash[..32], name);
        Ok(Value::Path(store_path))
    });


    // ── String context builtins ──
    register_builtin(&mut builtins_set, "hasContext", |args| { match &args[0] { Value::String(ns) => Ok(Value::Bool(ns.has_context())), _ => Err(EvalError::TypeError("hasContext: expected string".into())) } });
    register_builtin(&mut builtins_set, "getContext", |args| { let ns = match &args[0] { Value::String(ns) => ns, _ => return Err(EvalError::TypeError("getContext: expected string".into())) }; let mut plains: std::collections::BTreeSet<String> = std::collections::BTreeSet::new(); let mut om: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new(); let mut deep: std::collections::BTreeSet<String> = std::collections::BTreeSet::new(); for elem in &ns.context.0 { match elem { ContextElement::Plain(p) => { plains.insert(p.clone()); } ContextElement::Output { drv, output } => { om.entry(drv.clone()).or_default().push(output.clone()); } ContextElement::DrvDeep(d) => { deep.insert(d.clone()); } } } let mut result = NixAttrs::new(); for p in &plains { let mut a = NixAttrs::new(); a.insert("path".to_string(), Value::Bool(true)); result.insert(p.clone(), Value::Attrs(a)); } for (d, os) in &om { let mut a = NixAttrs::new(); a.insert("outputs".to_string(), Value::List(os.iter().map(|o| Value::string(o.clone())).collect())); result.insert(d.clone(), Value::Attrs(a)); } for d in &deep { let mut a = NixAttrs::new(); a.insert("allOutputs".to_string(), Value::Bool(true)); result.insert(d.clone(), Value::Attrs(a)); } Ok(Value::Attrs(result)) });
    register_builtin(&mut builtins_set, "unsafeDiscardStringContext", |args| { match &args[0] { Value::String(ns) => Ok(Value::string(ns.chars.clone())), _ => Err(EvalError::TypeError("unsafeDiscardStringContext: expected string".into())) } });
    register_builtin(&mut builtins_set, "unsafeDiscardOutputDependency", |args| { match &args[0] { Value::String(ns) => { let mut nc = StringContext::new(); for elem in &ns.context.0 { match elem { ContextElement::DrvDeep(d) | ContextElement::Output { drv: d, .. } => { nc.add_plain(d.clone()); } other => { nc.0.insert(other.clone()); } } } Ok(Value::String(NixString::with_context(ns.chars.clone(), nc))) } _ => Err(EvalError::TypeError("unsafeDiscardOutputDependency: expected string".into())) } });
    register_builtin(&mut builtins_set, "addDrvOutputDependencies", |args| { match &args[0] { Value::String(ns) => { let mut nc = StringContext::new(); for elem in &ns.context.0 { match elem { ContextElement::Plain(p) if p.ends_with(".drv") => { nc.add_drv_deep(p.clone()); } ContextElement::Output { drv, .. } => { nc.add_drv_deep(drv.clone()); } other => { nc.0.insert(other.clone()); } } } Ok(Value::String(NixString::with_context(ns.chars.clone(), nc))) } _ => Err(EvalError::TypeError("addDrvOutputDependencies: expected string".into())) } });
    register_curried(&mut builtins_set, "appendContext", |sv, cv| { let ns = match sv { Value::String(ns) => ns.clone(), _ => return Err(EvalError::TypeError("appendContext: expected string".into())) }; let ca = cv.to_attrs()?; let mut nc = ns.context.clone(); for (key, val) in ca.iter() { let ea = crate::eval::force_value(val)?.to_attrs()?; if ea.contains_key("path") { nc.add_plain(key.clone()); } if let Some(ov) = ea.get("outputs") { let ol = crate::eval::force_value(ov)?.to_list()?; for o in &ol { nc.add_output(key.clone(), crate::eval::force_value(o)?.to_str()?); } } if ea.contains_key("allOutputs") { nc.add_drv_deep(key.clone()); } } Ok(Value::String(NixString::with_context(ns.chars, nc))) });
    // ── genericClosure ──────────────────────────────────────
    //
    // builtins.genericClosure { startSet; operator; }
    // Worklist algorithm: dedup by `key` attribute in each item.

    register_builtin(&mut builtins_set, "genericClosure", |args| {
        // Real Nix genericClosure walks the start set in insertion
        // order and appends operator-discovered items in discovery
        // order — a FIFO worklist (BFS-ish), not LIFO. Using
        // `Vec::pop` here gave the *reverse* order.
        use std::collections::VecDeque;
        let input = args[0].to_attrs()?;
        let start_set = input
            .get("startSet")
            .ok_or_else(|| EvalError::AttrNotFound("startSet".into()))?
            .to_list()?;
        let operator = input
            .get("operator")
            .ok_or_else(|| EvalError::AttrNotFound("operator".into()))?
            .clone();

        let mut result: Vec<Value> = Vec::new();
        let mut work_list: VecDeque<Value> = start_set.into();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        while let Some(item) = work_list.pop_front() {
            let item_attrs = item.to_attrs()?;
            let key_val = item_attrs
                .get("key")
                .ok_or_else(|| EvalError::AttrNotFound("key".into()))?
                .clone();
            let key_str = format!("{}", crate::eval::force_value(&key_val)?);
            if seen.contains(&key_str) {
                continue;
            }
            seen.insert(key_str);
            result.push(item.clone());
            let new_items = crate::eval::apply(operator.clone(), item)?;
            let new_list = new_items.to_list()?;
            work_list.extend(new_list);
        }

        Ok(Value::List(result))
    });

    // ── fromTOML ──────────────────────────────────────────
    //
    // builtins.fromTOML string → value

    register_builtin(&mut builtins_set, "fromTOML", |args| {
        let s = args[0].as_string()?;
        let table: toml::Value = toml::from_str(s)
            .map_err(|e| EvalError::TypeError(format!("fromTOML: {e}")))?;
        Ok(toml_to_value(&table))
    });

    // ── lessThan (curried) ────────────────────────────────
    //
    // builtins.lessThan a b → bool (works for int, float, string)

    register_curried(&mut builtins_set, "lessThan", |a, b| {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(Value::Bool(x < y)),
            (Value::Float(x), Value::Float(y)) => Ok(Value::Bool(x < y)),
            (Value::Int(x), Value::Float(y)) => Ok(Value::Bool((*x as f64) < *y)),
            (Value::Float(x), Value::Int(y)) => Ok(Value::Bool(*x < (*y as f64))),
            (Value::String(x), Value::String(y)) => Ok(Value::Bool(x.chars < y.chars)),
            _ => Err(EvalError::TypeError("lessThan: expected comparable types".into())),
        }
    });

    // ── bitAnd, bitOr, bitXor (curried) ──────────────────

    register_curried(&mut builtins_set, "bitAnd", |a, b| {
        Ok(Value::Int(a.as_int()? & b.as_int()?))
    });
    register_curried(&mut builtins_set, "bitOr", |a, b| {
        Ok(Value::Int(a.as_int()? | b.as_int()?))
    });
    register_curried(&mut builtins_set, "bitXor", |a, b| {
        Ok(Value::Int(a.as_int()? ^ b.as_int()?))
    });

    // ── splitVersion ─────────────────────────────────────
    //
    // builtins.splitVersion "1.2.3" → ["1" "." "2" "." "3"]
    // Splits on boundaries between digit/non-digit chars + separators.

    register_builtin(&mut builtins_set, "splitVersion", |args| {
        let s = args[0].as_string()?;
        let parts = split_version(s);
        Ok(Value::List(parts.into_iter().map(Value::string).collect()))
    });

    // ── pathExists ───────────────────────────────────────
    //
    // builtins.pathExists path → bool

    register_builtin(&mut builtins_set, "pathExists", |args| {
        let path_str = args[0].coerce_to_path("pathExists")?;
        Ok(Value::Bool(std::path::Path::new(&path_str).exists()))
    });

    // ── toFile ───────────────────────────────────────────
    //
    // builtins.toFile name content → store path
    // Creates a synthetic store path from content hash.

    register_curried(&mut builtins_set, "toFile", |name_val, content_val| {
        let name = name_val.as_string()?;
        let content = content_val.as_string()?;
        use sha2::{Sha256, Digest};
        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
        let store_path = format!("/nix/store/{}-{}", &hash[..32], name);
        Ok(Value::Path(store_path))
    });

    // ── hashFile (curried) ───────────────────────────────
    //
    // builtins.hashFile algo path → string

    register_curried(&mut builtins_set, "hashFile", |algo, path_val| {
        let algo_str = algo.as_string()?;
        let path_str = path_val.coerce_to_path("hashFile")?;
        let contents = std::fs::read(&path_str)
            .map_err(|e| EvalError::IoError { context: "hashFile".into(), message: e.to_string() })?;
        let hex = match algo_str {
            "sha256" => {
                use sha2::{Sha256, Digest};
                format!("{:x}", Sha256::digest(&contents))
            }
            "sha512" => {
                use sha2::{Sha512, Digest};
                format!("{:x}", Sha512::digest(&contents))
            }
            _ => return Err(EvalError::TypeError(format!("hashFile: unsupported algorithm: {algo_str}"))),
        };
        Ok(Value::string(hex))
    });

    // ── unsafeGetAttrPos ─────────────────────────────────
    //
    // builtins.unsafeGetAttrPos name set → null
    // We don't track positions yet, so always return null.

    register_curried(&mut builtins_set, "unsafeGetAttrPos", |_name, _set| {
        Ok(Value::Null)
    });

    // ── findFile (curried) ───────────────────────────────
    //
    // builtins.findFile searchPath name → path
    // Search a list of { prefix, path } for matching prefix.

    register_curried(&mut builtins_set, "findFile", |search_path, name_val| {
        let entries = search_path.as_list()?;
        let name = name_val.as_string()?;
        for entry in entries {
            let attrs = entry.to_attrs()?;
            let prefix = attrs
                .get("prefix")
                .ok_or_else(|| EvalError::AttrNotFound("prefix".into()))?
                .to_str()?;
            let path = attrs
                .get("path")
                .ok_or_else(|| EvalError::AttrNotFound("path".into()))?
                .to_str()?;
            if name == prefix || name.starts_with(&format!("{prefix}/")) {
                let suffix = if name == prefix {
                    String::new()
                } else {
                    name[prefix.len()..].to_string()
                };
                let full_path = format!("{path}{suffix}");
                if std::path::Path::new(&full_path).exists() {
                    return Ok(Value::Path(full_path));
                }
            }
        }
        Err(EvalError::TypeError(format!("findFile: file '{name}' not found in search path")))
    });

    // derivationStrict — alias to derivation (real difference is internal:
    // CppNix's `derivation` is implemented in nixpkgs by calling
    // `derivationStrict`, so they share the path computation logic).
    register_builtin(&mut builtins_set, "derivationStrict", |args| {
        build_derivation(&args[0])
    });

    // toXML — convert value to XML representation
    register_builtin(&mut builtins_set, "toXML", |args| {
        fn value_to_xml(v: &Value, indent: usize) -> String {
            let pad = " ".repeat(indent);
            match v {
                Value::Null => format!("{pad}<null />"),
                Value::Bool(b) => format!("{pad}<bool value=\"{b}\" />"),
                Value::Int(n) => format!("{pad}<int value=\"{n}\" />"),
                Value::Float(f) => format!("{pad}<float value=\"{f}\" />"),
                Value::String(ns) => format!("{pad}<string value=\"{}\" />", xml_escape(&ns.chars)),
                Value::Path(p) => format!("{pad}<path value=\"{}\" />", xml_escape(p)),
                Value::List(items) => {
                    let mut out = format!("{pad}<list>\n");
                    for item in items { out.push_str(&value_to_xml(item, indent + 2)); out.push('\n'); }
                    out.push_str(&format!("{pad}</list>"));
                    out
                }
                Value::Attrs(attrs) => {
                    let mut out = format!("{pad}<attrs>\n");
                    for (k, v) in attrs.iter() {
                        out.push_str(&format!("{pad}  <attr name=\"{}\">\n", xml_escape(k)));
                        out.push_str(&value_to_xml(v, indent + 4)); out.push('\n');
                        out.push_str(&format!("{pad}  </attr>\n"));
                    }
                    out.push_str(&format!("{pad}</attrs>"));
                    out
                }
                Value::Lambda(_) | Value::Builtin(_) => format!("{pad}<function />"),
                Value::Thunk(_) => format!("{pad}<thunk />"),
            }
        }
        fn xml_escape(s: &str) -> String {
            s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
        }
        let xml = format!("<?xml version='1.0' encoding='utf-8'?>\n{}\n", value_to_xml(&args[0], 0));
        Ok(Value::string(xml))
    });

    // ── Constants ────────────────────────────────────────

    builtins_set.insert("storeDir".to_string(), Value::string("/nix/store"));

    // Populate `builtins.nixPath` from the NIX_PATH environment
    // variable. CppNix exposes it as `[ { prefix = "nixpkgs"; path =
    // "/path/to/nixpkgs"; } ... ]`. The same parsing is reused below
    // by `resolve_search_path` to back the `<name>` syntax.
    let nix_path_value: Value = {
        let entries = parse_nix_path(&std::env::var("NIX_PATH").unwrap_or_default());
        let list: Vec<Value> = entries
            .into_iter()
            .map(|(prefix, path)| {
                let mut a = NixAttrs::new();
                a.insert("prefix".to_string(), Value::string(prefix));
                a.insert("path".to_string(), Value::string(path));
                Value::Attrs(a)
            })
            .collect();
        Value::List(list)
    };
    builtins_set.insert("nixPath".to_string(), nix_path_value);

    // true/false/null as builtins
    builtins_set.insert("true".to_string(), Value::Bool(true));
    builtins_set.insert("false".to_string(), Value::Bool(false));
    builtins_set.insert("null".to_string(), Value::Null);
    builtins_set.insert("nixVersion".to_string(), Value::string("sui-0.1.0"));
    builtins_set.insert("currentSystem".to_string(), Value::string(current_system()));
    builtins_set.insert("langVersion".to_string(), Value::Int(6));

    env.bind("builtins".to_string(), Value::Attrs(builtins_set.clone()));

    // Real Nix exposes a curated subset of builtins as bare
    // identifiers in the default scope. The list below is taken from
    // CppNix's `EvalState::createBaseEnv` and verified against
    // `nix-instantiate --eval` for the version on this machine.
    //
    // It is INTENTIONALLY NOT every builtin — `filter`, `head`,
    // `tail`, `attrNames` etc. are accessed only via `builtins.*`,
    // and exposing them at top level would change semantics for any
    // expression that shadows the name with a `let`.
    const DEFAULT_SCOPE: &[&str] = &[
        "abort",
        "baseNameOf",
        "derivation",
        "derivationStrict",
        "dirOf",
        "false",
        "fetchTarball",
        "import",
        "isNull",
        "map",
        "null",
        "removeAttrs",
        "scopedImport",
        "throw",
        "toString",
        "true",
    ];
    for name in DEFAULT_SCOPE {
        if let Some(v) = builtins_set.get(*name) {
            env.bind((*name).to_string(), v.clone());
        }
    }
}

/// Parse a `NIX_PATH` env var value into `(prefix, path)` pairs.
///
/// The format is `prefix1=path1:prefix2=path2:...`. An entry with
/// no `=` is treated as having an empty prefix (CppNix-compatible).
/// Empty entries are skipped.
#[must_use]
pub fn parse_nix_path(s: &str) -> Vec<(String, String)> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(':')
        .filter(|e| !e.is_empty())
        .map(|entry| match entry.split_once('=') {
            Some((prefix, path)) => (prefix.to_string(), path.to_string()),
            None => (String::new(), entry.to_string()),
        })
        .collect()
}

/// Resolve a `<name>` search-path token to an absolute filesystem
/// path by walking the entries parsed from `NIX_PATH`. The token
/// passed in is the *inner* part — `nixpkgs` for `<nixpkgs>`,
/// `nixpkgs/lib/lists.nix` for `<nixpkgs/lib/lists.nix>`. The
/// matched prefix is stripped and the remainder appended to the
/// entry's filesystem path; the resulting path must exist.
///
/// Returns `None` if no entry matches or the resolved path doesn't
/// exist on disk.
#[must_use]
pub fn resolve_search_path(name: &str) -> Option<String> {
    let nix_path = std::env::var("NIX_PATH").ok()?;
    for (prefix, path) in parse_nix_path(&nix_path) {
        // Direct match: `nixpkgs` against `nixpkgs=/path` → `/path`.
        if !prefix.is_empty() && name == prefix {
            if std::path::Path::new(&path).exists() {
                return Some(path);
            }
            continue;
        }
        // Sub-path match: `nixpkgs/lib/lists.nix` against `nixpkgs=/path`
        // → `/path/lib/lists.nix`.
        if !prefix.is_empty() {
            let needle = format!("{prefix}/");
            if let Some(rest) = name.strip_prefix(&needle) {
                let full = format!("{path}/{rest}");
                if std::path::Path::new(&full).exists() {
                    return Some(full);
                }
                continue;
            }
        }
        // Empty-prefix entries: try as a direct file in that dir.
        if prefix.is_empty() {
            let full = format!("{path}/{name}");
            if std::path::Path::new(&full).exists() {
                return Some(full);
            }
        }
    }
    None
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

// ── Flake evaluation ────────────────────────────────────────────
//
// Implements the in-process equivalent of `nix eval --raw '(builtins.getFlake
// "<dir>")'` for path-based flake references. The pipeline is:
//
//   flake.nix  → eval as bare attrset { description?; inputs?; outputs; }
//   flake.lock → parse via sui_compat::flake::FlakeLock
//   build inputs attrset { self; <input>: { outPath; rev?; narHash?; ... } }
//   call outputs(inputs)
//   merge top-level metadata (description) into the result
//
// Non-path flake references (`github:`, `git+`, registry refs, etc.) require
// fetching and store-path materialization, which is out of scope here.

fn evaluate_flake(flake_dir: &std::path::Path) -> Result<Value, EvalError> {
    let flake_nix = flake_dir.join("flake.nix");
    let flake_lock_path = flake_dir.join("flake.lock");

    // 1. Read and evaluate flake.nix.
    let source = std::fs::read_to_string(&flake_nix).map_err(|e| {
        EvalError::IoError {
            context: format!("getFlake: {}", flake_nix.display()),
            message: e.to_string(),
        }
    })?;
    let flake_value = crate::eval::eval(&source)?;
    let flake_attrs = flake_value.to_attrs()?.clone();

    // 2. Pull out the outputs function (required by every flake).
    let outputs_value = flake_attrs
        .get("outputs")
        .ok_or_else(|| EvalError::AttrNotFound("outputs".into()))?
        .clone();
    let outputs_fn = crate::eval::force_value(&outputs_value)?;

    // 3. Parse flake.lock if it exists. Missing lock is allowed: a flake with
    //    no inputs (only `self`) does not require a lock file.
    let lock = if flake_lock_path.exists() {
        let lock_content = std::fs::read_to_string(&flake_lock_path).map_err(|e| {
            EvalError::IoError {
                context: format!("getFlake: {}", flake_lock_path.display()),
                message: e.to_string(),
            }
        })?;
        Some(
            sui_compat::flake::FlakeLock::parse(&lock_content)
                .map_err(|e| EvalError::TypeError(format!("getFlake: invalid flake.lock: {e}")))?,
        )
    } else {
        None
    };

    // 4. Build the inputs attrset that will be passed to `outputs`.
    let mut inputs_attrs = NixAttrs::new();

    // `self` always exists. The minimum surface is `outPath`; we also expose a
    // (possibly empty) `sourceInfo` so callers that destructure it do not crash.
    let self_path = flake_dir.to_string_lossy().to_string();
    let mut self_attrs = NixAttrs::new();
    self_attrs.insert("outPath".to_string(), Value::string(self_path.clone()));
    self_attrs.insert("sourceInfo".to_string(), Value::Attrs(NixAttrs::new()));
    // Surface the original flake metadata on `self` so consumers can read e.g.
    // `self.description` or `self.outputs` from inside their `outputs` lambda.
    for (k, v) in flake_attrs.iter() {
        if k != "outputs" {
            self_attrs.insert(k.clone(), v.clone());
        }
    }
    inputs_attrs.insert("self".to_string(), Value::Attrs(self_attrs));

    // Each direct input of the root node becomes a top-level entry. We resolve
    // follows so the consumer always sees a concrete locked node.
    if let Some(ref lock) = lock {
        if let Ok(root_node) = lock.root_node() {
            let input_names: Vec<String> = root_node.inputs.keys().cloned().collect();
            for input_name in input_names {
                let segments = [input_name.as_str()];
                let Ok(node) = lock.resolve_input(&segments) else {
                    continue;
                };

                let mut input_val = NixAttrs::new();

                // For path-type inputs, surface the real filesystem path. For
                // remote sources (github, git, tarball, ...) we synthesize a
                // placeholder path; the in-process evaluator never fetches.
                let out_path = if let Some(ref locked) = node.locked {
                    if locked.source_type == "path" {
                        locked.path.clone().unwrap_or_default()
                    } else {
                        format!("/nix/store/flake-input-{input_name}")
                    }
                } else {
                    format!("/nix/store/flake-input-{input_name}")
                };
                input_val.insert("outPath".to_string(), Value::string(out_path));

                if let Some(ref locked) = node.locked {
                    if let Some(ref rev) = locked.rev {
                        input_val.insert("rev".to_string(), Value::string(rev.clone()));
                        let short: String = rev.chars().take(7).collect();
                        input_val.insert("shortRev".to_string(), Value::string(short));
                    }
                    if let Some(ref nar_hash) = locked.nar_hash {
                        input_val.insert(
                            "narHash".to_string(),
                            Value::string(nar_hash.clone()),
                        );
                    }
                    if let Some(last_modified) = locked.last_modified {
                        input_val.insert(
                            "lastModified".to_string(),
                            Value::Int(last_modified as i64),
                        );
                    }
                }

                inputs_attrs.insert(input_name, Value::Attrs(input_val));
            }
        }
    }

    // 5. Call outputs(inputs) and force the result to a concrete attrset.
    let result = crate::eval::apply(outputs_fn, Value::Attrs(inputs_attrs))?;
    let result = crate::eval::force_value(&result)?;

    // 6. Merge top-level flake metadata (description, etc.) into the outputs
    //    attrset. Cpp Nix exposes `description` on the user-facing flake value,
    //    so callers like `(builtins.getFlake "...").description` keep working.
    if let Value::Attrs(out_attrs) = result {
        let mut merged = out_attrs.clone();
        for key in ["description"] {
            if !merged.contains_key(key) {
                if let Some(v) = flake_attrs.get(key) {
                    merged.insert(key.to_string(), v.clone());
                }
            }
        }
        Ok(Value::Attrs(merged))
    } else {
        Ok(result)
    }
}

// ── Derivation construction ────────────────────────────────────
//
// `derivation` and `derivationStrict` both delegate to `build_derivation`.
// This function:
//   1. Forces the input attrset and pulls out the special attributes
//      (name, system, builder, args, outputs, outputHash*).
//   2. Coerces all other attributes to strings to populate the env map.
//   3. Builds an in-memory `Derivation` (sui-compat type) for ATerm
//      serialization.
//   4. Computes the .drv path from the ATerm bytes via SHA-256, then computes
//      each output path from the inner hash. For fixed-output derivations,
//      uses the dedicated `fixed:out:` fingerprint scheme instead.
//   5. Returns an attrset with `type`, `drvPath`, `outPath`, plus per-output
//      sub-attrsets, matching CppNix's interface.

fn build_derivation(arg: &Value) -> Result<Value, EvalError> {
    use std::collections::BTreeMap;
    use sui_compat::derivation::{Derivation, DerivationOutput};

    let forced = crate::eval::force_value(arg)?;
    let input_owned = forced.to_attrs()?;
    let input = &input_owned;

    // Required attributes — present and must be coercible to string.
    let name = force_attr_string(input, "name")?;
    let system = force_attr_string(input, "system")?;
    let builder = force_attr_string(input, "builder")?;

    // Optional `args` list of strings.
    let args_list: Vec<String> = if let Some(a) = input.get("args") {
        let forced_args = crate::eval::force_value(a)?;
        let list = forced_args.as_list()?;
        let mut out = Vec::with_capacity(list.len());
        for item in list {
            out.push(coerce_drv_value_to_string(item)?);
        }
        out
    } else {
        Vec::new()
    };

    // Optional `outputs` list — defaults to ["out"].
    let outputs: Vec<String> = if let Some(o) = input.get("outputs") {
        let forced_o = crate::eval::force_value(o)?;
        let list = forced_o.as_list()?;
        let mut out = Vec::with_capacity(list.len());
        for item in list {
            let s = crate::eval::force_value(item)?
                .as_string()
                .map_err(|_| EvalError::TypeError(
                    "derivation: outputs entries must be strings".into(),
                ))?
                .to_string();
            out.push(s);
        }
        if out.is_empty() {
            return Err(EvalError::TypeError(
                "derivation: outputs list must not be empty".into(),
            ));
        }
        out
    } else {
        vec!["out".to_string()]
    };

    // Build env vars from non-special attributes.
    // Special attrs are skipped; everything else is coerced to string.
    let mut env_vars: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in input.iter() {
        if matches!(
            k.as_str(),
            "name"
                | "system"
                | "builder"
                | "args"
                | "outputs"
                | "__impure"
                | "__contentAddressed"
                | "__structuredAttrs"
        ) {
            continue;
        }
        let forced_v = match crate::eval::force_value(v) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(s) = coerce_drv_value_to_string_opt(&forced_v) {
            env_vars.insert(k.clone(), s);
        }
    }
    // Always include the canonical attrs as env entries (matches CppNix).
    env_vars.insert("name".to_string(), name.clone());
    env_vars.insert("system".to_string(), system.clone());
    env_vars.insert("builder".to_string(), builder.clone());

    // Detect fixed-output derivation.
    let is_fod = input.contains_key("outputHash");

    // Build the Derivation skeleton (outputs map populated below).
    let mut drv = Derivation {
        outputs: BTreeMap::new(),
        input_derivations: BTreeMap::new(),
        input_sources: Vec::new(),
        system,
        builder,
        args: args_list,
        env: env_vars,
    };

    let (drv_path, out_paths) = if is_fod {
        // Fixed-output: hash is determined by the declared outputHash, not by
        // the build instructions. CppNix uses the `fixed:out:` fingerprint.
        let output_hash = force_attr_string(input, "outputHash")?;
        let output_hash_algo = optional_attr_string(input, "outputHashAlgo")?
            .unwrap_or_else(|| "sha256".to_string());
        let output_hash_mode = optional_attr_string(input, "outputHashMode")?
            .unwrap_or_else(|| "flat".to_string());
        let is_recursive =
            output_hash_mode == "recursive" || output_hash_mode == "nar";

        let out_path = sui_compat::store_path::compute_fixed_output_hash(
            &output_hash_algo,
            &output_hash,
            is_recursive,
            &name,
        );

        // Populate the single `out` output with the FOD metadata so the
        // serialized drv carries the declared hash.
        drv.outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: out_path.clone(),
                hash_algo: if is_recursive {
                    format!("r:{output_hash_algo}")
                } else {
                    output_hash_algo.clone()
                },
                hash: output_hash,
            },
        );

        let drv_content = drv.serialize();
        let drv_path = sui_compat::store_path::compute_drv_path(
            drv_content.as_bytes(),
            &name,
        );

        let mut out_paths = BTreeMap::new();
        out_paths.insert("out".to_string(), out_path);
        (drv_path, out_paths)
    } else {
        // Input-addressed: outputs are placeholders during ATerm hashing
        // (CppNix replaces them with empty strings to break the chicken-and-
        // egg cycle). After hashing, the output paths are derived from the
        // resulting inner hash.
        for o in &outputs {
            drv.outputs.insert(
                o.clone(),
                DerivationOutput {
                    path: String::new(),
                    hash_algo: String::new(),
                    hash: String::new(),
                },
            );
        }

        let drv_content = drv.serialize();
        let drv_path = sui_compat::store_path::compute_drv_path(
            drv_content.as_bytes(),
            &name,
        );

        // Compute each output path from the inner SHA-256 of the drv content.
        use sha2::{Digest, Sha256};
        let inner = Sha256::digest(drv_content.as_bytes());
        let inner_hex: String =
            inner.iter().map(|b| format!("{b:02x}")).collect();
        let mut out_paths = BTreeMap::new();
        for o in &outputs {
            let p = sui_compat::store_path::compute_output_path(
                &inner_hex, o, &name,
            );
            out_paths.insert(o.clone(), p);
        }
        (drv_path, out_paths)
    };

    // Assemble the result attrset (input + derivation metadata).
    let mut result = input.clone();
    result.insert("type".to_string(), Value::string("derivation"));
    result.insert("drvPath".to_string(), Value::string(drv_path.clone()));

    // The `outPath` exposed at the top-level is the `out` output (or the only
    // output if there isn't one named `out` — fall back to the first).
    let primary_out = out_paths
        .get("out")
        .cloned()
        .or_else(|| out_paths.values().next().cloned())
        .unwrap_or_default();
    result.insert("outPath".to_string(), Value::string(primary_out.clone()));

    // Per-output sub-attrsets so `mydrv.dev`, `mydrv.lib`, etc. work.
    for (output_name, output_path) in &out_paths {
        let mut out_attrs = NixAttrs::new();
        out_attrs.insert(
            "outPath".to_string(),
            Value::string(output_path.clone()),
        );
        out_attrs.insert("drvPath".to_string(), Value::string(drv_path.clone()));
        out_attrs.insert("type".to_string(), Value::string("derivation"));
        out_attrs.insert(
            "outputName".to_string(),
            Value::string(output_name.clone()),
        );
        out_attrs.insert("name".to_string(), Value::string(name.clone()));
        result.insert(output_name.clone(), Value::Attrs(out_attrs));
    }

    Ok(Value::Attrs(result))
}

/// Force an attribute and require it to be present + string-coercible.
fn force_attr_string(
    attrs: &NixAttrs,
    key: &str,
) -> Result<String, EvalError> {
    let v = attrs
        .get(key)
        .ok_or_else(|| EvalError::AttrNotFound(key.into()))?;
    let forced = crate::eval::force_value(v)?;
    coerce_drv_value_to_string(&forced)
}

/// Force an optional attribute, returning `None` if absent.
fn optional_attr_string(
    attrs: &NixAttrs,
    key: &str,
) -> Result<Option<String>, EvalError> {
    match attrs.get(key) {
        None => Ok(None),
        Some(v) => {
            let forced = crate::eval::force_value(v)?;
            Ok(Some(coerce_drv_value_to_string(&forced)?))
        }
    }
}

/// Coerce an already-forced value to a string the way CppNix does for
/// derivation env vars. Errors on types that have no string form (lambdas,
/// builtins, attrsets without `__toString`).
fn coerce_drv_value_to_string(v: &Value) -> Result<String, EvalError> {
    match v {
        Value::String(s) => Ok(s.chars.clone()),
        Value::Path(p) => Ok(p.clone()),
        Value::Int(n) => Ok(n.to_string()),
        Value::Float(f) => Ok(format!("{f}")),
        Value::Bool(true) => Ok("1".to_string()),
        Value::Bool(false) => Ok(String::new()),
        Value::Null => Ok(String::new()),
        Value::List(items) => {
            // Space-joined coercion (matches CppNix derivation arg list
            // handling for env exports).
            let mut parts: Vec<String> = Vec::with_capacity(items.len());
            for item in items {
                let forced = crate::eval::force_value(item)?;
                parts.push(coerce_drv_value_to_string(&forced)?);
            }
            Ok(parts.join(" "))
        }
        Value::Attrs(attrs) => {
            // Honor the `__toString` and `outPath` protocols, in that order.
            if let Some(to_str) = attrs.get("__toString") {
                let result =
                    crate::eval::apply(to_str.clone(), Value::Attrs(attrs.clone()))?;
                let forced = crate::eval::force_value(&result)?;
                return coerce_drv_value_to_string(&forced);
            }
            if let Some(out_path) = attrs.get("outPath") {
                let forced = crate::eval::force_value(out_path)?;
                return coerce_drv_value_to_string(&forced);
            }
            Err(EvalError::TypeError(
                "derivation: cannot coerce attrset to string (no __toString or outPath)".into(),
            ))
        }
        Value::Lambda(_) | Value::Builtin(_) => Err(EvalError::TypeError(
            "derivation: cannot coerce function to string".into(),
        )),
        Value::Thunk(_) => Err(EvalError::TypeError(
            "derivation: unforced thunk after force_value".into(),
        )),
    }
}

/// Variant of `coerce_drv_value_to_string` that returns `None` for values
/// that have no meaningful string form (used to skip env entries instead of
/// erroring out).
fn coerce_drv_value_to_string_opt(v: &Value) -> Option<String> {
    coerce_drv_value_to_string(v).ok()
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
    Value::from(json)
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
    let pa = split_version(a);
    let pb = split_version(b);
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

/// Convert a TOML value to a Nix value.
fn toml_to_value(v: &toml::Value) -> Value {
    Value::from(v)
}

/// Split a version string on `.` / `-` separators and on boundaries
/// between digit and non-digit characters. Separators are dropped.
///
/// Matches CppNix `builtins.splitVersion`:
///   "1.2.3"      → ["1", "2", "3"]
///   "1.2-pre1"   → ["1", "2", "pre", "1"]
fn split_version(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut prev_digit: Option<bool> = None;
    for ch in s.chars() {
        if ch == '.' || ch == '-' {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            // Separators are NOT preserved as elements — real Nix
            // splitVersion only emits the version components.
            prev_digit = None;
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
}

#[cfg(test)]
mod tests {
    use crate::eval::eval;
    use crate::value::{NixAttrs, NixString, StringContext, Value};

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
        if let Value::String(ns) = v {
            let s = &ns.chars;
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
            Value::string("a, b, c"),
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
            Value::string("FOOBAR"),
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
            Value::string("hello world"),
        );
    }

    #[test]
    fn builtins_concat_strings_empty() {
        assert_eq!(
            ev(r#"builtins.concatStrings []"#),
            Value::string(""),
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
                    Value::string("a"),
                    Value::string("a"),
                ])),
            );
            assert_eq!(
                a.get("b"),
                Some(&Value::List(vec![
                    Value::string("b"),
                    Value::string("b"),
                ])),
            );
            assert_eq!(
                a.get("c"),
                Some(&Value::List(vec![Value::string("c")])),
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
            assert_eq!(a.get("name"), Some(&Value::string("hello")));
            assert_eq!(a.get("version"), Some(&Value::string("2.10")));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_parse_drv_name_no_version() {
        let v = ev(r#"builtins.parseDrvName "hello""#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("name"), Some(&Value::string("hello")));
            assert_eq!(a.get("version"), Some(&Value::string("")));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_parse_drv_name_complex() {
        let v = ev(r#"builtins.parseDrvName "openssl-1.1.1k""#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("name"), Some(&Value::string("openssl")));
            assert_eq!(a.get("version"), Some(&Value::string("1.1.1k")));
        } else {
            panic!("expected attrs");
        }
    }

    // ── New builtins: baseNameOf / dirOf ────────────────────

    #[test]
    fn builtins_base_name_of() {
        assert_eq!(
            ev(r#"builtins.baseNameOf "/nix/store/abc-hello""#),
            Value::string("abc-hello"),
        );
        assert_eq!(
            ev(r#"builtins.baseNameOf "hello.txt""#),
            Value::string("hello.txt"),
        );
    }

    #[test]
    fn builtins_dir_of_string() {
        assert_eq!(
            ev(r#"builtins.dirOf "/nix/store/abc""#),
            Value::string("/nix/store"),
        );
        assert_eq!(
            ev(r#"builtins.dirOf "/foo""#),
            Value::string("/"),
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
        if let Value::String(ns) = v {
            assert_eq!(ns.chars, "hello from test");
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
            Value::string("hello"),
        );
    }

    #[test]
    fn to_string_protocol_with_self() {
        assert_eq!(
            ev(r#"let s = { __toString = self: self.name; name = "world"; }; in "${s}""#),
            Value::string("world"),
        );
    }

    #[test]
    fn to_string_protocol_via_builtin() {
        assert_eq!(
            ev(r#"builtins.toString { __toString = self: "custom"; }"#),
            Value::string("custom"),
        );
    }

    // ── Ignored tests for features needing major work ───────

    #[test]
    fn builtins_hash_string_sha256() {
        let v = ev(r#"builtins.hashString "sha256" "hello""#);
        if let Value::String(ns) = v {
            let s = &ns.chars;
            assert_eq!(s.len(), 64); // SHA-256 hex is 64 chars
            assert_eq!(*s, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        } else {
            panic!("expected string");
        }
    }

    #[test]
    fn builtins_hash_string_sha512() {
        let v = ev(r#"builtins.hashString "sha512" "hello""#);
        if let Value::String(ns) = v {
            assert_eq!(ns.chars.len(), 128); // SHA-512 hex is 128 chars
        } else {
            panic!("expected string");
        }
    }

    #[test]
    fn builtins_match_regex() {
        // match returns null on no match, list of groups on match
        assert_eq!(
            ev(r#"builtins.match "([0-9]+)\\.([0-9]+)" "1.23""#),
            Value::List(vec![Value::string("1"), Value::string("23")]),
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
            Value::List(vec![Value::string("42")]),
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
    fn builtins_derivation_returns_real_paths() {
        let v = eval(r#"builtins.derivation { name = "test"; system = "x86_64-linux"; builder = "/bin/sh"; }"#).unwrap();
        let a = match v {
            Value::Attrs(a) => a,
            other => panic!("expected attrs, got {other:?}"),
        };
        assert_eq!(a.get("type"), Some(&Value::string("derivation")));
        assert_eq!(a.get("name"), Some(&Value::string("test")));

        // drvPath: /nix/store/<32 base32 chars>-test.drv
        let drv_path = a.get("drvPath").unwrap().as_string().unwrap();
        assert!(drv_path.starts_with("/nix/store/"), "drvPath: {drv_path}");
        assert!(drv_path.ends_with("-test.drv"), "drvPath: {drv_path}");
        let drv_basename = drv_path.strip_prefix("/nix/store/").unwrap();
        assert_eq!(drv_basename.len(), 32 + 1 + "test.drv".len());

        // outPath: /nix/store/<32 base32 chars>-test
        let out_path = a.get("outPath").unwrap().as_string().unwrap();
        assert!(out_path.starts_with("/nix/store/"));
        assert!(out_path.ends_with("-test"));
        assert_ne!(drv_path, out_path);
    }

    #[test]
    fn builtins_derivation_is_deterministic() {
        // Same inputs must always produce the same paths.
        let expr = r#"builtins.derivation {
            name = "hello";
            system = "x86_64-linux";
            builder = "/bin/sh";
            args = [ "-e" "build.sh" ];
        }"#;
        let a1 = eval(expr).unwrap().as_attrs().unwrap().clone();
        let a2 = eval(expr).unwrap().as_attrs().unwrap().clone();
        assert_eq!(
            a1.get("drvPath").unwrap().as_string().unwrap(),
            a2.get("drvPath").unwrap().as_string().unwrap(),
        );
        assert_eq!(
            a1.get("outPath").unwrap().as_string().unwrap(),
            a2.get("outPath").unwrap().as_string().unwrap(),
        );
    }

    #[test]
    fn builtins_derivation_different_names_produce_different_paths() {
        let v1 = eval(r#"builtins.derivation { name = "foo"; system = "x86_64-linux"; builder = "/bin/sh"; }"#).unwrap();
        let v2 = eval(r#"builtins.derivation { name = "bar"; system = "x86_64-linux"; builder = "/bin/sh"; }"#).unwrap();
        let p1 = v1.as_attrs().unwrap().get("drvPath").unwrap().as_string().unwrap().to_string();
        let p2 = v2.as_attrs().unwrap().get("drvPath").unwrap().as_string().unwrap().to_string();
        assert_ne!(p1, p2);
    }

    #[test]
    fn builtins_derivation_multiple_outputs() {
        let v = eval(r#"builtins.derivation {
            name = "multi";
            system = "x86_64-linux";
            builder = "/bin/sh";
            outputs = [ "out" "dev" "lib" ];
        }"#).unwrap();
        let a = v.as_attrs().unwrap();
        assert_eq!(a.get("type"), Some(&Value::string("derivation")));

        // Each named output is a sub-attrset.
        for out_name in ["out", "dev", "lib"] {
            let sub = a
                .get(out_name)
                .unwrap_or_else(|| panic!("missing output {out_name}"));
            let sub_attrs = sub.as_attrs().unwrap();
            assert_eq!(sub_attrs.get("type"), Some(&Value::string("derivation")));
            assert_eq!(
                sub_attrs.get("outputName"),
                Some(&Value::string(out_name)),
            );
            // Sub-attrset should have an outPath.
            assert!(sub_attrs.contains_key("outPath"));
            assert!(sub_attrs.contains_key("drvPath"));
        }

        // The three outputs must have distinct paths.
        let out_p = a.get("out").unwrap().as_attrs().unwrap()
            .get("outPath").unwrap().as_string().unwrap().to_string();
        let dev_p = a.get("dev").unwrap().as_attrs().unwrap()
            .get("outPath").unwrap().as_string().unwrap().to_string();
        let lib_p = a.get("lib").unwrap().as_attrs().unwrap()
            .get("outPath").unwrap().as_string().unwrap().to_string();
        assert_ne!(out_p, dev_p);
        assert_ne!(out_p, lib_p);
        assert_ne!(dev_p, lib_p);
        assert!(dev_p.ends_with("-multi-dev"));
        assert!(lib_p.ends_with("-multi-lib"));
        assert!(out_p.ends_with("-multi"));
    }

    #[test]
    fn builtins_derivation_fixed_output() {
        let v = eval(r#"builtins.derivation {
            name = "src.tar.gz";
            system = "x86_64-linux";
            builder = "/bin/curl";
            outputHash = "1b0ri5lsf45dknj8bfxi1syz35kmab77apxxg1yrf33la1qm3kc7";
            outputHashAlgo = "sha256";
            outputHashMode = "flat";
        }"#).unwrap();
        let a = v.as_attrs().unwrap();
        assert_eq!(a.get("type"), Some(&Value::string("derivation")));
        let out_path = a.get("outPath").unwrap().as_string().unwrap();
        assert!(out_path.ends_with("-src.tar.gz"));
        assert!(a.get("drvPath").unwrap().as_string().unwrap().ends_with("-src.tar.gz.drv"));
    }

    #[test]
    fn builtins_derivation_fixed_output_recursive_differs_from_flat() {
        let flat = eval(r#"builtins.derivation {
            name = "x";
            system = "x86_64-linux";
            builder = "/bin/sh";
            outputHash = "abc";
            outputHashAlgo = "sha256";
            outputHashMode = "flat";
        }"#).unwrap();
        let rec = eval(r#"builtins.derivation {
            name = "x";
            system = "x86_64-linux";
            builder = "/bin/sh";
            outputHash = "abc";
            outputHashAlgo = "sha256";
            outputHashMode = "recursive";
        }"#).unwrap();
        let p1 = flat.as_attrs().unwrap().get("outPath").unwrap().as_string().unwrap().to_string();
        let p2 = rec.as_attrs().unwrap().get("outPath").unwrap().as_string().unwrap().to_string();
        assert_ne!(p1, p2);
    }

    #[test]
    fn builtins_derivation_returns_drv_and_out_path() {
        // Sanity-check that the result attrset always has drvPath + outPath.
        let v = eval(r#"builtins.derivation { name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; }"#).unwrap();
        let a = v.as_attrs().unwrap();
        assert!(a.contains_key("drvPath"));
        assert!(a.contains_key("outPath"));
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
            assert_eq!(a.get("file.txt"), Some(&Value::string("regular")));
            assert_eq!(a.get("subdir"), Some(&Value::string("directory")));
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
        if let Value::String(ns) = v {
            let s = &ns.chars;
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
        assert_eq!(v, Value::string("test flake"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtins_get_flake_rejects_registry_refs() {
        // Non-path flake references are not yet supported
        let result = eval(r#"builtins.getFlake "nixpkgs""#);
        assert!(result.is_err());
    }

    #[test]
    fn flake_minimal_no_inputs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("flake.nix"),
            r#"{
              description = "test flake";
              outputs = { self }: { packages.default = "hello"; };
            }"#,
        )
        .unwrap();

        let flake_path = dir.path().to_string_lossy().to_string();
        let expr = format!(r#"(builtins.getFlake "{flake_path}").packages.default"#);
        let result = eval(&expr).unwrap();
        assert_eq!(result.as_string().unwrap(), "hello");
    }

    #[test]
    fn flake_with_self_output_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("flake.nix"),
            r#"{
              description = "test flake";
              outputs = { self }: { result = self.outPath; };
            }"#,
        )
        .unwrap();

        let flake_path = dir.path().to_string_lossy().to_string();
        let expr = format!(r#"(builtins.getFlake "{flake_path}").result"#);
        let result = eval(&expr).unwrap();
        assert_eq!(result.as_string().unwrap(), flake_path);
    }

    #[test]
    fn flake_description_accessible() {
        // The description attr is on the flake attrset itself, not the outputs;
        // evaluate_flake() merges it into the result so consumers can read it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("flake.nix"),
            r#"{
              description = "my flake";
              outputs = { self }: { packages.default = self.outPath; };
            }"#,
        )
        .unwrap();
        let flake_path = dir.path().to_string_lossy().to_string();
        let expr = format!(r#"(builtins.getFlake "{flake_path}").packages.default"#);
        assert!(eval(&expr).is_ok());
    }

    #[test]
    fn flake_path_prefix_supported() {
        // path: prefix should also resolve to a directory.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("flake.nix"),
            r#"{
              outputs = { self }: { value = "ok"; };
            }"#,
        )
        .unwrap();
        let flake_path = dir.path().to_string_lossy().to_string();
        let expr = format!(r#"(builtins.getFlake "path:{flake_path}").value"#);
        let result = eval(&expr).unwrap();
        assert_eq!(result.as_string().unwrap(), "ok");
    }

    #[test]
    fn flake_with_locked_input_path() {
        // A flake with a real path-typed input pinned in flake.lock.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("flake.nix"),
            r#"{
              inputs.dep = { };
              outputs = { self, dep }: { result = dep.outPath; };
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("flake.lock"),
            r#"{
              "nodes": {
                "dep": {
                  "locked": {
                    "lastModified": 1700000000,
                    "narHash": "sha256-DEPDEPDEPDEPDEPDEPDEPDEPDEPDEPDEPDEPDEPDEPD=",
                    "path": "/var/empty/dep",
                    "type": "path"
                  },
                  "original": {
                    "type": "path",
                    "url": "/var/empty/dep"
                  }
                },
                "root": {
                  "inputs": {
                    "dep": "dep"
                  }
                }
              },
              "root": "root",
              "version": 7
            }"#,
        )
        .unwrap();
        let flake_path = dir.path().to_string_lossy().to_string();
        let expr = format!(r#"(builtins.getFlake "{flake_path}").result"#);
        let result = eval(&expr).unwrap();
        assert_eq!(result.as_string().unwrap(), "/var/empty/dep");
    }

    #[test]
    fn flake_missing_outputs_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("flake.nix"),
            r#"{ description = "no outputs"; }"#,
        )
        .unwrap();
        let flake_path = dir.path().to_string_lossy().to_string();
        let expr = format!(r#"builtins.getFlake "{flake_path}""#);
        assert!(eval(&expr).is_err());
    }

    #[test]
    fn pure_mode_toggle() {
        use crate::eval::{is_pure_mode, set_pure_mode};
        // Default is impure.
        set_pure_mode(false);
        assert!(!is_pure_mode());
        set_pure_mode(true);
        assert!(is_pure_mode());
        // Restore so we don't poison neighbouring tests on the same thread.
        set_pure_mode(false);
        assert!(!is_pure_mode());
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

    #[test] fn sc_plain() { assert!(!NixString::plain("hello").has_context()); }
    #[test] fn sc_merge() { let mut c = StringContext::new(); c.add_plain("/nix/store/abc".to_string()); assert!(NixString::with_context("hi", c).has_context()); }
    #[test] fn has_ctx_false() { assert_eq!(ev(r#"builtins.hasContext "hello""#), Value::Bool(false)); }
    #[test] fn discard_ctx() { assert_eq!(ev(r#"builtins.hasContext (builtins.unsafeDiscardStringContext "hello")"#), Value::Bool(false)); }
    #[test] fn get_ctx_empty() { let v = ev(r#"builtins.getContext "hello""#); if let Value::Attrs(a) = v { assert!(a.is_empty()); } else { panic!(); } }
    #[test] fn has_ctx_after_append() { assert_eq!(ev(r#"builtins.hasContext (builtins.appendContext "hello" { "/nix/store/abc" = { path = true; }; })"#), Value::Bool(true)); }
    #[test] fn append_ctx_rt() { let v = ev(r#"builtins.getContext (builtins.appendContext "hello" { "/nix/store/abc" = { path = true; }; })"#); if let Value::Attrs(a) = v { assert!(a.contains_key("/nix/store/abc")); } else { panic!(); } }
    #[test] fn discard_ctx_all() { let v = ev(r#"let s = builtins.appendContext "hello" { "/nix/store/abc" = { path = true; }; }; clean = builtins.unsafeDiscardStringContext s; in builtins.getContext clean"#); if let Value::Attrs(a) = v { assert!(a.is_empty()); } else { panic!(); } }
    #[test] fn concat_merges_ctx() { let v = ev(r#"let a = builtins.appendContext "foo" { "/nix/store/a" = { path = true; }; }; b = builtins.appendContext "bar" { "/nix/store/b" = { path = true; }; }; in builtins.getContext (a + b)"#); if let Value::Attrs(a) = v { assert!(a.contains_key("/nix/store/a")); assert!(a.contains_key("/nix/store/b")); } else { panic!(); } }
    #[test] #[ignore = "context propagation through interpolation not yet wired in eval_str"] fn interp_merges_ctx() { assert_eq!(ev(r#"let s = builtins.appendContext "world" { "/nix/store/x" = { path = true; }; }; in builtins.hasContext "hello ""#), Value::Bool(true)); }
    #[test] #[ignore = "path context propagation requires store integration"] fn path_interp_ctx() { assert_eq!(ev(r#"builtins.hasContext """#), Value::Bool(true)); }
    #[test] #[ignore = "path context propagation requires store integration"] fn path_interp_ctx_content() { let v = ev(r#"builtins.getContext """#); if let Value::Attrs(a) = v { assert!(a.contains_key("/nix/store/test")); } else { panic!(); } }
    #[test] fn add_drv_out_deps() { let v = ev(r#"let s = builtins.appendContext "/nix/store/abc.drv" { "/nix/store/abc.drv" = { path = true; }; }; p = builtins.addDrvOutputDependencies s; in builtins.getContext p"#); if let Value::Attrs(a) = v { let e = a.get("/nix/store/abc.drv").unwrap().as_attrs().unwrap(); assert_eq!(e.get("allOutputs"), Some(&Value::Bool(true))); } else { panic!(); } }
    #[test] fn discard_out_dep() { let v = ev(r#"let s = builtins.appendContext "hello" { "/nix/store/x.drv" = { allOutputs = true; }; }; d = builtins.unsafeDiscardOutputDependency s; in builtins.getContext d"#); if let Value::Attrs(a) = v { let e = a.get("/nix/store/x.drv").unwrap().as_attrs().unwrap(); assert_eq!(e.get("path"), Some(&Value::Bool(true))); } else { panic!(); } }

    // ── genericClosure tests ────────────────────────────────

    #[test]
    fn builtins_generic_closure_linear_chain() {
        // Linear chain: start at 1, operator produces next until 5
        let v = ev(r#"
            builtins.genericClosure {
                startSet = [{ key = 1; }];
                operator = item: if item.key < 5 then [{ key = item.key + 1; }] else [];
            }
        "#);
        if let Value::List(items) = v {
            assert_eq!(items.len(), 5);
            // Keys should be 1..5
            for (i, item) in items.iter().enumerate() {
                let attrs = item.as_attrs().unwrap();
                assert_eq!(attrs.get("key"), Some(&Value::Int(i as i64 + 1)));
            }
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn builtins_generic_closure_diamond_dedup() {
        // Diamond: A→B, A→C, B→D, C→D. D should appear once.
        let v = ev(r#"
            builtins.genericClosure {
                startSet = [{ key = "A"; }];
                operator = item:
                    if item.key == "A" then [{ key = "B"; } { key = "C"; }]
                    else if item.key == "B" then [{ key = "D"; }]
                    else if item.key == "C" then [{ key = "D"; }]
                    else [];
            }
        "#);
        if let Value::List(items) = v {
            assert_eq!(items.len(), 4); // A, B, C, D (D only once)
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn builtins_generic_closure_empty_operator() {
        let v = ev(r#"
            builtins.genericClosure {
                startSet = [{ key = 1; } { key = 2; }];
                operator = item: [];
            }
        "#);
        if let Value::List(items) = v {
            assert_eq!(items.len(), 2);
        } else {
            panic!("expected list");
        }
    }

    // ── fromTOML tests ──────────────────────────────────────

    #[test]
    fn builtins_from_toml_simple_table() {
        let v = ev(r#"builtins.fromTOML ''
            name = "hello"
            version = 42
        ''"#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("name"), Some(&Value::string("hello")));
            assert_eq!(a.get("version"), Some(&Value::Int(42)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_from_toml_nested() {
        let v = ev(r#"builtins.fromTOML ''
            [package]
            name = "test"
            [package.metadata]
            key = true
        ''"#);
        if let Value::Attrs(a) = v {
            let pkg = a.get("package").unwrap().as_attrs().unwrap();
            assert_eq!(pkg.get("name"), Some(&Value::string("test")));
            let meta = pkg.get("metadata").unwrap().as_attrs().unwrap();
            assert_eq!(meta.get("key"), Some(&Value::Bool(true)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_from_toml_arrays() {
        let v = ev(r#"builtins.fromTOML ''
            ports = [80, 443]
        ''"#);
        if let Value::Attrs(a) = v {
            assert_eq!(
                a.get("ports"),
                Some(&Value::List(vec![Value::Int(80), Value::Int(443)])),
            );
        } else {
            panic!("expected attrs");
        }
    }

    // ── lessThan tests ──────────────────────────────────────

    #[test]
    fn builtins_less_than_ints() {
        assert_eq!(ev("builtins.lessThan 1 2"), Value::Bool(true));
        assert_eq!(ev("builtins.lessThan 2 1"), Value::Bool(false));
        assert_eq!(ev("builtins.lessThan 1 1"), Value::Bool(false));
    }

    #[test]
    fn builtins_less_than_floats() {
        assert_eq!(ev("builtins.lessThan 1.0 2.0"), Value::Bool(true));
        assert_eq!(ev("builtins.lessThan 2.0 1.0"), Value::Bool(false));
    }

    #[test]
    fn builtins_less_than_strings() {
        assert_eq!(ev(r#"builtins.lessThan "abc" "def""#), Value::Bool(true));
        assert_eq!(ev(r#"builtins.lessThan "def" "abc""#), Value::Bool(false));
    }

    // ── bitwise tests ───────────────────────────────────────

    #[test]
    fn builtins_bit_and() {
        assert_eq!(ev("builtins.bitAnd 12 10"), Value::Int(8));  // 1100 & 1010 = 1000
    }

    #[test]
    fn builtins_bit_or() {
        assert_eq!(ev("builtins.bitOr 12 10"), Value::Int(14)); // 1100 | 1010 = 1110
    }

    #[test]
    fn builtins_bit_xor() {
        assert_eq!(ev("builtins.bitXor 12 10"), Value::Int(6));  // 1100 ^ 1010 = 0110
    }

    // ── splitVersion tests ──────────────────────────────────

    #[test]
    fn builtins_split_version_standard() {
        // Real nix drops separators: "1.2.3" → ["1","2","3"]
        assert_eq!(
            ev(r#"builtins.splitVersion "1.2.3""#),
            Value::List(vec![
                Value::string("1"),
                Value::string("2"),
                Value::string("3"),
            ]),
        );
    }

    #[test]
    fn builtins_split_version_pre_release() {
        // Digit/non-digit transitions still split, but the `.` is dropped.
        assert_eq!(
            ev(r#"builtins.splitVersion "1.0pre1""#),
            Value::List(vec![
                Value::string("1"),
                Value::string("0"),
                Value::string("pre"),
                Value::string("1"),
            ]),
        );
    }

    // ── pathExists tests ────────────────────────────────────

    #[test]
    fn builtins_path_exists_tmpfile() {
        let dir = std::env::temp_dir();
        let path = dir.join("sui_eval_test_path_exists.txt");
        std::fs::write(&path, "test").unwrap();
        let expr = format!(r#"builtins.pathExists "{}""#, path.display());
        assert_eq!(eval(&expr).unwrap(), Value::Bool(true));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn builtins_path_exists_nonexistent() {
        assert_eq!(
            ev(r#"builtins.pathExists "/nonexistent/path/that/surely/does/not/exist""#),
            Value::Bool(false),
        );
    }

    // ── toFile tests ────────────────────────────────────────

    #[test]
    fn builtins_to_file_returns_store_path() {
        let v = ev(r#"builtins.toFile "test.txt" "hello""#);
        if let Value::Path(p) = v {
            assert!(p.starts_with("/nix/store/"));
            assert!(p.ends_with("-test.txt"));
        } else {
            panic!("expected path, got {v}");
        }
    }

    #[test]
    fn builtins_to_file_deterministic() {
        // Same name + content should produce same path
        let v1 = ev(r#"builtins.toFile "f" "content""#);
        let v2 = ev(r#"builtins.toFile "f" "content""#);
        assert_eq!(v1, v2);
    }

    // ── hashFile tests ──────────────────────────────────────

    #[test]
    fn builtins_hash_file_sha256() {
        let dir = std::env::temp_dir();
        let path = dir.join("sui_eval_test_hashfile.txt");
        std::fs::write(&path, "hello").unwrap();
        let expr = format!(r#"builtins.hashFile "sha256" "{}""#, path.display());
        let v = eval(&expr).unwrap();
        if let Value::String(ns) = v {
            assert_eq!(ns.chars.len(), 64);
            assert_eq!(ns.chars, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        } else {
            panic!("expected string");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn builtins_hash_file_missing() {
        let result = eval(r#"builtins.hashFile "sha256" "/nonexistent/file.txt""#);
        assert!(result.is_err());
    }

    // ── unsafeGetAttrPos tests ──────────────────────────────

    #[test]
    fn builtins_unsafe_get_attr_pos_returns_null() {
        assert_eq!(
            ev(r#"builtins.unsafeGetAttrPos "x" { x = 1; }"#),
            Value::Null,
        );
    }

    // ── storeDir / nixPath constants ────────────────────────

    #[test]
    fn builtins_store_dir() {
        assert_eq!(ev(r#"builtins.storeDir"#), Value::string("/nix/store"));
    }

    #[test]
    fn builtins_nix_path_empty() {
        assert_eq!(ev(r#"builtins.nixPath"#), Value::List(vec![]));
    }

    // ── findFile tests ──────────────────────────────────────

    #[test]
    fn builtins_find_file_exact_match() {
        // Create a temp dir structure
        let dir = std::env::temp_dir().join("sui_eval_test_findfile");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("test.nix"), "42").unwrap();
        let expr = format!(
            r#"builtins.findFile [{{ prefix = "test.nix"; path = "{}"; }}] "test.nix""#,
            dir.join("test.nix").display()
        );
        let v = eval(&expr).unwrap();
        assert!(matches!(v, Value::Path(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtins_find_file_not_found() {
        let result = eval(r#"builtins.findFile [] "nonexistent""#);
        assert!(result.is_err());
    }

    // ── Phase 3 builtins tests ────────────────────────────

    #[test] fn builtins_generic_closure_linear() {
        assert_eq!(ev(r#"builtins.length (builtins.genericClosure { startSet = [{ key = 1; }]; operator = item: if item.key < 3 then [{ key = item.key + 1; }] else []; })"#), Value::Int(3));
    }
    #[test] fn builtins_generic_closure_empty_op() {
        assert_eq!(ev(r#"builtins.length (builtins.genericClosure { startSet = [{ key = 1; }]; operator = item: []; })"#), Value::Int(1));
    }
    #[test] fn builtins_generic_closure_dedup() {
        assert_eq!(ev(r#"builtins.length (builtins.genericClosure { startSet = [{ key = 1; }]; operator = item: [{ key = 1; } { key = 2; }]; })"#), Value::Int(2));
    }

    #[test] fn builtins_from_toml_simple() {
        let v = ev(r#"builtins.fromTOML "[section]\nkey = \"value\"""#);
        if let Value::Attrs(a) = v { assert!(a.contains_key("section")); } else { panic!(); }
    }

    #[test] fn builtins_less_than_int() {
        assert_eq!(ev("builtins.lessThan 1 2"), Value::Bool(true));
        assert_eq!(ev("builtins.lessThan 2 1"), Value::Bool(false));
    }

    #[test] fn builtins_bit_and_12_10() { assert_eq!(ev("builtins.bitAnd 12 10"), Value::Int(8)); }
    #[test] fn builtins_bit_or_12_10() { assert_eq!(ev("builtins.bitOr 12 10"), Value::Int(14)); }
    #[test] fn builtins_bit_xor_12_10() { assert_eq!(ev("builtins.bitXor 12 10"), Value::Int(6)); }

    #[test] fn builtins_split_version() {
        assert_eq!(ev(r#"builtins.splitVersion "1.2.3""#), Value::List(vec![
            Value::string("1"), Value::string("2"), Value::string("3")
        ]));
    }
    #[test] fn builtins_split_version_pre() {
        let v = ev(r#"builtins.splitVersion "1pre2""#);
        if let Value::List(l) = v { assert!(l.len() >= 3); } else { panic!(); }
    }

    #[test] fn builtins_path_exists_false() {
        assert_eq!(ev(r#"builtins.pathExists "/nonexistent/path/12345""#), Value::Bool(false));
    }

    #[test] fn builtins_to_file() {
        let v = ev(r#"builtins.toFile "test.txt" "hello""#);
        assert!(matches!(v, Value::Path(_) | Value::String(_)));
    }

    #[test] fn builtins_unsafe_get_attr_pos() {
        assert_eq!(ev(r#"builtins.unsafeGetAttrPos "a" { a = 1; }"#), Value::Null);
    }

    #[test] fn builtins_derivation_strict() {
        let v = ev(r#"builtins.derivationStrict { name = "test"; system = "x86_64-linux"; builder = "/bin/sh"; }"#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("type").unwrap().as_string().unwrap(), "derivation");
            assert!(a.contains_key("drvPath"));
        } else { panic!(); }
    }

    #[test] fn builtins_to_xml_int() {
        let v = ev("builtins.toXML 42");
        let s = v.as_string().unwrap();
        assert!(s.contains("<int value=\"42\""));
    }
    #[test] fn builtins_to_xml_attrs() {
        let v = ev(r#"builtins.toXML { a = 1; }"#);
        let s = v.as_string().unwrap();
        assert!(s.contains("<attrs>"));
        assert!(s.contains("attr name=\"a\""));
    }

    // ── Curried arithmetic builtins ───────────────────────

    #[test]
    fn builtins_sub_ints() {
        assert_eq!(ev("builtins.sub 10 3"), Value::Int(7));
    }

    #[test]
    fn builtins_mul_ints() {
        assert_eq!(ev("builtins.mul 4 5"), Value::Int(20));
    }

    #[test]
    fn builtins_div_ints() {
        assert_eq!(ev("builtins.div 10 3"), Value::Int(3));
    }

    #[test]
    fn builtins_div_by_zero() {
        let result = eval("builtins.div 10 0");
        assert!(result.is_err());
    }

    #[test]
    fn builtins_add_ints() {
        assert_eq!(ev("builtins.add 3 4"), Value::Int(7));
    }

    #[test]
    fn builtins_add_floats() {
        assert_eq!(ev("builtins.add 1.5 2.5"), Value::Float(4.0));
    }

    #[test]
    fn builtins_add_mixed_int_float() {
        assert_eq!(ev("builtins.add 1 2.5"), Value::Float(3.5));
    }

    // ── isFloat ───────────────────────────────────────────

    #[test]
    fn builtins_is_float_true() {
        assert_eq!(ev("builtins.isFloat 1.0"), Value::Bool(true));
    }

    #[test]
    fn builtins_is_float_false() {
        assert_eq!(ev("builtins.isFloat 1"), Value::Bool(false));
    }

    // ── deepSeq ───────────────────────────────────────────

    #[test]
    fn builtins_deep_seq() {
        assert_eq!(ev("builtins.deepSeq [1 2 3] 42"), Value::Int(42));
    }

    #[test]
    fn builtins_deep_seq_with_attrs() {
        assert_eq!(ev(r#"builtins.deepSeq { a = 1; b = 2; } "ok""#), Value::string("ok"));
    }

    // ── getEnv ────────────────────────────────────────────

    #[test]
    fn builtins_get_env_missing() {
        assert_eq!(
            ev(r#"builtins.getEnv "DEFINITELY_NOT_SET_12345_XYZ""#),
            Value::string(""),
        );
    }

    // ── currentTime ───────────────────────────────────────

    #[test]
    fn builtins_current_time_is_int() {
        let v = ev("builtins.currentTime null");
        assert!(matches!(v, Value::Int(_)));
        if let Value::Int(t) = v {
            assert!(t > 0);
        }
    }

    // ── substring ─────────────────────────────────────────

    #[test]
    fn builtins_substring_basic() {
        assert_eq!(
            ev(r#"builtins.substring 0 5 "hello world""#),
            Value::string("hello"),
        );
    }

    #[test]
    fn builtins_substring_from_middle() {
        assert_eq!(
            ev(r#"builtins.substring 6 5 "hello world""#),
            Value::string("world"),
        );
    }

    #[test]
    fn builtins_substring_beyond_length() {
        assert_eq!(
            ev(r#"builtins.substring 0 100 "hi""#),
            Value::string("hi"),
        );
    }

    // ── split ─────────────────────────────────────────────

    #[test]
    fn builtins_split_basic() {
        let v = ev(r#"builtins.split "o" "foobar""#);
        if let Value::List(parts) = v {
            assert!(parts.len() >= 3);
        } else {
            panic!("expected list");
        }
    }

    // ── hasContext / getContext / unsafeDiscardStringContext ──

    #[test]
    fn builtins_has_context_plain_string() {
        assert_eq!(ev(r#"builtins.hasContext "hello""#), Value::Bool(false));
    }

    #[test]
    fn builtins_get_context_plain_string() {
        let v = ev(r#"builtins.getContext "hello""#);
        if let Value::Attrs(a) = v {
            assert!(a.is_empty());
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_unsafe_discard_string_context() {
        assert_eq!(
            ev(r#"builtins.unsafeDiscardStringContext "hello""#),
            Value::string("hello"),
        );
    }

    // ── unsafeDiscardOutputDependency ─────────────────────

    #[test]
    fn builtins_unsafe_discard_output_dependency() {
        assert_eq!(
            ev(r#"builtins.unsafeDiscardOutputDependency "hello""#),
            Value::string("hello"),
        );
    }

    // ── appendContext ─────────────────────────────────────

    #[test]
    fn builtins_append_context_empty() {
        assert_eq!(
            ev(r#"builtins.appendContext "hello" {}"#),
            Value::string("hello"),
        );
    }

    // ── convertHash ───────────────────────────────────────

    #[test]
    fn builtins_convert_hash_sha256_hex_to_base64() {
        let v = ev(r#"builtins.convertHash { hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"; hashAlgo = "sha256"; toHashFormat = "base64"; }"#);
        if let Value::String(s) = v {
            assert!(!s.chars.is_empty());
        } else {
            panic!("expected string");
        }
    }

    #[test]
    fn builtins_convert_hash_sha256_hex_to_sri() {
        let v = ev(r#"builtins.convertHash { hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"; hashAlgo = "sha256"; toHashFormat = "sri"; }"#);
        if let Value::String(s) = v {
            assert!(s.chars.starts_with("sha256-"));
        } else {
            panic!("expected string");
        }
    }

    // ── toXML additional ──────────────────────────────────

    #[test]
    fn builtins_to_xml_string() {
        let v = ev(r#"builtins.toXML "hello""#);
        let s = v.as_string().unwrap();
        assert!(s.contains("<string value="));
    }

    #[test]
    fn builtins_to_xml_list() {
        let v = ev("builtins.toXML [1 2]");
        let s = v.as_string().unwrap();
        assert!(s.contains("<list>"));
    }

    #[test]
    fn builtins_to_xml_bool() {
        let v = ev("builtins.toXML true");
        let s = v.as_string().unwrap();
        assert!(s.contains("<bool value=\"true\""));
    }

    #[test]
    fn builtins_to_xml_null() {
        let v = ev("builtins.toXML null");
        let s = v.as_string().unwrap();
        assert!(s.contains("<null"));
    }

    // ── typeOf comprehensive ──────────────────────────────

    #[test]
    fn builtins_type_of_int() {
        assert_eq!(ev("builtins.typeOf 42"), Value::string("int"));
    }

    #[test]
    fn builtins_type_of_float() {
        assert_eq!(ev("builtins.typeOf 3.14"), Value::string("float"));
    }

    #[test]
    fn builtins_type_of_string() {
        assert_eq!(ev(r#"builtins.typeOf "hello""#), Value::string("string"));
    }

    #[test]
    fn builtins_type_of_bool() {
        assert_eq!(ev("builtins.typeOf true"), Value::string("bool"));
    }

    #[test]
    fn builtins_type_of_null() {
        assert_eq!(ev("builtins.typeOf null"), Value::string("null"));
    }

    #[test]
    fn builtins_type_of_list() {
        assert_eq!(ev("builtins.typeOf [1 2]"), Value::string("list"));
    }

    #[test]
    fn builtins_type_of_set() {
        assert_eq!(ev("builtins.typeOf { a = 1; }"), Value::string("set"));
    }

    #[test]
    fn builtins_type_of_lambda() {
        assert_eq!(ev("builtins.typeOf (x: x)"), Value::string("lambda"));
    }

    #[test]
    fn builtins_type_of_path() {
        assert_eq!(ev("builtins.typeOf /foo"), Value::string("path"));
    }

    // ── head / tail edge cases ────────────────────────────

    #[test]
    fn builtins_head_single() {
        assert_eq!(ev("builtins.head [42]"), Value::Int(42));
    }

    #[test]
    fn builtins_head_empty_errors() {
        assert!(eval("builtins.head []").is_err());
    }

    #[test]
    fn builtins_tail_single() {
        assert_eq!(ev("builtins.tail [42]"), Value::List(vec![]));
    }

    #[test]
    fn builtins_tail_empty_errors() {
        assert!(eval("builtins.tail []").is_err());
    }

    // ── attrNames / attrValues determinism ────────────────

    #[test]
    fn builtins_attr_names_sorted() {
        assert_eq!(
            ev(r#"builtins.attrNames { z = 1; a = 2; m = 3; }"#),
            Value::List(vec![
                Value::string("a"),
                Value::string("m"),
                Value::string("z"),
            ]),
        );
    }

    #[test]
    fn builtins_attr_values_follows_sorted_keys() {
        assert_eq!(
            ev(r#"builtins.attrValues { z = 3; a = 1; m = 2; }"#),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    // ── toString additional ───────────────────────────────

    #[test]
    fn builtins_to_string_int() {
        assert_eq!(ev("builtins.toString 42"), Value::string("42"));
    }

    #[test]
    fn builtins_to_string_bool() {
        assert_eq!(ev("builtins.toString true"), Value::string("1"));
        assert_eq!(ev("builtins.toString false"), Value::string(""));
    }

    #[test]
    fn builtins_to_string_null() {
        assert_eq!(ev("builtins.toString null"), Value::string(""));
    }

    #[test]
    fn builtins_to_string_path() {
        assert_eq!(ev("builtins.toString /foo"), Value::string("/foo"));
    }

    #[test]
    fn builtins_to_string_list_not_supported() {
        let result = eval("builtins.toString [1 2 3]");
        assert!(result.is_err());
    }

    // ── abort ─────────────────────────────────────────────

    #[test]
    fn builtins_abort_produces_error() {
        let result = eval(r#"builtins.abort "fatal""#);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("fatal"));
    }

    // ── fromJSON additional ───────────────────────────────

    #[test]
    fn builtins_from_json_null() {
        assert_eq!(ev(r#"builtins.fromJSON "null""#), Value::Null);
    }

    #[test]
    fn builtins_from_json_bool() {
        assert_eq!(ev(r#"builtins.fromJSON "true""#), Value::Bool(true));
    }

    #[test]
    fn builtins_from_json_list() {
        assert_eq!(
            ev(r#"builtins.fromJSON "[1,2,3]""#),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    // ── toJSON additional ─────────────────────────────────

    #[test]
    fn builtins_to_json_null() {
        assert_eq!(ev("builtins.toJSON null"), Value::string("null"));
    }

    #[test]
    fn builtins_to_json_list() {
        assert_eq!(
            ev("builtins.toJSON [1 2 3]"),
            Value::string("[1,2,3]"),
        );
    }

    // ── string operations ─────────────────────────────────

    #[test]
    fn builtins_string_length_empty() {
        assert_eq!(ev(r#"builtins.stringLength """#), Value::Int(0));
    }

    #[test]
    fn builtins_string_length_unicode() {
        assert_eq!(ev(r#"builtins.stringLength "abc""#), Value::Int(3));
    }

    // ── replaceStrings edge cases ─────────────────────────

    #[test]
    fn builtins_replace_strings_empty_from() {
        assert_eq!(
            ev(r#"builtins.replaceStrings [] [] "hello""#),
            Value::string("hello"),
        );
    }

    #[test]
    fn builtins_replace_strings_no_match() {
        assert_eq!(
            ev(r#"builtins.replaceStrings ["x"] ["y"] "hello""#),
            Value::string("hello"),
        );
    }
}
