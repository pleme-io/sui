//! Bridge between sui-eval's `Value` and
//! `sui_spec::activation_script::apply`.
//!
//! Exposes `builtins.sui.activationScript` — given a config +
//! host + user + target, returns the typed activation outcome
//! as a Nix attrset.  The natural sequel to evalModules: the
//! operator pipes the eval_modules output into this builtin to
//! get an activation script.

use std::collections::BTreeMap;
use std::rc::Rc;

use super::*;
use sui_spec::activation_script::{
    self, ActivationArgs, ActivationScriptAlgorithm, ActivationTarget,
};
use sui_spec::module_system::Config;

pub(crate) fn register(sui_ext: &mut NixAttrs) {
    register_builtin(sui_ext, "activationScript", |args| {
        activation_script_builtin(&args[0])
    });
}

/// Builtin implementation.  Takes a single arg: an attrset
/// shaped `{ config, host, user, target, toplevelPath ? }`.
fn activation_script_builtin(arg: &Value) -> Result<Value, EvalError> {
    let forced = crate::eval::force_value(arg)?;
    let attrs = match forced {
        Value::Attrs(a) => a,
        other => {
            return Err(EvalError::type_error(format!(
                "builtins.sui.activationScript: expected attrset, got {}",
                other.type_name(),
            )));
        }
    };

    // ── target ────────────────────────────────────────────
    let target_str = match attrs.get("target") {
        Some(v) => match crate::eval::force_value(v)? {
            Value::String(s) => s.chars.to_string(),
            other => {
                return Err(EvalError::type_error(format!(
                    "builtins.sui.activationScript.target must be a string, got {}",
                    other.type_name(),
                )));
            }
        },
        None => {
            return Err(EvalError::type_error(
                "builtins.sui.activationScript: missing `target` field \
                 (expected \"nixos\" | \"darwin\" | \"homeManager\")".to_string(),
            ));
        }
    };
    let target = match target_str.as_str() {
        "nixos" => ActivationTarget::NixOS,
        "darwin" => ActivationTarget::Darwin,
        "homeManager" | "home-manager" => ActivationTarget::HomeManager,
        other => {
            return Err(EvalError::type_error(format!(
                "builtins.sui.activationScript.target=`{other}` — \
                 expected \"nixos\", \"darwin\", or \"homeManager\"",
            )));
        }
    };

    // ── config ────────────────────────────────────────────
    let config = match attrs.get("config") {
        Some(v) => parse_config(v)?,
        None => Config::new(),
    };

    // ── host / user / toplevelPath ────────────────────────
    let host = string_or_default(&attrs, "host", "unknown-host")?;
    let user = string_or_default(&attrs, "user", "unknown-user")?;
    let toplevel_path = string_or_default(
        &attrs,
        "toplevelPath",
        &format!("/nix/store/zzz-toplevel-{host}"),
    )?;

    // ── Look up the right algorithm for the target ───────
    let algo = find_algo_for_target(target)?;

    let outcome = activation_script::apply(
        &algo,
        &ActivationArgs { config, host, user, toplevel_path },
    )
    .map_err(|e| EvalError::type_error(format!(
        "builtins.sui.activationScript: {e:?}",
    )))?;

    // ── Convert outcome back to Nix attrset ──────────────
    let mut out = NixAttrs::new();
    out.insert("script".to_string(), Value::string(outcome.script_text));
    out.insert("target".to_string(), Value::string(target_str));

    let mut artifacts = NixAttrs::new();
    for (k, v) in outcome.artifacts {
        artifacts.insert(k, Value::string(v));
    }
    out.insert("artifacts".to_string(), Value::Attrs(Rc::new(artifacts)));
    Ok(Value::Attrs(Rc::new(out)))
}

