//! Version builtins: compareVersions, parseDrvName, splitVersion.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    // compareVersions — compare version strings
    register_builtin(builtins, "compareVersions", |args| {
        let a = args[0].as_string()?.to_string();
        Ok(Value::Builtin(BuiltinFn {
            name: "compareVersions<partial>",
            func: Rc::new(move |args2| {
                let b = args2[0].as_string()?;
                let result = compare_versions(&a, b);
                Ok(Value::Int(result))
            }),
        }))
    });

    // parseDrvName — parse "name-version" from package name
    register_builtin(builtins, "parseDrvName", |args| {
        let s = args[0].as_string()?;
        let (name, version) = parse_drv_name(s);
        let mut result = NixAttrs::new();
        result.insert("name".to_string(), Value::string(name));
        result.insert("version".to_string(), Value::string(version));
        Ok(Value::Attrs(result))
    });

    // splitVersion
    register_builtin(builtins, "splitVersion", |args| {
        let s = args[0].as_string()?;
        let parts = split_version(s);
        Ok(Value::List(Rc::new(parts.into_iter().map(Value::string).collect())))
    });
}

/// Compare two version strings, returning -1, 0, or 1.
///
/// Splits on `.`, `-`, AND digit/letter boundaries (matching Nix behavior).
/// Compares components numerically where possible, lexicographically otherwise.
/// The special component `"pre"` is less than everything except itself and empty.
pub(crate) fn compare_versions(a: &str, b: &str) -> i64 {
    let pa = split_version(a);
    let pb = split_version(b);
    let max_len = pa.len().max(pb.len());
    for i in 0..max_len {
        let ca = pa.get(i).map(|s| s.as_str()).unwrap_or("");
        let cb = pb.get(i).map(|s| s.as_str()).unwrap_or("");
        // Try numeric comparison first
        let ord = match (ca.parse::<i64>(), cb.parse::<i64>()) {
            (Ok(na), Ok(nb)) => na.cmp(&nb),
            _ => {
                // Nix: "pre" is less than everything except itself and empty
                match (ca, cb) {
                    ("pre", "pre") => std::cmp::Ordering::Equal,
                    ("pre", _) => std::cmp::Ordering::Less,
                    (_, "pre") => std::cmp::Ordering::Greater,
                    _ => ca.cmp(cb),
                }
            }
        };
        if ord != std::cmp::Ordering::Equal {
            return if ord == std::cmp::Ordering::Less { -1 } else { 1 };
        }
    }
    0
}

/// Parse a derivation name into (name, version).
///
/// The version starts at the last `-` followed by a digit.
pub(crate) fn parse_drv_name(s: &str) -> (String, String) {
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).rev() {
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            return (s[..i].to_string(), s[i + 1..].to_string());
        }
    }
    (s.to_string(), String::new())
}

/// Split a version string on `.` / `-` separators and on boundaries
/// between digit and non-digit characters.
pub(crate) fn split_version(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut prev_digit: Option<bool> = None;
    for ch in s.chars() {
        if ch == '.' || ch == '-' {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            prev_digit = None;
        } else {
            let is_digit = ch.is_ascii_digit();
            if let Some(was_digit) = prev_digit
                && is_digit != was_digit && !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            current.push(ch);
            prev_digit = Some(is_digit);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}
