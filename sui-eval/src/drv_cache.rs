//! Content-addressed derivation path cache.
//!
//! Maps `(lock_hash, source_hash, attr_path) → (drvPath, outPath)` using redb.
//! Survives process restarts. Designed to avoid full nixpkgs evaluation when
//! the result is already known for a given `flake.lock` + `flake.nix` + attribute.
//!
//! The cache file lives at `~/.cache/sui/drv-cache.redb` by default.

use std::path::{Path, PathBuf};

use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use sha2::{Digest, Sha256};
use tracing::{debug, info};

/// redb table: key = `"{lock_hash}:{source_hash}:{attr_path}"`, value = `"{drv_path}\n{out_path}"`.
const DRV_TABLE: TableDefinition<&str, &str> = TableDefinition::new("drv_paths");

/// Errors from the derivation cache.
#[derive(Debug, thiserror::Error)]
pub enum DrvCacheError {
    #[error("redb error: {0}")]
    Db(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A cached derivation path entry.
#[derive(Debug, Clone)]
pub struct DrvCacheEntry {
    pub drv_path: String,
    pub out_path: String,
}

/// redb-backed derivation path cache.
pub struct DrvCache {
    db: Database,
}

impl DrvCache {
    /// Open or create the cache at the given path.
    pub fn open(path: &Path) -> Result<Self, DrvCacheError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)
            .map_err(|e| DrvCacheError::Db(format!("open: {e}")))?;

        // Ensure the table exists.
        let txn = db
            .begin_write()
            .map_err(|e| DrvCacheError::Db(format!("txn: {e}")))?;
        { let _ = txn.open_table(DRV_TABLE); }
        txn.commit()
            .map_err(|e| DrvCacheError::Db(format!("commit: {e}")))?;

        info!(path = %path.display(), "Opened derivation cache");
        Ok(Self { db })
    }

    /// Default cache path: `~/.cache/sui/drv-cache.redb`.
    pub fn default_path() -> PathBuf {
        let base = std::env::var("XDG_CACHE_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache"))
            })
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("sui").join("drv-cache.redb")
    }

    /// Look up a cached derivation.
    pub fn get(
        &self,
        lock_hash: &str,
        source_hash: &str,
        attr_path: &str,
    ) -> Option<DrvCacheEntry> {
        let key = format!("{lock_hash}:{source_hash}:{attr_path}");
        let txn = self.db.begin_read().ok()?;
        let table = txn.open_table(DRV_TABLE).ok()?;
        let value = table.get(key.as_str()).ok()??;
        let s = value.value();
        let (drv, out) = s.split_once('\n')?;
        debug!(attr_path, "drv cache hit");
        Some(DrvCacheEntry {
            drv_path: drv.to_string(),
            out_path: out.to_string(),
        })
    }

    /// Store a derivation mapping.
    pub fn put(
        &self,
        lock_hash: &str,
        source_hash: &str,
        attr_path: &str,
        entry: &DrvCacheEntry,
    ) -> Result<(), DrvCacheError> {
        let key = format!("{lock_hash}:{source_hash}:{attr_path}");
        let value = format!("{}\n{}", entry.drv_path, entry.out_path);
        let txn = self.db.begin_write()
            .map_err(|e| DrvCacheError::Db(format!("txn: {e}")))?;
        {
            let mut table = txn.open_table(DRV_TABLE)
                .map_err(|e| DrvCacheError::Db(format!("table: {e}")))?;
            table.insert(key.as_str(), value.as_str())
                .map_err(|e| DrvCacheError::Db(format!("insert: {e}")))?;
        }
        txn.commit()
            .map_err(|e| DrvCacheError::Db(format!("commit: {e}")))?;
        debug!(attr_path, "drv cache put");
        Ok(())
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        let Ok(txn) = self.db.begin_read() else { return 0 };
        let Ok(table) = txn.open_table(DRV_TABLE) else { return 0 };
        table.len().unwrap_or(0) as usize
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// SHA-256 hex digest of file content — used for cache keys.
    pub fn hash_bytes(content: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        format!("{:x}", hasher.finalize())
    }
}

// ── Thread-local singleton ────────────────────────────────────

thread_local! {
    static GLOBAL_CACHE: std::cell::RefCell<Option<DrvCache>> = const { std::cell::RefCell::new(None) };
}

/// Initialize the global derivation cache (call once at startup).
pub fn init_global_cache() {
    GLOBAL_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.is_none() {
            let path = DrvCache::default_path();
            match DrvCache::open(&path) {
                Ok(c) => {
                    info!(entries = c.len(), "Derivation cache initialized");
                    *cache = Some(c);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to open derivation cache (continuing without)");
                }
            }
        }
    });
}

/// Access the global cache for a lookup.
pub fn with_cache<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&DrvCache) -> Option<R>,
{
    GLOBAL_CACHE.with(|cell| {
        let borrow = cell.borrow();
        borrow.as_ref().and_then(f)
    })
}

/// Access the global cache for a write.
pub fn with_cache_mut<F>(f: F)
where
    F: FnOnce(&DrvCache),
{
    GLOBAL_CACHE.with(|cell| {
        let borrow = cell.borrow();
        if let Some(cache) = borrow.as_ref() {
            f(cache);
        }
    });
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DrvCache::open(&tmp.path().join("test.redb")).unwrap();

        assert!(cache.get("lock1", "src1", "packages.x86_64-linux.default").is_none());

        cache
            .put("lock1", "src1", "packages.x86_64-linux.default", &DrvCacheEntry {
                drv_path: "/nix/store/abc-hello.drv".to_string(),
                out_path: "/nix/store/xyz-hello-2.10".to_string(),
            })
            .unwrap();

        let entry = cache.get("lock1", "src1", "packages.x86_64-linux.default").unwrap();
        assert_eq!(entry.drv_path, "/nix/store/abc-hello.drv");
        assert_eq!(entry.out_path, "/nix/store/xyz-hello-2.10");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn different_keys_no_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DrvCache::open(&tmp.path().join("test.redb")).unwrap();

        cache.put("lock1", "src1", "attr.a", &DrvCacheEntry {
            drv_path: "/nix/store/a.drv".into(),
            out_path: "/nix/store/a".into(),
        }).unwrap();

        cache.put("lock1", "src1", "attr.b", &DrvCacheEntry {
            drv_path: "/nix/store/b.drv".into(),
            out_path: "/nix/store/b".into(),
        }).unwrap();

        cache.put("lock2", "src1", "attr.a", &DrvCacheEntry {
            drv_path: "/nix/store/c.drv".into(),
            out_path: "/nix/store/c".into(),
        }).unwrap();

        assert_eq!(cache.get("lock1", "src1", "attr.a").unwrap().out_path, "/nix/store/a");
        assert_eq!(cache.get("lock1", "src1", "attr.b").unwrap().out_path, "/nix/store/b");
        assert_eq!(cache.get("lock2", "src1", "attr.a").unwrap().out_path, "/nix/store/c");
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn hash_bytes_deterministic() {
        let h1 = DrvCache::hash_bytes(b"hello world");
        let h2 = DrvCache::hash_bytes(b"hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex = 64 chars
    }
}
