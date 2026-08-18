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

/// The nix version sui impersonates through `builtins.nixVersion`.
///
/// **This is a load-bearing constant, not cosmetic.** nixpkgs and nix-darwin
/// modules feature-gate on `lib.versionAtLeast builtins.nixVersion "X"`, so a
/// stale value takes the WRONG branch for every gate between the stale value
/// and the real host version — silently forking the evaluated derivation
/// graph. No error, no warning, just a different graph.
///
/// It lives here because it drifted, exactly the way this module's own header
/// predicted the version *algorithms* would: the tree-walker and `sui-ir` were
/// corrected from `"2.24.0"` to `"2.34.7"` and the bytecode VM was left behind
/// at the stale literal. Measured 2026-08-17 — walker `"2.34.7"`, VM
/// `"2.24.0"`, host nix 2.31.5. One program, two answers to the same question,
/// and the arm that was wrong is the one that evaluates flakes.
///
/// Three engines hand-listing one fact is three chances to disagree, and it
/// took only one. Every engine now reads this constant; bump it here alongside
/// the host nix sui mirrors, and all three move together.
pub const IMPERSONATED_NIX_VERSION: &str = "2.34.7";

/// `builtins.langVersion` — the Nix *language* version, distinct from the
/// implementation version above.
///
/// Not observed to have drifted (all three engines said 6), but it was carried
/// as a third independent literal in the same three files as
/// [`IMPERSONATED_NIX_VERSION`], which is the same class of hazard whether or
/// not it has fired yet. Centralised while the shape was being fixed rather
/// than left as the next one to go stale.
pub const LANG_VERSION: i64 = 6;

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
            // CppNix (libstore/names.cc `compareVersionComponent`): a NUMERIC
            // component is always GREATER than any non-numeric one (including
            // "pre", the smallest). The prior `ca.cmp(cb)` fallback used
            // byte-lexicographic order, so a letter like "a" (0x61) wrongly
            // sorted GREATER than "1" (0x31) — the exact opposite of nix, which
            // flipped every `lib.versionAtLeast`/`versionOlder` gate comparing a
            // letter component (RC/date suffixes, `1.0.a`) against a number.
            // (An empty component parses as Err, so `"" < number` — matching
            // nix's `compareVersions "1" "1.0" == -1`.)
            (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
            (Err(_), Ok(_)) => std::cmp::Ordering::Less,
            // Both non-numeric: "pre" sorts below everything, else lexicographic.
            (Err(_), Err(_)) => match (ca, cb) {
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
/// Format an `f64` the way CppNix's JSON writer does — which is NOT the way
/// [`cppnix_format_float`] formats a Nix *value*, and NOT what `serde_json`
/// emits either.
///
/// Measured against real nix 2.31.5 on 2026-08-18. The rule is `%g`-like but
/// with its own thresholds, and it is independent of the digit count:
///
/// | value | nix JSON |
/// |---|---|
/// | `1.5` | `1.5` |
/// | `1.0` | `1.0`  (an integral value keeps a `.0`) |
/// | `3.0e10` | `30000000000.0` |
/// | `1.0e14` | `100000000000000.0` |
/// | `1.0e15` | `1e+15`  ← the large-side switch |
/// | `1.0e-4` | `0.0001` |
/// | `1.0e-5` | `1e-05`  ← the small-side switch, and note the ZERO PAD |
/// | `2.5e-5` | `2.5e-05` |
/// | `-0.0`   | `0.0`   ← the sign on a zero is dropped |
///
/// So: **fixed iff the scientific exponent is in `[-4, 14]`**, scientific
/// otherwise, with the exponent always signed and padded to at least two
/// digits. Digits are shortest-round-trip (`0.30000000000000004` prints in
/// full), which is what Rust's own `Display`/`LowerExp` already give.
///
/// # Why this is not cosmetic
///
/// `__structuredAttrs` serializes a derivation's attributes to JSON and puts
/// the result in the derivation's environment, where it is hashed into the
/// drvPath. A float that renders one byte differently there produces a
/// different store path for the same expression. `serde_json` renders
/// `0.00001` as `0.00001` and `1.0e-6` as `1e-6`, both of which nix writes
/// differently — so any derivation carrying such a float in a structured attr
/// diverged.
#[must_use]
pub fn cppnix_format_json_float(f: f64) -> String {
    // Non-finite has no JSON representation; `serde_json` writes `null` and so
    // does nix's writer. Keep that, rather than inventing a token.
    if !f.is_finite() {
        return "null".to_string();
    }
    // `-0.0 == 0.0` is true in IEEE, so this drops the sign exactly as nix does.
    if f == 0.0 {
        return "0.0".to_string();
    }

    // `{:e}` gives the shortest round-trip mantissa plus a decimal exponent,
    // which is precisely the pair the threshold rule is stated over.
    let sci = format!("{f:e}");
    let (mantissa, exp_str) = sci
        .split_once('e')
        .expect("LowerExp for f64 always emits an exponent");
    let exp: i32 = exp_str
        .parse()
        .expect("LowerExp for f64 always emits a parseable exponent");

    if (-4..=14).contains(&exp) {
        // Rust's `Display` for f64 never uses scientific notation, so within
        // this window it already produces exactly the fixed form nix wants.
        let fixed = format!("{f}");
        if fixed.contains('.') {
            fixed
        } else {
            // An integral float keeps a `.0` in JSON, or it would read back as
            // an integer and change the value's TYPE on round-trip.
            format!("{fixed}.0")
        }
    } else {
        let (sign, digits) = match exp_str.strip_prefix('-') {
            Some(d) => ('-', d),
            None => ('+', exp_str),
        };
        format!("{mantissa}e{sign}{digits:0>2}")
    }
}

/// Serialize a `serde_json::Value` with CppNix's float formatting.
///
/// ★ THE HOOK IS AT THE SERIALIZATION BOUNDARY, ON PURPOSE. There are six
/// places in this workspace that build a `serde_json::Value` from a Nix value
/// (three engines' `builtins.toJSON`, plus two `__structuredAttrs` emitters,
/// plus the CLI's `--json`), and every one of them constructs floats with
/// `serde_json::json!(f)`. Patching those six construction sites would leave
/// the seventh — and a float nested three levels inside a list inside an
/// attrset still has to render correctly. `Formatter::write_f64` is a single
/// funnel that every float in the tree passes through regardless of depth.
///
/// Anything that is NOT a Nix value (the eval cache, fetcher metadata, the
/// perf seal) must keep using plain `serde_json`: those are sui's own files,
/// not bytes compared against nix.
pub fn nix_json_to_string(value: &serde_json::Value) -> Result<String, serde_json::Error> {
    struct NixFloats;
    impl serde_json::ser::Formatter for NixFloats {
        fn write_f64<W>(&mut self, writer: &mut W, value: f64) -> std::io::Result<()>
        where
            W: ?Sized + std::io::Write,
        {
            writer.write_all(cppnix_format_json_float(value).as_bytes())
        }
    }
    let mut buf = Vec::with_capacity(128);
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, NixFloats);
    serde::Serialize::serialize(value, &mut ser)?;
    Ok(String::from_utf8(buf).expect("serde_json emits UTF-8"))
}

/// Escape a string for an XML attribute value, exactly as CppNix's `XMLWriter`
/// does: `& < > "` **and** the newline.
///
/// The LF is the one people leave out, and it is data loss rather than
/// cosmetics — XML attribute-value normalization turns a raw LF into a space
/// on the way back out, so the string does not round-trip. Both `toXML`
/// implementations (`sui-eval`'s tree-walker and `sui-ir`'s) had the four-char
/// table and neither had the newline, which is precisely the drift that lands
/// when a cross-engine fact is re-derived per engine instead of shared. It
/// lives here beside [`cppnix_format_float`] for that reason.
#[must_use]
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\n', "&#xA;")
}

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
            // Rust's `{:e}` formatter omits the `+` AND does not pad;
            // C's `%g` writes a sign plus a MINIMUM of two exponent digits,
            // so `1e8` must print as `1e+08` while `1e100` keeps all three.
            //
            // The padding half was missing for as long as this function has
            // existed, even though the comment here already said `e+NN`. It
            // survived because the test block below has no scientific-notation
            // case at all — every example is fixed-point. Since this formatter
            // is shared by all three engines' float Display impls, the same
            // wrong byte was emitted everywhere, consistently, which is the
            // shape that reads as correct.
            let (sign, digits) = match exp_part.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', exp_part),
            };
            let exp_part_signed = format!("{sign}{digits:0>2}");
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
    fn numeric_component_beats_letter_component() {
        // CppNix (libstore/names.cc): a numeric component is ALWAYS greater
        // than a non-numeric (letter) one. Verified against `nix eval`.
        assert_eq!(compare_versions("1.a", "1.1"), -1); // "a" < "1"
        assert_eq!(compare_versions("1.1", "1.a"), 1); // "1" > "a"
        assert_eq!(compare_versions("1.0.0", "1.0.a"), 1); // "0" > "a"
        assert_eq!(compare_versions("a", "1"), -1); // letter < number
        assert_eq!(compare_versions("1", "a"), 1);
        // a number also beats "pre" (pre is the smallest non-numeric)
        assert_eq!(compare_versions("1.1", "1.pre"), 1);
        assert_eq!(compare_versions("1.pre", "1.1"), -1);
        // two letters still compare lexicographically
        assert_eq!(compare_versions("1.a", "1.b"), -1);
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

    /// CppNix's JSON float format, which is a DIFFERENT rule from the value
    /// format pinned above — same input, different bytes, and conflating them
    /// is the mistake this pair of tests exists to prevent. Every expected
    /// value read off real `nix eval --raw --expr 'builtins.toJSON …'`
    /// on 2026-08-18.
    #[test]
    fn cppnix_json_float_switches_at_exponent_minus_four_and_fourteen() {
        // Fixed window: exponent in [-4, 14].
        assert_eq!(cppnix_format_json_float(1.5), "1.5");
        assert_eq!(cppnix_format_json_float(0.1), "0.1");
        assert_eq!(cppnix_format_json_float(0.0001), "0.0001");
        assert_eq!(cppnix_format_json_float(0.00015), "0.00015");
        assert_eq!(cppnix_format_json_float(3.0e10), "30000000000.0");
        assert_eq!(cppnix_format_json_float(1.0e14), "100000000000000.0");
        // An integral float KEEPS its `.0`, or it reads back as an integer and
        // the value changes type on round-trip.
        assert_eq!(cppnix_format_json_float(1.0), "1.0");
        assert_eq!(cppnix_format_json_float(1_000_000.0), "1000000.0");
        assert_eq!(cppnix_format_json_float(123_456_789.0), "123456789.0");

        // Outside it: scientific, exponent signed and padded to two digits.
        // `1e-05` and `1e-06` are the rows serde_json got wrong in BOTH ways —
        // it kept fixed notation for the first and dropped the pad on the
        // second.
        assert_eq!(cppnix_format_json_float(0.00001), "1e-05");
        assert_eq!(cppnix_format_json_float(1.0e-6), "1e-06");
        assert_eq!(cppnix_format_json_float(2.5e-5), "2.5e-05");
        assert_eq!(cppnix_format_json_float(9.9e-5), "9.9e-05");
        assert_eq!(cppnix_format_json_float(1.0e15), "1e+15");
        assert_eq!(cppnix_format_json_float(1.23e15), "1.23e+15");
        assert_eq!(cppnix_format_json_float(1.0e100), "1e+100");
        assert_eq!(cppnix_format_json_float(1.0e-100), "1e-100");
        assert_eq!(cppnix_format_json_float(1_234_567_890_123_456.0), "1.234567890123456e+15");

        // Digits are shortest-round-trip, not a fixed precision.
        assert_eq!(cppnix_format_json_float(0.300_000_000_000_000_04), "0.30000000000000004");

        // nix drops the sign on a zero; `serde_json` would write `-0.0`.
        assert_eq!(cppnix_format_json_float(0.0), "0.0");
        assert_eq!(cppnix_format_json_float(-0.0), "0.0");
        assert_eq!(cppnix_format_json_float(-1.5), "-1.5");
        assert_eq!(cppnix_format_json_float(-2.5e-5), "-2.5e-05");
    }

    /// ★ CALIBRATION: the JSON rule and the VALUE rule must stay DISTINCT.
    ///
    /// The obvious "simplification" is to make one call the other. These rows
    /// are the counterexamples: the same f64 renders differently in the two
    /// contexts, so collapsing them silently corrupts one surface.
    #[test]
    fn json_and_value_float_formats_are_not_interchangeable() {
        // Only values where the two rules genuinely disagree belong here.
        // `0.0001` renders as `0.0001` under BOTH and was in this list until
        // the test itself said so — a calibration row that cannot distinguish
        // the two is worse than absent, because it looks like coverage.
        // Verified against nix: value vs JSON respectively —
        //   3.0e10   `3e+10` vs `30000000000.0`
        //   1.0      `1`     vs `1.0`
        //   1.0e14   `1e+14` vs `100000000000000.0`
        //   1000000  `1e+06` vs `1000000.0`
        // `0.0001` and `1.0e15` render IDENTICALLY under both and were in this
        // list until the test said so — twice. A calibration row that cannot
        // distinguish the two rules is worse than absent: it looks like
        // coverage while proving nothing.
        for v in [3.0e10, 1.0, 1.0e14, 1_000_000.0] {
            assert_ne!(
                cppnix_format_json_float(v),
                cppnix_format_float(v),
                "the JSON and value float formats agree on {v}, so this \
                 calibration no longer distinguishes them — check whether one \
                 was made to call the other"
            );
        }
    }

    /// The float hook must apply at every DEPTH, which is why it is a
    /// `Formatter` and not a patch at each construction site.
    #[test]
    fn nix_json_to_string_formats_floats_at_any_depth() {
        let v = serde_json::json!({ "a": [1.0e-6, { "b": 2.5e-5 }], "c": 1.0e15 });
        assert_eq!(
            nix_json_to_string(&v).expect("serialize"),
            r#"{"a":[1e-06,{"b":2.5e-05}],"c":1e+15}"#
        );
    }

    /// Scientific notation — the case the block above never covered, which is
    /// exactly why the missing exponent zero-pad survived. Every value here was
    /// read off real `nix eval` on 2026-08-18.
    #[test]
    fn cppnix_format_float_pads_exponent_to_two_digits() {
        // Single-digit exponents pad; this is the regression.
        assert_eq!(cppnix_format_float(123_456_789.0), "1.23457e+08");
        assert_eq!(cppnix_format_float(1_000_000.0), "1e+06");
        assert_eq!(cppnix_format_float(0.000_01), "1e-05");
        // Two digits are already wide enough — must NOT gain a third.
        assert_eq!(cppnix_format_float(3.0e10), "3e+10");
        // Three-digit exponents keep all three — a fixed width would truncate.
        assert_eq!(cppnix_format_float(1.0e100), "1e+100");
        assert_eq!(cppnix_format_float(1.0e-100), "1e-100");
        // The fixed-point/scientific boundary stays where %g puts it.
        assert_eq!(cppnix_format_float(999_999.0), "999999");
        assert_eq!(cppnix_format_float(-123_456_789.0), "-1.23457e+08");
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
