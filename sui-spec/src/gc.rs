//! Typed border for the nix store garbage collector.
//!
//! cppnix's `nix-collect-garbage` walks the GC root set (profiles,
//! `/nix/var/nix/gcroots`, `--keep` paths, indirect roots), computes
//! the live set as the transitive references from those roots, and
//! deletes everything in `/nix/store` outside the live set.  Various
//! flags control the policy: `--delete-older-than`, `--max-freed`,
//! `--dry-run`.
//!
//! Today sui-store has an implementation; this module names the
//! algorithm as a typed Lisp spec so the impl rides on an explicit
//! contract.  Future GC variants (concurrent / lazy / mark-and-sweep
//! optimised) hang off this typed border.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defgc-algorithm
//!   :name "cppnix-stop-the-world"
//!   :phases ((:kind LockStore)
//!            (:kind CollectGcRoots :bind "roots")
//!            (:kind ComputeLiveSet :from "roots" :bind "live")
//!            (:kind ScanStore :bind "all-paths")
//!            (:kind ComputeDeadSet :from "all-paths")
//!            (:kind FilterByAgeAndSize)
//!            (:kind DeleteDeadPaths)
//!            (:kind UnlockStore)
//!            (:kind EmitReport)))
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border ───────────────────────────────────────────────────

/// One garbage-collection algorithm.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defgc-algorithm")]
pub struct GcAlgorithm {
    pub name: String,
    pub phases: Vec<GcPhase>,
}

/// One phase of a GC pipeline.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GcPhase {
    pub kind: GcPhaseKind,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
}

/// Closed set of GC phases.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GcPhaseKind {
    /// Acquire the global store lock to prevent concurrent builds
    /// from racing with the dead-set computation.
    LockStore,
    /// Collect every GC root: profile generation links, gcroots
    /// directory, indirect roots, builder roots, `--keep` paths.
    CollectGcRoots,
    /// Compute the live set as the transitive closure of references
    /// reachable from the GC roots.
    ComputeLiveSet,
    /// Enumerate every path in /nix/store.
    ScanStore,
    /// Subtract live set from all paths to derive the dead set.
    ComputeDeadSet,
    /// Apply policy filters: `--delete-older-than`, `--max-freed`,
    /// `--keep-going`.  Some dead paths may survive a single GC if
    /// the operator's policy caps the deletion rate.
    FilterByAgeAndSize,
    /// Delete dead paths from /nix/store.  Recursively `chmod -R u+w`
    /// store entries that have read-only bits set, then unlink.
    DeleteDeadPaths,
    /// Release the store lock.
    UnlockStore,
    /// Emit a typed `GcReport` (paths deleted, bytes freed, runtime)
    /// for operator surfacing.
    EmitReport,
    /// Optional: attest the GC run to the OutcomeChain so audit
    /// trails carry the deletion event.  Skipped on hosts without
    /// the attestation layer.
    AttestRunToChain,
}

// ── Spec interpreter (M3 stub) ─────────────────────────────────────

/// Inputs to a GC run.  M3 will replace this with typed values.
pub struct GcArgs {
    pub delete_older_than_days: Option<u32>,
    pub max_freed_bytes: Option<u64>,
    pub dry_run: bool,
}

/// Apply the GC algorithm.  M3 stub — returns typed
/// `not-yet-implemented` so the implementation gap surfaces.
///
/// # Errors
///
/// Always returns `SpecError::Interp { phase: "gc" }` until M3.
pub fn apply(_algo: &GcAlgorithm, _args: GcArgs) -> Result<String, SpecError> {
    Err(SpecError::Interp {
        phase: "gc".into(),
        message: "GC spec interpreter not yet landed — sui-store has a \
                  working impl today, M3 work lifts it to consume this spec"
            .into(),
    })
}

// ── Canonical spec ─────────────────────────────────────────────────

pub const CANONICAL_GC_LISP: &str = include_str!("../specs/gc.lisp");

/// Compile every authored GC algorithm.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<GcAlgorithm>, SpecError> {
    crate::loader::load_all::<GcAlgorithm>(CANONICAL_GC_LISP)
}

/// Return the GC algorithm whose `name` matches.
///
/// # Errors
///
/// Returns an error if the spec fails to parse or `name` is missing.
pub fn load_named(name: &str) -> Result<GcAlgorithm, SpecError> {
    load_canonical()?
        .into_iter()
        .find(|a| a.name == name)
        .ok_or_else(|| SpecError::Load(format!("no (defgc-algorithm) with :name {name:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_gc_algorithms_parse() {
        let algos = load_canonical().expect("canonical gc must compile");
        assert!(!algos.is_empty());
    }

    #[test]
    fn cppnix_stop_the_world_algorithm_exists() {
        let _algo = load_named("cppnix-stop-the-world")
            .expect("cppnix stop-the-world GC algorithm must exist");
    }

    #[test]
    fn every_gc_algorithm_brackets_lock_around_critical_section() {
        let algos = load_canonical().unwrap();
        for algo in &algos {
            let kinds: Vec<GcPhaseKind> =
                algo.phases.iter().map(|p| p.kind).collect();
            // Must lock, must unlock, lock must come before delete,
            // unlock must come after delete.
            let lock_pos = kinds.iter().position(|k| *k == GcPhaseKind::LockStore);
            let delete_pos = kinds.iter().position(|k| *k == GcPhaseKind::DeleteDeadPaths);
            let unlock_pos = kinds.iter().position(|k| *k == GcPhaseKind::UnlockStore);
            assert!(lock_pos.is_some(), "{}: missing LockStore", algo.name);
            assert!(unlock_pos.is_some(), "{}: missing UnlockStore", algo.name);
            assert!(delete_pos.is_some(), "{}: missing DeleteDeadPaths", algo.name);
            assert!(
                lock_pos.unwrap() < delete_pos.unwrap(),
                "{}: LockStore must precede DeleteDeadPaths",
                algo.name,
            );
            assert!(
                delete_pos.unwrap() < unlock_pos.unwrap(),
                "{}: DeleteDeadPaths must precede UnlockStore",
                algo.name,
            );
        }
    }

    #[test]
    fn every_gc_algorithm_computes_live_before_dead() {
        let algos = load_canonical().unwrap();
        for algo in &algos {
            let kinds: Vec<GcPhaseKind> =
                algo.phases.iter().map(|p| p.kind).collect();
            let live = kinds.iter().position(|k| *k == GcPhaseKind::ComputeLiveSet);
            let dead = kinds.iter().position(|k| *k == GcPhaseKind::ComputeDeadSet);
            if let (Some(l), Some(d)) = (live, dead) {
                assert!(
                    l < d,
                    "{}: ComputeLiveSet must precede ComputeDeadSet",
                    algo.name,
                );
            }
        }
    }

    #[test]
    fn apply_is_typed_not_yet() {
        let algo = load_named("cppnix-stop-the-world").unwrap();
        let err = apply(
            &algo,
            GcArgs {
                delete_older_than_days: Some(14),
                max_freed_bytes: None,
                dry_run: false,
            },
        )
        .expect_err("apply must return error until M3");
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "gc");
                assert!(message.contains("not yet landed"));
            }
            _ => panic!("expected SpecError::Interp"),
        }
    }
}
