//! `SUI_NORMALIZE=1` makes the tree-walker's `rec` attrsets merge the way nix
//! does — by splicing into the first-declared node.
//!
//! # The defect
//!
//! sui decided duplicate-key merge-vs-overwrite at EVAL time from the runtime
//! VALUE; nix decides it at PARSE time from SYNTAX. In the `rec` branch that
//! showed up as a one-line asymmetry — Phase 1b does a destructive
//! `attrs.insert` where the non-rec branch calls `merge_nested_insert` — and
//! the result was silent key loss on legal nix:
//!
//! ```text
//! rec { o = {e=1;}; o.x = 2; }
//!   nix   { o = { e = 1; x = 2; }; }
//!   sui   { o = { x = 2; }; }          <- `e` gone, exit 0, no error
//! ```
//!
//! # Why these tests shell out
//!
//! `normalize_env::enabled()` reads `SUI_NORMALIZE` through a `OnceLock`, a
//! one-way latch matching `resolve_env`/`perf`. A test that called
//! `std::env::set_var` mid-process would be a no-op the moment any earlier
//! test had already read it — and would pass or fail depending on test
//! ORDER, which is worse than no test. A subprocess gets a fresh latch.
//!
//! # Why nix is consulted rather than a hardcoded expectation
//!
//! These are parity assertions. A vendored expectation is a second place for
//! the truth to live, and this whole class of bug came from sui's model of
//! nix drifting from nix.

use assert_cmd::Command;

/// The oracle's answer, or `None` when nix is unavailable.
fn nix_eval(expr: &str) -> Option<String> {
    let out = std::process::Command::new("nix")
        .args(["eval", "--impure", "--expr", expr])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn sui_eval(expr: &str, normalize: bool) -> String {
    let mut cmd = Command::cargo_bin("sui").expect("cargo_bin sui");
    if normalize {
        cmd.env("SUI_NORMALIZE", "1");
    }
    let out = cmd.args(["eval", "-E", expr]).output().expect("run sui");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Every shape here is LEGAL nix that sui answered wrongly, at exit 0.
const REC_MERGE_CASES: &[&str] = &[
    // A dotted path merging into an earlier full-set literal.
    "rec { o = {e=1;}; o.x = 2; }",
    // Two full-set literals.
    "rec { a = {b=1;}; a = {c=2;}; }",
    // The reverse order, AND a sibling that reads through the merge. This one
    // was not merely a dropped key — it was `<<thunk:error>>`, a loud wrong
    // answer, because `a.b` did not exist after the clobber.
    "rec { a.c = 2; a = {b=1;}; x = a.b; }",
    // Plain dotted merge — correct before AND after; it is here so a
    // regression in the common case cannot hide behind the fixes.
    "rec { a.b = 1; a.c = 2; }",
    // The merged node must stay rec: `y` reads a sibling introduced by the
    // OTHER half of the merge.
    "rec { a = {b=1;}; a = {c=2;}; y = a.b + a.c; }",
];

#[test]
fn rec_attrsets_merge_like_nix_under_the_flag() {
    let Some(_) = nix_eval("1") else {
        eprintln!("rec_attrsets_merge_like_nix_under_the_flag: skipped (no usable nix)");
        return;
    };
    for expr in REC_MERGE_CASES {
        let Some(want) = nix_eval(expr) else {
            panic!("{expr}: the oracle refused a legal expression");
        };
        let got = sui_eval(expr, true);
        assert_eq!(
            got, want,
            "\n{expr}\n  nix: {want}\n  sui: {got}\n\
             `SUI_NORMALIZE=1` must reproduce nix's parse-time splice."
        );
    }
}

/// ★ ANTI-VACUITY. If the flag changed nothing, the test above would pass by
/// coincidence — so assert the flag is load-bearing: at least one case must
/// differ with it OFF. Without this row, deleting the wiring entirely would
/// leave the suite green the moment sui happened to agree for other reasons.
#[test]
fn the_flag_is_load_bearing() {
    let Some(_) = nix_eval("1") else {
        eprintln!("the_flag_is_load_bearing: skipped (no usable nix)");
        return;
    };
    let differing = REC_MERGE_CASES
        .iter()
        .filter(|e| sui_eval(e, false) != sui_eval(e, true))
        .count();
    assert!(
        differing >= 3,
        "only {differing} of {} cases differ between SUI_NORMALIZE off and on. \
         These cases exist because the flag CHANGES them; if they now agree \
         either the wiring is dead or the cases stopped covering the defect.",
        REC_MERGE_CASES.len()
    );
}

/// The default must be unchanged. Landing the normalizer behind a flag is only
/// safe if the flag is genuinely off by default — this is the row that says so.
#[test]
fn the_default_path_is_untouched() {
    // A plain attrset nobody normalizes: identical either way, and identical
    // to nix.
    let expr = "{ a = 1; b = { c = 2; }; }";
    let Some(want) = nix_eval(expr) else {
        eprintln!("the_default_path_is_untouched: skipped (no usable nix)");
        return;
    };
    assert_eq!(sui_eval(expr, false), want);
    assert_eq!(sui_eval(expr, true), want);
}
