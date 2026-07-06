//! `PgStore` — the L2 **durable** [`Store`] backed by Postgres.
//!
//! This is the `impl Store for PgStore` LiveTODO named in
//! `sui-supercacheci` (`StoreBackendKind::Postgres`): the never-touch-durable-
//! disk destination for sui's durable store. Where [`LocalStore`](crate::LocalStore)
//! keys metadata rows on a store-path string in an on-disk SQLite DB, and the
//! shipped `sui-graph-store` `GraphStore` keys `rkyv` blobs on a
//! [`GraphHash`] in an on-disk `redb` index, `PgStore` unifies both onto
//! **Postgres**:
//!
//! - a **content-addressed blob table** keyed by the 32-byte BLAKE3
//!   [`GraphHash`] of the NAR bytes (`content_key`), and
//! - a **metadata table** keyed by the absolute store path (itself
//!   content-derived in Nix), carrying the [`PathInfo`] columns plus a
//!   pointer to the blob's `content_key`.
//!
//! ## The two load-bearing invariants
//!
//! 1. **Atomic write.** A path's metadata row and its NAR blob are written in
//!    **one transaction** (the [`PgBackend::upsert_path_atomic`] contract). A
//!    half-applied write — metadata advanced without its blob — is not a state
//!    the durable store can be left in. This mirrors the fleet's
//!    ★★ PLATFORM-MEDIATED "state row **and** bundle artifact in one Postgres
//!    transaction" rule.
//! 2. **Content-address integrity.** The blob's key **is** `GraphHash::of(bytes)`.
//!    On the write path the key is *derived*, so a key/bytes mismatch is
//!    unrepresentable by construction (parse-don't-validate). On any read that
//!    crosses a durability/host boundary, [`PgStore::get_validated_blob`]
//!    recomputes the BLAKE3 and rejects a mismatch — the `get_validated`
//!    analogue that catches bitrot / tampering from the durable tier
//!    (`sui-graph-store`'s `HashMismatch` invariant, ported to the Pg axis).
//!
//! ## Mockable by construction (the pleme-io default delivery method)
//!
//! All Postgres row-level I/O sits behind the [`PgBackend`] trait — the
//! TYPED-SPEC Environment seam. [`PgStore`] is generic over it, so the whole
//! content-addressing + atomicity + invariant core is exercised against an
//! in-memory mock ([`InMemoryPgBackend`]) with **no real Postgres**. The real
//! `sqlx`-backed adapter ([`SqlxPgBackend`], behind the `postgres` feature) is
//! a thin translation of the same trait to SQL + a transaction.
//!
//! ## Tier honesty (never rounded up)
//!
//! The shipped tier is [`PgStoreTier::MockParityProven`] — the core is proven
//! against an in-memory oracle. It is **not** `LiveClusterProven`: this crate
//! does not (and offline cannot) stand up a real Postgres and prove
//! byte-for-byte parity with the shipped on-disk store. That differential
//! oracle gate (PgStore vs `LocalStore` / `GraphStore`, the §4 "byte-identical-
//! vs-disk" test) is the named milestone that upgrades the tier. See the
//! honesty-gate test at the bottom of this module.
//!
//! [`GraphHash`]: sui_graph_store::GraphHash

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use sui_compat::store_path::{compute_fixed_output_hash, StorePath, DEFAULT_STORE_DIR};
use sui_graph_store::GraphHash;

use crate::traits::{CorruptPath, PathInfo, Store, StoreError, StoreResult, VerifyResult};

// ─────────────────────────────────────────────────────────────────────────
// Tier marker — the honest self-description of what PgStore IS today.
// ─────────────────────────────────────────────────────────────────────────

/// What `PgStore` has been *proven* to be. Bumping [`PGSTORE_TIER`] to
/// [`LiveClusterProven`](PgStoreTier::LiveClusterProven) without a live-cluster
/// differential-parity test is a build-failing round-up (guarded by
/// `honest_gate_pgstore_is_mock_parity_proven`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgStoreTier {
    /// The content-addressing + atomicity + invariant core is proven against
    /// an in-memory [`PgBackend`] oracle. No real Postgres, no byte-parity
    /// gate against the shipped on-disk store yet.
    MockParityProven,
    /// Proven against a live Postgres with the §4 byte-identical-vs-disk
    /// differential gate green. The destination — **not** shipped here.
    LiveClusterProven,
}

/// The shipped, honest tier of this `PgStore`.
pub const PGSTORE_TIER: PgStoreTier = PgStoreTier::MockParityProven;

// ─────────────────────────────────────────────────────────────────────────
// Error surface
// ─────────────────────────────────────────────────────────────────────────

/// Errors from the Postgres backend seam. Folds into [`StoreError::Database`]
/// (the natural home for a durable-store I/O failure), except the
/// content-address mismatch which is an integrity violation → [`StoreError::Internal`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PgError {
    /// A backend (connection / query / transaction) operation failed.
    #[error("postgres backend error: {0}")]
    Backend(String),
    /// A stored blob's bytes do not hash to the key they are stored under —
    /// tampering or bitrot. Rejected at the write boundary and on validated reads.
    #[error("content-address mismatch: key={expected}, actual={actual}")]
    HashMismatch {
        /// The `content_key` the blob is (claimed to be) stored under.
        expected: String,
        /// The BLAKE3 actually computed from the bytes.
        actual: String,
    },
    /// A row could not be (de)serialised into a [`PgPathRow`].
    #[error("row serialization error: {0}")]
    Serialization(String),
}

