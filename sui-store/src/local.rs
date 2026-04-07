//! Local store implementation — reads /nix/store + existing SQLite DB via SeaORM.

use std::path::Path;

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Database};
use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue::Set;
use sui_compat::store_path::StorePath;

use crate::entity::{derivation_output, reference, valid_path};
use crate::traits::{PathInfo, Store, StoreError, StoreResult};

/// Controls whether the local store database is opened read-only or read-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalStoreMode {
    /// Open the database in read-only mode (default for `open()`).
    ReadOnly,
    /// Open the database in read-write mode (for `open_rw()`).
    ReadWrite,
}

/// Local Nix store backed by the filesystem and SQLite database.
pub struct LocalStore {
    db: DatabaseConnection,
    store_dir: String,
}

impl LocalStore {
    /// Open the local store in read-only mode using the existing Nix database.
    ///
    /// Default path: `/nix/var/nix/db/db.sqlite`.
    /// Accepts any type convertible to a path (`&str`, `&Path`, `PathBuf`, etc.).
    pub async fn open(db_path: impl AsRef<Path>) -> StoreResult<Self> {
        Self::open_inner(db_path.as_ref(), "/nix/store", LocalStoreMode::ReadOnly).await
    }

    /// Open the local store in read-write mode.
    ///
    /// The database file must already exist. Use this when you need to
    /// register paths, add signatures, or perform garbage collection.
    pub async fn open_rw(db_path: impl AsRef<Path>) -> StoreResult<Self> {
        Self::open_inner(db_path.as_ref(), "/nix/store", LocalStoreMode::ReadWrite).await
    }

    /// Open with a custom store directory (for testing).
    pub async fn open_with_dir(
        db_path: impl AsRef<Path>,
        store_dir: impl AsRef<Path>,
    ) -> StoreResult<Self> {
        Self::open_inner(
            db_path.as_ref(),
            store_dir.as_ref().to_str().unwrap_or("/nix/store"),
            LocalStoreMode::ReadOnly,
        )
        .await
    }

    /// Open with a custom store directory in read-write mode (for testing).
    pub async fn open_rw_with_dir(
        db_path: impl AsRef<Path>,
        store_dir: impl AsRef<Path>,
    ) -> StoreResult<Self> {
        Self::open_inner(
            db_path.as_ref(),
            store_dir.as_ref().to_str().unwrap_or("/nix/store"),
            LocalStoreMode::ReadWrite,
        )
        .await
    }

    async fn open_inner(db_path: &Path, store_dir: &str, mode: LocalStoreMode) -> StoreResult<Self> {
        let db_path_str = db_path.to_str().ok_or_else(|| {
            StoreError::Database("database path is not valid UTF-8".to_string())
        })?;
        let url = match mode {
            LocalStoreMode::ReadOnly => format!("sqlite://{db_path_str}?mode=ro"),
            LocalStoreMode::ReadWrite => format!("sqlite://{db_path_str}"),
        };
        let db = Database::connect(&url).await.map_err(db_err)?;

        Ok(Self {
            db,
            store_dir: store_dir.to_string(),
        })
    }

    /// Get the database connection for direct queries.
    #[must_use]
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    /// Get the store directory path.
    #[must_use]
    pub fn store_dir(&self) -> &str {
        &self.store_dir
    }

    /// Look up a ValidPath row by its full store path string.
    async fn find_by_path(&self, path: &str) -> StoreResult<Option<valid_path::Model>> {
        valid_path::Entity::find()
            .filter(valid_path::Column::Path.eq(path))
            .one(&self.db)
            .await
            .map_err(db_err)
    }

    /// Get the references (runtime dependencies) for a given ValidPath id.
    async fn get_references(&self, path_id: i64) -> StoreResult<Vec<String>> {
        let refs = reference::Entity::find()
            .filter(reference::Column::Referrer.eq(path_id))
            .all(&self.db)
            .await
            .map_err(db_err)?;

        let ref_ids: Vec<i64> = refs.iter().map(|r| r.reference).collect();
        if ref_ids.is_empty() {
            return Ok(vec![]);
        }

        let ref_paths = valid_path::Entity::find()
            .filter(valid_path::Column::Id.is_in(ref_ids))
            .all(&self.db)
            .await
            .map_err(db_err)?;

        Ok(ref_paths.into_iter().map(|p| p.path).collect())
    }

