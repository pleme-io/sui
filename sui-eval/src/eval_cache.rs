//! Content-addressed evaluation cache.
//!
//! Maps `(source_hash, lock_hash)` pairs to previously evaluated results,
//! skipping redundant evaluation when inputs haven't changed.
//!
//! ## Tier model (additive — every tier is optional, all may stack)
//!
//! 1. **In-memory** (`HashMap`) for the current session — instant
//!    lookups. Always present.
//! 2. **JSON file** at `~/.cache/sui/eval-cache.json` — survives
//!    across invocations. Optional; enabled by `with_persistent`.
//! 3. **GraphStore** (`sui-graph-store`, redb + rkyv on a ZFS-friendly
//!    blob layout) — fleet-shared / cross-process tier. Optional;
//!    enabled by `with_graph_store`. When set, the eval cache's
//!    entries become first-class blobs in `GraphKind::EvalCacheEntry`,
//!    which means a peer with the same GraphStore root (e.g. via
//!    `zfs send | zfs recv` or a future substituter push) gets every
//!    cached eval for free. Lookup-order on `get`: memory → graph_store
//!    (warm on disk via mmap, hits sub-200 µs); on a hit from the
//!    graph_store tier the result is promoted into memory so the next
//!    same-process lookup is sub-microsecond.
//!
//! All three tiers honor the same `enabled` flag (set by the CLI flag
//! that disables caching entirely) and the same key shape (`CacheKey`).
//! Adding a tier never removes an older one — `with_all_tiers` enables
//! all three at once; individual `with_*` constructors stack them
//! incrementally.
//!
//! Only JSON-serializable values are cached (no lambdas, no thunks).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sui_graph_store::{GraphHash, GraphKind, GraphStore};

// ── Types ──────────────────────────────────────────────────────

/// Hash of a source file plus its transitive inputs (flake.lock).
#[derive(Hash, Eq, PartialEq, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CacheKey {
    /// SHA-256 hex digest of the source file content.
    pub source_hash: String,
    /// SHA-256 hex digest of `flake.lock` in the same directory (if any).
    pub lock_hash: Option<String>,
}

/// A cached evaluation result.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CachedValue {
    /// The value serialized as JSON.
    pub value_json: String,
    /// Unix timestamp when this entry was stored.
    pub timestamp: i64,
}

/// A single cache entry for serialization (key + value).
#[derive(serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    key: CacheKey,
    value: CachedValue,
}

// ── Cache ──────────────────────────────────────────────────────

/// Content-addressed evaluation cache with optional persistence.
pub struct EvalCache {
    /// In-memory cache for the current session.
    memory: HashMap<CacheKey, CachedValue>,
    /// Path to the persistent cache file (JSON).
    db_path: Option<PathBuf>,
    /// Optional GraphStore tier — fleet-shared / cross-process cache.
    /// When set, entries are mirrored as `GraphKind::EvalCacheEntry`
    /// blobs and a `get` miss in memory falls through here next.
    graph_store: Option<GraphStore>,
    /// Whether this cache is enabled (can be disabled via CLI flag).
    enabled: bool,
}

impl EvalCache {
    /// Create a new in-memory-only cache.
    pub fn new() -> Self {
        Self {
            memory: HashMap::new(),
            db_path: None,
            graph_store: None,
            enabled: true,
        }
    }

    /// Create a cache with persistent storage at the given path.
    /// Loads existing entries from disk if the file exists.
    pub fn with_persistent(db_path: PathBuf) -> Self {
        let memory = Self::load_from_disk(&db_path).unwrap_or_default();
        Self {
            memory,
            db_path: Some(db_path),
            graph_store: None,
            enabled: true,
        }
    }

    /// Create a cache using the default persistent path (`~/.cache/sui/eval-cache.json`).
    pub fn default_persistent() -> Self {
        match default_cache_path() {
            Some(p) => Self::with_persistent(p),
            None => Self::new(),
        }
    }

    /// Create a disabled cache (always misses).
    pub fn disabled() -> Self {
        Self {
            memory: HashMap::new(),
            db_path: None,
            graph_store: None,
            enabled: false,
        }
    }

    /// Stack a `GraphStore` tier on this cache. Existing tiers
    /// (in-memory + optional JSON file) are preserved verbatim. Calls
    /// this builder-style: `EvalCache::default_persistent().with_graph_store(gs)`.
    #[must_use]
    pub fn with_graph_store(mut self, store: GraphStore) -> Self {
        self.graph_store = Some(store);
        self
    }

    /// Construct an `EvalCache` with all three tiers enabled.
    ///
    /// * In-memory — always.
    /// * JSON file — at `db_path` (also loaded on construction).
    /// * GraphStore — using `store`.
    #[must_use]
    pub fn with_all_tiers(db_path: PathBuf, store: GraphStore) -> Self {
        Self::with_persistent(db_path).with_graph_store(store)
    }

