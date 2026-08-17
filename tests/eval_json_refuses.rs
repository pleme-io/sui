//! `sui eval --json` must REFUSE what nix refuses, not emit a placeholder.
//!
//! # The defect
//!
//! `Value::to_json` renders a lambda as `"<lambda>"`, a builtin as
//! `"<builtin name>"`, and — worst — a thunk whose force FAILED as
//! `"<thunk:error>"`. All three are valid JSON, so the CLI printed them and
//! exited 0. Measured against nix 2.31.5 before the fix:
//!
//! ```text
//! nix eval --json --expr '{ f = x: x; }'         exit 1
//! sui eval --json -E     '{ f = x: x; }'         exit 0   {"f":"<lambda>"}
//! nix eval --json --expr '{ x = throw "boom"; }' exit 1
//! sui eval --json -E     '{ x = throw "boom"; }' exit 0   {"x":"<thunk:error>"}
//! ```
//!
//! The `throw` row is the sharpest silent divergence the CLI had: a real
//! evaluation **error** becomes a **value**. A script parsing that JSON sees a
//! string where nix would have refused, and `set -e` never fires.
//!
//! # Why exit codes and not output
//!
//! The exit code is the contract a caller can act on. Asserting on the JSON
//! text would pass just as well against the broken behaviour (`{"f":"<lambda>"}`
//! *is* the output), which is exactly how this survived — the placeholder is
//! indistinguishable from a legitimate string result to anything that only
//! reads stdout.

use assert_cmd::Command;

fn sui() -> Command {
    Command::cargo_bin("sui").expect("cargo_bin sui")
}

/// A function cannot be serialised, so `--json` must fail.
#[test]
fn a_function_is_refused_not_placeheld() {
    for expr in ["{ f = x: x; }", "[ (x: x) ]", "x: x"] {
        let assert = sui().args(["eval", "--json", expr]).assert().failure();
        let out = String::from_utf8_lossy(&assert.get_output().stdout);
        assert!(
            !out.contains("<lambda>"),
            "{expr}: emitted a placeholder on stdout instead of refusing: {out}"
        );
    }
}

/// A builtin is a function too.
#[test]
fn a_builtin_is_refused() {
    sui()
        .args(["eval", "--json", "{ f = builtins.add; }"])
        .assert()
        .failure();
}

/// ★ The one that matters: an evaluation ERROR must stay an error.
#[test]
fn a_failed_force_is_an_error_not_a_value() {
    let assert = sui()
        .args(["eval", "--json", r#"{ x = throw "boom"; }"#])
        .assert()
        .failure();
    let out = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        !out.contains("<thunk:error>"),
        "an evaluation error was emitted as a JSON value: {out}"
    );
    // The operator must see the throw's OWN message, not a generic refusal —
    // otherwise the diagnostic is worse than nix's and the fix trades one bad
    // output for another.
    let err = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        err.contains("boom"),
        "the throw's message must survive to stderr; got: {err}"
    );
}

/// ★ CALIBRATION — ordinary values must still serialise and exit 0.
///
/// Without this, a change that made `--json` refuse EVERYTHING would satisfy
/// every assertion above perfectly. That is the same failure shape the fix
/// addresses, one level up: an instrument that always fires measures nothing.
#[test]
fn ordinary_values_still_serialise() {
    for (expr, want) in [
        (r#"{ a = 1; b = "two"; }"#, r#"{"a":1,"b":"two"}"#),
        (r#""plain""#, r#""plain""#),
        ("[ 1 2 3 ]", "[1,2,3]"),
        ("null", "null"),
        ("1.5", "1.5"),
    ] {
        let assert = sui().args(["eval", "--json", expr]).assert().success();
        let out = String::from_utf8_lossy(&assert.get_output().stdout);
        assert_eq!(out.trim(), want, "for {expr}");
    }
}

/// A derivation must still serialise through the `outPath` rule.
///
/// `to_json` carries CppNix's `tryAttrsToString` behaviour — an attrset with
/// `outPath` or `__toString` serialises to that string — and it is not
/// cosmetic: without it, serialisation recurses forever on the self-referential
/// derivation graph. The strict variant had to preserve it, and this row is
/// what proves it did rather than assuming.
#[test]
fn a_derivation_still_serialises_via_outpath() {
    let assert = sui()
        .args([
            "eval",
            "--json",
            r#"derivation { name = "d"; system = "aarch64-darwin"; builder = "/bin/sh"; }"#,
        ])
        .assert()
        .success();
    let out = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        out.contains("/nix/store/"),
        "a derivation should serialise to its outPath string; got: {out}"
    );
}
