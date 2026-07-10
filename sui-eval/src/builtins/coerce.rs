//! String coercion helpers for derivation construction.
//!
//! Used by derivation builtins to convert Nix values to strings
//! for environment variable population.

use crate::value::*;

/// Coerce an already-forced value to a string the way CppNix does for
/// derivation env vars. Derivation-attribute coercion is CppNix
/// **copy-to-store** mode: a source path (`src = ./.`, `builder = ./x`)
/// is NAR-copied into `/nix/store/<hash>-<name>` and the store path is
/// what lands in the env — so the drv hash matches CppNix. Delegates to
/// `Value::coerce_to_string_copy_to_store()`, the single source of truth.
pub(crate) fn coerce_drv_value_to_string(v: &Value) -> Result<String, EvalError> {
    let (s, _ctx) = v.coerce_to_string_copy_to_store()?;
    Ok(s)
}

/// Like [`coerce_drv_value_to_string`] but also returns the string
/// context (drv-path + output-source references the string carries).
/// Used by derivation construction to populate `input_derivations`
/// / `input_sources` — CppNix parity on transitive closures. Copy-to-store
/// mode: a coerced source path adds its store path to `input_sources`.
pub(crate) fn coerce_drv_value_to_string_with_context(
    v: &Value,
) -> Result<(String, StringContext), EvalError> {
    v.coerce_to_string_copy_to_store()
}

/// Variant of `coerce_drv_value_to_string` that returns `None` for values
/// that have no meaningful string form (used to skip env entries instead of
/// erroring out).
pub(crate) fn coerce_drv_value_to_string_opt(v: &Value) -> Option<String> {
    coerce_drv_value_to_string(v).ok()
}

/// Like [`coerce_drv_value_to_string_opt`] but keeps the context.
/// Returns `None` if the value has no meaningful string form.
pub(crate) fn coerce_drv_value_to_string_opt_with_context(
    v: &Value,
) -> Option<(String, StringContext)> {
    v.coerce_to_string_copy_to_store().ok()
}

/// Force an attribute and require it to be present + string-coercible.
pub(crate) fn force_attr_string(
    attrs: &NixAttrs,
    key: &str,
) -> Result<String, EvalError> {
    let v = attrs
        .get(key)
        .ok_or_else(|| EvalError::AttrNotFound(key.into()))?;
    let forced = crate::eval::force_value(v)?;
    coerce_drv_value_to_string(&forced)
}

/// Force an optional attribute, returning `None` if absent.
pub(crate) fn optional_attr_string(
    attrs: &NixAttrs,
    key: &str,
) -> Result<Option<String>, EvalError> {
    match attrs.get(key) {
        None => Ok(None),
        Some(v) => {
            let forced = crate::eval::force_value(v)?;
            Ok(Some(coerce_drv_value_to_string(&forced)?))
        }
    }
}
