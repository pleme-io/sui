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
pub mod nar_refs;
pub mod nar_stream;
pub mod pg;
pub mod redis;
pub mod s3;
pub mod tiered;

use std::sync::Arc;

pub use index::StorageIndex;
pub use local::LocalStorage;
pub use nar_refs::{
    advertised_nar_url, advertised_url_line, is_addressable_nar_path, is_servable_narinfo,
    referrer_of, MemNarRefIndex,
    NarRefIndex, NarRefKey, NarRefScan, NAR_REF_PREFIX,
};
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
///
/// # Record verbs vs. composed verbs
///
/// The `*_record` verbs ([`put_narinfo_record`](StorageBackend::put_narinfo_record),
/// [`delete_narinfo_record`](StorageBackend::delete_narinfo_record),
/// [`delete_nar_record`](StorageBackend::delete_nar_record)) each touch **exactly
/// one key and maintain nothing**. They are what a backend implements.
///
/// [`put_narinfo`](StorageBackend::put_narinfo) and
/// [`delete`](StorageBackend::delete) are *composed* on top: they keep the
/// [`NarRefIndex`] in step and, in `delete`'s case, refuse to remove a NAR that
/// another narinfo still advertises. They are provided, so a backend cannot
/// forget to index — see [`nar_refs`] for what the two directions cost.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Retrieve narinfo text by store path hash.
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, StoreError>;

    /// Store narinfo text keyed by store path hash — **the record verb**: one
    /// key, no index maintenance.
    ///
    /// Callers want [`put_narinfo`](Self::put_narinfo), which also records the
    /// reverse edge.
    async fn put_narinfo_record(&self, hash: &str, content: &str) -> Result<(), StoreError>;

    /// Remove a narinfo record by store path hash. Idempotent; removing an
    /// absent narinfo is `Ok(())`.
    ///
    /// **The record verb**: it does not touch the NAR the narinfo advertises and
    /// does not maintain the index. Callers want [`delete`](Self::delete).
    async fn delete_narinfo_record(&self, hash: &str) -> Result<(), StoreError>;

    /// Remove one NAR blob by its relative path. Idempotent.
    ///
    /// **The record verb, and the one with teeth**: removing a NAR that a live
    /// narinfo still advertises is the stranding hazard this module exists to
    /// prevent, and *nothing here checks*. Callers want [`delete`](Self::delete);
    /// reach for this directly only after consulting
    /// [`nar_ref_index`](Self::nar_ref_index).
    async fn delete_nar_record(&self, nar_path: &str) -> Result<(), StoreError>;

    /// This backend's **narhash → store-hash reverse index**.
    ///
    /// **Required — no default.** A default would be an empty index, and an
    /// empty index does not read as "unknown", it reads as "nobody advertises
    /// this NAR" — which is precisely the answer that authorizes deleting a NAR
    /// out from under a live narinfo. Same mechanism, and the same reason, as
    /// [`nar_residency`](Self::nar_residency): the decision cannot be omitted.
    fn nar_ref_index(&self) -> &dyn NarRefIndex;

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

    /// Store narinfo text **and record the reverse edge it creates**.
    ///
    /// # Ordering, and why it is this way round
    ///
    /// The edge is recorded **before** the narinfo record is written. A crash
    /// between the two then leaves an edge with no narinfo — an over-report,
    /// which costs a NAR that could have been reclaimed. The other order leaves
    /// a narinfo with no edge, which is an under-report, and an under-report is
    /// what lets a later `delete` take the NAR this narinfo advertises. Leak
    /// over strand, every time.
    ///
    /// A narinfo whose `URL:` is not an addressable relative path is **refused**
    /// (see [`is_addressable_nar_path`]): it arrives over the wire, it is used
    /// as a key and joined onto a filesystem root, and there is no sanitizing it
    /// safely at each of those uses. Text carrying no `URL:` at all is stored
    /// as-is and indexes nothing — it advertises no NAR, so there is nothing to
    /// strand.
    ///
    /// # Errors
    ///
    /// Propagates the index write or the record write, and returns
    /// [`StoreError::NarInfo`] for an unaddressable `URL:`.
    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), StoreError> {
        match nar_refs::advertised_url_line(content) {
            Some(url) if nar_refs::is_addressable_nar_path(url) => {
                self.nar_ref_index().record(url, hash).await?;
            }
            Some(url) => {
                return Err(StoreError::NarInfo(format!(
                    "narinfo {hash} advertises an unaddressable URL: {url:?}",
                )));
            }
            None => {}
        }
        self.put_narinfo_record(hash, content).await
    }

    /// The NAR path this store path's narinfo advertises — **resolved, never
    /// guessed**.
    ///
    /// `None` means the narinfo is absent, unparseable, or advertises a URL this
    /// store will not address. All three mean the same thing to a caller: there
    /// is no NAR here it is entitled to touch.
    ///
    /// # Errors
    ///
    /// Propagates the narinfo read failure. A read failure is **not** flattened
    /// into `None`: "the tier is down" must never be mistaken for "this path has
    /// no NAR" by something about to delete.
    async fn advertised_nar(&self, hash: &str) -> Result<Option<String>, StoreError> {
        Ok(self.get_narinfo(hash).await?.as_deref().and_then(nar_refs::advertised_nar_url))
    }

    /// Delete a store path's narinfo, and its NAR **only if nothing else
    /// advertises that NAR**.
    ///
    /// # What changed, and why it is not cosmetic
    ///
    /// This used to guess: every backend best-effort-deleted
    /// `nar/{store-hash}.{xz,zst,nar}`. The NAR is keyed by *narhash*, not by
    /// store hash, so the guess normally deleted three keys that were never this
    /// path's NAR and left the real one behind. It now **resolves** the key from
    /// the narinfo's own `URL:`.
    ///
    /// Resolving alone would be a regression: two store paths with identical
    /// contents share one narhash and therefore one `URL:`, so deleting either
    /// would take the NAR the other still advertises — and a narinfo whose
    /// advertised NAR 404s is a hard Nix failure, not a cache miss. So the NAR
    /// goes only when [`nar_ref_index`](Self::nar_ref_index) reports no other
    /// referrer.
    ///
    /// # Ordering
    ///
    /// The narinfo record is removed **first**, then its edge. A crash between
    /// the two leaves a stale edge — an over-report that costs a retained NAR.
    /// The other order would leave a live narinfo with no edge, and the next
    /// `delete` of a co-referrer would strand it.
    ///
    /// # Errors
    ///
    /// Propagates the narinfo read, the record delete, or the index update.
    async fn delete(&self, hash: &str) -> Result<(), StoreError> {
        let advertised = self.advertised_nar(hash).await?;

        self.delete_narinfo_record(hash).await?;

        let Some(nar_path) = advertised else { return Ok(()) };
        self.nar_ref_index().forget(&nar_path, hash).await?;

        let others = self.nar_ref_index().referrers(&nar_path).await?;
        if others.is_empty() {
            self.delete_nar_record(&nar_path).await?;
        } else {
            tracing::debug!(
                hash = %hash,
                nar_path = %nar_path,
                referrers = others.len(),
                "delete: NAR retained — another narinfo still advertises it; removing it \
                 would 404 an advertised URL, which Nix treats as a hard failure",
            );
        }
        Ok(())
    }

    /// Rebuild every reverse edge from the narinfos this backend holds, and
    /// return the number of edges recorded.
    ///
    /// The index is maintained forward from [`put_narinfo`](Self::put_narinfo),
    /// so a store filled **before** the index existed has none — and an absent
    /// edge reads as "nobody advertises this NAR". Running this once after an
    /// upgrade closes that gap; it is idempotent, so running it again is free.
    ///
    /// O(narinfos), one narinfo read each: a maintenance verb, not something on
    /// a request path.
    ///
    /// # Errors
    ///
    /// Propagates the listing, a narinfo read, or an index write.
    async fn reindex_nar_refs(&self) -> Result<usize, StoreError> {
        let mut recorded = 0usize;
        for hash in self.list_narinfos().await? {
            if let Some(nar_path) = self.advertised_nar(&hash).await? {
                self.nar_ref_index().record(&nar_path, &hash).await?;
                recorded += 1;
            }
        }
        Ok(recorded)
    }

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

