//! Layer 9: CLI parity between the `sui` binary and real Nix.
//!
//! Drives the `sui` binary via `assert_cmd` for `sui eval --json
//! <expr>` and compares the parsed JSON stdout to real
//! `nix-instantiate --eval --json --strict -E <expr>`.
//!
//! Also exercises a handful of CLI error paths (parse error, unknown
//! variable, infinite recursion) and asserts sui exits non-zero.
//!
//! Online-only. Skipped silently when SUI_TEST_ONLINE is unset or
//! nix-instantiate is not on PATH.

use assert_cmd::Command;
use std::process::Command as StdCommand;

// ── Env/oracle helpers (duplicated from sui-eval/tests/common to
// avoid cross-crate `mod` sharing — this file lives at the repo root,
// not under sui-eval/tests/). Kept minimal. ─────────────────────────

fn online_mode() -> bool {
    std::env::var("SUI_TEST_ONLINE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn nix_available() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("nix-instantiate").is_file()))
        .unwrap_or(false)
}

fn skip_if_offline(test: &str) -> bool {
    if !online_mode() {
        eprintln!("skip {test}: SUI_TEST_ONLINE not set");
        return true;
    }
    if !nix_available() {
        eprintln!("skip {test}: nix-instantiate not on PATH");
        return true;
    }
    false
}

/// Run real `nix-instantiate --eval --json --strict -E <expr>` and
/// return the parsed JSON. Panics on oracle failure because the
/// caller is already guarding on `skip_if_offline` at test entry.
fn nix_eval_json(expr: &str) -> serde_json::Value {
    let expr_arg = if expr.trim_start().starts_with('-') {
        format!("({expr})")
    } else {
        expr.to_string()
    };
    let out = StdCommand::new("nix-instantiate")
        .args(["--eval", "--json", "--strict", "-E", &expr_arg])
        .output()
        .expect("spawn nix-instantiate");
    assert!(
        out.status.success(),
        "nix-instantiate failed for {expr:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "nix-instantiate stdout not JSON for {expr:?}: {e}\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// Run `sui eval --json <expr>` via `assert_cmd` and return the
/// parsed JSON from stdout. Panics if sui exited non-zero or if
/// stdout isn't JSON.
fn sui_eval_json(expr: &str) -> serde_json::Value {
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["eval", "--json", expr])
        .assert()
        .success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("sui stdout not JSON for {expr:?}: {e}\n{stdout}"))
}

fn assert_cli_parity(expr: &str) {
    let nix = nix_eval_json(expr);
    let sui = sui_eval_json(expr);
    assert_eq!(
        nix, sui,
        "CLI parity mismatch on {expr:?}\n  nix: {}\n  sui: {}",
        serde_json::to_string(&nix).unwrap_or_default(),
        serde_json::to_string(&sui).unwrap_or_default(),
    );
}

// ── Parity on a panel of canonical expressions ──────────────────────

#[test]
fn cli_parity_arithmetic() {
    if skip_if_offline("cli_parity_arithmetic") {
        return;
    }
    for expr in [
        "1 + 1",
        "2 * 3 + 4",
        "10 / 4",
        "1.5 + 2.5",
        "(-3) + 5",
    ] {
        assert_cli_parity(expr);
    }
}

#[test]
fn cli_parity_lists() {
    if skip_if_offline("cli_parity_lists") {
        return;
    }
    for expr in [
        "[1 2 3]",
        "[1 2] ++ [3 4]",
        "builtins.length [1 2 3]",
        "builtins.head [10 20]",
        "builtins.map (x: x * 2) [1 2 3]",
        "builtins.filter (x: x > 1) [0 1 2 3]",
    ] {
        assert_cli_parity(expr);
    }
}

#[test]
fn cli_parity_attrs() {
    if skip_if_offline("cli_parity_attrs") {
        return;
    }
    for expr in [
        "{}",
        "{ a = 1; b = 2; }",
        "{ a = 1; } // { b = 2; }",
        "builtins.attrNames { c = 1; a = 2; b = 3; }",
        r#"{ a.b = 1; }.a"#,
    ] {
        assert_cli_parity(expr);
    }
}

#[test]
fn cli_parity_strings() {
    if skip_if_offline("cli_parity_strings") {
        return;
    }
    for expr in [
        r#""hello" + " " + "world""#,
        r#"builtins.stringLength "abcdef""#,
        r#"builtins.substring 1 3 "abcdef""#,
        r#"builtins.concatStringsSep "," [ "a" "b" "c" ]"#,
    ] {
        assert_cli_parity(expr);
    }
}

#[test]
fn cli_parity_control_flow() {
    if skip_if_offline("cli_parity_control_flow") {
        return;
    }
    for expr in [
        "if 1 < 2 then 10 else 20",
        "let x = 1; y = x + 1; in y",
        "(x: y: x + y) 3 4",
        "({ a, b }: a * b) { a = 3; b = 4; }",
    ] {
        assert_cli_parity(expr);
    }
}

#[test]
fn cli_parity_types() {
    if skip_if_offline("cli_parity_types") {
        return;
    }
    for expr in [
        "builtins.typeOf 1",
        "builtins.typeOf 1.5",
        "builtins.typeOf true",
        r#"builtins.typeOf "x""#,
        "builtins.typeOf [1]",
        "builtins.typeOf { a = 1; }",
        "builtins.typeOf null",
    ] {
        assert_cli_parity(expr);
    }
}

// ── Error-path CLI behavior ─────────────────────────────────────────

#[test]
fn cli_parse_error_exits_nonzero() {
    if skip_if_offline("cli_parse_error_exits_nonzero") {
        return;
    }
    Command::cargo_bin("sui")
        .unwrap()
        .args(["eval", "let x = ; in x"])
        .assert()
        .failure();
}

#[test]
fn cli_unknown_variable_exits_nonzero() {
    if skip_if_offline("cli_unknown_variable_exits_nonzero") {
        return;
    }
    Command::cargo_bin("sui")
        .unwrap()
        .args(["eval", "does_not_exist"])
        .assert()
        .failure();
}

#[test]
fn cli_infinite_recursion_exits_nonzero() {
    if skip_if_offline("cli_infinite_recursion_exits_nonzero") {
        return;
    }
    Command::cargo_bin("sui")
        .unwrap()
        .args(["eval", "let x = x; in x"])
        .assert()
        .failure();
}

#[test]
fn cli_eval_without_json_flag_still_succeeds() {
    // `sui eval "1 + 1"` (no --json) should still produce a
    // successful exit and non-empty stdout.
    if skip_if_offline("cli_eval_without_json_flag_still_succeeds") {
        return;
    }
    Command::cargo_bin("sui")
        .unwrap()
        .args(["eval", "1 + 1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("2"));
}
