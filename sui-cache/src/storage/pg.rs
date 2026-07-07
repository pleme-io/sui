//! Postgres-backed **L2 durable cache tier** `StorageBackend`.
//!
//! This is the shared, durable middle tier of the tiered super-cache resolver
//! (`Redis L1 → Postgres L2 → object L3`). Where [`RedisBackend`](super::RedisBackend)
//! is an ephemeral hot cache whose keys may vanish under `maxmemory` LRU, the
//! Postgres tier is **authoritative**: a narinfo/NAR written here survives a pod
//! roll, and [`PgStorageBackend::list_narinfos`] returns the *full* set of keys,
//! not a hot subset.
//!
//! # Two Postgres axes, one crate, do not confuse them
//!
//! There are two Postgres-backed content-addressed surfaces in the sui workspace,
//! on **different traits**:
//!
//! - **This** `PgStorageBackend` — a [`StorageBackend`] (the binary-**cache** blob
//!   axis: narinfo strings keyed by store-path hash, NAR blobs keyed by relative
//!   URL). It is the L2 tier of [`TieredBackend`](super::TieredBackend).
//! - [`sui_store::PgStore`] — a `sui_store::Store` (the durable **nix-store** axis:
//!   `StorePath → PathInfo` + NAR data, content-addressed by `GraphHash`). It is
//!   the sibling that the on-disk graph store migrates onto.
//!
//! Both are Postgres, both content-addressed, **different traits, different key
//! shapes**. This module is the *cache* one.
//!
//! # The connection seam (Environment / testability contract)
//!
//! [`PgStorageBackend`] is generic over [`PgCacheConn`] — the minimal typed
//! row-verb surface it needs (`select` / `upsert` / `delete` / `keys` over a typed
//! [`PgTable`]). Unit tests inject an in-memory mock; production injects
//! [`SqlxPgCacheConn`] (a real `sqlx` Postgres pool, behind the `postgres`
//! feature). The full L2 mapping — the two-table split, the content-addressed
//! keying, typed UTF-8 handling, the `delete` NAR-pattern fan-out — is proven
//! against the mock with **no live Postgres required**.

use async_trait::async_trait;

use super::StorageBackend;
use crate::CacheError;

/// The two logical tables the cache tier keeps: narinfo text and NAR blobs.
///
/// A typed discriminant (never a stringly-typed table name at a call site) so the
/// SQL for each table is chosen by an exhaustive `match` — a new table is a
/// non-exhaustive-match compile error, and a typo'd table name cannot exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgTable {
    /// narinfo metadata, keyed by the 32-char store-path hash.
    Narinfo,
    /// compressed NAR blobs, keyed by relative URL path (`nar/<hash>.nar.xz`).
    Nar,
}

impl PgTable {
    /// The physical table name (for diagnostics / the real adapter's DDL).
    #[must_use]
    pub const fn table_name(self) -> &'static str {
        match self {
            PgTable::Narinfo => "sui_cache_narinfo",
            PgTable::Nar => "sui_cache_nar",
        }
    }
}

/// The minimal typed Postgres row-verb surface [`PgStorageBackend`] depends on.
///
/// This is the injectable **Environment seam**: a real implementation
/// ([`SqlxPgCacheConn`], `postgres` feature) talks to a live Postgres pool; tests
/// substitute an in-memory mock. Keeping the surface this small means the whole L2
/// mapping is proven against a mock, and the only unmocked code is the thin
/// SQL translation.
#[async_trait]
pub trait PgCacheConn: Send + Sync {
    /// `SELECT value FROM <table> WHERE key = $1` — raw bytes, or `Ok(None)` on a
    /// missing row.
    async fn select(&self, table: PgTable, key: &str) -> Result<Option<Vec<u8>>, CacheError>;

    /// Upsert (`INSERT … ON CONFLICT (key) DO UPDATE`) — idempotent by key; a
    /// re-`put` of a content-addressed key overwrites with identical bytes.
    async fn upsert(&self, table: PgTable, key: &str, value: &[u8]) -> Result<(), CacheError>;