impl From<PgError> for StoreError {
    fn from(e: PgError) -> Self {
        match e {
            PgError::HashMismatch { .. } => StoreError::Internal(e.to_string()),
            other => StoreError::Database(other.to_string()),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Row + blob shapes (the typed border between PgStore and any PgBackend)
// ─────────────────────────────────────────────────────────────────────────

/// One row of the durable metadata table. A straight column-per-field of
/// [`PathInfo`], plus `content_key` — the BLAKE3 of the NAR blob (present when
/// a blob was stored via [`Store::add_to_store`]; `None` for a metadata-only
/// [`Store::register_path`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PgPathRow {
    /// Full absolute store path (primary key).
    pub path: String,
    /// NAR hash in `sha256:<hex>` form.
    pub nar_hash: String,
    /// NAR size in bytes.
    pub nar_size: i64,
    /// Runtime reference store paths (absolute).
    pub references: Vec<String>,
    /// Producing `.drv` path, if known.
    pub deriver: Option<String>,
    /// Ed25519 signatures (`keyname:base64sig`).
    pub signatures: Vec<String>,
    /// Unix registration timestamp.
    pub registration_time: i64,
    /// Nix content-address assertion string, if any.
    pub content_address: Option<String>,
    /// BLAKE3 of the NAR blob this row points at (the content key), if a blob
    /// was stored.
    pub content_key: Option<[u8; 32]>,
}

impl PgPathRow {
    /// Build a metadata row from a [`PathInfo`] and an optional content key.
    #[must_use]
    pub fn from_path_info(info: &PathInfo, content_key: Option<[u8; 32]>) -> Self {
        Self {
            path: info.path.clone(),
            nar_hash: info.nar_hash.clone(),
            nar_size: info.nar_size,
            references: info.references.clone(),
            deriver: info.deriver.clone(),
            signatures: info.signatures.clone(),
            registration_time: info.registration_time,
            content_address: info.content_address.clone(),
            content_key,
        }
    }
}

impl From<&PgPathRow> for PathInfo {
    fn from(row: &PgPathRow) -> Self {
        Self {
            path: row.path.clone(),
            nar_hash: row.nar_hash.clone(),
            nar_size: row.nar_size,
            references: row.references.clone(),
            deriver: row.deriver.clone(),
            signatures: row.signatures.clone(),
            registration_time: row.registration_time,
            content_address: row.content_address.clone(),
        }
    }
}

/// A content-addressed NAR blob: its 32-byte BLAKE3 key and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgBlob {
    /// BLAKE3 of `bytes` — the content key the blob is stored under.
    pub content_key: [u8; 32],
    /// The raw NAR bytes.
    pub bytes: Vec<u8>,
}

// ─────────────────────────────────────────────────────────────────────────
// The mockable Postgres seam
// ─────────────────────────────────────────────────────────────────────────

/// The narrow Postgres row-operations seam [`PgStore`] is generic over.
///
/// Real impl: [`SqlxPgBackend`] (feature `postgres`). Test / dev impl:
/// [`InMemoryPgBackend`]. Keeping this trait narrow is what makes the whole
/// `PgStore` core testable with no live database.
///
/// # The atomicity contract
///
/// [`upsert_path_atomic`](PgBackend::upsert_path_atomic) MUST write the blob
/// (when present) **and** the metadata row in **one transaction**. An impl that
/// commits the row without its blob violates the durable-store invariant. The
/// impl MUST also reject a blob whose bytes do not hash to its `content_key`
/// with [`PgError::HashMismatch`] (the write-boundary integrity check).
#[async_trait]
pub trait PgBackend: Send + Sync {
    /// Atomically upsert a metadata row and (optionally) its content-addressed
    /// blob in one transaction.
    async fn upsert_path_atomic(
        &self,
        row: &PgPathRow,
        blob: Option<&PgBlob>,
    ) -> Result<(), PgError>;

    /// Fetch a metadata row by absolute store path.
    async fn get_path(&self, path: &str) -> Result<Option<PgPathRow>, PgError>;

    /// Fetch a NAR blob by its content key.
    async fn get_blob(&self, content_key: &[u8; 32]) -> Result<Option<Vec<u8>>, PgError>;

    /// True if a metadata row exists for the path.
    async fn has_path(&self, path: &str) -> Result<bool, PgError>;

    /// All metadata-row store paths, ascending (the authoritative listing).
    async fn all_paths(&self) -> Result<Vec<String>, PgError>;

    /// Append signatures to a path's row. Returns `false` if the path is absent.
    async fn append_signatures(&self, path: &str, sigs: &[String]) -> Result<bool, PgError>;

