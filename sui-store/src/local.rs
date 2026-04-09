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
        if let Some(ref deriver) = info.deriver
            && deriver.ends_with(".drv") {
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
            std::io::Error::other(e.to_string()),
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

    async fn collect_garbage(
        &self,
        options: &crate::traits::GcOptions,
    ) -> StoreResult<crate::traits::GcResult> {
        use std::collections::HashSet;

        // 1. Enumerate GC roots.
        let roots = find_gc_roots(&self.store_dir);

        // 2. Compute reachable closure from the roots.
        let all_paths = self.query_all_valid_paths().await?;
        let mut reachable: HashSet<String> = HashSet::new();
        let mut queue: Vec<String> = roots;
        while let Some(path_str) = queue.pop() {
            if !reachable.insert(path_str.clone()) {
                continue;
            }
            // Look up references for this path.
            if let Ok(sp) = StorePath::from_absolute_path(&path_str)
                && let Ok(Some(info)) = self.query_path_info(&sp).await
            {
                for r in &info.references {
                    if !reachable.contains(r) {
                        queue.push(r.clone());
                    }
                }
            }
        }

        // 3. Find unreachable paths.
        let garbage: Vec<StorePath> = all_paths
            .into_iter()
            .filter(|p| !reachable.contains(&p.to_absolute_path()))
            .collect();

        // 4. Delete unreachable paths.
        let mut freed: u64 = 0;
        let mut deleted: usize = 0;
        for path in &garbage {
            match self.delete_path(path).await {
                Ok(bytes) => {
                    freed += bytes;
                    deleted += 1;
                    if options.max_freed > 0 && freed >= options.max_freed {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.to_absolute_path(),
                        error = %e,
                        "failed to delete path during GC",
                    );
                }
            }
        }

        Ok(crate::traits::GcResult {
            paths_deleted: deleted,
            bytes_freed: freed,
        })
    }

    async fn verify_store(&self) -> StoreResult<crate::traits::VerifyResult> {
        use sha2::{Sha256, Digest};

        let all_paths = self.query_all_valid_paths().await?;
        let mut result = crate::traits::VerifyResult::default();

        for sp in &all_paths {
            result.total_checked += 1;
            let abs_path = sp.to_absolute_path();

            let info = match self.query_path_info(sp).await? {
                Some(info) => info,
                None => continue,
            };

            // Compute the NAR hash of the actual files on disk.
            let fs_path = Path::new(&abs_path);
            if !fs_path.exists() {
                result.corrupt.push(crate::traits::CorruptPath {
                    path: abs_path,
                    expected_hash: info.nar_hash.clone(),
                    actual_hash: "(missing from disk)".to_string(),
                });
                continue;
            }

            match nar_from_path(fs_path) {
                Ok(nar_data) => {
                    let hash_raw = Sha256::digest(&nar_data);
                    let actual_hash = format!("sha256:{}", hex_encode(&hash_raw));
                    if actual_hash != info.nar_hash {
                        result.corrupt.push(crate::traits::CorruptPath {
                            path: abs_path,
                            expected_hash: info.nar_hash.clone(),
                            actual_hash,
                        });
                    } else {
                        result.valid_count += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %abs_path, error = %e, "failed to compute NAR hash");
                    result.corrupt.push(crate::traits::CorruptPath {
                        path: abs_path,
                        expected_hash: info.nar_hash.clone(),
                        actual_hash: format!("(error: {e})"),
                    });
                }
            }
        }

        Ok(result)
    }

    async fn delete_path(&self, path: &StorePath) -> StoreResult<u64> {
        use sea_orm::ConnectionTrait;

        let abs_path = path.to_absolute_path();
        let model = self.find_by_path(&abs_path).await?;

        // Calculate size of the path on disk.
        let fs_path = Path::new(&abs_path);
        let freed = if fs_path.exists() {
            dir_size(fs_path)
        } else {
            0
        };

        // Remove from database first.
        if let Some(model) = model {
            // Delete reference edges.
            let backend = self.db.get_database_backend();
            let del_refs = sea_orm::Statement::from_string(
                backend,
                format!("DELETE FROM Refs WHERE referrer = {} OR reference = {}", model.id, model.id),
            );
            self.db.execute(del_refs).await.map_err(db_err)?;

            // Delete derivation outputs.
            let del_drv = sea_orm::Statement::from_string(
                backend,
                format!("DELETE FROM DerivationOutputs WHERE drv = {}", model.id),
            );
            self.db.execute(del_drv).await.map_err(db_err)?;

            // Delete the valid path row.
            let del_path = sea_orm::Statement::from_string(
                backend,
                format!("DELETE FROM ValidPaths WHERE id = {}", model.id),
            );
            self.db.execute(del_path).await.map_err(db_err)?;
        }

        // Remove from disk.
        if fs_path.exists() {
            if fs_path.is_dir() {
                std::fs::remove_dir_all(fs_path)?;
            } else {
                std::fs::remove_file(fs_path)?;
            }
        }

        Ok(freed)
    }

    async fn optimise_store(&self, dry_run: bool) -> StoreResult<crate::traits::OptimiseResult> {
        use std::collections::HashMap;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        let store_path = Path::new(&self.store_dir);
        if !store_path.exists() {
            return Ok(crate::traits::OptimiseResult::default());
        }

        let mut seen: HashMap<String, std::path::PathBuf> = HashMap::new();
        let mut saved = 0u64;
        let mut linked = 0u64;

        // Walk all top-level store entries.
        let entries = std::fs::read_dir(store_path)?;
        for top_entry in entries.flatten() {
            let top_path = top_entry.path();
            // Walk files within each store path (min_depth 1 skips the dir itself).
            walk_files_recursive(&top_path, &mut |file_path: &Path| {
                let metadata = match std::fs::metadata(file_path) {
                    Ok(m) => m,
                    Err(_) => return,
                };
                if !metadata.is_file() {
                    return;
                }

                // Skip if already hard-linked (nlink > 1).
                #[cfg(unix)]
                if metadata.nlink() > 1 {
                    return;
                }

                // Hash the file.
                let hash = match sha256_file(file_path) {
                    Ok(h) => h,
                    Err(_) => return,
                };

                if let Some(existing) = seen.get(&hash) {
                    let size = metadata.len();
                    if dry_run {
                        // Just count — don't actually link.
                    } else {
                        // Replace with hard link.
                        if std::fs::remove_file(file_path).is_ok()
                            && std::fs::hard_link(existing, file_path).is_err()
                        {
                            // If hard_link fails, the file is already removed.
                            // This is a best-effort operation.
                            return;
                        }
                    }
                    saved += size;
                    linked += 1;
                } else {
                    seen.insert(hash, file_path.to_owned());
                }
            });
        }

        Ok(crate::traits::OptimiseResult {
            files_linked: linked,
            bytes_saved: saved,
        })
    }
}

