//! CLI tests for the VM eval backend.
//!
//! ★ THE VM SIDE MUST PASS `--vm` EXPLICITLY. The tree-walker is the default
//! for `sui eval` (flipped 2026-08-17); relying on the default here would make
//! every row below compare the walker to ITSELF and pass unconditionally.
//!
//! These tests were already weaker than they looked, and it is worth stating
//! why so the shape is recognisable: the CLI's VM arm falls back to the
//! tree-walker on ANY error, so a VM that failed outright still produced the
//! walker's answer and the assertion `vm == tw` held anyway. **A VM failing
//! 100% of these expressions passed 36/36.** Passing `--vm` fixes the engine
//! SELECTION; it does not fix the fallback — that needs the strict latch, so
//! until then read a green run here as "the VM did not produce a *different*
//! answer", never as "the VM computed this".

use assert_cmd::Command;

/// Run `sui --vm eval --json <expr>` and return parsed JSON.
///
/// `--vm` is explicit and load-bearing: the default engine is the tree-walker.
fn vm_eval_json(expr: &str) -> serde_json::Value {
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["--vm", "eval", "--json", expr])
        .assert()
        .success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("vm eval JSON parse failed for {expr:?}: {e}\n{trimmed}"))
}

/// Run `sui --no-vm eval --json <expr>` (tree-walker) and return parsed JSON.
fn tw_eval_json(expr: &str) -> serde_json::Value {
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["--no-vm", "eval", "--json", expr])
        .assert()
        .success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("tw eval JSON parse failed for {expr:?}: {e}\n{trimmed}"))
}

/// Assert VM and tree-walker produce identical JSON.
fn assert_vm_tw_parity(expr: &str) {
    let vm = vm_eval_json(expr);
    let tw = tw_eval_json(expr);
    assert_eq!(
        vm, tw,
        "VM vs tree-walker mismatch for {expr:?}\nVM:  {vm}\nTW:  {tw}"
    );
}

// ── Scalars ───────────────────────────────────────────────────

#[test]
fn vm_parity_int() {
    assert_vm_tw_parity("42");
}

#[test]
fn vm_parity_negative_int() {
    assert_vm_tw_parity("(-7)");
}

#[test]
fn vm_parity_float() {
    assert_vm_tw_parity("3.14");
}

#[test]
fn vm_parity_bool() {
    assert_vm_tw_parity("true");
    assert_vm_tw_parity("false");
}

#[test]
fn vm_parity_null() {
    assert_vm_tw_parity("null");
}