    /// Delete a path's metadata row. Returns its `nar_size` (bytes freed), or
    /// `None` if the path was absent.
    async fn delete_path(&self, path: &str) -> Result<Option<i64>, PgError>;
}

// ─────────────────────────────────────────────────────────────────────────
// PgStore — the durable Store over any PgBackend
// ─────────────────────────────────────────────────────────────────────────

/// A Postgres-backed durable [`Store`], content-addressed by BLAKE3 and
/// generic over its [`PgBackend`] so the core is mock-testable.
pub struct PgStore<B: PgBackend> {
    backend: B,
    store_dir: String,
}

impl<B: PgBackend> PgStore<B> {
    /// Build a `PgStore` over the given backend, using the default
    /// `/nix/store` directory for store-path computation.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            store_dir: DEFAULT_STORE_DIR.to_string(),
        }
    }

    /// Build a `PgStore` with a custom store directory (for testing).
    pub fn with_store_dir(backend: B, store_dir: impl Into<String>) -> Self {
        Self {
            backend,
            store_dir: store_dir.into(),
        }
    }

    /// Borrow the underlying backend (e.g. to run a migration on the real one).
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// The store directory paths are computed against.
    #[must_use]
    pub fn store_dir(&self) -> &str {
        &self.store_dir
    }

    /// Read a blob **and validate** its content address: recompute the BLAKE3
    /// of the returned bytes and reject on mismatch. Use this on any read that
    /// crosses a durability/host boundary (a cross-runner pull, a substituter
    /// fetch) — it is the `get_validated` invariant on the Pg axis.
    ///
    /// # Errors
    /// [`StoreError::Internal`] if the stored bytes do not hash to the key.
    pub async fn get_validated_blob(
        &self,
        content_key: &[u8; 32],
    ) -> StoreResult<Option<Vec<u8>>> {
        match self.backend.get_blob(content_key).await.map_err(StoreError::from)? {
            None => Ok(None),
            Some(bytes) => {
                let actual = GraphHash::of(&bytes);
                if actual.as_bytes() != content_key {
                    return Err(PgError::HashMismatch {
                        expected: GraphHash(*content_key).to_string(),
                        actual: actual.to_string(),
                    }
                    .into());
                }
                Ok(Some(bytes))
            }
        }
    }
}

#[async_trait]
impl<B: PgBackend> Store for PgStore<B> {
    async fn query_path_info(&self, path: &StorePath) -> StoreResult<Option<PathInfo>> {
        let row = self
            .backend
            .get_path(&path.to_absolute_path())
            .await
            .map_err(StoreError::from)?;
        Ok(row.as_ref().map(PathInfo::from))
    }

    async fn is_valid_path(&self, path: &StorePath) -> StoreResult<bool> {
        self.backend
            .has_path(&path.to_absolute_path())
            .await
            .map_err(StoreError::from)
    }

    async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
        let paths = self.backend.all_paths().await.map_err(StoreError::from)?;
        Ok(paths
            .iter()
            .filter_map(|p| StorePath::from_absolute_path(p).ok())
            .collect())
    }

    async fn add_to_store(
        &self,
        name: &str,
        nar_data: &[u8],
        references: &[String],
    ) -> StoreResult<PathInfo> {
        // The content address of the NAR bytes — the durable content key.
        let content_hash = GraphHash::of(nar_data);
        // The Nix nar_hash metadata (`sha256:<hex>`), and the wire-compatible
        // content-addressed source store path (recursive sha256).
        let nar_sha256 = Sha256::digest(nar_data);
        let nar_hex = hex_lower(&nar_sha256);
        let mut nar_hash = String::with_capacity(7 + nar_hex.len());
        nar_hash.push_str("sha256:");
        nar_hash.push_str(&nar_hex);
        let path = compute_fixed_output_hash("sha256", &nar_hex, true, name);

        let row = PgPathRow {
            path,
            nar_hash,
            nar_size: nar_data.len() as i64,
            references: references.to_vec(),
            deriver: None,
            signatures: Vec::new(),
            registration_time: now_unix(),
            // The exact cppnix `ca` assertion string for a recursive-sha256
            // source path is left None rather than fabricated — the content
            // address is carried losslessly by `content_key` + the path digest.
            content_address: None,
            content_key: Some(*content_hash.as_bytes()),
        };
        let blob = PgBlob {
            content_key: *content_hash.as_bytes(),
            bytes: nar_data.to_vec(),
        };
        // Atomic: blob + metadata in one transaction.
        self.backend
            .upsert_path_atomic(&row, Some(&blob))
            .await
            .map_err(StoreError::from)?;
        Ok(PathInfo::from(&row))
    }

    async fn register_path(&self, info: &PathInfo) -> StoreResult<()> {
        // Metadata-only registration (a pre-built path whose bytes live
        // elsewhere / in the L3 object tier). No blob → content_key None.
        let row = PgPathRow::from_path_info(info, None);
        self.backend
            .upsert_path_atomic(&row, None)
            .await
            .map_err(StoreError::from)
    }

    async fn add_signatures(&self, path: &StorePath, signatures: &[String]) -> StoreResult<()> {
        let abs = path.to_absolute_path();
        let updated = self
            .backend
            .append_signatures(&abs, signatures)
            .await
            .map_err(StoreError::from)?;
        if updated {
            Ok(())
        } else {
            Err(StoreError::PathNotFound(abs))
        }
    }

    async fn delete_path(&self, path: &StorePath) -> StoreResult<u64> {
        let abs = path.to_absolute_path();
        match self
            .backend
            .delete_path(&abs)
            .await
            .map_err(StoreError::from)?
        {
            Some(freed) => Ok(freed.max(0) as u64),
            None => Err(StoreError::PathNotFound(abs)),
        }
    }

    async fn verify_store(&self) -> StoreResult<VerifyResult> {
        // Content-address integrity sweep: for every path carrying a blob,
        // recompute BLAKE3(blob) and compare to its key. This is the durable
        // half of the `get_validated` invariant, surfaced as `nix store verify`.
        let paths = self.backend.all_paths().await.map_err(StoreError::from)?;
        let mut result = VerifyResult::default();
        for path in paths {
            let Some(row) = self.backend.get_path(&path).await.map_err(StoreError::from)? else {
                continue; // vanished between listing and read — skip
            };
            result.total_checked += 1;
            let Some(key) = row.content_key else {
                // Metadata-only row — no blob bytes to hash-check.
                result.valid_count += 1;
                continue;
            };
            match self.backend.get_blob(&key).await.map_err(StoreError::from)? {
                Some(bytes) => {
                    let actual = GraphHash::of(&bytes);
                    if actual.as_bytes() == &key {
                        result.valid_count += 1;
                    } else {
                        result.corrupt.push(CorruptPath {
                            path: row.path,
                            expected_hash: GraphHash(key).to_string(),
                            actual_hash: actual.to_string(),
                        });
                    }
                }
                None => {
                    // Row points at a blob that isn't present — a broken
                    // (non-atomic) write. Reported as corrupt.
                    result.corrupt.push(CorruptPath {
                        path: row.path,
                        expected_hash: GraphHash(key).to_string(),
                        actual_hash: "<missing blob>".to_string(),
                    });
                }
            }
        }
        Ok(result)
    }

    // `query_references`, `compute_closure`, `collect_garbage`,
    // `query_referrers`, `optimise_store` fall through to the trait defaults.
    // The default `query_references` reads references from `query_path_info`,
    // which PgStore serves directly — correct as-is. GC / optimise are the
    // named incremental fill-ins (they return `NotSupported`, never a silent
    // wrong answer).
}

