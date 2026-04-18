//! Oracle testing harness for sui using tatara-lisp as the corpus
//! authoring language.
//!
//! # What this is
//!
//! Test cases are authored in `.lisp` files under `tests/oracle_corpus/`
//! as `(defnix NAME :source "..." :expected JSON :tags [...])` forms.
//! tatara-lisp's `#[derive(TataraDomain)]` macro compiles each form into
//! a typed `NixProgramSpec`; the harness iterates those, runs `:source`
//! through `sui_eval::eval`, and diffs the JSON-serialized result
//! against `:expected`.
//!
//! # Why this is
//!
//! Sui's stated goal is "evaluate any valid Nix identically to CppNix."
//! A corpus of expected input/output pairs — independent of either
//! evaluator's implementation — is the only credible measurement of
//! progress toward that goal. Lisp authoring (tatara-lisp macros +
//! registered domains) gives us:
//!
//!  - One test expressed once, re-used as:
//!     * oracle case (run through sui, assert = expected)
//!     * fuzz seed (permute with property-based generators)
//!     * executable spec for missing builtins (write the test
//!       before implementing the builtin in Rust)
//!  - BLAKE3-attestable test corpus (future: each program gets a
//!    content hash that enters the sui attestation chain)
//!  - Homoiconic composition — Lisp macros can generate families of
//!    related programs from a single pattern
//!
//! # File layout
//!
//! ```
//! tests/
//!   oracle.rs                  # this file — harness + fuzz + specs
//!   oracle_corpus/
//!     01_primitives.lisp       # arith, bool, null, strings
//!     02_collections.lisp      # attrsets, lists
//!     03_bindings.lisp         # let, with, inherit
//!     04_overlays.lisp         # //, //? , merge chains
//!     05_builtins.lisp         # builtins.map/foldl'/filter/...
//!     99_executable_specs.lisp # TODO entries for missing builtins
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

/// A single oracle test case. The fields mirror the Lisp authoring
/// surface exactly; `#[derive(TataraDomain)]` turns this into the
/// `(defnix …)` keyword.
///
/// `expected_json` is a **raw JSON string** parsed at test time. Using
/// a string rather than `serde_json::Value` sidesteps the Sexp→JSON
/// round-trip ambiguity (tatara-lisp has no `{}` or `[]` literals; lists
/// use `()`, which in the Sexp→JSON bridge becomes either `Array` or
/// `Object` depending on kwargs-heuristic — too brittle for arbitrary
/// JSON). Authors write the JSON they mean; the harness parses it.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[tatara(keyword = "defnix")]
pub struct NixProgramSpec {
    /// The Nix source text to evaluate.
    pub source: String,
    /// The expected result as a raw JSON string. Parsed by the harness
    /// into `serde_json::Value` and diffed against
    /// `sui_eval::eval(source).to_json()`.
    pub expected_json: String,
    /// Optional categorization — `("arith" "trivial")` style. Used
    /// for selective running (e.g., skip flake-heavy tests offline).
    #[serde(default)]
    pub tags: Vec<String>,
    /// If set, skip this case. Use this to mark "executable spec"
    /// entries for builtins that aren't implemented yet — the form
    /// documents the expected behavior; the skip means the harness
    /// won't red-flag it until someone flips the flag.
    #[serde(default)]
    pub skip: bool,
    /// Human-readable rationale. Shown on failure. Optional.
    #[serde(default)]
    pub note: String,
}

/// Load every `.lisp` file under `tests/oracle_corpus/` and compile
/// each one into a stream of `NamedDefinition<NixProgramSpec>` via
/// `tatara_lisp::compile_named`.
fn load_corpus() -> Vec<tatara_lisp::NamedDefinition<NixProgramSpec>> {
    let corpus_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("oracle_corpus");

    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&corpus_dir)
        .unwrap_or_else(|e| panic!("corpus dir {}: {e}", corpus_dir.display()))
        .filter_map(|e| e.ok().map(|d| d.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("lisp"))
        .collect();
    paths.sort(); // Deterministic order — 01_ before 02_ etc.

    let mut out = Vec::new();
    for path in paths {
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let mut defs = tatara_lisp::compile_named::<NixProgramSpec>(&src)
            .unwrap_or_else(|e| panic!("compile {}: {e}", path.display()));
        for def in &mut defs {
            // Tag each definition with its source file for failure messages.
            def.name = format!(
                "{}::{}",
                path.file_stem().and_then(|s| s.to_str()).unwrap_or("?"),
                def.name
            );
        }
        out.extend(defs);
    }
    out
}

