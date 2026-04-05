//! Core store trait — the interface all store backends implement.

use sui_compat::store_path::StorePath;

/// Result type for store operations.
pub type StoreResult<T> = Result<T, StoreError>;

/// Store operation errors.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The requested store path does not exist.
    #[error("path not found: {0}")]
    PathNotFound(String),
    /// A database or backend operation failed.
    #[error("database error: {0}")]
    Database(String),
    /// An I/O operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The operation is not supported by this store backend.
    #[error("not supported: {0}")]
    NotSupported(String),
}

/// Information about a store path.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PathInfo {
    /// Full absolute store path (e.g., `/nix/store/abc...-hello-2.12.1`).
    pub path: String,
    /// Hash of the NAR archive in `sha256:<base16>` format.
    pub nar_hash: String,
    /// Size of the NAR archive in bytes.
    pub nar_size: i64,
    /// Runtime dependency store paths.
    pub references: Vec<String>,
    /// Store path of the `.drv` that produced this path (if known).
    pub deriver: Option<String>,
    /// Ed25519 signatures (`keyname:base64sig`).
    pub signatures: Vec<String>,
    /// Unix timestamp of when this path was registered.
    pub registration_time: i64,
}

/// Garbage collection options.
#[derive(Debug, Clone, Default)]
pub struct GcOptions {
    /// Maximum bytes to free (0 = unlimited).
    pub max_freed: u64,
    /// Delete paths older than this many seconds.
    pub delete_older_than: Option<u64>,
}

/// Garbage collection result.
#[derive(Debug, Clone)]
pub struct GcResult {
    /// Number of store paths deleted.
    pub paths_deleted: usize,
    /// Total bytes freed.
    pub bytes_freed: u64,
}

/// The core store interface.
///
/// All store backends (local, remote, binary cache) implement this trait.
/// Async methods return `Send` futures so they can be used from `tokio::spawn`.
pub trait Store: Send + Sync {
    /// Query information about a store path.
    fn query_path_info(
        &self,
        path: &StorePath,
    ) -> impl std::future::Future<Output = StoreResult<Option<PathInfo>>> + Send;

    /// Check whether a store path is valid (registered in the store).
    fn is_valid_path(
        &self,
        path: &StorePath,
    ) -> impl std::future::Future<Output = StoreResult<bool>> + Send;

    /// List all valid store paths.
    fn query_all_valid_paths(
        &self,
    ) -> impl std::future::Future<Output = StoreResult<Vec<StorePath>>> + Send;

    /// Query the runtime references (dependencies) of a store path.
    fn query_references(
        &self,
        path: &StorePath,
    ) -> impl std::future::Future<Output = StoreResult<Vec<StorePath>>> + Send {
        async {
            let info = self.query_path_info(path).await?;
            match info {
                Some(info) => {
                    let refs: Vec<StorePath> = info
                        .references
                        .iter()
                        .filter_map(|r| StorePath::from_absolute_path(r).ok())
                        .collect();
                    Ok(refs)
                }
                None => Err(StoreError::PathNotFound(path.to_absolute_path())),
            }
        }
    }

    /// Compute the transitive closure of a set of store paths.
    fn compute_closure(
        &self,
        roots: &[StorePath],
    ) -> impl std::future::Future<Output = StoreResult<Vec<StorePath>>> + Send {
        async {
            let mut closure = Vec::new();
            let mut stack: Vec<StorePath> = roots.to_vec();
            let mut seen = std::collections::HashSet::new();

            while let Some(path) = stack.pop() {
                let key = path.to_absolute_path();
                if seen.contains(&key) {
                    continue;
                }
                seen.insert(key);

                let refs = self.query_references(&path).await?;
                for r in &refs {
                    stack.push(r.clone());
                }
                closure.push(path);
            }
            Ok(closure)
        }
    }

    /// Run garbage collection on the store.
    fn collect_garbage(
        &self,
        _options: &GcOptions,
    ) -> impl std::future::Future<Output = StoreResult<GcResult>> + Send {
        async {
            Err(StoreError::NotSupported(
                "garbage collection not implemented for this backend".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_info_serialization_roundtrip() {
        let info = PathInfo {
            path: "/nix/store/abc-hello".to_string(),
            nar_hash: "sha256:deadbeef".to_string(),
            nar_size: 1024,
            references: vec!["/nix/store/dep1".to_string()],
            deriver: Some("/nix/store/abc.drv".to_string()),
            signatures: vec!["key:sig".to_string()],
            registration_time: 1234567890,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: PathInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.path, info.path);
        assert_eq!(parsed.nar_hash, info.nar_hash);
        assert_eq!(parsed.nar_size, info.nar_size);
        assert_eq!(parsed.references, info.references);
        assert_eq!(parsed.deriver, info.deriver);
        assert_eq!(parsed.signatures, info.signatures);
        assert_eq!(parsed.registration_time, info.registration_time);
    }

    #[test]
    fn gc_options_default() {
        let opts = GcOptions::default();
        assert_eq!(opts.max_freed, 0);
        assert!(opts.delete_older_than.is_none());
    }

    #[test]
    fn store_error_display() {
        let e = StoreError::PathNotFound("/nix/store/abc".to_string());
        assert!(e.to_string().contains("/nix/store/abc"));

        let e = StoreError::NotSupported("gc".to_string());
        assert!(e.to_string().contains("gc"));
    }
}
