//! Oracle testing harness for sui using tatara-lisp as the corpus
//! authoring language.
//!
//! # What this is
//!
//! Test cases are authored in `.lisp` files under `tests/oracle_corpus/`
//! as `(defnix NAME :source "..." :expected-json "..." :tags [...])`
//! forms. tatara-lisp's `#[derive(TataraDomain)]` macro compiles each
//! form into a typed `NixProgramSpec`; the harness iterates those,
//! runs `:source` through `sui_eval::eval`, and diffs the JSON-
//! serialized result against `:expected-json`.
//!
//! Two modes:
//!
//! - **Author-defined oracle** (always on). Every `defnix` has an
//!   `:expected-json` the author curated. Cheap, offline, exact.
//! - **Differential oracle against CppNix** (opt-in via
//!   `SUI_TEST_ONLINE=1`). Runs `:source` through both sui and the
//!   system `nix-instantiate` binary and asserts they agree. Protects
//!   against drift from CppNix semantics even when authors miswrite
//!   `:expected-json`. Silently no-ops when `nix-instantiate` isn't
//!   on PATH so non-Nix machines stay green.

mod common;

use common::{load_corpus, value_to_json};

// ──────────────────────────────────────────────────────────────────
// Author-defined oracle — every `defnix` has a human-curated
// `:expected-json` field. Always runs. Offline, fast, exact.
// ──────────────────────────────────────────────────────────────────

#[test]
fn oracle_corpus_matches_expected() {
    let cases = load_corpus();
    assert!(
        !cases.is_empty(),
        "no (defnix …) forms found under tests/oracle_corpus/ — did you add a .lisp file?"
    );

    let mut failures: Vec<String> = Vec::new();
    let mut skipped = 0_usize;
    let mut ok = 0_usize;

    for case in cases {
        if case.spec.skip {
            skipped += 1;
            continue;
        }

        let expected: serde_json::Value = match serde_json::from_str(&case.spec.expected_json) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!(
                    "{}\n   malformed :expected-json\n   source:        {}\n   expected-json: {:?}\n   parse err:     {}",
                    case.name, case.spec.source, case.spec.expected_json, e,
                ));
                continue;
            }
        };

        let result = sui_eval::eval(&case.spec.source);
        match result {
            Ok(v) => {
                let actual = value_to_json(&v);
                if actual != expected {
                    failures.push(format!(
                        "{}\n   source:   {}\n   expected: {}\n   actual:   {}\n   note:     {}",
                        case.name, case.spec.source, expected, actual, case.spec.note,
                    ));
                } else {
                    ok += 1;
                }
            }
            Err(e) => {
                failures.push(format!(
                    "{}\n   source:   {}\n   eval err: {}\n   note:     {}",
                    case.name, case.spec.source, e, case.spec.note,
                ));
            }
        }
    }

    if !failures.is_empty() {
        panic!(
            "oracle: {} passed, {} skipped, {} FAILED\n\n{}",
            ok,
            skipped,
            failures.len(),
            failures.join("\n\n")
        );
    }
    eprintln!("oracle: {ok} passed, {skipped} skipped (executable-spec TODO)");
}

// ──────────────────────────────────────────────────────────────────
// Differential oracle — runs every (non-skipped) corpus entry
// through BOTH sui and system `nix-instantiate`, asserts agreement.
//
// Silently no-ops when:
//   - SUI_TEST_ONLINE is not set (default, preserves CI green)
//   - nix-instantiate is not on PATH
//
// This is Track-B follow-up #3 from the plan: "Hook up CppNix as
// second oracle." The infra for it was already in common/ — this
// test wires it to the corpus.
// ──────────────────────────────────────────────────────────────────

#[test]
fn oracle_corpus_matches_cppnix() {
    if common::skip_if_offline("oracle_corpus_matches_cppnix") {
        return;
    }

    let cases = load_corpus();
    let mut ok = 0_usize;
    let mut mismatches: Vec<String> = Vec::new();

    for case in cases {
        if case.spec.skip {
            continue;
        }
        // Tests tagged `sui-extension` exercise builtins that don't
        // exist in CppNix (e.g. `resolveFlakeRef`, sui-specific
        // introspection). Skip them in the differential — they'd
        // diverge by design. The author-curated oracle still covers
        // them.
        if case.spec.tags.iter().any(|t| t == "sui-extension") {
            continue;
        }
        let sui_out = common::sui_eval_json(&case.spec.source);
        let nix_out = common::nix_eval_json(&case.spec.source);
        if sui_out != nix_out {
            mismatches.push(format!(
                "{}\n   source: {}\n   sui:    {}\n   nix:    {}",
                case.name,
                case.spec.source,
                serde_json::to_string(&sui_out).unwrap_or_default(),
                serde_json::to_string(&nix_out).unwrap_or_default(),
            ));
        } else {
            ok += 1;
        }
    }

    if !mismatches.is_empty() {
        panic!(
            "differential oracle: {} agreed, {} DIVERGED\n\n{}",
            ok,
            mismatches.len(),
            mismatches.join("\n\n")
        );
    }
    eprintln!("differential oracle: {ok} programs agreed with nix-instantiate");
}

// ──────────────────────────────────────────────────────────────────
// Fuzz harness — proptest-generated random Nix programs asserting
// (a) no panics and (b) determinism.
// ──────────────────────────────────────────────────────────────────

mod fuzz {
    use proptest::prelude::*;

    fn arb_int() -> impl Strategy<Value = i64> {
        -1000i64..1000
    }

    fn arb_expr() -> impl Strategy<Value = String> {
        let leaf = arb_int().prop_map(|n| n.to_string());
        leaf.prop_recursive(4, 32, 3, |inner| {
            prop_oneof![
                (inner.clone(), inner.clone())
                    .prop_map(|(a, b)| format!("({a} + {b})")),
                (inner.clone(), inner.clone())
                    .prop_map(|(a, b)| format!("({a} - {b})")),
                (any::<bool>(), inner.clone(), inner.clone())
                    .prop_map(|(c, t, e)| {
                        format!("(if {} then {t} else {e})", if c { "true" } else { "false" })
                    }),
                (inner.clone(), inner)
                    .prop_map(|(bind, _body)| format!("(let x = {bind}; in x)")),
            ]
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 64,
            max_shrink_iters: 256,
            ..ProptestConfig::default()
        })]

        #[test]
        fn evaluator_is_total_on_generated_arithmetic(src in arb_expr()) {
            let r = sui_eval::eval(&src);
            let _ = r;
        }

        #[test]
        fn evaluation_is_deterministic(src in arb_expr()) {
            let a = sui_eval::eval(&src).map(|v| v.to_json()).ok();
            let b = sui_eval::eval(&src).map(|v| v.to_json()).ok();
            prop_assert_eq!(a, b);
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// Executable-spec sanity check — skipped entries must still be well-
// formed and carry a :note explaining the reason.
// ──────────────────────────────────────────────────────────────────

#[test]
fn executable_specs_parse_even_when_skipped() {
    let cases = load_corpus();
    let skipped_count = cases.iter().filter(|c| c.spec.skip).count();
    for case in &cases {
        if case.spec.skip {
            assert!(
                !case.spec.note.is_empty(),
                "{}: skipped cases must carry a :note explaining why",
                case.name
            );
        }
    }
    eprintln!("executable specs registered (skipped): {skipped_count}");
}