    /// Whether the cache is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// True iff the GraphStore tier is wired.
    pub fn has_graph_store(&self) -> bool {
        self.graph_store.is_some()
    }

    /// Look up a cached result. Tier order: memory → graph_store.
    /// **Behavior contract**: a hit from the graph_store tier is
    /// promoted into the memory tier so the next same-process lookup
    /// is sub-microsecond. The promotion is the only mutation `get`
    /// performs.
    pub fn get(&mut self, key: &CacheKey) -> Option<&CachedValue> {
        if !self.enabled {
            return None;
        }
        // Tier 1: in-memory (sub-microsecond).
        if self.memory.contains_key(key) {
            return self.memory.get(key);
        }
        // Tier 3: GraphStore (sub-200 µs warm via mmap).
        if let Some(store) = &self.graph_store {
            let gh = graph_hash_for_key(key);
            if let Ok(blob) = store.get(GraphKind::EvalCacheEntry, gh) {
                if let Ok(value) = serde_json::from_slice::<CachedValue>(&blob) {
                    self.memory.insert(key.clone(), value);
                    return self.memory.get(key);
                }
            }
        }
        None
    }

    /// Store a result in the cache. Writes to memory unconditionally
    /// and to every wired persistence tier (JSON + GraphStore)
    /// best-effort. Tier writes never fail loudly — eval-cache puts
    /// are advisory; a failed write doesn't change the correctness of
    /// the eval, just the chance of a future hit.
    pub fn put(&mut self, key: CacheKey, value: CachedValue) {
        if !self.enabled {
            return;
        }
        // Tier 1: memory (mandatory).
        self.memory.insert(key.clone(), value.clone());
        // Tier 2: JSON file (legacy persistent path).
        if let Some(ref path) = self.db_path {
            let _ = Self::save_to_disk(path, &self.memory);
        }
        // Tier 3: GraphStore (fleet-shared). Keyed by a deterministic
        // BLAKE3 of the cache key (NOT by content hash) — the eval
        // cache wants query-derived lookup, so this uses
        // `put_unchecked`. Domain-separated with the `"evalcache::v1::"`
        // prefix to keep query-derived hashes disjoint from CAS hashes
        // in the same GraphKind.
        if let Some(store) = &self.graph_store {
            if let Ok(blob) = serde_json::to_vec(&value) {
                let lookup_hash = graph_hash_for_key(&key);
                let _ = store.put_unchecked(GraphKind::EvalCacheEntry, lookup_hash, &blob);
            }
        }
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.memory.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.memory.is_empty()
    }

    /// Compute the cache key for a file on disk.
    ///
    /// The source hash is the SHA-256 of the file content. If a `flake.lock`
    /// exists in the same directory, its hash is included as the lock_hash.
    pub fn key_for_file(path: &Path) -> Option<CacheKey> {
        let content = std::fs::read(path).ok()?;
        let source_hash = sha256_hex(&content);

        let lock_hash = path
            .parent()
            .map(|dir| dir.join("flake.lock"))
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read(p).ok())
            .map(|c| sha256_hex(&c));

        Some(CacheKey {
            source_hash,
            lock_hash,
        })
    }

    // ── Persistence helpers ────────────────────────────────────

    fn load_from_disk(path: &Path) -> Option<HashMap<CacheKey, CachedValue>> {
        let data = std::fs::read_to_string(path).ok()?;
        let entries: Vec<CacheEntry> = serde_json::from_str(&data).ok()?;
        let mut map = HashMap::with_capacity(entries.len());
        for entry in entries {
            map.insert(entry.key, entry.value);
        }
        Some(map)
    }

    fn save_to_disk(
        path: &Path,
        memory: &HashMap<CacheKey, CachedValue>,
    ) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let entries: Vec<CacheEntry> = memory
            .iter()
            .map(|(k, v)| CacheEntry {
                key: k.clone(),
                value: v.clone(),
            })
            .collect();
        let json = serde_json::to_string(&entries)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, json)
    }
}

impl Default for EvalCache {
    fn default() -> Self {
        Self::new()
    }
}

// ── Helpers ────────────────────────────────────────────────────

/// Derive a deterministic `GraphHash` from an eval-cache `CacheKey`,
/// domain-separated so it can't collide with content-addressed entries
/// stored under the same `GraphKind`. The serialization is canonical
/// because we control both fields: `(source_hash, lock_hash)` are
/// already SHA-256 hex digests with stable ordering.
fn graph_hash_for_key(key: &CacheKey) -> GraphHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"evalcache::v1::");
    hasher.update(key.source_hash.as_bytes());
    hasher.update(b"::");
    if let Some(lock) = &key.lock_hash {
        hasher.update(lock.as_bytes());
    } else {
        hasher.update(b"<no-lock>");
    }
    GraphHash(hasher.finalize().into())
}

