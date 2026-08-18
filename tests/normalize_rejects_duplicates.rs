//! The rejection tier: sui must REFUSE what nix refuses.
//!
//! # The defect
//!
//! nix decides duplicate-attribute legality at PARSE time. Two declarations of
//! one static name merge iff BOTH sides are syntactic attrset literals;
//! anything else is `attribute 'a' already defined` and the file never
//! evaluates. sui accepted all of it at exit 0.
//!
//! ★ And the two engines accepted it DIFFERENTLY, which is the argument for
//! refusing rather than for picking a winner. Measured 2026-08-18, before this
//! landed:
//!
//! ```text
//! { a = 1; a = 2; }                     nix exit 1   walker { a = 2; }   vm { a = 1; }
//! { a = if … then 1 else 2; a = {c=2;}; } nix exit 1   walker { a = {c=2;}; } vm { a = 1; }
//! { a = "s"; a.b = 1; }                 nix exit 1   walker { a = {b=1;}; } vm { a = "s"; }
//! ```
//!
//! Last-wins on one engine, first-wins on the other, and nix's rule is
//! neither. No choice of winner reconciles them.
//!
//! # What this asserts, and what it deliberately does NOT
//!
//! The contract is the **exit code** and the **attribute path**. nix's prose —
//! `error: attribute 'a' already defined at «string»:1:3` plus a caret block
//! pointing at the second definition — needs a source-span formatter sui does
//! not have, and coupling that to this tier would turn a small change into an
//! error-rendering project.
//!
//! # ★ The dangerous direction is the FALSE REJECT
//!
//! Refusing a program nix accepts breaks working code, and this class has
//! already produced one: `fold_attr` returning `None` for an interpolated key
//! dropped the component, collapsing `a."${"b"}"` and `a."${"c"}"` into two
//! bindings of plain `a`. So the accept-set below is not decoration — it is
//! the half of the test that protects the fleet, and it includes the shapes
//! that look like duplicates and are not.

use assert_cmd::Command;

/// nix's verdict: `Some(true)` accepted, `Some(false)` refused, `None` if nix
/// is unavailable.
fn nix_accepts(expr: &str) -> Option<bool> {
    let out = std::process::Command::new("nix")
        .args(["eval", "--impure", "--expr", expr])
        .output()
        .ok()?;
    Some(out.status.success())
}

fn sui(expr: &str) -> (bool, String) {
    let out = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["eval", "-E", expr])
        .output()
        .expect("run sui");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
}

fn sui_vm(expr: &str) -> bool {
    Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .env("SUI_VM_STRICT", "1")
        .args(["--vm", "eval", "-E", expr])
        .output()
        .expect("run sui")
        .status
        .success()
}

/// Every one is `attribute '<path>' already defined` in nix, exit 1. The
/// second element is the path the message must name.
const REJECTED: &[(&str, &str)] = &[
    ("{ a = 1; a = 2; }", "a"),
    ("{ a = if true then 1 else 2; a = {c=2;}; }", "a"),
    // A merge needs two SYNTACTIC attrset literals. An identifier that happens
    // to evaluate to an attrset is not one — the rule is syntax, not value,
    // which is the whole reason this is decided at parse time.
    ("let x = {b=1;}; in { s = x; s = {c=2;}; }", "s"),
    ("{ a = \"s\"; a.b = 1; }", "a"),
    ("{ a = [1]; a = {c=2;}; }", "a"),
    ("let a = 1; a = 2; in a", "a"),
    // A dotted path names the FULL path in the message, not just the head.
    ("{ a.b = 1; a.b = 2; }", "a.b"),
    // inherit binds a name outright, so it can never merge with anything.
    ("let src = { t = 1; }; inherit (src) t; t.x = 2; in t", "t"),
];

/// Legal nix that LOOKS like the above. A false reject here breaks working
/// code, which is the failure direction that actually costs something.
const ACCEPTED: &[&str] = &[
    "{ a = {b=1;}; a = {c=2;}; }",
    "{ a.b = 1; a.c = 2; }",
    "rec { a = {b=1;}; a.c = 2; }",
    // ★ The later `rec` is DISCARDED, not rejected — measured, and a comment
    // in `sui-normalize` claimed the opposite until 2026-08-18. Legal, exit 0.
    "{ a = {b=1;}; a = rec {c=2;}; }",
    "{ a = rec {c=2; d=c;}; a = {b=1;}; }",
    // Two dynamic keys that happen to fold to the same name are an EVAL-time
    // collision, not a parse-time one — and these two do not even collide.
    "{ a.\"${\"b\"}\" = 1; a.\"${\"c\"}\" = 2; }",
    "let a = {b=1;}; a.c = 2; in a",
];

#[test]
fn sui_refuses_what_nix_refuses() {
    let Some(_) = nix_accepts("1") else {
        eprintln!("sui_refuses_what_nix_refuses: skipped (no usable nix)");
        return;
    };
    for (expr, path) in REJECTED {
        assert_eq!(
            nix_accepts(expr),
            Some(false),
            "calibration: nix must REJECT `{expr}`, or this row proves nothing"
        );
        let (ok, stderr) = sui(expr);
        assert!(!ok, "sui accepted `{expr}` at exit 0; nix exits 1");
        assert!(
            stderr.contains(&format!("'{path}'")),
            "the error must name the attribute path '{path}':\n  {expr}\n  {stderr}"
        );
        assert!(
            !sui_vm(expr),
            "the bytecode VM accepted `{expr}` where the walker refused it — \
             the engines must agree on the rejection, not only on the answer"
        );
    }
}

#[test]
fn sui_still_accepts_what_nix_accepts() {
    let Some(_) = nix_accepts("1") else {
        eprintln!("sui_still_accepts_what_nix_accepts: skipped (no usable nix)");
        return;
    };
    for expr in ACCEPTED {
        assert_eq!(
            nix_accepts(expr),
            Some(true),
            "calibration: nix must ACCEPT `{expr}`, or this row proves nothing"
        );
        let (ok, stderr) = sui(expr);
        assert!(ok, "sui REFUSED legal nix — a false reject:\n  {expr}\n  {stderr}");
        assert!(sui_vm(expr), "the bytecode VM refused legal nix: {expr}");
    }
}
