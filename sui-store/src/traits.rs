//! Core store trait — the interface all store backends implement.

use sui_compat::store_path::StorePath;

/// Result type for store operations.
pub type StoreResult<T> = Result<T, StoreError>;

/// Store operation errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The requested store path does not exist.
    #[error("path not found: {0}")]
    PathNotFound(String),
    /// A database or backend operation failed.
    #[error("database error: {0}")]
    Database(String),
    /// An HTTP request to a binary cache failed.
    #[error("http error: {0}")]
    Http(String),
    /// A NarInfo response could not be parsed.
    #[error("narinfo parse error: {0}")]
    NarInfo(String),
    /// An I/O operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The operation is not supported by this store backend.
    #[error("not supported: {0}")]
    NotSupported(String),
}

/// Information about a store path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[must_use]
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

impl StoreError {
    /// Returns `true` if this is a `PathNotFound` error.
    #[must_use]
    pub fn is_path_not_found(&self) -> bool {
        matches!(self, Self::PathNotFound(_))
    }

    /// Returns `true` if this is a `NotSupported` error.
    #[must_use]
    pub fn is_not_supported(&self) -> bool {
        matches!(self, Self::NotSupported(_))
    }
}

impl From<crate::http::HttpError> for StoreError {
    fn from(e: crate::http::HttpError) -> Self {
        Self::Http(e.to_string())
    }
}

impl Default for PathInfo {
    fn default() -> Self {
        Self {
            path: String::new(),
            nar_hash: String::new(),
            nar_size: 0,
            references: Vec::new(),
            deriver: None,
            signatures: Vec::new(),
            registration_time: 0,
            content_address: None,
        }
    }
}

impl PathInfo {
    /// Create a new `PathInfo` with the given path and NAR hash.
    ///
    /// All other fields default to zero/empty. Use the struct update syntax
    /// or setter calls to fill in remaining fields.
    #[must_use]
    pub fn new(path: impl Into<String>, nar_hash: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            nar_hash: nar_hash.into(),
            ..Self::default()
        }
    }
}

impl std::fmt::Display for PathInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (nar_size={})", self.path, self.nar_size)
    }
}

impl From<&sui_compat::narinfo::NarInfo> for PathInfo {
    fn from(info: &sui_compat::narinfo::NarInfo) -> Self {
        Self {
            path: info.store_path.clone(),
            nar_hash: info.nar_hash.clone(),
            nar_size: info.nar_size as i64,
            references: info.references.clone(),
            deriver: info.deriver.clone(),
            signatures: info.signatures.clone(),
            registration_time: 0,
            content_address: info.ca.clone(),
        }
    }
}

/// Garbage collection options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcOptions {
    /// Maximum bytes to free (0 = unlimited).
    pub max_freed: u64,
    /// Delete paths older than this many seconds.
    pub delete_older_than: Option<u64>,
}

impl GcOptions {
    /// Set the maximum number of bytes to free.
    #[must_use]
    pub fn with_max_freed(mut self, bytes: u64) -> Self {
        self.max_freed = bytes;
        self
    }

    /// Delete store paths older than the given number of seconds.
    #[must_use]
    pub fn with_delete_older_than(mut self, seconds: u64) -> Self {
        self.delete_older_than = Some(seconds);
        self
    }
}

/// Garbage collection result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[must_use]
pub struct GcResult {
    /// Number of store paths deleted.
    pub paths_deleted: usize,
    /// Total bytes freed.
    pub bytes_freed: u64,
}

