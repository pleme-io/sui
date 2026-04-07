//! MVP delegation to the real `nix` binary.
//!
//! For complex evaluation scenarios (e.g., full nixpkgs import), sui can
//! fall back to running `nix eval --json` and parsing the JSON output back
//! into a sui `Value`. This enables incremental adoption — users get a
//! working evaluator immediately even for expressions that touch builtins
//! or language features sui has not yet implemented.

use std::path::Path;
use std::process::Command;

use crate::value::{EvalError, NixAttrs, Value};

/// Evaluate a flake attribute path using the real `nix` CLI as a fallback.
///
/// Constructs `nix eval --json <flake_dir>#<attr_path>` and parses the JSON
/// result back into a sui `Value`.
///
/// # Errors
///
/// Returns `EvalError::IoError` if the nix binary cannot be spawned, and
/// `EvalError::Throw` if the evaluation fails.
pub fn eval_with_nix(flake_dir: &Path, attr_path: &str) -> Result<Value, EvalError> {
    let flake_ref = format!("{}#{attr_path}", flake_dir.display());

    let output = Command::new("nix")
        .args(["eval", "--json", &flake_ref])
        .output()
        .map_err(|e| EvalError::IoError {
            context: "nix eval delegation".into(),
            message: e.to_string(),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(EvalError::Throw(format!("nix eval failed: {stderr}")));
    }

    let json_str = String::from_utf8_lossy(&output.stdout);
    let json_val: serde_json::Value = serde_json::from_str(&json_str).map_err(|e| {
        EvalError::ParseError(format!("nix eval JSON parse: {e}"))
    })?;

    json_to_value(&json_val)
}

/// Evaluate a raw Nix expression using the real `nix` CLI.
///
/// Runs `nix eval --json --expr <expr>` and parses the result.
pub fn eval_expr_with_nix(expr: &str) -> Result<Value, EvalError> {
    let output = Command::new("nix")
        .args(["eval", "--json", "--expr", expr])
        .output()
        .map_err(|e| EvalError::IoError {
            context: "nix eval --expr delegation".into(),
            message: e.to_string(),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(EvalError::Throw(format!("nix eval --expr failed: {stderr}")));
    }

    let json_str = String::from_utf8_lossy(&output.stdout);
    let json_val: serde_json::Value = serde_json::from_str(&json_str).map_err(|e| {
        EvalError::ParseError(format!("nix eval JSON parse: {e}"))
    })?;

    json_to_value(&json_val)
}

/// Convert a `serde_json::Value` back to a sui `Value`.
///
/// This is the inverse of the serialization path — it reconstructs the
/// evaluator's value types from JSON so the rest of the evaluator can
/// consume the result transparently.
pub fn json_to_value(json: &serde_json::Value) -> Result<Value, EvalError> {
    match json {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(b) => Ok(Value::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Int(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Float(f))
            } else {
                Err(EvalError::TypeError(format!(
                    "cannot convert JSON number: {n}"
                )))
            }
        }
        serde_json::Value::String(s) => Ok(Value::string(s.clone())),
        serde_json::Value::Array(arr) => {
            let vals: Result<Vec<_>, _> = arr.iter().map(json_to_value).collect();
            Ok(Value::List(vals?))
        }
        serde_json::Value::Object(map) => {
            let mut attrs = NixAttrs::new();
            for (k, v) in map {
                attrs.insert(k.clone(), json_to_value(v)?);
            }
            Ok(Value::Attrs(attrs))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── json_to_value ─────────────────────────────────────

    #[test]
    fn json_null_to_value() {
        let v = json_to_value(&serde_json::Value::Null).unwrap();
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn json_bool_to_value() {
        let v = json_to_value(&serde_json::json!(true)).unwrap();
        assert_eq!(v, Value::Bool(true));
    }

    #[test]
    fn json_int_to_value() {
        let v = json_to_value(&serde_json::json!(42)).unwrap();
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn json_float_to_value() {
        let v = json_to_value(&serde_json::json!(3.14)).unwrap();
        assert_eq!(v, Value::Float(3.14));
    }

    #[test]
    fn json_string_to_value() {
        let v = json_to_value(&serde_json::json!("hello")).unwrap();
        assert_eq!(v, Value::string("hello"));
    }

    #[test]
    fn json_array_to_value() {
        let v = json_to_value(&serde_json::json!([1, "two", true])).unwrap();
        if let Value::List(items) = v {
            assert_eq!(items.len(), 3);
            assert_eq!(items[0], Value::Int(1));
            assert_eq!(items[1], Value::string("two"));
            assert_eq!(items[2], Value::Bool(true));
        } else {
            panic!("expected List, got {v:?}");
        }
    }

    #[test]
    fn json_object_to_value() {
        let v = json_to_value(&serde_json::json!({"a": 1, "b": "two"})).unwrap();
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.len(), 2);
            assert_eq!(attrs.get("a").unwrap(), &Value::Int(1));
            assert_eq!(attrs.get("b").unwrap(), &Value::string("two"));
        } else {
            panic!("expected Attrs, got {v:?}");
        }
    }

    #[test]
    fn json_nested_object_to_value() {
        let v = json_to_value(&serde_json::json!({
            "outer": {
                "inner": [1, 2, 3]
            }
        }))
        .unwrap();
        if let Value::Attrs(outer) = v {
            if let Value::Attrs(inner) = outer.get("outer").unwrap() {
                if let Value::List(list) = inner.get("inner").unwrap() {
                    assert_eq!(list.len(), 3);
                } else {
                    panic!("expected inner list");
                }
            } else {
                panic!("expected inner attrs");
            }
        } else {
            panic!("expected outer attrs");
        }
    }

    #[test]
    fn json_empty_array_to_value() {
        let v = json_to_value(&serde_json::json!([])).unwrap();
        assert_eq!(v, Value::List(vec![]));
    }

    #[test]
    fn json_empty_object_to_value() {
        let v = json_to_value(&serde_json::json!({})).unwrap();
        if let Value::Attrs(attrs) = v {
            assert!(attrs.is_empty());
        } else {
            panic!("expected empty Attrs");
        }
    }

    #[test]
    fn json_negative_int_to_value() {
        let v = json_to_value(&serde_json::json!(-7)).unwrap();
        assert_eq!(v, Value::Int(-7));
    }

    // ── eval_with_nix (gated behind SUI_TEST_ONLINE) ─────

    #[test]
    fn eval_with_nix_requires_nix_binary() {
        if std::env::var("SUI_TEST_ONLINE").is_err() {
            eprintln!("skipping: SUI_TEST_ONLINE not set");
            return;
        }

        // Create a minimal flake to evaluate.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("flake.nix"),
            r#"{ outputs = { self }: { value = 42; }; }"#,
        )
        .unwrap();

        let result = eval_with_nix(dir.path(), "value");
        match result {
            Ok(Value::Int(42)) => {} // expected
            Ok(v) => panic!("expected Int(42), got {v:?}"),
            Err(e) => panic!("eval_with_nix failed: {e}"),
        }
    }

    #[test]
    fn eval_expr_with_nix_simple() {
        if std::env::var("SUI_TEST_ONLINE").is_err() {
            eprintln!("skipping: SUI_TEST_ONLINE not set");
            return;
        }

        let result = eval_expr_with_nix("1 + 2");
        match result {
            Ok(Value::Int(3)) => {}
            Ok(v) => panic!("expected Int(3), got {v:?}"),
            Err(e) => panic!("eval_expr_with_nix failed: {e}"),
        }
    }
}