    /// `DELETE FROM <table> WHERE key = $1` — idempotent; deleting an absent key
    /// is `Ok(())`.
    async fn delete(&self, table: PgTable, key: &str) -> Result<(), CacheError>;

    /// `SELECT key FROM <table>` — the **authoritative** full key set (this is a
    /// durable tier, not a partial hot cache).
    async fn keys(&self, table: PgTable) -> Result<Vec<String>, CacheError>;

    /// `DELETE FROM <table>` — clear the whole table, returning the row count
    /// removed. The typed whole-store wipe primitive (the inverse of a warm
    /// push); reaches NAR rows a per-key `delete` cannot.
    async fn clear(&self, table: PgTable) -> Result<u64, CacheError>;
}

/// L2 durable cache tier: content-addressed key → value over Postgres, shared
/// across pods, survives a roll.
///
/// Generic over the [`PgCacheConn`] seam so it is fully testable against a mock.
pub struct PgStorageBackend<C: PgCacheConn> {
    conn: C,
}

impl<C: PgCacheConn> PgStorageBackend<C> {
    /// Wrap a [`PgCacheConn`].
    pub fn new(conn: C) -> Self {
        Self { conn }
    }

    /// Borrow the underlying connection (for composition / diagnostics).
    pub fn conn(&self) -> &C {
        &self.conn
    }
}

#[async_trait]
impl<C: PgCacheConn> StorageBackend for PgStorageBackend<C> {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, CacheError> {
        match self.conn.select(PgTable::Narinfo, hash).await? {
            Some(bytes) => {
                let text = String::from_utf8(bytes).map_err(|e| {
                    CacheError::NarInfo(format!("invalid utf-8 in pg narinfo {hash}: {e}"))
                })?;
                Ok(Some(text))
            }
            None => Ok(None),
        }
    }

    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), CacheError> {
        self.conn.upsert(PgTable::Narinfo, hash, content.as_bytes()).await
    }

    async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, CacheError> {
        self.conn.select(PgTable::Nar, path).await
    }

    async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), CacheError> {
        self.conn.upsert(PgTable::Nar, path, data).await
    }

    async fn delete(&self, hash: &str) -> Result<(), CacheError> {
        // narinfo keyed directly by hash.
        self.conn.delete(PgTable::Narinfo, hash).await?;
        // NAR blobs are keyed by relative URL; only the hash is in hand here, so —
        // mirroring `RedisBackend`/`S3Storage::delete` — best-effort-delete the
        // common NAR path patterns. `delete` is idempotent, so absent keys are
        // harmless.
        for ext in ["nar.xz", "nar.zst", "nar"] {
            self.conn.delete(PgTable::Nar, &format!("nar/{hash}.{ext}")).await?;
        }
        Ok(())
    }

    async fn list_narinfos(&self) -> Result<Vec<String>, CacheError> {
        self.conn.keys(PgTable::Narinfo).await
    }

    /// Complete L2 wipe: truncate BOTH the narinfo and NAR tables. Unlike the
    /// per-hash `delete`, this reclaims NAR rows (keyed by narhash, unreachable
    /// from a store-path hash). Returns the narinfo row count removed.
    async fn wipe_all(&self) -> Result<usize, CacheError> {
        let narinfos = self.conn.clear(PgTable::Narinfo).await? as usize;
        self.conn.clear(PgTable::Nar).await?;
        Ok(narinfos)
    }
}

// ---------------------------------------------------------------------------
// Production transport — real sqlx Postgres pool, gated behind the `postgres`
// feature so the default build + unit tests pull zero driver surface. The L2
// mapping above is proven against the in-memory mock; this is the thin SQL layer.
// ---------------------------------------------------------------------------

#[cfg(feature = "postgres")]
mod sqlx_conn {
    use super::{CacheError, PgCacheConn, PgStorageBackend, PgTable};
    use async_trait::async_trait;
    use sqlx::postgres::{PgPool, PgPoolOptions};
    use sqlx::Row;

