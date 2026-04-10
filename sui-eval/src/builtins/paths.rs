//! Path builtins: baseNameOf, dirOf, toPath, storePath, pathExists, readFile,
//! readDir, readFileType, path, filterSource.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    // baseNameOf — extract filename from path
    register_builtin(builtins, "baseNameOf", |args| {
        let s = match &args[0] {
            Value::String(ns) => ns.chars.to_string(),
            Value::Path(p) => p.to_string(),
            _ => return Err(EvalError::TypeError("baseNameOf: expected string or path".to_string())),
        };
        let base = s.rsplit('/').next().unwrap_or(&s);
        Ok(Value::string(base))
    });

    // dirOf — extract directory from path
    register_builtin(builtins, "dirOf", |args| {
        let (s, is_path) = match &args[0] {
            Value::String(ns) => (ns.chars.to_string(), false),
            Value::Path(p) => (p.to_string(), true),
            _ => return Err(EvalError::TypeError("dirOf: expected string or path".to_string())),
        };
        let dir = match s.rfind('/') {
            Some(0) => "/".to_string(),
            Some(i) => s[..i].to_string(),
            None => ".".to_string(),
        };
        if is_path {
            Ok(Value::Path(Box::new(SmolStr::from(dir.as_str()))))
        } else {
            Ok(Value::string(dir))
        }
    });

    // readFile — read file contents to string
    register_builtin(builtins, "readFile", |args| {
        let path = args[0].coerce_to_path("readFile")?;
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| EvalError::IoError { context: "readFile".into(), message: e.to_string() })?;
        Ok(Value::string(contents))
    });

    register_builtin(builtins, "readFileType", |args| {
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

    register_builtin(builtins, "readDir", |args| {
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
        Ok(Value::Attrs(Box::new(attrs)))
    });

    register_builtin(builtins, "toPath", |args| {
        let s = args[0].as_string()?;
        if !s.starts_with('/') {
            return Err(EvalError::TypeError(format!("toPath: path must be absolute: {s}")));
        }
        Ok(Value::Path(Box::new(SmolStr::from(s))))
    });

    register_builtin(builtins, "storePath", |args| {
        let s = args[0].as_string()?;
        if !s.starts_with("/nix/store/") {
            return Err(EvalError::TypeError(format!("storePath: not a store path: {s}")));
        }
        Ok(Value::Path(Box::new(SmolStr::from(s))))
    });

    // pathExists
    register_builtin(builtins, "pathExists", |args| {
        let path_str = args[0].coerce_to_path("pathExists")?;
        Ok(Value::Bool(std::path::Path::new(&path_str).exists()))
    });

    // builtins.path { path; name?; sha256?; recursive?; }
    register_builtin(builtins, "path", |args| {
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
            if expected_str != actual {
                return Err(EvalError::TypeError(format!(
                    "path: sha256 mismatch: expected {expected_str}, got {actual}"
                )));
            }
        }
        let hash = format!("{:x}", hasher.finalize());
        let store_path = format!("/nix/store/{}-{}", &hash[..32], name);
        Ok(Value::Path(Box::new(SmolStr::from(store_path.as_str()))))
    });

    // filterSource
    register_curried(builtins, "filterSource", |pred, src| {
        let src_path = src.coerce_to_path("filterSource")?;
        let src_path_buf = std::path::PathBuf::from(&src_path);
        if !src_path_buf.exists() {
            return Err(EvalError::IoError {
                context: format!("filterSource: {src_path}"),
                message: "no such file or directory".into(),
            });
        }
        let name = src_path_buf
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "source".into());
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        let pred_clone = pred.clone();
        fn walk_filter(
            base: &std::path::Path,
            current: &std::path::Path,
            pred: &Value,
            hasher: &mut sha2::Sha256,
            kept: &mut Vec<std::path::PathBuf>,
        ) -> Result<(), EvalError> {
            let metadata = std::fs::symlink_metadata(current).map_err(|e| EvalError::IoError {
                context: format!("filterSource: {}", current.display()),
                message: e.to_string(),
            })?;
            let kind = if metadata.is_dir() {
                "directory"
            } else if metadata.is_symlink() {
                "symlink"
            } else {
                "regular"
            };
            let path_arg = Value::string(current.to_string_lossy().to_string());
            let kind_arg = Value::string(kind);
            let partial = crate::eval::apply(pred.clone(), path_arg)?;
            let keep = crate::eval::apply(partial, kind_arg)?.as_bool()?;
            if !keep {
                return Ok(());
            }
            let rel = current.strip_prefix(base).unwrap_or(current);
            hasher.update(rel.to_string_lossy().as_bytes());
            hasher.update([0u8]);
            kept.push(current.to_path_buf());
            if metadata.is_dir() {
                let entries =
                    std::fs::read_dir(current).map_err(|e| EvalError::IoError {
                        context: format!("filterSource: {}", current.display()),
                        message: e.to_string(),
                    })?;
                let mut sorted: Vec<_> = entries.flatten().map(|e| e.path()).collect();
                sorted.sort();
                for child in sorted {
                    walk_filter(base, &child, pred, hasher, kept)?;
                }
            }
            Ok(())
        }
        let mut kept_paths: Vec<std::path::PathBuf> = Vec::new();
        walk_filter(&src_path_buf, &src_path_buf, &pred_clone, &mut hasher, &mut kept_paths)?;
        let hash = format!("{:x}", hasher.finalize());
        let target = std::env::temp_dir()
            .join("sui-filterSource")
            .join(format!("{hash}-{name}"));
        if !target.exists() {
            std::fs::create_dir_all(&target).map_err(|e| EvalError::IoError {
                context: format!("filterSource: {}", target.display()),
                message: e.to_string(),
            })?;
            for kept in &kept_paths {
                let rel = kept.strip_prefix(&src_path_buf).unwrap_or(kept);
                if rel.as_os_str().is_empty() {
                    continue;
                }
                let dst = target.join(rel);
                let metadata =
                    std::fs::symlink_metadata(kept).map_err(|e| EvalError::IoError {
                        context: format!("filterSource: {}", kept.display()),
                        message: e.to_string(),
                    })?;
                if metadata.is_dir() {
                    std::fs::create_dir_all(&dst).map_err(|e| EvalError::IoError {
                        context: format!("filterSource: {}", dst.display()),
                        message: e.to_string(),
                    })?;
                } else {
                    if let Some(parent) = dst.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    std::fs::copy(kept, &dst).map_err(|e| EvalError::IoError {
                        context: format!("filterSource: {}", dst.display()),
                        message: e.to_string(),
                    })?;
                }
            }
        }
        Ok(Value::Path(Box::new(SmolStr::from(target.to_string_lossy().as_ref()))))
    });
}
