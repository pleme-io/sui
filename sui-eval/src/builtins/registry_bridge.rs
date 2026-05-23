//! Bridge from `builtins.sui.registry.resolve` to `sui_spec::registry`.

use std::rc::Rc;

use super::*;
use super::bridge_helpers::{as_string, attrs_required_string};
use sui_spec::registry::{self, RegistryEntry, RegistryScope};

const NAME: &str = "builtins.sui.registry";

pub(crate) fn register(sui_ext: &mut NixAttrs) {
    let mut set = NixAttrs::new();

    // `resolve { scopes, ref }` where:
    //   scopes = { flakeLocal = [ {from,to,exact?} ... ];
    //              user = [ ... ]; system = [ ... ]; global = [ ... ]; };
    //   ref    = "nixpkgs"  (the flake-ref to look up)
    register_builtin(&mut set, "resolve", |args| {
        let bridge = format!("{NAME}.resolve");
        let attrs = match crate::eval::force_value(&args[0])? {
            Value::Attrs(a) => a,
            other => return Err(EvalError::type_error(format!(
                "{bridge}: expected attrset, got {}",
                other.type_name(),
            ))),
        };
        let flake_ref = attrs_required_string(&attrs, "ref", &bridge)?;
        let scopes_val = attrs.get("scopes").ok_or_else(|| EvalError::type_error(format!(
            "{bridge}: missing required field `scopes`",
        )))?;
        let scopes_attrs = match crate::eval::force_value(scopes_val)? {
            Value::Attrs(a) => a,
            other => return Err(EvalError::type_error(format!(
                "{bridge}: `scopes` must be an attrset, got {}",
                other.type_name(),
            ))),
        };
        let registries = parse_scopes(&scopes_attrs, &bridge)?;
        let entry = registry::resolve(&registries, &flake_ref).map_err(|e| {
            EvalError::type_error(format!("{bridge}: {e:?}"))
        })?;
        Ok(entry_to_value(&entry))
    });

    sui_ext.insert("registry".to_string(), Value::Attrs(Rc::new(set)));
}

fn parse_scopes(
    attrs: &NixAttrs,
    bridge: &str,
) -> Result<Vec<(RegistryScope, Vec<RegistryEntry>)>, EvalError> {
    let mut out: Vec<(RegistryScope, Vec<RegistryEntry>)> = Vec::new();
    for (key, scope) in [
        ("flakeLocal", RegistryScope::FlakeLocal),
        ("user", RegistryScope::User),
        ("system", RegistryScope::System),
        ("global", RegistryScope::Global),
    ] {
        let Some(v) = attrs.get(key) else { continue; };
        let list = match crate::eval::force_value(v)? {
            Value::List(l) => l,
            other => return Err(EvalError::type_error(format!(
                "{bridge}: `scopes.{key}` must be a list, got {}",
                other.type_name(),
            ))),
        };
        let mut entries = Vec::with_capacity(list.len());
        for item in list.iter() {
            let entry_attrs = match crate::eval::force_value(item)? {
                Value::Attrs(a) => a,
                other => return Err(EvalError::type_error(format!(
                    "{bridge}: every entry in scopes.{key} must be an attrset, got {}",
                    other.type_name(),
                ))),
            };
            let from = attrs_required_string(&entry_attrs, "from", bridge)?;
            let to = attrs_required_string(&entry_attrs, "to", bridge)?;
            let exact = match entry_attrs.get("exact") {
                Some(v) => match crate::eval::force_value(v)? {
                    Value::Bool(b) => b,
                    _ => false,
                },
                None => false,
            };
            entries.push(RegistryEntry { from, to, exact });
        }
        out.push((scope, entries));
    }
    Ok(out)
}

fn entry_to_value(entry: &RegistryEntry) -> Value {
    let mut out = NixAttrs::new();
    out.insert("from".to_string(), Value::string(&entry.from));
    out.insert("to".to_string(), Value::string(&entry.to));
    out.insert("exact".to_string(), Value::Bool(entry.exact));
    Value::Attrs(Rc::new(out))
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

    fn entry(from: &str, to: &str) -> Value {
        attrs_of(&[
            ("from", Value::string(from)),
            ("to", Value::string(to)),
        ])
    }

    fn _list(items: Vec<Value>) -> Value {
        Value::List(Rc::new(items))
    }

    #[test]
    fn parse_scopes_lowest_precedence_wins() {
        let scopes = attrs_of(&[
            ("global", _list(vec![entry("nixpkgs", "github:NixOS/nixpkgs/global")])),
            ("flakeLocal", _list(vec![entry("nixpkgs", "github:NixOS/nixpkgs/local")])),
        ]);
        let attrs = match scopes {
            Value::Attrs(a) => a,
            _ => panic!(),
        };
        let registries = parse_scopes(&attrs, "test").unwrap();
        let resolved = registry::resolve(&registries, "nixpkgs").unwrap();
        // FlakeLocal precedence wins.
        assert_eq!(resolved.to, "github:NixOS/nixpkgs/local");
    }

    #[test]
    fn missing_ref_field_errors() {
        let scopes = attrs_of(&[]);
        let attrs = match scopes {
            Value::Attrs(a) => a,
            _ => panic!(),
        };
        let err = attrs_required_string(&attrs, "ref", "test").unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("`ref`"));
    }
}
