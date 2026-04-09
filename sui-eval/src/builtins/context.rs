//! String context builtins: hasContext, getContext, appendContext,
//! unsafeDiscardStringContext, unsafeDiscardOutputDependency, addDrvOutputDependencies.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_builtin(builtins, "hasContext", |args| {
        match &args[0] {
            Value::String(ns) => Ok(Value::Bool(ns.has_context())),
            _ => Err(EvalError::TypeError("hasContext: expected string".into())),
        }
    });
    register_builtin(builtins, "getContext", |args| {
        let ns = match &args[0] {
            Value::String(ns) => ns,
            _ => return Err(EvalError::TypeError("getContext: expected string".into())),
        };
        let mut plains: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut om: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
        let mut deep: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for elem in ns.context.iter() {
            match elem {
                ContextElement::Plain(p) => { plains.insert(p.to_string()); }
                ContextElement::Output { drv, output } => {
                    om.entry(drv.to_string()).or_default().push(output.to_string());
                }
                ContextElement::DrvDeep(d) => { deep.insert(d.to_string()); }
            }
        }
        let mut result = NixAttrs::new();
        for p in &plains {
            let mut a = NixAttrs::new();
            a.insert("path".to_string(), Value::Bool(true));
            result.insert(p.clone(), Value::Attrs(a));
        }
        for (d, os) in &om {
            let mut a = NixAttrs::new();
            a.insert("outputs".to_string(), Value::List(os.iter().map(|o| Value::string(o.clone())).collect()));
            result.insert(d.clone(), Value::Attrs(a));
        }
        for d in &deep {
            let mut a = NixAttrs::new();
            a.insert("allOutputs".to_string(), Value::Bool(true));
            result.insert(d.clone(), Value::Attrs(a));
        }
        Ok(Value::Attrs(result))
    });
    register_builtin(builtins, "unsafeDiscardStringContext", |args| {
        match &args[0] {
            Value::String(ns) => Ok(Value::string(ns.chars.clone())),
            _ => Err(EvalError::TypeError("unsafeDiscardStringContext: expected string".into())),
        }
    });
    register_builtin(builtins, "unsafeDiscardOutputDependency", |args| {
        match &args[0] {
            Value::String(ns) => {
                let mut nc = StringContext::new();
                for elem in ns.context.iter() {
                    match elem {
                        ContextElement::DrvDeep(d) | ContextElement::Output { drv: d, .. } => {
                            nc.add_plain(d.clone());
                        }
                        other => { nc.insert(other.clone()); }
                    }
                }
                Ok(Value::String(NixString::with_context(ns.chars.clone(), nc)))
            }
            _ => Err(EvalError::TypeError("unsafeDiscardOutputDependency: expected string".into())),
        }
    });
    register_builtin(builtins, "addDrvOutputDependencies", |args| {
        match &args[0] {
            Value::String(ns) => {
                let mut nc = StringContext::new();
                for elem in ns.context.iter() {
                    match elem {
                        ContextElement::Plain(p) if p.ends_with(".drv") => {
                            nc.add_drv_deep(p.clone());
                        }
                        ContextElement::Output { drv, .. } => {
                            nc.add_drv_deep(drv.clone());
                        }
                        other => { nc.insert(other.clone()); }
                    }
                }
                Ok(Value::String(NixString::with_context(ns.chars.clone(), nc)))
            }
            _ => Err(EvalError::TypeError("addDrvOutputDependencies: expected string".into())),
        }
    });
    register_curried(builtins, "appendContext", |sv, cv| {
        let ns = match sv {
            Value::String(ns) => ns.clone(),
            _ => return Err(EvalError::TypeError("appendContext: expected string".into())),
        };
        let ca = cv.to_attrs()?;
        let mut nc = ns.context.clone();
        for (key, val) in ca.iter() {
            let ea = crate::eval::force_value(val)?.to_attrs()?;
            if ea.contains_key("path") {
                nc.add_plain(key.clone());
            }
            if let Some(ov) = ea.get("outputs") {
                let ol = crate::eval::force_value(ov)?.to_list()?;
                for o in &ol {
                    nc.add_output(key.clone(), crate::eval::force_value(o)?.to_str()?);
                }
            }
            if ea.contains_key("allOutputs") {
                nc.add_drv_deep(key.clone());
            }
        }
        Ok(Value::String(NixString::with_context(ns.chars, nc)))
    });
}
