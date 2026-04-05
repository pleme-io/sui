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
            Value::Attrs(_) => return Err(EvalError::TypeError("toString: cannot convert set".to_string())),
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
    register_builtin(&mut builtins_set, "split", |args| {
        let regex_str = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "split<partial>",
            func: Arc::new(move |args2| {
                let s = args2[0].as_string()?;
                // Simple split on literal string (full regex in later phase)
                let parts: Vec<Value> = s.split(&regex_str)
                    .map(|p| Value::String(p.to_string()))
                    .collect();
                Ok(Value::List(parts))
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
}
