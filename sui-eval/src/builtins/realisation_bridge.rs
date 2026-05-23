//! Bridge from `builtins.sui.realisation.parse` to `sui_spec::realisation`.

use std::rc::Rc;

use super::*;
use super::bridge_helpers::as_string;
use sui_spec::realisation;

const NAME: &str = "builtins.sui.realisation";

pub(crate) fn register(sui_ext: &mut NixAttrs) {
    let mut set = NixAttrs::new();

    register_builtin(&mut set, "parse", |args| {
        let text = as_string(&args[0], &format!("{NAME}.parse"))?;
        let fmt = realisation::load_canonical()
            .map_err(|e| EvalError::type_error(format!("{NAME}.parse: load: {e:?}")))?
            .into_iter()
            .find(|f| f.name == "cppnix-realisation-v1")
            .ok_or_else(|| EvalError::type_error(format!(
                "{NAME}.parse: missing cppnix-realisation-v1 format",
            )))?;
        let parsed = realisation::parse(&text, &fmt)
            .map_err(|e| EvalError::type_error(format!("{NAME}.parse: {e:?}")))?;

        let mut out = NixAttrs::new();
        out.insert("id".to_string(), Value::string(parsed.id));
        out.insert("outPath".to_string(), Value::string(parsed.out_path));
        out.insert("signatures".to_string(), Value::list(
            parsed.signatures.into_iter().map(Value::string).collect(),
        ));
        out.insert("dependentRealisations".to_string(), Value::list(
            parsed.dependent_realisations.into_iter().map(Value::string).collect(),
        ));
        Ok(Value::Attrs(Rc::new(out)))
    });

    sui_ext.insert("realisation".to_string(), Value::Attrs(Rc::new(set)));
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "id": "sha256:abc!out",
        "outPath": "/nix/store/abc-hello",
        "signatures": ["cache.nixos.org-1:sig"],
        "dependentRealisations": []
    }"#;

    #[test]
    fn parse_returns_typed_record() {
        let fmt = realisation::load_canonical().unwrap().into_iter()
            .find(|f| f.name == "cppnix-realisation-v1").unwrap();
        let parsed = realisation::parse(SAMPLE, &fmt).unwrap();
        assert_eq!(parsed.id, "sha256:abc!out");
        assert_eq!(parsed.out_path, "/nix/store/abc-hello");
        assert_eq!(parsed.signatures.len(), 1);
        assert!(parsed.dependent_realisations.is_empty());
    }
}