/// Convert a sui `Value` to a `serde_json::Value` via the existing
/// `to_json` serializer. sui's `to_json()` already returns a
/// `serde_json::Value` directly, so this is a thin wrapper kept for
/// naming clarity at the call site.
fn value_to_json(v: &sui_eval::Value) -> serde_json::Value {
    v.to_json()
}

// ──────────────────────────────────────────────────────────────────
// Oracle test — loads the corpus and runs every defnix through sui.
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
                        case.name,
                        case.spec.source,
                        expected,
                        actual,
                        case.spec.note,
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
// Fuzz generator — produces random well-formed Nix programs and
// asserts (a) they don't panic the evaluator, (b) repeat evaluations
// return the same result (determinism).
// ──────────────────────────────────────────────────────────────────
//
// Why in Rust not Lisp: tatara-lisp is Tier-0 (template macros only,
// no user-defined functions / closures / recursion). Random-program
// generation needs a real generator; this lives in Rust using proptest
// strategies. The *shape* of what we generate matches what Lisp
// authors write by hand in the corpus — so a Lisp-authored shrinking
// strategy could replace this later without a harness change.

mod fuzz {
    use proptest::prelude::*;

    /// Generate a Nix integer literal in a range that won't overflow
    /// i64 when summed in chains we build.
    fn arb_int() -> impl Strategy<Value = i64> {
        -1000i64..1000
    }

    /// Grammar:
    ///   expr := int
    ///         | expr + expr
    ///         | expr - expr
    ///         | if bool then expr else expr
    ///         | let name = expr; in <name>
    ///         | (expr)
    fn arb_expr() -> impl Strategy<Value = String> {
        let leaf = arb_int().prop_map(|n| n.to_string());
        leaf.prop_recursive(
            4,  // max depth
            32, // total node budget
            3,  // max branching
            |inner| {
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
            },
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 64,
            // Bound shrink search so failures report fast — the point
            // of this test is "does the evaluator crash on random
            // well-formed Nix," not "what's the minimum repro."
            max_shrink_iters: 256,
            ..ProptestConfig::default()
        })]

        /// No panics, no infinite loops: every generated program
        /// evaluates to Ok or Err in bounded time (no unwraps).
        #[test]
        fn evaluator_is_total_on_generated_arithmetic(src in arb_expr()) {
            let r = sui_eval::eval(&src);
            // We don't care Ok vs Err here — only that it returned.
            // A bug would look like a panic from a `.unwrap()`, stack
            // overflow, or hang (proptest kills after a timeout).
            let _ = r;
        }

        /// Determinism: evaluating the same source twice returns the
        /// same outcome. Catches subtle bugs like re-use of the
        /// global IMPORT_CACHE / interner state across calls.
        #[test]
        fn evaluation_is_deterministic(src in arb_expr()) {
            let a = sui_eval::eval(&src).map(|v| v.to_json()).ok();
            let b = sui_eval::eval(&src).map(|v| v.to_json()).ok();
            prop_assert_eq!(a, b);
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// Executable specs: tests for builtins that aren't implemented yet.
//
// These live in tests/oracle_corpus/99_executable_specs.lisp marked
// `:skip #t`. When you implement the builtin in sui, flip the skip
// flag — the spec becomes a passing test with zero additional work.
// ──────────────────────────────────────────────────────────────────

#[test]
fn executable_specs_parse_even_when_skipped() {
    // Smoke test: even the skipped corpus entries must be
    // well-formed Lisp, well-formed NixProgramSpecs. Catches typos
    // in the spec document without requiring sui to implement them.
    let cases = load_corpus();
    let skipped_count = cases.iter().filter(|c| c.spec.skip).count();
    // Every skipped case must have a `note` explaining why.
    for case in &cases {
        if case.spec.skip {
            assert!(
                !case.spec.note.is_empty(),
                "{}: skipped cases must carry a :note explaining why",
                case.name
            );
        }
    }
    // Report for visibility — not a failure.
    eprintln!("executable specs registered (skipped): {skipped_count}");
}
