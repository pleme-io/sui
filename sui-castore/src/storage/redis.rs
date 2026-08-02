//! Redis-backed **L1 hot cache** `StorageBackend`.
//!
//! This is the sub-millisecond top tier of the tiered super-cache resolver
//! (`Redis L1 → Postgres L2 → object L3`). It maps a **content-addressed key**
//! (the 32-char store-path hash for narinfo, or the relative NAR URL — which is
//! itself content-derived; when the daemon addresses graph blobs the key is
//! `GraphHash::display_short()`) to its stored value.
//!
//! # It is a cache, not a source of truth
//!
//! A key may vanish under Redis `maxmemory` LRU eviction at any moment, and
//! [`RedisBackend::list_narinfos`] therefore returns only the currently-resident
//! hot subset — *never* an authoritative listing. Durability/correctness comes
//! from the durable tiers below it in a `TieredBackend`; a hot-only write that a
//! pod roll loses must always be re-derivable from L2/L3. Because the key is
//! content-derived, an L1 miss satisfied by a lower tier returns the same bytes
//! for the same key — read-through transparency.
//!
//! # TTL / eviction awareness
//!
//! Writes are optionally stamped with a per-write TTL ([`RedisBackend::with_ttl`]);
//! with no TTL, entries rely on the Redis `maxmemory` band's LRU policy (the
//! super-cache controller derives `redis.maxmemory_mib` from the memory band).
//! Either way the backend treats a missing key as a plain cache miss (`Ok(None)`).
//!
//! # The client seam (Environment / testability contract)
//!
//! [`RedisBackend`] is generic over [`RedisConn`] — the minimal async redis
//! verb surface it needs. Unit tests inject an in-memory mock; production injects
//! [`RedisConnectionManager`] (a multiplexed, auto-reconnecting
//! `redis::aio::ConnectionManager`, behind the `redis-client` feature). The pure
//! L1 semantics are proven against the mock with **no live Redis required**.

use async_trait::async_trait;

use super::nar_refs::{referrer_of, NarRefIndex, NarRefKey, NarRefScan};
use super::nar_stream::{self, NarSource};
use super::{NarResidency, StorageBackend};
use crate::StoreError;

/// Key namespace for narinfo strings, so they never collide with NAR blobs in a
/// single Redis keyspace.
const NARINFO_PREFIX: &str = "sui:narinfo:";
/// Key namespace for NAR blobs.
const NAR_PREFIX: &str = "sui:nar:";
/// Key namespace this backend puts in front of the canonical reverse-edge key
/// ([`NarRefKey`]), so an edge lands at `sui:nar-refs/<nar path>/<store hash>`.
///
/// The canonical form is reused verbatim rather than re-encoded into Redis's
/// `a:b:c` house style, so the four key-value tiers agree on one edge encoding.
/// `sui:nar-refs/` is disjoint from `sui:nar:` — an edge is never swept by a NAR
/// scan, or vice versa.
const NAR_REF_NAMESPACE: &str = "sui:";

/// Default per-value byte cap for the hot tier.
///
/// Redis has no streaming `SET`: a value is one contiguous buffer on both sides
/// of the wire, so this tier cannot be made O(chunk) — it can only be made
/// **bounded**. 64 MiB is comfortably above a typical NAR and far below the
/// point where a handful of concurrent warms matters against a 6 GiB pod.
///
/// Refusing is correct, not a degradation: L1 is best-effort by contract, the
/// durable tiers below it stream the same content without a cap, and a
/// [`TieredBackend`](super::TieredBackend) discards this tier's write result
/// entirely. A refused warm cannot fail a build.
pub const DEFAULT_REDIS_MAX_VALUE_BYTES: usize = 64 * 1024 * 1024;

/// The minimal async redis verb surface [`RedisBackend`] depends on.
///
/// This is the injectable **Environment seam**: a real implementation
/// ([`RedisConnectionManager`], `redis-client` feature) talks to a live Redis;
/// tests substitute an in-memory mock. Keeping the surface this small means the
/// L1 read-through / write-through / eviction semantics are all proven against a
/// mock, and the only unmocked code is the thin verb translation.
#[async_trait]
pub trait RedisConn: Send + Sync {
    /// `GET key` — raw bytes, or `Ok(None)` on a miss / evicted key.
    async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError>;

