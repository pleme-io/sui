//! Content-addressed store backends for sui.
//!
//! This crate owns the [`StorageBackend`] trait and every concrete
//! implementation (Local / S3 / Redis / Postgres / Tiered), plus the
//! [`BackendConfig`] config-select enum and the [`build_backend`] factory.
//! Both `sui-cache` (the Nix binary-cache server) and `sui-registry` (the OCI
//! registry `porto`) depend on this crate; neither depends on the other.
//!
//! # Feature flags
//!
//! - **`redis-client`** — enables the production [`RedisConnectionManager`]
//!   transport. Without it the Redis arm of [`build_backend`] returns a typed
//!   [`StoreError::NotImplemented`].
//! - **`postgres`** — enables the production [`SqlxPgCacheConn`] transport.
//!   Without it the Postgres arm returns [`StoreError::NotImplemented`].

pub mod config;
pub mod env_expand;
pub mod storage;

pub use config::BackendConfig;
pub use env_expand::{expand_env_vars, ExpandEnvError};
pub use storage::{
    build_backend, LocalStorage, PgCacheConn, PgStorageBackend, PgTable, RedisBackend, RedisConn,
    S3Storage, StorageBackend, StorageIndex, TieredBackend, TieredTier, WritePolicy,
    TIERED_BACKEND_TIER,
};

#[cfg(feature = "redis-client")]
pub use storage::RedisConnectionManager;

#[cfg(feature = "postgres")]
pub use storage::SqlxPgCacheConn;

/// Error type for content-addressed store operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// An I/O operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A store path was not found on the local filesystem.
    #[error("path not found: {0}")]
    PathNotFound(String),

    /// A signing or verification operation failed.
    #[error("signing error: {0}")]
    Signing(String),

    /// A feature is not yet implemented (missing Cargo feature flag).
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    /// A narinfo could not be parsed or was invalid UTF-8.
    #[error("narinfo error: {0}")]
    NarInfo(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_error_display_path_not_found() {
        let e = StoreError::PathNotFound("/nix/store/abc".to_string());
        assert!(format!("{e}").contains("/nix/store/abc"));
    }

    #[test]
    fn store_error_display_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e = StoreError::Io(io_err);
        assert!(format!("{e}").contains("missing"));
    }

    #[test]
    fn store_error_display_signing() {
        let e = StoreError::Signing("bad key".to_string());
        assert!(format!("{e}").contains("bad key"));
    }

    #[test]
    fn store_error_display_not_implemented() {
        let e = StoreError::NotImplemented("redis L1");
        assert!(format!("{e}").contains("redis L1"));
    }

    #[test]
    fn store_error_display_narinfo() {
        let e = StoreError::NarInfo("parse failed".to_string());
        assert!(format!("{e}").contains("parse failed"));
    }
}
