//! `SUI_VM_STRICT=1` turns a laundered answer into a refusal.
//!
//! # Why this matters more than it looks
//!
//! The CLI's VM arm falls back to the tree-walker on any error, so
//! `sui --vm eval <expr>` can exit 0 having printed an answer the VM did not
//! compute. Every VM-vs-walker comparison built on the CLI is therefore
//! answerable *by the walker on both sides* — which is why `tests/vm_cli.rs`
//! (36 cases) and `tests/vm_capabilities.rs` (23) were vacuous: a VM failing
//! 100% of those expressions passed 36/36.
//!
//! The probe below is not a synthetic failure. `builtins.getContext` on an
//! interpolated derivation fails on the VM *because* `VMValue::String` carries
//! no string context (`sui-bytecode/src/value.rs:43`) — the exact defect that
//! made the tree-walker the default engine. Measured:
//!
//! ```text
//! strict off: exit 0, prints the WALKER's answer, VM failure only on stderr
//! strict on:  exit 1, refuses, naming the boundary and the VM error
//! ```
//!
//! So this file pins the instrument on a real divergence rather than a
//! contrived one, and it will keep working as a latch test even after that
//! particular divergence is closed — at which point the probe stops falling
//! back and `strict_is_transparent_when_the_vm_succeeds` is the row that keeps
//! it honest.

use assert_cmd::Command;

/// An expression the VM cannot complete: `getContext` needs string context,
/// which the VM does not carry.
const VM_FALLS_BACK: &str = "let dep = derivation { name = \"d\"; \
    system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }; \
    in builtins.getContext \"${dep}\"";

/// Without the latch, the CLI answers with the tree-walker and exits 0.
///
/// This is the behaviour being guarded against, asserted so that a future
/// change which silently removes the fallback is also caught — the two rows
/// together pin BOTH directions, where either alone would pass on a broken
/// implementation.
#[test]
fn without_strict_the_walker_silently_answers() {
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["--vm", "eval", "--json", VM_FALLS_BACK])
        .assert()
        .success();
    let out = assert.get_output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains(".drv"),
        "expected the walker's answer on stdout, got: {stdout}"
    );
    assert!(
        stderr.contains("fallback"),
        "the VM failure should at least be visible on stderr; got: {stderr}"
    );
}

/// With the latch, the same command refuses.
#[test]
fn strict_refuses_to_launder_the_answer() {
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .env("SUI_VM_STRICT", "1")
        .args(["--vm", "eval", "--json", VM_FALLS_BACK])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("SUI_VM_STRICT"),
        "the refusal must name the latch so an operator knows which knob \
         produced it; got: {stderr}"
    );
    assert!(
        stderr.contains("boundary"),
        "the refusal must name WHICH fallback boundary was crossed — there \
         are three and they mean different things; got: {stderr}"
    );
}

/// ★ THE CALIBRATION. Strict must be transparent when the VM actually
/// succeeds.
///
/// Without this row, a latch that refused *everything* would satisfy the test
/// above perfectly. That is the same failure shape the latch exists to catch,
/// one level up — an instrument that always fires measures nothing.
#[test]
fn strict_is_transparent_when_the_vm_succeeds() {
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .env("SUI_VM_STRICT", "1")
        .args(["--vm", "eval", "--json", "1 + 1"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert_eq!(stdout.trim(), "2");
}

/// Bridging a builtin is architecture, not failure — strict must NOT refuse it.
///
/// `builtins.hashString` is served by the bridge today (measured: it is the one
/// of `match` / `fromTOML` / `hashString` that still reports `builtin=1`; the
/// other two became native and the comments listing them had aged). If strict
/// ever started refusing this layer, strict mode would be unable to evaluate
/// most real expressions and would simply be turned off.
#[test]
fn strict_does_not_refuse_a_bridged_builtin() {
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .env("SUI_VM_STRICT", "1")
        .args(["--vm", "eval", "--json", r#"builtins.hashString "sha256" "hi""#])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("8f434346"),
        "expected the sha256 of \"hi\"; got: {stdout}"
    );
}

/// The counters are readable, and they are not all-zero — an instrument that
/// reports zero for everything is indistinguishable from one that is not wired
/// up. `vm_fallback_count()` sat in the tree unread since it was written; this
/// row is what stops the new counters going the same way.
#[test]
fn the_fallback_report_counts_a_real_bridge() {
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .env("SUI_VM_FALLBACK_REPORT", "1")
        .args(["--vm", "eval", "--json", r#"builtins.hashString "sha256" "hi""#])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("builtin=1"),
        "expected the bridged-builtin counter to register; got: {stderr}"
    );
    // Every layer must appear, so adding one cannot drop it from the report.
    for layer in ["builtin", "imported-file", "whole-expression"] {
        assert!(
            stderr.contains(layer),
            "the report omits the `{layer}` layer; got: {stderr}"
        );
    }
}
