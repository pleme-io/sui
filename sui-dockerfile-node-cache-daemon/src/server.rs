//! Request-handling core: local-first, remote-on-miss, persist-and-return.
//!
//! This module is transport-agnostic — it answers a
//! [`DaemonRequest`](crate::protocol::DaemonRequest) and produces a
//! [`DaemonResponse`](crate::protocol::DaemonResponse) given a
//! [`LocalCacheStore`] and a remote
//! [`StorageBackend`](sui_castore::StorageBackend). The UDS
//! listen loop (`listen`, in this crate's `lib.rs`) is a thin shell
//! around [`NodeCacheDaemon::handle_request`].

use std::sync::Arc;

use sui_cache::StorageBackend;

use crate::protocol::{CachedArtifact, DaemonRequest, DaemonResponse, WarmStatus};
use crate::store::LocalCacheStore;
use crate::DaemonError;

/// The daemon's request-handling core. Generic over the local store so
/// tests can inject [`crate::store::MockLocalCacheStore`] and
/// production wires [`crate::store::RealLocalCacheStore`]; the remote
/// tier is always the existing `dyn StorageBackend` trait object,
/// reused unmodified from `sui-cache`.
pub struct NodeCacheDaemon<L: LocalCacheStore> {
    local: Arc<L>,
    remote: Arc<dyn StorageBackend>,
}

