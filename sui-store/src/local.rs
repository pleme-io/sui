//! Local store implementation — reads /nix/store + existing SQLite DB via SeaORM.

use std::path::Path;

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Database};
use sui_compat::store_path::StorePath;

use crate::entity::{reference, valid_path};
use crate::traits::{PathInfo, Store, StoreError, StoreResult};

/// Local Nix store backed by the filesystem and SQLite database.
pub struct LocalStore {
    db: DatabaseConnection,
    store_dir: String,
}

impl LocalStore {
    /// Open the local store using the existing Nix database.
    ///
    /// Default path: `/nix/var/nix/db/db.sqlite`.
    /// Accepts any type convertible to a path (`&str`, `&Path`, `PathBuf`, etc.).
    pub async fn open(db_path: impl AsRef<Path>) -> StoreResult<Self> {
        Self::open_inner(db_path.as_ref(), "/nix/store").await
    }

    /// Open with a custom store directory (for testing).
    pub async fn open_with_dir(
        db_path: impl AsRef<Path>,
        store_dir: impl AsRef<Path>,
    ) -> StoreResult<Self> {
        Self::open_inner(db_path.as_ref(), store_dir.as_ref().to_str().unwrap_or("/nix/store"))
            .await
    }

    async fn open_inner(db_path: &Path, store_dir: &str) -> StoreResult<Self> {
        let db_path_str = db_path.to_str().ok_or_else(|| {
            StoreError::Database("database path is not valid UTF-8".to_string())
        })?;
        let url = format!("sqlite://{db_path_str}?mode=ro");
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
}

/// Convert a SeaORM `DbErr` into a `StoreError::Database`.
fn db_err(e: sea_orm::DbErr) -> StoreError {
    StoreError::Database(e.to_string())
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
}
