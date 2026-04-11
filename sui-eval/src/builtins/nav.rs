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

/// Parse a `NIX_PATH` env var value into `(prefix, path)` pairs.
///
/// The format is `prefix1=path1:prefix2=path2:...`. An entry with
/// no `=` is treated as having an empty prefix (CppNix-compatible).
/// Empty entries are skipped.
#[must_use]
pub fn parse_nix_path(s: &str) -> Vec<(String, String)> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(':')
        .filter(|e| !e.is_empty())
        .map(|entry| match entry.split_once('=') {
            Some((prefix, path)) => (prefix.to_string(), path.to_string()),
            None => (String::new(), entry.to_string()),
        })
        .collect()
}

/// Resolve a `<name>` search-path token to an absolute filesystem
/// path by walking the entries parsed from `NIX_PATH`.
#[must_use]
pub fn resolve_search_path(name: &str) -> Option<String> {
    let nix_path = std::env::var("NIX_PATH").ok()?;
    for (prefix, path) in parse_nix_path(&nix_path) {
        if !prefix.is_empty() && name == prefix {
            if std::path::Path::new(&path).exists() {
                return Some(path);
            }
            continue;
        }
        if !prefix.is_empty() {
            let needle = format!("{prefix}/");
            if let Some(rest) = name.strip_prefix(&needle) {
                let full = format!("{path}/{rest}");
                if std::path::Path::new(&full).exists() {
                    return Some(full);
                }
                continue;
            }
        }
        if prefix.is_empty() {
            let full = format!("{path}/{name}");
            if std::path::Path::new(&full).exists() {
                return Some(full);
            }
        }
    }
    None
}
