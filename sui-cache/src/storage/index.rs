//! redb metadata index — ephemeral local cache for S3 narinfo lookups.
//!
//! The index is disposable: when a sui pod scales to zero and back,
//! the redb file is gone. On cold start, the index rebuilds from S3
//! narinfo listings. This makes the index fully breathable — zero state
//! between scale events, S3 is the durable source of truth.

use std::path::Path;

use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use tracing::{debug, info};

use crate::CacheError;

// Table definitions
const NARINFO_TABLE: TableDefinition<&str, (u64, u64)> = TableDefinition::new("narinfos");
// Key: 32-char hash, Value: (timestamp_secs, nar_size)

const STORE_PATH_TABLE: TableDefinition<&str, &str> = TableDefinition::new("store_paths");
// Key: store path basename, Value: 32-char hash

/// Ephemeral metadata index backed by redb.
pub struct StorageIndex {
    db: Database,
}

impl StorageIndex {
    /// Open or create the index at the given path.
    pub fn open(path: &Path) -> Result<Self, CacheError> {
        let db = Database::create(path)
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb open: {e}"))))?;

        // Ensure tables exist
        let write_txn = db
            .begin_write()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb txn: {e}"))))?;
        {
            let _ = write_txn.open_table(NARINFO_TABLE);
            let _ = write_txn.open_table(STORE_PATH_TABLE);
        }
        write_txn
            .commit()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb commit: {e}"))))?;

        info!(path = %path.display(), "Opened redb index");
        Ok(Self { db })
    }

    /// Record a narinfo in the index.
    pub fn index_narinfo(&self, hash: &str, nar_size: u64) -> Result<(), CacheError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let write_txn = self.db.begin_write()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb txn: {e}"))))?;
        {
            let mut table = write_txn.open_table(NARINFO_TABLE)
                .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb table: {e}"))))?;
            table.insert(hash, (now, nar_size))
                .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb insert: {e}"))))?;
        }
        write_txn.commit()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb commit: {e}"))))?;

        debug!(hash = %hash, nar_size, "Indexed narinfo");
        Ok(())
    }

    /// Record a store path → hash mapping.
    pub fn index_store_path(&self, store_path: &str, hash: &str) -> Result<(), CacheError> {
        let write_txn = self.db.begin_write()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb txn: {e}"))))?;
        {
            let mut table = write_txn.open_table(STORE_PATH_TABLE)
                .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb table: {e}"))))?;
            table.insert(store_path, hash)
                .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb insert: {e}"))))?;
        }
        write_txn.commit()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb commit: {e}"))))?;
        Ok(())
    }

    /// Check if a hash exists in the index.
    pub fn has_narinfo(&self, hash: &str) -> Result<bool, CacheError> {
        let read_txn = self.db.begin_read()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb txn: {e}"))))?;
        let table = read_txn.open_table(NARINFO_TABLE)
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb table: {e}"))))?;
        let exists = table.get(hash)
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb get: {e}"))))?
            .is_some();
        Ok(exists)
    }

    /// List all indexed hashes.
    pub fn list_hashes(&self) -> Result<Vec<String>, CacheError> {
        let read_txn = self.db.begin_read()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb txn: {e}"))))?;
        let table = read_txn.open_table(NARINFO_TABLE)
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb table: {e}"))))?;

        let mut hashes = Vec::new();
        let iter = table.iter()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb iter: {e}"))))?;
        for entry in iter {
            let (key, _) = entry
                .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb entry: {e}"))))?;
            hashes.push(key.value().to_string());
        }
        Ok(hashes)
    }

    /// Total number of indexed narinfos.
    pub fn count(&self) -> Result<u64, CacheError> {
        let read_txn = self.db.begin_read()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb txn: {e}"))))?;
        let table = read_txn.open_table(NARINFO_TABLE)
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb table: {e}"))))?;
        table.len()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb len: {e}"))))
    }

    /// Remove a hash from the index.
    pub fn remove(&self, hash: &str) -> Result<(), CacheError> {
        let write_txn = self.db.begin_write()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb txn: {e}"))))?;
        {
            let mut table = write_txn.open_table(NARINFO_TABLE)
                .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb table: {e}"))))?;
            let _ = table.remove(hash);
        }
        write_txn.commit()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("redb commit: {e}"))))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.redb");
        let idx = StorageIndex::open(&db_path).unwrap();

        // Index a narinfo
        idx.index_narinfo("abc123", 1024).unwrap();
        assert!(idx.has_narinfo("abc123").unwrap());
        assert!(!idx.has_narinfo("xyz789").unwrap());
        assert_eq!(idx.count().unwrap(), 1);

        // List
        let hashes = idx.list_hashes().unwrap();
        assert_eq!(hashes, vec!["abc123"]);

        // Remove
        idx.remove("abc123").unwrap();
        assert!(!idx.has_narinfo("abc123").unwrap());
        assert_eq!(idx.count().unwrap(), 0);
    }

    #[test]
    fn store_path_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.redb");
        let idx = StorageIndex::open(&db_path).unwrap();

        idx.index_store_path("abc123-hello", "abc123").unwrap();
    }
}
