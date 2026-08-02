//! Storage backend trait and implementations.
//!
//! The `StorageBackend` trait abstracts over where narinfo metadata and
//! compressed NAR blobs are persisted. Implementations provided:
//!
//! - [`LocalStorage`] — local filesystem (default)
//! - [`S3Storage`] — S3-compatible object storage (AWS, MinIO, R2, RustFS)
//! - [`RedisBackend`] — Redis L1 hot cache (sub-ms, TTL/eviction-aware)
//! - [`PgStorageBackend`] — Postgres L2 durable cache tier (shared,
//!   authoritative)
//! - [`TieredBackend`] — L1→L2→L3 read-through/write-through resolver
//! - [`StorageIndex`] — redb ephemeral metadata index (accelerates S3 lookups)
//!
//! [`build_backend`] is the typed config-select factory: it dispatches a
//! [`BackendConfig`](crate::config::BackendConfig) to its concrete backend
//! (recursing for the tiered arm), so a deployment picks `{disk | s3 | redis |
//! pg | tiered}` by configuration — never a silent hard-coded constructor.

pub mod index;
pub mod local;
pub mod nar_stream;
pub mod pg;
pub mod redis;
pub mod s3;
pub mod tiered;

use std::sync::Arc;

pub use index::StorageIndex;
pub use local::LocalStorage;
pub use nar_stream::{
    bytes_stream, collect_nar, empty_stream, file_stream, spool_or_buffer, whole_value_stream,
    BytesNarSource, FileNarSource, NarSource, NarStream, SpooledNarSource,
    DEFAULT_INGEST_MEMORY_CAP, NAR_CHUNK_BYTES,
};
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
use crate::StoreError;

/// What a backend's NAR path costs in resident memory.
///
/// **Every [`StorageBackend`] implementor must state this — it has no default.**
/// That is the mechanism, not decoration: the streaming verbs
/// ([`get_nar_stream`](StorageBackend::get_nar_stream) /
/// [`put_nar_stream`](StorageBackend::put_nar_stream)) *do* carry a buffering
/// fallback so a test double stays a few lines, and without a required
/// declaration a new production backend could silently inherit it and
/// reintroduce the OOM. Making the declaration mandatory means adding a backend
/// without deciding is a **compile error**, and shipping a production backend
/// that declares [`WholeValue`](NarResidency::WholeValue) is caught by
/// [`every_production_backend_bounds_its_nar_path`] in CI.
///
/// Tier-honest: this is *parse-time-rejected* (you cannot omit the decision) plus
/// *CI-gate-caught* (you cannot ship the wrong one from the factory). It is
/// **not** truly-unrepresentable — the buffering code path still exists and a
/// hand-constructed backend may use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarResidency {
    /// **O(chunk).** The backend never holds more than [`NAR_CHUNK_BYTES`] of a
    /// NAR, whatever the NAR's size.
    Streaming,
    /// **O(min(nar, cap)).** Bounded by a configured cap; a NAR past the cap is
    /// *refused* ([`StoreError::TooLarge`]), never buffered. The hot-tier shape:
    /// a cap is a real bound because the durable tiers below still take the
    /// write.
    Capped(usize),
    /// **O(nar).** The whole NAR is materialized. Legal only for in-memory test
    /// doubles and small-value stores — never for a tier that serves real
    /// builds.
    WholeValue,
}

impl NarResidency {
    /// Whether the peak is bounded independently of NAR size.
    #[must_use]
    pub const fn is_bounded(self) -> bool {
        !matches!(self, NarResidency::WholeValue)
    }

    /// The **weaker** of two residencies — the honest answer for a composite
    /// backend, whose real cost is its worst tier's.
    ///
    /// Order: `Streaming` (best) < `Capped` < `WholeValue` (worst); two
    /// `Capped`s compose to the larger cap, because either one may be the one
    /// that holds the bytes.
    #[must_use]
    pub fn weaker(self, other: Self) -> Self {
        match (self, other) {
            (NarResidency::WholeValue, _) | (_, NarResidency::WholeValue) => {
                NarResidency::WholeValue
            }
            (NarResidency::Capped(a), NarResidency::Capped(b)) => NarResidency::Capped(a.max(b)),
            (NarResidency::Capped(a), NarResidency::Streaming)
            | (NarResidency::Streaming, NarResidency::Capped(a)) => NarResidency::Capped(a),
            (NarResidency::Streaming, NarResidency::Streaming) => NarResidency::Streaming,
        }
    }
}

