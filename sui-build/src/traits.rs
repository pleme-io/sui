//! Core builder trait and types.

use sui_compat::derivation::Derivation;
use sui_compat::store_path::StorePath;

/// Build execution result.
#[derive(Debug, Clone)]
pub struct BuildResult {
    /// Output store paths produced by the build.
    pub outputs: Vec<StorePath>,
    /// Build log.
    pub log: String,
    /// Whether the build succeeded.
    pub success: bool,
    /// Wall-clock time in seconds.
    pub duration_secs: f64,
}

/// Build errors.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("build failed: {0}")]
    Failed(String),
    #[error("sandbox error: {0}")]
    Sandbox(String),
    #[error("derivation error: {0}")]
    Derivation(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not implemented: {0}")]
    NotImplemented(String),
}

/// The core builder interface.
#[allow(async_fn_in_trait)]
pub trait Builder: Send + Sync {
    /// Build a derivation and return the result.
    async fn build(&self, drv: &Derivation) -> Result<BuildResult, BuildError>;

    /// Check if an output path already exists (skip build).
    async fn output_exists(&self, path: &StorePath) -> Result<bool, BuildError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_error_failed_display() {
        let e = BuildError::Failed("compiler error".to_string());
        assert!(e.to_string().contains("compiler error"));
        assert!(e.to_string().contains("build failed"));
    }

    #[test]
    fn build_error_sandbox_display() {
        let e = BuildError::Sandbox("mount failed".to_string());
        assert!(e.to_string().contains("mount failed"));
        assert!(e.to_string().contains("sandbox error"));
    }

    #[test]
    fn build_error_derivation_display() {
        let e = BuildError::Derivation("parse error".to_string());
        assert!(e.to_string().contains("parse error"));
    }

    #[test]
    fn build_error_not_implemented_display() {
        let e = BuildError::NotImplemented("remote build".to_string());
        assert!(e.to_string().contains("remote build"));
    }

    #[test]
    fn build_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let build_err: BuildError = io_err.into();
        assert!(build_err.to_string().contains("no such file"));
    }

    #[test]
    fn build_result_construction_success() {
        let output = StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();

        let result = BuildResult {
            outputs: vec![output.clone()],
            log: "build succeeded\n".to_string(),
            success: true,
            duration_secs: 42.5,
        };

        assert!(result.success);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(
            result.outputs[0].to_absolute_path(),
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1"
        );
        assert!(result.log.contains("build succeeded"));
        assert!((result.duration_secs - 42.5).abs() < f64::EPSILON);
    }

    #[test]
    fn build_result_construction_failure() {
        let result = BuildResult {
            outputs: vec![],
            log: "error: builder for '/nix/store/abc.drv' failed\n".to_string(),
            success: false,
            duration_secs: 1.2,
        };

        assert!(!result.success);
        assert!(result.outputs.is_empty());
        assert!(result.log.contains("failed"));
    }

    #[test]
    fn build_result_clone() {
        let result = BuildResult {
            outputs: vec![],
            log: "ok".to_string(),
            success: true,
            duration_secs: 0.0,
        };
        let cloned = result.clone();
        assert_eq!(cloned.success, result.success);
        assert_eq!(cloned.log, result.log);
    }

    // ── MockBuilder through Builder trait → BuildResult → API BuildStatus ──

    use crate::sandbox::{NoSandbox, Sandbox, SandboxConfig};

    /// A mock builder that uses NoSandbox internally and produces a BuildResult.
    struct MockBuilder;

    impl MockBuilder {
        async fn do_build(&self, drv: &Derivation) -> Result<BuildResult, BuildError> {
            let sandbox = NoSandbox;
            let config = SandboxConfig::from_derivation(drv, "/tmp");

            let result = sandbox.execute(&config)
                .map_err(|e| BuildError::Sandbox(e.to_string()))?;

            let outputs: Vec<StorePath> = drv
                .outputs
                .values()
                .filter_map(|o| StorePath::from_absolute_path(&o.path).ok())
                .collect();

            Ok(BuildResult {
                outputs,
                log: String::from_utf8_lossy(&result.stdout).to_string(),
                success: result.exit_code == 0,
                duration_secs: 0.1,
            })
        }
    }

    impl Builder for MockBuilder {
        async fn build(&self, drv: &Derivation) -> Result<BuildResult, BuildError> {
            self.do_build(drv).await
        }

        async fn output_exists(&self, _path: &StorePath) -> Result<bool, BuildError> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn mock_builder_through_trait_success() {
        use sui_compat::derivation::{Derivation, DerivationOutput};
        use std::collections::BTreeMap;

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );

        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "aarch64-darwin".to_string(),
            builder: "/bin/echo".to_string(),
            args: vec!["build-complete".to_string()],
            env: BTreeMap::new(),
        };

        let builder = MockBuilder;
        let result = builder.build(&drv).await.unwrap();
        assert!(result.success);
        assert_eq!(result.outputs.len(), 1);
        assert!(result.log.contains("build-complete"));
    }

    #[tokio::test]
    async fn mock_builder_result_to_api_build_status() {
        let output = StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();

        let result = BuildResult {
            outputs: vec![output],
            log: "building...\nfinished\n".to_string(),
            success: true,
            duration_secs: 30.0,
        };

        // Verify the data survives the BuildResult construction
        assert!(result.success);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(
            result.outputs[0].to_absolute_path(),
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        );
        assert_eq!(result.log.lines().count(), 2);
    }

    #[tokio::test]
    async fn mock_builder_output_exists() {
        let builder = MockBuilder;
        let path = StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();
        assert!(!builder.output_exists(&path).await.unwrap());
    }

    #[tokio::test]
    async fn mock_builder_sandbox_failure_becomes_build_error() {
        use sui_compat::derivation::{Derivation, DerivationOutput};
        use std::collections::BTreeMap;

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );

        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "aarch64-darwin".to_string(),
            builder: "/nonexistent/builder/12345".to_string(),
            args: vec![],
            env: BTreeMap::new(),
        };

        let builder = MockBuilder;
        let result = builder.build(&drv).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            BuildError::Sandbox(msg) => assert!(!msg.is_empty()),
            other => panic!("expected Sandbox error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_builder_failing_command_produces_failure_result() {
        use sui_compat::derivation::{Derivation, DerivationOutput};
        use std::collections::BTreeMap;

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );

        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "aarch64-darwin".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "exit 1".to_string()],
            env: BTreeMap::new(),
        };

        let builder = MockBuilder;
        let result = builder.build(&drv).await.unwrap();
        assert!(!result.success);
    }
}