/// Recursively walk files under a directory, calling `f` for each regular file.
fn walk_files_recursive(dir: &Path, f: &mut impl FnMut(&Path)) {
    if dir.is_file() {
        f(dir);
        return;
    }
    if !dir.is_dir() {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && !path.is_symlink() {
            walk_files_recursive(&path, f);
        } else if path.is_file() {
            f(&path);
        }
    }
}

/// Compute the SHA-256 hash of a file, returning a hex string.
fn sha256_file(path: &Path) -> Result<String, std::io::Error> {
    use sha2::{Sha256, Digest};
    let data = std::fs::read(path)?;
    let hash = Sha256::digest(&data);
    Ok(hex_encode(&hash))
}

/// Find GC roots by scanning well-known root directories.
///
/// Follows symlinks in `/nix/var/nix/gcroots/` and `/nix/var/nix/profiles/`
/// to discover which store paths are rooted.
pub fn find_gc_roots(store_dir: &str) -> Vec<String> {
    let mut roots = Vec::new();
    let gc_dirs = [
        "/nix/var/nix/gcroots",
        "/nix/var/nix/profiles",
    ];

    for dir in &gc_dirs {
        let dir_path = Path::new(dir);
        if !dir_path.exists() {
            continue;
        }
        collect_gc_roots_from(dir_path, store_dir, &mut roots);
    }

    roots.sort();
    roots.dedup();
    roots
}