/// Abstraction over binary cache storage.
///
/// Narinfo files are keyed by the 32-character store path hash.
/// NAR blobs are keyed by their relative URL path (e.g. `nar/<hash>.nar.xz`).
///
/// # NAR verbs come in two shapes; prefer the streaming pair
///
/// [`get_nar`](StorageBackend::get_nar) / [`put_nar`](StorageBackend::put_nar)
/// hand whole `Vec<u8>` / `&[u8]` values across the boundary and are therefore
/// **O(nar) resident by signature**. They remain for callers that genuinely have
/// or want the whole thing (tests, small values, the GC).
///
/// [`get_nar_stream`](StorageBackend::get_nar_stream) /
/// [`put_nar_stream`](StorageBackend::put_nar_stream) move the same content in
/// [`NAR_CHUNK_BYTES`] chunks and are what the HTTP server and every tier-to-tier
/// transfer use. `narinfo` is ~728 bytes and deliberately has no streaming pair.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Retrieve narinfo text by store path hash.
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, StoreError>;

    /// Store narinfo text keyed by store path hash.
    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), StoreError>;

    /// Retrieve a NAR blob by its relative path.
    ///
    /// **O(nar) resident.** Prefer [`get_nar_stream`](Self::get_nar_stream) on
    /// any path that serves real build artifacts.
    async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError>;

    /// Store a NAR blob at the given relative path.
    ///
    /// **O(nar) resident.** Prefer [`put_nar_stream`](Self::put_nar_stream) on
    /// any path that ingests real build artifacts.
    async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), StoreError>;

    /// Declare what this backend's NAR path costs in resident memory.
    ///
    /// **Required — no default.** See [`NarResidency`] for why.
    fn nar_residency(&self) -> NarResidency;

    /// Retrieve a NAR blob as a bounded-chunk stream.
    ///
    /// The default materializes via [`get_nar`](Self::get_nar) — correct, and
    /// **O(nar) resident**. A backend that declares
    /// [`NarResidency::Streaming`] must override this.
    ///
    /// # Errors
    ///
    /// Propagates the backend's read failure. `Ok(None)` is a clean miss.
    async fn get_nar_stream(&self, path: &str) -> Result<Option<NarStream>, StoreError> {
        Ok(self.get_nar(path).await?.map(nar_stream::whole_value_stream))
    }

    /// Store a NAR blob from a re-openable bounded-chunk source.
    ///
    /// The default drains the source into one buffer and calls
    /// [`put_nar`](Self::put_nar) — correct, and **O(nar) resident**. A backend
    /// that declares [`NarResidency::Streaming`] must override this.
    ///
    /// # Errors
    ///
    /// Propagates the source's read failure or the backend's write failure.
    async fn put_nar_stream(&self, path: &str, src: &dyn NarSource) -> Result<(), StoreError> {
        let data = nar_stream::collect_nar(src.open().await?, None).await?;
        self.put_nar(path, &data).await
    }

    /// Delete a store path's narinfo and associated NAR blob.
    async fn delete(&self, hash: &str) -> Result<(), StoreError>;

    /// List all stored narinfo hashes.
    async fn list_narinfos(&self) -> Result<Vec<String>, StoreError>;

    /// Clear EVERY narinfo and NAR blob from this backend. Returns the number
    /// of narinfos removed.
    ///
    /// The default lists every narinfo and best-effort `delete`s it (narinfo-only
    /// clear; NAR blobs keyed by *narhash* are not reached). Concrete durable
    /// tiers override with a real truncation that reclaims NAR bytes.
    async fn wipe_all(&self) -> Result<usize, StoreError> {
        let hashes = self.list_narinfos().await?;
        let n = hashes.len();
        for hash in hashes {
            self.delete(&hash).await?;
        }
        Ok(n)
    }
}

