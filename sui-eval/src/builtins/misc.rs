//! Miscellaneous builtins: genericClosure, functionArgs, placeholder, import,
//! scopedImport, getEnv, currentTime, findFile, unsafeGetAttrPos, toFile.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_builtin(builtins, "functionArgs", |args| {
        match &args[0] {
            Value::Lambda(closure) => {
                let mut result = NixAttrs::new();
                if let rnix::ast::Param::Pattern(pat) = &closure.param {
                    for entry in pat.pat_entries() {
                        if let Some(ident) = entry.ident() {
                            let has_default = entry.default().is_some();
                            result.insert(ident.to_string(), Value::Bool(has_default));
                        }
                    }
                }
                Ok(Value::Attrs(Rc::new(result)))
            }
            Value::Builtin(_) => Ok(Value::Attrs(Rc::new(NixAttrs::new()))),
            _ => Err(EvalError::TypeError("functionArgs: expected function".to_string())),
        }
    });

    // Impure builtins
    register_builtin(builtins, "getEnv", |args| {
        let name = args[0].as_string()?;
        Ok(Value::string(std::env::var(name).unwrap_or_default()))
    });

    register_builtin(builtins, "currentTime", |_args| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Ok(Value::Int(now))
    });

    register_builtin(builtins, "placeholder", |args| {
        let output = args[0].as_string()?;
        use sha2::{Sha256, Digest};
        let hash = Sha256::digest(format!("nix-output:{output}").as_bytes());
        let hash_str = format!("{:x}", hash);
        Ok(Value::string(format!("/placeholder-{}", &hash_str[..32])))
    });

    // genericClosure
    register_builtin(builtins, "genericClosure", |args| {
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
            let new_items = crate::eval::apply_and_force(operator.clone(), item)?;
            let new_list = new_items.to_list()?;
            work_list.extend(new_list);
        }

        Ok(Value::List(Rc::new(result)))
    });

    // scopedImport
    register_curried(builtins, "scopedImport", |scope_val, path_val| {
        let scope = scope_val.to_attrs()?.clone();
        let raw_path = path_val.coerce_to_path("scopedImport")?;
        let resolved = crate::path::resolve_import(
            crate::eval::current_eval_dir().as_deref(),
            &raw_path,
        ).unwrap_or_else(|_| std::path::PathBuf::from(&raw_path));
        let path = resolved.to_string_lossy().into_owned();
        let source = std::fs::read_to_string(&path).map_err(|e| EvalError::IoError {
            context: format!("scopedImport {path}"),
            message: e.to_string(),
        })?;
        fn render_scope_attrs(attrs: &NixAttrs) -> Result<String, EvalError> {
            let mut out = String::from("{");
            for (k, v) in attrs.iter() {
                let forced = crate::eval::force_value(v)?;
                let rhs = match &forced {
                    Value::Int(n) => n.to_string(),
                    Value::Float(f) => format!("{f:.6}"),
                    Value::Bool(true) => "true".to_string(),
                    Value::Bool(false) => "false".to_string(),
                    Value::Null => "null".to_string(),
                    Value::String(ns) => {
                        let escaped = ns
                            .chars
                            .replace('\\', "\\\\")
                            .replace('"', "\\\"")
                            .replace('$', "\\$");
                        format!("\"{escaped}\"")
                    }
                    Value::Path(p) => format!("\"{p}\""),
                    other => {
                        return Err(EvalError::NotImplemented(format!(
                            "scopedImport: cannot render scope value of type {} as literal",
                            other.type_name()
                        )))
                    }
                };
                out.push_str(&format!(" {k} = {rhs};"));
            }
            out.push_str(" }");
            Ok(out)
        }
        let scope_src = render_scope_attrs(&scope)?;
        let wrapped = format!("with {scope_src}; ({source})");
        let path_buf = std::path::PathBuf::from(&path);
        let _guard = crate::eval::push_eval_file(path_buf.clone());
        crate::eval::eval_with_file(&wrapped, Some(path_buf))
    });

    // import
    register_builtin(builtins, "import", |args| {
        crate::perf::inc(crate::perf::Counter::Import);
        let raw_path = args[0].coerce_to_path("import")?;
        let resolved = crate::path::resolve_import(
            crate::eval::current_eval_dir().as_deref(),
            &raw_path,
        ).unwrap_or_else(|_| std::path::PathBuf::from(&raw_path));
        let path = resolved.to_string_lossy().into_owned();

        let canonical = crate::path::normalize(std::path::Path::new(&path));

        let cached = IMPORT_CACHE.with(|c| c.borrow().get(&canonical).cloned());
        if let Some(value) = cached {
            crate::perf::inc(crate::perf::Counter::ImportHit);
            return Ok(value);
        }

        let source = std::fs::read_to_string(&path)
            .map_err(|e| EvalError::IoError { context: format!("import {path}"), message: e.to_string() })?;
        let path_buf = std::path::PathBuf::from(&path);
        let _guard = crate::eval::push_eval_file(path_buf.clone());
        let value = crate::eval::eval_with_file(&source, Some(path_buf))?;

        IMPORT_CACHE.with(|c| c.borrow_mut().insert(canonical, value.clone()));

        Ok(value)
    });

    // unsafeGetAttrPos
    register_curried(builtins, "unsafeGetAttrPos", |_name, _set| {
        Ok(Value::Null)
    });

    // findFile (curried)
    register_curried(builtins, "findFile", |search_path, name_val| {
        let entries = search_path.as_list()?;
        let name = name_val.as_string()?;
        for entry in entries {
            let entry = crate::eval::force_value(entry)?;
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
                    return Ok(Value::Path(Box::new(SmolStr::from(full_path.as_str()))));
                }
            }
        }
        Err(EvalError::TypeError(format!("findFile: file '{name}' not found in search path")))
    });

    // toFile (curried)
    register_curried(builtins, "toFile", |name_val, content_val| {
        let name = name_val.as_string()?;
        let content = content_val.as_string()?;
        use sha2::{Sha256, Digest};
        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
        let store_path = format!("/nix/store/{}-{}", &hash[..32], name);
        Ok(Value::Path(Box::new(SmolStr::from(store_path.as_str()))))
    });
}
