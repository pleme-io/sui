//! Bridge between sui-eval's `Value` and `sui_spec::hash`.
//!
//! Exposes `builtins.sui.hash` namespace with:
//!
//! - `builtins.sui.hash.convert { from, to, input }` — re-encode
//!   a hash from one encoding to another (base16 / nix-base32 /
//!   base64 / sri).
//! - `builtins.sui.hash.decode "<input>"` — auto-detect the
//!   encoding and return `{ algorithm, hex }`.
//!
//! Pure functions — no IO env needed.

use std::rc::Rc;

use super::*;
use sui_spec::hash;

pub(crate) fn register(sui_ext: &mut NixAttrs) {
    // Build a sub-attrset and insert as `builtins.sui.hash`.
    let mut hash_set = NixAttrs::new();

    register_builtin(&mut hash_set, "convert", |args| {
        let arg = crate::eval::force_value(&args[0])?;
        let attrs = match arg {
            Value::Attrs(a) => a,
            other => {
                return Err(EvalError::type_error(format!(
                    "builtins.sui.hash.convert: expected attrset, got {}",
                    other.type_name(),
                )));
            }
        };
        let from = attrs_string(&attrs, "from")?;
        let to = attrs_string(&attrs, "to")?;
        let input = attrs_string(&attrs, "input")?;
        let out = hash::apply_conversion(&from, &to, &input).map_err(|e| {
            EvalError::type_error(format!("builtins.sui.hash.convert: {e:?}"))
        })?;
        Ok(Value::string(out))
    });

    register_builtin(&mut hash_set, "decode", |args| {
        let input = crate::eval::force_value(&args[0])?;
        let s = match input {
            Value::String(ns) => ns.chars.to_string(),
            other => {
                return Err(EvalError::type_error(format!(
                    "builtins.sui.hash.decode: expected string, got {}",
                    other.type_name(),
                )));
            }
        };
        let (algo, bytes) = hash::decode_hash(&s).map_err(|e| {
            EvalError::type_error(format!("builtins.sui.hash.decode: {e:?}"))
        })?;
        let mut out = NixAttrs::new();
        out.insert("algorithm".to_string(), Value::string(algo));
        let mut hex = String::with_capacity(bytes.len() * 2);
        for b in &bytes {
            hex.push_str(&format!("{b:02x}"));
        }
        out.insert("hex".to_string(), Value::string(hex));
        Ok(Value::Attrs(Rc::new(out)))
    });

    sui_ext.insert("hash".to_string(), Value::Attrs(Rc::new(hash_set)));
}

fn attrs_string(attrs: &NixAttrs, key: &str) -> Result<String, EvalError> {
    let v = attrs.get(key).ok_or_else(|| EvalError::type_error(format!(
        "missing required field `{key}`",
    )))?;
    match crate::eval::force_value(v)? {
        Value::String(s) => Ok(s.chars.to_string()),
        other => Err(EvalError::type_error(format!(
            "field `{key}` must be a string, got {}",
            other.type_name(),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs_of(pairs: &[(&str, Value)]) -> Value {
        let mut a = NixAttrs::new();
        for (k, v) in pairs {
            a.insert(k.to_string(), v.clone());
        }
        Value::Attrs(Rc::new(a))
    }

    fn call_convert(arg: Value) -> Result<Value, EvalError> {
        // Re-implement the closure logic to test directly.
        let forced = crate::eval::force_value(&arg)?;
        let attrs = match forced {
            Value::Attrs(a) => a,
            _ => panic!("test must pass attrs"),
        };
        let from = attrs_string(&attrs, "from")?;
        let to = attrs_string(&attrs, "to")?;
        let input = attrs_string(&attrs, "input")?;
        let out = hash::apply_conversion(&from, &to, &input).map_err(|e| {
            EvalError::type_error(format!("{e:?}"))
        })?;
        Ok(Value::string(out))
    }

    fn call_decode(s: &str) -> Result<Value, EvalError> {
        let (algo, bytes) = hash::decode_hash(s).map_err(|e| {
            EvalError::type_error(format!("{e:?}"))
        })?;
        let mut out = NixAttrs::new();
        out.insert("algorithm".to_string(), Value::string(algo));
        let mut hex = String::with_capacity(bytes.len() * 2);
        for b in &bytes {
            hex.push_str(&format!("{b:02x}"));
        }
        out.insert("hex".to_string(), Value::string(hex));
        Ok(Value::Attrs(Rc::new(out)))
    }

    #[test]
    fn convert_hex_to_sri() {
        let arg = attrs_of(&[
            ("from", Value::string("base16")),
            ("to", Value::string("sri")),
            ("input", Value::string("sha256:deadbeef")),
        ]);
        let result = call_convert(arg).unwrap();
        match result {
            Value::String(s) => assert!(s.chars.starts_with("sha256-")),
            _ => panic!("expected sri string"),
        }
    }

    #[test]
    fn decode_returns_typed_record() {
        let result = call_decode("sha256:deadbeef").unwrap();
        let attrs = match result {
            Value::Attrs(a) => a,
            _ => panic!("expected attrs"),
        };
        match attrs.get("algorithm") {
            Some(Value::String(s)) => assert_eq!(s.chars.as_str(), "sha256"),
            other => panic!("expected algorithm field, got {other:?}"),
        }
        match attrs.get("hex") {
            Some(Value::String(s)) => assert_eq!(s.chars.as_str(), "deadbeef"),
            other => panic!("expected hex field, got {other:?}"),
        }
    }
}