/// Config-select factory: build the concrete [`StorageBackend`] a
/// [`BackendConfig`] names.
///
/// This is **typed dispatch, not stringly** — a new backend kind is a
/// non-exhaustive-`match` compile error, and the [`Tiered`](BackendConfig::Tiered)
/// arm recurses, composing each sub-backend into a [`TieredBackend`]. The result
/// is an `Arc<dyn StorageBackend>` ready for injection into any consumer.
///
/// The `Redis` and `Pg` arms require their production transports; without the
/// corresponding Cargo feature (`redis-client` / `postgres`) they return a typed
/// [`StoreError::NotImplemented`] rather than silently falling back to disk.
///
/// Returns a boxed future because the `Tiered` arm is recursive.
///
/// # Errors
///
/// Propagates any backend construction failure, or [`StoreError::NotImplemented`]
/// when a config selects a backend whose feature is not compiled in.
pub fn build_backend(
    config: &BackendConfig,
) -> BoxFuture<'_, Result<Arc<dyn StorageBackend>, StoreError>> {
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
async fn build_redis(
    url: &str,
    ttl_secs: Option<u64>,
) -> Result<Arc<dyn StorageBackend>, StoreError> {
    let backend = match ttl_secs {
        Some(t) => RedisBackend::connect_with_ttl(url, t).await?,
        None => RedisBackend::connect(url).await?,
    };
    Ok(Arc::new(backend) as Arc<dyn StorageBackend>)
}

#[cfg(not(feature = "redis-client"))]
async fn build_redis(
    _url: &str,
    _ttl_secs: Option<u64>,
) -> Result<Arc<dyn StorageBackend>, StoreError> {
    Err(StoreError::NotImplemented(
        "redis L1 backend requires building sui-castore with --features redis-client",
    ))
}

#[cfg(feature = "postgres")]
async fn build_pg(url: &str, max_conns: u32) -> Result<Arc<dyn StorageBackend>, StoreError> {
    let backend = PgStorageBackend::connect(url, max_conns).await?;
    Ok(Arc::new(backend) as Arc<dyn StorageBackend>)
}

#[cfg(not(feature = "postgres"))]
async fn build_pg(_url: &str, _max_conns: u32) -> Result<Arc<dyn StorageBackend>, StoreError> {
    Err(StoreError::NotImplemented(
        "postgres L2 backend requires building sui-castore with --features postgres",
    ))
}

#[cfg(test)]
mod residency_gate {
    use super::*;

    /// **The gate.** Every backend a production [`BackendConfig`] can name must
    /// bound its NAR path. A backend that regresses to
    /// [`NarResidency::WholeValue`] fails here.
    ///
    /// The tripwire that keeps this honest as the fleet grows is
    /// [`build_backend`]'s exhaustive `match`: a new [`BackendConfig`] arm is a
    /// compile error there, and the author lands here next.
    ///
    /// Feature-gated arms (`Redis`, `Pg`) are exercised in their own modules
    /// against their mock seams; this covers what a default build can construct.
    #[tokio::test]
    async fn every_production_backend_bounds_its_nar_path() {
        let dir = tempfile::tempdir().unwrap();

        let local = build_backend(&BackendConfig::Local { path: dir.path().to_path_buf() })
            .await
            .unwrap();
        assert_eq!(
            local.nar_residency(),
            NarResidency::Streaming,
            "the local/L3 tier must stream — it is the durable object tier",
        );

        let s3 = build_backend(&BackendConfig::S3 {
            bucket: "b".to_string(),
            region: "us-east-1".to_string(),
            endpoint: Some("http://127.0.0.1:9".to_string()),
        })
        .await
        .unwrap();
        assert_eq!(s3.nar_residency(), NarResidency::Streaming, "S3 must multipart-stream");

        // The composite: three streaming tiers compose to streaming.
        let tiered = build_backend(&BackendConfig::Tiered {
            l1: Box::new(BackendConfig::Local { path: dir.path().join("l1") }),
            l2: Box::new(BackendConfig::Local { path: dir.path().join("l2") }),
            l3: Box::new(BackendConfig::Local { path: dir.path().join("l3") }),
            write_policy: WritePolicy::WriteThrough,
        })
        .await
        .unwrap();
        assert_eq!(tiered.nar_residency(), NarResidency::Streaming);
        assert!(tiered.nar_residency().is_bounded());
    }

    #[test]
    fn residency_composes_to_the_weaker_side() {
        use NarResidency::{Capped, Streaming, WholeValue};
        assert_eq!(Streaming.weaker(Streaming), Streaming);
        assert_eq!(Streaming.weaker(Capped(8)), Capped(8));
        assert_eq!(Capped(8).weaker(Capped(64)), Capped(64), "the larger cap governs");
        assert_eq!(Capped(8).weaker(WholeValue), WholeValue);
        assert_eq!(WholeValue.weaker(Streaming), WholeValue);
    }

    #[test]
    fn only_whole_value_is_unbounded() {
        assert!(NarResidency::Streaming.is_bounded());
        assert!(NarResidency::Capped(1).is_bounded());
        assert!(!NarResidency::WholeValue.is_bounded());
    }
}
