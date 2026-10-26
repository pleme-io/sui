use std::collections::BTreeMap;
use std::sync::Mutex;

use sui_compat::store_path::StorePath;
use sui_store::traits::{PathInfo, Store};

/// A simple in-memory store for testing.
pub(crate) struct MockStore {
    pub(crate) paths: Mutex<BTreeMap<String, PathInfo>>,
}

impl MockStore {
    pub(crate) fn new() -> Self {
        Self {
            paths: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn with_path(self, info: PathInfo) -> Self {
        self.paths.lock().unwrap().insert(info.path.clone(), info);
        self
    }
}

#[async_trait::async_trait]
impl Store for MockStore {
    async fn query_path_info(
        &self,
        path: &StorePath,
    ) -> sui_store::traits::StoreResult<Option<PathInfo>> {
        let abs = path.to_absolute_path();
        Ok(self.paths.lock().unwrap().get(&abs).cloned())
    }

    async fn is_valid_path(
        &self,
        path: &StorePath,
    ) -> sui_store::traits::StoreResult<bool> {
        let abs = path.to_absolute_path();
        Ok(self.paths.lock().unwrap().contains_key(&abs))
    }

    async fn query_all_valid_paths(
        &self,
    ) -> sui_store::traits::StoreResult<Vec<StorePath>> {
        Ok(self
            .paths
            .lock()
            .unwrap()
            .keys()
            .filter_map(|p| StorePath::from_absolute_path(p).ok())
            .collect())
    }

    async fn register_path(
        &self,
        info: &PathInfo,
    ) -> sui_store::traits::StoreResult<()> {
        self.paths
            .lock()
            .unwrap()
            .insert(info.path.clone(), info.clone());
        Ok(())
    }
}