/// **The stranding gate.** Every backend a production [`BackendConfig`] can name
/// must never leave a narinfo advertising a NAR it has deleted.
///
/// A narinfo is served 200 OK with `URL: nar/…`; a client then fetches that NAR.
/// Nix treats a **missing advertised NAR** as a hard failure, not a cache miss —
/// the same outage class as 2026-07-26, where 500s from a substituter failed
/// every build on the cluster. So the property is not "delete frees bytes", it
/// is "**every narinfo that survives a delete is still servable end to end**",
/// and that is what these assert.
///
/// The per-backend equivalents for the feature-gated tiers live beside their
/// mock seams (`pg::tests`, `redis::tests`); `s3::tests` runs the same scenario
/// against an in-process object store. This module covers what a default build
/// can construct through [`build_backend`], which is the factory a deployment
/// actually goes through.
#[cfg(test)]
mod nar_ref_gate {
    use super::*;

    /// Two narinfos advertising ONE NAR — the case that makes the index
    /// necessary. A NAR serializes a store path's *contents*, not its name, so
    /// two paths with identical contents produce one narhash and one `URL:`.
    const SHARED_NAR: &str = "nar/sharednarhash.nar.xz";

    fn narinfo_for(url: &str) -> String {
        format!(
            "StorePath: /nix/store/pkg\nURL: {url}\nCompression: xz\nFileHash: sha256:aaa\n\
             FileSize: 100\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n"
        )
    }