    fn to_cache_err(e: sqlx::Error) -> CacheError {
        CacheError::Io(std::io::Error::other(format!("postgres: {e}")))
    }

    impl PgTable {
        /// `CREATE TABLE IF NOT EXISTS` DDL — a full static SQL string per arm
        /// (typed emission: no runtime string assembly of the table name).
        const fn ddl(self) -> &'static str {
            match self {
                PgTable::Narinfo => {
                    "CREATE TABLE IF NOT EXISTS sui_cache_narinfo (key TEXT PRIMARY KEY, value BYTEA NOT NULL)"
                }
                PgTable::Nar => {
                    "CREATE TABLE IF NOT EXISTS sui_cache_nar (key TEXT PRIMARY KEY, value BYTEA NOT NULL)"
                }
            }
        }

        const fn select_sql(self) -> &'static str {
            match self {
                PgTable::Narinfo => "SELECT value FROM sui_cache_narinfo WHERE key = $1",
                PgTable::Nar => "SELECT value FROM sui_cache_nar WHERE key = $1",
            }
        }

        const fn upsert_sql(self) -> &'static str {
            match self {
                PgTable::Narinfo => {
                    "INSERT INTO sui_cache_narinfo (key, value) VALUES ($1, $2) \
                     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
                }
                PgTable::Nar => {
                    "INSERT INTO sui_cache_nar (key, value) VALUES ($1, $2) \
                     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
                }
            }
        }

        const fn delete_sql(self) -> &'static str {
            match self {
                PgTable::Narinfo => "DELETE FROM sui_cache_narinfo WHERE key = $1",
                PgTable::Nar => "DELETE FROM sui_cache_nar WHERE key = $1",
            }
        }

        const fn clear_sql(self) -> &'static str {
            match self {
                PgTable::Narinfo => "DELETE FROM sui_cache_narinfo",
                PgTable::Nar => "DELETE FROM sui_cache_nar",
            }
        }

        const fn keys_sql(self) -> &'static str {
            match self {
                PgTable::Narinfo => "SELECT key FROM sui_cache_narinfo",
                PgTable::Nar => "SELECT key FROM sui_cache_nar",
            }
        }
    }

    /// Production [`PgCacheConn`] over a `sqlx` Postgres connection pool.
    pub struct SqlxPgCacheConn {
        pool: PgPool,
    }

    impl SqlxPgCacheConn {
        /// Connect to `url` (e.g. `postgres://user@postgres.super-cache-ci.svc:5432/sui`),
        /// bounding the pool at `max_conns`, and ensure the two cache tables exist.
        ///
        /// # Errors
        ///
        /// Returns [`CacheError::Io`] if the pool cannot be built or the schema
        /// DDL fails.
        pub async fn connect(url: &str, max_conns: u32) -> Result<Self, CacheError> {
            let pool = PgPoolOptions::new()
                .max_connections(max_conns)
                .connect(url)
                .await
                .map_err(to_cache_err)?;
            let this = Self { pool };
            this.ensure_schema().await?;
            Ok(this)
        }

        async fn ensure_schema(&self) -> Result<(), CacheError> {
            for t in [PgTable::Narinfo, PgTable::Nar] {
                sqlx::query(t.ddl()).execute(&self.pool).await.map_err(to_cache_err)?;
            }
            Ok(())
        }
    }

    #[async_trait]
    impl PgCacheConn for SqlxPgCacheConn {
        async fn select(&self, table: PgTable, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
            let row = sqlx::query(table.select_sql())
                .bind(key)
                .fetch_optional(&self.pool)
                .await
                .map_err(to_cache_err)?;
            match row {
                Some(r) => {
                    let v: Vec<u8> = r.try_get("value").map_err(to_cache_err)?;
                    Ok(Some(v))
                }
                None => Ok(None),
            }
        }

        async fn upsert(&self, table: PgTable, key: &str, value: &[u8]) -> Result<(), CacheError> {
            sqlx::query(table.upsert_sql())
                .bind(key)
                .bind(value)
                .execute(&self.pool)
                .await
                .map_err(to_cache_err)?;
            Ok(())
        }

        async fn delete(&self, table: PgTable, key: &str) -> Result<(), CacheError> {
            sqlx::query(table.delete_sql())
                .bind(key)
                .execute(&self.pool)
                .await
                .map_err(to_cache_err)?;
            Ok(())
        }

        async fn keys(&self, table: PgTable) -> Result<Vec<String>, CacheError> {
            let rows = sqlx::query(table.keys_sql())
                .fetch_all(&self.pool)
                .await
                .map_err(to_cache_err)?;
            rows.into_iter()
                .map(|r| r.try_get::<String, _>("key").map_err(to_cache_err))
                .collect()
        }

        async fn clear(&self, table: PgTable) -> Result<u64, CacheError> {
            let res = sqlx::query(table.clear_sql())
                .execute(&self.pool)
                .await
                .map_err(to_cache_err)?;
            Ok(res.rows_affected())
        }
    }

    impl PgStorageBackend<SqlxPgCacheConn> {
        /// Connect an L2 backend to a Postgres `url`, pool-bounded at `max_conns`.
        ///
        /// # Errors
        ///
        /// Propagates a connection/schema failure from [`SqlxPgCacheConn::connect`].
        pub async fn connect(url: &str, max_conns: u32) -> Result<Self, CacheError> {
            Ok(Self::new(SqlxPgCacheConn::connect(url, max_conns).await?))
        }
    }
}

