//! Shared helpers across the `builtins.sui.*` bridge modules.
//!
//! Each bridge needs to (a) coerce a Nix Value into a typed shape
//! and (b) emit a typed error that mentions which bridge generated
//! it.  The four helpers below — `attrs_required_string`,
//! `attrs_optional_string`, `attrs_required_attrs`,
//! `attrs_required_list` — collapse what would otherwise be
//! cut-paste boilerplate across the bridges.
//!
//! Pattern follows the sui-spec Environment-trait convention: the
//! same idea applied at the engine/spec boundary.  Third-site
//! extraction per the prime directive — the same shape appeared
//! in three bridges (module_system, activation_script, hash) and
//! is about to appear in four more.

use std::rc::Rc;

use super::*;

/// Get a required string field from an attrset.
///
/// # Errors
///
/// Emits a typed `EvalError::TypeError` with the bridge name in
/// the message when the field is missing OR not a string.
pub(crate) fn attrs_required_string(
    attrs: &NixAttrs,
    key: &str,
    bridge: &str,
) -> Result<String, EvalError> {
    let v = attrs.get(key).ok_or_else(|| EvalError::type_error(format!(
        "{bridge}: missing required field `{key}`",
    )))?;
    match crate::eval::force_value(v)? {
        Value::String(s) => Ok(s.chars.to_string()),
        other => Err(EvalError::type_error(format!(
            "{bridge}: field `{key}` must be a string, got {}",
            other.type_name(),
        ))),
    }
}

/// Get an optional string field — returns `None` if absent, or a
/// type-error if present but wrong type.
pub(crate) fn attrs_optional_string(
    attrs: &NixAttrs,
    key: &str,
    bridge: &str,
) -> Result<Option<String>, EvalError> {
    match attrs.get(key) {
        Some(v) => match crate::eval::force_value(v)? {
            Value::String(s) => Ok(Some(s.chars.to_string())),
            other => Err(EvalError::type_error(format!(
                "{bridge}: field `{key}` must be a string, got {}",
                other.type_name(),
            ))),
        },
        None => Ok(None),
    }
}

/// Get a required attrset field.
pub(crate) fn attrs_required_attrs(
    attrs: &NixAttrs,
    key: &str,
    bridge: &str,
) -> Result<Rc<NixAttrs>, EvalError> {
    let v = attrs.get(key).ok_or_else(|| EvalError::type_error(format!(
        "{bridge}: missing required field `{key}`",
    )))?;
    match crate::eval::force_value(v)? {
        Value::Attrs(a) => Ok(a),
        other => Err(EvalError::type_error(format!(
            "{bridge}: field `{key}` must be an attrset, got {}",
            other.type_name(),
        ))),
    }
}

/// Force a value and coerce to attrset.  Surfaces the bridge name
/// in the error.
pub(crate) fn as_attrs(value: &Value, bridge: &str) -> Result<Rc<NixAttrs>, EvalError> {
    match crate::eval::force_value(value)? {
        Value::Attrs(a) => Ok(a),
        other => Err(EvalError::type_error(format!(
            "{bridge}: expected attrset, got {}",
            other.type_name(),
        ))),
    }
}

/// Force a value and coerce to string.
pub(crate) fn as_string(value: &Value, bridge: &str) -> Result<String, EvalError> {
    match crate::eval::force_value(value)? {
        Value::String(s) => Ok(s.chars.to_string()),
        other => Err(EvalError::type_error(format!(
            "{bridge}: expected string, got {}",
            other.type_name(),
        ))),
    }
}

/// Get a required int field.
pub(crate) fn attrs_required_int(
    attrs: &NixAttrs,
    key: &str,
    bridge: &str,
) -> Result<i64, EvalError> {
    let v = attrs.get(key).ok_or_else(|| EvalError::type_error(format!(
        "{bridge}: missing required field `{key}`",
    )))?;
    match crate::eval::force_value(v)? {
        Value::Int(n) => Ok(n),
        other => Err(EvalError::type_error(format!(
            "{bridge}: field `{key}` must be an int, got {}",
            other.type_name(),
        ))),
    }
}

