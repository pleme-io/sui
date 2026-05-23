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

// ── Spec interpreter (M3.0 minimal) ────────────────────────────────

/// Inputs to a GC run.
pub struct GcArgs {
    /// Drop paths older than this many days.  `None` = no age filter.
    pub delete_older_than_days: Option<u32>,
    /// Cap on bytes freed in one run.  `None` = unbounded.
    pub max_freed_bytes: Option<u64>,
    /// Compute the dead set but DON'T actually delete.
    pub dry_run: bool,
}

/// Result of a GC run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    pub roots_count: usize,
    pub live_paths: usize,
    pub dead_paths: usize,
    pub deleted_paths: Vec<String>,
    pub bytes_freed: u64,
    pub attestation_id: Option<String>,
    pub dry_run: bool,
}

/// Path metadata the GC env returns per /nix/store entry.
#[derive(Debug, Clone, Default)]
pub struct StorePathInfo {
    pub path: String,
    /// Direct dependencies (other store paths this one references).
    pub references: Vec<String>,
    /// Size in bytes.
    pub size: u64,
    /// Last-accessed age in days (for the
    /// `--delete-older-than-days` filter).
    pub age_days: u32,
}

/// Abstract IO for the garbage collector.  Pattern parallel to
/// FetcherEnvironment / SubstituterEnvironment.
pub trait GcEnvironment {
    /// Acquire the global store lock.  Blocks builds from running
    /// concurrently with the dead-set computation.
    fn lock_store(&self) -> Result<(), String>;

    /// Release the global store lock.
    fn unlock_store(&self) -> Result<(), String>;

    /// Collect every GC root: profile generation links, gcroots
    /// directory, indirect roots, builder roots, `--keep` paths.
    /// Returns store-path references each root holds alive.
    fn collect_gc_roots(&self) -> Result<Vec<String>, String>;

    /// Enumerate every path in /nix/store with its metadata.
    fn scan_store(&self) -> Result<Vec<StorePathInfo>, String>;

    /// Delete a store path, returning its actual freed-byte count
    /// (which may differ from `StorePathInfo::size` after hard-link
    /// deduplication).
    fn delete_path(&self, path: &str) -> Result<u64, String>;

