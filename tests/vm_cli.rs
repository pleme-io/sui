//! CLI tests for the `--vm` flag.
//!
//! Verifies that `sui --vm eval` produces the same results as `sui eval`
//! for expressions the bytecode VM supports.

use assert_cmd::Command;

/// Run `sui --vm eval --json <expr>` and return parsed JSON.
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

/// Run `sui eval --json <expr>` (tree-walker) and return parsed JSON.
fn tw_eval_json(expr: &str) -> serde_json::Value {
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["eval", "--json", expr])
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

// ── Error paths ───────────────────────────────────────────────

#[test]
fn vm_eval_error_exits_nonzero() {
    Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["--vm", "eval", "let in"])
        .assert()
        .failure();
}
