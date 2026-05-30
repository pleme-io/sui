//! Nix-replacement coverage catalog — one row per Nix workload sui
//! must cover. Authored as Lisp forms in
//! `specs/nix_replacement_coverage.lisp`, projected to typed Rust
//! records here.
//!
//! ## Why this exists
//!
//! `cli_coverage.lisp` catalogs sui's **argv surface** — every
//! subcommand the binary accepts and whether it routes to a Working
//! implementation. This catalog complements it with the **workload
//! surface** — every behavior real operators run (link-in-place, gc,
//! sandboxed builds, substituter push/pull, module-system fixed
//! point, etc.), independent of which command name they reach it by.
//!
//! Together the two catalogs are sui's complete answer to "what's
//! left to be a full nix replacement?"
//!
//! ## Wire shape
//!
//! ```lisp
//! (defnix-replacement-surface
//!   :name     "lockfile-graph"
//!   :category SubstrateL1
//!   :status   Done
//!   :owns     "sui-spec::lockfile_graph"
//!   :notes    "Parsed + follows-resolved + content-addressed lockfile.")
//! ```
//!
//! Adding a row is the canonical way to declare a new must-cover
//! workload. Marking `:status Done` is a *verifiable* claim — the
//! `owns` field points at the typed Rust piece that implements the
//! surface, so reviewers can check.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

/// One workload sui must cover to fully replace cppnix.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defnix-replacement-surface")]
pub struct NixReplacementSurface {
    /// Stable name. Used as the catalog key.
    pub name: String,
    /// Which layer this workload belongs to.
    pub category: WorkloadCategory,
    /// Coverage status today.
    pub status: CoverageStatus,
    /// The typed Rust piece that owns (or will own) the implementation.
    /// Free-form `crate::module::Type` reference; reviewers grep for it.
    pub owns: String,
    /// Operator-readable description.
    pub notes: String,
}

/// Stable workload categories.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkloadCategory {
    /// `/nix/store` layout, gc, optimize, add-path, verify.
    Storage,
    /// L1 typed graph substrate (lockfile / AST / module / derivation).
    SubstrateL1,
    /// Nix language evaluator (builtins, flake eval, module system).
    EvalEngine,
    /// Derivation hash + build sandbox.
    Derivation,
    /// Build execution (sandboxed builder, output realization).
    Build,
    /// Source fetcher (github, git, tarball, path, ...).
    Fetcher,
    /// Binary cache (narinfo pull/push, typed-closure stream).
    Substituter,
    /// Daemon protocols (cppnix worker, sui-native graph, fleet
    /// work-stealing).
    Daemon,
    /// System rebuild (nixos / darwin / home-manager).
    SystemRebuild,
    /// CLI convenience surfaces operators rely on (`nix-shell`,
    /// `nix-channel`, `nixos-rebuild` host script).
    Convenience,
}

/// Stable coverage statuses. Ordered worst → best for sort purposes.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CoverageStatus {
    /// No code yet; row exists to claim the gap.
    NotStarted,
    /// In the backlog with an owner identified but no code yet.
    Queued,
    /// Some implementation exists; gaps documented in `notes`.
    InProgress,
    /// Operator can use sui for this workload today without behavior
    /// loss vs cppnix.
    Done,
}

impl CoverageStatus {
    /// Display name for dashboards.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::NotStarted => "NotStarted",
            Self::Queued => "Queued",
            Self::InProgress => "InProgress",
            Self::Done => "Done",
        }
    }
}

pub const CANONICAL_NIX_REPLACEMENT_COVERAGE_LISP: &str =
    include_str!("../specs/nix_replacement_coverage.lisp");

/// Load the full canonical coverage catalog.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<NixReplacementSurface>, SpecError> {
    crate::loader::load_all::<NixReplacementSurface>(CANONICAL_NIX_REPLACEMENT_COVERAGE_LISP)
}

/// Coverage histogram — number of surfaces in each status bucket.
#[derive(Debug, Default, Clone, Copy)]
pub struct CoverageHistogram {
    pub done: u32,
    pub in_progress: u32,
    pub queued: u32,
    pub not_started: u32,
    pub total: u32,
}

impl CoverageHistogram {
    /// Build a histogram from a slice of surfaces.
    #[must_use]
    pub fn from_surfaces(surfaces: &[NixReplacementSurface]) -> Self {
        let mut h = Self::default();
        for s in surfaces {
            h.total += 1;
            match s.status {
                CoverageStatus::Done => h.done += 1,
                CoverageStatus::InProgress => h.in_progress += 1,
                CoverageStatus::Queued => h.queued += 1,
                CoverageStatus::NotStarted => h.not_started += 1,
            }
        }
        h
    }

    /// Fraction of surfaces marked Done. 0.0..=1.0.
    #[must_use]
    pub fn done_fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.done as f32 / self.total as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn catalog_loads() {
        let cat = load_canonical().expect("catalog parses");
        // Make sure we have at least one row per category we declared.
        let categories: std::collections::HashSet<_> = cat.iter().map(|s| s.category).collect();
        assert!(categories.contains(&WorkloadCategory::Storage));
        assert!(categories.contains(&WorkloadCategory::SubstrateL1));
        assert!(categories.contains(&WorkloadCategory::EvalEngine));
        assert!(categories.contains(&WorkloadCategory::Derivation));
        assert!(categories.contains(&WorkloadCategory::Fetcher));
        assert!(categories.contains(&WorkloadCategory::Substituter));
        assert!(categories.contains(&WorkloadCategory::Daemon));
        assert!(categories.contains(&WorkloadCategory::SystemRebuild));
    }

    #[test]
    fn histogram_sums_to_total() {
        let cat = load_canonical().unwrap();
        let h = CoverageHistogram::from_surfaces(&cat);
        assert_eq!(h.total, cat.len() as u32);
        assert_eq!(
            h.done + h.in_progress + h.queued + h.not_started,
            h.total
        );
    }

    #[test]
    fn done_fraction_in_range() {
        let cat = load_canonical().unwrap();
        let h = CoverageHistogram::from_surfaces(&cat);
        let f = h.done_fraction();
        assert!((0.0..=1.0).contains(&f));
    }

    #[test]
    fn every_surface_has_owns_pointer() {
        let cat = load_canonical().unwrap();
        for s in &cat {
            assert!(!s.owns.is_empty(), "row {} missing :owns", s.name);
        }
    }

    #[test]
    fn lockfile_graph_is_done() {
        let cat = load_canonical().unwrap();
        let row = cat
            .iter()
            .find(|s| s.name == "lockfile-graph")
            .expect("lockfile-graph row");
        assert!(matches!(row.status, CoverageStatus::Done));
    }

    #[test]
    fn daemon_graph_protocol_is_done() {
        let cat = load_canonical().unwrap();
        let row = cat
            .iter()
            .find(|s| s.name == "daemon-graph-protocol-native")
            .expect("daemon-graph-protocol-native row");
        assert!(matches!(row.status, CoverageStatus::Done));
    }
}
