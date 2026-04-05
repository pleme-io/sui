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
}
