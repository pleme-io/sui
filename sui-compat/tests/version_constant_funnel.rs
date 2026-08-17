//! Every engine must DERIVE `builtins.nixVersion` / `builtins.langVersion`
//! from `sui_compat::versions`, never re-hardcode them.
//!
//! WHY A SOURCE-LEVEL TEST. The constants are now shared, which fixes today's
//! drift — but nothing stops the next engine from writing its own literal, and
//! that is precisely how this defect happened: the tree-walker and `sui-ir`
//! were corrected from `"2.24.0"` to `"2.34.7"` and the bytecode VM was left
//! behind, because "mirroring the walker's values byte-for-byte" was a comment
//! rather than a mechanism.
//!
//! The failure it guards is silent and consequential. nixpkgs and nix-darwin
//! feature-gate on `lib.versionAtLeast builtins.nixVersion "X"`, so an engine
//! with a stale value takes the WRONG branch of every gate between its value
//! and the real one — evaluating a different derivation graph with no error.
//!
//! A RUNTIME comparison would be the obvious alternative and is strictly
//! weaker here: no crate in the workspace depends on all three engines, so a
//! runtime test could only cover the ones its own crate can reach, and a fourth
//! engine would be invisible to it. This check is over the SOURCE, so it covers
//! every engine that exists and every engine added later.
//!
//! TIER: CI-caught (a red `cargo test`), not unrepresentable. The
//! truly-unrepresentable version is a single constructor for the whole
//! `builtins` constant block that every engine must call — named, not
//! scheduled.

use std::path::{Path, PathBuf};

/// Every `src/**/*.rs` in the workspace.
fn workspace_sources() -> Vec<PathBuf> {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // sui-compat/ -> workspace root

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
                // Skip build output and vendored trees; `target` in particular
                // holds generated copies that would produce phantom hits.
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

/// The constant's own home, and this test itself.
///
/// Excluding THIS FILE matters for the denominator, not just for tidiness: it
/// names `"nixVersion"` (in the detector) and `IMPERSONATED_NIX_VERSION` (in
/// the assertion), so it matches its own "installer that derives" predicate and
/// silently counted itself toward the floor below. A gate that partly satisfies
/// its own anti-vacuity floor is measuring itself.
fn is_the_owning_module(p: &Path) -> bool {
    p.ends_with("sui-compat/src/versions.rs")
        || p.ends_with("sui-compat/tests/version_constant_funnel.rs")
}

/// A file that installs `builtins.nixVersion` (or `langVersion`) must reach the
/// shared constant. Anything else is a re-hardcode.
#[test]
fn no_engine_hardcodes_the_nix_version() {
    let mut installers = Vec::new();
    let mut offenders = Vec::new();

    for path in workspace_sources() {
        if is_the_owning_module(&path) {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Only files that actually INSTALL the builtin are in scope — a file
        // merely mentioning the name (a test, a doc comment, a match arm) is
        // not required to import the constant.
        //
        // The match is over a WINDOW, not a single line, and that is not
        // incidental: the first version of this check required `"nixVersion"`
        // and `insert`/`intern` on the same line, and the very commit that
        // added it reformatted two of the three installers into multi-line
        // `insert(` calls — so the detector went blind to 2 of 3 and would have
        // reported "no offenders" over a denominator of one. The floor below
        // caught it. Keep both.
        let lines: Vec<&str> = src.lines().collect();
        let installs = lines.iter().enumerate().any(|(i, l)| {
            if !l.contains("\"nixVersion\"") {
                return false;
            }
            let lo = i.saturating_sub(3);
            let hi = (i + 4).min(lines.len());
            lines[lo..hi]
                .iter()
                .any(|w| w.contains("insert") || w.contains("intern"))
        });
        if !installs {
            continue;
        }
        installers.push(path.clone());
        if !src.contains("IMPERSONATED_NIX_VERSION") {
            offenders.push(path.clone());
        }
    }

    // ANTI-VACUITY, and it comes first. If the scan stops finding installers —
    // a refactor renames the key, the walk breaks, the skip list widens — then
    // "no offenders" is the empty set passing, not a clean result. The
    // denominator is checked before the verdict, and it is a FLOOR rather than
    // an exact count so that adding a fifth engine does not demand an edit
    // here.
    assert!(
        installers.len() >= 3,
        "expected at least 3 engines installing `builtins.nixVersion` \
         (tree-walker, bytecode VM, sui-ir); found {}. Either the scan broke or \
         an engine stopped installing it — both make this check vacuous, and a \
         vacuous check reports success.\nfound: {installers:#?}",
        installers.len()
    );

    assert!(
        offenders.is_empty(),
        "\nthese files install `builtins.nixVersion` without reading \
         `sui_compat::versions::IMPERSONATED_NIX_VERSION`:\n{}\n\n\
         Hand-mirroring is what let the bytecode VM sit at \"2.24.0\" while the \
         walker said \"2.34.7\" — nixpkgs feature-gates on \
         `lib.versionAtLeast builtins.nixVersion`, so the two engines evaluated \
         DIFFERENT derivation graphs and neither errored. Derive it.\n\
         ({} installer(s) scanned)",
        offenders
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n"),
        installers.len()
    );

    eprintln!(
        "no_engine_hardcodes_the_nix_version: {} installers, all deriving",
        installers.len()
    );
}

/// The constant must be a plausible version, so a typo cannot quietly make
/// every `versionAtLeast` gate take the low branch.
#[test]
fn the_impersonated_version_is_well_formed() {
    let v = sui_compat::versions::IMPERSONATED_NIX_VERSION;
    assert!(!v.is_empty(), "IMPERSONATED_NIX_VERSION is empty");
    let parts = sui_compat::versions::split_version(v);
    assert!(
        parts.len() >= 3,
        "IMPERSONATED_NIX_VERSION {v:?} does not look like a version"
    );
    // Compared through the module's own comparator, so the constant is checked
    // by the same algorithm nixpkgs' gates will run against it.
    assert_eq!(
        sui_compat::versions::compare_versions(v, "2.0"),
        1,
        "IMPERSONATED_NIX_VERSION {v:?} does not order above 2.0 — every \
         `versionAtLeast` gate in nixpkgs would take the low branch"
    );
    assert!(sui_compat::versions::LANG_VERSION > 0);
}
