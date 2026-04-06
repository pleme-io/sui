//! Layer 10: property-based tests for sui's evaluator.
//!
//! These tests encode invariants we know from the Nix spec and
//! proptest generates random inputs to attack them. No oracle — all
//! of these run offline.
//!
//! Scale: default proptest config is 256 cases per property, so this
//! file contributes ~2000 assertions per run at ~1 ms each.
//!
//! Properties covered:
//!   - integer addition commutativity + identity
//!   - integer addition associativity
//!   - integer multiplication identity + zero
//!   - negation is an involution
//!   - string concatenation associativity
//!   - string concat is `stringLength`-additive
//!   - list concat length invariant
//!   - list concat associativity
//!   - `[]` is an identity for list concat
//!   - right-biased `//` merge
//!   - `attrNames` of merge is union of sides
//!   - `toJSON ∘ fromJSON` is identity on a JSON-representable subset
//!   - `builtins.length (builtins.genList id n) == n`
//!   - `builtins.elemAt (builtins.genList id n) i == i` for i < n

mod common;

use proptest::prelude::*;

/// Evaluate an expression, unwrapping the result as a JSON value.
/// Used as a tiny helper so individual properties are one-liners.
fn eval(expr: &str) -> serde_json::Value {
    common::sui_eval_json(expr)
}

// ── Integer arithmetic ──────────────────────────────────────────────

proptest! {
    #[test]
    fn int_addition_commutative(a: i32, b: i32) {
        // Constrain to i32 so shelling out to a Nix expression stays simple.
        let lhs = eval(&format!("{a} + {b}"));
        let rhs = eval(&format!("{b} + {a}"));
        prop_assert_eq!(lhs, rhs);
    }

    #[test]
    fn int_addition_associative(a: i32, b: i32, c: i32) {
        let l = eval(&format!("({a} + {b}) + {c}"));
        let r = eval(&format!("{a} + ({b} + {c})"));
        prop_assert_eq!(l, r);
    }

    #[test]
    fn int_zero_is_additive_identity(a: i32) {
        let e = eval(&format!("{a} + 0"));
        prop_assert_eq!(e, serde_json::json!(a));
    }

    #[test]
    fn int_one_is_multiplicative_identity(a: i32) {
        let e = eval(&format!("{a} * 1"));
        prop_assert_eq!(e, serde_json::json!(a));
    }

    #[test]
    fn int_zero_multiplication(a: i32) {
        let e = eval(&format!("{a} * 0"));
        prop_assert_eq!(e, serde_json::json!(0));
    }

    #[test]
    fn int_double_negation(a: i32) {
        let e = eval(&format!("(-(-({a})))"));
        prop_assert_eq!(e, serde_json::json!(a));
    }
}

// ── Strings ─────────────────────────────────────────────────────────

/// Produce a printable ASCII string suitable for embedding in a Nix
/// string literal — no backslashes, no `"`, no `${`, no control chars.
/// This keeps `format!("\"{}\"", s)` safe without escaping logic.
fn safe_str() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _:/@.,-]{0,12}".prop_map(String::from)
}