/// Get an optional int field.  Absent = `None`.
pub(crate) fn attrs_optional_int(
    attrs: &NixAttrs,
    key: &str,
    bridge: &str,
) -> Result<Option<i64>, EvalError> {
    match attrs.get(key) {
        Some(v) => match crate::eval::force_value(v)? {
            Value::Int(n) => Ok(Some(n)),
            other => Err(EvalError::type_error(format!(
                "{bridge}: field `{key}` must be an int, got {}",
                other.type_name(),
            ))),
        },
        None => Ok(None),
    }
}

/// Get a required bool field.
pub(crate) fn attrs_required_bool(
    attrs: &NixAttrs,
    key: &str,
    bridge: &str,
) -> Result<bool, EvalError> {
    let v = attrs.get(key).ok_or_else(|| EvalError::type_error(format!(
        "{bridge}: missing required field `{key}`",
    )))?;
    match crate::eval::force_value(v)? {
        Value::Bool(b) => Ok(b),
        other => Err(EvalError::type_error(format!(
            "{bridge}: field `{key}` must be a bool, got {}",
            other.type_name(),
        ))),
    }
}

/// Get an optional bool field with a fallback default.
pub(crate) fn attrs_bool_or_default(
    attrs: &NixAttrs,
    key: &str,
    default: bool,
    bridge: &str,
) -> Result<bool, EvalError> {
    match attrs.get(key) {
        Some(v) => match crate::eval::force_value(v)? {
            Value::Bool(b) => Ok(b),
            other => Err(EvalError::type_error(format!(
                "{bridge}: field `{key}` must be a bool, got {}",
                other.type_name(),
            ))),
        },
        None => Ok(default),
    }
}

/// Get a required list field.
pub(crate) fn attrs_required_list(
    attrs: &NixAttrs,
    key: &str,
    bridge: &str,
) -> Result<Rc<Vec<Value>>, EvalError> {
    let v = attrs.get(key).ok_or_else(|| EvalError::type_error(format!(
        "{bridge}: missing required field `{key}`",
    )))?;
    match crate::eval::force_value(v)? {
        Value::List(l) => Ok(l),
        other => Err(EvalError::type_error(format!(
            "{bridge}: field `{key}` must be a list, got {}",
            other.type_name(),
        ))),
    }
}

/// Force a value and coerce to list.
pub(crate) fn as_list(value: &Value, bridge: &str) -> Result<Rc<Vec<Value>>, EvalError> {
    match crate::eval::force_value(value)? {
        Value::List(l) => Ok(l),
        other => Err(EvalError::type_error(format!(
            "{bridge}: expected list, got {}",
            other.type_name(),
        ))),
    }
}