// ─────────────────────────────────────────────────────────────────────────
// helpers
// ─────────────────────────────────────────────────────────────────────────

const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

/// Lowercase-hex encode without an external dep (sui-compat's hex is crate-private).
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX_CHARS[(b >> 4) as usize] as char);
        s.push(HEX_CHARS[(b & 0x0f) as usize] as char);
    }
    s
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────
// InMemoryPgBackend — the mock/dev backend (non-durable)
// ─────────────────────────────────────────────────────────────────────────

/// An in-memory [`PgBackend`] — the test oracle **and** a non-durable dev
/// backend (the peer of [`LocalStore::open_in_memory`](crate::LocalStore)).
///
/// It honors the two backend invariants exactly like the real adapter must:
/// `upsert_path_atomic` writes blob + row under **one lock** (the mock's
/// transaction), and rejects a blob whose bytes do not hash to its
/// `content_key`. `all_paths` is sorted ascending (a `BTreeMap`), matching the
/// on-disk store's `order_by` so the two are differential-comparable.
#[derive(Default)]
pub struct InMemoryPgBackend {
    inner: std::sync::Mutex<InMemState>,
}

#[derive(Default)]
struct InMemState {
    paths: BTreeMap<String, PgPathRow>,
    blobs: BTreeMap<[u8; 32], Vec<u8>>,
}

impl InMemoryPgBackend {
    /// A fresh, empty in-memory backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of metadata rows (test/inspection helper).
    #[must_use]
    pub fn path_count(&self) -> usize {
        self.inner.lock().expect("poisoned").paths.len()
    }

    /// Number of stored blobs (test/inspection helper).
    #[must_use]
    pub fn blob_count(&self) -> usize {
        self.inner.lock().expect("poisoned").blobs.len()
    }

    /// Corrupt a stored blob in place — a bitrot simulator for the integrity
    /// tests. Returns `true` if the blob existed and was mutated.
    pub fn corrupt_blob_for_test(&self, content_key: &[u8; 32], new_bytes: Vec<u8>) -> bool {
        let mut st = self.inner.lock().expect("poisoned");
        if st.blobs.contains_key(content_key) {
            st.blobs.insert(*content_key, new_bytes);
            true
        } else {
            false
        }
    }
}

#[async_trait]
impl PgBackend for InMemoryPgBackend {
    async fn upsert_path_atomic(
        &self,
        row: &PgPathRow,
        blob: Option<&PgBlob>,
    ) -> Result<(), PgError> {
        // Write-boundary content-address check (parse-don't-validate): reject
        // a blob whose bytes don't hash to its key BEFORE anything is committed.
        if let Some(b) = blob {
            let actual = GraphHash::of(&b.bytes);
            if actual.as_bytes() != &b.content_key {
                return Err(PgError::HashMismatch {
                    expected: GraphHash(b.content_key).to_string(),
                    actual: actual.to_string(),
                });
            }
        }
        // One lock == one transaction: blob and row land together or not at all.
        let mut st = self.inner.lock().expect("poisoned");
        if let Some(b) = blob {
            st.blobs.insert(b.content_key, b.bytes.clone());
        }
        st.paths.insert(row.path.clone(), row.clone());
        Ok(())
    }