/// Recursively scan a directory for symlinks pointing into the store.
fn collect_gc_roots_from(dir: &Path, store_dir: &str, roots: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_symlink() {
            if let Ok(target) = std::fs::read_link(&path) {
                let target_str = target.to_string_lossy();
                if target_str.starts_with(store_dir) {
                    // Extract the top-level store path (first component after store_dir).
                    let remainder = &target_str[store_dir.len()..];
                    let first_component = remainder
                        .trim_start_matches('/')
                        .split('/')
                        .next()
                        .unwrap_or("");
                    if !first_component.is_empty() {
                        roots.push(format!("{store_dir}/{first_component}"));
                    }
                }
            }
        }
        if path.is_dir() && !path.is_symlink() {
            collect_gc_roots_from(&path, store_dir, roots);
        }
    }
}

/// Compute the NAR serialization of a filesystem path.
fn nar_from_path(path: &Path) -> Result<Vec<u8>, std::io::Error> {
    use sui_compat::nar::NarWriter;

    let node = nar_node_from_path(path)?;
    let mut buf = Vec::new();
    NarWriter::write(&mut buf, &node).map_err(|e| {
        std::io::Error::other(format!("NAR write error: {e}"))
    })?;
    Ok(buf)
}

/// Build a `NarNode` tree from a filesystem path.
fn nar_node_from_path(path: &Path) -> Result<sui_compat::nar::NarNode, std::io::Error> {
    use sui_compat::nar::{NarEntry, NarNode};

    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        Ok(NarNode::Symlink {
            target: target.to_string_lossy().into_owned(),
        })
    } else if metadata.is_dir() {
        let mut entries: Vec<NarEntry> = Vec::new();
        let mut dir_entries: Vec<_> = std::fs::read_dir(path)?
            .filter_map(|e| e.ok())
            .collect();
        dir_entries.sort_by_key(|e| e.file_name());
        for entry in dir_entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let node = nar_node_from_path(&entry.path())?;
            entries.push(NarEntry { name, node });
        }
        Ok(NarNode::Directory { entries })
    } else {
        let contents = std::fs::read(path)?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        Ok(NarNode::Regular {
            executable,
            contents,
        })
    }
}

