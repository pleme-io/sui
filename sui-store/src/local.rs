//! Local store implementation — reads /nix/store + existing SQLite DB via SeaORM.

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Database};
use sui_compat::store_path::StorePath;

use crate::entity::{derivation_output, reference, valid_path};
use crate::traits::{PathInfo, Store, StoreError, StoreResult};

/// Local Nix store backed by the filesystem and SQLite database.
pub struct LocalStore {
    db: DatabaseConnection,
    store_dir: String,
}

impl LocalStore {
    /// Open the local store using the existing Nix database.
    ///
    /// Default path: `/nix/var/nix/db/db.sqlite`
    pub async fn open(db_path: &str) -> StoreResult<Self> {
        let url = format!("sqlite://{db_path}?mode=ro");
        let db = Database::connect(&url)
            .await
            .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(Self {
            db,
            store_dir: "/nix/store".to_string(),
        })
    }

    /// Open with a custom store directory (for testing).
    pub async fn open_with_dir(db_path: &str, store_dir: &str) -> StoreResult<Self> {
        let url = format!("sqlite://{db_path}?mode=ro");
        let db = Database::connect(&url)
            .await
            .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(Self {
            db,
            store_dir: store_dir.to_string(),
        })
    }

    /// Get the database connection for direct queries.
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    /// Get the store directory path.
    pub fn store_dir(&self) -> &str {
        &self.store_dir
    }

    /// Look up a ValidPath row by its full store path string.
    async fn find_by_path(&self, path: &str) -> StoreResult<Option<valid_path::Model>> {
        valid_path::Entity::find()
            .filter(valid_path::Column::Path.eq(path))
            .one(&self.db)
            .await
            .map_err(|e| StoreError::Database(e.to_string()))
    }

    /// Get the references (runtime dependencies) for a given ValidPath id.
    async fn get_references(&self, path_id: i64) -> StoreResult<Vec<String>> {
        let refs = reference::Entity::find()
            .filter(reference::Column::Referrer.eq(path_id))
            .all(&self.db)
            .await
            .map_err(|e| StoreError::Database(e.to_string()))?;

        let ref_ids: Vec<i64> = refs.iter().map(|r| r.reference).collect();
        if ref_ids.is_empty() {
            return Ok(vec![]);
        }

        let ref_paths = valid_path::Entity::find()
            .filter(valid_path::Column::Id.is_in(ref_ids))
            .all(&self.db)
            .await
            .map_err(|e| StoreError::Database(e.to_string()))?;

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
            registration_time: model.registration_time, content_address: model.ca.clone(),
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
            .map_err(|e| StoreError::Database(e.to_string()))?;

        paths
            .into_iter()
            .filter_map(|p| StorePath::from_absolute_path(&p.path).ok())
            .collect::<Vec<_>>()
            .pipe_ok()
    }
}

/// Helper trait to wrap a value in Ok.
trait PipeOk: Sized {
    fn pipe_ok(self) -> StoreResult<Self> {
        Ok(self)
    }
}
impl<T> PipeOk for T {}

#[cfg(test)]
mod tests {
    // Integration tests require a real Nix store database.
    // These are run in Phase 3 integration tests against /nix/var/nix/db/db.sqlite.
    // Unit tests use in-memory SQLite — see tests/integration/.
}
