//! Storage backend trait and implementations.
//!
//! The `StorageBackend` trait abstracts over where narinfo metadata and
//! compressed NAR blobs are persisted. Three implementations are provided:
//!
//! - [`LocalStorage`] — local filesystem (default)
//! - [`S3Storage`] — S3-compatible object storage (AWS, MinIO, R2, RustFS)
//! - [`RedisBackend`] — Redis L1 hot cache (sub-ms, TTL/eviction-aware)
//! - [`PgStorageBackend`] — Postgres L2 durable cache tier (shared, authoritative)
//! - [`TieredBackend`] — L1→L2→L3 read-through/write-through resolver
//! - [`StorageIndex`] — redb ephemeral metadata index (accelerates S3 lookups)
//!
//! [`build_backend`] is the typed config-select factory: it dispatches a
//! [`BackendConfig`](crate::config::BackendConfig) to its concrete backend
//! (recursing for the tiered arm), so a deployment picks `{disk | s3 | redis | pg
//! | tiered}` by configuration — never a silent hard-coded constructor.

pub mod index;
pub mod local;
pub mod pg;
pub mod redis;
pub mod s3;
pub mod tiered;

use std::sync::Arc;

pub use index::StorageIndex;
pub use local::LocalStorage;
pub use pg::{PgCacheConn, PgStorageBackend, PgTable};
pub use redis::{RedisBackend, RedisConn};
pub use s3::S3Storage;
pub use tiered::{TieredBackend, TieredTier, WritePolicy, TIERED_BACKEND_TIER};

#[cfg(feature = "redis-client")]
pub use redis::RedisConnectionManager;

#[cfg(feature = "postgres")]
pub use pg::SqlxPgCacheConn;

use async_trait::async_trait;
use futures::future::BoxFuture;

use crate::config::BackendConfig;
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

/// Config-select factory: build the concrete [`StorageBackend`] a
/// [`BackendConfig`] names.
///
/// This is **typed dispatch, not stringly** — a new backend kind is a
/// non-exhaustive-`match` compile error, and the [`Tiered`](BackendConfig::Tiered)
/// arm recurses, composing each sub-backend into a [`TieredBackend`]. The result
/// is an `Arc<dyn StorageBackend>` that drops straight into
/// `AppState.storage` — the server never changes shape whether the backend is one
/// tier or three.
///
/// The `Redis` and `Pg` arms require their production transports; without the
/// corresponding Cargo feature (`redis-client` / `postgres`) they return a typed
/// [`CacheError::NotImplemented`] rather than silently falling back to disk — the
/// never-silent-hardcode rule.
///
/// Returns a boxed future because the `Tiered` arm is recursive.
///
/// # Errors
///
/// Propagates any backend construction failure, or [`CacheError::NotImplemented`]
/// when a config selects a backend whose feature is not compiled in.
pub fn build_backend(config: &BackendConfig) -> BoxFuture<'_, Result<Arc<dyn StorageBackend>, CacheError>> {
    Box::pin(async move {
        match config {
            BackendConfig::Local { path } => {
                Ok(Arc::new(LocalStorage::new(path.clone())) as Arc<dyn StorageBackend>)
            }
            BackendConfig::S3 { bucket, region, endpoint } => {
                let s3 = S3Storage::new(bucket.clone(), region.clone(), endpoint.clone())?;
                Ok(Arc::new(s3) as Arc<dyn StorageBackend>)
            }
            BackendConfig::Redis { url, ttl_secs } => build_redis(url, *ttl_secs).await,
            BackendConfig::Pg { url, max_conns } => build_pg(url, *max_conns).await,
            BackendConfig::Tiered { l1, l2, l3, write_policy } => {
                let l1 = build_backend(l1).await?;
                let l2 = build_backend(l2).await?;
                let l3 = build_backend(l3).await?;
                Ok(Arc::new(TieredBackend::with_write_policy(l1, l2, l3, *write_policy))
                    as Arc<dyn StorageBackend>)
            }
        }
    })
}

#[cfg(feature = "redis-client")]
async fn build_redis(url: &str, ttl_secs: Option<u64>) -> Result<Arc<dyn StorageBackend>, CacheError> {
    let backend = match ttl_secs {
        Some(t) => RedisBackend::connect_with_ttl(url, t).await?,
        None => RedisBackend::connect(url).await?,
    };
    Ok(Arc::new(backend) as Arc<dyn StorageBackend>)
}

#[cfg(not(feature = "redis-client"))]
async fn build_redis(_url: &str, _ttl_secs: Option<u64>) -> Result<Arc<dyn StorageBackend>, CacheError> {
    Err(CacheError::NotImplemented(
        "redis L1 backend requires building sui-cache with --features redis-client",
    ))
}

#[cfg(feature = "postgres")]
async fn build_pg(url: &str, max_conns: u32) -> Result<Arc<dyn StorageBackend>, CacheError> {
    let backend = PgStorageBackend::connect(url, max_conns).await?;
    Ok(Arc::new(backend) as Arc<dyn StorageBackend>)
}

#[cfg(not(feature = "postgres"))]
async fn build_pg(_url: &str, _max_conns: u32) -> Result<Arc<dyn StorageBackend>, CacheError> {
    Err(CacheError::NotImplemented(
        "postgres L2 backend requires building sui-cache with --features postgres",
    ))
}
