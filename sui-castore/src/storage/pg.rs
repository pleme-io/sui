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
use crate::StoreError;

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
    async fn select(&self, table: PgTable, key: &str) -> Result<Option<Vec<u8>>, StoreError>;

    /// Upsert (`INSERT … ON CONFLICT (key) DO UPDATE`) — idempotent by key; a
    /// re-`put` of a content-addressed key overwrites with identical bytes.
    async fn upsert(&self, table: PgTable, key: &str, value: &[u8]) -> Result<(), StoreError>;

    /// `DELETE FROM <table> WHERE key = $1` — idempotent; deleting an absent key
    /// is `Ok(())`.
    async fn delete(&self, table: PgTable, key: &str) -> Result<(), StoreError>;

    /// `SELECT key FROM <table>` — the **authoritative** full key set (this is a
    /// durable tier, not a partial hot cache).
    async fn keys(&self, table: PgTable) -> Result<Vec<String>, StoreError>;

    /// `DELETE FROM <table>` — clear the whole table, returning the row count
    /// removed. The typed whole-store wipe primitive (the inverse of a warm
    /// push); reaches NAR rows a per-key `delete` cannot.
    async fn clear(&self, table: PgTable) -> Result<u64, StoreError>;

    /// Idempotently (re-)create every table this connection serves.
    ///
    /// This is the **self-heal** verb. [`PgStorageBackend`] calls it when a row
    /// verb reports [`StoreError::SchemaMissing`], then retries the verb once —
    /// so a durable tier that comes back on an empty volume repairs itself on
    /// the next request instead of erroring until someone restarts the process.
    ///
    /// It must be safe to call **at any time, any number of times**
    /// (`CREATE TABLE IF NOT EXISTS`), including concurrently.
    ///
    /// The default is a no-op, so a backend whose schema cannot go missing (an
    /// in-memory mock) needs no implementation. A backend that *does* return
    /// `SchemaMissing` must override this — otherwise the retry re-runs against
    /// the same absent schema and fails identically, which is still correct, just
    /// unhealed.
    ///
    /// # Errors
    ///
    /// Returns an error if the DDL cannot be executed.
    async fn ensure_schema(&self) -> Result<(), StoreError> {
        Ok(())
    }
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

    /// Run a row verb; if it reports [`StoreError::SchemaMissing`], re-run the
    /// idempotent DDL via [`PgCacheConn::ensure_schema`] and retry **once**.
    ///
    /// This lives here — in the generic layer over the [`PgCacheConn`] seam —
    /// rather than inside the sqlx adapter, so the self-heal is provable against
    /// the in-memory mock with no live Postgres.
    ///
    /// Exactly one retry: a schema that is still missing after its own DDL ran
    /// is a real failure (permissions, wrong database, a dropped role), not a
    /// transient, and must surface rather than spin.
    async fn healing<T, F, Fut>(&self, op: F) -> Result<T, StoreError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, StoreError>>,
    {
        match op().await {
            Err(StoreError::SchemaMissing(detail)) => {
                tracing::warn!(
                    detail = %detail,
                    "pg L2: schema absent — re-running idempotent DDL and retrying once \
                     (a durable tier came back on an empty volume?)",
                );
                self.conn.ensure_schema().await?;
                op().await
            }
            other => other,
        }
    }
}

