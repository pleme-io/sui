//! Sui-specific extension builtins: blake3, sha3, fromYAML, toYAML, fromCSV,
//! regexNamedCaptures, timestamp, fileSize, fileMtime.

use super::*;

pub(crate) fn register(sui_ext: &mut NixAttrs) {
    // Hash algorithms — blake3, sha3-256, sha3-512.
    register_builtin(sui_ext, "blake3", |args| {
        let s = args[0].as_string()?;
        Ok(Value::string(blake3::hash(s.as_bytes()).to_hex().to_string()))
    });
    register_builtin(sui_ext, "sha3_256", |args| {
        use sha3::{Digest, Sha3_256};
        let s = args[0].as_string()?;
        Ok(Value::string(format!("{:x}", Sha3_256::digest(s.as_bytes()))))
    });
    register_builtin(sui_ext, "sha3_512", |args| {
        use sha3::{Digest, Sha3_512};
        let s = args[0].as_string()?;
        Ok(Value::string(format!("{:x}", Sha3_512::digest(s.as_bytes()))))
    });

    // YAML round-trip
    register_builtin(sui_ext, "fromYAML", |args| {
        let s = args[0].as_string()?;
        let y: serde_yaml_ng::Value = serde_yaml_ng::from_str(s).map_err(|e| {
            EvalError::TypeError(format!("sui.fromYAML: {e}"))
        })?;
        let j = serde_json::to_value(&y).map_err(|e| {
            EvalError::TypeError(format!("sui.fromYAML: yaml->json: {e}"))
        })?;
        Ok(json_to_value(&j))
    });
    register_builtin(sui_ext, "toYAML", |args| {
        let j = args[0].to_json();
        let y: serde_yaml_ng::Value = serde_yaml_ng::from_value(
            serde_yaml_ng::to_value(&j).map_err(|e| {
                EvalError::TypeError(format!("sui.toYAML: json->yaml: {e}"))
            })?,
        )
        .map_err(|e| EvalError::TypeError(format!("sui.toYAML: {e}")))?;
        let out = serde_yaml_ng::to_string(&y).map_err(|e| {
            EvalError::TypeError(format!("sui.toYAML: serialize: {e}"))
        })?;
        Ok(Value::string(out))
    });

    // CSV -> list of attrs (or list of lists when no header).
    register_curried(sui_ext, "fromCSV", |csv_val, opts_val| {
        let csv = csv_val.as_string()?;
        let opts = opts_val.to_attrs()?;
        let has_header = opts
            .get("hasHeader")
            .and_then(|v| crate::eval::force_value(v).ok())
            .and_then(|v| match v {
                Value::Bool(b) => Some(b),
                _ => None,
            })
            .unwrap_or(true);
        let delimiter = opts
            .get("delimiter")
            .and_then(|v| crate::eval::force_value(v).ok())
            .and_then(|v| match v {
                Value::String(ns) => Some(ns.chars.clone()),
                _ => None,
            })
            .map(|s| s.chars().next().unwrap_or(','))
            .unwrap_or(',');
        let mut lines = csv.lines();
        if has_header {
            let header_line = lines
                .next()
                .ok_or_else(|| EvalError::TypeError("sui.fromCSV: empty input".into()))?;
            let headers: Vec<&str> = header_line.split(delimiter).collect();
            let mut rows: Vec<Value> = Vec::new();
            for line in lines {
                if line.is_empty() {
                    continue;
                }
                let cells: Vec<&str> = line.split(delimiter).collect();
                let mut a = NixAttrs::new();
                for (i, h) in headers.iter().enumerate() {
                    let v = cells.get(i).copied().unwrap_or("");
                    a.insert((*h).to_string(), Value::string(v));
                }
                rows.push(Value::Attrs(Rc::new(a)));
            }
            Ok(Value::List(Rc::new(rows)))
        } else {
            let mut rows: Vec<Value> = Vec::new();
            for line in lines {
                if line.is_empty() {
                    continue;
                }
                let cells: Vec<Value> = line
                    .split(delimiter)
                    .map(Value::string)
                    .collect();
                rows.push(Value::List(Rc::new(cells)));
            }
            Ok(Value::List(Rc::new(rows)))
        }
    });

    // Regex named captures
    register_curried(sui_ext, "regexNamedCaptures", |pat, subj| {
        let p = pat.as_string()?;
        let s = subj.as_string()?;
        let re = regex::Regex::new(p)
            .map_err(|e| EvalError::TypeError(format!("sui.regexNamedCaptures: {e}")))?;
        let Some(caps) = re.captures(s) else {
            return Ok(Value::Null);
        };
        let mut out = NixAttrs::new();
        for name in re.capture_names().flatten() {
            if let Some(m) = caps.name(name) {
                out.insert(name.to_string(), Value::string(m.as_str()));
            }
        }
        Ok(Value::Attrs(Rc::new(out)))
    });

    // ISO-8601 timestamp
    register_builtin(sui_ext, "timestamp", |_args| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let date = format_unix_yyyymmddhhmmss(now);
        if date.len() == 14 {
            Ok(Value::string(format!(
                "{}-{}-{}T{}:{}:{}Z",
                &date[0..4],
                &date[4..6],
                &date[6..8],
                &date[8..10],
                &date[10..12],
                &date[12..14],
            )))
        } else {
            Ok(Value::string(date))
        }
    });

    // File metadata helpers
    register_builtin(sui_ext, "fileSize", |args| {
        let path = args[0].coerce_to_path("sui.fileSize")?;
        let metadata = std::fs::metadata(&path).map_err(|e| EvalError::IoError {
            context: format!("sui.fileSize: {path}"),
            message: e.to_string(),
        })?;
        Ok(Value::Int(metadata.len() as i64))
    });
    register_builtin(sui_ext, "fileMtime", |args| {
        let path = args[0].coerce_to_path("sui.fileMtime")?;
        let metadata = std::fs::metadata(&path).map_err(|e| EvalError::IoError {
            context: format!("sui.fileMtime: {path}"),
            message: e.to_string(),
        })?;
        let mtime = metadata
            .modified()
            .map_err(|e| EvalError::IoError {
                context: format!("sui.fileMtime: {path}"),
                message: e.to_string(),
            })?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Ok(Value::Int(mtime))
    });
}