    async fn get_path(&self, path: &str) -> Result<Option<PgPathRow>, PgError> {
        Ok(self.inner.lock().expect("poisoned").paths.get(path).cloned())
    }

    async fn get_blob(&self, content_key: &[u8; 32]) -> Result<Option<Vec<u8>>, PgError> {
        Ok(self
            .inner
            .lock()
            .expect("poisoned")
            .blobs
            .get(content_key)
            .cloned())
    }

    async fn has_path(&self, path: &str) -> Result<bool, PgError> {
        Ok(self.inner.lock().expect("poisoned").paths.contains_key(path))
    }

    async fn all_paths(&self) -> Result<Vec<String>, PgError> {
        Ok(self
            .inner
            .lock()
            .expect("poisoned")
            .paths
            .keys()
            .cloned()
            .collect())
    }

    async fn append_signatures(&self, path: &str, sigs: &[String]) -> Result<bool, PgError> {
        let mut st = self.inner.lock().expect("poisoned");
        match st.paths.get_mut(path) {
            Some(row) => {
                for s in sigs {
                    if !row.signatures.contains(s) {
                        row.signatures.push(s.clone());
                    }
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn delete_path(&self, path: &str) -> Result<Option<i64>, PgError> {
        let mut st = self.inner.lock().expect("poisoned");
        Ok(st.paths.remove(path).map(|row| row.nar_size))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// SqlxPgBackend — the real Postgres adapter (feature = "postgres")
// ─────────────────────────────────────────────────────────────────────────

/// The DDL the real Postgres backend expects. Two content-addressed tables in a
/// dedicated schema; the metadata row points at its blob's content key. Applied
/// by [`SqlxPgBackend::migrate`].
///
/// This is a static schema string, not a `format!()` of SQL — the parameterised
/// data queries all use `sqlx`'s bind API (`$1`, `$2`, …), never string
/// interpolation.
#[cfg(feature = "postgres")]
pub const PGSTORE_DDL: &str = "\
CREATE SCHEMA IF NOT EXISTS sui_store;
CREATE TABLE IF NOT EXISTS sui_store.nar_blobs (
    content_key BYTEA PRIMARY KEY,
    bytes       BYTEA NOT NULL
);
CREATE TABLE IF NOT EXISTS sui_store.valid_paths (
    path              TEXT PRIMARY KEY,
    nar_hash          TEXT NOT NULL,
    nar_size          BIGINT NOT NULL,
    references_        TEXT[] NOT NULL DEFAULT '{}',
    deriver           TEXT,
    signatures        TEXT[] NOT NULL DEFAULT '{}',
    registration_time BIGINT NOT NULL DEFAULT 0,
    content_address   TEXT,
    content_key       BYTEA REFERENCES sui_store.nar_blobs(content_key)
);";

/// A real Postgres [`PgBackend`] over an `sqlx::PgPool`.
///
/// **Tier:** compiles + is authored, but **not** integration-tested against a
/// live Postgres in this crate (offline). The atomic upsert runs both writes in
/// one `sqlx::Transaction`; the write-boundary hash check mirrors the mock.
/// Standing up a real PG and running the differential-parity gate is the
/// [`PgStoreTier::LiveClusterProven`] milestone.
#[cfg(feature = "postgres")]
pub struct SqlxPgBackend {
    pool: sqlx::PgPool,
}

#[cfg(feature = "postgres")]
impl SqlxPgBackend {
    /// Connect a bounded pool to `url` (e.g.
    /// `postgres://user@postgres.super-cache-ci.svc.cluster.local:5432/sui`).
    ///
    /// # Errors
    /// [`PgError::Backend`] on a connection failure.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, PgError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(|e| PgError::Backend(e.to_string()))?;
        Ok(Self { pool })
    }

    /// Wrap an already-built pool.
    #[must_use]
    pub fn from_pool(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }

    /// Apply [`PGSTORE_DDL`] (idempotent — every statement is `IF NOT EXISTS`).
    ///
    /// # Errors
    /// [`PgError::Backend`] on a DDL failure.
    pub async fn migrate(&self) -> Result<(), PgError> {
        sqlx::raw_sql(PGSTORE_DDL)
            .execute(&self.pool)
            .await
            .map_err(|e| PgError::Backend(e.to_string()))?;
        Ok(())
    }
}

#[cfg(feature = "postgres")]
#[async_trait]
impl PgBackend for SqlxPgBackend {
    async fn upsert_path_atomic(
        &self,
        row: &PgPathRow,
        blob: Option<&PgBlob>,
    ) -> Result<(), PgError> {
        // Write-boundary content-address check (same invariant as the mock).
        if let Some(b) = blob {
            let actual = GraphHash::of(&b.bytes);
            if actual.as_bytes() != &b.content_key {
                return Err(PgError::HashMismatch {
                    expected: GraphHash(b.content_key).to_string(),
                    actual: actual.to_string(),
                });
            }
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| PgError::Backend(e.to_string()))?;

        if let Some(b) = blob {
            sqlx::query(
                "INSERT INTO sui_store.nar_blobs (content_key, bytes) VALUES ($1, $2) \
                 ON CONFLICT (content_key) DO NOTHING",
            )
            .bind(&b.content_key[..])
            .bind(&b.bytes[..])
            .execute(&mut *tx)
            .await
            .map_err(|e| PgError::Backend(e.to_string()))?;
        }

        sqlx::query(
            "INSERT INTO sui_store.valid_paths \
             (path, nar_hash, nar_size, references_, deriver, signatures, \
              registration_time, content_address, content_key) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (path) DO UPDATE SET \
              nar_hash = EXCLUDED.nar_hash, nar_size = EXCLUDED.nar_size, \
              references_ = EXCLUDED.references_, deriver = EXCLUDED.deriver, \
              signatures = EXCLUDED.signatures, \
              registration_time = EXCLUDED.registration_time, \
              content_address = EXCLUDED.content_address, \
              content_key = EXCLUDED.content_key",
        )
        .bind(&row.path)
        .bind(&row.nar_hash)
        .bind(row.nar_size)
        .bind(&row.references)
        .bind(&row.deriver)
        .bind(&row.signatures)
        .bind(row.registration_time)
        .bind(&row.content_address)
        .bind(row.content_key.as_ref().map(|k| k.to_vec()))
        .execute(&mut *tx)
        .await
        .map_err(|e| PgError::Backend(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| PgError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn get_path(&self, path: &str) -> Result<Option<PgPathRow>, PgError> {
        let maybe = sqlx::query(
            "SELECT path, nar_hash, nar_size, references_, deriver, signatures, \
                    registration_time, content_address, content_key \
             FROM sui_store.valid_paths WHERE path = $1",
        )
        .bind(path)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| PgError::Backend(e.to_string()))?;

        maybe.map(row_from_pg).transpose()
    }

    async fn get_blob(&self, content_key: &[u8; 32]) -> Result<Option<Vec<u8>>, PgError> {
        use sqlx::Row;
        let maybe = sqlx::query("SELECT bytes FROM sui_store.nar_blobs WHERE content_key = $1")
            .bind(&content_key[..])
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| PgError::Backend(e.to_string()))?;
        Ok(maybe.map(|r| r.get::<Vec<u8>, _>("bytes")))
    }

    async fn has_path(&self, path: &str) -> Result<bool, PgError> {
        let maybe = sqlx::query("SELECT 1 FROM sui_store.valid_paths WHERE path = $1")
            .bind(path)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| PgError::Backend(e.to_string()))?;
        Ok(maybe.is_some())
    }

    async fn all_paths(&self) -> Result<Vec<String>, PgError> {
        use sqlx::Row;
        let rows = sqlx::query("SELECT path FROM sui_store.valid_paths ORDER BY path ASC")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| PgError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>("path")).collect())
    }

    async fn append_signatures(&self, path: &str, sigs: &[String]) -> Result<bool, PgError> {
        // array_cat + dedup via a subquery keeps it one statement, one round-trip.
        let res = sqlx::query(
            "UPDATE sui_store.valid_paths \
             SET signatures = ( \
                SELECT array_agg(DISTINCT s) FROM unnest(signatures || $2::text[]) AS s \
             ) WHERE path = $1",
        )
        .bind(path)
        .bind(sigs)
        .execute(&self.pool)
        .await
        .map_err(|e| PgError::Backend(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }

    async fn delete_path(&self, path: &str) -> Result<Option<i64>, PgError> {
        use sqlx::Row;
        let maybe = sqlx::query(
            "DELETE FROM sui_store.valid_paths WHERE path = $1 RETURNING nar_size",
        )
        .bind(path)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| PgError::Backend(e.to_string()))?;
        Ok(maybe.map(|r| r.get::<i64, _>("nar_size")))
    }
}

#[cfg(feature = "postgres")]
fn row_from_pg(r: sqlx::postgres::PgRow) -> Result<PgPathRow, PgError> {
    use sqlx::Row;
    let content_key: Option<Vec<u8>> = r.get("content_key");
    let content_key = match content_key {
        None => None,
        Some(v) => {
            let arr: [u8; 32] = v
                .as_slice()
                .try_into()
                .map_err(|_| PgError::Serialization("content_key is not 32 bytes".to_string()))?;
            Some(arr)
        }
    };
    Ok(PgPathRow {
        path: r.get("path"),
        nar_hash: r.get("nar_hash"),
        nar_size: r.get("nar_size"),
        references: r.get("references_"),
        deriver: r.get("deriver"),
        signatures: r.get("signatures"),
        registration_time: r.get("registration_time"),
        content_address: r.get("content_address"),
        content_key,
    })
}

// ─────────────────────────────────────────────────────────────────────────
// Tests — the whole core proven against the in-memory oracle
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> PgStore<InMemoryPgBackend> {
        PgStore::new(InMemoryPgBackend::new())
    }

    fn sp(abs: &str) -> StorePath {
        StorePath::from_absolute_path(abs).expect("valid store path")
    }

    fn hello() -> StorePath {
        sp("/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1")
    }

    fn hello_info() -> PathInfo {
        PathInfo {
            path: hello().to_absolute_path(),
            nar_hash: "sha256:aaa".to_string(),
            nar_size: 5000,
            references: vec![
                "/nix/store/3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37".to_string(),
            ],
            deriver: Some("/nix/store/abc.drv".to_string()),
            signatures: vec!["key:sig".to_string()],
            registration_time: 1000,
            content_address: None,
        }
    }

    // ── required Store methods ────────────────────────────────

    #[tokio::test]
    async fn register_then_query_round_trips_path_info() {
        let s = store();
        s.register_path(&hello_info()).await.unwrap();
        let got = s.query_path_info(&hello()).await.unwrap().unwrap();
        assert_eq!(got, hello_info());
    }

    #[tokio::test]
    async fn is_valid_path_reflects_registration() {
        let s = store();
        assert!(!s.is_valid_path(&hello()).await.unwrap());
        s.register_path(&hello_info()).await.unwrap();
        assert!(s.is_valid_path(&hello()).await.unwrap());
    }

    #[tokio::test]
    async fn query_all_valid_paths_is_sorted() {
        let s = store();
        for p in [
            "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-bash-5.2",
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
            "/nix/store/3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37",
        ] {
            s.register_path(&PathInfo::new(p, "sha256:x")).await.unwrap();
        }
        let paths: Vec<String> = s
            .query_all_valid_paths()
            .await
            .unwrap()
            .iter()
            .map(StorePath::to_absolute_path)
            .collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "authoritative listing must be ascending");
        assert_eq!(paths.len(), 3);
    }

    #[tokio::test]
    async fn query_path_info_missing_is_none() {
        let s = store();
        assert!(s.query_path_info(&hello()).await.unwrap().is_none());
    }

    // ── add_to_store: content-addressing + atomicity ─────────

    #[tokio::test]
    async fn add_to_store_is_content_addressed_and_atomic() {
        let s = store();
        let nar = b"the NAR bytes of a built path";
        let info = s.add_to_store("hello-2.12.1", nar, &[]).await.unwrap();

        // The returned path is deterministic from the NAR content.
        assert!(info.path.starts_with("/nix/store/"));
        assert!(info.path.ends_with("-hello-2.12.1"));
        assert!(info.nar_hash.starts_with("sha256:"));
        assert_eq!(info.nar_size, nar.len() as i64);

        // BOTH the metadata row AND the blob landed (atomic write-through).
        assert_eq!(s.backend().path_count(), 1);
        assert_eq!(s.backend().blob_count(), 1);

        // The stored blob is retrievable AND validates against its key.
        let key = *GraphHash::of(nar).as_bytes();
        let bytes = s.get_validated_blob(&key).await.unwrap().unwrap();
        assert_eq!(bytes, nar);
    }

    #[tokio::test]
    async fn add_to_store_same_bytes_same_path_idempotent() {
        let s = store();
        let nar = b"identical bytes";
        let a = s.add_to_store("pkg", nar, &[]).await.unwrap();
        let b = s.add_to_store("pkg", nar, &[]).await.unwrap();
        assert_eq!(a.path, b.path, "content-address ⇒ same path");
        assert_eq!(s.backend().path_count(), 1);
        assert_eq!(s.backend().blob_count(), 1);
    }

    #[tokio::test]
    async fn add_to_store_different_bytes_different_path() {
        let s = store();
        let a = s.add_to_store("pkg", b"aaaa", &[]).await.unwrap();
        let b = s.add_to_store("pkg", b"bbbb", &[]).await.unwrap();
        assert_ne!(a.path, b.path);
        assert_eq!(s.backend().blob_count(), 2);
    }

    #[tokio::test]
    async fn add_to_store_preserves_references() {
        let s = store();
        let refs = vec![
            "/nix/store/3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37".to_string(),
        ];
        let info = s.add_to_store("pkg", b"body", &refs).await.unwrap();
        assert_eq!(info.references, refs);
        let sp = StorePath::from_absolute_path(&info.path).unwrap();
        let queried = s.query_references(&sp).await.unwrap();
        assert_eq!(queried.len(), 1);
    }

    // ── content-address integrity (the never-fork / bitrot invariant) ──

    #[tokio::test]
    async fn get_validated_blob_rejects_bitrot() {
        let s = store();
        let nar = b"trusted bytes";
        let _ = s.add_to_store("pkg", nar, &[]).await.unwrap();
        let key = *GraphHash::of(nar).as_bytes();

        // Corrupt the durable blob under the same key (bitrot / tamper).
        assert!(s.backend().corrupt_blob_for_test(&key, b"EVIL".to_vec()));

        let err = s.get_validated_blob(&key).await.unwrap_err();
        assert!(
            err.to_string().contains("mismatch") || matches!(err, StoreError::Internal(_)),
            "validated read must reject bitrot, got {err:?}"
        );
    }

    #[tokio::test]
    async fn upsert_rejects_blob_whose_bytes_dont_hash_to_key() {
        // Parse-don't-validate at the write boundary: a lying content_key
        // cannot be committed.
        let backend = InMemoryPgBackend::new();
        let bad = PgBlob {
            content_key: [0u8; 32], // does not hash "real bytes"
            bytes: b"real bytes".to_vec(),
        };
        let row = PgPathRow::from_path_info(
            &PathInfo::new("/nix/store/00000000000000000000000000000000-x", "sha256:x"),
            Some([0u8; 32]),
        );
        let err = backend.upsert_path_atomic(&row, Some(&bad)).await.unwrap_err();
        assert!(matches!(err, PgError::HashMismatch { .. }));
    }

    #[tokio::test]
    async fn verify_store_passes_for_intact_blobs() {
        let s = store();
        let _ = s.add_to_store("a", b"aaaa", &[]).await.unwrap();
        let _ = s.add_to_store("b", b"bbbb", &[]).await.unwrap();
        s.register_path(&hello_info()).await.unwrap(); // metadata-only, no blob

        let r = s.verify_store().await.unwrap();
        assert_eq!(r.total_checked, 3);
        assert_eq!(r.valid_count, 3);
        assert!(r.corrupt.is_empty());
    }

    #[tokio::test]
    async fn verify_store_flags_corrupt_blob() {
        let s = store();
        let info = s.add_to_store("a", b"aaaa", &[]).await.unwrap();
        let key = *GraphHash::of(b"aaaa").as_bytes();
        assert!(s.backend().corrupt_blob_for_test(&key, b"zzzz".to_vec()));

        let r = s.verify_store().await.unwrap();
        assert_eq!(r.total_checked, 1);
        assert_eq!(r.valid_count, 0);
        assert_eq!(r.corrupt.len(), 1);
        assert_eq!(r.corrupt[0].path, info.path);
    }

    // ── signatures + delete ──────────────────────────────────

    #[tokio::test]
    async fn add_signatures_appends_and_dedups() {
        let s = store();
        s.register_path(&hello_info()).await.unwrap();
        s.add_signatures(&hello(), &["k2:sig2".to_string()]).await.unwrap();
        // idempotent: re-adding an existing sig doesn't duplicate it
        s.add_signatures(&hello(), &["key:sig".to_string()]).await.unwrap();

        let info = s.query_path_info(&hello()).await.unwrap().unwrap();
        assert!(info.signatures.contains(&"key:sig".to_string()));
        assert!(info.signatures.contains(&"k2:sig2".to_string()));
        assert_eq!(info.signatures.len(), 2);
    }

    #[tokio::test]
    async fn add_signatures_missing_path_errors() {
        let s = store();
        let err = s.add_signatures(&hello(), &["x:y".to_string()]).await.unwrap_err();
        assert!(err.is_path_not_found());
    }

    #[tokio::test]
    async fn delete_path_returns_freed_bytes_and_removes() {
        let s = store();
        s.register_path(&hello_info()).await.unwrap();
        let freed = s.delete_path(&hello()).await.unwrap();
        assert_eq!(freed, 5000);
        assert!(!s.is_valid_path(&hello()).await.unwrap());
    }

    #[tokio::test]
    async fn delete_path_missing_errors() {
        let s = store();
        let err = s.delete_path(&hello()).await.unwrap_err();
        assert!(err.is_path_not_found());
    }

    // ── default trait methods still work over PgStore ────────

    #[tokio::test]
    async fn compute_closure_walks_pg_backed_refs() {
        let s = store();
        let leaf = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-bash-5.2";
        let mid = "/nix/store/3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37";
        let root = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1";
        s.register_path(&PathInfo {
            references: vec![mid.to_string()],
            ..PathInfo::new(root, "sha256:r")
        })
        .await
        .unwrap();
        s.register_path(&PathInfo {
            references: vec![leaf.to_string()],
            ..PathInfo::new(mid, "sha256:m")
        })
        .await
        .unwrap();
        s.register_path(&PathInfo::new(leaf, "sha256:l")).await.unwrap();

        let closure = s.compute_closure(&[sp(root)]).await.unwrap();
        assert_eq!(closure.len(), 3);
    }

    // ── dyn dispatch (the AppState / Arc<dyn Store> pattern) ──

    #[tokio::test]
    async fn arc_dyn_store_dispatch() {
        let s: std::sync::Arc<dyn Store> = std::sync::Arc::new(store());
        // PgStore drops straight into an Arc<dyn Store> consumer.
        assert!(!s.is_valid_path(&hello()).await.unwrap());
    }

    // ── honesty gate ─────────────────────────────────────────

    #[test]
    fn honest_gate_pgstore_is_mock_parity_proven() {
        // The shipped tier is MockParityProven. Bumping PGSTORE_TIER to
        // LiveClusterProven without a live-Postgres differential-parity test is
        // a round-up — this gate fails the build if that happens.
        assert_eq!(PGSTORE_TIER, PgStoreTier::MockParityProven);
    }

    #[test]
    fn error_folds_backend_to_database_and_mismatch_to_internal() {
        let db: StoreError = PgError::Backend("boom".to_string()).into();
        assert!(matches!(db, StoreError::Database(_)));
        let integ: StoreError = PgError::HashMismatch {
            expected: "a".to_string(),
            actual: "b".to_string(),
        }
        .into();
        assert!(matches!(integ, StoreError::Internal(_)));
    }
}
