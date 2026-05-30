//! `GraphStore` — the public entry point.
//!
//! ## Wire model
//!
//! Two-tier:
//!
//! 1. **Blob** — the actual rkyv archive bytes, written to
//!    `<root>/blobs/<kind>/<aa>/<bb>/<hash>.rkyv`, mmap'd on read.
//! 2. **Index** — a redb B+ tree at `<root>/index.redb` mapping the
//!    33-byte key `[kind_tag : u8, hash : 32 bytes]` to an [`IndexEntry`]
//!    holding bookkeeping (blob length, created-at nanos). The entry is
//!    bincode-style packed so a single `as` cast lifts it from
//!    `&[u8]` — no per-row deserialization cost.
//!
//! Writes are atomic at the redb commit boundary: a blob is written to
//! a `.tmp` sibling, fsync'd, renamed into place, *then* the index entry
//! is inserted in a single redb transaction. A crash anywhere on the
//! path leaves either no entry (blob may be orphaned, picked up by GC)
//! or a complete entry pointing at a complete blob — never a half-state.
//!
//! Reads take a redb read transaction (cheap MVCC snapshot, doesn't
//! block writers), look up the entry, and `mmap` the file. Callers can
//! either trust the local hash they just computed (`get`) or pay the
//! bytecheck pass on substituter-pulled bytes (`get_validated`).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use memmap2::Mmap;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use tracing::{debug, trace};

use crate::content_address::GraphHash;
use crate::error::{Error, Result};
use crate::layout::{GraphKind, StoreLayout};

/// redb table: 33-byte composite key → 24-byte packed index entry.
/// Key layout: `[kind_tag : u8, hash : 32 bytes]`.
/// Value layout: `[len : u64 LE, created_unix_nanos : u128 LE]` truncated
/// to 24 bytes (so the table is a flat memcpy on both sides).
const INDEX_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("graph-index-v1");

/// 24 bytes: u64 length + u128 created-at nanos.
const ENTRY_PACKED_LEN: usize = 24;

/// Bookkeeping payload stored alongside every blob's index entry.
/// Kept tiny so the redb pages stay dense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    /// Length of the blob in bytes (matches the on-disk file size).
    pub blob_len: u64,
    /// Wall-clock instant the entry was created, in unix nanoseconds.
    /// Used by GC to evict old entries; not load-bearing for correctness.
    pub created_at_nanos: u128,
}

impl IndexEntry {
    fn pack(self) -> [u8; ENTRY_PACKED_LEN] {
        let mut out = [0u8; ENTRY_PACKED_LEN];
        out[0..8].copy_from_slice(&self.blob_len.to_le_bytes());
        out[8..24].copy_from_slice(&self.created_at_nanos.to_le_bytes());
        out
    }

    fn unpack(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != ENTRY_PACKED_LEN {
            return Err(Error::Storage(redb::StorageError::Corrupted(format!(
                "index entry has wrong length: expected {} got {}",
                ENTRY_PACKED_LEN,
                bytes.len()
            ))));
        }
        let mut len_buf = [0u8; 8];
        len_buf.copy_from_slice(&bytes[0..8]);
        let mut ts_buf = [0u8; 16];
        ts_buf.copy_from_slice(&bytes[8..24]);
        Ok(Self {
            blob_len: u64::from_le_bytes(len_buf),
            created_at_nanos: u128::from_le_bytes(ts_buf),
        })
    }
}

fn index_key(kind: GraphKind, hash: GraphHash) -> [u8; 33] {
    let mut k = [0u8; 33];
    k[0] = kind.tag();
    k[1..33].copy_from_slice(hash.as_bytes());
    k
}

/// The store. Clone-able (the underlying [`redb::Database`] is held by
/// `Arc` and is safe to share across threads — redb is internally
/// concurrent). Construct one per process; pass clones into worker tasks.
#[derive(Clone)]
pub struct GraphStore {
    layout: StoreLayout,
    db: Arc<Database>,
}

impl GraphStore {
    /// Open or create a store at `root`. Creates the root, blobs
    /// subtree, and redb index on first call. Idempotent.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let layout = StoreLayout::at(root.into());
        fs::create_dir_all(layout.root()).map_err(|e| Error::io(layout.root(), e))?;
        fs::create_dir_all(layout.root().join("blobs"))
            .map_err(|e| Error::io(layout.root().join("blobs"), e))?;

