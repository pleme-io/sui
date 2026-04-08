//! Sui — Rust-native Nix replacement with API-first design.
//!
//! This is the root crate of the Sui workspace. It provides the CLI binary
//! (`sui`) and a triple-stack API server (REST + GraphQL + gRPC) that delegates
//! to the domain crates (`sui-compat`, `sui-store`, `sui-eval`, `sui-build`,
//! `sui-daemon`, `sui-orchestrate`).

pub mod api;

/// Default path to the Nix `SQLite` database.
pub const NIX_DB_PATH: &str = "/nix/var/nix/db/db.sqlite";

/// Parse a store path string, trying it as-is first, then with `/nix/store/` prefix.
///
/// This is the shared logic for both CLI `store path-info` and REST `get_path_info`,
/// which accept either a basename like `abc123-hello` or a full path like
/// `/nix/store/abc123-hello`.
///
/// # Errors
///
/// Returns `StorePathError` if the input cannot be parsed as a valid store path.
pub fn parse_store_path(input: &str) -> Result<sui_compat::store_path::StorePath, sui_compat::store_path::StorePathError> {
    sui_compat::store_path::StorePath::from_absolute_path(input)
        .or_else(|_| sui_compat::store_path::StorePath::from_absolute_path(&format!("/nix/store/{input}")))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_error_missing_argument_display() {
        let err = CliError::MissingArgument("no expression provided".into());
        assert_eq!(err.to_string(), "no expression provided");
    }

    #[test]
    fn cli_error_path_not_valid_display() {
        let err = CliError::PathNotValid("/nix/store/abc-hello".into());
        assert_eq!(err.to_string(), "path '/nix/store/abc-hello' is not valid");
    }

    #[test]
    fn cli_error_orchestrate_display() {
        let err = CliError::Orchestrate {
            operation: "rebuild",
            message: "something went wrong".into(),
        };
        assert_eq!(err.to_string(), "rebuild failed: something went wrong");
    }

    #[test]
    fn cli_error_deploy_display() {
        let err = CliError::Deploy("connection refused".into());
        assert_eq!(err.to_string(), "deploy failed: connection refused");
    }

    #[test]
    fn cli_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: CliError = io_err.into();
        assert!(matches!(err, CliError::Io(_)));
    }

    #[test]
    fn cli_error_from_json() {
        let json_err = serde_json::from_str::<String>("not json").unwrap_err();
        let err: CliError = json_err.into();
        assert!(matches!(err, CliError::Json(_)));
    }

    #[test]
    fn cli_error_is_debug() {
        let err = CliError::MissingArgument("test".into());
        let debug = format!("{err:?}");
        assert!(debug.contains("MissingArgument"));
    }

    #[test]
    fn parse_store_path_absolute() {
        let sp = parse_store_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();
        assert_eq!(
            sp.to_absolute_path(),
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1"
        );
    }

    #[test]
    fn parse_store_path_basename() {
        let sp = parse_store_path("sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1").unwrap();
        assert_eq!(
            sp.to_absolute_path(),
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1"
        );
    }

    #[test]
    fn parse_store_path_invalid() {
        assert!(parse_store_path("not-a-valid-store-path").is_err());
    }
}
