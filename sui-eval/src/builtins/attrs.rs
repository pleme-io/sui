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
        Ok(Value::List(Rc::new(NixList::new(attrs.values().cloned().collect()))))
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
                // SYM-KEYED end to end (lever 1 of the 20s campaign,
                // 2026-07-21). The previous `iter_unsorted` + `contains_key`
                // + `insert` loop was the single hottest code shape in the
                // whole cid eval — live sampling attributed 63/70 interner
                // leaves to THIS closure: String-per-key materialization,
                // re-intern in contains_key, third intern in insert. The
                // Symbols never needed to leave symbol space at all.
                // Byte-neutral: result order is re-derived at observation
                // time via sorted_entries, same as before.
                for (sym, v) in b.iter_syms() {
                    if a.contains_key_sym(&sym) {
                        result.insert_sym(sym, v.clone());
                    }
                }
                Ok(Value::Attrs(Rc::new(result)))
            }),
        })))
    });

    // `filterAttrs` was registered here and is GONE. It is nixpkgs
    // `lib.attrsets.filterAttrs`, not a CppNix builtin at any feature level —
    // verified absent from nix 2.31.5 with every experimental feature on.
    //
    // This one is the WORST of the six, and is why the class was found at all:
    // nixpkgs feature-detects with `builtins ? filterAttrs`, so sui exposing it
    // did not merely accept an extra expression — it silently steered nixpkgs
    // down a DIFFERENT branch than real nix takes. A permissiveness bug that
    // changes which code runs is not a superset, it is a fork.

    // Attrset higher-order operations
    register_builtin(builtins, "mapAttrs", |args| {
        let func = args[0].clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "mapAttrs<partial>",
            func: Rc::new(move |args2| {
                let attrs = args2[0].to_attrs()?;
                let mut result = NixAttrs::new();
                // Fresh result map; each value is an independent lazy thunk,
                // so per-entry mapping is order-independent and the sorted
                // iter was dead work. Byte-neutral.
                // Sym-keyed (lever 1): one resolve for the lambda's key arg,
                // zero-intern insert, no per-call Vec collect.
                for (sym, v) in attrs.iter_syms() {
                    let f = func.clone();
                    let key = sui_intern::resolve(sym);
                    let val = v.clone();
                    let thunk = Thunk::new_native(move || {
                        let partial = crate::eval::apply(f, Value::string(key))?;
                        crate::eval::apply(partial, val)
                    });
                    result.insert_sym(sym, Value::Thunk(thunk));
                }
                Ok(Value::Attrs(Rc::new(result)))
            }),
        })))
    });
    register_builtin(builtins, "listToAttrs", |args| {
        let list = args[0].to_list()?;
        let mut attrs = NixAttrs::new();
        for item in list {
            let item_attrs = item.to_attrs()?;
            let name = item_attrs.get("name")
                .ok_or_else(|| EvalError::AttrNotFound("name".to_string()))?
                .to_str()?;
            let value = item_attrs.get("value")
                .ok_or_else(|| EvalError::AttrNotFound("value".to_string()))?
                .clone();
            // Nix `listToAttrs` semantics: on a duplicate `name`, the FIRST
            // occurrence wins (later duplicates are ignored). cppnix builds
            // the attrset with an ordered insert that refuses to overwrite an
            // existing key. `NixAttrs::insert` is last-wins, so guard with an
            // explicit first-wins skip. (Byte-parity root: a Cargo.lock that
            // lists a crate twice — a registry entry then a git entry of the
            // same name+version — must resolve to the FIRST/registry source,
            // exactly as nix does; last-wins picked the git source and
            // produced a structurally different rust_<crate> derivation.)
            if !attrs.contains_key(&name) {
                attrs.insert(name, value);
            }
        }
        Ok(Value::Attrs(Rc::new(attrs)))
    });
    register_builtin(builtins, "catAttrs", |args| {
        let name = args[0].as_string()?.to_string();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "catAttrs<partial>",
            func: Rc::new(move |args2| {
                let list = args2[0].to_list()?;
                let mut result = Vec::new();
                for item in &list {
                    if let Ok(attrs) = item.to_attrs()
                        && let Some(v) = attrs.get(&name) {
                            result.push(v.clone());
                        }
                }
                Ok(Value::List(Rc::new(NixList::new(result))))
            }),
        })))
    });
    register_builtin(builtins, "removeAttrs", |args| {
        let set = args[0].to_attrs()?.clone();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "removeAttrs<partial>",
            func: Rc::new(move |args2| {
                let names = args2[0].to_list()?;
                let remove: Vec<String> = names.iter()
                    .filter_map(|v| v.to_str().ok())
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
                let list = args2[0].to_list()?;
                // Collect all keys and their values across all attrsets
                let mut collected: std::collections::BTreeMap<String, Vec<Value>> =
                    std::collections::BTreeMap::new();
                for item in &list {
                    let attrs = item.to_attrs()?;
                    // Feeds a BTreeMap keyed by name — the sort re-imposed by
                    // `iter()` is redundant with the BTreeMap's own ordering.
                    for (k, v) in attrs.iter_unsorted() {
                        collected.entry(k.clone()).or_default().push(v.clone());
                    }
                }
                let mut result = NixAttrs::new();
                for (k, vs) in collected {
                    // CRITICAL: Wrap each merge result in a native thunk.
                    // CppNix's zipAttrsWith produces a lazy attrset where
                    // each key's merge result is independently evaluable.
                    // Eagerly applying the merge function forces ALL keys,
                    // which breaks the nixpkgs module system's fixpoint:
                    // pushedDownDefinitionsByName uses zipAttrsWith, and
                    // eagerly merging ALL definitions forces config values
                    // while config is still being computed (blackhole).
                    let f = func.clone();
                    let key = k.clone();
                    let thunk = Thunk::new_native(move || {
                        let partial = crate::eval::apply(
                            f,
                            Value::string(key),
                        )?;
                        crate::eval::apply(partial, Value::List(Rc::new(NixList::new(vs))))
                    });
                    result.insert(k, Value::Thunk(thunk));
                }
                Ok(Value::Attrs(Rc::new(result)))
            }),
        })))
    });
}
