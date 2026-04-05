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
    /// Content-address assertion (e.g., `fixed:out:r:sha256:...`).
    pub content_address: Option<String>,
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
/// Uses `#[async_trait]` for object safety — enables `dyn Store`.
#[async_trait::async_trait]
pub trait Store: Send + Sync {
    /// Query information about a store path.
    async fn query_path_info(
        &self,
        path: &StorePath,
    ) -> StoreResult<Option<PathInfo>>;

    /// Check whether a store path is valid (registered in the store).
    async fn is_valid_path(
        &self,
        path: &StorePath,
    ) -> StoreResult<bool>;

    /// List all valid store paths.
    async fn query_all_valid_paths(
        &self,
    ) -> StoreResult<Vec<StorePath>>;

    /// Query the runtime references (dependencies) of a store path.
    async fn query_references(
        &self,
        path: &StorePath,
    ) -> StoreResult<Vec<StorePath>> {
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

    /// Compute the transitive closure of a set of store paths.
    async fn compute_closure(
        &self,
        roots: &[StorePath],
    ) -> StoreResult<Vec<StorePath>> {
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

    /// Run garbage collection on the store.
    async fn collect_garbage(
        &self,
        _options: &GcOptions,
    ) -> StoreResult<GcResult> {
        Err(StoreError::NotSupported(
            "garbage collection not implemented for this backend".to_string(),
        ))
    }

    /// Add a store path with its NAR content. Returns the registered PathInfo.
    async fn add_to_store(
        &self,
        _name: &str,
        _nar_data: &[u8],
        _references: &[String],
    ) -> StoreResult<PathInfo> {
        Err(StoreError::NotSupported(
            "add_to_store not implemented for this backend".to_string(),
        ))
    }

    /// Register a pre-built path in the store database.
    async fn register_path(
        &self,
        _info: &PathInfo,
    ) -> StoreResult<()> {
        Err(StoreError::NotSupported(
            "register_path not implemented for this backend".to_string(),
        ))
    }

    /// Add signatures to an existing store path.
    async fn add_signatures(
        &self,
        _path: &StorePath,
        _signatures: &[String],
    ) -> StoreResult<()> {
        Err(StoreError::NotSupported(
            "add_signatures not implemented for this backend".to_string(),
        ))
    }

    /// Query paths that refer to the given path (reverse dependencies).
    async fn query_referrers(
        &self,
        _path: &StorePath,
    ) -> StoreResult<Vec<StorePath>> {
        Err(StoreError::NotSupported(
            "query_referrers not implemented for this backend".to_string(),
        ))
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
            content_address: None,
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

    // ── TestStore: implements only required methods ───────────

    /// Minimal store for exercising default trait methods.
    struct TestStore {
        infos: std::collections::HashMap<String, PathInfo>,
    }

    impl TestStore {
        fn new() -> Self {
            Self {
                infos: std::collections::HashMap::new(),
            }
        }

        fn with_path(mut self, info: PathInfo) -> Self {
            self.infos.insert(info.path.clone(), info);
            self
        }
    }

    #[async_trait::async_trait]
    impl Store for TestStore {
        async fn query_path_info(
            &self,
            path: &StorePath,
        ) -> StoreResult<Option<PathInfo>> {
            Ok(self.infos.get(&path.to_absolute_path()).cloned())
        }

        async fn is_valid_path(
            &self,
            path: &StorePath,
        ) -> StoreResult<bool> {
            Ok(self.infos.contains_key(&path.to_absolute_path()))
        }

        async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
            self.infos
                .keys()
                .map(|p| {
                    StorePath::from_absolute_path(p)
                        .map_err(|e| StoreError::Database(e.to_string()))
                })
                .collect()
        }
    }

    // Helper to create a real StorePath from the well-known hello hash.
    fn hello_path() -> StorePath {
        StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap()
    }

    fn glibc_path() -> StorePath {
        StorePath::from_absolute_path(
            "/nix/store/3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37",
        )
        .unwrap()
    }

    fn bash_path() -> StorePath {
        StorePath::from_absolute_path(
            "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-bash-5.2",
        )
        .unwrap()
    }

    fn hello_info() -> PathInfo {
        PathInfo {
            path: hello_path().to_absolute_path(),
            nar_hash: "sha256:aaa".to_string(),
            nar_size: 5000,
            references: vec![glibc_path().to_absolute_path()],
            deriver: Some("/nix/store/abc.drv".to_string()),
            signatures: vec!["key:sig".to_string()],
            registration_time: 1000,
            content_address: None,
        }
    }

    fn glibc_info() -> PathInfo {
        PathInfo {
            path: glibc_path().to_absolute_path(),
            nar_hash: "sha256:bbb".to_string(),
            nar_size: 30000,
            references: vec![bash_path().to_absolute_path()],
            deriver: None,
            signatures: vec![],
            registration_time: 900,
            content_address: None,
        }
    }

    fn bash_info() -> PathInfo {
        PathInfo {
            path: bash_path().to_absolute_path(),
            nar_hash: "sha256:ccc".to_string(),
            nar_size: 8000,
            references: vec![], // leaf — no deps
            deriver: None,
            signatures: vec![],
            registration_time: 800,
            content_address: None,
        }
    }

    // ── Default method: query_references ─────────────────────

    #[tokio::test]
    async fn query_references_returns_refs_from_path_info() {
        let store = TestStore::new()
            .with_path(hello_info())
            .with_path(glibc_info());

        let refs = store.query_references(&hello_path()).await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].to_absolute_path(), glibc_path().to_absolute_path());
    }

