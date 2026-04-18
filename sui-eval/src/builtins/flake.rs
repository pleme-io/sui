//! Flake builtins: parseFlakeRef, flakeRefToString, getFlake, evaluate_flake.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_builtin(builtins, "parseFlakeRef", |args| {
        let s = args[0].as_string()?.to_string();
        parse_flake_ref(&s)
    });
    register_builtin(builtins, "flakeRefToString", |args| {
        let attrs = args[0].to_attrs()?;
        flake_ref_to_string(&attrs)
    });

    // sui-specific: resolve an indirect ref (`flake:nixpkgs`) to its
    // concrete registry target. Not a CppNix builtin — exposed here
    // so the layered-registry machinery is testable without needing
    // the full `getFlake` → fetcher → store pipeline.
    register_builtin(builtins, "resolveFlakeRef", |args| {
        let arg = crate::eval::force_value(&args[0])?;
        // Accept either a pre-parsed attrset or a string that needs
        // parsing first. Matches the ergonomic CppNix pattern for
        // flake-ref-shaped inputs.
        let attrs = match arg {
            Value::Attrs(a) => (*a).clone(),
            Value::String(_) => {
                let s = arg.as_string()?.to_string();
                let parsed = parse_flake_ref(&s)?;
                let Value::Attrs(a) = parsed else {
                    return Err(EvalError::TypeError(
                        "resolveFlakeRef: parsed flake ref is not an attrset".into(),
                    ));
                };
                (*a).clone()
            }
            _ => {
                return Err(EvalError::TypeError(
                    "resolveFlakeRef: expected string or attrset".into(),
                ));
            }
        };
        // Only indirect refs need resolving; concrete refs pass
        // through so callers can chain `resolveFlakeRef (parseFlakeRef …)`
        // without branching on type.
        let ty = attrs
            .get("type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if ty == "indirect" {
            super::flake_registry::resolve_indirect(&attrs)
        } else {
            Ok(Value::Attrs(std::rc::Rc::new(attrs)))
        }
    });

    register_builtin(builtins, "getFlake", |args| {
        let flake_ref = crate::eval::force_value(&args[0])?;
        let flake_ref_str = flake_ref.as_string()?.to_string();

        // Path-based references: evaluate directly.
        if flake_ref_str.starts_with('/') || flake_ref_str.starts_with('.') {
            return evaluate_flake(&std::path::PathBuf::from(&flake_ref_str));
        }
        if let Some(path) = flake_ref_str.strip_prefix("path:") {
            return evaluate_flake(&std::path::PathBuf::from(path));
        }

        // GitHub shorthand: "github:owner/repo" or "github:owner/repo/rev".
        if let Some(gh_ref) = flake_ref_str.strip_prefix("github:") {
            let parts: Vec<&str> = gh_ref.splitn(3, '/').collect();
            if parts.len() < 2 {
                return Err(EvalError::TypeError(format!(
                    "getFlake: invalid github ref: {flake_ref_str}"
                )));
            }
            let owner = parts[0];
            let repo = parts[1];
            let rev = if parts.len() >= 3 { parts[2] } else { "HEAD" };

            let locked = sui_compat::flake::LockedInput {
                source_type: "github".to_string(),
                owner: Some(owner.to_string()),
                repo: Some(repo.to_string()),
                rev: Some(rev.to_string()),
                nar_hash: None,
                last_modified: None,
                path: None,
                url: None,
                git_ref: None,
                dir: None,
                extra: std::collections::BTreeMap::new(),
            };

            let fetcher = crate::fetcher::InputFetcher::new();
            let fetched_dir = fetcher.fetch(&locked).map_err(|e| {
                EvalError::IoError {
                    context: format!("getFlake: fetch {flake_ref_str}"),
                    message: e.to_string(),
                }
            })?;

            return evaluate_flake(&fetched_dir);
        }

        // For any other reference style, return a proper error.
        Err(EvalError::NotImplemented(format!(
            "getFlake: unsupported flake reference scheme: {flake_ref_str}"
        )))
    });
}
