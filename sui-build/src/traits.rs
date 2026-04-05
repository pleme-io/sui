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