        let db = Database::create(layout.index_db_path())?;

        // Ensure the table exists so first-touch reads don't trip
        // `TableDoesNotExist`.
        let txn = db.begin_write()?;
        {
            let _t = txn.open_table(INDEX_TABLE)?;
        }
        txn.commit()?;

        Ok(Self {
            layout,
            db: Arc::new(db),
        })
    }

    /// Read-only view of the layout (paths, kinds). Useful for testing
    /// and for GC sweeps that walk the filesystem directly.
    #[must_use]
    pub fn layout(&self) -> &StoreLayout {
        &self.layout
    }

    /// Write a blob. Atomic: blob lands on disk + fsyncs *before* the
    /// index transaction commits. Idempotent on the same `(kind, hash,
    /// bytes)` triple. If the hash doesn't match the bytes the call
    /// fails with [`Error::HashMismatch`] — protects against caller
    /// mistakes upstream.
    pub fn put(&self, kind: GraphKind, hash: GraphHash, bytes: &[u8]) -> Result<()> {
        let actual = GraphHash::of(bytes);
        if actual != hash {
            return Err(Error::HashMismatch {
                expected: hash,
                actual,
            });
        }

        let final_path = self.layout.blob_path(kind, hash);
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        // Fast-path: blob already on disk (idempotent re-put). Just
        // make sure the index entry exists.
        if final_path.exists() {
            trace!(target: "sui-graph-store", "blob already present, refreshing index entry only");
            self.upsert_index(kind, hash, bytes.len() as u64)?;
            return Ok(());
        }

        let tmp_path = final_path.with_extension("rkyv.tmp");
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)
                .map_err(|e| Error::io(&tmp_path, e))?;
            f.write_all(bytes).map_err(|e| Error::io(&tmp_path, e))?;
            f.sync_all().map_err(|e| Error::io(&tmp_path, e))?;
        }

        fs::rename(&tmp_path, &final_path).map_err(|e| Error::io(&final_path, e))?;
        self.upsert_index(kind, hash, bytes.len() as u64)?;

        debug!(
            target: "sui-graph-store",
            kind = %kind,
            hash = %hash,
            len = bytes.len(),
            "stored blob"
        );
        Ok(())
    }

    /// Write a blob under a **query-derived lookup key** (not the
    /// BLAKE3 of the bytes). Skips the content-hash validation that
    /// [`Self::put`] enforces.
    ///
    /// Use only when the caller has a deterministic mapping from a
    /// non-content query (e.g. an eval-cache `(source_hash, lock_hash)`
    /// tuple) to a lookup hash, and the stored bytes are themselves
    /// not the BLAKE3 preimage of that hash. The lookup-hash space
    /// shares the same redb table as content-addressed entries, so
    /// callers MUST ensure their query-derived hashes can't collide
    /// with arbitrary content (the usual trick: domain-separate by
    /// hashing `"my-tier::v1::" + serialized_query`).
    ///
    /// Most callers should prefer [`Self::put`].
    pub fn put_unchecked(
        &self,
        kind: GraphKind,
        lookup_hash: GraphHash,
        bytes: &[u8],
    ) -> Result<()> {
        let final_path = self.layout.blob_path(kind, lookup_hash);
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        let tmp_path = final_path.with_extension("rkyv.tmp");
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)
                .map_err(|e| Error::io(&tmp_path, e))?;
            f.write_all(bytes).map_err(|e| Error::io(&tmp_path, e))?;
            f.sync_all().map_err(|e| Error::io(&tmp_path, e))?;
        }
        fs::rename(&tmp_path, &final_path).map_err(|e| Error::io(&final_path, e))?;
        self.upsert_index(kind, lookup_hash, bytes.len() as u64)?;

        debug!(
            target: "sui-graph-store",
            kind = %kind,
            lookup = %lookup_hash,
            len = bytes.len(),
            "stored blob via put_unchecked"
        );
        Ok(())
    }

    fn upsert_index(&self, kind: GraphKind, hash: GraphHash, blob_len: u64) -> Result<()> {
        let entry = IndexEntry {
            blob_len,
            created_at_nanos: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        };
        let key = index_key(kind, hash);
        let packed = entry.pack();
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(INDEX_TABLE)?;
            table.insert(&key[..], &packed[..])?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Check whether the index knows about `(kind, hash)`. Cheap; does
    /// not touch the filesystem.
    pub fn contains(&self, kind: GraphKind, hash: GraphHash) -> Result<bool> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(INDEX_TABLE)?;
        let key = index_key(kind, hash);
        Ok(table.get(&key[..])?.is_some())
    }

    /// Look up an entry's bookkeeping without reading the blob. Used by
    /// GC and inspection tools.
    pub fn lookup(&self, kind: GraphKind, hash: GraphHash) -> Result<IndexEntry> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(INDEX_TABLE)?;
        let key = index_key(kind, hash);
        let raw = table
            .get(&key[..])?
            .ok_or(Error::NotFound { hash })?;
        IndexEntry::unpack(raw.value())
    }

    /// `mmap` the blob bytes for `(kind, hash)`. Caller is trusted to
    /// have already verified the hash (e.g. they just computed it). Use
    /// [`Self::get_validated`] for substituter-pulled bytes that must
    /// also pass rkyv's bytecheck pass.
    pub fn get(&self, kind: GraphKind, hash: GraphHash) -> Result<MappedBlob> {
        let entry = self.lookup(kind, hash)?;
        let path = self.layout.blob_path(kind, hash);
        let file = File::open(&path).map_err(|e| Error::io(&path, e))?;
        // SAFETY: redb-tracked path; the file is locally written by
        // `put` and not mutated afterward (overwrite is via rename of
        // a fresh tmp file). mmap of immutable bytes is sound.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| Error::io(&path, e))?;

        if mmap.len() as u64 != entry.blob_len {
            return Err(Error::Storage(redb::StorageError::Corrupted(format!(
                "blob length mismatch: index says {} bytes, file has {} bytes",
                entry.blob_len,
                mmap.len()
            ))));
        }

        Ok(MappedBlob { mmap, path })
    }

    /// Same as [`Self::get`] but additionally recomputes the BLAKE3 of
    /// the bytes and rejects on mismatch. Pay this cost for any blob
    /// that arrived from an untrusted source (substituter pull,
    /// cross-host fleet transfer).
    pub fn get_validated(&self, kind: GraphKind, hash: GraphHash) -> Result<MappedBlob> {
        let blob = self.get(kind, hash)?;
        let actual = GraphHash::of(&blob);
        if actual != hash {
            return Err(Error::HashMismatch {
                expected: hash,
                actual,
            });
        }
        Ok(blob)
    }

    /// Iterate every `(kind, hash)` pair in the index. Backed by a
    /// redb read transaction so the snapshot is consistent at call
    /// time. Used by GC and inspection.
    pub fn iter_keys(&self) -> Result<Vec<(GraphKind, GraphHash)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(INDEX_TABLE)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (k, _v) = entry?;
            let key = k.value();
            if key.len() == 33 {
                if let Some(kind) = GraphKind::from_tag(key[0]) {
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(&key[1..33]);
                    out.push((kind, GraphHash(hash)));
                }
            }
        }
        Ok(out)
    }

    /// Number of entries in the index. Cheap; one read txn + table scan.
    pub fn len(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(INDEX_TABLE)?;
        Ok(table.len()?)
    }

    /// True when the store has no entries.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