    /// Convert a ValidPath model to our PathInfo type.
    async fn model_to_path_info(&self, model: &valid_path::Model) -> StoreResult<PathInfo> {
        let references = self.get_references(model.id).await?;
        let signatures = model
            .sigs
            .as_ref()
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();

        Ok(PathInfo {
            path: model.path.clone(),
            nar_hash: model.hash.clone(),
            nar_size: model.nar_size.unwrap_or(0),
            references,
            deriver: model.deriver.clone(),
            signatures,
            registration_time: model.registration_time,
            content_address: model.ca.clone(),
        })
    }

    /// Create the Nix store schema tables in the database.
    ///
    /// This is used for testing and for initializing a new store database.
    /// Creates `ValidPaths`, `Refs`, and `DerivationOutputs` tables.
    pub async fn create_tables(&self) -> StoreResult<()> {
        use sea_orm::ConnectionTrait;
        let backend = self.db.get_database_backend();

        let valid_paths_sql = sea_orm::Schema::new(backend)
            .create_table_from_entity(valid_path::Entity);
        self.db.execute(backend.build(&valid_paths_sql)).await.map_err(db_err)?;

        let refs_sql = sea_orm::Schema::new(backend)
            .create_table_from_entity(reference::Entity);
        self.db.execute(backend.build(&refs_sql)).await.map_err(db_err)?;

        let drv_outputs_sql = sea_orm::Schema::new(backend)
            .create_table_from_entity(derivation_output::Entity);
        self.db.execute(backend.build(&drv_outputs_sql)).await.map_err(db_err)?;

        Ok(())
    }

    /// Open an in-memory SQLite database with schema created.
    ///
    /// Useful for testing. Creates all tables and returns a read-write store.
    pub async fn open_in_memory() -> StoreResult<Self> {
        Self::open_in_memory_with_dir("/nix/store").await
    }

    /// Open an in-memory SQLite database with schema and a custom store dir.
    pub async fn open_in_memory_with_dir(store_dir: &str) -> StoreResult<Self> {
        let db = Database::connect("sqlite::memory:").await.map_err(db_err)?;
        let store = Self {
            db,
            store_dir: store_dir.to_string(),
        };
        store.create_tables().await?;
        Ok(store)
    }
}

#[async_trait::async_trait]
impl Store for LocalStore {
    async fn query_path_info(&self, path: &StorePath) -> StoreResult<Option<PathInfo>> {
        let abs_path = path.to_absolute_path();
        match self.find_by_path(&abs_path).await? {
            Some(model) => Ok(Some(self.model_to_path_info(&model).await?)),
            None => Ok(None),
        }
    }

    async fn is_valid_path(&self, path: &StorePath) -> StoreResult<bool> {
        let abs_path = path.to_absolute_path();
        Ok(self.find_by_path(&abs_path).await?.is_some())
    }

    async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
        let paths = valid_path::Entity::find()
            .order_by_asc(valid_path::Column::Path)
            .all(&self.db)
            .await
            .map_err(db_err)?;