impl<L: LocalCacheStore + 'static> NodeCacheDaemon<L> {
    #[must_use]
    pub fn new(local: Arc<L>, remote: Arc<dyn StorageBackend>) -> Self {
        Self { local, remote }
    }

    /// Answer one request. Never panics: every fallible path is
    /// surfaced as [`DaemonResponse::Error`], not an unwind.
    pub async fn handle_request(&self, request: DaemonRequest) -> DaemonResponse {
        match request {
            DaemonRequest::Get { content_hash } => self.handle_get(&content_hash).await,
            DaemonRequest::Put { content_hash, artifact } => self.handle_put(&content_hash, &artifact).await,
            DaemonRequest::Warm { content_hash } => self.handle_warm(content_hash).await,
        }
    }

    async fn handle_get(&self, content_hash: &str) -> DaemonResponse {
        match self.local.get(content_hash).await {
            Ok(Some(artifact)) => return DaemonResponse::Get { artifact: Some(artifact) },
            Ok(None) => {}
            Err(e) => return DaemonResponse::Error { message: e.to_string() },
        }

        // Local miss: reach into the remote tier exactly once, persist
        // locally on a hit, and return — the caller never needs to know
        // which tier actually served the answer.
        match self.fetch_from_remote_and_persist(content_hash).await {
            Ok(artifact) => DaemonResponse::Get { artifact },
            Err(e) => DaemonResponse::Error { message: e.to_string() },
        }
    }

    async fn handle_put(&self, content_hash: &str, artifact: &CachedArtifact) -> DaemonResponse {
        match self.local.put(content_hash, artifact).await {
            Ok(()) => DaemonResponse::Put { ok: true },
            Err(e) => DaemonResponse::Error { message: e.to_string() },
        }
    }

    async fn handle_warm(&self, content_hash: String) -> DaemonResponse {
        match self.local.get(&content_hash).await {
            Ok(Some(_)) => return DaemonResponse::Warm { status: WarmStatus::AlreadyLocal },
            Ok(None) => {}
            Err(e) => return DaemonResponse::Error { message: e.to_string() },
        }

        // Fire-and-forget: schedule the fetch-and-persist on the
        // runtime and respond immediately. A follow-up `Get` for the
        // same hash observes the result once the task completes.
        let local = Arc::clone(&self.local);
        let remote = Arc::clone(&self.remote);
        tokio::spawn(async move {
            let daemon = NodeCacheDaemon { local, remote };
            if let Err(e) = daemon.fetch_from_remote_and_persist(&content_hash).await {
                tracing::warn!(target: "sui-dockerfile-node-cache-daemon", content_hash, error = %e, "background warm fetch failed");
            }
        });
        DaemonResponse::Warm { status: WarmStatus::FetchScheduled }
    }

    /// Fetch a hash from the remote tier and, on a hit, persist it
    /// locally before returning. Called at most once per `Get`/`Warm` —
    /// never re-queries the remote tier for a hash already resolved
    /// locally in this call.
    async fn fetch_from_remote_and_persist(&self, content_hash: &str) -> Result<Option<CachedArtifact>, DaemonError> {
        let remote_hit = self.remote.get_narinfo(content_hash).await?;
        let Some(image_ref) = remote_hit else {
            return Ok(None);
        };
        let artifact = CachedArtifact { image_ref };
        self.local.put(content_hash, &artifact).await?;
        Ok(Some(artifact))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MockLocalCacheStore;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use sui_cache::CacheError;

    /// Counting remote mock — lets tests assert "called exactly once".
    #[derive(Default)]
    struct CountingRemote {
        entries: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
        get_calls: AtomicUsize,
    }

    impl CountingRemote {
        fn with_entry(self, hash: &str, image_ref: &str) -> Self {
            self.entries.lock().unwrap().insert(hash.to_string(), image_ref.to_string());
            self
        }
    }

    #[async_trait]
    impl StorageBackend for CountingRemote {
        async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, CacheError> {
            self.get_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.entries.lock().unwrap().get(hash).cloned())
        }
        async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), CacheError> {
            self.entries.lock().unwrap().insert(hash.to_string(), content.to_string());
            Ok(())
        }
        async fn get_nar(&self, _path: &str) -> Result<Option<Vec<u8>>, CacheError> {
            Ok(None)
        }
        async fn put_nar(&self, _path: &str, _data: &[u8]) -> Result<(), CacheError> {
            Ok(())
        }
        /// An in-memory test double holds whole values by construction. The
        /// declaration is required precisely so a *production* backend cannot
        /// inherit this path by omission.
        fn nar_residency(&self) -> sui_cache::NarResidency {
            sui_cache::NarResidency::WholeValue
        }

        async fn delete(&self, hash: &str) -> Result<(), CacheError> {
            self.entries.lock().unwrap().remove(hash);
            Ok(())
        }
        async fn list_narinfos(&self) -> Result<Vec<String>, CacheError> {
            Ok(self.entries.lock().unwrap().keys().cloned().collect())
        }
    }

    #[tokio::test]
    async fn local_hit_never_calls_remote() {
        let local = Arc::new(
            MockLocalCacheStore::new().with_entry("h1", CachedArtifact { image_ref: "img:local".to_string() }),
        );
        let remote: Arc<CountingRemote> = Arc::new(CountingRemote::default());
        let remote_dyn: Arc<dyn StorageBackend> = remote.clone();
        let daemon = NodeCacheDaemon::new(local, remote_dyn);

        let resp = daemon.handle_request(DaemonRequest::Get { content_hash: "h1".to_string() }).await;

        match resp {
            DaemonResponse::Get { artifact: Some(a) } => assert_eq!(a.image_ref, "img:local"),
            other => panic!("expected local hit, got {other:?}"),
        }
        assert_eq!(remote.get_calls.load(Ordering::SeqCst), 0, "remote must not be touched on a local hit");
    }

    #[tokio::test]
    async fn local_miss_calls_remote_exactly_once_and_persists_for_next_get() {
        let local = Arc::new(MockLocalCacheStore::new());
        let remote = Arc::new(CountingRemote::default().with_entry("h2", "img:remote"));
        let remote_dyn: Arc<dyn StorageBackend> = remote.clone();
        // Keep a handle to assert local persistence.
        let local_check = Arc::clone(&local);
        let daemon = NodeCacheDaemon::new(local, remote_dyn);

        let resp = daemon.handle_request(DaemonRequest::Get { content_hash: "h2".to_string() }).await;
        match resp {
            DaemonResponse::Get { artifact: Some(a) } => assert_eq!(a.image_ref, "img:remote"),
            other => panic!("expected remote-fetched hit, got {other:?}"),
        }
        assert_eq!(remote.get_calls.load(Ordering::SeqCst), 1);
        assert!(local_check.contains("h2"), "remote hit must be persisted locally");

        // A second Get for the same hash must now be a pure local hit —
        // zero additional remote calls.
        let resp2 = daemon.handle_request(DaemonRequest::Get { content_hash: "h2".to_string() }).await;
        assert!(matches!(resp2, DaemonResponse::Get { artifact: Some(_) }));
        assert_eq!(remote.get_calls.load(Ordering::SeqCst), 1, "second Get must not re-touch remote");
    }

    #[tokio::test]
    async fn local_and_remote_miss_returns_none_without_error() {
        let local = Arc::new(MockLocalCacheStore::new());
        let remote: Arc<dyn StorageBackend> = Arc::new(CountingRemote::default());
        let daemon = NodeCacheDaemon::new(local, remote);

        let resp = daemon.handle_request(DaemonRequest::Get { content_hash: "nope".to_string() }).await;
        assert!(matches!(resp, DaemonResponse::Get { artifact: None }));
    }

    #[tokio::test]
    async fn put_persists_locally_only_never_touches_remote() {
        let local = Arc::new(MockLocalCacheStore::new());
        let remote = Arc::new(CountingRemote::default());
        let remote_dyn: Arc<dyn StorageBackend> = remote.clone();
        let local_check = Arc::clone(&local);
        let daemon = NodeCacheDaemon::new(local, remote_dyn);

        let resp = daemon
            .handle_request(DaemonRequest::Put {
                content_hash: "h3".to_string(),
                artifact: CachedArtifact { image_ref: "img:new".to_string() },
            })
            .await;
        assert!(matches!(resp, DaemonResponse::Put { ok: true }));
        assert!(local_check.contains("h3"));
        assert_eq!(remote.get_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn warm_on_already_local_hash_reports_already_local_and_skips_remote() {
        let local = Arc::new(
            MockLocalCacheStore::new().with_entry("h4", CachedArtifact { image_ref: "img:local".to_string() }),
        );
        let remote: Arc<dyn StorageBackend> = Arc::new(CountingRemote::default());
        let daemon = NodeCacheDaemon::new(local, remote);

        let resp = daemon.handle_request(DaemonRequest::Warm { content_hash: "h4".to_string() }).await;
        assert!(matches!(resp, DaemonResponse::Warm { status: WarmStatus::AlreadyLocal }));
    }

    #[tokio::test]
    async fn warm_on_missing_hash_schedules_background_fetch_that_persists() {
        let local = Arc::new(MockLocalCacheStore::new());
        let remote = Arc::new(CountingRemote::default().with_entry("h5", "img:warmed"));
        let remote_dyn: Arc<dyn StorageBackend> = remote.clone();
        let local_check = Arc::clone(&local);
        let daemon = NodeCacheDaemon::new(local, remote_dyn);

        let resp = daemon.handle_request(DaemonRequest::Warm { content_hash: "h5".to_string() }).await;
        assert!(matches!(resp, DaemonResponse::Warm { status: WarmStatus::FetchScheduled }));

        // Give the spawned background task a chance to run.
        for _ in 0..50 {
            if local_check.contains("h5") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(local_check.contains("h5"), "background warm fetch must persist locally");
        assert_eq!(remote.get_calls.load(Ordering::SeqCst), 1);
    }
}
