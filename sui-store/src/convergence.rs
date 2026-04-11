//! Convergence store extensions.
//!
//! Extends the sui Store trait with convergence-aware queries:
//! requisites, referrers, impact analysis, and generational tracking.
//! The convergence store IS the Nix store — attestations are store paths.

use sui_compat::store_path::StorePath;
use crate::traits::StoreResult;

/// A convergence-aware store path with generation tracking.
#[derive(Debug, Clone)]
pub struct GenerationalPath {
    /// The store path.
    pub path: StorePath,
    /// Which generation this convergence attestation belongs to.
    pub generation: u64,
    /// Blake3 hash of the previous generation (append-only chain).
    pub previous_hash: Option<String>,
}

/// Impact analysis result for a convergence point change.
#[derive(Debug, Clone, Default)]
pub struct ImpactReport {
    /// Store paths that would need re-convergence.
    pub affected_paths: Vec<StorePath>,
    /// Number of convergence points affected.
    pub affected_point_count: usize,
    /// Estimated re-convergence cost (arbitrary units).
    pub estimated_cost: f64,
}

/// Convergence-aware extensions to the sui Store.
///
/// These methods compose with the existing Store trait's
/// query_references() and compute_closure() to provide
/// convergence-specific queries.
#[allow(async_fn_in_trait)]
pub trait ConvergenceStore {
    /// Forward closure: all convergence points this point depends on.
    async fn convergence_requisites(&self, path: &StorePath) -> StoreResult<Vec<StorePath>>;

    /// Reverse closure: all points that depend on this point.
    async fn convergence_referrers(&self, path: &StorePath) -> StoreResult<Vec<StorePath>>;

    /// Impact analysis: what must re-converge if this point changes?
    async fn convergence_impact(&self, path: &StorePath) -> StoreResult<ImpactReport>;

    /// Latest generation for a convergence point.
    async fn convergence_generation(&self, path: &StorePath) -> StoreResult<u64>;

    /// Full generation history for a convergence point.
    async fn convergence_history(
        &self,
        path: &StorePath,
    ) -> StoreResult<Vec<GenerationalPath>>;
}

/// Default implementation that wraps the standard Store operations.
/// Real convergence-aware stores override these with optimized queries.
pub struct DefaultConvergenceStore;

impl ConvergenceStore for DefaultConvergenceStore {
    async fn convergence_requisites(&self, _path: &StorePath) -> StoreResult<Vec<StorePath>> {
        // Default: return empty — real implementation uses store's compute_closure
        Ok(Vec::new())
    }

    async fn convergence_referrers(&self, _path: &StorePath) -> StoreResult<Vec<StorePath>> {
        Ok(Vec::new())
    }

    async fn convergence_impact(&self, _path: &StorePath) -> StoreResult<ImpactReport> {
        Ok(ImpactReport::default())
    }

    async fn convergence_generation(&self, _path: &StorePath) -> StoreResult<u64> {
        Ok(0)
    }

    async fn convergence_history(
        &self,
        _path: &StorePath,
    ) -> StoreResult<Vec<GenerationalPath>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_default_convergence_store() {
        let store = DefaultConvergenceStore;
        let path = StorePath::from_basename("0000000000000000000000000000000a-test")
            .unwrap();

        let requisites = store.convergence_requisites(&path).await.unwrap();
        assert!(requisites.is_empty());

        let referrers = store.convergence_referrers(&path).await.unwrap();
        assert!(referrers.is_empty());

        let impact = store.convergence_impact(&path).await.unwrap();
        assert_eq!(impact.affected_point_count, 0);

        let generation = store.convergence_generation(&path).await.unwrap();
        assert_eq!(generation, 0);

        let history = store.convergence_history(&path).await.unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn test_generational_path() {
        let path = StorePath::from_basename("0000000000000000000000000000000a-test")
            .unwrap();
        let gp = GenerationalPath {
            path,
            generation: 42,
            previous_hash: Some("blake3:abc123".into()),
        };
        assert_eq!(gp.generation, 42);
        assert!(gp.previous_hash.is_some());
    }

    #[test]
    fn test_impact_report_default() {
        let report = ImpactReport::default();
        assert!(report.affected_paths.is_empty());
        assert_eq!(report.affected_point_count, 0);
        assert_eq!(report.estimated_cost, 0.0);
    }
}