    /// `SET key value [EX ttl_secs]` — store raw bytes, optionally with an
    /// expiry. `ttl_secs == None` means no explicit expiry (LRU-evicted by the
    /// `maxmemory` policy).
    async fn set_bytes(&self, key: &str, value: &[u8], ttl_secs: Option<u64>) -> Result<(), StoreError>;

    /// `DEL key` — idempotent; deleting an absent key is `Ok(())`.
    async fn del(&self, key: &str) -> Result<(), StoreError>;

    /// Non-blocking `SCAN MATCH prefix*` — every key currently resident under
    /// `prefix`. Partial by nature (a cache), and must use `SCAN`, never the
    /// O(N) blocking `KEYS`.
    async fn keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>, StoreError>;
}

/// L1 hot cache: content-addressed key → value, sub-ms hits, TTL/eviction-aware.
///
/// Generic over the [`RedisConn`] seam so it is fully testable against a mock.
pub struct RedisBackend<C: RedisConn> {
    conn: C,
    /// Optional TTL (seconds) applied to every write; `None` => rely on the
    /// `maxmemory` LRU policy.
    ttl_secs: Option<u64>,
    /// Per-value byte cap. A NAR larger than this is refused, never buffered.
    max_value_bytes: usize,
}

impl<C: RedisConn> RedisBackend<C> {
    /// Wrap a [`RedisConn`] with no per-write TTL (entries are LRU-evicted by
    /// the `maxmemory` band).
    pub fn new(conn: C) -> Self {
        Self {
            conn,
            ttl_secs: None,
            max_value_bytes: DEFAULT_REDIS_MAX_VALUE_BYTES,
        }
    }

    /// Wrap a [`RedisConn`], stamping every write with a `ttl_secs` expiry.
    pub fn with_ttl(conn: C, ttl_secs: u64) -> Self {
        Self {
            conn,
            ttl_secs: Some(ttl_secs),
            max_value_bytes: DEFAULT_REDIS_MAX_VALUE_BYTES,
        }
    }

    /// Override the per-value byte cap (default
    /// [`DEFAULT_REDIS_MAX_VALUE_BYTES`]).
    #[must_use]
    pub fn with_max_value_bytes(mut self, max: usize) -> Self {
        self.max_value_bytes = max;
        self
    }

    /// The per-value byte cap this tier refuses beyond.
    #[must_use]
    pub fn max_value_bytes(&self) -> usize {
        self.max_value_bytes
    }

    /// The per-write TTL, if any.
    #[must_use]
    pub fn ttl_secs(&self) -> Option<u64> {
        self.ttl_secs
    }

    /// Borrow the underlying connection (for composition / diagnostics).
    pub fn conn(&self) -> &C {
        &self.conn
    }

    fn narinfo_key(hash: &str) -> String {
        format!("{NARINFO_PREFIX}{hash}")
    }

    fn nar_key(path: &str) -> String {
        format!("{NAR_PREFIX}{path}")
    }

    /// Redis key of one reverse edge.
    fn nar_ref_key(nar_path: &str, hash: &str) -> String {
        format!("{NAR_REF_NAMESPACE}{}", NarRefKey { nar_path, hash })
    }

    /// Redis `SCAN` prefix enumerating every edge into `nar_path`.
    fn nar_ref_scan(nar_path: &str) -> String {
        format!("{NAR_REF_NAMESPACE}{}", NarRefScan { nar_path })
    }

    /// Redis `SCAN` prefix covering every edge this backend holds.
    fn nar_ref_namespace() -> String {
        format!("{NAR_REF_NAMESPACE}{}", super::nar_refs::NAR_REF_PREFIX)
    }
}