proptest! {
    #[test]
    fn string_concat_associative(
        a in safe_str(),
        b in safe_str(),
        c in safe_str(),
    ) {
        let lhs = eval(&format!(r#"("{a}" + "{b}") + "{c}""#));
        let rhs = eval(&format!(r#""{a}" + ("{b}" + "{c}")"#));
        prop_assert_eq!(lhs, rhs);
    }

    #[test]
    fn string_concat_length_additive(a in safe_str(), b in safe_str()) {
        let total = eval(&format!(r#"builtins.stringLength ("{a}" + "{b}")"#));
        let expected = a.len() + b.len();
        prop_assert_eq!(total, serde_json::json!(expected));
    }

    #[test]
    fn string_length_matches_rust(s in safe_str()) {
        let got = eval(&format!(r#"builtins.stringLength "{s}""#));
        prop_assert_eq!(got, serde_json::json!(s.len()));
    }
}

// ── Lists ───────────────────────────────────────────────────────────

/// Generate a Nix-source list literal of random ints with len <= 8.
fn int_list() -> impl Strategy<Value = (Vec<i32>, String)> {
    prop::collection::vec(any::<i32>(), 0..8)
        .prop_map(|v| {
            // Emit each element wrapped in parens so negatives parse.
            let src = format!(
                "[{}]",
                v.iter()
                    .map(|n| format!("({n})"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            (v, src)
        })
}

proptest! {
    #[test]
    fn list_concat_length_invariant((xs, xs_src) in int_list(), (ys, ys_src) in int_list()) {
        let got = eval(&format!("builtins.length ({xs_src} ++ {ys_src})"));
        let expected = xs.len() + ys.len();
        prop_assert_eq!(got, serde_json::json!(expected));
    }

    #[test]
    fn list_empty_is_concat_identity((_xs, xs_src) in int_list()) {
        let left = eval(&format!("{xs_src} ++ []"));
        let right = eval(&format!("[] ++ {xs_src}"));
        let direct = eval(&xs_src);
        prop_assert_eq!(left, direct.clone());
        prop_assert_eq!(right, direct);
    }

    #[test]
    fn list_concat_associative(
        (_, a) in int_list(),
        (_, b) in int_list(),
        (_, c) in int_list(),
    ) {
        let l = eval(&format!("({a} ++ {b}) ++ {c}"));
        let r = eval(&format!("{a} ++ ({b} ++ {c})"));
        prop_assert_eq!(l, r);
    }

    #[test]
    fn gen_list_length(n in 0u32..20) {
        let got = eval(&format!("builtins.length (builtins.genList (x: x) {n})"));
        prop_assert_eq!(got, serde_json::json!(n));
    }

    #[test]
    fn gen_list_element_is_index(n in 1u32..20) {
        let i = n - 1;
        let got = eval(&format!(
            "builtins.elemAt (builtins.genList (x: x) {n}) {i}"
        ));
        prop_assert_eq!(got, serde_json::json!(i));
    }
}

// ── Attribute sets ──────────────────────────────────────────────────

/// A small set of valid Nix attribute names. Using a fixed alphabet
/// keeps source generation trivial.
fn attr_name() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "c", "d", "e", "f"]).prop_map(String::from)
}

/// Generate an attrset source like `{ a = 1; b = 2; }` with random
/// subset of names from the fixed alphabet and random i32 values.
fn attrset() -> impl Strategy<Value = (Vec<(String, i32)>, String)> {
    prop::collection::btree_map(attr_name(), any::<i32>(), 0..4).prop_map(|m| {
        let v: Vec<(String, i32)> = m.into_iter().collect();
        let src = format!(
            "{{{} }}",
            v.iter()
                .map(|(k, val)| format!(" {k} = ({val});"))
                .collect::<Vec<_>>()
                .join("")
        );
        (v, src)
    })
}

proptest! {
    #[test]
    fn attrset_right_biased_merge(
        (left_pairs, left_src) in attrset(),
        (right_pairs, right_src) in attrset(),
    ) {
        // Check every key present in `right`: after `left // right`,
        // that key must equal its value from `right`, not `left`.
        let merged_src = format!("{left_src} // {right_src}");
        for (k, v) in &right_pairs {
            let got = eval(&format!("({merged_src}).{k}"));
            prop_assert_eq!(got, serde_json::json!(v));
        }
        // Keys only in `left` must survive untouched.
        let right_keys: std::collections::BTreeSet<&String> =
            right_pairs.iter().map(|(k, _)| k).collect();
        for (k, v) in &left_pairs {
            if !right_keys.contains(k) {
                let got = eval(&format!("({merged_src}).{k}"));
                prop_assert_eq!(got, serde_json::json!(v));
            }
        }
    }

    #[test]
    fn attrset_merge_key_union(
        (left_pairs, left_src) in attrset(),
        (right_pairs, right_src) in attrset(),
    ) {
        let got = eval(&format!("builtins.attrNames ({left_src} // {right_src})"));
        let mut expected: Vec<String> =
            left_pairs.iter().map(|(k, _)| k.clone()).collect();
        for (k, _) in &right_pairs {
            if !expected.contains(k) {
                expected.push(k.clone());
            }
        }
        expected.sort();
        prop_assert_eq!(got, serde_json::json!(expected));
    }
}

// ── JSON roundtrip ──────────────────────────────────────────────────

proptest! {
    #[test]
    fn json_int_roundtrip(n: i32) {
        // Produce a JSON literal, feed to fromJSON, check equality
        let src = format!(r#"builtins.fromJSON "{n}""#);
        prop_assert_eq!(eval(&src), serde_json::json!(n));
    }

    #[test]
    fn json_bool_roundtrip(b: bool) {
        let src = format!(r#"builtins.fromJSON "{b}""#);
        prop_assert_eq!(eval(&src), serde_json::json!(b));
    }

    #[test]
    fn json_list_roundtrip(xs in prop::collection::vec(any::<i32>(), 0..6)) {
        let json_literal = serde_json::to_string(&xs).unwrap();
        // Escape backslashes and quotes for embedding in a Nix string.
        let escaped = json_literal.replace('\\', "\\\\").replace('"', "\\\"");
        let src = format!(r#"builtins.fromJSON "{escaped}""#);
        prop_assert_eq!(eval(&src), serde_json::json!(xs));
    }
}