/// SHA-256 hex digest of a byte slice.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Default persistent cache path: `~/.cache/sui/eval-cache.json`.
fn default_cache_path() -> Option<PathBuf> {
    dirs_next().map(|d| d.join("sui").join("eval-cache.json"))
}

/// Platform cache directory (`$XDG_CACHE_HOME` or `~/.cache`).
fn dirs_next() -> Option<PathBuf> {
    if let Ok(val) = std::env::var("XDG_CACHE_HOME") {
        if !val.is_empty() {
            return Some(PathBuf::from(val));
        }
    }
    #[cfg(target_os = "macos")]
    {
        home_dir().map(|h| h.join("Library").join("Caches"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        home_dir().map(|h| h.join(".cache"))
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Return the current Unix timestamp (seconds since epoch).
pub fn now_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_returns_same_value() {
        let mut cache = EvalCache::new();
        let key = CacheKey {
            source_hash: "abc123".to_string(),
            lock_hash: None,
        };
        let value = CachedValue {
            value_json: r#"{"type":"int","value":42}"#.to_string(),
            timestamp: 1000,
        };
        cache.put(key.clone(), value.clone());
        let got = cache.get(&key).unwrap();
        assert_eq!(got.value_json, value.value_json);
    }

    #[test]
    fn cache_miss_returns_none() {
        let mut cache = EvalCache::new();
        let key = CacheKey {
            source_hash: "nonexistent".to_string(),
            lock_hash: None,
        };
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn different_content_different_key() {
        let mut cache = EvalCache::new();
        let k1 = CacheKey {
            source_hash: sha256_hex(b"file content A"),
            lock_hash: None,
        };
        let k2 = CacheKey {
            source_hash: sha256_hex(b"file content B"),
            lock_hash: None,
        };
        cache.put(
            k1.clone(),
            CachedValue {
                value_json: "A".to_string(),
                timestamp: 1,
            },
        );
        assert!(cache.get(&k1).is_some());
        assert!(cache.get(&k2).is_none());
    }

    #[test]
    fn lock_hash_change_invalidates() {
        let mut cache = EvalCache::new();
        let k1 = CacheKey {
            source_hash: "same".to_string(),
            lock_hash: Some("lock-v1".to_string()),
        };
        let k2 = CacheKey {
            source_hash: "same".to_string(),
            lock_hash: Some("lock-v2".to_string()),
        };
        cache.put(
            k1.clone(),
            CachedValue {
                value_json: "v1".to_string(),
                timestamp: 1,
            },
        );
        assert!(cache.get(&k1).is_some());
        assert!(cache.get(&k2).is_none());
    }

    #[test]
    fn disabled_cache_always_misses() {
        let mut cache = EvalCache::disabled();
        let key = CacheKey {
            source_hash: "abc".to_string(),
            lock_hash: None,
        };
        cache.put(
            key.clone(),
            CachedValue {
                value_json: "x".to_string(),
                timestamp: 1,
            },
        );
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn key_for_file_hashes_content() {
        let dir = std::env::temp_dir().join("sui-eval-cache-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.nix");
        std::fs::write(&path, "1 + 2").unwrap();

        let key = EvalCache::key_for_file(&path).unwrap();
        assert!(!key.source_hash.is_empty());
        assert!(key.lock_hash.is_none()); // no flake.lock

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn key_for_file_with_flake_lock() {
        let dir = std::env::temp_dir().join("sui-eval-cache-test-lock");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("flake.nix");
        let lock = dir.join("flake.lock");
        std::fs::write(&path, "{ }").unwrap();
        std::fs::write(&lock, r#"{"nodes":{}}"#).unwrap();

        let key = EvalCache::key_for_file(&path).unwrap();
        assert!(key.lock_hash.is_some());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&lock);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn persistent_roundtrip() {
        let dir = std::env::temp_dir().join("sui-eval-cache-persist");
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("test-cache.json");

        // Write
        {
            let mut c = EvalCache::with_persistent(db.clone());
            c.put(
                CacheKey {
                    source_hash: "h1".to_string(),
                    lock_hash: None,
                },
                CachedValue {
                    value_json: r#""hello""#.to_string(),
                    timestamp: now_timestamp(),
                },
            );
            assert_eq!(c.len(), 1);
        }

        // Read back
        {
            let mut c = EvalCache::with_persistent(db.clone());
            let key = CacheKey {
                source_hash: "h1".to_string(),
                lock_hash: None,
            };
            let v = c.get(&key).unwrap();
            assert_eq!(v.value_json, r#""hello""#);
        }

        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_dir(&dir);
    }

    // ── GraphStore tier — additive integration tests ───────────────

    fn temp_graph_store() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::open(dir.path().to_path_buf()).unwrap();
        (dir, store)
    }

    #[test]
    fn graph_store_tier_round_trips_a_value() {
        let (_dir, store) = temp_graph_store();
        let mut cache = EvalCache::new().with_graph_store(store);
        assert!(cache.has_graph_store());

        let key = CacheKey {
            source_hash: sha256_hex(b"some source"),
            lock_hash: Some(sha256_hex(b"some lock")),
        };
        let value = CachedValue {
            value_json: r#"{"answer":42}"#.to_string(),
            timestamp: 1_700_000_000,
        };

        cache.put(key.clone(), value.clone());
        let got = cache.get(&key).expect("memory tier hits");
        assert_eq!(got.value_json, value.value_json);
    }

    #[test]
    fn graph_store_tier_survives_fresh_cache_instance() {
        let (_dir, store) = temp_graph_store();
        let key = CacheKey {
            source_hash: sha256_hex(b"persist me"),
            lock_hash: None,
        };
        let value = CachedValue {
            value_json: r#""persisted""#.to_string(),
            timestamp: 42,
        };

        // First cache writes; drops.
        {
            let mut c = EvalCache::new().with_graph_store(store.clone());
            c.put(key.clone(), value.clone());
        }

        // Fresh cache, same GraphStore — must promote on first read.
        let mut c2 = EvalCache::new().with_graph_store(store);
        let got = c2.get(&key).expect("graph_store tier hits");
        assert_eq!(got.value_json, value.value_json);
        // Promotion: next lookup must hit memory (no GraphStore round-trip).
        let again = c2.get(&key).expect("memory promotion");
        assert_eq!(again.value_json, value.value_json);
    }

    #[test]
    fn graph_store_tier_isolates_by_cache_key() {
        let (_dir, store) = temp_graph_store();
        let mut cache = EvalCache::new().with_graph_store(store);

        let k_a = CacheKey {
            source_hash: sha256_hex(b"file a"),
            lock_hash: None,
        };
        let k_b = CacheKey {
            source_hash: sha256_hex(b"file b"),
            lock_hash: None,
        };

        cache.put(
            k_a.clone(),
            CachedValue {
                value_json: "A".to_string(),
                timestamp: 1,
            },
        );

        // Second key must miss — domain-separated lookup hash must
        // not collide.
        assert!(cache.get(&k_b).is_none());
        assert!(cache.get(&k_a).is_some());
    }

    #[test]
    fn graph_store_tier_disabled_when_cache_disabled() {
        let (_dir, store) = temp_graph_store();
        let mut cache = EvalCache::disabled().with_graph_store(store);
        let key = CacheKey {
            source_hash: "x".to_string(),
            lock_hash: None,
        };
        cache.put(
            key.clone(),
            CachedValue {
                value_json: "y".to_string(),
                timestamp: 0,
            },
        );
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn all_three_tiers_stack_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("eval-cache.json");
        let (_gdir, store) = temp_graph_store();

        let key = CacheKey {
            source_hash: sha256_hex(b"triple-tier source"),
            lock_hash: None,
        };
        let value = CachedValue {
            value_json: r#""triple-tier""#.to_string(),
            timestamp: 99,
        };

        // First cache writes through all three tiers.
        {
            let mut c = EvalCache::with_all_tiers(db_path.clone(), store.clone());
            c.put(key.clone(), value.clone());
        }

        // Fresh cache pointed at the same JSON file (no GraphStore)
        // must still hit (Tier 2 — JSON persistence preserved).
        {
            let mut c = EvalCache::with_persistent(db_path.clone());
            assert!(c.get(&key).is_some(), "tier 2 (JSON) must still serve");
        }

        // Fresh cache pointed at the GraphStore only must also hit
        // (Tier 3 — fleet-shared persistence preserved).
        {
            let mut c = EvalCache::new().with_graph_store(store);
            assert!(c.get(&key).is_some(), "tier 3 (GraphStore) must still serve");
        }
    }

    #[test]
    fn sha256_hex_deterministic() {
        let a = sha256_hex(b"hello");
        let b = sha256_hex(b"hello");
        assert_eq!(a, b);
        assert_ne!(a, sha256_hex(b"world"));
    }

    #[test]
    fn now_timestamp_reasonable() {
        let ts = now_timestamp();
        // Should be after 2020 and before 2100
        assert!(ts > 1_577_836_800);
        assert!(ts < 4_102_444_800);
    }

    #[test]
    fn len_and_is_empty() {
        let mut cache = EvalCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
        cache.put(
            CacheKey { source_hash: "x".to_string(), lock_hash: None },
            CachedValue { value_json: "1".to_string(), timestamp: 1 },
        );
        assert!(!cache.is_empty());
        assert_eq!(cache.len(), 1);
    }
}