#[async_trait]
impl<C: RedisConn> StorageBackend for RedisBackend<C> {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, StoreError> {
        let key = Self::narinfo_key(hash);
        match self.conn.get_bytes(&key).await? {
            Some(bytes) => {
                let text = String::from_utf8(bytes)
                    .map_err(|e| StoreError::NarInfo(format!("invalid utf-8 in redis narinfo {hash}: {e}")))?;
                Ok(Some(text))
            }
            None => Ok(None),
        }
    }

    async fn put_narinfo_record(&self, hash: &str, content: &str) -> Result<(), StoreError> {
        let key = Self::narinfo_key(hash);
        self.conn.set_bytes(&key, content.as_bytes(), self.ttl_secs).await
    }

    async fn delete_narinfo_record(&self, hash: &str) -> Result<(), StoreError> {
        self.conn.del(&Self::narinfo_key(hash)).await
    }

    async fn delete_nar_record(&self, nar_path: &str) -> Result<(), StoreError> {
        self.conn.del(&Self::nar_key(nar_path)).await
    }

    fn nar_ref_index(&self) -> &dyn NarRefIndex {
        self
    }

    async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let key = Self::nar_key(path);
        self.conn.get_bytes(&key).await
    }

    /// Store a NAR in the hot tier, **refusing anything over the cap**.
    ///
    /// The cap is checked against the slice's length before the value ever
    /// reaches the wire, so an oversized NAR costs this tier nothing.
    async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), StoreError> {
        if data.len() > self.max_value_bytes {
            return Err(StoreError::TooLarge {
                limit: self.max_value_bytes as u64,
                at_least: data.len() as u64,
            });
        }
        let key = Self::nar_key(path);
        self.conn.set_bytes(&key, data, self.ttl_secs).await
    }

    /// **O(min(nar, cap)).** Redis has no streaming `SET` — a value is one
    /// contiguous buffer by protocol — so this tier is bounded by a cap rather
    /// than by a chunk. That is a real bound: see
    /// [`DEFAULT_REDIS_MAX_VALUE_BYTES`] for why refusing is the correct
    /// behavior for a best-effort hot tier.
    fn nar_residency(&self) -> NarResidency {
        NarResidency::Capped(self.max_value_bytes)
    }

    /// Drain the source **only up to the cap**, refusing the moment it is
    /// crossed.
    ///
    /// The refusal is the load-bearing part: collection stops at the cap and the
    /// remainder of the NAR is never read, so a 2 GiB NAR costs this tier
    /// `cap + one chunk` and not 2 GiB. Without it, "L1 refuses oversized
    /// values" would be a claim the code does not make.
    async fn put_nar_stream(&self, path: &str, src: &dyn NarSource) -> Result<(), StoreError> {
        // A known size lets the refusal happen before a single byte is read.
        if let Some(n) = src.size_hint() {
            if n > self.max_value_bytes as u64 {
                return Err(StoreError::TooLarge {
                    limit: self.max_value_bytes as u64,
                    at_least: n,
                });
            }
        }
        let data =
            nar_stream::collect_nar(src.open().await?, Some(self.max_value_bytes)).await?;
        let key = Self::nar_key(path);
        self.conn.set_bytes(&key, &data, self.ttl_secs).await
    }

    async fn list_narinfos(&self) -> Result<Vec<String>, StoreError> {
        let keys = self.conn.keys_with_prefix(NARINFO_PREFIX).await?;
        Ok(keys
            .into_iter()
            .filter_map(|k| k.strip_prefix(NARINFO_PREFIX).map(str::to_string))
            .collect())
    }

    /// Complete L1 wipe: `DEL` every key under BOTH the narinfo and NAR prefixes
    /// (a scoped clear — never `FLUSHDB`, which would blow away an unrelated
    /// co-tenant of the same Redis db). Returns the narinfo key count removed.
    async fn wipe_all(&self) -> Result<usize, StoreError> {
        let narinfos = self.conn.keys_with_prefix(NARINFO_PREFIX).await?;
        let n = narinfos.len();
        for key in &narinfos {
            self.conn.del(key).await?;
        }
        for key in self.conn.keys_with_prefix(NAR_PREFIX).await? {
            self.conn.del(&key).await?;
        }
        // `sui:nar-refs/…` is NOT under `sui:nar:`, so the reverse index needs
        // its own sweep or a wipe would leave every edge pointing at nothing.
        for key in self.conn.keys_with_prefix(&Self::nar_ref_namespace()).await? {
            self.conn.del(&key).await?;
        }
        Ok(n)
    }
}