#[test]
fn vm_parity_string() {
    assert_vm_tw_parity(r#""hello world""#);
}

// ── Arithmetic ────────────────────────────────────────────────

#[test]
fn vm_parity_addition() {
    assert_vm_tw_parity("1 + 2");
}

#[test]
fn vm_parity_subtraction() {
    assert_vm_tw_parity("10 - 3");
}

#[test]
fn vm_parity_multiplication() {
    assert_vm_tw_parity("6 * 7");
}

#[test]
fn vm_parity_division() {
    assert_vm_tw_parity("10 / 3");
}

// ── Logic ─────────────────────────────────────────────────────

#[test]
fn vm_parity_and() {
    assert_vm_tw_parity("true && false");
}

#[test]
fn vm_parity_or() {
    assert_vm_tw_parity("false || true");
}

#[test]
fn vm_parity_not() {
    assert_vm_tw_parity("!true");
}

#[test]
fn vm_parity_implication() {
    assert_vm_tw_parity("false -> true");
}

// ── Comparison ────────────────────────────────────────────────

#[test]
fn vm_parity_equal() {
    assert_vm_tw_parity("1 == 1");
    assert_vm_tw_parity("1 == 2");
}

#[test]
fn vm_parity_less() {
    assert_vm_tw_parity("1 < 2");
}

#[test]
fn vm_parity_greater() {
    assert_vm_tw_parity("2 > 1");
}

// ── Attrsets ──────────────────────────────────────────────────

#[test]
fn vm_parity_attrset() {
    assert_vm_tw_parity("{ a = 1; b = 2; }");
}

#[test]
fn vm_parity_attrset_select() {
    assert_vm_tw_parity("{ a = 1; b = 2; }.a");
}

#[test]
fn vm_parity_attrset_update() {
    assert_vm_tw_parity("{ a = 1; } // { b = 2; }");
}

// ── Lists ─────────────────────────────────────────────────────

#[test]
fn vm_parity_list() {
    assert_vm_tw_parity("[1 2 3]");
}

#[test]
fn vm_parity_list_concat() {
    assert_vm_tw_parity("[1 2] ++ [3 4]");
}

// ── Let/in ────────────────────────────────────────────────────

#[test]
fn vm_parity_let() {
    assert_vm_tw_parity("let x = 10; y = 20; in x + y");
}

// ── Lambdas ───────────────────────────────────────────────────

#[test]
fn vm_parity_lambda() {
    assert_vm_tw_parity("(x: x + 1) 5");
}

#[test]
fn vm_parity_pattern_lambda() {
    assert_vm_tw_parity("({ a, b }: a + b) { a = 3; b = 4; }");
}

// ── If/else ───────────────────────────────────────────────────

#[test]
fn vm_parity_if_true() {
    assert_vm_tw_parity("if true then 1 else 2");
}

#[test]
fn vm_parity_if_false() {
    assert_vm_tw_parity("if false then 1 else 2");
}

// ── Builtins ──────────────────────────────────────────────────

#[test]
fn vm_parity_builtins_length() {
    assert_vm_tw_parity("builtins.length [1 2 3]");
}

#[test]
fn vm_parity_builtins_type_of() {
    assert_vm_tw_parity("builtins.typeOf 42");
}

// ── Flag variants ─────────────────────────────────────────────

#[test]
fn explicit_vm_flag_is_noop() {
    // --vm is redundant (VM is default) but should still work.
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["--vm", "eval", "--json", "1 + 2"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let val: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(val, serde_json::json!(3));
}

#[test]
fn no_vm_flag_uses_tree_walker() {
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["--no-vm", "eval", "--json", "1 + 2"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let val: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(val, serde_json::json!(3));
}

// ── String-key attrsets (laziness) ────────────────────────────

#[test]
fn vm_parity_string_key_attrset() {
    // String keys like "1" must be treated as static keys
    // with lazy value evaluation (thunk-wrapped).
    assert_vm_tw_parity(r#"{ "a" = 1; "b" = 2; }."b""#);
}

#[test]
fn vm_string_key_attrset_lazy() {
    // Accessing one key must not evaluate other keys' throw expressions.
    // This is the pattern nixpkgs uses in lib.systems.parse.mkSkeletonFromList.
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args([
            "eval",
            "--json",
            r#"{ "1" = throw "boom"; "2" = "ok"; }."2""#,
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let val: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(val, serde_json::json!("ok"));
}

#[test]
fn vm_string_key_dynamic_select_lazy() {
    // Dynamic select with `or` fallback must not evaluate unaccessed keys.
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args([
            "eval",
            "--json",
            r#"{ "1" = throw "boom"; "2" = "ok"; }.${toString 2} or "fallback""#,
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let val: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(val, serde_json::json!("ok"));
}

// ── Error paths ───────────────────────────────────────────────

#[test]
fn vm_eval_error_exits_nonzero() {
    Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["eval", "let in"])
        .assert()
        .failure();
}

#[test]
fn no_vm_eval_error_exits_nonzero() {
    Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["--no-vm", "eval", "let in"])
        .assert()
        .failure();
}

// ── The calibration ───────────────────────────────────────────

/// The two helpers above must reach **different engines**.
///
/// Every `assert_vm_tw_parity` row is worthless if they do not, and the failure
/// is invisible: two identical invocations agree perfectly and 36 tests pass
/// while proving nothing. That is not hypothetical — the tree-walker became the
/// default on 2026-08-17, and had `vm_eval_json` not been given an explicit
/// `--vm` in the same commit, every row here would have silently become
/// walker-vs-walker.
///
/// The probe is a derivation that interpolates another derivation. The VM's
/// `VMValue::String` carries no string context (`sui-bytecode/src/value.rs:43`),
/// so its `inputDrvs` comes out empty and the drvPath hash differs from the
/// walker's — which matches nix. Measured at the time of writing:
///
/// ```text
/// nix + walker: /nix/store/fhfg067pxrm022w3hv7zsav1q9sxb30i-top.drv
/// VM:           /nix/store/kicfsn1hmp1qr6mb3li07i5pcrh4x6x1-top.drv
/// ```
///
/// This mirrors `sui-bytecode/tests/vm_vs_treewalker_derivation.rs`'s
/// `known_vm_blocked_shapes_still_diverge`, at the CLI level.
///
/// **When the VM learns string context this test SHOULD fail**, and the fix is
/// to delete it and pick a new calibration — not to relax it. A test asserting
/// a divergence is a liability the moment the divergence is closed, so it says
/// so out loud rather than being quietly weakened later.
#[test]
fn the_two_helpers_reach_different_engines() {
    const CTX_BEARING: &str = "let dep = derivation { name = \"dep\"; \
        system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }; \
        in (derivation { name = \"top\"; system = \"aarch64-darwin\"; \
        builder = \"/bin/sh\"; ref = \"${dep}\"; }).drvPath";

    let vm = vm_eval_json(CTX_BEARING);
    let tw = tw_eval_json(CTX_BEARING);
    assert_ne!(
        vm, tw,
        "the VM and tree-walker returned the SAME drvPath for a \
         context-bearing derivation. Either both helpers are now invoking the \
         same engine — in which case every parity row in this file is vacuous \
         and `vm_eval_json` has lost its `--vm` — or the VM has gained string \
         context, in which case delete this test and choose a new calibration."
    );
}