/// Collect a list of strings from a list-valued attrset field.
/// Returns an empty Vec when the field is absent.
pub(crate) fn attrs_string_list(
    attrs: &NixAttrs,
    key: &str,
    bridge: &str,
) -> Result<Vec<String>, EvalError> {
    let Some(v) = attrs.get(key) else { return Ok(Vec::new()); };
    let list = match crate::eval::force_value(v)? {
        Value::List(l) => l,
        other => return Err(EvalError::type_error(format!(
            "{bridge}: field `{key}` must be a list, got {}",
            other.type_name(),
        ))),
    };
    let mut out = Vec::with_capacity(list.len());
    for item in list.iter() {
        out.push(as_string(item, bridge)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs_of(pairs: &[(&str, Value)]) -> Rc<NixAttrs> {
        let mut a = NixAttrs::new();
        for (k, v) in pairs {
            a.insert(k.to_string(), v.clone());
        }
        Rc::new(a)
    }

    #[test]
    fn required_string_finds_value() {
        let a = attrs_of(&[("name", Value::string("hello"))]);
        let s = attrs_required_string(&a, "name", "test").unwrap();
        assert_eq!(s, "hello");
    }

    #[test]
    fn required_string_missing_field_errors() {
        let a = attrs_of(&[]);
        let err = attrs_required_string(&a, "name", "test").unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("test:"));
        assert!(msg.contains("missing required field"));
        assert!(msg.contains("`name`"));
    }

    #[test]
    fn required_string_wrong_type_errors() {
        let a = attrs_of(&[("name", Value::Int(42))]);
        let err = attrs_required_string(&a, "name", "test").unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("must be a string"));
    }

    #[test]
    fn optional_string_returns_none_when_absent() {
        let a = attrs_of(&[]);
        let o = attrs_optional_string(&a, "name", "test").unwrap();
        assert!(o.is_none());
    }

    #[test]
    fn optional_string_returns_some_when_present() {
        let a = attrs_of(&[("name", Value::string("x"))]);
        let o = attrs_optional_string(&a, "name", "test").unwrap();
        assert_eq!(o.as_deref(), Some("x"));
    }

    #[test]
    fn required_attrs_extracts_sub_attrset() {
        let inner = attrs_of(&[("k", Value::Int(1))]);
        let a = attrs_of(&[("sub", Value::Attrs(inner))]);
        let sub = attrs_required_attrs(&a, "sub", "test").unwrap();
        assert!(sub.get("k").is_some());
    }

    #[test]
    fn bridge_name_appears_in_every_error() {
        let a = attrs_of(&[]);
        let err = attrs_required_string(&a, "x", "builtins.sui.foo").unwrap_err();
        assert!(format!("{err:?}").contains("builtins.sui.foo"));
        let err = attrs_required_attrs(&a, "x", "builtins.sui.foo").unwrap_err();
        assert!(format!("{err:?}").contains("builtins.sui.foo"));
    }

    // ── Int / Bool / List helper tests ─────────────────────────

    #[test]
    fn required_int_finds_value() {
        let a = attrs_of(&[("port", Value::Int(8080))]);
        let n = attrs_required_int(&a, "port", "test").unwrap();
        assert_eq!(n, 8080);
    }

    #[test]
    fn required_int_wrong_type_errors() {
        let a = attrs_of(&[("port", Value::string("8080"))]);
        let err = attrs_required_int(&a, "port", "test").unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("must be an int"));
    }

    #[test]
    fn optional_int_returns_none_when_absent() {
        let a = attrs_of(&[]);
        let r = attrs_optional_int(&a, "port", "test").unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn required_bool_finds_value() {
        let a = attrs_of(&[("enable", Value::Bool(true))]);
        let b = attrs_required_bool(&a, "enable", "test").unwrap();
        assert!(b);
    }

    #[test]
    fn bool_or_default_falls_back_when_absent() {
        let a = attrs_of(&[]);
        let b = attrs_bool_or_default(&a, "enable", true, "test").unwrap();
        assert!(b);
        let b = attrs_bool_or_default(&a, "enable", false, "test").unwrap();
        assert!(!b);
    }

    #[test]
    fn required_list_finds_value() {
        let a = attrs_of(&[("xs", Value::list(vec![Value::Int(1), Value::Int(2)]))]);
        let l = attrs_required_list(&a, "xs", "test").unwrap();
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn string_list_collects_strings() {
        let a = attrs_of(&[("refs", Value::list(vec![
            Value::string("a"),
            Value::string("b"),
        ]))]);
        let v = attrs_string_list(&a, "refs", "test").unwrap();
        assert_eq!(v, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn string_list_returns_empty_when_absent() {
        let a = attrs_of(&[]);
        let v = attrs_string_list(&a, "refs", "test").unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn string_list_errors_on_non_string_element() {
        let a = attrs_of(&[("refs", Value::list(vec![
            Value::string("a"),
            Value::Int(42),
        ]))]);
        let err = attrs_string_list(&a, "refs", "test").unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("expected string"));
    }
}
