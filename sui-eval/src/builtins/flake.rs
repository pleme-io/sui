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

        // Step 1: normalize input to a parsed attrset. CppNix accepts
        // either a string (parsed) or a pre-parsed attrset; we do the
        // same so `builtins.getFlake { type = "github"; owner = ...; repo = ...; }`
        // and `builtins.getFlake "github:owner/repo"` both work.
        let parsed_attrs = match &flake_ref {
            Value::String(_) => {
                let s = flake_ref.as_string()?.to_string();
                let parsed = parse_flake_ref(&s)?;
                parsed.to_attrs()?
            }
            Value::Attrs(a) => (**a).clone(),
            _ => {
                return Err(EvalError::TypeError(
                    "getFlake: expected string or attrset".into(),
                ));
            }
        };

        // Step 2: if indirect, resolve through the registry. The
        // resolver handles chain-follow + caller-override preservation.
        let ty = parsed_attrs
            .get("type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let resolved_attrs = if ty == "indirect" {
            let resolved_val = super::flake_registry::resolve_indirect(&parsed_attrs)?;
            resolved_val.to_attrs()?
        } else {
            parsed_attrs
        };

        // Step 3: dispatch on the concrete type and fetch into a
        // local path. Every non-path scheme goes through InputFetcher.
        let ref_type = resolved_attrs
            .get("type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();

        // path: evaluates directly — no fetch needed.
        if ref_type == "path" {
            let p = resolved_attrs
                .get("path")
                .ok_or_else(|| EvalError::AttrNotFound("path".into()))?
                .to_str()?;
            return evaluate_flake(&std::path::PathBuf::from(&*p));
        }

        // github/gitlab/sourcehut/git/tarball — convert to LockedInput
        // and run through the fetcher.
        let locked = attrs_to_locked_input(&resolved_attrs)?;
        let context_str = flake_ref_to_string(&resolved_attrs)
            .ok()
            .and_then(|v| v.as_string().ok().map(|s| s.to_string()))
            .unwrap_or_else(|| ref_type.to_string());

        let fetcher = crate::fetcher::InputFetcher::new();
        let fetched_dir = fetcher.fetch(&locked).map_err(|e| EvalError::IoError {
            context: format!("getFlake: fetch {context_str}"),
            message: e.to_string(),
        })?;

        evaluate_flake(&fetched_dir)
    });
}

/// Convert a parsed flake-ref attrset (output of `parseFlakeRef` or
/// the registry resolver) into a `sui_compat::flake::LockedInput` that
/// `crate::fetcher::InputFetcher` accepts. `LockedInput` expects a
/// concrete `rev` for github/git sources; we use `ref` as a fallback
/// when no `rev` is present (the GitHub tarball endpoint accepts both
/// SHAs and ref names, and flake consumers get the deterministic-
/// enough behavior they need for one-shot evaluation).
fn attrs_to_locked_input(
    attrs: &NixAttrs,
) -> Result<sui_compat::flake::LockedInput, EvalError> {
    let ty = attrs
        .get("type")
        .ok_or_else(|| EvalError::AttrNotFound("type".into()))?
        .to_str()?;

    let str_field = |name: &str| -> Option<String> {
        attrs.get(name).and_then(|v| v.to_str().ok())
    };

    // `ref` fallback: if rev is missing, use ref; if both are missing,
    // use "HEAD". The fetcher will try the GitHub tarball endpoint with
    // whatever string we hand it, so this covers branch names, tags,
    // and full SHAs uniformly.
    let rev = str_field("rev")
        .or_else(|| str_field("ref"))
        .unwrap_or_else(|| "HEAD".to_string());

    Ok(sui_compat::flake::LockedInput {
        source_type: ty.to_string(),
        owner: str_field("owner"),
        repo: str_field("repo"),
        rev: Some(rev),
        nar_hash: None,
        last_modified: None,
        path: str_field("path"),
        url: str_field("url"),
        git_ref: str_field("ref"),
        dir: str_field("dir"),
        host: None,
        extra: std::collections::BTreeMap::new(),
    })
}
