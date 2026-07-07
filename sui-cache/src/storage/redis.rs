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

use super::StorageBackend;
use crate::CacheError;

/// Key namespace for narinfo strings, so they never collide with NAR blobs in a
/// single Redis keyspace.
const NARINFO_PREFIX: &str = "sui:narinfo:";
/// Key namespace for NAR blobs.
const NAR_PREFIX: &str = "sui:nar:";

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
    async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError>;

    /// `SET key value [EX ttl_secs]` — store raw bytes, optionally with an
    /// expiry. `ttl_secs == None` means no explicit expiry (LRU-evicted by the
    /// `maxmemory` policy).
    async fn set_bytes(&self, key: &str, value: &[u8], ttl_secs: Option<u64>) -> Result<(), CacheError>;

    /// `DEL key` — idempotent; deleting an absent key is `Ok(())`.
    async fn del(&self, key: &str) -> Result<(), CacheError>;

    /// Non-blocking `SCAN MATCH prefix*` — every key currently resident under
    /// `prefix`. Partial by nature (a cache), and must use `SCAN`, never the
    /// O(N) blocking `KEYS`.
    async fn keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>, CacheError>;
}

/// L1 hot cache: content-addressed key → value, sub-ms hits, TTL/eviction-aware.
///
/// Generic over the [`RedisConn`] seam so it is fully testable against a mock.
pub struct RedisBackend<C: RedisConn> {
    conn: C,
    /// Optional TTL (seconds) applied to every write; `None` => rely on the
    /// `maxmemory` LRU policy.
    ttl_secs: Option<u64>,
}

impl<C: RedisConn> RedisBackend<C> {
    /// Wrap a [`RedisConn`] with no per-write TTL (entries are LRU-evicted by
    /// the `maxmemory` band).
    pub fn new(conn: C) -> Self {
        Self { conn, ttl_secs: None }
    }

    /// Wrap a [`RedisConn`], stamping every write with a `ttl_secs` expiry.
    pub fn with_ttl(conn: C, ttl_secs: u64) -> Self {
        Self { conn, ttl_secs: Some(ttl_secs) }
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
}

#[async_trait]
impl<C: RedisConn> StorageBackend for RedisBackend<C> {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, CacheError> {
        let key = Self::narinfo_key(hash);
        match self.conn.get_bytes(&key).await? {
            Some(bytes) => {
                let text = String::from_utf8(bytes)
                    .map_err(|e| CacheError::NarInfo(format!("invalid utf-8 in redis narinfo {hash}: {e}")))?;
                Ok(Some(text))
            }
            None => Ok(None),
        }
    }

    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), CacheError> {
        let key = Self::narinfo_key(hash);
        self.conn.set_bytes(&key, content.as_bytes(), self.ttl_secs).await
    }

