//! Built-in binary cache server and push pipeline for sui.
//!
//! Replaces Attic, Cachix, and nix-serve with a single integrated component.
//! Implements the standard Nix binary cache HTTP protocol (narinfo + NAR).
//!
//! # Architecture
//!
//! - [`storage`] — pluggable storage backends (local filesystem, S3 stub)
//! - [`server`] — axum HTTP server implementing the cache protocol
//! - [`signing`] — ed25519 key management and narinfo signing
//! - [`push`] — pipeline to push store paths to the cache
//! - [`gc`] — garbage collection of unreferenced cache entries
//! - [`config`] — cache configuration types

pub mod config;
pub mod gc;
pub mod push;
pub mod server;
pub mod signing;
pub mod storage;

pub use config::{BackendConfig, CacheConfig};
pub use gc::GcResult;
pub use push::PushResult;
pub use server::{build_router, serve, AppState};
pub use signing::{verify_narinfo_signature, CacheSigner};
pub use storage::{LocalStorage, StorageBackend};

/// Errors from cache operations.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// An I/O operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A store path was not found on the local filesystem.
    #[error("path not found: {0}")]
    PathNotFound(String),

    /// A signing or verification operation failed.
    #[error("signing error: {0}")]
    Signing(String),

    /// A feature is not yet implemented.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    /// A narinfo could not be parsed.
    #[error("narinfo error: {0}")]
    NarInfo(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_error_display() {
        let e = CacheError::PathNotFound("/nix/store/abc".to_string());
        assert!(format!("{e}").contains("/nix/store/abc"));
    }

    #[test]
    fn cache_error_io_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e = CacheError::Io(io_err);
        assert!(format!("{e}").contains("missing"));
    }

    #[test]
    fn cache_error_signing_display() {
        let e = CacheError::Signing("bad key".to_string());
        assert!(format!("{e}").contains("bad key"));
    }

    #[test]
    fn cache_error_not_implemented_display() {
        let e = CacheError::NotImplemented("S3");
        assert!(format!("{e}").contains("S3"));
    }

    #[test]
    fn cache_error_narinfo_display() {
        let e = CacheError::NarInfo("parse failed".to_string());
        assert!(format!("{e}").contains("parse failed"));
    }
}
