//! Content-addressed evaluation cache.
//!
//! Caches forced Nix expression results by hashing the source content
//! and (optionally) the flake lock. Lookup is O(1) via `HashMap`.
//! Persistence is best-effort JSON to `~/.cache/sui/eval-cache.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur during cache operations.
#[derive(Error, Debug)]
pub enum CacheError {
    /// Filesystem I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON (de)serialization failure.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// A cache key combining source hash and optional lock hash.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheKey {
    /// BLAKE3 hash of the source expression.
    pub source_hash: String,
    /// BLAKE3 hash of `flake.lock` (if present).
    pub lock_hash: Option<String>,
}

/// A cached evaluation result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedValue {
    /// JSON representation of the evaluated value.
    pub value_json: String,
    /// Unix timestamp when the value was cached.
    pub timestamp: i64,
}

/// Content-addressed evaluation cache.
///
/// In-memory `HashMap` backed by optional persistent JSON file.
pub struct EvalCache {
    memory: HashMap<CacheKey, CachedValue>,
    db_path: Option<PathBuf>,
    enabled: bool,
}

impl EvalCache {
    /// Create a new cache. If `persist` is true, loads/saves to
    /// `~/.cache/sui/eval-cache.json`.
    #[must_use]
    pub fn new(persist: bool) -> Self {
        let db_path = if persist {
            dirs_next::cache_dir().map(|d| d.join("sui").join("eval-cache.json"))
        } else {
            None
        };

        let memory = db_path
            .as_ref()
            .map_or_else(HashMap::new, |p| Self::load_from(p));

        Self {
            memory,
            db_path,
            enabled: true,
        }
    }

    /// Create a cache with a custom database path (for testing).
    #[must_use]
    pub fn with_path(path: PathBuf) -> Self {
        let memory = Self::load_from(&path);

        Self {
            memory,
            db_path: Some(path),
            enabled: true,
        }
    }

    /// Compute a cache key for a file on disk.
    ///
    /// # Errors
    ///
    /// Returns `CacheError::Io` if the file (or lock file) cannot be read.
    pub fn key_for_file(path: &Path, lock_path: Option<&Path>) -> Result<CacheKey, CacheError> {
        let source = std::fs::read(path)?;
        let source_hash = blake3::hash(&source).to_hex().to_string();

        let lock_hash = if let Some(lp) = lock_path {
            let lock = std::fs::read(lp)?;
            Some(blake3::hash(&lock).to_hex().to_string())
        } else {
            None
        };

        Ok(CacheKey {
            source_hash,
            lock_hash,
        })
    }

    /// Compute a cache key from a string expression.
    #[must_use]
    pub fn key_for_expr(expr: &str) -> CacheKey {
        CacheKey {
            source_hash: blake3::hash(expr.as_bytes()).to_hex().to_string(),
            lock_hash: None,
        }
    }

    /// Look up a cached value.
    #[must_use]
    pub fn get(&self, key: &CacheKey) -> Option<&CachedValue> {
        if !self.enabled {
            return None;
        }
        self.memory.get(key)
    }

    /// Store a value in the cache.
    pub fn put(&mut self, key: CacheKey, value: CachedValue) {
        if !self.enabled {
            return;
        }
        self.memory.insert(key, value);
        self.persist();
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.memory.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.memory.is_empty()
    }

    /// Clear all cached entries.
    pub fn clear(&mut self) {
        self.memory.clear();
        self.persist();
    }

    /// Load entries from a JSON file into the in-memory map.
    fn load_from(path: &Path) -> HashMap<CacheKey, CachedValue> {
        let Ok(data) = std::fs::read_to_string(path) else {
            return HashMap::new();
        };
        let Ok(entries): Result<Vec<(CacheKey, CachedValue)>, _> = serde_json::from_str(&data)
        else {
            return HashMap::new();
        };
        entries.into_iter().collect()
    }

    /// Best-effort persist to disk.
    fn persist(&self) {
        if let Some(ref path) = self.db_path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let entries: Vec<(&CacheKey, &CachedValue)> = self.memory.iter().collect();
            if let Ok(json) = serde_json::to_string(&entries) {
                let _ = std::fs::write(path, json);
            }
        }
    }
}

