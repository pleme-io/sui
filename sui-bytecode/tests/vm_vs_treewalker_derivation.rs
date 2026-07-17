//! Two-engine differential: the bytecode VM's derivation construction MUST
//! produce byte-identical `.drvPath` values to the tree-walker's for every
//! supported derivation shape.
//!
//! WHY THIS TEST IS THE FORCING FUNCTION.
//! -------------------------------------
//! The VM's `vm_build_derivation` is a hand-rolled copy of the tree-walker's
//! `construct_derivation`. When the two drift, the VM emits a *wrong-but-non-
//! erroring* `.drv` — a multi-output package silently collapses to `out`-only,
//! an `outputs` env var goes missing, a float env attr formats differently.
//! The "VM errors → fall back to the tree-walker" safety net does NOT catch
//! this, because the VM does not error; it produces a plausible-looking but
//! divergent drvPath. Only a BYTE-EQUALITY gate against the tree-walker catches
//! the drift. That is this test.
//!
//! The VM is sui's CLI-DEFAULT engine, so any drift here bites real users.
//!
//! TIER-HONEST SCOPE (the two-engine string-context reality).
//! ----------------------------------------------------------
//! The VM does NOT track string context (`VMValue::String` carries no context —
//! deferred to the VM's "Phase 2"; see `sui-bytecode/src/value.rs`). Two
//! derivation-construction concerns depend on that context and therefore CANNOT
//! reach byte-parity in the VM until Phase-2 context lands:
//!
//!   * context-bearing env vars — `input = "${dep}"` must record `dep.drv` as an
//!     inputDrv edge; the VM has no context, so the edge is dropped → the ATerm
//!     (and thus drvPath) diverges.
//!   * `__structuredAttrs = true` — the tree-walker collapses every attr into a
//!     single `__json` env var; the VM has no such collapse.
//!
//! These are enumerated in [`KNOWN_VM_BLOCKED`] with an EXPLICIT documented
//! divergence, so the test file *names* exactly which shapes remain and why —
//! rather than silently skipping them. The [`MUST_MATCH`] table is the real
//! gate: every context-free shape the VM CAN build correctly, asserted
//! byte-equal, failing the build on any diff.

use sui_bytecode::StringKeyedValue;

/// Evaluate `expr` through the tree-walker and return its string result.
///
/// `expr` must evaluate to a string (we always ask for `.drvPath`).
fn tree_walker_str(expr: &str) -> String {
    let v = sui_eval::eval(expr)
        .unwrap_or_else(|e| panic!("tree-walker failed for `{expr}`: {e}"));
    // `.drvPath` is a lazy thunk; force + coerce to the string.
    let forced = sui_eval::eval::force_value(&v)
        .unwrap_or_else(|e| panic!("tree-walker force failed for `{expr}`: {e}"));
    forced
        .to_str()
        .unwrap_or_else(|e| panic!("tree-walker result for `{expr}` is not a string: {e}"))
}

/// Evaluate `expr` through the bytecode VM and return its string result.
fn vm_str(expr: &str) -> String {
    let r = sui_bytecode::eval_full(expr)
        .unwrap_or_else(|e| panic!("bytecode VM failed for `{expr}`: {e}"));
    match r.to_string_keyed() {
        StringKeyedValue::String(s) => s,
        other => panic!("bytecode VM result for `{expr}` is not a string: {other:?}"),
    }
}

/// One derivation shape: a human label and the `.drvPath` expression to compare
/// across both engines.
struct Shape {
    label: &'static str,
    expr: &'static str,
}

/// Shapes the VM CAN build byte-identically to the tree-walker today. Every row
/// here is a hard gate: a byte diff fails the build.
///
/// These exercise the confirmed VM derivation fixes:
///   * single + multi-output (the `outputs`-list-item force + `outputs` env var).
///   * thunk-valued env vars (the force-then-coerce env loop).
///   * float env attr (%f / `{f:.6}`, not Rust shortest).
///   * `__ignoreNulls` (null attrs dropped, `__ignoreNulls` consumed).
///   * list-valued env var (space-joined coercion).
///   * flat FOD (env["out"] set + refs-folded drvPath + modulo remembered).
const MUST_MATCH: &[Shape] = &[
    Shape {
        label: "single-output",
        expr: r#"(derivation { name="s"; system="x86_64-linux"; builder="/b"; }).drvPath"#,
    },
    Shape {
        label: "multi-output out/dev/lib",
        expr: r#"(derivation { name="m"; system="x86_64-linux"; builder="/b"; outputs=["out" "dev" "lib"]; }).drvPath"#,
    },
    Shape {
        label: "thunk-valued env vars",
        expr: r#"(derivation { name="t"; system="x86_64-linux"; builder="/b"; foo=(1+2); bar=(if true then "y" else "n"); }).drvPath"#,
    },
    Shape {
        label: "float env attr (%f)",
        expr: r#"(derivation { name="f"; system="x86_64-linux"; builder="/b"; ratio=1.5; pi=3.14159; }).drvPath"#,
    },
    Shape {
        label: "__ignoreNulls drops nulls",
        expr: r#"(derivation { name="n"; system="x86_64-linux"; builder="/b"; __ignoreNulls=true; userHook=null; keep="k"; }).drvPath"#,
    },
    Shape {
        label: "list-valued env var",
        expr: r#"(derivation { name="l"; system="x86_64-linux"; builder="/b"; tags=["a" "b" "c"]; }).drvPath"#,
    },
    Shape {
        label: "FOD flat",
        expr: r#"(derivation { name="fod"; system="x86_64-linux"; builder="/b"; outputHash="0000000000000000000000000000000000000000000000000000"; outputHashAlgo="sha256"; outputHashMode="flat"; }).drvPath"#,
    },
];