/// The reverse index as one Redis key per edge.
///
/// **Edges carry no TTL, deliberately**, even when narinfo/NAR writes do. An
/// edge that expired while the narinfo it describes is still live would be an
/// under-report, and an under-report authorizes deleting a NAR out from under
/// that narinfo. An edge that outlives its narinfo is an over-report, which
/// costs a retained NAR. The whole tier is still best-effort — Redis
/// `maxmemory` LRU can drop an edge regardless — which is why a
/// [`TieredBackend`](super::TieredBackend) unions this tier's answer with the
/// durable tiers' rather than trusting it alone.
#[async_trait]
impl<C: RedisConn> NarRefIndex for RedisBackend<C> {
    async fn record(&self, nar_path: &str, hash: &str) -> Result<(), StoreError> {
        self.conn.set_bytes(&Self::nar_ref_key(nar_path, hash), b"", None).await
    }

    async fn forget(&self, nar_path: &str, hash: &str) -> Result<(), StoreError> {
        self.conn.del(&Self::nar_ref_key(nar_path, hash)).await
    }

    async fn referrers(&self, nar_path: &str) -> Result<Vec<String>, StoreError> {
        let scan = NarRefScan { nar_path };
        let prefix = Self::nar_ref_scan(nar_path);
        let mut hashes: Vec<String> = self
            .conn
            .keys_with_prefix(&prefix)
            .await?
            .iter()
            .filter_map(|k| k.strip_prefix(NAR_REF_NAMESPACE))
            .filter_map(|k| referrer_of(&scan, k))
            .map(str::to_string)
            .collect();
        hashes.sort();
        hashes.dedup();
        Ok(hashes)
    }
}

// ---------------------------------------------------------------------------
// Production transport — real redis client, gated behind the `redis-client`
// feature so the default build + unit tests pull zero redis dependency surface.
// ---------------------------------------------------------------------------

#[cfg(feature = "redis-client")]
mod client {
    use super::{StoreError, RedisBackend, RedisConn};
    use async_trait::async_trait;

    fn to_store_err(e: redis::RedisError) -> StoreError {
        StoreError::Io(std::io::Error::other(format!("redis: {e}")))
    }

    /// Production [`RedisConn`] over a multiplexed, auto-reconnecting
    /// `redis::aio::ConnectionManager`. Cheap to clone (each verb clones the
    /// manager handle), so a single `RedisConnectionManager` fans out across the
    /// async runtime without a bespoke pool.
    #[derive(Clone)]
    pub struct RedisConnectionManager {
        mgr: redis::aio::ConnectionManager,
    }

    impl RedisConnectionManager {
        /// Connect to `url` (e.g. `redis://redis.super-cache-ci.svc:6379`).
        ///
        /// # Errors
        ///
        /// Returns [`StoreError::Io`] if the URL is invalid or the initial
        /// connection cannot be established.
        pub async fn connect(url: &str) -> Result<Self, StoreError> {
            let client = redis::Client::open(url).map_err(to_store_err)?;
            let mgr = redis::aio::ConnectionManager::new(client)
                .await
                .map_err(to_store_err)?;
            Ok(Self { mgr })
        }
    }

    #[async_trait]
    impl RedisConn for RedisConnectionManager {
        async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
            let mut c = self.mgr.clone();
            let v: Option<Vec<u8>> = redis::cmd("GET")
                .arg(key)
                .query_async(&mut c)
                .await
                .map_err(to_store_err)?;
            Ok(v)
        }

