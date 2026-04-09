//! Conversion builtins: toJSON, fromJSON, fromTOML, toXML, convertHash, hashFile.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_builtin(builtins, "toJSON", |args| {
        Ok(Value::string(serde_json::to_string(&args[0].to_json())
            .unwrap_or_else(|_| "null".to_string())))
    });
    register_builtin(builtins, "fromJSON", |args| {
        let s = args[0].as_string()?;
        let json: serde_json::Value = serde_json::from_str(s)
            .map_err(|e| EvalError::TypeError(format!("fromJSON: {e}")))?;
        Ok(json_to_value(&json))
    });

    register_builtin(builtins, "fromTOML", |args| {
        let s = args[0].as_string()?;
        let table: toml::Value = toml::from_str(s)
            .map_err(|e| EvalError::TypeError(format!("fromTOML: {e}")))?;
        Ok(toml_to_value(&table))
    });

    // convertHash
    register_builtin(builtins, "convertHash", |args| {
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
        let bytes: Vec<u8> = if raw_hash.len() == expected_len * 2
            && raw_hash.chars().all(|c| c.is_ascii_hexdigit())
        {
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

    // hashFile (curried)
    register_curried(builtins, "hashFile", |algo, path_val| {
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

    // toXML — convert value to XML representation
    register_builtin(builtins, "toXML", |args| {
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
}
