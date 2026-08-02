//! Phase 3b of `supa-charge-akeyless-ci`: a node-local L0 disk cache
//! daemon.
//!
//! One instance runs per Kubernetes node (as a `DaemonSet`, see the
//! `sui-dockerfile-node-cache-daemon` Helm chart). It listens on a
//! Unix domain socket and answers `Get`/`Put`/`Warm` requests (see
//! [`protocol`]) against a node-local disk cache ([`store`]), falling
//! through to the existing remote
//! [`sui_castore::StorageBackend`] (Postgres L2 / Redis L1,
//! unmodified — see [`server`]) on a local miss, persisting the result
//! locally so the next lookup on the same node is a zero-network-hop
//! local hit.
//!
//! ## Why a Unix domain socket, not TCP-localhost
//!
//! A UDS avoids the kernel's TCP/IP stack entirely (no port allocation,
//! no `SO_REUSEADDR` races across pod restarts, no risk of binding a
//! port another node-local process expects), and its filesystem-path
//! identity is exactly the shape a `DaemonSet`'s `hostPath` mount and a
//! runner pod's matching mount need to share to discover each other
//! (see the Helm chart's `values.yaml` for the mount-path contract).
//! Standard UNIX file permissions on the socket's parent directory are
//! also a real (if coarse) access boundary that a TCP port lacks. A
//! TCP-localhost fallback was judged unnecessary complexity for a
//! same-host-only protocol — this crate does not implement one.
//!
//! ## Reuse, not reinvention
//!
//! This crate does **not** invent a new remote cache abstraction — the
//! "remote tier" side of [`server::NodeCacheDaemon`] is exactly
//! [`sui_castore::StorageBackend`], the same trait
//! `sui-dockerfile-wrapper` (Phase 2) and `sui cache serve` already
//! consume. Only the local L0 tier ([`store::LocalCacheStore`]) and the
//! wire protocol ([`protocol`]) are new.

#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod protocol;
pub mod server;
pub mod store;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

pub use protocol::{CachedArtifact, DaemonRequest, DaemonResponse, WarmStatus};
pub use server::NodeCacheDaemon;
pub use store::{LocalCacheStore, MockLocalCacheStore, RealLocalCacheStore};

/// Errors this crate's server/store/protocol layers can produce.
/// Always typed, never a panic on a fallible path.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("remote cache backend: {0}")]
    Remote(#[from] sui_cache::CacheError),
}

/// Bind a `UnixListener` at `socket_path`, removing any stale socket
/// file left behind by a prior (crashed) instance first — a fresh bind
/// on an existing path otherwise fails with `AddrInUse`.
///
/// # Errors
///
/// Propagates any I/O failure removing the stale path or binding.
pub fn bind_unix_listener(socket_path: &Path) -> Result<UnixListener, DaemonError> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(UnixListener::bind(socket_path)?)
}

/// Serve requests on `listener` until `shutdown` reports `true`.
/// Each accepted connection handles exactly one request-response pair
/// then closes — this daemon's protocol is not multiplexed, mirroring
/// its "quick node-local lookup" workload rather than a long-lived
/// session.
///
/// Graceful shutdown: `tokio::select!` races `listener.accept()`
/// against the shutdown watch, so an in-flight connection being
/// accepted never races a mid-shutdown drop; already-spawned
/// connection tasks are allowed to finish on their own (fire-and-forget
/// `Warm` background fetches are, by design, best-effort and may be
/// dropped on shutdown — never a correctness requirement).
pub async fn serve<L: LocalCacheStore + 'static>(
    listener: UnixListener,
    daemon: Arc<NodeCacheDaemon<L>>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let daemon = Arc::clone(&daemon);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, &daemon).await {
                                tracing::warn!(
                                    target: "sui-dockerfile-node-cache-daemon",
                                    error = %e,
                                    "connection handling failed"
                                );
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(target: "sui-dockerfile-node-cache-daemon", error = %e, "accept failed");
                    }
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!(target: "sui-dockerfile-node-cache-daemon", "graceful shutdown requested");
                    break;
                }
            }
        }
    }
}

async fn handle_connection<L: LocalCacheStore + 'static>(
    mut stream: UnixStream,
    daemon: &NodeCacheDaemon<L>,
) -> Result<(), DaemonError> {
    let request: DaemonRequest = protocol::read_message(&mut stream).await?;
    let response = daemon.handle_request(request).await;
    protocol::write_message(&mut stream, &response).await
}

/// Default socket path — the well-known integration-contract path a
/// `DaemonSet` pod and a same-node GitHub Actions runner pod must both
/// mount (as a shared `hostPath`, see the Helm chart) to discover each
/// other. Also the default the `sui-dockerfile-wrapper`
/// `DaemonAwareCacheClient` probes when a caller doesn't override it.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    PathBuf::from("/var/run/sui-dockerfile-cache/node-cache.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use sui_cache::StorageBackend;
    use sui_cache::CacheError;
    use tokio::net::UnixStream as ClientStream;

    struct EmptyRemote;
    #[async_trait]
    impl StorageBackend for EmptyRemote {
        async fn get_narinfo(&self, _hash: &str) -> Result<Option<String>, CacheError> {
            Ok(None)
        }
        async fn put_narinfo(&self, _hash: &str, _content: &str) -> Result<(), CacheError> {
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

        async fn delete(&self, _hash: &str) -> Result<(), CacheError> {
            Ok(())
        }
        async fn list_narinfos(&self) -> Result<Vec<String>, CacheError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn end_to_end_over_a_real_unix_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");

        let local = Arc::new(
            MockLocalCacheStore::new().with_entry("hit", CachedArtifact { image_ref: "img:e2e".to_string() }),
        );
        let remote: Arc<dyn StorageBackend> = Arc::new(EmptyRemote);
        let daemon = Arc::new(NodeCacheDaemon::new(local, remote));

        let listener = bind_unix_listener(&socket_path).unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let server_task = tokio::spawn(serve(listener, daemon, shutdown_rx));

        let mut client = ClientStream::connect(&socket_path).await.unwrap();
        protocol::write_message(&mut client, &DaemonRequest::Get { content_hash: "hit".to_string() })
            .await
            .unwrap();
        let resp: DaemonResponse = protocol::read_message(&mut client).await.unwrap();
        match resp {
            DaemonResponse::Get { artifact: Some(a) } => assert_eq!(a.image_ref, "img:e2e"),
            other => panic!("expected Get hit, got {other:?}"),
        }

        shutdown_tx.send(true).unwrap();
        server_task.await.unwrap();
    }

    #[test]
    fn default_socket_path_matches_the_helm_chart_mount_contract() {
        assert_eq!(default_socket_path(), PathBuf::from("/var/run/sui-dockerfile-cache/node-cache.sock"));
    }
}
