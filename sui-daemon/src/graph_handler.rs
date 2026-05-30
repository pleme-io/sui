//! Stateful request handler for the graph protocol.
//!
//! A [`GraphHandler`] owns a [`GraphStore`] (mandatory) + an
//! [`LruHotCache`] (optional but recommended) + a [`StatsTracker`].
//! Every method maps one [`LocalRequest`] variant to its corresponding
//! [`LocalResponse`]. Errors are typed via [`LocalError`] + [`ErrorCode`]
//! and returned over the wire — never panic; never bring the daemon
//! down on a client mistake.
//!
//! The trait split [`GraphRequestHandler`] is there so the connection
//! loop in `graph_server.rs` works against an interface, not a concrete
//! type — drop-in test doubles and future caching-layer wrappers all
//! compose cleanly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use sui_graph_store::{GraphHash, GraphKind, GraphStore};
use sui_protocol::{ErrorCode, LocalError, LocalRequest, LocalResponse, StatsSnapshot};
use tracing::warn;

use crate::hot_cache::LruHotCache;

/// Operational counters carried in [`StatsSnapshot`].
#[derive(Debug, Default)]
pub struct StatsTracker {
    started_at: Option<Instant>,
    puts: AtomicU64,
}

impl StatsTracker {
    /// Record the daemon's start instant. Called once on bringup.
    pub fn mark_started(&mut self) {
        self.started_at = Some(Instant::now());
    }

    /// Bump the put counter. Called on every successful `PutGraph`.
    pub fn record_put(&self) {
        self.puts.fetch_add(1, Ordering::Relaxed);
    }

    /// Seconds since `mark_started`. Zero if not yet marked.
    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.map_or(0, |s| s.elapsed().as_secs())
    }

    pub fn puts(&self) -> u64 {
        self.puts.load(Ordering::Relaxed)
    }
}

/// The trait the graph_server connection loop drives.
#[async_trait]
pub trait GraphRequestHandler: Send + Sync + 'static {
    async fn handle(&self, request: LocalRequest) -> LocalResponse;
}

/// Concrete handler that fronts a [`GraphStore`] + [`LruHotCache`].
pub struct GraphHandler {
    store: GraphStore,
    cache: Arc<LruHotCache>,
    stats: Arc<StatsTracker>,
    build_id: [u8; 32],
}

impl GraphHandler {
    /// Build a handler.
    ///
    /// `build_id` is an opaque 32-byte identity returned in `Ping`
    /// responses (and `VersionHandshake` if you wire that). Conventional
    /// pattern: BLAKE3 of `git rev-parse HEAD` + build timestamp.
    #[must_use]
    pub fn new(
        store: GraphStore,
        cache: Arc<LruHotCache>,
        stats: Arc<StatsTracker>,
        build_id: [u8; 32],
    ) -> Self {
        Self {
            store,
            cache,
            stats,
            build_id,
        }
    }

    fn parse_kind(tag: u8) -> Result<GraphKind, LocalResponse> {
        GraphKind::from_tag(tag).ok_or_else(|| {
            LocalResponse::Error(LocalError {
                code: ErrorCode::InvalidGraphKind,
                message: format!("unknown graph kind tag: {tag}"),
            })
        })
    }

    fn ping(&self) -> LocalResponse {
        LocalResponse::Pong {
            build_id: self.build_id,
            uptime_seconds: self.stats.uptime_seconds(),
        }
    }

    fn get_graph(&self, kind_tag: u8, hash: GraphHash) -> LocalResponse {
        let kind = match Self::parse_kind(kind_tag) {
            Ok(k) => k,
            Err(resp) => return resp,
        };
        // Tier 1: hot cache.
        if let Some(bytes) = self.cache.get(kind, hash) {
            return LocalResponse::GraphBytes(bytes.to_vec());
        }
        // Tier 2: store (mmap + cast).
        match self.store.get(kind, hash) {
            Ok(blob) => {
                let bytes = Bytes::from(blob.as_ref().to_vec());
                self.cache.put(kind, hash, bytes.clone());
                LocalResponse::GraphBytes(bytes.to_vec())
            }
            Err(sui_graph_store::Error::NotFound { .. }) => LocalResponse::Error(LocalError {
                code: ErrorCode::GraphNotFound,
                message: format!("no blob for hash {hash}"),
            }),
            Err(e) => {
                warn!(target: "sui-daemon::graph", error = %e, "store get failed");
                LocalResponse::Error(LocalError {
                    code: ErrorCode::StoreUnavailable,
                    message: e.to_string(),
                })
            }
        }
    }

    fn put_graph(&self, kind_tag: u8, hash: GraphHash, bytes: Vec<u8>) -> LocalResponse {
        let kind = match Self::parse_kind(kind_tag) {
            Ok(k) => k,
            Err(resp) => return resp,
        };
        match self.store.put(kind, hash, &bytes) {
            Ok(()) => {
                // Warm the cache on put so the immediate-read-after-put
                // pattern (common in tend prebuild) is sub-microsecond.
                self.cache.put(kind, hash, Bytes::from(bytes));
                self.stats.record_put();
                LocalResponse::GraphStored { hash }
            }
            Err(sui_graph_store::Error::HashMismatch { expected, actual }) => {
                LocalResponse::Error(LocalError {
                    code: ErrorCode::GraphHashMismatch,
                    message: format!("expected {expected} got {actual}"),
                })
            }
            Err(e) => {
                warn!(target: "sui-daemon::graph", error = %e, "store put failed");
                LocalResponse::Error(LocalError {
                    code: ErrorCode::StoreUnavailable,
                    message: e.to_string(),
                })
            }
        }
    }

