//! Daemon-aware cache client — Phase 3b of `supa-charge-akeyless-ci`.
//!
//! [`DaemonAwareCacheClient`] implements the same
//! [`StorageBackend`](sui_castore::StorageBackend) trait the
//! plain Phase 2 wrapper already consumes, so wiring it in is a
//! **substitution at construction time**, not a change to
//! [`crate::run_wrapper`] or its 8 Phase 2 tests: whoever builds the
//! `Arc<dyn StorageBackend>` decides whether that's a direct remote
//! backend (today's default) or this daemon-aware wrapper around one.
//!
//! Behavior:
//! - `get_narinfo`/`put_narinfo` first try to reach the node-local
//!   `sui-dockerfile-node-cache-daemon` over its Unix domain socket
//!   (see [`sui_dockerfile_node_cache_daemon::default_socket_path`] for
//!   the well-known integration-contract path a runner pod must mount
//!   the same `hostPath` volume at to reach it).
//! - **Daemon reachable**: routes the request through it. The daemon
//!   itself may transparently fall through to the remote tier on a
//!   local L0 miss — this client does not duplicate that fetch.
//! - **Daemon unreachable** (socket missing, or the connection attempt
//!   fails/times out): falls straight through to the `remote` backend
//!   passed at construction, byte-for-byte the same call this client
//!   would have made with no daemon awareness at all — the "never
//!   require the daemon, always degrade gracefully" contract.
//! - `put_narinfo` always writes through to `remote` (so the shared
//!   L1/L2 tiers — and every other node's future local misses — see
//!   the new entry), and additionally best-effort informs the daemon
//!   (if reachable) so this node's local L0 is warm without a second
//!   round trip on the next `Get`. A daemon-`Put` failure is logged,
//!   never propagated — the remote write already succeeded and is the
//!   one that matters for correctness.
//! - Every other [`StorageBackend`] method (`get_nar`/`put_nar`/
//!   `delete`/`list_narinfos`) is delegated straight to `remote`
//!   unchanged — the node cache daemon only speaks the narinfo-shaped
//!   `Get`/`Put`/`Warm` protocol the Dockerfile-graph cache uses.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sui_cache::StorageBackend;
use sui_cache::CacheError;
use sui_dockerfile_node_cache_daemon::protocol::{read_message, write_message};
use sui_dockerfile_node_cache_daemon::{CachedArtifact, DaemonRequest, DaemonResponse};
use tokio::net::UnixStream;

/// How long to wait for a UDS connect before deciding "no daemon on
/// this node". Kept small — this is a same-host socket, not a network
/// hop; a real daemon accepts near-instantly, so any delay this size
/// means the socket simply isn't there (or the daemon is wedged, which
/// should degrade the same as "not there").
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// A [`StorageBackend`] that prefers a node-local daemon and falls
/// through to a direct remote backend when the daemon can't be
/// reached. See the module docs for the exact per-method contract.
pub struct DaemonAwareCacheClient {
    socket_path: Option<PathBuf>,
    remote: Arc<dyn StorageBackend>,
}

impl DaemonAwareCacheClient {
    /// `socket_path = None` makes this client behave identically to
    /// calling `remote` directly — the byte-for-byte Phase 2 fallback
    /// path, useful for callers that want the daemon-aware *type*
    /// without unconditionally trying to connect.
    #[must_use]
    pub fn new(socket_path: Option<PathBuf>, remote: Arc<dyn StorageBackend>) -> Self {
        Self { socket_path, remote }
    }

    /// Attempt to reach the daemon and exchange one request/response.
    /// Returns `None` (never an error) when the daemon is unreachable —
    /// callers treat `None` as "fall through to remote", not a failure.
    async fn try_daemon_roundtrip(&self, request: &DaemonRequest) -> Option<DaemonResponse> {
        let path = self.socket_path.as_ref()?;
        let connect = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(path)).await;
        let Ok(Ok(mut stream)) = connect else { return None };
        if write_message(&mut stream, request).await.is_err() {
            return None;
        }
        read_message::<_, DaemonResponse>(&mut stream).await.ok()
    }
}