    #[tokio::test]
    async fn query_references_returns_empty_for_leaf() {
        let store = TestStore::new().with_path(bash_info());

        let refs = store.query_references(&bash_path()).await.unwrap();
        assert!(refs.is_empty());
    }

    #[tokio::test]
    async fn query_references_errors_for_missing_path() {
        let store = TestStore::new();

        let result = store.query_references(&hello_path()).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            StoreError::PathNotFound(p) => {
                assert!(p.contains("hello-2.12.1"));
            }
            other => panic!("expected PathNotFound, got {other:?}"),
        }
    }

    // ── Default method: compute_closure ──────────────────────

    #[tokio::test]
    async fn compute_closure_walks_transitive_deps() {
        let store = TestStore::new()
            .with_path(hello_info())
            .with_path(glibc_info())
            .with_path(bash_info());

        let closure = store.compute_closure(&[hello_path()]).await.unwrap();
        // Should contain hello, glibc, and bash (transitive)
        assert_eq!(closure.len(), 3);
        let paths: Vec<String> = closure.iter().map(|p| p.to_absolute_path()).collect();
        assert!(paths.contains(&hello_path().to_absolute_path()));
        assert!(paths.contains(&glibc_path().to_absolute_path()));
        assert!(paths.contains(&bash_path().to_absolute_path()));
    }

    #[tokio::test]
    async fn compute_closure_deduplicates() {
        // Both hello and glibc depend on bash; bash should appear once
        let store = TestStore::new()
            .with_path(hello_info())
            .with_path(glibc_info())
            .with_path(bash_info());

        let closure = store
            .compute_closure(&[hello_path(), glibc_path()])
            .await
            .unwrap();
        let bash_count = closure
            .iter()
            .filter(|p| p.to_absolute_path() == bash_path().to_absolute_path())
            .count();
        assert_eq!(bash_count, 1);
    }

    #[tokio::test]
    async fn compute_closure_empty_roots() {
        let store = TestStore::new();
        let closure = store.compute_closure(&[]).await.unwrap();
        assert!(closure.is_empty());
    }

    #[tokio::test]
    async fn compute_closure_single_leaf() {
        let store = TestStore::new().with_path(bash_info());

        let closure = store.compute_closure(&[bash_path()]).await.unwrap();
        assert_eq!(closure.len(), 1);
        assert_eq!(closure[0].to_absolute_path(), bash_path().to_absolute_path());
    }

    // ── Default method: collect_garbage ──────────────────────

    #[tokio::test]
    async fn collect_garbage_returns_not_supported() {
        let store = TestStore::new();
        let result = store.collect_garbage(&GcOptions::default()).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            StoreError::NotSupported(msg) => {
                assert!(msg.contains("garbage collection"));
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    // ── Default method: add_to_store ─────────────────────────

    #[tokio::test]
    async fn add_to_store_returns_not_supported() {
        let store = TestStore::new();
        let result = store.add_to_store("test", b"data", &[]).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            StoreError::NotSupported(msg) => {
                assert!(msg.contains("add_to_store"));
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    // ── Default method: register_path ────────────────────────

    #[tokio::test]
    async fn register_path_returns_not_supported() {
        let store = TestStore::new();
        let info = hello_info();
        let result = store.register_path(&info).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            StoreError::NotSupported(msg) => {
                assert!(msg.contains("register_path"));
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    // ── Default method: add_signatures ───────────────────────

    #[tokio::test]
    async fn add_signatures_returns_not_supported() {
        let store = TestStore::new();
        let result = store
            .add_signatures(&hello_path(), &["sig1".to_string()])
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            StoreError::NotSupported(msg) => {
                assert!(msg.contains("add_signatures"));
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    // ── Default method: query_referrers ──────────────────────

    #[tokio::test]
    async fn query_referrers_returns_not_supported() {
        let store = TestStore::new();
        let result = store.query_referrers(&hello_path()).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            StoreError::NotSupported(msg) => {
                assert!(msg.contains("query_referrers"));
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    // ── Store trait: object safety ───────────────────────────

    #[test]
    fn store_trait_is_object_safe() {
        fn assert_obj_safe(_: &dyn Store) {}
        let store = TestStore::new();
        assert_obj_safe(&store);
    }

    // ── StoreError: Io variant ──────────────────────────────

    #[test]
    fn store_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let store_err: StoreError = io_err.into();
        assert!(store_err.to_string().contains("denied"));
    }

    #[test]
    fn store_error_database_display() {
        let e = StoreError::Database("connection lost".to_string());
        assert!(e.to_string().contains("connection lost"));
    }
}
