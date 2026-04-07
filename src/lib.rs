//! Sui — Rust-native Nix replacement with API-first design.
//!
//! This is the root crate of the Sui workspace. It provides the CLI binary
//! (`sui`) and a triple-stack API server (REST + GraphQL + gRPC) that delegates
//! to the domain crates (`sui-compat`, `sui-store`, `sui-eval`, `sui-build`,
//! `sui-daemon`, `sui-orchestrate`).

pub mod api;

/// Default path to the Nix SQLite database.
pub const NIX_DB_PATH: &str = "/nix/var/nix/db/db.sqlite";

/// Unified error type for CLI operations.
///
/// Replaces ad-hoc `anyhow::anyhow!("…")` / `anyhow::bail!("…")` calls with
/// typed variants that preserve the upstream error chain.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The Nix store database could not be opened.
    #[error("failed to open Nix store at {path}: {source}")]
    StoreOpen {
        path: &'static str,
        source: sui_store::StoreError,
    },

    /// A store query failed.
    #[error(transparent)]
    Store(#[from] sui_store::StoreError),

    /// A store path string could not be parsed.
    #[error(transparent)]
    StorePath(#[from] sui_compat::store_path::StorePathError),

    /// Nix expression evaluation failed.
    #[error("evaluation error: {0}")]
    Eval(#[from] sui_eval::EvalError),

    /// The user did not provide a required argument.
    #[error("{0}")]
    MissingArgument(String),

    /// A store path was not found.
    #[error("path '{0}' is not valid")]
    PathNotValid(String),

    /// System orchestration failed (rebuild / rollback / detection).
    #[error("{operation} failed: {message}")]
    Orchestrate { operation: &'static str, message: String },

    /// Fleet deployment failed.
    #[error("deploy failed: {0}")]
    Deploy(String),

    /// JSON serialization error.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// API server error (axum / network).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
