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
    /// An internal invariant was violated.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Information about a store path.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

impl PathInfo {
    /// Create a new `PathInfo` with the given path and NAR hash.
    ///
    /// All other fields default to zero/empty. Use the struct update syntax
    /// or setter calls to fill in remaining fields.
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
        // NarInfo's `References` field stores bare basenames
        // (e.g. `abc...-glibc-2.37`). PathInfo.references must contain absolute
        // store paths (e.g. `/nix/store/abc...-glibc-2.37`) so the default
        // `Store::query_references` impl can parse them via
        // `StorePath::from_absolute_path`. Anything that already looks
        // absolute is passed through unchanged for robustness.
        let store_dir = sui_compat::store_path::DEFAULT_STORE_DIR;
        let references = info
            .references
            .iter()
            .map(|r| {
                if r.starts_with('/') {
                    r.clone()
                } else {
                    format!("{store_dir}/{r}")
                }
            })
            .collect();
        Self {
            path: info.store_path.clone(),
            nar_hash: info.nar_hash.clone(),
            nar_size: info.nar_size as i64,
            references,
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

    // ── PathInfo::new constructor ──────────────────────────

    #[test]
    fn path_info_new_sets_path_and_hash() {
        let info = PathInfo::new("/nix/store/abc-x", "sha256:aaa");
        assert_eq!(info.path, "/nix/store/abc-x");
        assert_eq!(info.nar_hash, "sha256:aaa");
        assert_eq!(info.nar_size, 0);
        assert!(info.references.is_empty());
        assert!(info.deriver.is_none());
        assert!(info.signatures.is_empty());
        assert_eq!(info.registration_time, 0);
        assert!(info.content_address.is_none());
    }

    #[test]
    fn path_info_new_accepts_string_owned() {
        let info = PathInfo::new(String::from("/nix/store/abc-x"), String::from("sha256:aaa"));
        assert_eq!(info.path, "/nix/store/abc-x");
    }

    #[test]
    fn path_info_default_is_zero() {
        let info = PathInfo::default();
        assert!(info.path.is_empty());
        assert_eq!(info.nar_size, 0);
    }

    // ── PathInfo Display ───────────────────────────────────

    #[test]
    fn path_info_display_includes_path_and_size() {
        let info = PathInfo {
            path: "/nix/store/abc-hello".to_string(),
            nar_hash: "sha256:aaa".to_string(),
            nar_size: 1024,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 0,
            content_address: None,
        };
        let s = info.to_string();
        assert!(s.contains("/nix/store/abc-hello"));
        assert!(s.contains("1024"));
    }

    // ── PathInfo From<&NarInfo> conversion ─────────────────

    #[test]
    fn path_info_from_narinfo_full() {
        let narinfo = sui_compat::narinfo::NarInfo {
            store_path: "/nix/store/abc-hello".to_string(),
            url: "nar/abc.nar.xz".to_string(),
            compression: "xz".to_string(),
            file_hash: "sha256:fhash".to_string(),
            file_size: 500,
            nar_hash: "sha256:nhash".to_string(),
            nar_size: 1024,
            references: vec!["dep1".to_string(), "dep2".to_string()],
            deriver: Some("abc.drv".to_string()),
            signatures: vec!["k:s".to_string()],
            ca: Some("fixed:out:r:sha256:cafe".to_string()),
        };
        let info = PathInfo::from(&narinfo);
        assert_eq!(info.path, "/nix/store/abc-hello");
        assert_eq!(info.nar_hash, "sha256:nhash");
        assert_eq!(info.nar_size, 1024);
        assert_eq!(info.references.len(), 2);
        assert_eq!(info.deriver.as_deref(), Some("abc.drv"));
        assert_eq!(info.signatures, vec!["k:s"]);
        assert_eq!(info.content_address.as_deref(), Some("fixed:out:r:sha256:cafe"));
        // registration_time is not in NarInfo, defaults to 0
        assert_eq!(info.registration_time, 0);
    }

    #[test]
    fn path_info_from_narinfo_minimal() {
        let narinfo = sui_compat::narinfo::NarInfo {
            store_path: "/nix/store/abc-leaf".to_string(),
            url: "nar/abc.nar".to_string(),
            compression: "none".to_string(),
            file_hash: "sha256:f".to_string(),
            file_size: 0,
            nar_hash: "sha256:n".to_string(),
            nar_size: 0,
            references: vec![],
            deriver: None,
            signatures: vec![],
            ca: None,
        };
        let info = PathInfo::from(&narinfo);
        assert!(info.references.is_empty());
        assert!(info.deriver.is_none());
        assert!(info.content_address.is_none());
        assert_eq!(info.nar_size, 0);
    }

    // ── StoreError discriminants ───────────────────────────

    #[test]
    fn store_error_is_path_not_found_true() {
        let e = StoreError::PathNotFound("/nix/store/x".to_string());
        assert!(e.is_path_not_found());
        assert!(!e.is_not_supported());
    }

    #[test]
    fn store_error_is_path_not_found_false() {
        let e = StoreError::Database("x".to_string());
        assert!(!e.is_path_not_found());
    }

    #[test]
    fn store_error_is_not_supported_true() {
        let e = StoreError::NotSupported("gc".to_string());
        assert!(e.is_not_supported());
        assert!(!e.is_path_not_found());
    }

    #[test]
    fn store_error_is_not_supported_false_for_io() {
        let e = StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            "boom",
        ));
        assert!(!e.is_not_supported());
        assert!(!e.is_path_not_found());
    }

    #[test]
    fn store_error_io_display_contains_message() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        let e: StoreError = io_err.into();
        assert!(e.to_string().contains("missing file"));
        assert!(e.to_string().contains("io error"));
    }

    #[test]
    fn store_error_not_supported_display() {
        let e = StoreError::NotSupported("gc not implemented".to_string());
        let s = e.to_string();
        assert!(s.contains("not supported"));
        assert!(s.contains("gc"));
    }

    // ── StoreError From<HttpError> ─────────────────────────

    #[test]
    fn store_error_from_http_error_request() {
        use crate::http::HttpError;
        let http_err = HttpError::Request("dns failed".to_string());
        let store_err: StoreError = http_err.into();
        assert!(matches!(store_err, StoreError::Http(_)));
        assert!(store_err.to_string().contains("dns failed"));
    }

    #[test]
    fn store_error_from_http_error_decode() {
        use crate::http::HttpError;
        let http_err = HttpError::Decode("bad utf-8".to_string());
        let store_err: StoreError = http_err.into();
        assert!(matches!(store_err, StoreError::Http(_)));
        assert!(store_err.to_string().contains("bad utf-8"));
    }

    // ── GcOptions builder methods ──────────────────────────

    #[test]
    fn gc_options_with_max_freed() {
        let opts = GcOptions::default().with_max_freed(1024);
        assert_eq!(opts.max_freed, 1024);
        assert!(opts.delete_older_than.is_none());
    }

    #[test]
    fn gc_options_with_delete_older_than() {
        let opts = GcOptions::default().with_delete_older_than(3600);
        assert_eq!(opts.delete_older_than, Some(3600));
        assert_eq!(opts.max_freed, 0);
    }

    #[test]
    fn gc_options_chain_builder() {
        let opts = GcOptions::default()
            .with_max_freed(1_000_000)
            .with_delete_older_than(7200);
        assert_eq!(opts.max_freed, 1_000_000);
        assert_eq!(opts.delete_older_than, Some(7200));
    }

    #[test]
    fn gc_options_eq() {
        let a = GcOptions::default().with_max_freed(100);
        let b = GcOptions {
            max_freed: 100,
            delete_older_than: None,
        };
        assert_eq!(a, b);
    }

    // ── GcResult Display ───────────────────────────────────

    #[test]
    fn gc_result_display_format() {
        let r = GcResult {
            paths_deleted: 5,
            bytes_freed: 1024,
        };
        let s = r.to_string();
        assert!(s.contains("5"));
        assert!(s.contains("1024"));
        assert!(s.contains("paths"));
    }

    #[test]
    fn gc_result_default() {
        let r = GcResult::default();
        assert_eq!(r.paths_deleted, 0);
        assert_eq!(r.bytes_freed, 0);
    }

    #[test]
    fn gc_result_eq() {
        let a = GcResult {
            paths_deleted: 1,
            bytes_freed: 100,
        };
        let b = GcResult {
            paths_deleted: 1,
            bytes_freed: 100,
        };
        let c = GcResult {
            paths_deleted: 1,
            bytes_freed: 200,
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // ── compute_closure ordering & determinism ─────────────

    #[tokio::test]
    async fn compute_closure_dedup_with_diamond_deps() {
        // diamond: a -> b, a -> c, b -> d, c -> d
        let a_path =
            StorePath::from_absolute_path("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a")
                .unwrap();
        let b_path =
            StorePath::from_absolute_path("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b")
                .unwrap();
        let c_path =
            StorePath::from_absolute_path("/nix/store/cccccccccccccccccccccccccccccccc-c")
                .unwrap();
        let d_path =
            StorePath::from_absolute_path("/nix/store/dddddddddddddddddddddddddddddddd-d")
                .unwrap();

        let store = TestStore::new()
            .with_path(PathInfo {
                path: a_path.to_absolute_path(),
                nar_hash: "sha256:a".to_string(),
                nar_size: 1,
                references: vec![b_path.to_absolute_path(), c_path.to_absolute_path()],
                deriver: None,
                signatures: vec![],
                registration_time: 0,
                content_address: None,
            })
            .with_path(PathInfo {
                path: b_path.to_absolute_path(),
                nar_hash: "sha256:b".to_string(),
                nar_size: 1,
                references: vec![d_path.to_absolute_path()],
                deriver: None,
                signatures: vec![],
                registration_time: 0,
                content_address: None,
            })
            .with_path(PathInfo {
                path: c_path.to_absolute_path(),
                nar_hash: "sha256:c".to_string(),
                nar_size: 1,
                references: vec![d_path.to_absolute_path()],
                deriver: None,
                signatures: vec![],
                registration_time: 0,
                content_address: None,
            })
            .with_path(PathInfo {
                path: d_path.to_absolute_path(),
                nar_hash: "sha256:d".to_string(),
                nar_size: 1,
                references: vec![],
                deriver: None,
                signatures: vec![],
                registration_time: 0,
                content_address: None,
            });

        let closure = store.compute_closure(&[a_path.clone()]).await.unwrap();
        // a, b, c, d each appear exactly once
        assert_eq!(closure.len(), 4);
        let paths: Vec<String> = closure.iter().map(|p| p.to_absolute_path()).collect();
        assert!(paths.contains(&a_path.to_absolute_path()));
        assert!(paths.contains(&b_path.to_absolute_path()));
        assert!(paths.contains(&c_path.to_absolute_path()));
        assert!(paths.contains(&d_path.to_absolute_path()));

        // d should appear once even though both b and c reference it
        let d_count = paths
            .iter()
            .filter(|p| **p == d_path.to_absolute_path())
            .count();
        assert_eq!(d_count, 1);
    }

    #[tokio::test]
    async fn compute_closure_propagates_query_error() {
        // The store has hello but hello references glibc which is missing.
        // query_references for hello succeeds (info present), then while
        // walking, glibc isn't in the store -> query_references for glibc errors.
        let store = TestStore::new().with_path(hello_info());
        let result = store.compute_closure(&[hello_path()]).await;
        assert!(result.is_err());
    }

    // ── PartialEq on PathInfo ──────────────────────────────

    #[test]
    fn path_info_eq_full_match() {
        let a = hello_info();
        let b = hello_info();
        assert_eq!(a, b);
    }

    #[test]
    fn path_info_neq_when_size_differs() {
        let a = hello_info();
        let mut b = hello_info();
        b.nar_size = 9999;
        assert_ne!(a, b);
    }

    #[test]
    fn path_info_neq_when_signatures_differ() {
        let a = hello_info();
        let mut b = hello_info();
        b.signatures = vec!["other:sig".to_string()];
        assert_ne!(a, b);
    }

    #[test]
    fn path_info_neq_when_deriver_differs() {
        let a = hello_info();
        let mut b = hello_info();
        b.deriver = None;
        assert_ne!(a, b);
    }

    // ── MockStore (alternate Store impl) ──────────────────
    // Verifies that multiple Store implementations can coexist via dyn dispatch.

    /// Counts every Store-trait call. Useful for verifying call dispatch.
    struct MockStore {
        info: Option<PathInfo>,
        query_count: std::sync::atomic::AtomicUsize,
        valid_count: std::sync::atomic::AtomicUsize,
    }

    impl MockStore {
        fn new(info: Option<PathInfo>) -> Self {
            Self {
                info,
                query_count: std::sync::atomic::AtomicUsize::new(0),
                valid_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn query_count(&self) -> usize {
            self.query_count.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn valid_count(&self) -> usize {
            self.valid_count.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl Store for MockStore {
        async fn query_path_info(
            &self,
            _path: &StorePath,
        ) -> StoreResult<Option<PathInfo>> {
            self.query_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self.info.clone())
        }
        async fn is_valid_path(
            &self,
            _path: &StorePath,
        ) -> StoreResult<bool> {
            self.valid_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self.info.is_some())
        }
        async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn mock_store_counts_query_calls() {
        let store = MockStore::new(Some(hello_info()));
        let _ = store.query_path_info(&hello_path()).await.unwrap();
        let _ = store.query_path_info(&hello_path()).await.unwrap();
        let _ = store.query_path_info(&hello_path()).await.unwrap();
        assert_eq!(store.query_count(), 3);
    }

    #[tokio::test]
    async fn mock_store_counts_valid_calls() {
        let store = MockStore::new(Some(hello_info()));
        let _ = store.is_valid_path(&hello_path()).await.unwrap();
        let _ = store.is_valid_path(&hello_path()).await.unwrap();
        assert_eq!(store.valid_count(), 2);
    }

    #[tokio::test]
    async fn mock_store_via_dyn_dispatch() {
        let store: Box<dyn Store> = Box::new(MockStore::new(Some(hello_info())));
        let info = store.query_path_info(&hello_path()).await.unwrap();
        assert!(info.is_some());
    }

    #[tokio::test]
    async fn mock_store_returns_none_when_empty() {
        let store = MockStore::new(None);
        let info = store.query_path_info(&hello_path()).await.unwrap();
        assert!(info.is_none());
    }

    #[tokio::test]
    async fn mock_store_returns_false_when_empty() {
        let store = MockStore::new(None);
        assert!(!store.is_valid_path(&hello_path()).await.unwrap());
    }

    #[tokio::test]
    async fn mock_store_query_all_returns_empty() {
        let store = MockStore::new(Some(hello_info()));
        let paths = store.query_all_valid_paths().await.unwrap();
        assert!(paths.is_empty());
    }

    // ── Multiple Store impls coexist via Vec<Box<dyn Store>> ──

    #[tokio::test]
    async fn vec_of_dyn_store_dispatch() {
        let stores: Vec<Box<dyn Store>> = vec![
            Box::new(TestStore::new().with_path(hello_info())),
            Box::new(MockStore::new(Some(hello_info()))),
        ];
        for store in &stores {
            let info = store.query_path_info(&hello_path()).await.unwrap();
            assert!(info.is_some());
        }
    }

    // ── compute_closure with multiple roots ────────────────

    #[tokio::test]
    async fn compute_closure_multiple_roots_no_overlap() {
        let store = TestStore::new()
            .with_path(hello_info())
            .with_path(glibc_info())
            .with_path(bash_info());

        let closure = store
            .compute_closure(&[glibc_path(), bash_path()])
            .await
            .unwrap();
        // glibc + bash + bash (already in glibc closure) = 2 unique
        assert_eq!(closure.len(), 2);
    }

    // ── add_to_store / register_path / add_signatures error messages ──

    #[tokio::test]
    async fn add_to_store_error_includes_method_name() {
        let store = TestStore::new();
        let err = store.add_to_store("x", b"d", &[]).await.unwrap_err();
        assert!(err.to_string().contains("add_to_store"));
    }

    #[tokio::test]
    async fn register_path_error_includes_method_name() {
        let store = TestStore::new();
        let err = store.register_path(&hello_info()).await.unwrap_err();
        assert!(err.to_string().contains("register_path"));
    }

    #[tokio::test]
    async fn add_signatures_error_includes_method_name() {
        let store = TestStore::new();
        let err = store
            .add_signatures(&hello_path(), &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("add_signatures"));
    }

    #[tokio::test]
    async fn query_referrers_error_includes_method_name() {
        let store = TestStore::new();
        let err = store.query_referrers(&hello_path()).await.unwrap_err();
        assert!(err.to_string().contains("query_referrers"));
    }

    #[tokio::test]
    async fn collect_garbage_error_includes_method_name() {
        let store = TestStore::new();
        let err = store
            .collect_garbage(&GcOptions::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("garbage collection"));
    }

    // ── Send + Sync constraints ────────────────────────────

    #[test]
    fn store_trait_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn Store>();
    }

    #[test]
    fn path_info_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PathInfo>();
    }

    #[test]
    fn store_error_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StoreError>();
    }

    #[test]
    fn store_error_internal_display() {
        let e = StoreError::Internal("something unexpected".to_string());
        let msg = e.to_string();
        assert!(msg.contains("internal error"));
        assert!(msg.contains("something unexpected"));
    }
}
