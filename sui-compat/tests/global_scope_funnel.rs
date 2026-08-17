//! Every engine derives the bare-identifier scope from `sui_compat::scope` —
//! enforced, not mirrored by comment.
//!
//! # The regression this guards
//!
//! The set of names Nix resolves bare (`map`, `throw`, `import`, `break`, …)
//! was hand-written into three engines. `sui-ir`'s copy even carried the
//! comment *"mirrored from `sui-eval/src/builtins/mod.rs`"* — and mirroring by
//! hand is exactly how it broke: the walker and `sui-ir` were both missing
//! `break`, which the bytecode VM had.
//!
//! That is a wrong ANSWER, not a cosmetic gap. A genuine nix global cannot be
//! shadowed by a `with`, so:
//!
//! ```text
//! with { break = "LIB"; }; break == "LIB"
//!   nix         false
//!   VM          false
//!   tree-walker true      ← changed what the program means
//! ```
//!
//! Measured: `builtins.typeOf break` → `lambda` on nix 2.31.5, so the VM was
//! right and the majority was wrong — which is the argument for measuring
//! against the oracle rather than reconciling the copies against each other.
//!
//! Third instance of one shape: `IMPERSONATED_NIX_VERSION` (the VM two minor
//! versions behind), the `builtins` attrset name set, and now this.
//!
//! TIER: CI-caught. Truly-unrepresentable would mean no engine can construct a
//! scope except through a constructor this crate owns — worth doing when a
//! fourth engine appears, not for three. Named, not scheduled.

use std::path::{Path, PathBuf};

/// Names from the shared list that are distinctive enough that finding a
/// literal copy of one, outside the owning module, means a hand-written scope.
///
/// Deliberately NOT names like `"map"` or `"import"`, which appear all over a
/// compiler for unrelated reasons — a scan keyed on those would false-positive
/// constantly and then get an allowlist, and an allowlist is how a guard dies.
const DISTINCTIVE: &[&str] = &["derivationStrict", "scopedImport", "fetchMercurial"];

fn workspace_sources() -> Vec<PathBuf> {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop();
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

/// Files allowed to hold the literal names.
fn is_permitted(p: &Path) -> bool {
    // The owning module, and this guard (which names them in DISTINCTIVE and
    // would otherwise satisfy its own denominator).
    p.ends_with("sui-compat/src/scope.rs")
        || p.ends_with("sui-compat/tests/global_scope_funnel.rs")
        // Builtin REGISTRATION legitimately names every builtin — that is a
        // different concern from the bare-identifier SCOPE, and the names
        // overlap. Registration sites are excluded by path.
        || p.ends_with("sui-eval/src/builtins/mod.rs")
        || p.ends_with("sui-ir/src/builtins.rs")
        || p.ends_with("sui-bytecode/src/builtins.rs")
}

/// A file that lists SEVERAL distinctive globals together is writing its own
/// scope. One mention is a registration or a test; all three together is a
/// copy of the list.
#[test]
fn no_engine_hand_writes_the_global_scope() {
    let mut offenders = Vec::new();
    let mut scanned = 0usize;

    for path in workspace_sources() {
        if is_permitted(&path) {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        scanned += 1;
        let hits: Vec<&str> = DISTINCTIVE
            .iter()
            .copied()
            .filter(|n| src.contains(&format!("\"{n}\"")))
            .collect();
        if hits.len() == DISTINCTIVE.len() {
            offenders.push(path.clone());
        }
    }

    // ANTI-VACUITY, before the verdict. A broken walk or an over-wide
    // allowlist would leave `offenders` empty and report success over nothing.
    assert!(
        scanned > 100,
        "only scanned {scanned} files — the walk is broken, and 'no offenders' \
         over a broken walk is not a clean result"
    );

    assert!(
        offenders.is_empty(),
        "\nthese files hand-write the bare-identifier global scope instead of \
         deriving it from `sui_compat::scope`:\n{}\n\n\
         Three hand-written copies is how `break` ended up in one engine and \
         not the other two, which let a `with` shadow a real nix global and \
         changed what a program means. Derive it.",
        offenders
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The shared list must match what nix actually resolves.
///
/// Skipped without `SUI_TEST_ONLINE=1`; the offline half above still enforces
/// that there is ONE list, so a skip here narrows sharpening, not coverage.
#[test]
fn the_global_scope_matches_real_nix() {
    if std::env::var("SUI_TEST_ONLINE").as_deref() != Ok("1") {
        eprintln!("the_global_scope_matches_real_nix: skipped (SUI_TEST_ONLINE unset)");
        return;
    }

    let mut wrong = Vec::new();
    for name in sui_compat::scope::CALLABLE_GLOBALS {
        let out = std::process::Command::new("nix")
            .args(["eval", "--impure", "--raw", "--expr"])
            .arg(format!("builtins.typeOf {name}"))
            .output();
        let Ok(out) = out else { continue };
        if !out.status.success() {
            wrong.push((*name, String::from_utf8_lossy(&out.stderr).trim().to_string()));
        }
    }
    assert!(
        wrong.is_empty(),
        "these are listed as bare nix globals but nix does not resolve them: \
         {wrong:?}. Either nix dropped them or they were never globals — \
         re-measure with `nix eval --impure --raw --expr 'builtins.typeOf \
         <name>'` rather than deleting the row to make this pass."
    );

    // Calibration: a real builtin that is NOT bare must stay unresolvable, or
    // a nix that resolved everything would make the loop above vacuous.
    let probe = std::process::Command::new("nix")
        .args(["eval", "--impure", "--raw", "--expr", "builtins.typeOf attrNames"])
        .output();
    if let Ok(p) = probe {
        assert!(
            !p.status.success(),
            "`attrNames` resolved as a bare global — nix's scope is wider than \
             this list models, or the probe is not measuring what it claims"
        );
    }
}