        async fn set_bytes(&self, key: &str, value: &[u8], ttl_secs: Option<u64>) -> Result<(), StoreError> {
            let mut c = self.mgr.clone();
            let mut cmd = redis::cmd("SET");
            cmd.arg(key).arg(value);
            if let Some(secs) = ttl_secs {
                cmd.arg("EX").arg(secs);
            }
            let _: () = cmd.query_async(&mut c).await.map_err(to_store_err)?;
            Ok(())
        }

        async fn del(&self, key: &str) -> Result<(), StoreError> {
            let mut c = self.mgr.clone();
            let _: i64 = redis::cmd("DEL")
                .arg(key)
                .query_async(&mut c)
                .await
                .map_err(to_store_err)?;
            Ok(())
        }

        async fn keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
            let mut c = self.mgr.clone();
            let pattern = format!("{prefix}*");
            let mut cursor: u64 = 0;
            let mut out = Vec::new();
            loop {
                let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                    .arg(cursor)
                    .arg("MATCH")
                    .arg(&pattern)
                    .arg("COUNT")
                    .arg(512)
                    .query_async(&mut c)
                    .await
                    .map_err(to_store_err)?;
                out.extend(batch);
                if next == 0 {
                    break;
                }
                cursor = next;
            }
            Ok(out)
        }
    }

    impl RedisBackend<RedisConnectionManager> {
        /// Connect an L1 backend to `url` with no per-write TTL (LRU-evicted).
        ///
        /// # Errors
        ///
        /// Propagates a connection failure from [`RedisConnectionManager::connect`].
        pub async fn connect(url: &str) -> Result<Self, StoreError> {
            Ok(Self::new(RedisConnectionManager::connect(url).await?))
        }

        /// Connect an L1 backend to `url`, stamping every write with `ttl_secs`.
        ///
        /// # Errors
        ///
        /// Propagates a connection failure from [`RedisConnectionManager::connect`].
        pub async fn connect_with_ttl(url: &str, ttl_secs: u64) -> Result<Self, StoreError> {
            Ok(Self::with_ttl(RedisConnectionManager::connect(url).await?, ttl_secs))
        }
    }
}

#[cfg(feature = "redis-client")]
pub use client::RedisConnectionManager;

