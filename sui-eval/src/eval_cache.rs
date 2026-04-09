//! Content-addressed evaluation cache.
//!
//! Maps `(source_hash, lock_hash)` pairs to previously evaluated results,
//! skipping redundant evaluation when inputs haven't changed.
//!
//! The cache operates in two tiers:
//!   1. **In-memory** (`HashMap`) for the current session — instant lookups.
//!   2. **Persistent** (JSON file at `~/.cache/sui/eval-cache.json`) — survives
//!      across invocations.
//!
//! Only JSON-serializable values are cached (no lambdas, no thunks).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

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
    /// Whether this cache is enabled (can be disabled via CLI flag).
    enabled: bool,
}

impl EvalCache {
    /// Create a new in-memory-only cache.
    pub fn new() -> Self {
        Self {
            memory: HashMap::new(),
            db_path: None,
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
            enabled: false,
        }
    }

    /// Whether the cache is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Look up a cached result.
    pub fn get(&self, key: &CacheKey) -> Option<&CachedValue> {
        if !self.enabled {
            return None;
        }
        self.memory.get(key)
    }

    /// Store a result in the cache.
    pub fn put(&mut self, key: CacheKey, value: CachedValue) {
        if !self.enabled {
            return;
        }
        self.memory.insert(key, value);
        // Best-effort persist to disk.
        if let Some(ref path) = self.db_path {
            let _ = Self::save_to_disk(path, &self.memory);
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
        let cache = EvalCache::new();
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
            let c = EvalCache::with_persistent(db.clone());
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