/// `mmap`-backed handle to a blob's bytes. Derefs to `&[u8]` so callers
/// can either:
///
/// * Hand the slice to `rkyv::access::<ArchivedT, _>()` (validated)
/// * Hand the slice to `rkyv::access_unchecked::<ArchivedT>()` (trusted)
///
/// Hold the `MappedBlob` for as long as you want the reference to live;
/// the `mmap` is released when this drops.
pub struct MappedBlob {
    mmap: Mmap,
    path: PathBuf,
}

impl std::fmt::Debug for MappedBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedBlob")
            .field("path", &self.path)
            .field("len", &self.mmap.len())
            .finish()
    }
}

impl MappedBlob {
    /// Filesystem path of the backing file. For diagnostics only.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<[u8]> for MappedBlob {
    fn as_ref(&self) -> &[u8] {
        &self.mmap
    }
}

impl std::ops::Deref for MappedBlob {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &self.mmap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    fn new_store() -> (tempfile::TempDir, GraphStore) {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(dir.path().to_path_buf()).unwrap();
        (dir, store)
    }

    #[test]
    fn empty_store_reports_zero() {
        let (_dir, store) = new_store();
        assert!(store.is_empty().unwrap());
        assert_eq!(store.len().unwrap(), 0);
    }

    #[test]
    fn put_then_get_roundtrip() {
        let (_dir, store) = new_store();
        let payload = b"hello sui graph store".as_slice();
        let h = GraphHash::of(payload);

        store.put(GraphKind::Lockfile, h, payload).unwrap();
        assert!(store.contains(GraphKind::Lockfile, h).unwrap());

        let blob = store.get(GraphKind::Lockfile, h).unwrap();
        assert_eq!(&*blob, payload);
    }

