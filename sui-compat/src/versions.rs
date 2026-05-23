//! Nix version-string algorithms — single typed implementation
//! shared by every engine (tree-walker `sui-eval`, bytecode VM
//! `sui-bytecode`, and any future engine).
//!
//! Also hosts [`cppnix_format_float`] — the CppNix-equivalent
//! formatter every engine routes float Display through (see
//! sui-compat::versions::cppnix_format_float for the contract).
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

/// Format a `f64` the way CppNix does — `printf("%g", f)` semantics
/// with 6 significant digits, trailing-zero strip, no decimal point
/// for whole numbers.
///
/// Examples matching `nix eval` byte-for-byte (verified on cppnix
/// 2.30 / cid 2026-05-23):
/// - `1.0 / 3.0`   → `"0.333333"`
/// - `10.0 / 3.0`  → `"3.33333"`     (6 sig digits, not 6 decimal places)
/// - `3.14159`     → `"3.14159"`
/// - `12.345`      → `"12.345"`
/// - `1.5`         → `"1.5"`
/// - `3.0`         → `"3"`
/// - `5.0 - 2.0`   → `"3"`
/// - `0.0`         → `"0"`
/// - `0.0001`      → `"0.0001"`
/// - `NaN`         → `"NaN"`
/// - `inf`         → `"inf"`
///
/// Used by every engine's float Display impl (`Value::Float`,
/// `VMValue::Float`, `StringKeyedValue::Float`) so probe JSON
/// round-trips byte-identically against cppnix.
#[must_use]
pub fn cppnix_format_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".to_string() } else { "-inf".to_string() };
    }
    if f == 0.0 {
        return "0".to_string();
    }

    // %g semantics: choose total significant digits = 6.  For values
    // in the range [1e-4, 1e6) use fixed-point; outside, scientific.
    let exp = f.abs().log10().floor() as i32;
    if (-4..6).contains(&exp) {
        // Fixed-point: after_decimal = 5 - exp, clamped at 0.
        // (One sig digit before decimal when exp >= 0, |exp| leading
        // zeros + one sig digit after decimal when exp < 0.)
        let after_decimal = (5 - exp).max(0) as usize;
        let raw = format!("{f:.*}", after_decimal);
        if let Some((whole, frac)) = raw.split_once('.') {
            let trimmed = frac.trim_end_matches('0');
            if trimmed.is_empty() {
                whole.to_string()
            } else {
                format!("{whole}.{trimmed}")
            }
        } else {
            raw
        }
    } else {
        // Scientific: 6 sig digits → 5 after the leading digit.
        // Strip trailing zeros from the mantissa.
        let raw = format!("{f:.5e}");
        // raw is like "3.33333e10" or "1.00000e-5"
        if let Some((mantissa, exp_part)) = raw.split_once('e') {
            let mantissa_trimmed =
                if let Some((w, frac)) = mantissa.split_once('.') {
                    let trimmed = frac.trim_end_matches('0');
                    if trimmed.is_empty() {
                        w.to_string()
                    } else {
                        format!("{w}.{trimmed}")
                    }
                } else {
                    mantissa.to_string()
                };
            // CppNix emits `e+NN` for positive, `e-NN` for negative.
            // Rust's `{:e}` formatter omits the `+`; restore it.
            let exp_part_signed = if exp_part.starts_with('-') {
                exp_part.to_string()
            } else {
                format!("+{exp_part}")
            };
            format!("{mantissa_trimmed}e{exp_part_signed}")
        } else {
            raw
        }
    }
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
    fn cppnix_format_float_known_outputs() {
        // From `nix eval` on cppnix (verified on cid 2026-05-23):
        assert_eq!(cppnix_format_float(1.0 / 3.0), "0.333333");
        assert_eq!(cppnix_format_float(3.14159), "3.14159");
        assert_eq!(cppnix_format_float(1.5), "1.5");
        assert_eq!(cppnix_format_float(3.0), "3");
        assert_eq!(cppnix_format_float(0.0), "0");
        assert_eq!(cppnix_format_float(-3.14), "-3.14");
        assert_eq!(cppnix_format_float(-3.0), "-3");
    }

    #[test]
    fn cppnix_format_float_nan_and_infinity() {
        assert_eq!(cppnix_format_float(f64::NAN), "NaN");
        assert_eq!(cppnix_format_float(f64::INFINITY), "inf");
        assert_eq!(cppnix_format_float(f64::NEG_INFINITY), "-inf");
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