    /// Run the whole scenario against one backend, naming it in every failure.
    async fn assert_never_strands(name: &str, backend: &dyn StorageBackend) {
        backend.put_narinfo("pathA", &narinfo_for(SHARED_NAR)).await.unwrap();
        backend.put_narinfo("pathB", &narinfo_for(SHARED_NAR)).await.unwrap();
        backend.put_nar(SHARED_NAR, b"shared contents").await.unwrap();
        // A decoy shaped like the old extension guess, which the store hash
        // would have produced. Nothing advertises it.
        backend.put_nar("nar/pathA.nar.zst", b"unrelated").await.unwrap();

        backend.delete("pathA").await.unwrap();

        // The surviving narinfo is still servable END TO END: the narinfo is
        // there AND the NAR it names is there. Checking only one of the two is
        // how a strand hides.
        let surviving = backend
            .get_narinfo("pathB")
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{name}: pathB's narinfo vanished"));
        let advertised = nar_refs::advertised_nar_url(&surviving)
            .unwrap_or_else(|| panic!("{name}: pathB advertises nothing"));
        assert!(
            backend.get_nar(&advertised).await.unwrap().is_some(),
            "{name}: STRANDED — pathB's narinfo advertises {advertised}, which is gone. \
             A client would get 200 on the narinfo and 404 on the NAR, which nix treats \
             as a hard build failure.",
        );
        assert_eq!(
            backend.nar_ref_index().referrers(SHARED_NAR).await.unwrap(),
            vec!["pathB".to_string()],
            "{name}: the index must have dropped exactly pathA's edge",
        );
        let decoy = backend.get_nar("nar/pathA.nar.zst").await.unwrap().unwrap_or_else(|| {
            panic!(
                "{name}: GUESSED — delete removed nar/pathA.nar.zst, a key built from the \
                 STORE hash that no narinfo ever advertised. A NAR is keyed by narhash; \
                 delete must resolve the advertised URL, never guess an extension.",
            )
        });
        assert_eq!(decoy, b"unrelated", "{name}: the decoy's bytes were altered");

        // With the last referrer gone the NAR is reclaimable — otherwise the
        // gate above would pass trivially by never deleting anything.
        backend.delete("pathB").await.unwrap();
        assert!(
            backend.get_nar(SHARED_NAR).await.unwrap().is_none(),
            "{name}: nothing advertises the NAR any more; it must be reclaimed",
        );
    }

    #[tokio::test]
    async fn every_production_backend_pairs_its_nar_with_its_narinfo() {
        let dir = tempfile::tempdir().unwrap();

        let local = build_backend(&BackendConfig::Local { path: dir.path().join("solo") })
            .await
            .unwrap();
        assert_never_strands("LocalStorage", local.as_ref()).await;

        let tiered = build_backend(&BackendConfig::Tiered {
            l1: Box::new(BackendConfig::Local { path: dir.path().join("l1") }),
            l2: Box::new(BackendConfig::Local { path: dir.path().join("l2") }),
            l3: Box::new(BackendConfig::Local { path: dir.path().join("l3") }),
            write_policy: WritePolicy::WriteThrough,
        })
        .await
        .unwrap();
        assert_never_strands("TieredBackend", tiered.as_ref()).await;
    }

    /// The **migration gap**, stated as a test rather than a doc line.
    ///
    /// A store written by a pre-index binary has narinfos and no edges, and an
    /// absent edge reads as "nobody advertises this NAR". Deleting one of two
    /// co-referring paths therefore strands the other — until
    /// [`reindex_nar_refs`](StorageBackend::reindex_nar_refs) has run once. Both
    /// halves are asserted, so the gap cannot be quietly forgotten *or* quietly
    /// claimed to be closed.
    #[tokio::test]
    async fn an_unindexed_store_can_strand_until_reindexed() {
        let dir = tempfile::tempdir().unwrap();

        // The gap: narinfos written the pre-index way, via the record verb.
        let stale = LocalStorage::new(dir.path().join("stale"));
        stale.put_narinfo_record("pathA", &narinfo_for(SHARED_NAR)).await.unwrap();
        stale.put_narinfo_record("pathB", &narinfo_for(SHARED_NAR)).await.unwrap();
        stale.put_nar(SHARED_NAR, b"shared").await.unwrap();
        stale.delete("pathA").await.unwrap();
        assert!(
            stale.get_narinfo("pathB").await.unwrap().is_some(),
            "pathB's narinfo is still there…",
        );
        assert!(
            stale.get_nar(SHARED_NAR).await.unwrap().is_none(),
            "…and its NAR is gone: this IS the strand, and it is what an un-reindexed \
             upgrade looks like",
        );

        // The close: same fixture, reindexed before the delete.
        let healed = LocalStorage::new(dir.path().join("healed"));
        healed.put_narinfo_record("pathA", &narinfo_for(SHARED_NAR)).await.unwrap();
        healed.put_narinfo_record("pathB", &narinfo_for(SHARED_NAR)).await.unwrap();
        healed.put_nar(SHARED_NAR, b"shared").await.unwrap();
        assert_eq!(healed.reindex_nar_refs().await.unwrap(), 2);
        healed.delete("pathA").await.unwrap();
        assert!(
            healed.get_nar(SHARED_NAR).await.unwrap().is_some(),
            "after a reindex the co-referrer is visible and the NAR is retained",
        );
    }
}