    #[test]
    fn put_validates_caller_hash() {
        let (_dir, store) = new_store();
        let payload = b"correct payload".as_slice();
        let wrong = GraphHash::of(b"different payload");
        let err = store.put(GraphKind::Ast, wrong, payload).unwrap_err();
        assert!(matches!(err, Error::HashMismatch { .. }));
    }

    #[test]
    fn get_missing_returns_not_found() {
        let (_dir, store) = new_store();
        let h = GraphHash::of(b"nothing");
        let err = store.get(GraphKind::Module, h).unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }));
    }

    #[test]
    fn put_is_idempotent_on_same_bytes() {
        let (_dir, store) = new_store();
        let payload = b"idempotent".as_slice();
        let h = GraphHash::of(payload);
        store.put(GraphKind::Derivation, h, payload).unwrap();
        store.put(GraphKind::Derivation, h, payload).unwrap();
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn different_kinds_with_same_hash_coexist() {
        let (_dir, store) = new_store();
        let payload = b"same bytes, different graph kinds".as_slice();
        let h = GraphHash::of(payload);
        store.put(GraphKind::Lockfile, h, payload).unwrap();
        store.put(GraphKind::Ast, h, payload).unwrap();
        assert_eq!(store.len().unwrap(), 2);
        let keys = store.iter_keys().unwrap();
        assert!(keys.contains(&(GraphKind::Lockfile, h)));
        assert!(keys.contains(&(GraphKind::Ast, h)));
    }

    #[test]
    fn get_validated_rejects_corruption() {
        let (_dir, store) = new_store();
        let payload = b"valid payload".as_slice();
        let h = GraphHash::of(payload);
        store.put(GraphKind::Lockfile, h, payload).unwrap();

        // Corrupt the blob on disk to simulate bitrot / tampering.
        let path = store.layout.blob_path(GraphKind::Lockfile, h);
        std::fs::write(&path, b"tampered").unwrap();

        // Length mismatch trips before bytecheck — index entry says 13
        // bytes, file has 8.
        let err = store.get(GraphKind::Lockfile, h).unwrap_err();
        assert!(matches!(err, Error::Storage(_)));
    }

    #[test]
    fn iter_keys_returns_all_inserted() {
        let (_dir, store) = new_store();
        let mut inserted = Vec::new();
        for i in 0..10u32 {
            let payload = format!("entry {i}");
            let h = GraphHash::of(payload.as_bytes());
            store
                .put(GraphKind::EvalCacheEntry, h, payload.as_bytes())
                .unwrap();
            inserted.push((GraphKind::EvalCacheEntry, h));
        }
        let mut found = store.iter_keys().unwrap();
        found.sort();
        inserted.sort();
        assert_eq!(found, inserted);
    }

    #[test]
    fn store_clone_is_a_view_of_same_data() {
        let (_dir, store_a) = new_store();
        let store_b = store_a.clone();
        let payload = b"shared via clone".as_slice();
        let h = GraphHash::of(payload);
        store_a.put(GraphKind::Lockfile, h, payload).unwrap();
        assert!(store_b.contains(GraphKind::Lockfile, h).unwrap());
    }
}