/// Shapes the VM CANNOT yet build byte-identically — blocked on the VM's
/// Phase-2 string-context work (`VMValue::String` carries no context) or the
/// `__structuredAttrs` `__json` collapse. Documented here so the divergence is
/// NAMED, not hidden. When VM string-context lands, promote these to
/// `MUST_MATCH` and the test tightens automatically.
const KNOWN_VM_BLOCKED: &[Shape] = &[
    Shape {
        // Blocked: `${dep}` interpolation must record `dep.drv` as an inputDrv
        // edge; the VM has no string context, so the edge is dropped.
        label: "FOD/env referencing another drv (inputDrv edge)",
        expr: r#"(let dep = derivation { name="dep"; system="x86_64-linux"; builder="/b"; }; in (derivation { name="cons"; system="x86_64-linux"; builder="/b"; input="${dep}"; }).drvPath)"#,
    },
    Shape {
        // Blocked: the VM does not collapse attrs into `__json`.
        label: "__structuredAttrs = true",
        expr: r#"(derivation { name="sa"; system="x86_64-linux"; builder="/b"; __structuredAttrs=true; buildInputs=["x"]; }).drvPath"#,
    },
];

/// THE GATE. For every context-free derivation shape, the VM's `.drvPath` must
/// equal the tree-walker's byte-for-byte. Failures aggregate so one run reports
/// every broken shape, not just the first.
#[test]
fn vm_matches_treewalker_byte_for_byte() {
    let mut failures: Vec<String> = Vec::new();
    for shape in MUST_MATCH {
        let tw = tree_walker_str(shape.expr);
        let vm = vm_str(shape.expr);
        if tw != vm {
            failures.push(format!(
                "{}:\n    tree-walker => {tw}\n    bytecode VM => {vm}",
                shape.label
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} derivation shape(s) diverge between the VM and the tree-walker:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
}

/// The multi-output marquee: on the DEFAULT (VM) engine, a three-output
/// derivation must expose exactly three outputs in `.all` (was 1 before the
/// outputs-list-item force fix), AND its drvPath must equal the tree-walker's.
#[test]
fn vm_multi_output_has_three_outputs_and_matches() {
    let all_len_expr = r#"builtins.length (derivation { name="m"; system="x86_64-linux"; builder="/b"; outputs=["out" "dev" "lib"]; }).all"#;
    let vm_len = sui_bytecode::eval_full(all_len_expr).expect("VM eval");
    match vm_len.to_string_keyed() {
        StringKeyedValue::Int(n) => assert_eq!(
            n, 3,
            "VM multi-output `.all` length must be 3 (the outputs-list-item force fix); got {n}"
        ),
        other => panic!("expected Int, got {other:?}"),
    }

    let drv_expr = r#"(derivation { name="m"; system="x86_64-linux"; builder="/b"; outputs=["out" "dev" "lib"]; }).drvPath"#;
    assert_eq!(
        vm_str(drv_expr),
        tree_walker_str(drv_expr),
        "VM multi-output drvPath must equal the tree-walker's"
    );
}

/// Documents the CURRENT divergence of the Phase-2-blocked shapes. This test
/// PASSES today (the shapes DO diverge, as expected) — it exists so that when
/// VM string-context lands and a shape starts matching, THIS test fails and
/// forces the row to be promoted into `MUST_MATCH`. It is a tripwire against a
/// silent, undocumented parity gain going untracked.
#[test]
fn known_vm_blocked_shapes_still_diverge() {
    for shape in KNOWN_VM_BLOCKED {
        let tw = tree_walker_str(shape.expr);
        let vm = vm_str(shape.expr);
        assert_ne!(
            tw, vm,
            "`{}` now MATCHES between engines — VM string-context has advanced. \
             Promote this row from KNOWN_VM_BLOCKED to MUST_MATCH.",
            shape.label
        );
    }
}