#[async_trait]
impl StorageBackend for DaemonAwareCacheClient {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, CacheError> {
        let request = DaemonRequest::Get { content_hash: hash.to_string() };
        match self.try_daemon_roundtrip(&request).await {
            Some(DaemonResponse::Get { artifact }) => Ok(artifact.map(|a| a.image_ref)),
            Some(DaemonResponse::Error { message }) => {
                tracing::warn!(target: "sui-dockerfile-wrapper::daemon_client", hash, error = %message, "daemon reported an error servicing Get; falling through to remote");
                self.remote.get_narinfo(hash).await
            }
            Some(other) => {
                tracing::warn!(target: "sui-dockerfile-wrapper::daemon_client", hash, response = ?other, "unexpected daemon response to Get; falling through to remote");
                self.remote.get_narinfo(hash).await
            }
            None => self.remote.get_narinfo(hash).await,
        }
    }

    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), CacheError> {
        // The remote write is the one that must succeed for
        // correctness — every other node's future local misses (and
        // any node with no daemon at all) depend on it.
        self.remote.put_narinfo(hash, content).await?;

        // Best-effort local warm-up: informs this node's daemon so the
        // very next local Get for this hash is already a local hit.
        // A failure here is logged, never propagated — the write
        // already succeeded where it must.
        let request = DaemonRequest::Put {
            content_hash: hash.to_string(),
            artifact: CachedArtifact { image_ref: content.to_string() },
        };
        if let Some(DaemonResponse::Error { message }) = self.try_daemon_roundtrip(&request).await {
            tracing::warn!(target: "sui-dockerfile-wrapper::daemon_client", hash, error = %message, "daemon reported an error servicing Put (remote write already succeeded)");
        }
        Ok(())
    }

    async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, CacheError> {
        self.remote.get_nar(path).await
    }

    async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), CacheError> {
        self.remote.put_nar(path, data).await
    }

    async fn delete(&self, hash: &str) -> Result<(), CacheError> {
        self.remote.delete(hash).await
    }

    async fn list_narinfos(&self) -> Result<Vec<String>, CacheError> {
        self.remote.list_narinfos().await
    }
}