impl Default for EvalCache {
    fn default() -> Self {
        Self::new(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cache() {
        let cache = EvalCache::default();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn put_and_get() {
        let mut cache = EvalCache::default();
        let key = CacheKey {
            source_hash: "abc".into(),
            lock_hash: None,
        };
        let val = CachedValue {
            value_json: "42".into(),
            timestamp: 0,
        };
        cache.put(key.clone(), val);
        assert_eq!(cache.get(&key).unwrap().value_json, "42");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn key_for_expr_deterministic() {
        let k1 = EvalCache::key_for_expr("1 + 2");
        let k2 = EvalCache::key_for_expr("1 + 2");
        let k3 = EvalCache::key_for_expr("1 + 3");
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
    }

    #[test]
    fn different_lock_hash_is_different_key() {
        let k1 = CacheKey {
            source_hash: "same".into(),
            lock_hash: Some("lock_a".into()),
        };
        let k2 = CacheKey {
            source_hash: "same".into(),
            lock_hash: Some("lock_b".into()),
        };
        assert_ne!(k1, k2);
    }

    #[test]
    fn clear_removes_all() {
        let mut cache = EvalCache::default();
        cache.put(
            CacheKey {
                source_hash: "a".into(),
                lock_hash: None,
            },
            CachedValue {
                value_json: "1".into(),
                timestamp: 0,
            },
        );
        cache.put(
            CacheKey {
                source_hash: "b".into(),
                lock_hash: None,
            },
            CachedValue {
                value_json: "2".into(),
                timestamp: 0,
            },
        );
        assert_eq!(cache.len(), 2);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn disabled_cache_returns_none() {
        let mut cache = EvalCache::default();
        cache.enabled = false;
        let key = CacheKey {
            source_hash: "x".into(),
            lock_hash: None,
        };
        cache.put(
            key.clone(),
            CachedValue {
                value_json: "v".into(),
                timestamp: 0,
            },
        );
        assert!(cache.get(&key).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn persistence_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("cache.json");

        let key = CacheKey {
            source_hash: "test".into(),
            lock_hash: Some("lock123".into()),
        };

        // Write phase.
        {
            let mut cache = EvalCache::with_path(path.clone());
            cache.put(
                key.clone(),
                CachedValue {
                    value_json: r#""hello""#.into(),
                    timestamp: 123,
                },
            );
        }

        // Read-back phase.
        {
            let cache = EvalCache::with_path(path);
            let cached = cache.get(&key).expect("should load persisted entry");
            assert_eq!(cached.value_json, r#""hello""#);
            assert_eq!(cached.timestamp, 123);
        }
    }

    #[test]
    fn key_for_file_works() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("expr.nix");
        let lock = dir.path().join("flake.lock");

        std::fs::write(&src, "builtins.add 1 2").unwrap();
        std::fs::write(&lock, r#"{"nodes":{}}"#).unwrap();

        let k1 = EvalCache::key_for_file(&src, Some(&lock)).unwrap();
        let k2 = EvalCache::key_for_file(&src, Some(&lock)).unwrap();
        assert_eq!(k1, k2);
        assert!(k1.lock_hash.is_some());

        // Without lock.
        let k3 = EvalCache::key_for_file(&src, None).unwrap();
        assert!(k3.lock_hash.is_none());
        assert_eq!(k1.source_hash, k3.source_hash);
    }

    #[test]
    fn key_for_file_missing_returns_error() {
        let result = EvalCache::key_for_file(Path::new("/nonexistent/file.nix"), None);
        assert!(result.is_err());
    }

    #[test]
    fn overwrite_existing_key() {
        let mut cache = EvalCache::default();
        let key = CacheKey {
            source_hash: "same".into(),
            lock_hash: None,
        };
        cache.put(
            key.clone(),
            CachedValue {
                value_json: "old".into(),
                timestamp: 1,
            },
        );
        cache.put(
            key.clone(),
            CachedValue {
                value_json: "new".into(),
                timestamp: 2,
            },
        );
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&key).unwrap().value_json, "new");
        assert_eq!(cache.get(&key).unwrap().timestamp, 2);
    }
}