    fn get_stats(&self) -> LocalResponse {
        LocalResponse::Stats(StatsSnapshot {
            hot_cache_entries: self.cache.len() as u64,
            hot_cache_bytes: self.cache.total_bytes(),
            cache_hits: self.cache.hits(),
            cache_misses: self.cache.misses(),
            puts: self.stats.puts(),
            uptime_seconds: self.stats.uptime_seconds(),
        })
    }
}

#[async_trait]
impl GraphRequestHandler for GraphHandler {
    async fn handle(&self, request: LocalRequest) -> LocalResponse {
        match request {
            LocalRequest::Ping => self.ping(),
            LocalRequest::GetGraph { kind_tag, hash } => self.get_graph(kind_tag, hash),
            LocalRequest::PutGraph {
                kind_tag,
                hash,
                bytes,
            } => self.put_graph(kind_tag, hash, bytes),
            LocalRequest::GetStats => self.get_stats(),
        }
    }
}

/// Build a stable `build_id` from a string label. Convenience for
/// callers that don't want to thread `git rev-parse` themselves.
#[must_use]
pub fn build_id_from_label(label: &str) -> [u8; 32] {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut hasher = blake3::Hasher::new();
    hasher.update(label.as_bytes());
    hasher.update(b"::");
    hasher.update(&secs.to_le_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    fn fresh() -> (tempfile::TempDir, GraphHandler) {
        let dir = tempdir().unwrap();
        let store = GraphStore::open(dir.path().to_path_buf()).unwrap();
        let cache = Arc::new(LruHotCache::new());
        let mut stats = StatsTracker::default();
        stats.mark_started();
        let h = GraphHandler::new(
            store,
            cache,
            Arc::new(stats),
            build_id_from_label("test"),
        );
        (dir, h)
    }

    #[tokio::test]
    async fn ping_returns_uptime_and_build_id() {
        let (_d, h) = fresh();
        let resp = h.handle(LocalRequest::Ping).await;
        match resp {
            LocalResponse::Pong { build_id, .. } => {
                assert_ne!(build_id, [0u8; 32]);
            }
            _ => panic!("expected Pong"),
        }
    }

    #[tokio::test]
    async fn put_then_get_round_trips_via_handler() {
        let (_d, h) = fresh();
        let payload = b"hello via handler".to_vec();
        let hash = GraphHash::of(&payload);

        let put = h
            .handle(LocalRequest::PutGraph {
                kind_tag: GraphKind::Lockfile.tag(),
                hash,
                bytes: payload.clone(),
            })
            .await;
        assert!(matches!(put, LocalResponse::GraphStored { .. }));

        let get = h
            .handle(LocalRequest::GetGraph {
                kind_tag: GraphKind::Lockfile.tag(),
                hash,
            })
            .await;
        match get {
            LocalResponse::GraphBytes(b) => assert_eq!(b, payload),
            other => panic!("expected GraphBytes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_kind_tag_returns_typed_error() {
        let (_d, h) = fresh();
        let resp = h
            .handle(LocalRequest::GetGraph {
                kind_tag: 255,
                hash: GraphHash::of(b"x"),
            })
            .await;
        match resp {
            LocalResponse::Error(e) => {
                assert!(matches!(e.code, ErrorCode::InvalidGraphKind));
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_blob_returns_not_found() {
        let (_d, h) = fresh();
        let resp = h
            .handle(LocalRequest::GetGraph {
                kind_tag: GraphKind::Ast.tag(),
                hash: GraphHash::of(b"nothing"),
            })
            .await;
        match resp {
            LocalResponse::Error(e) => {
                assert!(matches!(e.code, ErrorCode::GraphNotFound));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn put_hash_mismatch_returns_typed_error() {
        let (_d, h) = fresh();
        let payload = b"real content".to_vec();
        let wrong = GraphHash::of(b"different content");
        let resp = h
            .handle(LocalRequest::PutGraph {
                kind_tag: GraphKind::Lockfile.tag(),
                hash: wrong,
                bytes: payload,
            })
            .await;
        match resp {
            LocalResponse::Error(e) => assert!(matches!(e.code, ErrorCode::GraphHashMismatch)),
            other => panic!("expected mismatch error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stats_increments_with_traffic() {
        let (_d, h) = fresh();
        let bytes = b"stats payload".to_vec();
        let hash = GraphHash::of(&bytes);
        h.handle(LocalRequest::PutGraph {
            kind_tag: GraphKind::Module.tag(),
            hash,
            bytes,
        })
        .await;
        h.handle(LocalRequest::GetGraph {
            kind_tag: GraphKind::Module.tag(),
            hash,
        })
        .await;
        let s = h.handle(LocalRequest::GetStats).await;
        match s {
            LocalResponse::Stats(s) => {
                assert_eq!(s.puts, 1);
                // hot-cache warmed on put, so cache_hits records the get.
                assert_eq!(s.cache_hits, 1);
            }
            other => panic!("expected Stats, got {other:?}"),
        }
    }
}