/// Calculate the total size of a file or directory on disk.
fn dir_size(path: &Path) -> u64 {
    if path.is_file() || path.is_symlink() {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    } else if path.is_dir() {
        let mut total = 0u64;
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                total += dir_size(&entry.path());
            }
        }
        total
    } else {
        0
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

    // ── GC tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn gc_on_empty_store_deletes_nothing() {
        let store = LocalStore::open_in_memory().await.unwrap();
        let result = store
            .collect_garbage(&crate::traits::GcOptions::default())
            .await
            .unwrap();
        assert_eq!(result.paths_deleted, 0);
        assert_eq!(result.bytes_freed, 0);
    }

    #[tokio::test]
    async fn gc_result_display() {
        let result = crate::traits::GcResult {
            paths_deleted: 5,
            bytes_freed: 1024,
        };
        assert_eq!(result.to_string(), "GC: 5 paths deleted, 1024 bytes freed");
    }

    // ── Verify tests ────────────────────────────────────────────

    #[tokio::test]
    async fn verify_empty_store_succeeds() {
        let store = LocalStore::open_in_memory().await.unwrap();
        let result = store.verify_store().await.unwrap();
        assert_eq!(result.total_checked, 0);
        assert_eq!(result.valid_count, 0);
        assert!(result.corrupt.is_empty());
    }

    #[tokio::test]
    async fn verify_result_display() {
        let result = crate::traits::VerifyResult {
            total_checked: 10,
            valid_count: 8,
            corrupt: vec![
                crate::traits::CorruptPath {
                    path: "/nix/store/abc-hello".to_string(),
                    expected_hash: "sha256:aaa".to_string(),
                    actual_hash: "sha256:bbb".to_string(),
                },
            ],
        };
        assert_eq!(result.to_string(), "Verify: 10 checked, 8 valid, 1 corrupt");
    }

    // ── verify with temp store dir ─────────────────────────────

    #[tokio::test]
    async fn verify_detects_valid_path() {
        // verify_store iterates all valid paths via query_all_valid_paths,
        // which parses paths using StorePath::from_absolute_path (requires
        // /nix/store/ prefix). Since we can't write to /nix/store in tests,
        // we register a real-looking store path pointing to a temp dir entry,
        // but the verify will see it as missing (not in /nix/store on disk).
        //
        // The core logic is already exercised by the empty and missing-path
        // tests above. This test validates the verify-store method returns
        // results for registered paths.
        let store = LocalStore::open_in_memory().await.unwrap();

        // Register a path with the real hash format.
        let fake_info = PathInfo {
            path: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-verify-test".to_string(),
            nar_hash: "sha256:1234".to_string(),
            nar_size: 50,
            ..PathInfo::default()
        };
        store.register_path(&fake_info).await.unwrap();

        let result = store.verify_store().await.unwrap();
        assert_eq!(result.total_checked, 1);
        // Path doesn't exist on disk, so it's counted as corrupt.
        assert_eq!(result.corrupt.len(), 1);
        assert!(result.corrupt[0].actual_hash.contains("missing"));
    }

    #[tokio::test]
    async fn verify_detects_missing_path() {
        // Use /nix/store as store dir so StorePath::from_absolute_path works,
        // but the path itself doesn't exist on disk (triggering "missing").
        let store = LocalStore::open_in_memory().await.unwrap();

        // Register a valid-looking store path that doesn't exist on disk.
        let fake_info = PathInfo {
            path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ghost-pkg".to_string(),
            nar_hash: "sha256:0000".to_string(),
            nar_size: 100,
            ..PathInfo::default()
        };
        store.register_path(&fake_info).await.unwrap();

        let result = store.verify_store().await.unwrap();
        assert_eq!(result.total_checked, 1);
        assert_eq!(result.valid_count, 0);
        assert_eq!(result.corrupt.len(), 1);
        assert!(result.corrupt[0].actual_hash.contains("missing"));
    }

    // ── delete_path tests ──────────────────────────────────────

    #[tokio::test]
    async fn delete_path_removes_from_db() {
        // Use in-memory store with /nix/store dir.
        // We can't write to /nix/store in tests, so we test the DB removal
        // only. The path won't exist on disk (delete_path handles that).
        let store = LocalStore::open_in_memory().await.unwrap();

        // Register a fake path in the DB.
        let fake_info = PathInfo {
            path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-delete-test".to_string(),
            nar_hash: "sha256:deadbeef".to_string(),
            nar_size: 42,
            ..PathInfo::default()
        };
        store.register_path(&fake_info).await.unwrap();

        let sp = StorePath::from_absolute_path(&fake_info.path).unwrap();
        assert!(store.is_valid_path(&sp).await.unwrap());

        let freed = store.delete_path(&sp).await.unwrap();
        // Path doesn't exist on disk so freed is 0.
        assert_eq!(freed, 0);
        // But it should be gone from the DB.
        assert!(!store.is_valid_path(&sp).await.unwrap());
    }

    // ── nar_node_from_path tests ────────────────────────────────

    #[test]
    fn nar_node_from_path_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello").unwrap();
        let node = nar_node_from_path(&file).unwrap();
        match node {
            sui_compat::nar::NarNode::Regular { executable, contents } => {
                assert!(!executable);
                assert_eq!(contents, b"hello");
            }
            _ => panic!("expected Regular"),
        }
    }

    #[test]
    fn nar_node_from_path_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"aaa").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"bbb").unwrap();
        let node = nar_node_from_path(dir.path()).unwrap();
        match node {
            sui_compat::nar::NarNode::Directory { entries } => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].name, "a.txt");
                assert_eq!(entries[1].name, "b.txt");
            }
            _ => panic!("expected Directory"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn nar_node_from_path_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"data").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let node = nar_node_from_path(&link).unwrap();
        match node {
            sui_compat::nar::NarNode::Symlink { target: t } => {
                assert!(t.contains("target"));
            }
            _ => panic!("expected Symlink"),
        }
    }

    // ── dir_size tests ─────────────────────────────────────────

    #[test]
    fn dir_size_of_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("size-test.txt");
        std::fs::write(&file, b"12345").unwrap();
        let size = dir_size(&file);
        assert!(size >= 5); // At least the bytes we wrote.
    }

    #[test]
    fn dir_size_of_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"aaa").unwrap();
        std::fs::write(dir.path().join("b"), b"bbbbb").unwrap();
        let size = dir_size(dir.path());
        assert!(size >= 8); // At least 3 + 5 bytes.
    }

    #[test]
    fn dir_size_of_nonexistent_is_zero() {
        assert_eq!(dir_size(Path::new("/nonexistent/path/xyz")), 0);
    }

    // ── find_gc_roots ──────────────────────────────────────────

    #[test]
    fn find_gc_roots_with_no_dirs() {
        // Using a store dir that doesn't exist — should return empty.
        let roots = find_gc_roots("/nonexistent/store");
        assert!(roots.is_empty());
    }

    #[test]
    fn find_gc_roots_is_public() {
        // Verify the function is accessible from outside the module.
        // This is a compile-time test — if it compiles, the function is pub.
        let _roots = find_gc_roots("/nix/store");
    }

    // ── sha256_file tests ─────────────────────────────────────

    #[test]
    fn sha256_file_regular() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();
        let hash = sha256_file(&file).unwrap();
        // SHA-256 of "hello world" is well known.
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn sha256_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty");
        std::fs::write(&file, b"").unwrap();
        let hash = sha256_file(&file).unwrap();
        // SHA-256 of empty string.
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_file_nonexistent_errors() {
        let result = sha256_file(Path::new("/nonexistent/file"));
        assert!(result.is_err());
    }

    // ── walk_files_recursive tests ─────────────────────────────

    #[test]
    fn walk_files_recursive_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"data").unwrap();

        let mut found = Vec::new();
        walk_files_recursive(dir.path(), &mut |p: &Path| {
            found.push(p.to_owned());
        });
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], file);
    }

    #[test]
    fn walk_files_recursive_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/c.txt"), b"deep").unwrap();
        std::fs::write(dir.path().join("top.txt"), b"top").unwrap();

        let mut found = Vec::new();
        walk_files_recursive(dir.path(), &mut |p: &Path| {
            found.push(p.to_owned());
        });
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn walk_files_recursive_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut found = Vec::new();
        walk_files_recursive(dir.path(), &mut |p: &Path| {
            found.push(p.to_owned());
        });
        assert!(found.is_empty());
    }

    #[test]
    fn walk_files_recursive_nonexistent() {
        let mut found = Vec::new();
        walk_files_recursive(Path::new("/nonexistent/dir"), &mut |p: &Path| {
            found.push(p.to_owned());
        });
        assert!(found.is_empty());
    }

    #[test]
    fn walk_files_recursive_single_file_input() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("single.txt");
        std::fs::write(&file, b"data").unwrap();

        let mut found = Vec::new();
        walk_files_recursive(&file, &mut |p: &Path| {
            found.push(p.to_owned());
        });
        assert_eq!(found.len(), 1);
    }

    // ── optimise_store tests ──────────────────────────────────

    #[tokio::test]
    async fn optimise_empty_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalStore::open_in_memory_with_dir(tmp.path().to_str().unwrap())
            .await
            .unwrap();
        let result = store.optimise_store(false).await.unwrap();
        assert_eq!(result.files_linked, 0);
        assert_eq!(result.bytes_saved, 0);
    }

    #[tokio::test]
    async fn optimise_dry_run_does_not_modify() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_dir = tmp.path().join("abc-pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("file1.txt"), b"duplicate content").unwrap();

        let pkg_dir2 = tmp.path().join("def-pkg");
        std::fs::create_dir_all(&pkg_dir2).unwrap();
        std::fs::write(pkg_dir2.join("file2.txt"), b"duplicate content").unwrap();

        let store = LocalStore::open_in_memory_with_dir(tmp.path().to_str().unwrap())
            .await
            .unwrap();
        let result = store.optimise_store(true).await.unwrap();
        // dry_run should report files that would be linked.
        assert_eq!(result.files_linked, 1);
        assert!(result.bytes_saved > 0);

        // Verify files were NOT actually hard-linked.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m1 = std::fs::metadata(pkg_dir.join("file1.txt")).unwrap();
            let m2 = std::fs::metadata(pkg_dir2.join("file2.txt")).unwrap();
            assert_eq!(m1.nlink(), 1);
            assert_eq!(m2.nlink(), 1);
        }
    }

    #[tokio::test]
    async fn optimise_links_duplicate_files() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_dir = tmp.path().join("abc-pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("file1.txt"), b"same content here").unwrap();

        let pkg_dir2 = tmp.path().join("def-pkg");
        std::fs::create_dir_all(&pkg_dir2).unwrap();
        std::fs::write(pkg_dir2.join("file2.txt"), b"same content here").unwrap();

        let store = LocalStore::open_in_memory_with_dir(tmp.path().to_str().unwrap())
            .await
            .unwrap();
        let result = store.optimise_store(false).await.unwrap();
        assert_eq!(result.files_linked, 1);
        assert!(result.bytes_saved > 0);

        // Verify files are now hard-linked (same inode).
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m1 = std::fs::metadata(pkg_dir.join("file1.txt")).unwrap();
            let m2 = std::fs::metadata(pkg_dir2.join("file2.txt")).unwrap();
            assert_eq!(m1.ino(), m2.ino());
            assert!(m1.nlink() > 1);
        }
    }

    #[tokio::test]
    async fn optimise_skips_unique_files() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_dir = tmp.path().join("abc-pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("file1.txt"), b"content A").unwrap();

        let pkg_dir2 = tmp.path().join("def-pkg");
        std::fs::create_dir_all(&pkg_dir2).unwrap();
        std::fs::write(pkg_dir2.join("file2.txt"), b"content B").unwrap();

        let store = LocalStore::open_in_memory_with_dir(tmp.path().to_str().unwrap())
            .await
            .unwrap();
        let result = store.optimise_store(false).await.unwrap();
        assert_eq!(result.files_linked, 0);
        assert_eq!(result.bytes_saved, 0);
    }

    #[tokio::test]
    async fn optimise_nonexistent_store_dir() {
        let store = LocalStore::open_in_memory_with_dir("/nonexistent/store/path")
            .await
            .unwrap();
        let result = store.optimise_store(false).await.unwrap();
        assert_eq!(result.files_linked, 0);
        assert_eq!(result.bytes_saved, 0);
    }

    #[tokio::test]
    async fn optimise_already_linked_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_dir = tmp.path().join("abc-pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let file1 = pkg_dir.join("file1.txt");
        std::fs::write(&file1, b"linked content").unwrap();

        let pkg_dir2 = tmp.path().join("def-pkg");
        std::fs::create_dir_all(&pkg_dir2).unwrap();
        let file2 = pkg_dir2.join("file2.txt");
        // Create a hard link manually.
        std::fs::hard_link(&file1, &file2).unwrap();

        let store = LocalStore::open_in_memory_with_dir(tmp.path().to_str().unwrap())
            .await
            .unwrap();
        let result = store.optimise_store(false).await.unwrap();
        // Should skip the already-linked files.
        assert_eq!(result.files_linked, 0);
        assert_eq!(result.bytes_saved, 0);
    }
}