#[cfg(feature = "postgres")]
pub use sqlx_conn::SqlxPgCacheConn;

// ---------------------------------------------------------------------------
// Unit tests — the L2 mapping proven against an in-memory mock PgCacheConn.
// No live Postgres required.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory [`PgCacheConn`] mock: a per-table `HashMap`. Durable within the
    /// process (unlike the Redis mock, there is no `evict`) — this tier is
    /// authoritative.
    #[derive(Default)]
    struct MockPg {
        narinfo: Mutex<HashMap<String, Vec<u8>>>,
        nar: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl MockPg {
        fn table(&self, t: PgTable) -> &Mutex<HashMap<String, Vec<u8>>> {
            match t {
                PgTable::Narinfo => &self.narinfo,
                PgTable::Nar => &self.nar,
            }
        }
    }

    #[async_trait]
    impl PgCacheConn for MockPg {
        async fn select(&self, table: PgTable, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
            Ok(self.table(table).lock().unwrap().get(key).cloned())
        }

        async fn upsert(&self, table: PgTable, key: &str, value: &[u8]) -> Result<(), CacheError> {
            self.table(table).lock().unwrap().insert(key.to_string(), value.to_vec());
            Ok(())
        }

        async fn delete(&self, table: PgTable, key: &str) -> Result<(), CacheError> {
            self.table(table).lock().unwrap().remove(key);
            Ok(())
        }

        async fn keys(&self, table: PgTable) -> Result<Vec<String>, CacheError> {
            Ok(self.table(table).lock().unwrap().keys().cloned().collect())
        }

        async fn clear(&self, table: PgTable) -> Result<u64, CacheError> {
            let mut m = self.table(table).lock().unwrap();
            let n = m.len() as u64;
            m.clear();
            Ok(n)
        }
    }

    const NARINFO: &str = "StorePath: /nix/store/abc-hello\nURL: nar/abc.nar.xz\nCompression: xz\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n";

    #[test]
    fn table_names_are_distinct() {
        assert_ne!(PgTable::Narinfo.table_name(), PgTable::Nar.table_name());
    }

    #[tokio::test]
    async fn get_missing_narinfo_returns_none() {
        let backend = PgStorageBackend::new(MockPg::default());
        assert!(backend.get_narinfo("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_then_get_narinfo_roundtrips() {
        let backend = PgStorageBackend::new(MockPg::default());
        backend.put_narinfo("abc", NARINFO).await.unwrap();
        assert_eq!(backend.get_narinfo("abc").await.unwrap().unwrap(), NARINFO);
    }

    #[tokio::test]
    async fn put_then_get_nar_roundtrips() {
        let backend = PgStorageBackend::new(MockPg::default());
        let data = b"\x00\x01\x02 fake nar bytes";
        backend.put_nar("nar/abc.nar.xz", data).await.unwrap();
        assert_eq!(backend.get_nar("nar/abc.nar.xz").await.unwrap().unwrap(), data);
    }

    #[tokio::test]
    async fn narinfo_and_nar_keyspaces_do_not_collide() {
        // Same bare id used as both a narinfo hash and a nar path fragment: the
        // two-table split keeps them apart.
        let backend = PgStorageBackend::new(MockPg::default());
        backend.put_narinfo("dead", "the-narinfo").await.unwrap();
        backend.put_nar("dead", b"the-nar").await.unwrap();
        assert_eq!(backend.get_narinfo("dead").await.unwrap().unwrap(), "the-narinfo");
        assert_eq!(backend.get_nar("dead").await.unwrap().unwrap(), b"the-nar");
    }

    #[tokio::test]
    async fn delete_removes_narinfo_and_common_nar_patterns() {
        let backend = PgStorageBackend::new(MockPg::default());
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
    async fn wipe_all_truncates_both_tables_incl_narhash_keyed_nar() {
        let backend = PgStorageBackend::new(MockPg::default());
        // Real keying: narinfo by store-hash, NAR by a DIFFERENT narhash — the
        // orphan class a per-hash `delete` cannot reach.
        backend.put_narinfo("storehash", NARINFO).await.unwrap();
        backend.put_nar("nar/0narhashXXXXXXXXXXXXXXXXXXXXXXXXXX.nar", b"blob").await.unwrap();
        backend.put_narinfo("other", NARINFO).await.unwrap();

        let removed = backend.wipe_all().await.unwrap();
        assert_eq!(removed, 2, "wipe_all should report the narinfo count");

        // Both tables fully cleared — including the narhash-keyed NAR.
        assert!(backend.list_narinfos().await.unwrap().is_empty());
        assert!(backend.get_narinfo("storehash").await.unwrap().is_none());
        assert!(backend.get_narinfo("other").await.unwrap().is_none());
        assert!(backend
            .get_nar("nar/0narhashXXXXXXXXXXXXXXXXXXXXXXXXXX.nar")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn delete_absent_is_idempotent() {
        let backend = PgStorageBackend::new(MockPg::default());
        backend.delete("ghost").await.unwrap();
    }

    #[tokio::test]
    async fn list_narinfos_is_authoritative_and_full() {
        let backend = PgStorageBackend::new(MockPg::default());
        backend.put_narinfo("aaa", "1").await.unwrap();
        backend.put_narinfo("bbb", "2").await.unwrap();
        // A NAR write must not leak into the narinfo listing.
        backend.put_nar("nar/ccc.nar.xz", b"3").await.unwrap();
        let mut hashes = backend.list_narinfos().await.unwrap();
        hashes.sort();
        assert_eq!(hashes, vec!["aaa".to_string(), "bbb".to_string()]);
    }

    #[tokio::test]
    async fn overwrite_narinfo_takes_latest() {
        let backend = PgStorageBackend::new(MockPg::default());
        backend.put_narinfo("h", "v1").await.unwrap();
        backend.put_narinfo("h", "v2").await.unwrap();
        assert_eq!(backend.get_narinfo("h").await.unwrap().unwrap(), "v2");
    }

    #[tokio::test]
    async fn invalid_utf8_narinfo_surfaces_typed_error() {
        let mock = MockPg::default();
        mock.narinfo.lock().unwrap().insert("bad".to_string(), vec![0xff, 0xfe, 0xfd]);
        let backend = PgStorageBackend::new(mock);
        let err = backend.get_narinfo("bad").await.unwrap_err();
        assert!(matches!(err, CacheError::NarInfo(_)));
    }
}