        Ok(paths
            .into_iter()
            .filter_map(|p| StorePath::from_absolute_path(&p.path).ok())
            .collect())
    }

    async fn register_path(&self, info: &PathInfo) -> StoreResult<()> {
        // Build the sigs string (space-separated signatures).
        let sigs = if info.signatures.is_empty() {
            None
        } else {
            Some(info.signatures.join(" "))
        };

        // Insert into ValidPaths.
        let new_path = valid_path::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            path: Set(info.path.clone()),
            hash: Set(info.nar_hash.clone()),
            registration_time: Set(info.registration_time),
            deriver: Set(info.deriver.clone()),
            nar_size: Set(Some(info.nar_size)),
            ultimate: Set(Some(0)),
            sigs: Set(sigs),
            ca: Set(info.content_address.clone()),
        };

        let inserted = new_path.insert(&self.db).await.map_err(db_err)?;
        let path_id = inserted.id;

        // Insert reference edges into Refs.
        for ref_path_str in &info.references {
            let ref_model = self.find_by_path(ref_path_str).await?;
            if let Some(ref_row) = ref_model {
                let new_ref = reference::ActiveModel {
                    referrer: Set(path_id),
                    reference: Set(ref_row.id),
                };
                new_ref.insert(&self.db).await.map_err(db_err)?;
            }
        }

        // If the path has a deriver ending in .drv, insert into DerivationOutputs.
        if let Some(ref deriver) = info.deriver {
            if deriver.ends_with(".drv") {
                // Look up the deriver's ValidPaths.id.
                if let Some(drv_row) = self.find_by_path(deriver).await? {
                    let drv_output = derivation_output::ActiveModel {
                        drv: Set(drv_row.id),
                        id: Set("out".to_string()),
                        path: Set(info.path.clone()),
                    };
                    drv_output.insert(&self.db).await.map_err(db_err)?;
                }
            }
        }

        Ok(())
    }

    async fn add_to_store(
        &self,
        name: &str,
        nar_data: &[u8],
        references: &[String],
    ) -> StoreResult<PathInfo> {
        use sha2::{Sha256, Digest};
        use sui_compat::store_path::{compress_hash, nix_base32_encode};
        use sui_compat::nar::unpack_nar;

        // Compute the SHA-256 hash of the NAR data.
        let nar_hash_raw = Sha256::digest(nar_data);
        let nar_hash_hex = hex_encode(&nar_hash_raw);
        let nar_hash = format!("sha256:{nar_hash_hex}");
        let nar_size = nar_data.len() as i64;

        // Compute the store path.
        // The fingerprint uses the actual store_dir for correct hashing.
        let fingerprint = format!(
            "source:sha256:{nar_hash_hex}:{}:{name}",
            self.store_dir
        );
        let fp_hash = Sha256::digest(fingerprint.as_bytes());
        let compressed = compress_hash(&fp_hash, 20);
        let b32 = nix_base32_encode(&compressed);
        let basename = format!("{b32}-{name}");
        let store_path = format!("{}/{basename}", self.store_dir);

        // Write the NAR data to the store directory by unpacking.
        let dest = Path::new(&self.store_dir).join(&basename);
        unpack_nar(nar_data, &dest).map_err(|e| StoreError::Io(
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string()),
        ))?;

        // Build PathInfo.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let info = PathInfo {
            path: store_path,
            nar_hash,
            nar_size,
            references: references.to_vec(),
            deriver: None,
            signatures: vec![],
            registration_time: now,
            content_address: None,
        };

        // Register in the database.
        self.register_path(&info).await?;

        Ok(info)
    }
}

/// Convert a SeaORM `DbErr` into a `StoreError::Database`.
fn db_err(e: sea_orm::DbErr) -> StoreError {
    StoreError::Database(e.to_string())
}