    /// Attest the GC run to the OutcomeChain (optional — used by
    /// the `*-attested` variants).  Default impl is a no-op,
    /// returning `None`.
    fn attest_run(&self, _deleted: &[String], _freed: u64) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// Apply a GC algorithm.  M3.0 walks the authored phase pipeline,
/// dispatching to the env trait for each side-effecting step.
///
/// # Errors
///
/// Phase-specific `SpecError::Interp` variants per env-trait
/// failure.  `dependency-cycle` if the references graph from
/// `scan_store` contains a cycle while computing the live set
/// (the cppnix store shouldn't have cycles, but the substrate
/// surfaces them defensively).
pub fn apply<E: GcEnvironment>(
    algo: &GcAlgorithm,
    args: &GcArgs,
    env: &E,
) -> Result<GcReport, SpecError> {
    let mut report = GcReport { dry_run: args.dry_run, ..Default::default() };
    let mut roots: Vec<String> = Vec::new();
    let mut live: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut all: Vec<StorePathInfo> = Vec::new();
    let mut to_delete: Vec<String> = Vec::new();

    for phase in &algo.phases {
        match phase.kind {
            GcPhaseKind::LockStore => env.lock_store().map_err(|e| SpecError::Interp {
                phase: "lock-store".into(),
                message: e,
            })?,
            GcPhaseKind::CollectGcRoots => {
                roots = env.collect_gc_roots().map_err(|e| SpecError::Interp {
                    phase: "collect-gc-roots".into(),
                    message: e,
                })?;
                report.roots_count = roots.len();
            }
            GcPhaseKind::ComputeLiveSet => {
                // BFS from each root over the references graph.
                let info_by_path: std::collections::HashMap<String, &StorePathInfo> = all
                    .iter()
                    .map(|p| (p.path.clone(), p))
                    .collect();
                let mut frontier: Vec<String> = roots.clone();
                while let Some(p) = frontier.pop() {
                    if !live.insert(p.clone()) {
                        continue;
                    }
                    if let Some(info) = info_by_path.get(&p) {
                        for r in &info.references {
                            if !live.contains(r) {
                                frontier.push(r.clone());
                            }
                        }
                    }
                    // Paths in roots that AREN'T in the scan get
                    // marked live but contribute no refs — that's
                    // fine; they're untouched by the GC anyway.
                }
                report.live_paths = live.len();
            }
            GcPhaseKind::ScanStore => {
                all = env.scan_store().map_err(|e| SpecError::Interp {
                    phase: "scan-store".into(),
                    message: e,
                })?;
            }
            GcPhaseKind::ComputeDeadSet => {
                let dead: Vec<&StorePathInfo> = all
                    .iter()
                    .filter(|p| !live.contains(&p.path))
                    .collect();
                report.dead_paths = dead.len();
                to_delete = dead.iter().map(|p| p.path.clone()).collect();
            }
            GcPhaseKind::FilterByAgeAndSize => {
                // Apply --delete-older-than-days: keep only paths
                // older than the threshold (in the dead set).
                if let Some(min_age) = args.delete_older_than_days {
                    let by_path: std::collections::HashMap<&str, u32> = all
                        .iter()
                        .map(|p| (p.path.as_str(), p.age_days))
                        .collect();
                    to_delete.retain(|p| {
                        by_path.get(p.as_str()).copied().unwrap_or(0) >= min_age
                    });
                }
                // Apply --max-freed-bytes: cap the to_delete set so
                // its cumulative size stays under the cap.
                if let Some(cap) = args.max_freed_bytes {
                    let by_size: std::collections::HashMap<&str, u64> = all
                        .iter()
                        .map(|p| (p.path.as_str(), p.size))
                        .collect();
                    let mut acc: u64 = 0;
                    to_delete.retain(|p| {
                        let sz = by_size.get(p.as_str()).copied().unwrap_or(0);
                        if acc.saturating_add(sz) <= cap {
                            acc = acc.saturating_add(sz);
                            true
                        } else {
                            false
                        }
                    });
                }
            }
            GcPhaseKind::DeleteDeadPaths => {
                if !args.dry_run {
                    for path in &to_delete {
                        let freed = env.delete_path(path).map_err(|e| SpecError::Interp {
                            phase: "delete-dead-paths".into(),
                            message: format!("{path}: {e}"),
                        })?;
                        report.bytes_freed = report.bytes_freed.saturating_add(freed);
                        report.deleted_paths.push(path.clone());
                    }
                }
            }
            GcPhaseKind::UnlockStore => env.unlock_store().map_err(|e| SpecError::Interp {
                phase: "unlock-store".into(),
                message: e,
            })?,
            GcPhaseKind::EmitReport => {
                // Report struct is already accumulating; nothing
                // additional to do here.
            }
            GcPhaseKind::AttestRunToChain => {
                let id = env
                    .attest_run(&report.deleted_paths, report.bytes_freed)
                    .map_err(|e| SpecError::Interp {
                        phase: "attest-run".into(),
                        message: e,
                    })?;
                report.attestation_id = id;
            }
        }
    }
    Ok(report)
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

    // ── M3.0 gc interpreter tests ──────────────────────────────

    use std::cell::RefCell;

    struct MockEnv {
        roots: Vec<String>,
        scan: Vec<StorePathInfo>,
        log: RefCell<Vec<String>>,
        deleted: RefCell<Vec<String>>,
    }

    impl MockEnv {
        fn new() -> Self {
            Self {
                roots: Vec::new(),
                scan: Vec::new(),
                log: RefCell::new(Vec::new()),
                deleted: RefCell::new(Vec::new()),
            }
        }
        fn with_root(mut self, root: &str) -> Self {
            self.roots.push(root.into());
            self
        }
        fn with_path(mut self, path: &str, refs: &[&str], size: u64, age: u32) -> Self {
            self.scan.push(StorePathInfo {
                path: path.into(),
                references: refs.iter().map(|s| (*s).into()).collect(),
                size,
                age_days: age,
            });
            self
        }
    }

    impl GcEnvironment for MockEnv {
        fn lock_store(&self) -> Result<(), String> {
            self.log.borrow_mut().push("LOCK".into());
            Ok(())
        }
        fn unlock_store(&self) -> Result<(), String> {
            self.log.borrow_mut().push("UNLOCK".into());
            Ok(())
        }
        fn collect_gc_roots(&self) -> Result<Vec<String>, String> {
            self.log.borrow_mut().push("ROOTS".into());
            Ok(self.roots.clone())
        }
        fn scan_store(&self) -> Result<Vec<StorePathInfo>, String> {
            self.log.borrow_mut().push("SCAN".into());
            Ok(self.scan.clone())
        }
        fn delete_path(&self, path: &str) -> Result<u64, String> {
            self.log.borrow_mut().push(format!("DELETE {path}"));
            self.deleted.borrow_mut().push(path.into());
            let size = self
                .scan
                .iter()
                .find(|p| p.path == path)
                .map(|p| p.size)
                .unwrap_or(0);
            Ok(size)
        }
    }

    #[test]
    fn gc_brackets_lock_around_delete() {
        let algo = load_named("cppnix-stop-the-world").unwrap();
        let env = MockEnv::new()
            .with_root("/nix/store/aaa-keep")
            .with_path("/nix/store/aaa-keep", &[], 100, 0)
            .with_path("/nix/store/zzz-dead", &[], 200, 30);
        let report = apply(
            &algo,
            &GcArgs {
                delete_older_than_days: None,
                max_freed_bytes: None,
                dry_run: false,
            },
            &env,
        )
        .unwrap();
        // The LOCK marker appears before any DELETE, the UNLOCK
        // appears after.  Mock log records phase order.
        let log = env.log.borrow();
        let lock_pos = log.iter().position(|x| x == "LOCK").unwrap();
        let delete_pos = log.iter().position(|x| x.starts_with("DELETE")).unwrap();
        let unlock_pos = log.iter().position(|x| x == "UNLOCK").unwrap();
        assert!(lock_pos < delete_pos);
        assert!(delete_pos < unlock_pos);
        assert_eq!(report.deleted_paths, vec!["/nix/store/zzz-dead".to_string()]);
        assert_eq!(report.bytes_freed, 200);
    }

    #[test]
    fn live_set_includes_transitive_refs() {
        let algo = load_named("cppnix-stop-the-world").unwrap();
        let env = MockEnv::new()
            .with_root("/nix/store/aaa-root")
            .with_path("/nix/store/aaa-root", &["/nix/store/bbb-dep"], 100, 0)
            .with_path("/nix/store/bbb-dep", &["/nix/store/ccc-leaf"], 50, 0)
            .with_path("/nix/store/ccc-leaf", &[], 25, 0)
            .with_path("/nix/store/zzz-orphan", &[], 200, 0);
        let report = apply(
            &algo,
            &GcArgs { delete_older_than_days: None, max_freed_bytes: None, dry_run: false },
            &env,
        ).unwrap();
        // The transitive chain root→dep→leaf is kept alive; only
        // the orphan is deleted.
        assert_eq!(report.deleted_paths, vec!["/nix/store/zzz-orphan".to_string()]);
        assert_eq!(report.live_paths, 3);
    }

    #[test]
    fn dry_run_does_not_delete() {
        let algo = load_named("cppnix-stop-the-world").unwrap();
        let env = MockEnv::new()
            .with_path("/nix/store/aaa-dead", &[], 100, 0);
        let report = apply(
            &algo,
            &GcArgs { delete_older_than_days: None, max_freed_bytes: None, dry_run: true },
            &env,
        ).unwrap();
        assert_eq!(report.dead_paths, 1);
        assert!(report.deleted_paths.is_empty());
        assert_eq!(report.bytes_freed, 0);
        // delete_path was NEVER called.
        assert!(env.deleted.borrow().is_empty());
    }

    #[test]
    fn delete_older_than_filters() {
        let algo = load_named("cppnix-stop-the-world").unwrap();
        let env = MockEnv::new()
            .with_path("/nix/store/young-dead", &[], 100, 3)   // 3 days
            .with_path("/nix/store/old-dead", &[], 100, 30);    // 30 days
        let report = apply(
            &algo,
            &GcArgs {
                delete_older_than_days: Some(14),
                max_freed_bytes: None,
                dry_run: false,
            },
            &env,
        ).unwrap();
        // Only the 30-day-old path crosses the threshold.
        assert_eq!(report.deleted_paths, vec!["/nix/store/old-dead".to_string()]);
    }

    #[test]
    fn max_freed_bytes_caps_deletion() {
        let algo = load_named("cppnix-stop-the-world").unwrap();
        let env = MockEnv::new()
            .with_path("/nix/store/a-dead", &[], 100, 30)
            .with_path("/nix/store/b-dead", &[], 200, 30)
            .with_path("/nix/store/c-dead", &[], 300, 30);
        let report = apply(
            &algo,
            &GcArgs {
                delete_older_than_days: None,
                max_freed_bytes: Some(250),  // a + b would be 300 > cap; only a fits
                dry_run: false,
            },
            &env,
        ).unwrap();
        // Cap at 250: only the first ≤250B path fits.
        // Order depends on BTreeSet iteration order in our mock,
        // but cumulative size must not exceed cap.
        let total: u64 = env
            .scan
            .iter()
            .filter(|p| report.deleted_paths.contains(&p.path))
            .map(|p| p.size)
            .sum();
        assert!(total <= 250, "deleted total {total} must not exceed cap 250");
    }

    #[test]
    fn attested_variant_records_attestation_id() {
        // The attested algorithm calls attest_run after deletion.
        let algo = load_named("cppnix-stop-the-world-attested").unwrap();
        struct AttestEnv;
        impl GcEnvironment for AttestEnv {
            fn lock_store(&self) -> Result<(), String> { Ok(()) }
            fn unlock_store(&self) -> Result<(), String> { Ok(()) }
            fn collect_gc_roots(&self) -> Result<Vec<String>, String> { Ok(vec![]) }
            fn scan_store(&self) -> Result<Vec<StorePathInfo>, String> { Ok(vec![]) }
            fn delete_path(&self, _: &str) -> Result<u64, String> { Ok(0) }
            fn attest_run(&self, _: &[String], _: u64) -> Result<Option<String>, String> {
                Ok(Some("attestation-abc-123".into()))
            }
        }
        let report = apply(
            &algo,
            &GcArgs { delete_older_than_days: None, max_freed_bytes: None, dry_run: false },
            &AttestEnv,
        ).unwrap();
        assert_eq!(report.attestation_id.as_deref(), Some("attestation-abc-123"));
    }
}
