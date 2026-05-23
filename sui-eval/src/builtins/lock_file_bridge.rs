//! Bridge from `builtins.sui.lockFile.*` to `sui_spec::lock_file`.

use std::rc::Rc;

use super::*;
use super::bridge_helpers::{as_string};
use sui_spec::lock_file;

const NAME: &str = "builtins.sui.lockFile";

pub(crate) fn register(sui_ext: &mut NixAttrs) {
    let mut set = NixAttrs::new();

    register_builtin(&mut set, "parse", |args| {
        let text = as_string(&args[0], &format!("{NAME}.parse"))?;
        let fmt = lock_file::load_canonical()
            .map_err(|e| EvalError::type_error(format!("{NAME}.parse: load: {e:?}")))?
            .into_iter()
            .find(|f| f.name == "cppnix-flake-lock-v7")
            .ok_or_else(|| EvalError::type_error(format!(
                "{NAME}.parse: missing cppnix-flake-lock-v7 format",
            )))?;
        let parsed = lock_file::parse(&text, &fmt)
            .map_err(|e| EvalError::type_error(format!("{NAME}.parse: {e:?}")))?;

        let mut out = NixAttrs::new();
        out.insert("version".to_string(), Value::Int(parsed.version as i64));
        out.insert("root".to_string(), Value::string(parsed.root));
        let mut nodes = NixAttrs::new();
        for (k, v) in parsed.nodes {
            nodes.insert(k, json_to_value(&v));
        }
        out.insert("nodes".to_string(), Value::Attrs(Rc::new(nodes)));
        Ok(Value::Attrs(Rc::new(out)))
    });

    register_builtin(&mut set, "rootInputs", |args| {
        let text = as_string(&args[0], &format!("{NAME}.rootInputs"))?;
        let fmt = lock_file::load_canonical()
            .map_err(|e| EvalError::type_error(format!("{NAME}.rootInputs: load: {e:?}")))?
            .into_iter()
            .find(|f| f.name == "cppnix-flake-lock-v7")
            .ok_or_else(|| EvalError::type_error(format!(
                "{NAME}.rootInputs: missing cppnix-flake-lock-v7 format",
            )))?;
        let parsed = lock_file::parse(&text, &fmt)
            .map_err(|e| EvalError::type_error(format!("{NAME}.rootInputs: {e:?}")))?;
        let inputs = lock_file::root_inputs(&parsed)
            .map_err(|e| EvalError::type_error(format!("{NAME}.rootInputs: {e:?}")))?;
        Ok(Value::list(inputs.into_iter().map(Value::string).collect()))
    });

    sui_ext.insert("lockFile".to_string(), Value::Attrs(Rc::new(set)));
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "version": 7,
        "root": "root",
        "nodes": {
            "root": { "inputs": { "nixpkgs": "nixpkgs" } },
            "nixpkgs": { "locked": { "narHash": "sha256:abc" } }
        }
    }"#;

    fn call_parse(text: &str) -> Result<Value, EvalError> {
        let bridge = format!("{NAME}.parse");
        let text = as_string(&Value::string(text), &bridge)?;
        let fmt = lock_file::load_canonical().unwrap().into_iter()
            .find(|f| f.name == "cppnix-flake-lock-v7").unwrap();
        let parsed = lock_file::parse(&text, &fmt)
            .map_err(|e| EvalError::type_error(format!("{bridge}: {e:?}")))?;
        let mut out = NixAttrs::new();
        out.insert("version".to_string(), Value::Int(parsed.version as i64));
        out.insert("root".to_string(), Value::string(parsed.root));
        let mut nodes = NixAttrs::new();
        for (k, v) in parsed.nodes {
            nodes.insert(k, json_to_value(&v));
        }
        out.insert("nodes".to_string(), Value::Attrs(Rc::new(nodes)));
        Ok(Value::Attrs(Rc::new(out)))
    }

    #[test]
    fn parse_returns_typed_record() {
        let result = call_parse(SAMPLE).unwrap();
        let attrs = match result {
            Value::Attrs(a) => a,
            _ => panic!("expected attrs"),
        };
        match attrs.get("version") {
            Some(Value::Int(7)) => {}
            other => panic!("expected version=7, got {other:?}"),
        }
        match attrs.get("root") {
            Some(Value::String(s)) => assert_eq!(s.chars.as_str(), "root"),
            _ => panic!("expected root string"),
        }
    }

    #[test]
    fn parse_garbage_errors() {
        let err = call_parse("not json").unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("lockFile.parse"));
    }
}
