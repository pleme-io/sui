//! Attrset navigation and NIX_PATH resolution.

use crate::value::*;

/// Navigate a nested attrset by a dot-separated attribute path.
///
/// Each path segment is looked up via `Value::Attrs`, and thunks are
/// forced along the way.  Returns the leaf value (forced).
pub fn navigate_attrs(value: &Value, path: &[&str]) -> Result<Value, EvalError> {
    let mut current = crate::eval::force_value(value)?;
    for key in path {
        match current {
            Value::Attrs(ref attrs) => {
                let next = attrs
                    .get(key)
                    .ok_or_else(|| EvalError::AttrNotFound((*key).to_string()))?
                    .clone();
                current = crate::eval::force_value(&next)?;
            }
            _ => {
                return Err(EvalError::builtin_type(
                    &format!("navigate_attrs at '{key}'"),
                    "attrset",
                    current.type_name(),
                ));
            }
        }
    }
    Ok(current)
}

pub use sui_compat::search_path::parse_nix_path;

/// Resolve a `<name>` search-path token: `<nix/...>` to sui's embedded
/// corepkgs, everything else through the one NIX_PATH resolver in
/// `sui_compat::search_path`, with `flake:` entries fetched by the flake
/// fetcher.
#[must_use]
pub fn resolve_search_path(name: &str) -> Option<String> {
    if let Some(sub) = name.strip_prefix("nix/") {
        return resolve_corepkg(sub);
    }
    register_flake_source_fetcher();
    sui_compat::search_path::resolve(name)
}

pub fn register_flake_source_fetcher() {
    sui_compat::search_path::set_flake_source_fetcher(|flake_ref| {
        let parsed = sui_compat::flake_ref::FlakeRef::parse(&format!("{flake_ref}#")).ok()?;
        let dir = crate::fetcher::resolve_flake_dir(&parsed).ok()?;
        Some(crate::path::dematerialize(&dir).to_string_lossy().into_owned())
    });
}

/// Resolve a `<nix/sub>` path to an embedded corepkg file.
/// Writes the embedded content to a temp directory on first access
/// so that `import` can read it as a normal file.
fn resolve_corepkg(sub: &str) -> Option<String> {
    use std::sync::OnceLock;
    static COREPKGS_DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

    let dir = COREPKGS_DIR.get_or_init(|| {
        let dir = crate::fetcher::sui_cache_dir("corepkgs");
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(
            dir.join("fetchurl.nix"),
            include_str!("../corepkgs/fetchurl.nix"),
        ).ok()?;
        Some(dir)
    });

    let dir = dir.as_ref()?;
    let path = dir.join(sub);
    if path.exists() {
        Some(path.to_string_lossy().into_owned())
    } else {
        None
    }
}
