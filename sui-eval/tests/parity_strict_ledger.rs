//! `SUI_PARITY_STRICT` must not report "clean eval" while it is dropping attrs.
//!
//! The ledger un-blinds the places `derivation` swallows a failure rather than
//! propagating it. It had three record sites and **four** drop sites: the
//! `__structuredAttrs` `__json` loop encoded each attr with
//! `if let Ok(jv) = …to_json_with_context(…)`, and the `Err` arm had no `else`.
//! A value that forced fine but could not be JSON-encoded vanished from
//! `__json` with no record anywhere — and a strict run printed
//! `no swallowed force-error drops (clean eval)` over it.
//!
//! That is worse than the flat-env sibling it mirrors, not better:
//! `__structuredAttrs` is set by every modern `mkDerivation`, and `__json` is
//! an env var the derivation hashes over, so a dropped key moves the drv hash
//! silently.
//!
//! Measured before the fix, same expression, same binary flags:
//!
//! ```text
//! old: [SUI_PARITY_STRICT] no swallowed force-error drops (clean eval)
//! new: [SUI_PARITY_STRICT]   drv=p attr=bad site=structured-attrs x1
//!        force-err: json-encode-err type=list Throw("throw: boom")
//! ```
//!
//! The drvPath was byte-identical across both (`7a2adbjqnfl0xzs15r4hby6d3ma2y232`),
//! which is the point: this is reporting-only. The attr was always dropped —
//! it simply used to be dropped in silence.

use sui_eval::builtins::parity_strict;

/// The expression whose `bad` attr forces to a list and then fails to
/// JSON-encode, because encoding recurses into an element that throws.
const DROPS_AN_ATTR: &str = r#"
(derivation {
  name = "p";
  system = builtins.currentSystem;
  builder = "/bin/sh";
  __structuredAttrs = true;
  good = "g";
  bad = [ (throw "boom") ];
}).drvPath
"#;

/// A structured-attrs drop is recorded, not swallowed.
///
/// Each `#[test]` runs on its own thread and the ledger is `thread_local!`
/// (`sui-eval/src/builtins/derivation.rs:66`), so evaluating and draining in
/// the same test body is the correct — and only — way to read it.
#[test]
fn a_structured_attrs_json_encode_failure_is_recorded() {
    // The collector only records while STRICT is set.
    // SAFETY: `#[test]` bodies run on their own thread and this test drains
    // its own thread-local ledger; no other test reads this variable.
    unsafe { std::env::set_var("SUI_PARITY_STRICT", "1") };
    let _ = parity_strict::drain(); // start from a known-empty ledger

    let result = sui_eval::eval_with_file(DROPS_AN_ATTR, None);
    assert!(
        result.is_ok(),
        "the expression must still EVALUATE — the attr is dropped, not fatal"
    );

    let drops = parity_strict::drain();
    assert!(
        !drops.is_empty(),
        "the __structuredAttrs JSON-encode failure was not recorded. Before \
         this was closed, a strict run printed \"clean eval\" over exactly \
         this drop — which is the defect, not a cosmetic gap: __json is an \
         env var the drv hashes over."
    );

    let hit = drops
        .iter()
        .find(|d| d.attr == "bad")
        .expect("the dropped attr `bad` must be named in the ledger");
    assert_eq!(hit.site, parity_strict::DropSite::StructuredAttrs);
    assert!(
        hit.force_err.contains("json-encode-err"),
        "the record must say WHY it was dropped, so a reader can tell a \
         JSON-encode failure from a force error; got: {}",
        hit.force_err
    );

    // CALIBRATION: an attr that encodes fine must NOT be recorded. Without
    // this, a record arm that fired on every attr would satisfy the assertions
    // above while telling the operator nothing.
    assert!(
        drops.iter().all(|d| d.attr != "good"),
        "`good` encodes cleanly and must not appear as a drop — a ledger that \
         records everything is as useless as one that records nothing"
    );
}