#[async_trait]
impl<C: PgCacheConn> StorageBackend for PgStorageBackend<C> {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, StoreError> {
        match self.healing(|| self.conn.select(PgTable::Narinfo, hash)).await? {
            Some(bytes) => {
                let text = String::from_utf8(bytes).map_err(|e| {
                    StoreError::NarInfo(format!("invalid utf-8 in pg narinfo {hash}: {e}"))
                })?;
                Ok(Some(text))
            }
            None => Ok(None),
        }
    }

    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), StoreError> {
        self.healing(|| self.conn.upsert(PgTable::Narinfo, hash, content.as_bytes())).await
    }

    async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.healing(|| self.conn.select(PgTable::Nar, path)).await
    }

    async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), StoreError> {
        self.healing(|| self.conn.upsert(PgTable::Nar, path, data)).await
    }

    async fn delete(&self, hash: &str) -> Result<(), StoreError> {
        // narinfo keyed directly by hash.
        self.healing(|| self.conn.delete(PgTable::Narinfo, hash)).await?;
        // NAR blobs are keyed by relative URL; only the hash is in hand here, so —
        // mirroring `RedisBackend`/`S3Storage::delete` — best-effort-delete the
        // common NAR path patterns. `delete` is idempotent, so absent keys are
        // harmless.
        for ext in ["nar.xz", "nar.zst", "nar"] {
            let key = format!("nar/{hash}.{ext}");
            self.healing(|| self.conn.delete(PgTable::Nar, &key)).await?;
        }
        Ok(())
    }

    async fn list_narinfos(&self) -> Result<Vec<String>, StoreError> {
        self.healing(|| self.conn.keys(PgTable::Narinfo)).await
    }

    /// Complete L2 wipe: truncate BOTH the narinfo and NAR tables. Unlike the
    /// per-hash `delete`, this reclaims NAR rows (keyed by narhash, unreachable
    /// from a store-path hash). Returns the narinfo row count removed.
    async fn wipe_all(&self) -> Result<usize, StoreError> {
        let narinfos = self.healing(|| self.conn.clear(PgTable::Narinfo)).await? as usize;
        self.healing(|| self.conn.clear(PgTable::Nar)).await?;
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
    use super::{StoreError, PgCacheConn, PgStorageBackend, PgTable};
    use async_trait::async_trait;
    use sqlx::postgres::{PgPool, PgPoolOptions};
    use sqlx::Row;

    /// Postgres SQLSTATE `42P01` — `undefined_table`. The one code that means
    /// "your schema is gone", and therefore the one that is self-healable.
    const UNDEFINED_TABLE: &str = "42P01";

    fn to_store_err(e: sqlx::Error) -> StoreError {
        // Classify BEFORE flattening into an opaque io::Error, so the healable
        // case keeps its own typed variant. Everything else stays `Io` — a
        // connection reset, an OOM-killed backend mid-query, a protocol error
        // are all real failures, never rounded up into "just re-run the DDL".
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some(UNDEFINED_TABLE) {
                return StoreError::SchemaMissing(format!("postgres: {e}"));
            }
        }
        StoreError::Io(std::io::Error::other(format!("postgres: {e}")))
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
        /// # Schema lifecycle (three independent nets — read this before changing it)
        ///
        /// The DDL is `CREATE TABLE IF NOT EXISTS`, so running it is always safe.
        /// It runs at three moments, each covering a failure the others do not:
        ///
        /// 1. **Process start** (the explicit [`create_tables`](Self::create_tables)
        ///    below) — a first-ever deploy against an empty database.
        /// 2. **Every new physical connection** (the `after_connect` hook) — this
        ///    is the one that matters when the *database* restarts while this
        ///    process keeps running. `PgPool` transparently replaces dead
        ///    connections; without this hook those replacements land on a
        ///    schemaless database and every query fails forever, with no restart
        ///    of *this* process to re-trigger step 1. That is exactly how a
        ///    Postgres pod on an `emptyDir` takes the cache down permanently.
        /// 3. **On a `42P01` at query time** (the `PgStorageBackend::healing`
        ///    retry) — covers the schema vanishing under an *already-established,
        ///    still-live* connection, which neither of the above can see.
        ///
        /// # Errors
        ///
        /// Returns [`StoreError::Io`] if the pool cannot be built or the schema
        /// DDL fails.
        pub async fn connect(url: &str, max_conns: u32) -> Result<Self, StoreError> {
            let pool = PgPoolOptions::new()
                .max_connections(max_conns)
                // Net 2: re-assert the schema on EVERY physical connection, so a
                // pool reconnect to a rebuilt/wiped database repairs itself.
                .after_connect(|conn, _meta| {
                    Box::pin(async move {
                        for t in [PgTable::Narinfo, PgTable::Nar] {
                            sqlx::query(t.ddl()).execute(&mut *conn).await?;
                        }
                        Ok(())
                    })
                })
                .connect(url)
                .await
                .map_err(to_store_err)?;
            let this = Self { pool };
            // Net 1: explicit, so a first deploy fails loudly at startup rather
            // than on the first request.
            this.create_tables().await?;
            Ok(this)
        }

        /// Run the idempotent `CREATE TABLE IF NOT EXISTS` DDL for both tables.
        ///
        /// Named distinctly from the [`PgCacheConn::ensure_schema`] trait method
        /// that delegates to it — an inherent method of the same name would make
        /// that delegation resolve to itself and recurse forever.
        async fn create_tables(&self) -> Result<(), StoreError> {
            for t in [PgTable::Narinfo, PgTable::Nar] {
                sqlx::query(t.ddl()).execute(&self.pool).await.map_err(to_store_err)?;
            }
            Ok(())
        }
    }

    #[async_trait]
    impl PgCacheConn for SqlxPgCacheConn {
        async fn select(&self, table: PgTable, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
            let row = sqlx::query(table.select_sql())
                .bind(key)
                .fetch_optional(&self.pool)
                .await
                .map_err(to_store_err)?;
            match row {
                Some(r) => {
                    let v: Vec<u8> = r.try_get("value").map_err(to_store_err)?;
                    Ok(Some(v))
                }
                None => Ok(None),
            }
        }

        async fn upsert(&self, table: PgTable, key: &str, value: &[u8]) -> Result<(), StoreError> {
            sqlx::query(table.upsert_sql())
                .bind(key)
                .bind(value)
                .execute(&self.pool)
                .await
                .map_err(to_store_err)?;
            Ok(())
        }

        async fn delete(&self, table: PgTable, key: &str) -> Result<(), StoreError> {
            sqlx::query(table.delete_sql())
                .bind(key)
                .execute(&self.pool)
                .await
                .map_err(to_store_err)?;
            Ok(())
        }

        async fn keys(&self, table: PgTable) -> Result<Vec<String>, StoreError> {
            let rows = sqlx::query(table.keys_sql())
                .fetch_all(&self.pool)
                .await
                .map_err(to_store_err)?;
            rows.into_iter()
                .map(|r| r.try_get::<String, _>("key").map_err(to_store_err))
                .collect()
        }

        async fn clear(&self, table: PgTable) -> Result<u64, StoreError> {
            let res = sqlx::query(table.clear_sql())
                .execute(&self.pool)
                .await
                .map_err(to_store_err)?;
            Ok(res.rows_affected())
        }

        /// Net 3: the self-heal the `healing` retry drives.
        async fn ensure_schema(&self) -> Result<(), StoreError> {
            self.create_tables().await
        }
    }

    impl PgStorageBackend<SqlxPgCacheConn> {
        /// Connect an L2 backend to a Postgres `url`, pool-bounded at `max_conns`.
        ///
        /// # Errors
        ///
        /// Propagates a connection/schema failure from [`SqlxPgCacheConn::connect`].
        pub async fn connect(url: &str, max_conns: u32) -> Result<Self, StoreError> {
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
        /// Whether the tables "exist". Models the real failure: a Postgres that
        /// is up and connectable but whose relations are gone (an `emptyDir`
        /// PGDATA destroyed by a pod roll).
        schema_missing: Mutex<bool>,
        /// How many times the idempotent DDL has been run — proves both that
        /// the self-heal fires and that re-running it is harmless.
        ensure_schema_calls: Mutex<usize>,
    }

    impl MockPg {
        fn table(&self, t: PgTable) -> &Mutex<HashMap<String, Vec<u8>>> {
            match t {
                PgTable::Narinfo => &self.narinfo,
                PgTable::Nar => &self.nar,
            }
        }
        /// Drop the schema out from under a live connection.
        ///
        /// Clears the rows as well, because that is what actually happens: an
        /// `emptyDir` PGDATA destroyed by a pod roll takes the data with it, and
        /// so does a `DROP TABLE`. Losing the schema and keeping the rows is not
        /// a reachable state, so the mock must not model one.
        fn drop_schema(&self) {
            *self.schema_missing.lock().unwrap() = true;
            self.narinfo.lock().unwrap().clear();
            self.nar.lock().unwrap().clear();
        }
        fn ddl_runs(&self) -> usize {
            *self.ensure_schema_calls.lock().unwrap()
        }
        fn guard(&self) -> Result<(), StoreError> {
            if *self.schema_missing.lock().unwrap() {
                // Mirrors the real sqlx mapping of SQLSTATE 42P01.
                Err(StoreError::SchemaMissing(
                    "postgres: relation \"sui_cache_narinfo\" does not exist".to_string(),
                ))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl PgCacheConn for MockPg {
        async fn select(&self, table: PgTable, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
            self.guard()?;
            Ok(self.table(table).lock().unwrap().get(key).cloned())
        }

        async fn upsert(&self, table: PgTable, key: &str, value: &[u8]) -> Result<(), StoreError> {
            self.guard()?;
            self.table(table).lock().unwrap().insert(key.to_string(), value.to_vec());
            Ok(())
        }

        async fn delete(&self, table: PgTable, key: &str) -> Result<(), StoreError> {
            self.guard()?;
            self.table(table).lock().unwrap().remove(key);
            Ok(())
        }

        async fn keys(&self, table: PgTable) -> Result<Vec<String>, StoreError> {
            self.guard()?;
            Ok(self.table(table).lock().unwrap().keys().cloned().collect())
        }

        async fn clear(&self, table: PgTable) -> Result<u64, StoreError> {
            self.guard()?;
            let mut m = self.table(table).lock().unwrap();
            let n = m.len() as u64;
            m.clear();
            Ok(n)
        }

        /// Idempotent, exactly like `CREATE TABLE IF NOT EXISTS`: running it
        /// when the schema already exists is a no-op, never an error.
        async fn ensure_schema(&self) -> Result<(), StoreError> {
            *self.ensure_schema_calls.lock().unwrap() += 1;
            *self.schema_missing.lock().unwrap() = false;
            Ok(())
        }
    }

    /// A connection whose schema can never be repaired — proves the retry is
    /// bounded at one and a permanent fault still surfaces.
    #[derive(Default)]
    struct UnhealablePg {
        attempts: Mutex<usize>,
    }

    #[async_trait]
    impl PgCacheConn for UnhealablePg {
        async fn select(&self, _t: PgTable, _k: &str) -> Result<Option<Vec<u8>>, StoreError> {
            *self.attempts.lock().unwrap() += 1;
            Err(StoreError::SchemaMissing("still gone".to_string()))
        }
        async fn upsert(&self, _t: PgTable, _k: &str, _v: &[u8]) -> Result<(), StoreError> {
            Err(StoreError::SchemaMissing("still gone".to_string()))
        }
        async fn delete(&self, _t: PgTable, _k: &str) -> Result<(), StoreError> {
            Err(StoreError::SchemaMissing("still gone".to_string()))
        }
        async fn keys(&self, _t: PgTable) -> Result<Vec<String>, StoreError> {
            Err(StoreError::SchemaMissing("still gone".to_string()))
        }
        async fn clear(&self, _t: PgTable) -> Result<u64, StoreError> {
            Err(StoreError::SchemaMissing("still gone".to_string()))
        }
        // `ensure_schema` keeps the no-op default: the DDL "runs" but the schema
        // stays absent (no permission to create, wrong database, …).
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

    // ── schema self-heal (the incident's root cause) ───────────────────────

    #[tokio::test]
    async fn schema_vanishing_under_a_live_connection_self_heals_on_the_next_read() {
        // THE incident. The sui-cache process kept running while the Postgres
        // pod rolled and its emptyDir PGDATA was destroyed. `sqlx`'s pool
        // transparently reconnected — to a database with no tables — and every
        // query failed from then on, with no restart of THIS process to
        // re-trigger the connect-time DDL. It 500ed for over an hour.
        let backend = PgStorageBackend::new(MockPg::default());
        backend.put_narinfo("h", NARINFO).await.unwrap();
        assert_eq!(backend.conn().ddl_runs(), 0, "no heal needed while healthy");

        backend.conn().drop_schema();

        // The read must succeed rather than erroring: the DDL is re-run and the
        // query retried. The row itself is gone (the volume was wiped), so the
        // honest answer is a clean MISS — which is precisely the harmless case:
        // the client records a cache miss and builds.
        let got = backend.get_narinfo("h").await.expect("must self-heal, not error");
        assert!(got.is_none(), "the data really is gone — a miss, not a 500");
        assert_eq!(backend.conn().ddl_runs(), 1, "the idempotent DDL must have re-run");

        // The tier is now fully functional again — writes land and read back.
        backend.put_narinfo("h2", NARINFO).await.unwrap();
        assert_eq!(backend.get_narinfo("h2").await.unwrap().unwrap(), NARINFO);
    }

    #[tokio::test]
    async fn ensure_schema_is_idempotent_run_twice_no_error() {
        // Requirement: schema creation is safe to run on every startup, and any
        // number of times after. (`CREATE TABLE IF NOT EXISTS` in the real
        // adapter; the mock mirrors that contract.)
        let conn = MockPg::default();
        conn.ensure_schema().await.expect("first run");
        conn.ensure_schema().await.expect("second run must be a harmless no-op");
        conn.ensure_schema().await.expect("third run must be a harmless no-op");
        assert_eq!(conn.ddl_runs(), 3);

        // …and it is equally safe when the schema is currently absent.
        conn.drop_schema();
        conn.ensure_schema().await.expect("run against an absent schema");
        conn.ensure_schema().await.expect("and again once it exists");
        let backend = PgStorageBackend::new(conn);
        backend.put_narinfo("x", NARINFO).await.expect("usable after repeated DDL");
    }

    #[tokio::test]
    async fn every_verb_self_heals_not_just_reads() {
        // A write arriving first must repair the schema too — otherwise the
        // cache stays unfillable until something happens to read.
        for_each_verb_self_heals().await;
    }

    async fn for_each_verb_self_heals() {
        // put_narinfo
        let b = PgStorageBackend::new(MockPg::default());
        b.conn().drop_schema();
        b.put_narinfo("h", NARINFO).await.expect("put_narinfo self-heals");
        assert_eq!(b.get_narinfo("h").await.unwrap().unwrap(), NARINFO);

        // put_nar
        let b = PgStorageBackend::new(MockPg::default());
        b.conn().drop_schema();
        b.put_nar("nar/h.nar.xz", b"blob").await.expect("put_nar self-heals");
        assert_eq!(b.get_nar("nar/h.nar.xz").await.unwrap().unwrap(), b"blob");

        // list_narinfos
        let b = PgStorageBackend::new(MockPg::default());
        b.conn().drop_schema();
        assert!(b.list_narinfos().await.expect("list self-heals").is_empty());

        // delete
        let b = PgStorageBackend::new(MockPg::default());
        b.conn().drop_schema();
        b.delete("h").await.expect("delete self-heals");

        // wipe_all
        let b = PgStorageBackend::new(MockPg::default());
        b.conn().drop_schema();
        assert_eq!(b.wipe_all().await.expect("wipe self-heals"), 0);
    }

    #[tokio::test]
    async fn an_unrepairable_schema_surfaces_after_exactly_one_retry() {
        // No infinite retry loop: a schema that is still missing after its own
        // DDL ran is a real fault (permissions, wrong database) and must
        // surface. Exactly two attempts — the original and one retry.
        let backend = PgStorageBackend::new(UnhealablePg::default());
        let err = backend.get_narinfo("h").await.unwrap_err();
        assert!(matches!(err, StoreError::SchemaMissing(_)));
        assert_eq!(
            *backend.conn().attempts.lock().unwrap(),
            2,
            "exactly one retry after the heal attempt — never a spin",
        );
    }

    #[tokio::test]
    async fn a_non_schema_error_is_never_retried_as_a_schema_problem() {
        // Only `SchemaMissing` triggers the DDL path. A connection reset or an
        // OOM-killed backend mid-query must not be rounded up into "just
        // re-create the tables".
        struct BrokenPg;
        #[async_trait]
        impl PgCacheConn for BrokenPg {
            async fn select(&self, _t: PgTable, _k: &str) -> Result<Option<Vec<u8>>, StoreError> {
                Err(StoreError::Io(std::io::Error::other(
                    "postgres: expected to read 5 bytes, got 0 bytes at EOF",
                )))
            }
            async fn upsert(&self, _t: PgTable, _k: &str, _v: &[u8]) -> Result<(), StoreError> {
                unreachable!()
            }
            async fn delete(&self, _t: PgTable, _k: &str) -> Result<(), StoreError> {
                unreachable!()
            }
            async fn keys(&self, _t: PgTable) -> Result<Vec<String>, StoreError> {
                unreachable!()
            }
            async fn clear(&self, _t: PgTable) -> Result<u64, StoreError> {
                unreachable!()
            }
            async fn ensure_schema(&self) -> Result<(), StoreError> {
                panic!("a non-schema error must never trigger the DDL path");
            }
        }
        let backend = PgStorageBackend::new(BrokenPg);
        assert!(matches!(backend.get_narinfo("h").await.unwrap_err(), StoreError::Io(_)));
    }

    #[tokio::test]
    async fn invalid_utf8_narinfo_surfaces_typed_error() {
        let mock = MockPg::default();
        mock.narinfo.lock().unwrap().insert("bad".to_string(), vec![0xff, 0xfe, 0xfd]);
        let backend = PgStorageBackend::new(mock);
        let err = backend.get_narinfo("bad").await.unwrap_err();
        assert!(matches!(err, StoreError::NarInfo(_)));
    }

    // ── Fault injection ────────────────────────────────────────────────
    //
    // Every mock above is INFALLIBLE: `MockPg` returns `Ok` from all five
    // trait methods, so no test in this file could observe what the backend
    // does when Postgres refuses. That is not a small gap — it is why the
    // 2026-07-26 camelot outage class was invisible to a green suite.
    //
    // What happened: the `sui-cache-pg` pod was rescheduled at 19:00:06Z with
    // `pgdata: emptyDir`, so its database came up EMPTY. sui had connected
    // ~24h earlier (pod start 2026-07-25T18:23:55Z) and `ensure_schema` is
    // called from exactly ONE place — inside `connect` — and never again. So
    // the process held a live pool to a blank database and every read hit
    // `relation "sui_cache_narinfo" does not exist`. `get_narinfo` propagated
    // that with `?`, the HTTP layer mapped `Err` to 500, and Nix treats a 500
    // from a substituter as a HARD FAILURE rather than a cache miss — so every
    // Nix build on the cluster failed.
    //
    // Two separable defects, and these tests pin the boundary between them:
    //   1. STORAGE CONTRACT (here): a backend fault must surface truthfully as
    //      `Err`, and must remain DISTINGUISHABLE from `Ok(None)`. Collapsing
    //      them here would make the storage layer lie, and a genuinely broken
    //      cache would then look permanently empty with no signal anywhere.
    //   2. PROTOCOL SEMANTICS (sui-cache/src/server.rs): for a *substituter*,
    //      "I cannot answer" and "I do not have it" are the same answer to the
    //      client — both mean "build it yourself". That collapse belongs at the
    //      HTTP boundary, where it is a deliberate protocol decision, NOT in
    //      the storage layer where it would be data loss dressed as resilience.
    //
    // So these tests deliberately assert that the storage layer KEEPS erroring.
    // The degradation is tested on the server side.

    /// A [`PgCacheConn`] that fails after `ok_calls` successful selects,
    /// reproducing the incident's timeline (works, then the schema vanishes
    /// underneath a live pool) rather than a backend that was never healthy.
    struct FaultyPg {
        inner:     MockPg,
        ok_calls:  Mutex<usize>,
        fail_with: String,
    }

    impl FaultyPg {
        /// Fails every call — a backend that is broken from the start.
        fn always(msg: &str) -> Self {
            Self {
                inner:     MockPg::default(),
                ok_calls:  Mutex::new(0),
                fail_with: msg.to_string(),
            }
        }

        /// Serves `n` calls normally, then fails every call after — the
        /// schema-vanished-under-a-live-pool shape.
        fn after(n: usize, msg: &str) -> Self {
            Self {
                inner:     MockPg::default(),
                ok_calls:  Mutex::new(n),
                fail_with: msg.to_string(),
            }
        }

        /// Consume one budgeted success, or fail. Returns `Err` once spent.
        fn tick(&self) -> Result<(), StoreError> {
            let mut left = self.ok_calls.lock().unwrap();
            if *left == 0 {
                return Err(StoreError::Io(std::io::Error::other(format!(
                    "postgres: {}",
                    self.fail_with
                ))));
            }
            *left -= 1;
            Ok(())
        }
    }

    /// The exact string Postgres returns for the missing relation, so the
    /// fixture cannot drift from the incident it encodes.
    const RELATION_MISSING: &str = "error returned from database: \
                                    relation \"sui_cache_narinfo\" does not exist";

    #[async_trait]
    impl PgCacheConn for FaultyPg {
        async fn select(&self, table: PgTable, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
            self.tick()?;
            self.inner.select(table, key).await
        }
        async fn upsert(&self, table: PgTable, key: &str, value: &[u8]) -> Result<(), StoreError> {
            self.tick()?;
            self.inner.upsert(table, key, value).await
        }
        async fn delete(&self, table: PgTable, key: &str) -> Result<(), StoreError> {
            self.tick()?;
            self.inner.delete(table, key).await
        }
        async fn keys(&self, table: PgTable) -> Result<Vec<String>, StoreError> {
            self.tick()?;
            self.inner.keys(table).await
        }
        async fn clear(&self, table: PgTable) -> Result<u64, StoreError> {
            self.tick()?;
            self.inner.clear(table).await
        }
    }

    /// THE INCIDENT, as a test. A backend fault must NOT be reported as a miss
    /// by the storage layer — the two must stay distinguishable, because the
    /// caller's correct response differs (a miss means "not cached"; a fault
    /// means "this cache is broken, page someone").
    #[tokio::test]
    async fn backend_fault_is_an_error_never_a_silent_miss() {
        let backend = PgStorageBackend::new(FaultyPg::always(RELATION_MISSING));
        let err = backend.get_narinfo("abc").await.unwrap_err();
        assert!(
            err.to_string().contains("does not exist"),
            "the underlying cause must survive to the caller, got: {err}"
        );

        // The discriminating assertion: a HEALTHY backend with no such key
        // returns Ok(None). If a fault also returned Ok(None), these two would
        // be indistinguishable and a broken cache would masquerade as an empty
        // one — invisible, permanently.
        let healthy = PgStorageBackend::new(MockPg::default());
        assert!(healthy.get_narinfo("abc").await.unwrap().is_none());
    }

    /// Writes must not silently succeed against a broken backend — a swallowed
    /// `put` would report a populated cache that holds nothing.
    #[tokio::test]
    async fn backend_fault_on_write_is_an_error() {
        let backend = PgStorageBackend::new(FaultyPg::always(RELATION_MISSING));
        assert!(backend.put_narinfo("h", NARINFO).await.is_err());
        assert!(backend.put_nar("nar/h.nar.xz", b"bytes").await.is_err());
    }

    /// Every read path, not just narinfo — parity matters because `get_nar`
    /// serves the actual build artifacts and has its own code path.
    #[tokio::test]
    async fn every_read_path_propagates_a_backend_fault() {
        let backend = PgStorageBackend::new(FaultyPg::always(RELATION_MISSING));
        assert!(backend.get_narinfo("h").await.is_err(), "get_narinfo");
        assert!(backend.get_nar("nar/h.nar.xz").await.is_err(), "get_nar");
        assert!(backend.list_narinfos().await.is_err(), "list_narinfos");
        assert!(backend.delete("h").await.is_err(), "delete");
    }

    /// Concurrency: parallel writers must not lose writes. The mock is
    /// `Mutex`-guarded per table, so this pins the backend's own key handling
    /// rather than the DB's — a regression that mangled keys (e.g. a shared
    /// buffer) would show up as a count mismatch.
    #[tokio::test]
    async fn concurrent_puts_do_not_lose_writes() {
        use std::sync::Arc;
        let backend = Arc::new(PgStorageBackend::new(MockPg::default()));
        let mut set = tokio::task::JoinSet::new();
        for i in 0..64 {
            let b = Arc::clone(&backend);
            set.spawn(async move { b.put_narinfo(&format!("k{i}"), &format!("v{i}")).await });
        }
        while let Some(r) = set.join_next().await {
            r.expect("task panicked").expect("put failed");
        }
        assert_eq!(backend.list_narinfos().await.unwrap().len(), 64);
        for i in 0..64 {
            assert_eq!(
                backend.get_narinfo(&format!("k{i}")).await.unwrap().unwrap(),
                format!("v{i}"),
                "key k{i} round-tripped wrong under concurrency"
            );
        }
    }

    /// Idempotence under repeat: a content-addressed cache re-`put`s the same
    /// key with identical bytes constantly, and that must be a no-op, not a
    /// duplicate or an error.
    #[tokio::test]
    async fn repeated_identical_put_is_idempotent() {
        let backend = PgStorageBackend::new(MockPg::default());
        for _ in 0..10 {
            backend.put_narinfo("same", NARINFO).await.unwrap();
        }
        assert_eq!(backend.list_narinfos().await.unwrap().len(), 1);
        assert_eq!(backend.get_narinfo("same").await.unwrap().unwrap(), NARINFO);
    }
}