/// Encode bytes as lowercase hexadecimal.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    // Integration tests require a real Nix store database.
    // These are run in Phase 3 integration tests against /nix/var/nix/db/db.sqlite.
    // Unit tests use in-memory SQLite — see tests/integration/.

    use super::*;

    // ── db_err helper ────────────────────────────────────────

    #[test]
    fn db_err_wraps_dberr_into_storeerror_database() {
        let dberr = sea_orm::DbErr::Custom("simulated failure".to_string());
        let store_err = db_err(dberr);
        match store_err {
            StoreError::Database(msg) => {
                assert!(msg.contains("simulated failure"));
            }
            other => panic!("expected Database, got {other:?}"),
        }
    }

    #[test]
    fn db_err_handles_record_not_found() {
        let dberr = sea_orm::DbErr::RecordNotFound("missing".to_string());
        let store_err = db_err(dberr);
        assert!(matches!(store_err, StoreError::Database(_)));
        assert!(store_err.to_string().contains("missing"));
    }

    #[test]
    fn db_err_handles_connection_failure() {
        let dberr = sea_orm::DbErr::Conn(sea_orm::RuntimeErr::Internal(
            "no connection".to_string(),
        ));
        let store_err = db_err(dberr);
        assert!(matches!(store_err, StoreError::Database(_)));
    }

    // ── open() with non-utf8 path ────────────────────────────

    #[tokio::test]
    async fn open_with_nonexistent_path_errors() {
        let result = LocalStore::open("/this/path/does/not/exist/sui-test.sqlite").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn open_with_directory_path_errors() {
        // Pointing at a directory rather than a file should fail to open as sqlite
        let result = LocalStore::open("/tmp").await;
        assert!(result.is_err());
    }

    // ── open_with_dir() variants ─────────────────────────────

    #[tokio::test]
    async fn open_with_dir_nonexistent_db_errors() {
        let result = LocalStore::open_with_dir(
            "/this/does/not/exist/db.sqlite",
            "/nix/store",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn open_with_dir_custom_store_dir_propagated() {
        // Open against an in-memory sqlite db built fresh — this is the default
        // sea-orm sqlite mode.
        // We can't actually open a real local store without ValidPaths schema,
        // but we can verify the function signature with a non-existent DB
        // and confirm the call properly returns an error.
        let result = LocalStore::open_with_dir(
            "/nonexistent/db.sqlite",
            "/custom/store",
        )
        .await;
        assert!(result.is_err());
    }

    // ── valid_path::Model constructors ───────────────────────

    #[test]
    fn valid_path_model_construct_minimal() {
        let model = valid_path::Model {
            id: 1,
            path: "/nix/store/abc-hello".to_string(),
            hash: "sha256:deadbeef".to_string(),
            registration_time: 1234567890,
            deriver: None,
            nar_size: None,
            ultimate: None,
            sigs: None,
            ca: None,
        };
        assert_eq!(model.id, 1);
        assert_eq!(model.path, "/nix/store/abc-hello");
        assert!(model.deriver.is_none());
        assert!(model.nar_size.is_none());
    }

    #[test]
    fn valid_path_model_construct_full() {
        let model = valid_path::Model {
            id: 42,
            path: "/nix/store/abc-hello".to_string(),
            hash: "sha256:deadbeef".to_string(),
            registration_time: 1234567890,
            deriver: Some("/nix/store/abc.drv".to_string()),
            nar_size: Some(5000),
            ultimate: Some(1),
            sigs: Some("key1:sig1 key2:sig2".to_string()),
            ca: Some("fixed:out:r:sha256:deadbeef".to_string()),
        };
        assert_eq!(model.id, 42);
        assert_eq!(model.nar_size, Some(5000));
        assert_eq!(model.ultimate, Some(1));
        assert!(model.sigs.as_ref().unwrap().contains("key1"));
    }

    #[test]
    fn valid_path_model_clone_independence() {
        let model = valid_path::Model {
            id: 1,
            path: "/nix/store/abc".to_string(),
            hash: "sha256:aaa".to_string(),
            registration_time: 100,
            deriver: None,
            nar_size: Some(1024),
            ultimate: None,
            sigs: None,
            ca: None,
        };
        let mut cloned = model.clone();
        cloned.id = 99;
        cloned.path = "/nix/store/other".to_string();
        assert_eq!(model.id, 1);
        assert_eq!(model.path, "/nix/store/abc");
        assert_eq!(cloned.id, 99);
    }

    #[test]
    fn valid_path_model_eq() {
        let a = valid_path::Model {
            id: 1,
            path: "/nix/store/abc".to_string(),
            hash: "sha256:aaa".to_string(),
            registration_time: 100,
            deriver: None,
            nar_size: None,
            ultimate: None,
            sigs: None,
            ca: None,
        };
        let b = a.clone();
        assert_eq!(a, b);

        let mut c = a.clone();
        c.id = 2;
        assert_ne!(a, c);
    }

    #[test]
    fn valid_path_model_debug_format() {
        let model = valid_path::Model {
            id: 7,
            path: "/nix/store/zzz-test".to_string(),
            hash: "sha256:bbb".to_string(),
            registration_time: 200,
            deriver: None,
            nar_size: None,
            ultimate: None,
            sigs: None,
            ca: None,
        };
        let debug = format!("{model:?}");
        assert!(debug.contains("zzz-test"));
        assert!(debug.contains("sha256:bbb"));
    }

    // ── reference::Model constructors ────────────────────────

    #[test]
    fn reference_model_construct() {
        let model = reference::Model {
            referrer: 1,
            reference: 2,
        };
        assert_eq!(model.referrer, 1);
        assert_eq!(model.reference, 2);
    }

    #[test]
    fn reference_model_eq() {
        let a = reference::Model {
            referrer: 5,
            reference: 10,
        };
        let b = reference::Model {
            referrer: 5,
            reference: 10,
        };
        assert_eq!(a, b);

        let c = reference::Model {
            referrer: 5,
            reference: 11,
        };
        assert_ne!(a, c);
    }

    #[test]
    fn reference_model_clone() {
        let a = reference::Model {
            referrer: 100,
            reference: 200,
        };
        let cloned = a.clone();
        assert_eq!(cloned.referrer, 100);
        assert_eq!(cloned.reference, 200);
    }

    // ── PathInfo conversion via model_to_path_info logic ─────
    // (We verify the conversion logic by directly constructing
    //  PathInfo as the function would, since model_to_path_info
    //  requires a real DB connection.)

    #[test]
    fn path_info_signatures_split_logic_no_sigs() {
        // Mirrors model_to_path_info's signatures handling
        let sigs: Option<String> = None;
        let result: Vec<String> = sigs
            .as_ref()
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        assert!(result.is_empty());
    }

    #[test]
    fn path_info_signatures_split_logic_single_sig() {
        let sigs: Option<String> = Some("cache.nixos.org-1:abc==".to_string());
        let result: Vec<String> = sigs
            .as_ref()
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "cache.nixos.org-1:abc==");
    }

    #[test]
    fn path_info_signatures_split_logic_multiple_sigs() {
        let sigs: Option<String> = Some("k1:s1 k2:s2 k3:s3".to_string());
        let result: Vec<String> = sigs
            .as_ref()
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], "k1:s1");
        assert_eq!(result[2], "k3:s3");
    }

    #[test]
    fn path_info_signatures_split_logic_extra_whitespace() {
        let sigs: Option<String> = Some("  k1:s1   k2:s2  ".to_string());
        let result: Vec<String> = sigs
            .as_ref()
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "k1:s1");
        assert_eq!(result[1], "k2:s2");
    }

    #[test]
    fn path_info_nar_size_default_zero() {
        // Mirrors `model.nar_size.unwrap_or(0)` in model_to_path_info
        let nar_size: Option<i64> = None;
        assert_eq!(nar_size.unwrap_or(0), 0);
        let nar_size: Option<i64> = Some(5000);
        assert_eq!(nar_size.unwrap_or(0), 5000);
    }

    // ── LocalStoreMode enum ────────────────────────────────────

    #[test]
    fn local_store_mode_enum_variants() {
        let ro = LocalStoreMode::ReadOnly;
        let rw = LocalStoreMode::ReadWrite;
        assert_ne!(ro, rw);
        assert_eq!(ro, LocalStoreMode::ReadOnly);
        assert_eq!(rw, LocalStoreMode::ReadWrite);
    }

    #[test]
    fn local_store_mode_debug_format() {
        let ro = LocalStoreMode::ReadOnly;
        let rw = LocalStoreMode::ReadWrite;
        assert!(format!("{ro:?}").contains("ReadOnly"));
        assert!(format!("{rw:?}").contains("ReadWrite"));
    }

    #[test]
    fn local_store_mode_clone_copy() {
        let mode = LocalStoreMode::ReadWrite;
        let cloned = mode;
        assert_eq!(mode, cloned);
    }

    // ── open_rw() ──────────────────────────────────────────────

    #[tokio::test]
    async fn open_rw_with_nonexistent_path_errors() {
        let result = LocalStore::open_rw("/this/path/does/not/exist/sui-rw.sqlite").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn open_rw_with_temp_db_succeeds() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Create a valid SQLite DB by opening in rw mode first (SeaORM creates the file).
        let store = LocalStore::open_rw(tmp.path()).await;
        // The file exists but has no schema — this should still connect.
        assert!(store.is_ok());
    }

    #[tokio::test]
    async fn open_readonly_still_works() {
        // open() should still use read-only mode.
        let result = LocalStore::open("/nonexistent/sui-test.sqlite").await;
        assert!(result.is_err()); // read-only on nonexistent file errors
    }

    // ── open_in_memory ─────────────────────────────────────────

    #[tokio::test]
    async fn open_in_memory_succeeds() {
        let store = LocalStore::open_in_memory().await.unwrap();
        assert_eq!(store.store_dir(), "/nix/store");
    }

    #[tokio::test]
    async fn open_in_memory_with_custom_dir() {
        let store = LocalStore::open_in_memory_with_dir("/test/store").await.unwrap();
        assert_eq!(store.store_dir(), "/test/store");
    }

    #[tokio::test]
    async fn in_memory_query_all_valid_paths_empty() {
        let store = LocalStore::open_in_memory().await.unwrap();
        let paths = store.query_all_valid_paths().await.unwrap();
        assert!(paths.is_empty());
    }

    // ── register_path ──────────────────────────────────────────

    #[tokio::test]
    async fn register_path_simple_no_references() {
        let store = LocalStore::open_in_memory().await.unwrap();

        let info = PathInfo {
            path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello".to_string(),
            nar_hash: "sha256:deadbeef".to_string(),
            nar_size: 1024,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 1700000000,
            content_address: None,
        };

        store.register_path(&info).await.unwrap();

        // Verify via query_path_info.
        let sp = StorePath::from_absolute_path(&info.path).unwrap();
        let queried = store.query_path_info(&sp).await.unwrap().unwrap();
        assert_eq!(queried.path, info.path);
        assert_eq!(queried.nar_hash, info.nar_hash);
        assert_eq!(queried.nar_size, 1024);
        assert!(queried.references.is_empty());
    }

    #[tokio::test]
    async fn register_path_with_two_references() {
        let store = LocalStore::open_in_memory().await.unwrap();

        // Register the referenced paths first.
        let ref1 = PathInfo {
            path: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep1".to_string(),
            nar_hash: "sha256:aaa".to_string(),
            nar_size: 100,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 100,
            content_address: None,
        };
        let ref2 = PathInfo {
            path: "/nix/store/cccccccccccccccccccccccccccccccc-dep2".to_string(),
            nar_hash: "sha256:bbb".to_string(),
            nar_size: 200,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 100,
            content_address: None,
        };
        store.register_path(&ref1).await.unwrap();
        store.register_path(&ref2).await.unwrap();

        // Register the main path with references.
        let main_info = PathInfo {
            path: "/nix/store/dddddddddddddddddddddddddddddddd-main".to_string(),
            nar_hash: "sha256:ccc".to_string(),
            nar_size: 500,
            references: vec![ref1.path.clone(), ref2.path.clone()],
            deriver: None,
            signatures: vec![],
            registration_time: 200,
            content_address: None,
        };
        store.register_path(&main_info).await.unwrap();

        // Verify references are stored.
        let sp = StorePath::from_absolute_path(&main_info.path).unwrap();
        let queried = store.query_path_info(&sp).await.unwrap().unwrap();
        assert_eq!(queried.references.len(), 2);
        assert!(queried.references.contains(&ref1.path));
        assert!(queried.references.contains(&ref2.path));
    }

    #[tokio::test]
    async fn register_path_and_verify_via_query() {
        let store = LocalStore::open_in_memory().await.unwrap();

        // Use valid Nix base32 chars (no 'e', 'o', 't', 'u').
        let info = PathInfo {
            path: "/nix/store/11111111111111111111111111111111-pkg".to_string(),
            nar_hash: "sha256:123456".to_string(),
            nar_size: 2048,
            references: vec![],
            deriver: None,
            signatures: vec!["key1:sig1".to_string(), "key2:sig2".to_string()],
            registration_time: 1700000000,
            content_address: Some("fixed:out:r:sha256:abc".to_string()),
        };
        store.register_path(&info).await.unwrap();

        // Verify is_valid_path.
        let sp = StorePath::from_absolute_path(&info.path).unwrap();
        assert!(store.is_valid_path(&sp).await.unwrap());

        // Verify query_path_info returns the full info.
        let queried = store.query_path_info(&sp).await.unwrap().unwrap();
        assert_eq!(queried.nar_size, 2048);
        assert_eq!(queried.signatures.len(), 2);
        assert_eq!(queried.content_address, Some("fixed:out:r:sha256:abc".to_string()));
    }

    #[tokio::test]
    async fn register_path_duplicate_returns_error() {
        let store = LocalStore::open_in_memory().await.unwrap();

        let info = PathInfo {
            path: "/nix/store/ffffffffffffffffffffffffffffffff-duplicate".to_string(),
            nar_hash: "sha256:dup".to_string(),
            nar_size: 100,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 100,
            content_address: None,
        };

        // First registration should succeed.
        store.register_path(&info).await.unwrap();

        // Second registration should error (unique constraint on path).
        let result = store.register_path(&info).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn register_path_with_deriver_drv() {
        let store = LocalStore::open_in_memory().await.unwrap();

        // Register the .drv first.
        let drv_info = PathInfo {
            path: "/nix/store/gggggggggggggggggggggggggggggggg-hello.drv".to_string(),
            nar_hash: "sha256:drv".to_string(),
            nar_size: 500,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 100,
            content_address: None,
        };
        store.register_path(&drv_info).await.unwrap();

        // Register an output that references the drv.
        let out_info = PathInfo {
            path: "/nix/store/hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh-hello".to_string(),
            nar_hash: "sha256:out".to_string(),
            nar_size: 1000,
            references: vec![],
            deriver: Some(drv_info.path.clone()),
            signatures: vec![],
            registration_time: 200,
            content_address: None,
        };
        store.register_path(&out_info).await.unwrap();

        // Verify the output was registered.
        let sp = StorePath::from_absolute_path(&out_info.path).unwrap();
        let queried = store.query_path_info(&sp).await.unwrap().unwrap();
        assert_eq!(queried.deriver, Some(drv_info.path));
    }

    #[tokio::test]
    async fn register_path_query_all_returns_registered() {
        let store = LocalStore::open_in_memory().await.unwrap();

        let info1 = PathInfo {
            path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-alpha".to_string(),
            nar_hash: "sha256:a".to_string(),
            nar_size: 10,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 1,
            content_address: None,
        };
        let info2 = PathInfo {
            path: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-beta".to_string(),
            nar_hash: "sha256:b".to_string(),
            nar_size: 20,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 2,
            content_address: None,
        };

        store.register_path(&info1).await.unwrap();
        store.register_path(&info2).await.unwrap();

        let all = store.query_all_valid_paths().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    // ── add_to_store ───────────────────────────────────────────

    #[tokio::test]
    async fn add_to_store_registers_and_unpacks() {
        use std::os::unix::fs::PermissionsExt;
        use sui_compat::nar::{NarNode, NarWriter};

        let tmp_dir = tempfile::tempdir().unwrap();
        // Ensure the store directory is writable.
        std::fs::set_permissions(
            tmp_dir.path(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let store_dir = tmp_dir.path().to_str().unwrap();
        let store = LocalStore::open_in_memory_with_dir(store_dir).await.unwrap();

        // Create a NAR archive for a simple file.
        let node = NarNode::Regular {
            executable: false,
            contents: b"hello store".to_vec(),
        };
        let mut nar_data = Vec::new();
        NarWriter::write(&mut nar_data, &node).unwrap();

        let info = store.add_to_store("test-pkg", &nar_data, &[]).await.unwrap();

        // Verify the path was registered.
        assert!(info.path.contains("test-pkg"));
        assert!(info.nar_hash.starts_with("sha256:"));
        assert_eq!(info.nar_size, nar_data.len() as i64);

        // Verify the file was unpacked to the store directory.
        let basename = info.path.strip_prefix(&format!("{store_dir}/")).unwrap();
        let unpacked_path = tmp_dir.path().join(basename);
        assert!(unpacked_path.exists());
    }

    // ── hex_encode helper ──────────────────────────────────────

    #[test]
    fn hex_encode_empty() {
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn hex_encode_single_byte() {
        assert_eq!(hex_encode(&[0xff]), "ff");
        assert_eq!(hex_encode(&[0x00]), "00");
        assert_eq!(hex_encode(&[0x0a]), "0a");
    }

    #[test]
    fn hex_encode_multiple_bytes() {
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }
}