// ---------------------------------------------------------------------------
// Unit tests — the L1 semantics proven against an in-memory mock RedisConn.
// No live Redis required.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory [`RedisConn`] mock. Records each write's TTL so tests can prove
    /// TTL/eviction awareness, and exposes `evict` to simulate `maxmemory` LRU
    /// dropping a hot key (or a pod roll losing the whole tier via `clear`).
    #[derive(Default)]
    struct MockRedis {
        // key -> (value, ttl_secs seen on last write)
        map: Mutex<HashMap<String, (Vec<u8>, Option<u64>)>>,
    }

    impl MockRedis {
        fn ttl_of(&self, key: &str) -> Option<u64> {
            self.map.lock().unwrap().get(key).and_then(|(_, t)| *t)
        }

        /// Simulate `maxmemory` LRU evicting a single hot key.
        fn evict(&self, key: &str) {
            self.map.lock().unwrap().remove(key);
        }

        /// Simulate a pod roll losing the entire hot tier.
        fn clear(&self) {
            self.map.lock().unwrap().clear();
        }

        fn len(&self) -> usize {
            self.map.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl RedisConn for MockRedis {
        async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
            Ok(self.map.lock().unwrap().get(key).map(|(v, _)| v.clone()))
        }

        async fn set_bytes(&self, key: &str, value: &[u8], ttl_secs: Option<u64>) -> Result<(), StoreError> {
            self.map
                .lock()
                .unwrap()
                .insert(key.to_string(), (value.to_vec(), ttl_secs));
            Ok(())
        }

        async fn del(&self, key: &str) -> Result<(), StoreError> {
            self.map.lock().unwrap().remove(key);
            Ok(())
        }

        async fn keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }
    }

    const NARINFO: &str = "StorePath: /nix/store/abc-hello\nURL: nar/abc.nar.xz\nCompression: xz\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n";

    #[tokio::test]
    async fn get_missing_narinfo_returns_none() {
        let backend = RedisBackend::new(MockRedis::default());
        assert!(backend.get_narinfo("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_then_get_narinfo_roundtrips() {
        let backend = RedisBackend::new(MockRedis::default());
        backend.put_narinfo("abc", NARINFO).await.unwrap();
        let got = backend.get_narinfo("abc").await.unwrap().unwrap();
        assert_eq!(got, NARINFO);
    }

    #[tokio::test]
    async fn get_missing_nar_returns_none() {
        let backend = RedisBackend::new(MockRedis::default());
        assert!(backend.get_nar("nar/missing.nar.xz").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_then_get_nar_roundtrips() {
        let backend = RedisBackend::new(MockRedis::default());
        let data = b"\x00\x01\x02 fake nar bytes";
        backend.put_nar("nar/abc.nar.xz", data).await.unwrap();
        let got = backend.get_nar("nar/abc.nar.xz").await.unwrap().unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn narinfo_and_nar_keyspaces_do_not_collide() {
        // Same bare id used for both a narinfo hash and a nar path fragment.
        let backend = RedisBackend::new(MockRedis::default());
        backend.put_narinfo("dead", "the-narinfo").await.unwrap();
        backend.put_nar("dead", b"the-nar").await.unwrap();
        assert_eq!(backend.get_narinfo("dead").await.unwrap().unwrap(), "the-narinfo");
        assert_eq!(backend.get_nar("dead").await.unwrap().unwrap(), b"the-nar");
    }

    #[tokio::test]
    async fn no_ttl_by_default() {
        let mock = MockRedis::default();
        let backend = RedisBackend::new(mock);
        assert_eq!(backend.ttl_secs(), None);
        backend.put_narinfo("abc", NARINFO).await.unwrap();
        // The write carried no expiry.
        assert_eq!(backend.conn().ttl_of("sui:narinfo:abc"), None);
    }

    #[tokio::test]
    async fn with_ttl_stamps_every_write() {
        let backend = RedisBackend::with_ttl(MockRedis::default(), 3600);
        assert_eq!(backend.ttl_secs(), Some(3600));
        backend.put_narinfo("abc", NARINFO).await.unwrap();
        backend.put_nar("nar/abc.nar.xz", b"data").await.unwrap();
        assert_eq!(backend.conn().ttl_of("sui:narinfo:abc"), Some(3600));
        assert_eq!(backend.conn().ttl_of("sui:nar:nar/abc.nar.xz"), Some(3600));
    }

    #[tokio::test]
    async fn eviction_of_a_hot_key_is_a_plain_miss() {
        // A cache, not a source of truth: an evicted key reads back as Ok(None).
        let backend = RedisBackend::new(MockRedis::default());
        backend.put_narinfo("abc", NARINFO).await.unwrap();
        assert!(backend.get_narinfo("abc").await.unwrap().is_some());
        backend.conn().evict("sui:narinfo:abc");
        assert!(backend.get_narinfo("abc").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn pod_roll_clears_the_whole_hot_tier() {
        let backend = RedisBackend::new(MockRedis::default());
        backend.put_narinfo("a", NARINFO).await.unwrap();
        backend.put_nar("nar/a.nar.xz", b"x").await.unwrap();
        backend.conn().clear();
        assert!(backend.get_narinfo("a").await.unwrap().is_none());
        assert!(backend.get_nar("nar/a.nar.xz").await.unwrap().is_none());
    }

    /// `delete` removes the NAR the narinfo names, not three store-hash-shaped
    /// guesses. `NARINFO` advertises `nar/abc.nar.xz` while the store hash is
    /// `xyz` — the ordinary case, since a NAR is keyed by narhash.
    #[tokio::test]
    async fn delete_resolves_the_nar_from_the_narinfo_instead_of_guessing() {
        let backend = RedisBackend::new(MockRedis::default());
        backend.put_narinfo("xyz", NARINFO).await.unwrap();
        backend.put_nar("nar/abc.nar.xz", b"the real nar").await.unwrap();
        backend.put_nar("nar/xyz.nar.zst", b"someone else's nar").await.unwrap();

        backend.delete("xyz").await.unwrap();

        assert!(backend.get_narinfo("xyz").await.unwrap().is_none());
        assert!(backend.get_nar("nar/abc.nar.xz").await.unwrap().is_none());
        assert_eq!(
            backend.get_nar("nar/xyz.nar.zst").await.unwrap().unwrap(),
            b"someone else's nar",
            "a key this narinfo never named must be untouched",
        );
    }

    /// The hot tier's own reverse index, round-tripped through the mock's
    /// `SCAN` — proving the edge encoding and the prefix scan agree.
    #[tokio::test]
    async fn the_hot_tier_indexes_and_forgets_its_edges() {
        let backend = RedisBackend::new(MockRedis::default());
        backend.put_narinfo("xyz", NARINFO).await.unwrap();
        backend.put_narinfo("second", NARINFO).await.unwrap();
        assert_eq!(
            backend.nar_ref_index().referrers("nar/abc.nar.xz").await.unwrap(),
            vec!["second".to_string(), "xyz".to_string()],
        );

        backend.delete("xyz").await.unwrap();
        assert_eq!(
            backend.nar_ref_index().referrers("nar/abc.nar.xz").await.unwrap(),
            vec!["second".to_string()],
        );
    }

    #[tokio::test]
    async fn delete_absent_is_idempotent() {
        let backend = RedisBackend::new(MockRedis::default());
        // Must not error on a wholly-absent key.
        backend.delete("ghost").await.unwrap();
        assert_eq!(backend.conn().len(), 0);
    }

    #[tokio::test]
    async fn list_narinfos_returns_hot_subset_stripped() {
        let backend = RedisBackend::new(MockRedis::default());
        backend.put_narinfo("aaa", "1").await.unwrap();
        backend.put_narinfo("bbb", "2").await.unwrap();
        // A NAR write must not leak into the narinfo listing.
        backend.put_nar("nar/ccc.nar.xz", b"3").await.unwrap();
        let mut hashes = backend.list_narinfos().await.unwrap();
        hashes.sort();
        assert_eq!(hashes, vec!["aaa".to_string(), "bbb".to_string()]);
    }

    #[tokio::test]
    async fn list_narinfos_empty_when_cold() {
        let backend = RedisBackend::new(MockRedis::default());
        assert!(backend.list_narinfos().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn overwrite_narinfo_takes_latest() {
        let backend = RedisBackend::new(MockRedis::default());
        backend.put_narinfo("h", "v1").await.unwrap();
        backend.put_narinfo("h", "v2").await.unwrap();
        assert_eq!(backend.get_narinfo("h").await.unwrap().unwrap(), "v2");
    }

    // ── the cap: a bound, enforced by refusing rather than buffering ───────

    #[tokio::test]
    async fn residency_reports_the_configured_cap() {
        let backend = RedisBackend::new(MockRedis::default()).with_max_value_bytes(1024);
        assert_eq!(backend.max_value_bytes(), 1024);
        assert_eq!(backend.nar_residency(), NarResidency::Capped(1024));
        assert!(backend.nar_residency().is_bounded(), "a cap IS a bound");
    }

    #[tokio::test]
    async fn an_over_cap_nar_is_refused_and_stores_nothing() {
        let backend = RedisBackend::new(MockRedis::default()).with_max_value_bytes(16);
        let err = backend.put_nar("nar/big.nar.xz", &[0u8; 64]).await.unwrap_err();
        assert!(matches!(err, StoreError::TooLarge { limit: 16, at_least: 64 }));
        assert!(
            backend.get_nar("nar/big.nar.xz").await.unwrap().is_none(),
            "a refused write must leave the tier untouched",
        );
    }

    #[tokio::test]
    async fn a_value_exactly_at_the_cap_is_accepted() {
        // The boundary is inclusive; an off-by-one here would silently shrink
        // the hot tier's usable range.
        let backend = RedisBackend::new(MockRedis::default()).with_max_value_bytes(16);
        backend.put_nar("nar/edge.nar.xz", &[7u8; 16]).await.unwrap();
        assert_eq!(backend.get_nar("nar/edge.nar.xz").await.unwrap().unwrap(), vec![7u8; 16]);
    }

    /// A source that counts how many bytes were actually pulled out of it, so a
    /// test can prove the refusal happened *early* rather than after reading
    /// the whole NAR and then throwing it away.
    struct CountingSource {
        total: usize,
        chunk: usize,
        read: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// Whether the source advertises its length up front, as a spooled
        /// upload does and a tier-to-tier promotion may not.
        advertise_len: bool,
    }

    #[async_trait]
    impl super::nar_stream::NarSource for CountingSource {
        fn size_hint(&self) -> Option<u64> {
            self.advertise_len.then_some(self.total as u64)
        }
        async fn open(&self) -> Result<super::nar_stream::NarStream, StoreError> {
            use futures::StreamExt as _;
            let (total, chunk, read) = (self.total, self.chunk, self.read.clone());
            Ok(futures::stream::unfold(0usize, move |sent| {
                let read = read.clone();
                async move {
                    if sent >= total {
                        return None;
                    }
                    let n = (total - sent).min(chunk);
                    read.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                    Some((Ok(bytes::Bytes::from(vec![3u8; n])), sent + n))
                }
            })
            .boxed())
        }
    }

    #[tokio::test]
    async fn an_over_cap_stream_is_refused_without_reading_a_single_byte() {
        // The cheap path: a spooled upload knows its length, so the tier can
        // decline before touching the source at all.
        let read = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let backend = RedisBackend::new(MockRedis::default()).with_max_value_bytes(1024);
        let src = CountingSource {
            total: 1_000_000,
            chunk: 4096,
            read: read.clone(),
            advertise_len: true,
        };
        let err = backend.put_nar_stream("nar/big.nar.xz", &src).await.unwrap_err();
        assert!(matches!(err, StoreError::TooLarge { .. }));
        assert_eq!(read.load(std::sync::atomic::Ordering::Relaxed), 0, "nothing should be read");
    }

    #[tokio::test]
    async fn an_over_cap_stream_of_unknown_length_stops_at_the_cap() {
        // The important case, and the one the whole change turns on: with NO
        // length advertised, collection must stop the instant the cap is
        // crossed. Reading the whole 1 MB and then refusing would mean the cap
        // bounds nothing at all.
        let read = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let backend = RedisBackend::new(MockRedis::default()).with_max_value_bytes(1024);
        let src = CountingSource {
            total: 1_000_000,
            chunk: 4096,
            read: read.clone(),
            advertise_len: false,
        };
        let err = backend.put_nar_stream("nar/big.nar.xz", &src).await.unwrap_err();
        assert!(matches!(err, StoreError::TooLarge { .. }));
        let bytes_read = read.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            bytes_read <= 1024 + 4096,
            "refusal must happen at the cap (+ at most one chunk), but {bytes_read} bytes \
             were pulled — the cap is not bounding anything",
        );
    }

    #[tokio::test]
    async fn an_under_cap_stream_round_trips() {
        let backend = RedisBackend::new(MockRedis::default()).with_max_value_bytes(1 << 20);
        let src = super::nar_stream::BytesNarSource::new(vec![9u8; 5000]);
        backend.put_nar_stream("nar/ok.nar.xz", &src).await.unwrap();
        assert_eq!(backend.get_nar("nar/ok.nar.xz").await.unwrap().unwrap(), vec![9u8; 5000]);
    }

    #[tokio::test]
    async fn invalid_utf8_narinfo_surfaces_typed_error() {
        // A corrupt hot entry must surface a typed NarInfo error, not silently
        // fabricate bytes.
        let mock = MockRedis::default();
        mock.map
            .lock()
            .unwrap()
            .insert("sui:narinfo:bad".to_string(), (vec![0xff, 0xfe, 0xfd], None));
        let backend = RedisBackend::new(mock);
        let err = backend.get_narinfo("bad").await.unwrap_err();
        assert!(matches!(err, StoreError::NarInfo(_)));
    }
}
