//! `install_vm_bridges()` is the ONE place the VM's tree-walker bridges may be
//! installed — enforced, not asserted in prose.
//!
//! WHY THIS EXISTS. `sui-eval/src/lib.rs` has carried this comment since the
//! function was introduced:
//!
//! > ★ THIS IS THE ONE INSTALL SITE. `BytecodeEvaluator` calls it, and so does
//! > `sui-bytecode`'s bridged-parity test suite — so a test can never drift
//! > from what production wires up
//!
//! It was false. `src/main.rs` hand-rolled its own copy that installed **two**
//! of the three bridges, silently omitting the **path materializer**. The
//! consequence was exact and is the reason a comment is not good enough here:
//! the VM's filesystem-redirect fix was green in
//! `sui-bytecode/tests/vm_bridge_parity.rs` — which *does* call
//! `install_vm_bridges()` — and **absent from the shipped binary**. `sui eval`
//! answered `pathExists = false` for every file in a fetched flake input,
//! silently, because `false` is a legal answer and the VM's per-file fallback
//! only fires on an ERROR.
//!
//! A green test beside a broken binary is the worst outcome available, and the
//! only structural defence is to make the duplicate impossible to reintroduce
//! without turning something red.
//!
//! TIER: CI-caught (a red `cargo test`), not unrepresentable. The
//! truly-unrepresentable version makes the setters `pub(crate)` in
//! `sui-bytecode` and exposes only a bundle constructor — but they are
//! deliberately public so the bridged-parity suite can install stubs for its
//! red-runs. Named, not scheduled.

use std::path::{Path, PathBuf};

/// The three thread-local bridge setters the VM depends on.
const SETTERS: &[&str] = &[
    "set_flake_resolver",
    "set_builtin_bridge",
    "set_path_materializer",
];

/// Every `*.rs` in the workspace.
fn workspace_sources() -> Vec<PathBuf> {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // sui-eval/ -> workspace root

    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if p.is_dir() {
                if matches!(name, "target" | ".git" | "vendor" | "node_modules") {
                    continue;
                }
                stack.push(p);
            } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Files permitted to name a setter.
fn is_permitted(p: &Path) -> bool {
    // `sui-bytecode` DEFINES and re-exports them, and its own suite installs
    // stubs to red-run the bridges. That crate owns the mechanism.
    let in_bytecode_src = p
        .components()
        .any(|c| c.as_os_str() == "sui-bytecode")
        && p.components().any(|c| c.as_os_str() == "src");
    // The one install site.
    let is_install_site = p.ends_with("sui-eval/src/lib.rs");
    // This file names the setters in `SETTERS` and would match itself —
    // a guard that satisfies its own denominator is measuring itself.
    let is_this_guard = p.ends_with("sui-eval/tests/vm_bridge_install_funnel.rs");
    in_bytecode_src || is_install_site || is_this_guard
}

#[test]
fn only_install_vm_bridges_installs_the_bridges() {
    let mut offenders: Vec<(PathBuf, &str)> = Vec::new();
    let mut install_site_seen = false;

    for path in workspace_sources() {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let names: Vec<&str> = SETTERS
            .iter()
            .copied()
            .filter(|s| src.contains(*s))
            .collect();
        if names.is_empty() {
            continue;
        }
        if path.ends_with("sui-eval/src/lib.rs") {
            // Anti-vacuity: the install site must still install ALL THREE.
            // Losing one here is precisely the bug this guard exists for, and
            // it would otherwise leave the offender list empty and green.
            for s in SETTERS {
                assert!(
                    src.contains(s),
                    "install_vm_bridges() no longer installs `{s}` — that is \
                     the defect this guard exists to catch, seen from the \
                     inside. All three bridges must be installed together or \
                     the VM silently diverges from the tree-walker."
                );
            }
            install_site_seen = true;
            continue;
        }
        if is_permitted(&path) {
            continue;
        }
        for n in names {
            offenders.push((path.clone(), n));
        }
    }

    // ANTI-VACUITY, checked BEFORE the verdict. If the scan stops finding the
    // install site — the file moves, the walk breaks, the names are renamed —
    // then "no offenders" is the empty set passing, not a clean result.
    assert!(
        install_site_seen,
        "did not find the install site (sui-eval/src/lib.rs naming the bridge \
         setters). Either it moved or this scan is broken — both make the \
         check below vacuous, and a vacuous check reports success."
    );

    assert!(
        offenders.is_empty(),
        "\nthese files install a VM bridge directly instead of calling \
         `sui_eval::install_vm_bridges()`:\n{}\n\n\
         That is how the path materializer went missing from the shipped \
         binary while every test stayed green. Add the bridge inside \
         `install_vm_bridges` and every consumer gets it; copy one out here \
         and the next fix reaches the tests and nothing else.",
        offenders
            .iter()
            .map(|(p, n)| format!("  {} — {n}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