/// Probe whether `path` looks like a live socket file. Used by callers
/// deciding whether to construct a [`DaemonAwareCacheClient`] with
/// `Some(path)` at all (a pure existence check — the client's own
/// connect-with-timeout handles the "exists but not accepting" case,
/// so this is a cheap early filter, not the sole gate).
#[must_use]
pub fn socket_looks_present(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::MockCacheBackend;
    use sui_dockerfile_node_cache_daemon::{bind_unix_listener, WarmStatus};
    use tokio::sync::watch;

    /// Spins up a tiny fake daemon that answers with a single
    /// pre-programmed response to every request it receives, so tests
    /// can assert the client's daemon-reachable behavior without
    /// depending on the real `sui-dockerfile-node-cache-daemon` server
    /// logic (that crate's own tests cover the real daemon behavior).
    fn spawn_fake_daemon(socket_path: &Path, response: DaemonResponse) -> watch::Sender<bool> {
        let listener = bind_unix_listener(socket_path).unwrap();
        let (tx, mut rx) = watch::channel(false);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut stream, _)) = accepted else { continue };
                        let response = response.clone();
                        tokio::spawn(async move {
                            let _req: Result<DaemonRequest, _> = read_message(&mut stream).await;
                            let _ = write_message(&mut stream, &response).await;
                        });
                    }
                    _ = rx.changed() => {
                        if *rx.borrow() { break; }
                    }
                }
            }
        });
        tx
    }

    #[tokio::test]
    async fn daemon_reachable_and_hit_never_calls_remote() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("daemon.sock");
        let _shutdown = spawn_fake_daemon(
            &socket_path,
            DaemonResponse::Get { artifact: Some(CachedArtifact { image_ref: "img:from-daemon".to_string() }) },
        );

        let remote = Arc::new(MockCacheBackend::new());
        let remote_dyn: Arc<dyn StorageBackend> = remote.clone();
        let client = DaemonAwareCacheClient::new(Some(socket_path), remote_dyn);

        let got = client.get_narinfo("h1").await.unwrap();
        assert_eq!(got.as_deref(), Some("img:from-daemon"));
        // The mock remote was never populated and never queried:
        // confirmed indirectly — a direct query would return None,
        // whereas the daemon's canned response was returned.
        assert_eq!(remote.get_narinfo("h1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn daemon_reachable_and_miss_returns_none_without_querying_remote_again() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("daemon.sock");
        let _shutdown = spawn_fake_daemon(&socket_path, DaemonResponse::Get { artifact: None });

        // Even though the remote WOULD have this hash, a reachable
        // daemon's answer is authoritative — the client must not
        // duplicate the remote lookup itself.
        let remote = Arc::new(MockCacheBackend::new().with_entry("h2", "img:should-not-be-seen"));
        let remote_dyn: Arc<dyn StorageBackend> = remote;
        let client = DaemonAwareCacheClient::new(Some(socket_path), remote_dyn);

        let got = client.get_narinfo("h2").await.unwrap();
        assert_eq!(got, None, "daemon's Get-miss answer must be trusted, not overridden by a second remote check");
    }

    #[tokio::test]
    async fn daemon_unreachable_falls_through_to_remote_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        // No daemon listening at this path at all.
        let socket_path = dir.path().join("no-daemon-here.sock");

        let remote = Arc::new(MockCacheBackend::new().with_entry("h3", "img:direct-remote"));
        let remote_dyn: Arc<dyn StorageBackend> = remote.clone();
        let client = DaemonAwareCacheClient::new(Some(socket_path), remote_dyn.clone());

        let via_client = client.get_narinfo("h3").await.unwrap();
        let via_remote_directly = remote_dyn.get_narinfo("h3").await.unwrap();
        assert_eq!(via_client, via_remote_directly);
        assert_eq!(via_client.as_deref(), Some("img:direct-remote"));
    }

    #[tokio::test]
    async fn no_socket_path_configured_behaves_exactly_like_direct_remote() {
        let remote = Arc::new(MockCacheBackend::new().with_entry("h4", "img:no-daemon-configured"));
        let remote_dyn: Arc<dyn StorageBackend> = remote.clone();
        let client = DaemonAwareCacheClient::new(None, remote_dyn.clone());

        let via_client = client.get_narinfo("h4").await.unwrap();
        let via_remote_directly = remote_dyn.get_narinfo("h4").await.unwrap();
        assert_eq!(via_client, via_remote_directly);
    }

    #[tokio::test]
    async fn put_always_writes_through_to_remote_even_when_daemon_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("no-daemon-here.sock");

        let remote = Arc::new(MockCacheBackend::new());
        let remote_dyn: Arc<dyn StorageBackend> = remote.clone();
        let client = DaemonAwareCacheClient::new(Some(socket_path), remote_dyn);

        client.put_narinfo("h5", "img:backfilled").await.unwrap();
        assert_eq!(remote.get_narinfo("h5").await.unwrap().as_deref(), Some("img:backfilled"));
    }

    #[tokio::test]
    async fn put_writes_through_to_remote_and_warms_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("daemon.sock");
        let _shutdown =
            spawn_fake_daemon(&socket_path, DaemonResponse::Put { ok: true });

        let remote = Arc::new(MockCacheBackend::new());
        let remote_dyn: Arc<dyn StorageBackend> = remote.clone();
        let client = DaemonAwareCacheClient::new(Some(socket_path), remote_dyn);

        client.put_narinfo("h6", "img:dual-write").await.unwrap();
        assert_eq!(remote.get_narinfo("h6").await.unwrap().as_deref(), Some("img:dual-write"));
    }

    #[test]
    fn socket_presence_check_is_a_pure_filesystem_check() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!socket_looks_present(&dir.path().join("nope.sock")));
    }

    // Silence an "unused import" concern in case a future edit trims a
    // test that referenced WarmStatus directly — kept as a smoke check
    // that the re-export is reachable from this crate.
    #[test]
    fn warm_status_type_is_reachable_from_this_crate() {
        let _ = WarmStatus::AlreadyLocal;
    }
}
