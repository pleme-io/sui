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

        // Error-case path: spec declared `expected-error`, so we must
        // FAIL with a message containing that substring.
        if !case.spec.expected_error.is_empty() {
            let needle = case.spec.expected_error.to_lowercase();
            match sui_eval::eval(&case.spec.source) {
                Ok(v) => {
                    failures.push(format!(
                        "{}\n   source:         {}\n   expected-error: {:?}\n   actual:         Ok({})\n   note:           {}",
                        case.name,
                        case.spec.source,
                        case.spec.expected_error,
                        value_to_json(&v),
                        case.spec.note,
                    ));
                }
                Err(e) => {
                    let msg = e.to_string().to_lowercase();
                    if msg.contains(&needle) {
                        ok += 1;
                    } else {
                        failures.push(format!(
                            "{}\n   source:         {}\n   expected-error: {:?}\n   actual-error:   {}\n   note:           {}",
                            case.name, case.spec.source, case.spec.expected_error, e, case.spec.note,
                        ));
                    }
                }
            }
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

        // Error-case path: both engines must error, and the two
        // error messages must share the `:expected-error` substring.
        // This catches the "sui returns Ok but CppNix errors" class
        // of bug — e.g. the `let x = x; in x` silent-Ok regression
        // fixed at ac7ce0a.
        if !case.spec.expected_error.is_empty() {
            let needle = case.spec.expected_error.to_lowercase();
            let sui_res = sui_eval::eval(&case.spec.source);
            let nix_out = common::nix_eval_json(&case.spec.source);
            let nix_errored = common::is_error_json(&nix_out);
            match sui_res {
                Ok(v) => {
                    mismatches.push(format!(
                        "{}\n   source:         {}\n   expected-error: {:?}\n   sui:            Ok({})\n   nix:            {}",
                        case.name,
                        case.spec.source,
                        case.spec.expected_error,
                        common::value_to_json(&v),
                        if nix_errored { "errored (expected)" } else { "unexpectedly ok" },
                    ));
                }
                Err(e) => {
                    let sui_msg = e.to_string().to_lowercase();
                    let nix_msg = nix_out
                        .get("__error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_lowercase();
                    let sui_has = sui_msg.contains(&needle);
                    let nix_has = nix_msg.contains(&needle);
                    if !nix_errored {
                        mismatches.push(format!(
                            "{}\n   source:         {}\n   expected-error: {:?}\n   sui:            {e}\n   nix:            returned a value, not an error",
                            case.name, case.spec.source, case.spec.expected_error,
                        ));
                    } else if !sui_has || !nix_has {
                        mismatches.push(format!(
                            "{}\n   source:         {}\n   expected-error: {:?}\n   sui match?      {sui_has}\n   nix match?      {nix_has}\n   sui msg:        {e}\n   nix msg:        {nix_msg}",
                            case.name, case.spec.source, case.spec.expected_error,
                        ));
                    } else {
                        ok += 1;
                    }
                }
            }
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

// ──────────────────────────────────────────────────────────────────
// getFlake integration — exercises the end-to-end indirect → resolve
// → fetch → evaluate chain against real github. Opt-in via the same
// SUI_TEST_ONLINE gate as the differential oracle (network-dependent).
// ──────────────────────────────────────────────────────────────────

#[test]
fn getflake_indirect_resolves_and_fetches() {
    if common::skip_if_offline("getflake_indirect_resolves_and_fetches") {
        return;
    }
    // Using `flake-utils` rather than `nixpkgs` because the nixpkgs
    // tarball is ~60 MB and unzips to ~250 MB of text. flake-utils is
    // tiny (~20 KB) and has a stable top-level flake.nix with a
    // non-empty description. Routes through:
    //   parseFlakeRef → registry lookup → github archive fetch →
    //   tar extract → evaluate_flake → attrset with `description`.
    let result = sui_eval::eval(r#"(builtins.getFlake "flake-utils").description"#);
    match result {
        Ok(sui_eval::Value::String(s)) => {
            assert!(!s.as_str().is_empty(), "description should be non-empty");
            eprintln!("getFlake flake-utils description: {}", s.as_str());
        }
        Ok(other) => panic!("expected string description, got {other:?}"),
        Err(e) => panic!("getFlake chain failed: {e}"),
    }
}

#[test]
fn getflake_github_scheme_fetches() {
    if common::skip_if_offline("getflake_github_scheme_fetches") {
        return;
    }
    // Exercises the github: scheme directly (no registry hop). Same
    // end-to-end chain past the parse step — proves the refactored
    // dispatch doesn't regress the concrete-ref path.
    let result =
        sui_eval::eval(r#"(builtins.getFlake "github:numtide/flake-utils").description"#);
    assert!(
        matches!(result, Ok(sui_eval::Value::String(_))),
        "github:numtide/flake-utils should fetch + evaluate cleanly, got {result:?}"
    );
}

#[test]
fn getflake_attrset_input_works() {
    if common::skip_if_offline("getflake_attrset_input_works") {
        return;
    }
    // Exercises the attrset-input path: CppNix's getFlake accepts a
    // pre-parsed ref attrset in addition to the string form. This
    // proves the normalize step at the top of getFlake handles both.
    let result = sui_eval::eval(
        r#"(builtins.getFlake { type = "github"; owner = "numtide"; repo = "flake-utils"; }).description"#,
    );
    assert!(
        matches!(result, Ok(sui_eval::Value::String(_))),
        "attrset-form getFlake should work, got {result:?}"
    );
}

/// Probe what's actually reachable on a fetched flake. Each sub-assert
/// uses its own eval so one failure doesn't mask the others — the
/// harness emits a human-readable report even on green. Real flakes
/// access `.outputs`, `.lib.*`, `.inputs.*` etc., so any gap here is
/// a gap blocking "drop-in replacement" status.
#[test]
fn getflake_return_shape_is_usable() {
    if common::skip_if_offline("getflake_return_shape_is_usable") {
        return;
    }
    let probes: &[(&str, &str)] = &[
        // Shallow shape ──────────────────────────────────────────
        ("description",           r#"builtins.isString (builtins.getFlake "flake-utils").description"#),
        ("outPath",               r#"builtins.isString (builtins.getFlake "flake-utils").outPath"#),
        ("has lib",               r#"builtins.hasAttr "lib" (builtins.getFlake "flake-utils")"#),
        ("lib is attrs",          r#"builtins.isAttrs (builtins.getFlake "flake-utils").lib"#),
        ("lib.eachDefaultSystem", r#"builtins.isFunction (builtins.getFlake "flake-utils").lib.eachDefaultSystem"#),
        ("inputs attrset",        r#"builtins.isAttrs (builtins.getFlake "flake-utils").inputs"#),
        // Actual output USE — the real "is this a drop-in" signal
        ("apply eachDefaultSystem",
          r#"let u = (builtins.getFlake "flake-utils").lib;
                 r = u.eachDefaultSystem (system: { hello = "world-${system}"; });
             in builtins.isAttrs r"#),
        ("eachDefaultSystem produces per-system keys",
          r#"let u = (builtins.getFlake "flake-utils").lib;
                 r = u.eachDefaultSystem (system: { hello = "hi"; });
             in builtins.hasAttr "hello" r"#),
        ("lib.defaultSystems is list",
          r#"builtins.isList (builtins.getFlake "flake-utils").lib.defaultSystems"#),
        // Metadata
        ("outPath under /tmp or /nix or similar",
          r#"let p = (builtins.getFlake "flake-utils").outPath;
             in builtins.substring 0 1 p == "/"
          "#),
    ];
    let mut results: Vec<(String, Result<bool, String>)> = Vec::new();
    for (name, src) in probes {
        let r = sui_eval::eval(src);
        let ok = match r {
            Ok(sui_eval::Value::Bool(b)) => Ok(b),
            Ok(other) => Err(format!("non-bool: {other:?}")),
            Err(e) => Err(e.to_string()),
        };
        results.push((name.to_string(), ok));
    }
    eprintln!("\ngetFlake return-shape probe:");
    let mut failed = Vec::new();
    for (name, r) in &results {
        match r {
            Ok(true) => eprintln!("  ✓ {name}"),
            Ok(false) => {
                eprintln!("  ✗ {name}: predicate returned false");
                failed.push(name.clone());
            }
            Err(e) => {
                eprintln!("  ✗ {name}: {e}");
                failed.push(name.clone());
            }
        }
    }
    // Minimum bar: description + outPath + has-lib MUST work. The
    // deeper ones (lib.eachDefaultSystem, inputs attrset, actual
    // function application) are how we discover real gaps — if a
    // probe fails, it's a precise TODO.
    let required = ["description", "outPath", "has lib"];
    for req in required {
        let passed = matches!(
            results.iter().find(|(n, _)| n == req).map(|(_, r)| r),
            Some(Ok(true))
        );
        assert!(passed, "required probe '{req}' failed — see log above");
    }
    if !failed.is_empty() {
        eprintln!(
            "\nnon-required probes failing (each is a next-session target): {}",
            failed.join(", ")
        );
    } else {
        eprintln!("\nall probes green — getFlake return shape is production-usable on flake-utils.");
    }
}

/// The harder test: fetch a flake that declares inputs in its own
/// flake.lock and verify sui resolves + fetches those transitively.
/// flake-parts imports nothing from nixpkgs but has a `nixpkgs-lib`
/// input that's fetched as a tarball. If sui's flake.lock reader
/// + transitive fetcher work, `.inputs.nixpkgs-lib.outPath` should
/// be a non-empty string path.
#[test]
fn getflake_resolves_transitive_inputs() {
    if common::skip_if_offline("getflake_resolves_transitive_inputs") {
        return;
    }
    let probes: &[(&str, &str)] = &[
        ("flake-parts.inputs is attrset",
          r#"builtins.isAttrs (builtins.getFlake "flake-parts").inputs"#),
        ("flake-parts.inputs has nixpkgs-lib",
          r#"builtins.hasAttr "nixpkgs-lib" (builtins.getFlake "flake-parts").inputs"#),
        ("nixpkgs-lib.outPath is string",
          r#"builtins.isString (builtins.getFlake "flake-parts").inputs.nixpkgs-lib.outPath"#),
    ];
    let mut failed = Vec::new();
    eprintln!("\ntransitive-inputs probe (flake-parts):");
    for (name, src) in probes {
        match sui_eval::eval(src) {
            Ok(sui_eval::Value::Bool(true)) => eprintln!("  ✓ {name}"),
            Ok(other) => {
                eprintln!("  ✗ {name}: {other:?}");
                failed.push(*name);
            }
            Err(e) => {
                eprintln!("  ✗ {name}: {e}");
                failed.push(*name);
            }
        }
    }
    if !failed.is_empty() {
        eprintln!(
            "\n{} transitive-input probes failed — each is a targeted gap",
            failed.len()
        );
    }
    // Don't assert-fail — these are exploratory. The log is the signal.
}

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