fn parse_config(v: &Value) -> Result<Config, EvalError> {
    let forced = crate::eval::force_value(v)?;
    let attrs = match forced {
        Value::Attrs(a) => a,
        other => {
            return Err(EvalError::type_error(format!(
                "builtins.sui.activationScript.config must be an attrset, got {}",
                other.type_name(),
            )));
        }
    };
    let mut config = Config::new();
    for (k, v) in attrs.iter() {
        config.insert(k.to_string(), crate::eval::force_value(v)?.to_json());
    }
    Ok(config)
}

fn string_or_default(
    attrs: &NixAttrs,
    key: &str,
    fallback: &str,
) -> Result<String, EvalError> {
    match attrs.get(key) {
        Some(v) => match crate::eval::force_value(v)? {
            Value::String(s) => Ok(s.chars.to_string()),
            other => Err(EvalError::type_error(format!(
                "builtins.sui.activationScript.{key} must be a string, got {}",
                other.type_name(),
            ))),
        },
        None => Ok(fallback.into()),
    }
}

fn find_algo_for_target(target: ActivationTarget) -> Result<ActivationScriptAlgorithm, EvalError> {
    let algos = activation_script::load_canonical().map_err(|e| {
        EvalError::type_error(format!("activation algo load: {e:?}"))
    })?;
    algos
        .into_iter()
        .find(|a| a.target == target)
        .ok_or_else(|| EvalError::type_error(format!(
            "no activation_script algorithm declared for target {target:?}",
        )))
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

    #[test]
    fn empty_darwin_activation_produces_a_script() {
        let arg = attrs_of(&[
            ("target", Value::string("darwin")),
            ("host", Value::string("cid")),
            ("user", Value::string("drzzln")),
            ("config", attrs_of(&[])),
        ]);
        let result = activation_script_builtin(&arg).unwrap();
        let out = match result {
            Value::Attrs(a) => a,
            _ => panic!("expected attrs"),
        };
        let script = out.get("script").expect("script field");
        let s = match script {
            Value::String(ns) => ns.chars.to_string(),
            _ => panic!("script must be a string"),
        };
        assert!(s.starts_with("#!/bin/sh"));
        assert!(s.contains("cid"));
    }

    #[test]
    fn nixos_activation_records_systemd_artifact() {
        let arg = attrs_of(&[
            ("target", Value::string("nixos")),
            ("host", Value::string("rio")),
            ("user", Value::string("drzzln")),
            ("config", attrs_of(&[])),
        ]);
        let result = activation_script_builtin(&arg).unwrap();
        let out = match result {
            Value::Attrs(a) => a,
            _ => panic!("expected attrs"),
        };
        let artifacts = match out.get("artifacts").expect("artifacts") {
            Value::Attrs(a) => a,
            _ => panic!("artifacts must be attrs"),
        };
        // NixOS records systemd-units, NOT launchd-plists.
        assert!(artifacts.get("systemd-units").is_some());
        assert!(artifacts.get("launchd-plists").is_none());
    }

    #[test]
    fn rejects_missing_target() {
        let arg = attrs_of(&[("config", attrs_of(&[]))]);
        let err = activation_script_builtin(&arg).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("missing `target`"));
    }

    #[test]
    fn rejects_unknown_target() {
        let arg = attrs_of(&[
            ("target", Value::string("plan9")),
            ("config", attrs_of(&[])),
        ]);
        let err = activation_script_builtin(&arg).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("plan9"));
    }

    #[test]
    fn rejects_non_attrset_arg() {
        let err = activation_script_builtin(&Value::Int(42)).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("expected attrset"));
    }

    #[test]
    fn home_manager_target_accepts_kebab_alias() {
        let arg = attrs_of(&[
            ("target", Value::string("home-manager")),
            ("config", attrs_of(&[])),
        ]);
        let result = activation_script_builtin(&arg).unwrap();
        let out = match result {
            Value::Attrs(a) => a,
            _ => panic!("expected attrs"),
        };
        match out.get("target") {
            Some(Value::String(s)) => assert_eq!(s.chars.as_str(), "home-manager"),
            other => panic!("expected home-manager target, got {other:?}"),
        }
    }
}
