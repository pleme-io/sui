use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use sui_compat::activity::SharedSink;
use sui_compat::derivation::Derivation;
use sui_compat::hash::{HashAlgorithm, NixHash};
use sui_compat::store_path::StorePath;
use sui_store::traits::{PathInfo, Store, StoreError, StoreResult};
use sui_store::worker_client::{RemotePathInfo, WorkerConn, WorkerError};

use crate::connection::DrvRealizer;

#[derive(Clone)]
pub struct WorkerStore {
    conn: Arc<Mutex<WorkerConn>>,
}

impl WorkerStore {
    #[must_use]
    pub fn new(conn: WorkerConn) -> Self {
        Self { conn: Arc::new(Mutex::new(conn)) }
    }

    async fn with<T, F>(&self, f: F) -> Result<T, WorkerError>
    where
        T: Send + 'static,
        F: FnOnce(&mut WorkerConn) -> Result<T, WorkerError> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            f(&mut guard)
        })
        .await
        .map_err(|e| WorkerError::Protocol(format!("worker task: {e}")))?
    }

    #[must_use]
    pub fn realizer(&self) -> Arc<dyn DrvRealizer> {
        Arc::new(WorkerRealizer { store: self.clone() })
    }
}

fn store_err(e: WorkerError) -> StoreError {
    match e {
        WorkerError::Daemon(m) => StoreError::Internal(m),
        other => StoreError::Internal(other.to_string()),
    }
}

fn to_path_info(r: RemotePathInfo) -> PathInfo {
    let nar_hash = if r.nar_hash.contains(':') { r.nar_hash } else { format!("sha256:{}", r.nar_hash) };
    PathInfo {
        path: r.path,
        nar_hash,
        nar_size: i64::try_from(r.nar_size).unwrap_or(i64::MAX),
        references: r.references,
        deriver: (!r.deriver.is_empty()).then_some(r.deriver),
        signatures: r.sigs,
        registration_time: i64::try_from(r.registration_time).unwrap_or(0),
        content_address: (!r.ca.is_empty()).then_some(r.ca),
    }
}

fn to_remote(info: &PathInfo) -> StoreResult<RemotePathInfo> {
    let hash = NixHash::parse_any(HashAlgorithm::Sha256, &info.nar_hash).map_err(|e| StoreError::Internal(e.to_string()))?;
    Ok(RemotePathInfo {
        path: info.path.clone(),
        deriver: info.deriver.clone().unwrap_or_default(),
        nar_hash: hash.to_hex(),
        references: info.references.clone(),
        registration_time: u64::try_from(info.registration_time).unwrap_or(0),
        nar_size: u64::try_from(info.nar_size).unwrap_or(0),
        ultimate: false,
        sigs: info.signatures.clone(),
        ca: info.content_address.clone().unwrap_or_default(),
    })
}

#[async_trait::async_trait]
impl Store for WorkerStore {
    async fn query_path_info(&self, path: &StorePath) -> StoreResult<Option<PathInfo>> {
        let p = path.to_absolute_path();
        self.with(move |c| c.query_path_info(&p)).await.map(|o| o.map(to_path_info)).map_err(store_err)
    }

    async fn is_valid_path(&self, path: &StorePath) -> StoreResult<bool> {
        let p = path.to_absolute_path();
        self.with(move |c| c.is_valid_path(&p)).await.map_err(store_err)
    }

    async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
        Err(StoreError::NotSupported("listing every valid path through the daemon".into()))
    }

    async fn add_temp_root(&self, path: &StorePath) -> StoreResult<()> {
        let p = path.to_absolute_path();
        self.with(move |c| c.add_temp_root(&p)).await.map_err(store_err)
    }

    async fn add_to_store_nar(&self, info: &PathInfo, nar: &[u8], _repair: bool) -> StoreResult<()> {
        let remote = to_remote(info)?;
        let nar = nar.to_vec();
        self.with(move |c| c.add_to_store_nar(&remote, &nar, true)).await.map_err(store_err)
    }
}

struct WorkerRealizer {
    store: WorkerStore,
}

impl DrvRealizer for WorkerRealizer {
    fn realize<'a>(
        &'a self,
        drv_path: &'a str,
        _sink: SharedSink,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move {
            let target = format!("{drv_path}!*");
            self.store.with(move |c| c.build_derived(&[target])).await.map_err(|e| e.to_string())?;
            Ok(Vec::new())
        })
    }

    fn build_basic<'a>(
        &'a self,
        drv: &'a Derivation,
        drv_path: &'a str,
        _sink: SharedSink,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move {
            let drv = drv.clone();
            let path = drv_path.to_string();
            let result = self
                .store
                .with(move |c| {
                    let srcs = drv.input_sources.clone();
                    c.build_derivation(&path, &drv, &srcs).map(|r| (r, drv))
                })
                .await
                .map_err(|e| e.to_string())?;
            let (result, drv) = result;
            if !result.success() {
                return Err(result.error_msg);
            }
            Ok(drv.outputs.values().map(|o| o.path.clone()).collect())
        })
    }
}
