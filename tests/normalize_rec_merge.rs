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

/// `normalize = false` must now set `SUI_NORMALIZE=0` explicitly: the default
/// FLIPPED to on (2026-08-18), so "no env var" means ON. Leaving this as
/// "unset the var" would have quietly turned every off-vs-on comparison into
/// on-vs-on — the anti-vacuity row below is what catches that.
fn sui_eval(expr: &str, normalize: bool) -> String {
    let mut cmd = Command::cargo_bin("sui").expect("cargo_bin sui");
    cmd.env("SUI_NORMALIZE", if normalize { "1" } else { "0" });
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

    // ── `let` — same rule, and sui never implemented it ──────────────────
    "let a = {b=1;}; a = {c=2;}; in a",
    "let a = {b=1;}; a.c = 2; in a",
    "let a.c = 2; a = {b=1;}; in a",

    // ── legacy `let { … }` — silently DISCARDED multi-segment paths, and
    // two of these were hard `UndefinedVar` errors, not just lost keys ────
    "let { a.b = 1; a.c = 2; body = a; }",
    "let { a = {b=1;}; a.c = 2; body = a; }",
    "let { body = a; a.b = 1; }",

    // ── the non-rec branch: RE-SCOPING, which no value merge can do ──────
    //
    // The second side's bindings become bindings OF THE FIRST NODE, so they
    // are scoped by it. The first was `c=99` (reading the merged-in `b`
    // instead of the outer one, because the later `rec` must be DISCARDED);
    // the second was `c=5` (missing that the second side's `b=9` lands in the
    // first node's rec scope).
    "let b=1; in { a={x=2;}; a=rec{b=99;c=b;}; }",
    "let b=5; in { a=rec{c=b;}; a={b=9;}; }",

    // ★ THE ACCEPTANCE CASE, vendored in the corpus as
    // `known_broken/eval-okay-regrettable-rec-attrset-merge.nix` with
    // `.exp = 6`. A dotted path splices INTO a `rec` literal; the spliced
    // member resolves `d` from inside that rec scope; and the rec body's `b`
    // reads `c` from the spliced-in member. Mutual recursion ACROSS the merge
    // boundary — nothing short of a real splice produces it. sui answered
    // `UndefinedVar 'c'`.
    "{ a = rec { b = c + 1; d = 2; }; a.c = d + 3; }.a.b",
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

/// ★ ANTI-VACUITY, and it now guards two things. If plan-driven construction
/// changed nothing, the test above would pass by coincidence — so at least ten
/// cases must differ with `SUI_NORMALIZE=0`. Since the default flipped, this
/// row ALSO proves the opt-out latch is still live: if `SUI_NORMALIZE=0`
/// stopped disabling the pass, every case would agree and this fails.
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
        differing >= 10,
        "only {differing} of {} cases differ between SUI_NORMALIZE off and on. \
         These cases exist because the flag CHANGES them; if they now agree \
         either the wiring is dead or the cases stopped covering the defect.",
        REC_MERGE_CASES.len()
    );
}

/// A group that needs no normalization must be identical either way — and
/// identical to nix. The pass may only touch groups with a duplicate key or a
/// dotted path; this is the row that says the other 66% of the fleet's
/// attrsets are untouched.
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