impl std::fmt::Display for GcResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GC: {} paths deleted, {} bytes freed",
            self.paths_deleted, self.bytes_freed
        )
    }
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
        let info = self
            .query_path_info(path)
            .await?
            .ok_or_else(|| StoreError::PathNotFound(path.to_absolute_path()))?;

        Ok(info
            .references
            .iter()
            .filter_map(|r| StorePath::from_absolute_path(r).ok())
            .collect())
    }

    /// Compute the transitive closure of a set of store paths.
    ///
    /// Uses `BTreeSet` for deterministic traversal order.
    async fn compute_closure(
        &self,
        roots: &[StorePath],
    ) -> StoreResult<Vec<StorePath>> {
        let mut closure = Vec::new();
        let mut stack: Vec<StorePath> = roots.to_vec();
        let mut seen = std::collections::BTreeSet::new();

        while let Some(path) = stack.pop() {
            let key = path.to_absolute_path();
            if !seen.insert(key) {
                continue;
            }

            let refs = self.query_references(&path).await?;
            stack.extend(refs);
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
    fn path_info_serialization_all_fields_present() {
        let info = PathInfo {
            path: "/nix/store/abc-hello".to_string(),
            nar_hash: "sha256:deadbeef".to_string(),
            nar_size: 1024,
            references: vec!["/nix/store/dep1".to_string(), "/nix/store/dep2".to_string()],
            deriver: Some("/nix/store/abc.drv".to_string()),
            signatures: vec!["key1:sig1".to_string(), "key2:sig2".to_string()],
            registration_time: 1234567890,
            content_address: Some("fixed:out:r:sha256:cafe".to_string()),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"content_address\""));
        assert!(json.contains("fixed:out:r:sha256:cafe"));
        let parsed: PathInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content_address, info.content_address);
        assert_eq!(parsed.references.len(), 2);
        assert_eq!(parsed.signatures.len(), 2);
    }

    #[test]
    fn path_info_serialization_none_fields() {
        let info = PathInfo {
            path: "/nix/store/abc-minimal".to_string(),
            nar_hash: "sha256:000".to_string(),
            nar_size: 0,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 0,
            content_address: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: PathInfo = serde_json::from_str(&json).unwrap();
        assert!(parsed.deriver.is_none());
        assert!(parsed.content_address.is_none());
        assert!(parsed.references.is_empty());
        assert!(parsed.signatures.is_empty());
        assert_eq!(parsed.nar_size, 0);
    }

    #[test]
    fn path_info_json_pretty_roundtrip() {
        let info = PathInfo {
            path: "/nix/store/abc-hello".to_string(),
            nar_hash: "sha256:deadbeef".to_string(),
            nar_size: 42,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 999,
            content_address: None,
        };
        let pretty = serde_json::to_string_pretty(&info).unwrap();
        let parsed: PathInfo = serde_json::from_str(&pretty).unwrap();
        assert_eq!(parsed.path, info.path);
        assert_eq!(parsed.nar_size, 42);
    }

    #[test]
    fn path_info_deserialization_from_json_object() {
        let json = r#"{
            "path": "/nix/store/xyz-test",
            "nar_hash": "sha256:abc123",
            "nar_size": 9999,
            "references": ["/nix/store/dep-a"],
            "deriver": null,
            "signatures": [],
            "registration_time": 0,
            "content_address": null
        }"#;
        let info: PathInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.path, "/nix/store/xyz-test");
        assert_eq!(info.nar_size, 9999);
        assert_eq!(info.references, vec!["/nix/store/dep-a"]);
    }

    #[test]
    fn path_info_clone_independence() {
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
        let mut cloned = info.clone();
        cloned.nar_size = 9999;
        cloned.path = "/nix/store/other".to_string();
        assert_eq!(info.nar_size, 1024);
        assert_eq!(info.path, "/nix/store/abc-hello");
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
    /// Uses BTreeMap for deterministic iteration order.
    struct TestStore {
        infos: std::collections::BTreeMap<String, PathInfo>,
    }

    impl TestStore {
        fn new() -> Self {
            Self {
                infos: std::collections::BTreeMap::new(),
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

    #[test]
    fn store_error_http_display() {
        let e = StoreError::Http("timeout".to_string());
        assert!(e.to_string().contains("timeout"));
        assert!(e.to_string().contains("http"));
    }

    #[test]
    fn store_error_narinfo_display() {
        let e = StoreError::NarInfo("missing field: StorePath".to_string());
        assert!(e.to_string().contains("missing field"));
        assert!(e.to_string().contains("narinfo"));
    }

    // ── Arc<dyn Store> dispatch (the AppState pattern) ──────

    #[tokio::test]
    async fn arc_dyn_store_query_path_info() {
        let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(
            TestStore::new().with_path(hello_info()),
        );
        let info = store.query_path_info(&hello_path()).await.unwrap();
        assert!(info.is_some());
        assert_eq!(info.unwrap().nar_hash, "sha256:aaa");
    }

    #[tokio::test]
    async fn arc_dyn_store_is_valid_path() {
        let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(
            TestStore::new().with_path(hello_info()),
        );
        assert!(store.is_valid_path(&hello_path()).await.unwrap());
        assert!(!store.is_valid_path(&bash_path()).await.unwrap());
    }

    #[tokio::test]
    async fn arc_dyn_store_query_all_valid_paths() {
        let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(
            TestStore::new()
                .with_path(hello_info())
                .with_path(glibc_info()),
        );
        let paths = store.query_all_valid_paths().await.unwrap();
        assert_eq!(paths.len(), 2);
    }

    #[tokio::test]
    async fn arc_dyn_store_query_references() {
        let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(
            TestStore::new()
                .with_path(hello_info())
                .with_path(glibc_info()),
        );
        let refs = store.query_references(&hello_path()).await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].to_absolute_path(), glibc_path().to_absolute_path());
    }

    #[tokio::test]
    async fn arc_dyn_store_compute_closure() {
        let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(
            TestStore::new()
                .with_path(hello_info())
                .with_path(glibc_info())
                .with_path(bash_info()),
        );
        let closure = store.compute_closure(&[hello_path()]).await.unwrap();
        assert_eq!(closure.len(), 3);
    }

    #[tokio::test]
    async fn arc_dyn_store_default_methods_not_supported() {
        let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(
            TestStore::new(),
        );
        assert!(store.collect_garbage(&GcOptions::default()).await.is_err());
        assert!(store.add_to_store("x", b"data", &[]).await.is_err());
        assert!(store.register_path(&hello_info()).await.is_err());
        assert!(store.add_signatures(&hello_path(), &["sig".to_string()]).await.is_err());
        assert!(store.query_referrers(&hello_path()).await.is_err());
    }

    // ── Box<dyn Store> dispatch ────────────────────────────

    #[tokio::test]
    async fn box_dyn_store_query_path_info() {
        let store: Box<dyn Store> = Box::new(
            TestStore::new().with_path(hello_info()),
        );
        let info = store.query_path_info(&hello_path()).await.unwrap();
        assert!(info.is_some());
    }

    #[tokio::test]
    async fn box_dyn_store_is_valid_path() {
        let store: Box<dyn Store> = Box::new(
            TestStore::new().with_path(hello_info()),
        );
        assert!(store.is_valid_path(&hello_path()).await.unwrap());
        assert!(!store.is_valid_path(&glibc_path()).await.unwrap());
    }

    // ── GcResult and GcOptions ─────────────────────────────

    #[test]
    fn gc_result_fields() {
        let result = GcResult {
            paths_deleted: 42,
            bytes_freed: 1_000_000,
        };
        assert_eq!(result.paths_deleted, 42);
        assert_eq!(result.bytes_freed, 1_000_000);
    }

    #[test]
    fn gc_options_with_values() {
        let opts = GcOptions {
            max_freed: 500_000,
            delete_older_than: Some(3600),
        };
        assert_eq!(opts.max_freed, 500_000);
        assert_eq!(opts.delete_older_than, Some(3600));
    }

    #[test]
    fn gc_result_clone() {
        let result = GcResult {
            paths_deleted: 10,
            bytes_freed: 5000,
        };
        let cloned = result.clone();
        assert_eq!(cloned.paths_deleted, result.paths_deleted);
        assert_eq!(cloned.bytes_freed, result.bytes_freed);
    }

    #[test]
    fn gc_options_clone() {
        let opts = GcOptions {
            max_freed: 100,
            delete_older_than: Some(60),
        };
        let cloned = opts.clone();
        assert_eq!(cloned.max_freed, opts.max_freed);
        assert_eq!(cloned.delete_older_than, opts.delete_older_than);
    }

    // ── StoreError debug ───────────────────────────────────

    #[test]
    fn store_error_debug_format() {
        let e = StoreError::PathNotFound("/nix/store/abc".to_string());
        let debug = format!("{e:?}");
        assert!(debug.contains("PathNotFound"));
    }

    // ── query_path_info missing returns None ────────────────

    #[tokio::test]
    async fn query_path_info_missing_returns_none() {
        let store = TestStore::new();
        let result = store.query_path_info(&hello_path()).await.unwrap();
        assert!(result.is_none());
    }

    // ── is_valid_path false for missing ────────────────────

    #[tokio::test]
    async fn is_valid_path_false_when_missing() {
        let store = TestStore::new();
        assert!(!store.is_valid_path(&hello_path()).await.unwrap());
    }

    // ── query_all_valid_paths empty store ──────────────────

    #[tokio::test]
    async fn query_all_valid_paths_empty_store() {
        let store = TestStore::new();
        let paths = store.query_all_valid_paths().await.unwrap();
        assert!(paths.is_empty());
    }

    // ── PathInfo debug ─────────────────────────────────────

    #[test]
    fn path_info_debug_format() {
        let info = hello_info();
        let debug = format!("{info:?}");
        assert!(debug.contains("hello"));
        assert!(debug.contains("sha256:aaa"));
    }

    // ── compute_closure with cycle-like duplicates ─────────

    #[tokio::test]
    async fn compute_closure_handles_self_reference() {
        let mut info = bash_info();
        info.references = vec![bash_path().to_absolute_path()];
        let store = TestStore::new().with_path(info);

        let closure = store.compute_closure(&[bash_path()]).await.unwrap();
        assert_eq!(closure.len(), 1);
    }
}
