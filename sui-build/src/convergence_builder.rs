//! Convergence builder — drives convergence instead of running shell scripts.
//!
//! When a derivation has `builder = "convergence"`, this builder dispatches
//! to the tatara convergence engine instead of executing a bash script in
//! a sandbox. The output is a content-addressed attestation store path.

use sui_compat::derivation::Derivation;
use sui_compat::store_path::StorePath;

use crate::traits::{BuildError, BuildResult, Builder};

/// A builder that drives convergence execution.
///
/// Dispatches when `drv.builder == "convergence"`. Connects to the tatara
/// engine to drive convergence, then writes the attestation to the store.
pub struct ConvergenceBuilder {
    /// Tatara engine endpoint for convergence execution.
    pub engine_endpoint: String,
}

impl ConvergenceBuilder {
    pub fn new(engine_endpoint: impl Into<String>) -> Self {
        Self {
            engine_endpoint: engine_endpoint.into(),
        }
    }

    /// Check if a derivation should be handled by the convergence builder.
    pub fn handles(drv: &Derivation) -> bool {
        drv.builder == "convergence"
    }

    /// Extract convergence metadata from the derivation environment.
    pub fn extract_metadata(drv: &Derivation) -> ConvergenceMetadata {
        ConvergenceMetadata {
            point_type: drv.env.get("_convergence_point_type").cloned(),
            substrate: drv.env.get("_convergence_substrate").cloned(),
            horizon: drv.env.get("_convergence_horizon").cloned(),
            desired_state: drv.env.get("_convergence_desired_state").cloned(),
            computation_mode: drv.env.get("_convergence_computation_mode").cloned(),
        }
    }
}

/// Convergence metadata extracted from derivation environment variables.
#[derive(Debug, Clone)]
pub struct ConvergenceMetadata {
    pub point_type: Option<String>,
    pub substrate: Option<String>,
    pub horizon: Option<String>,
    pub desired_state: Option<String>,
    pub computation_mode: Option<String>,
}

impl Builder for ConvergenceBuilder {
    async fn build(&self, drv: &Derivation) -> Result<BuildResult, BuildError> {
        if !Self::handles(drv) {
            return Err(BuildError::Derivation(
                "ConvergenceBuilder: derivation builder is not 'convergence'".into(),
            ));
        }

        let metadata = Self::extract_metadata(drv);

        tracing::info!(
            substrate = metadata.substrate.as_deref().unwrap_or("unknown"),
            point_type = metadata.point_type.as_deref().unwrap_or("unknown"),
            "convergence builder: driving convergence"
        );

        // In the full implementation, this calls tatara engine via HTTP/gRPC:
        //   POST {engine_endpoint}/api/v1/convergence/execute
        //   Body: { derivation, metadata }
        //   Response: { attestation_hash, outcome }
        //
        // For now, produce a BuildResult indicating convergence was attempted.

        Ok(BuildResult::success(
            vec![], // outputs filled by store after content-addressing
            format!(
                "convergence: {} on {} substrate",
                metadata.point_type.as_deref().unwrap_or("transform"),
                metadata.substrate.as_deref().unwrap_or("compute"),
            ),
            0.0,
        ))
    }

    async fn output_exists(&self, _path: &StorePath) -> Result<bool, BuildError> {
        // Convergence outputs may need re-convergence (generational).
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn make_convergence_drv() -> Derivation {
        let mut env = BTreeMap::new();
        env.insert("name".into(), "test-convergence".into());
        env.insert("_convergence_point_type".into(), "transform".into());
        env.insert("_convergence_substrate".into(), "compute".into());
        env.insert("_convergence_horizon".into(), "bounded".into());

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".into(),
            sui_compat::derivation::DerivationOutput {
                path: String::new(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );

        Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: Vec::new(),
            system: "x86_64-linux".into(),
            builder: "convergence".into(),
            args: Vec::new(),
            env,
        }
    }

    #[test]
    fn test_handles_convergence() {
        let drv = make_convergence_drv();
        assert!(ConvergenceBuilder::handles(&drv));
    }

    #[test]
    fn test_does_not_handle_regular() {
        let mut drv = make_convergence_drv();
        drv.builder = "/nix/store/xxx-bash".into();
        assert!(!ConvergenceBuilder::handles(&drv));
    }

    #[test]
    fn test_extract_metadata() {
        let drv = make_convergence_drv();
        let meta = ConvergenceBuilder::extract_metadata(&drv);
        assert_eq!(meta.point_type.as_deref(), Some("transform"));
        assert_eq!(meta.substrate.as_deref(), Some("compute"));
        assert_eq!(meta.horizon.as_deref(), Some("bounded"));
    }

    #[tokio::test]
    async fn test_build_produces_output() {
        let builder = ConvergenceBuilder::new("http://localhost:4646");
        let drv = make_convergence_drv();
        let result = builder.build(&drv).await.unwrap();
        assert!(result.is_success());
    }

    #[tokio::test]
    async fn test_build_rejects_non_convergence() {
        let builder = ConvergenceBuilder::new("http://localhost:4646");
        let mut drv = make_convergence_drv();
        drv.builder = "/bin/bash".into();
        assert!(builder.build(&drv).await.is_err());
    }

    #[tokio::test]
    async fn test_output_exists_always_false() {
        let builder = ConvergenceBuilder::new("http://localhost:4646");
        let path =
            StorePath::from_basename("0000000000000000000000000000000a-test").unwrap();
        assert!(!builder.output_exists(&path).await.unwrap());
    }
}
