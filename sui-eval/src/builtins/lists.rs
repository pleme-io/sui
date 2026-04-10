//! List builtins: length, head, tail, elemAt, elem, map, filter, sort, foldl',
//! genList, concatMap, concatLists, all, any, partition, groupBy.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_builtin(builtins, "length", |args| {
        Ok(Value::Int(args[0].as_list()?.len() as i64))
    });
    register_builtin(builtins, "head", |args| {
        let list = args[0].as_list()?;
        list.first()
            .cloned()
            .ok_or_else(|| EvalError::TypeError("head: empty list".to_string()))
    });
    register_builtin(builtins, "tail", |args| {
        let list = args[0].as_list()?;
        if list.is_empty() {
            return Err(EvalError::TypeError("tail: empty list".to_string()));
        }
        Ok(Value::List(Rc::new(list[1..].to_vec())))
    });
    register_builtin(builtins, "elemAt", |args| {
        // Curried: builtins.elemAt list index
        let list = args[0].as_list()?.to_vec();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "elemAt<partial>",
            func: Rc::new(move |args2| {
                let idx = args2[0].as_int()? as usize;
                list.get(idx)
                    .cloned()
                    .ok_or_else(|| EvalError::TypeError(format!("elemAt: index {idx} out of bounds")))
            }),
        })))
    });
    register_builtin(builtins, "elem", |args| {
        let needle = args[0].clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "elem<partial>",
            func: Rc::new(move |args2| {
                let haystack = args2[0].as_list()?;
                Ok(Value::Bool(haystack.contains(&needle)))
            }),
        })))
    });
    register_builtin(builtins, "genList", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "genList<partial>",
            func: Rc::new(move |args2| {
                let n = args2[0].as_int()?;
                let mut result = Vec::new();
                for i in 0..n {
                    result.push(crate::eval::apply(func.clone(), Value::Int(i))?);
                }
                Ok(Value::List(Rc::new(result)))
            }),
        })))
    });

    // ── Higher-order list operations (critical for nixpkgs) ─────
    //
    // Clone cost analysis: all `.clone()` calls on `func`, `pred`, and
    // list element `v` in these loops are Rc reference-count bumps, NOT
    // deep copies.  The Value enum's heap variants are:
    //
    //   Lambda(Closure { env: Env(Rc<EnvInner>), .. })  → Rc bump
    //   Builtin(BuiltinFn { func: Rc<BuiltinFunc>, .. }) → Rc bump
    //   Attrs(NixAttrs)  → BTreeMap clone (but inner Values are Rc'd)
    //   List(Vec<Value>) → Vec clone (but inner Values are Rc'd)
    //   String(NixString) → String clone (typically interned/small)
    //   Thunk(Thunk(Rc<RefCell<ThunkRepr>>))  → Rc bump
    //
    // For the common case in nixpkgs — `map`, `filter`, `foldl'` over
    // lists of attrsets or lambdas — every clone is O(1).  The `apply`
    // function consumes its `arg: Value` by value, so we must clone once
    // per predicate/function call; matched elements are cloned a second
    // time to push into the result Vec.  This is inherent to the
    // ownership model and already minimal.

    register_builtin(builtins, "map", |args| {
        let func = args[0].clone(); // Rc bump (captured closure/builtin)
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "map<partial>",
            func: Rc::new(move |args2| {
                let list = args2[0].as_list()?;
                let result: Result<Vec<_>, _> = list.iter()
                    .map(|v| crate::eval::apply(func.clone(), v.clone()))
                    .collect();
                Ok(Value::List(Rc::new(result?)))
            }),
        })))
    });
    register_builtin(builtins, "filter", |args| {
        let pred = args[0].clone(); // Rc bump
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "filter<partial>",
            func: Rc::new(move |args2| {
                let list = args2[0].as_list()?;
                let mut result = Vec::new();
                for v in list {
                    if crate::eval::apply(pred.clone(), v.clone())?.as_bool()? {
                        result.push(v.clone());
                    }
                }
                Ok(Value::List(Rc::new(result)))
            }),
        })))
    });
    register_builtin(builtins, "foldl'", |args| {
        let func = args[0].clone(); // Rc bump
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "foldl'<p1>",
            func: Rc::new(move |args2| {
                let init = args2[0].clone();
                let func2 = func.clone();
                Ok(Value::Builtin(Box::new(BuiltinFn {
                    name: "foldl'<p2>",
                    func: Rc::new(move |args3| {
                        let list = args3[0].as_list()?;
                        let mut acc = init.clone();
                        for v in list {
                            let partial = crate::eval::apply(func2.clone(), acc)?;
                            acc = crate::eval::apply(partial, v.clone())?;
                        }
                        Ok(acc)
                    }),
                })))
            }),
        })))
    });
    register_builtin(builtins, "concatMap", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "concatMap<partial>",
            func: Rc::new(move |args2| {
                let list = args2[0].as_list()?;
                let mut result = Vec::new();
                for v in list {
                    let mapped = crate::eval::apply(func.clone(), v.clone())?;
                    result.extend_from_slice(mapped.as_list()?);
                }
                Ok(Value::List(Rc::new(result)))
            }),
        })))
    });
    register_builtin(builtins, "concatLists", |args| {
        let lists = args[0].as_list()?;
        // Pre-compute total length to allocate once.
        let total_len: usize = lists.iter()
            .filter_map(|v| v.as_list().ok())
            .map(|l| l.len())
            .sum();
        let mut result = Vec::with_capacity(total_len);
        for v in lists {
            let inner = v.as_list()?;
            result.extend(inner.iter().cloned());
        }
        Ok(Value::List(Rc::new(result)))
    });
    register_builtin(builtins, "sort", |args| {
        let cmp = args[0].clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "sort<partial>",
            func: Rc::new(move |args2| {
                let mut list = args2[0].as_list()?.to_vec();
                if list.len() <= 1 {
                    return Ok(Value::List(Rc::new(list)));
                }
                // O(n log n) stable sort via Rust's merge sort.
                // Capture any comparator error and propagate after sort.
                let mut err: Option<EvalError> = None;
                list.sort_by(|a, b| {
                    if err.is_some() {
                        return std::cmp::Ordering::Equal;
                    }
                    match crate::eval::apply(cmp.clone(), a.clone())
                        .and_then(|partial| crate::eval::apply(partial, b.clone()))
                        .and_then(|v| v.as_bool().map_err(|_| {
                            EvalError::TypeError("sort comparator must return bool".into())
                        }))
                    {
                        Ok(true) => std::cmp::Ordering::Less,
                        Ok(false) => std::cmp::Ordering::Greater,
                        Err(e) => {
                            err = Some(e);
                            std::cmp::Ordering::Equal
                        }
                    }
                });
                if let Some(e) = err {
                    return Err(e);
                }
                Ok(Value::List(Rc::new(list)))
            }),
        })))
    });
    register_builtin(builtins, "all", |args| {
        let pred = args[0].clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "all<partial>",
            func: Rc::new(move |args2| {
                let list = args2[0].as_list()?;
                for v in list {
                    if !crate::eval::apply(pred.clone(), v.clone())?.as_bool()? {
                        return Ok(Value::Bool(false));
                    }
                }
                Ok(Value::Bool(true))
            }),
        })))
    });
    register_builtin(builtins, "any", |args| {
        let pred = args[0].clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "any<partial>",
            func: Rc::new(move |args2| {
                let list = args2[0].as_list()?;
                for v in list {
                    if crate::eval::apply(pred.clone(), v.clone())?.as_bool()? {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }),
        })))
    });

    // partition — split list by predicate into { right, wrong }
    register_builtin(builtins, "partition", |args| {
        let pred = args[0].clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "partition<partial>",
            func: Rc::new(move |args2| {
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
                result.insert("right".to_string(), Value::List(Rc::new(right)));
                result.insert("wrong".to_string(), Value::List(Rc::new(wrong)));
                Ok(Value::Attrs(Rc::new(result)))
            }),
        })))
    });

    // groupBy — group list elements by key function
    register_builtin(builtins, "groupBy", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "groupBy<partial>",
            func: Rc::new(move |args2| {
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
                    result.insert(k, Value::List(Rc::new(vs)));
                }
                Ok(Value::Attrs(Rc::new(result)))
            }),
        })))
    });
}
