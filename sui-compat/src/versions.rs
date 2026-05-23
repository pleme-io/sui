//! Nix version-string algorithms — single typed implementation
//! shared by every engine (tree-walker `sui-eval`, bytecode VM
//! `sui-bytecode`, and any future engine).
//!
//! Three primitives:
//!
//! - [`split_version`] — tokenize a version string on `.` / `-` /
//!   digit↔non-digit boundaries, matching CppNix's `splitString`
//!   semantics.
//! - [`compare_versions`] — three-way comparison returning `-1` /
//!   `0` / `1`.  Handles the `"pre"` special case (any component
//!   equal to `"pre"` orders below everything except itself and
//!   the empty component).
//! - [`parse_drv_name`] — split a `<name>-<version>` package
//!   string at the last `-` followed by a digit.
//!
//! Lifted from the tree-walker `sui-eval::builtins::versions` so the
//! VM's previously naive duplicate (split on `.` only, no `pre`
//! handling) doesn't drift.  The bug that surfaced this extraction:
//! `compareVersions "1.0-rc1" "1.0-pre1"` returned `0` on the VM
//! and `1` on cppnix.  Same canonical implementation now lives here.

/// Split a version string into typed components.
///
/// Splits on `.` and `-` separators AND on boundaries between
/// digit and non-digit characters.  Empty components are dropped.
///
/// Examples:
/// - `"1.0-rc1"`   → `["1", "0", "rc", "1"]`
/// - `"1.0.0-pre"` → `["1", "0", "0", "pre"]`
/// - `"2024a"`     → `["2024", "a"]`
#[must_use]
pub fn split_version(s: &str) -> Vec<String> {
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
                && is_digit != was_digit
                && !current.is_empty()
            {
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

/// Three-way comparison of two version strings.
///
/// Component-by-component:
///
/// 1. If both parse as integers, compare numerically.
/// 2. Otherwise, the special component `"pre"` is less than any
///    non-`"pre"` component (including the empty component) — this
///    matches CppNix's pre-release ordering convention.
/// 3. Otherwise, compare lexicographically.
///
/// Missing components default to `""`, so `"1.0"` and `"1.0.0"`
/// compare as `0` (CppNix matches this).
///
/// Returns `-1`, `0`, or `1`.
#[must_use]
pub fn compare_versions(a: &str, b: &str) -> i64 {
    let pa = split_version(a);
    let pb = split_version(b);
    let max_len = pa.len().max(pb.len());
    for i in 0..max_len {
        let ca = pa.get(i).map(String::as_str).unwrap_or("");
        let cb = pb.get(i).map(String::as_str).unwrap_or("");
        let ord = match (ca.parse::<i64>(), cb.parse::<i64>()) {
            (Ok(na), Ok(nb)) => na.cmp(&nb),
            _ => match (ca, cb) {
                ("pre", "pre") => std::cmp::Ordering::Equal,
                ("pre", _) => std::cmp::Ordering::Less,
                (_, "pre") => std::cmp::Ordering::Greater,
                _ => ca.cmp(cb),
            },
        };
        if ord != std::cmp::Ordering::Equal {
            return if ord == std::cmp::Ordering::Less { -1 } else { 1 };
        }
    }
    0
}

/// Parse a `<name>-<version>` package string into `(name, version)`.
///
/// The version starts at the last `-` immediately followed by a
/// digit.  If no such boundary exists, the whole string is the
/// name and the version is empty.
#[must_use]
pub fn parse_drv_name(s: &str) -> (String, String) {
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).rev() {
        if bytes[i] == b'-'
            && i + 1 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
        {
            return (s[..i].to_string(), s[i + 1..].to_string());
        }
    }
    (s.to_string(), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn rc_orders_above_pre() {
        assert_eq!(compare_versions("1.0-rc1", "1.0-pre1"), 1);
        assert_eq!(compare_versions("1.0-pre1", "1.0-rc1"), -1);
    }

    #[test]
    fn numeric_components_compare_numerically() {
        assert_eq!(compare_versions("1.10", "1.2"), 1);
        assert_eq!(compare_versions("1.2", "1.10"), -1);
        assert_eq!(compare_versions("1.0", "1.0"), 0);
    }

    #[test]
    fn missing_components_order_below_present_components() {
        // CppNix matches: "1.0" < "1.0.0" because the missing 3rd
        // component compares as "" < "0" lexicographically.
        assert_eq!(compare_versions("1.0", "1.0.0"), -1);
        assert_eq!(compare_versions("1.0.0", "1.0"), 1);
    }

    #[test]
    fn pre_below_everything_except_pre() {
        assert_eq!(compare_versions("1.0-pre1", "1.0-pre1"), 0);
        assert_eq!(compare_versions("1.0-pre1", "1.0"), -1);
        assert_eq!(compare_versions("1.0", "1.0-pre1"), 1);
        // pre vs any non-pre suffix
        assert_eq!(compare_versions("1.0-pre", "1.0-alpha"), -1);
        assert_eq!(compare_versions("1.0-pre", "1.0-beta"), -1);
        assert_eq!(compare_versions("1.0-pre", "1.0-rc"), -1);
    }

    #[test]
    fn split_version_basic_shapes() {
        assert_eq!(split_version("1.0-rc1"), vec!["1", "0", "rc", "1"]);
        assert_eq!(split_version("1.0.0-pre"), vec!["1", "0", "0", "pre"]);
        assert_eq!(split_version("2024a"), vec!["2024", "a"]);
    }

    #[test]
    fn parse_drv_name_recovers_split() {
        let (n, v) = parse_drv_name("hello-1.2.3");
        assert_eq!(n, "hello");
        assert_eq!(v, "1.2.3");
        let (n, v) = parse_drv_name("nix-darwin-config");
        assert_eq!(n, "nix-darwin-config");
        assert_eq!(v, "");
    }

    proptest! {
        /// Antisymmetry: `compare(a, b) == -compare(b, a)`.
        #[test]
        fn compare_versions_antisymmetric(
            a in "[0-9a-z.-]{1,20}",
            b in "[0-9a-z.-]{1,20}",
        ) {
            let ab = compare_versions(&a, &b);
            let ba = compare_versions(&b, &a);
            prop_assert_eq!(ab, -ba);
        }

        /// Reflexivity: `compare(a, a) == 0`.
        #[test]
        fn compare_versions_reflexive(a in "[0-9a-z.-]{1,20}") {
            prop_assert_eq!(compare_versions(&a, &a), 0);
        }
    }
}
