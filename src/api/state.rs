//! Shared application state for the API server.

use std::sync::Arc;

/// Application state shared across all API handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: Option<Arc<dyn sui_store::Store>>,
}

impl AppState {
    /// Create state with a connected store (any backend).
    pub fn with_store(store: impl sui_store::Store + 'static) -> Self {
        Self {
            store: Some(Arc::new(store)),
        }
    }

    /// Create state without a store (stub mode).
    pub fn stub() -> Self {
        Self { store: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_compat::store_path::StorePath;
    use sui_store::traits::{PathInfo, Store, StoreResult};

    /// Test store for exercising AppState through Arc<dyn Store>.
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

        async fn is_valid_path(&self, path: &StorePath) -> StoreResult<bool> {
            Ok(self.infos.contains_key(&path.to_absolute_path()))
        }

        async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
            self.infos
                .keys()
                .filter_map(|p| StorePath::from_absolute_path(p).ok())
                .collect::<Vec<_>>()
                .pipe_ok()
        }
    }

    trait PipeOk: Sized {
        fn pipe_ok(self) -> StoreResult<Self> { Ok(self) }
    }
    impl<T> PipeOk for T {}

    fn hello_path() -> StorePath {
        StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap()
    }

    fn hello_info() -> PathInfo {
        PathInfo {
            path: hello_path().to_absolute_path(),
            nar_hash: "sha256:aaa".to_string(),
            nar_size: 5000,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 1000,
            content_address: None,
        }
    }

    #[test]
    fn app_state_stub_has_no_store() {
        let state = AppState::stub();
        assert!(state.store.is_none());
    }

    #[test]
    fn app_state_with_store_has_store() {
        let state = AppState::with_store(TestStore::new());
        assert!(state.store.is_some());
    }

    #[tokio::test]
    async fn app_state_query_through_arc_dyn_store() {
        let state = AppState::with_store(
            TestStore::new().with_path(hello_info()),
        );
        let store = state.store.as_ref().unwrap();
        let info = store.query_path_info(&hello_path()).await.unwrap();
        assert!(info.is_some());
        assert_eq!(info.unwrap().nar_hash, "sha256:aaa");
    }

    #[tokio::test]
    async fn app_state_is_valid_path_through_arc_dyn_store() {
        let state = AppState::with_store(
            TestStore::new().with_path(hello_info()),
        );
        let store = state.store.as_ref().unwrap();
        assert!(store.is_valid_path(&hello_path()).await.unwrap());
    }

    #[tokio::test]
    async fn app_state_query_all_valid_paths_through_arc_dyn_store() {
        let state = AppState::with_store(
            TestStore::new().with_path(hello_info()),
        );
        let store = state.store.as_ref().unwrap();
        let paths = store.query_all_valid_paths().await.unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].to_absolute_path(), hello_path().to_absolute_path());
    }

    #[tokio::test]
    async fn app_state_missing_path_returns_none() {
        let state = AppState::with_store(TestStore::new());
        let store = state.store.as_ref().unwrap();
        let info = store.query_path_info(&hello_path()).await.unwrap();
        assert!(info.is_none());
    }

    #[test]
    fn app_state_clone_preserves_store() {
        let state = AppState::with_store(TestStore::new());
        let cloned = state.clone();
        assert!(cloned.store.is_some());
    }
}
