//! String coercion helpers for derivation construction.
//!
//! Used by derivation builtins to convert Nix values to strings
//! for environment variable population.

use crate::value::*;

/// Coerce an already-forced value to a string the way CppNix does for
/// derivation env vars. Delegates to `Value::coerce_to_string()` which
/// is the single source of truth for string coercion semantics.
pub(crate) fn coerce_drv_value_to_string(v: &Value) -> Result<String, EvalError> {
    let (s, _ctx) = v.coerce_to_string()?;
    Ok(s)
}

/// Variant of `coerce_drv_value_to_string` that returns `None` for values
/// that have no meaningful string form (used to skip env entries instead of
/// erroring out).
pub(crate) fn coerce_drv_value_to_string_opt(v: &Value) -> Option<String> {
    coerce_drv_value_to_string(v).ok()
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
