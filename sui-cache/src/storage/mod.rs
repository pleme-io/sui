//! Storage backend trait and implementations.
//!
//! The `StorageBackend` trait abstracts over where narinfo metadata and
//! compressed NAR blobs are persisted. Two implementations are provided:
//!
//! - [`LocalStorage`] — local filesystem (default)
//! - S3-compatible object storage (stub for now)

pub mod local;
pub mod s3;

pub use local::LocalStorage;

use async_trait::async_trait;

use crate::CacheError;

/// Abstraction over binary cache storage.
///
/// Narinfo files are keyed by the 32-character store path hash.
/// NAR blobs are keyed by their relative URL path (e.g. `nar/<hash>.nar.xz`).
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Retrieve narinfo text by store path hash.
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, CacheError>;

    /// Store narinfo text keyed by store path hash.
    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), CacheError>;

    /// Retrieve a NAR blob by its relative path.
    async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, CacheError>;

    /// Store a NAR blob at the given relative path.
    async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), CacheError>;

    /// Delete a store path's narinfo and associated NAR blob.
    async fn delete(&self, hash: &str) -> Result<(), CacheError>;

    /// List all stored narinfo hashes.
    async fn list_narinfos(&self) -> Result<Vec<String>, CacheError>;
}