    async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let key = Self::nar_key(path);
        self.conn.get_bytes(&key).await
    }

    async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), CacheError> {
        let key = Self::nar_key(path);
        self.conn.set_bytes(&key, data, self.ttl_secs).await
    }

    async fn delete(&self, hash: &str) -> Result<(), CacheError> {
        // Narinfo is keyed directly by hash.
        self.conn.del(&Self::narinfo_key(hash)).await?;
        // NAR blobs are keyed by relative URL; we only have the hash here, so —
        // mirroring `S3Storage::delete` — best-effort delete the common NAR path
        // patterns. `del` is idempotent, so absent keys are harmless.
        for ext in ["nar.xz", "nar.zst", "nar"] {
            self.conn.del(&Self::nar_key(&format!("nar/{hash}.{ext}"))).await?;
        }
        Ok(())
    }

    async fn list_narinfos(&self) -> Result<Vec<String>, CacheError> {
        let keys = self.conn.keys_with_prefix(NARINFO_PREFIX).await?;
        Ok(keys
            .into_iter()
            .filter_map(|k| k.strip_prefix(NARINFO_PREFIX).map(str::to_string))
            .collect())
    }

    /// Complete L1 wipe: `DEL` every key under BOTH the narinfo and NAR prefixes
    /// (a scoped clear — never `FLUSHDB`, which would blow away an unrelated
    /// co-tenant of the same Redis db). Returns the narinfo key count removed.
    async fn wipe_all(&self) -> Result<usize, CacheError> {
        let narinfos = self.conn.keys_with_prefix(NARINFO_PREFIX).await?;
        let n = narinfos.len();
        for key in &narinfos {
            self.conn.del(key).await?;
        }
        for key in self.conn.keys_with_prefix(NAR_PREFIX).await? {
            self.conn.del(&key).await?;
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Production transport — real redis client, gated behind the `redis-client`
// feature so the default build + unit tests pull zero redis dependency surface.
// ---------------------------------------------------------------------------

#[cfg(feature = "redis-client")]
mod client {
    use super::{CacheError, RedisBackend, RedisConn};
    use async_trait::async_trait;

    fn to_cache_err(e: redis::RedisError) -> CacheError {
        CacheError::Io(std::io::Error::other(format!("redis: {e}")))
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
        /// Returns [`CacheError::Io`] if the URL is invalid or the initial
        /// connection cannot be established.
        pub async fn connect(url: &str) -> Result<Self, CacheError> {
            let client = redis::Client::open(url).map_err(to_cache_err)?;
            let mgr = redis::aio::ConnectionManager::new(client)
                .await
                .map_err(to_cache_err)?;
            Ok(Self { mgr })
        }
    }

    #[async_trait]
    impl RedisConn for RedisConnectionManager {
        async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
            let mut c = self.mgr.clone();
            let v: Option<Vec<u8>> = redis::cmd("GET")
                .arg(key)
                .query_async(&mut c)
                .await
                .map_err(to_cache_err)?;
            Ok(v)
        }

        async fn set_bytes(&self, key: &str, value: &[u8], ttl_secs: Option<u64>) -> Result<(), CacheError> {
            let mut c = self.mgr.clone();
            let mut cmd = redis::cmd("SET");
            cmd.arg(key).arg(value);
            if let Some(secs) = ttl_secs {
                cmd.arg("EX").arg(secs);
            }
            let _: () = cmd.query_async(&mut c).await.map_err(to_cache_err)?;
            Ok(())
        }

        async fn del(&self, key: &str) -> Result<(), CacheError> {
            let mut c = self.mgr.clone();
            let _: i64 = redis::cmd("DEL")
                .arg(key)
                .query_async(&mut c)
                .await
                .map_err(to_cache_err)?;
            Ok(())
        }

        async fn keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>, CacheError> {
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
                    .map_err(to_cache_err)?;
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
        pub async fn connect(url: &str) -> Result<Self, CacheError> {
            Ok(Self::new(RedisConnectionManager::connect(url).await?))
        }

        /// Connect an L1 backend to `url`, stamping every write with `ttl_secs`.
        ///
        /// # Errors
        ///
        /// Propagates a connection failure from [`RedisConnectionManager::connect`].
        pub async fn connect_with_ttl(url: &str, ttl_secs: u64) -> Result<Self, CacheError> {
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
        async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
            Ok(self.map.lock().unwrap().get(key).map(|(v, _)| v.clone()))
        }

        async fn set_bytes(&self, key: &str, value: &[u8], ttl_secs: Option<u64>) -> Result<(), CacheError> {
            self.map
                .lock()
                .unwrap()
                .insert(key.to_string(), (value.to_vec(), ttl_secs));
            Ok(())
        }

        async fn del(&self, key: &str) -> Result<(), CacheError> {
            self.map.lock().unwrap().remove(key);
            Ok(())
        }

        async fn keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>, CacheError> {
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

    #[tokio::test]
    async fn delete_removes_narinfo_and_common_nar_patterns() {
        let backend = RedisBackend::new(MockRedis::default());
        backend.put_narinfo("xyz", NARINFO).await.unwrap();
        backend.put_nar("nar/xyz.nar.xz", b"nar-xz").await.unwrap();
        backend.put_nar("nar/xyz.nar.zst", b"nar-zst").await.unwrap();
        backend.put_nar("nar/xyz.nar", b"nar-plain").await.unwrap();

        backend.delete("xyz").await.unwrap();

        assert!(backend.get_narinfo("xyz").await.unwrap().is_none());
        assert!(backend.get_nar("nar/xyz.nar.xz").await.unwrap().is_none());
        assert!(backend.get_nar("nar/xyz.nar.zst").await.unwrap().is_none());
        assert!(backend.get_nar("nar/xyz.nar").await.unwrap().is_none());
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
        assert!(matches!(err, CacheError::NarInfo(_)));
    }
}
