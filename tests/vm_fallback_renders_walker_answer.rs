//! When the VM refuses an expression, the CLI must return the TREE-WALKER's
//! ANSWER — not merely route to its evaluation.
//!
//! # The defect
//!
//! `src/main.rs`'s whole-expression fallback re-ran the walker correctly and
//! then converted its `Value` back through `sui_eval::eval_to_string_keyed`,
//! which is **deliberately lazy** (forcing a flake's inputs would trigger git
//! clones). Nothing downstream forced it, so the answer was laundered:
//!
//! ```text
//! sui --vm eval -E '{ a = rec { b = c+1; d = 2; }; a.c = d+3; }'
//!   { a = <<thunk>>; }          walker: { a = { b = 6; c = 5; d = 2; }; }
//! ```
//!
//! Two properties of the bug made it hard to see and important to pin:
//!
//! * **The rule was DEPTH, not shape.** A scalar at depth 1 laundered too
//!   (`e = 1 + 1` → `<<thunk>>`), so it was not "nested attrsets only".
//! * **It turned ERRORS into VALUES.** `throw` at depth produced
//!   `{"a":"<thunk>"}` at exit 0 where nix and the walker exit 1 — the exact
//!   divergence class `try_to_json` was introduced to kill, reintroduced on the
//!   VM arm.
//!
//! It also hid behind an unrepresentative probe: the acceptance case with a
//! `.a.b` selector returns a *scalar*, so it rendered correctly and looked like
//! clean routing. Every case here therefore keeps a value at depth.
//!
//! The fix is NOT to make `eval_to_string_keyed` eager — its laziness is
//! load-bearing. It is `render_walker_value`, so the arm never converts.

use assert_cmd::Command;

/// An expression the VM REFUSES (`unresolved variable: c` — `c` exists only
/// after `a.c` is spliced into the `rec` literal, and the VM does not splice),
/// whose value has attributes at depth.
const REFUSED_WITH_DEPTH: &str = "{ a = rec { b = c + 1; d = 2; }; a.c = d + 3; }";

fn nix(args: &[&str]) -> Option<(String, bool)> {
    let out = std::process::Command::new("nix").args(args).output().ok()?;
    Some((
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        out.status.success(),
    ))
}

fn sui_vm(args: &[&str]) -> (String, bool) {
    let out = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["--vm"])
        .args(args)
        .output()
        .expect("run sui");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        out.status.success(),
    )
}

/// The refusal must yield nix's value, in both render modes.
#[test]
fn a_refused_expression_renders_the_walkers_answer() {
    for (nix_args, sui_args) in [
        (
            vec!["eval", "--impure", "--expr", REFUSED_WITH_DEPTH],
            vec!["eval", "-E", REFUSED_WITH_DEPTH],
        ),
        (
            vec!["eval", "--impure", "--json", "--expr", REFUSED_WITH_DEPTH],
            vec!["eval", "--json", "-E", REFUSED_WITH_DEPTH],
        ),
    ] {
        let Some((want, ok)) = nix(&nix_args) else {
            eprintln!("a_refused_expression_renders_the_walkers_answer: skipped (no usable nix)");
            return;
        };
        assert!(ok, "the oracle refused a legal expression: {nix_args:?}");
        let (got, _) = sui_vm(&sui_args);
        assert_eq!(
            got, want,
            "\n{sui_args:?}\n  nix: {want}\n  sui: {got}\n\
             A VM refusal must route to the walker's ANSWER, not just its \
             evaluation. A `<<thunk>>` or `\"<thunk>\"` here means the arm is \
             converting through the lazy path again."
        );
    }
}

/// ★ The bug was DEPTH, not shape — this is the row that says so. A plain
/// scalar one level down laundered just as an attrset did, so a test using only
/// nested attrsets would under-describe the defect.
#[test]
fn a_scalar_at_depth_one_is_not_laundered() {
    let expr = "{ a.b = 1; a.b = 2; c = { d = 1; }; e = 1 + 1; }";
    let (got, _) = sui_vm(&["eval", "-E", expr]);
    assert!(
        !got.contains("thunk"),
        "depth-1 values came back as placeholders: {got}"
    );
    // And it must equal what the walker alone says.
    let tw = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["eval", "-E", expr])
        .output()
        .expect("run sui");
    assert_eq!(
        got,
        String::from_utf8_lossy(&tw.stdout).trim(),
        "the fallback must agree with the walker it fell back to"
    );
}

/// ★ ANTI-VACUITY + the sharpest half of the bug: an evaluation ERROR must stay
/// an error. Without this row the tests above would pass while `throw` at depth
/// still produced valid JSON at exit 0 — a consumer parsing that sees a string
/// where nix refused outright.
#[test]
fn a_throw_at_depth_stays_an_error() {
    let expr = "{ a = rec { b = throw \"boom\"; d = 2; }; a.c = d + 3; }";
    let Some((_, nix_ok)) = nix(&["eval", "--impure", "--json", "--expr", expr]) else {
        eprintln!("a_throw_at_depth_stays_an_error: skipped (no usable nix)");
        return;
    };
    assert!(!nix_ok, "calibration: nix must REJECT this, or the row proves nothing");
    let (out, ok) = sui_vm(&["eval", "--json", "-E", expr]);
    assert!(!ok, "sui exited 0 where nix exited non-zero; stdout: {out}");
    assert!(
        out.is_empty(),
        "an error must not also emit a value on stdout: {out}"
    );
}
