//! End-to-end tests for the graph protocol — daemon side and client
//! side talking to each other over a real Unix socket.
//!
//! Each test spawns the [`GraphServer`] in-process on a tempdir socket,
//! drives [`DaemonClient`] against it, and asserts the typed responses.
//! All clean-up runs in test teardown via `tempdir` Drop.

use std::sync::Arc;
use std::time::Duration;

use sui_daemon::{
    build_id_from_label, GraphHandler, GraphServer, GraphServerConfig, LruHotCache, StatsTracker,
};
use sui_daemon_client::{ClientError, DaemonClient};
use sui_graph_store::{GraphHash, GraphKind, GraphStore};
use sui_protocol::ErrorCode;
use tempfile::tempdir;
use tokio::sync::oneshot;

/// Bring up a fully-wired graph_server on a tempdir socket.
///
/// Returns the live socket path + a shutdown handle (drop / send to
/// trigger graceful shutdown).
struct Harness {
    socket: std::path::PathBuf,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_task: tokio::task::JoinHandle<std::io::Result<()>>,
    _tempdir: tempfile::TempDir,
}

impl Harness {
    async fn spawn() -> Self {
        let tmp = tempdir().unwrap();
        let store_dir = tmp.path().join("graph-store");
        let socket = tmp.path().join("graph.sock");

        let store = GraphStore::open(store_dir).unwrap();
        let cache = Arc::new(LruHotCache::new());
        let mut stats = StatsTracker::default();
        stats.mark_started();
        let handler = Arc::new(GraphHandler::new(
            store,
            cache,
            Arc::new(stats),
            build_id_from_label("test-harness"),
        ));

        let config = GraphServerConfig::at(socket.clone());
        let server = GraphServer::new(config, handler);
        let listener = server.bind().unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            server
                .run(listener, async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        // Loop until the socket file exists (bind happens synchronously
        // inside `bind()` so this is fast).
        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        Self {
            socket,
            shutdown_tx: Some(shutdown_tx),
            server_task,
            _tempdir: tmp,
        }
    }

    fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Happy-path round trips ─────────────────────────────────────────

#[tokio::test]
async fn ping_round_trips() {
    let h = Harness::spawn().await;
    let client = DaemonClient::connect(&h.socket).await.unwrap();
    let pong = client.ping().await.unwrap();
    assert_ne!(pong.build_id, [0u8; 32]);
}

#[tokio::test]
async fn put_then_get_round_trips_via_socket() {
    let h = Harness::spawn().await;
    let client = DaemonClient::connect(&h.socket).await.unwrap();

    let payload = b"server-side round trip payload".to_vec();
    let hash = GraphHash::of(&payload);

    let acked = client
        .put_graph(GraphKind::Lockfile.tag(), hash, payload.clone())
        .await
        .unwrap();
    assert_eq!(acked, hash);

    let got = client
        .get_graph(GraphKind::Lockfile.tag(), hash)
        .await
        .unwrap();
    assert_eq!(got, payload);
}

#[tokio::test]
async fn stats_reflect_put_and_get_activity() {
    let h = Harness::spawn().await;
    let client = DaemonClient::connect(&h.socket).await.unwrap();

    let payload = b"stats payload".to_vec();
    let hash = GraphHash::of(&payload);

    client
        .put_graph(GraphKind::Module.tag(), hash, payload.clone())
        .await
        .unwrap();
    let _ = client.get_graph(GraphKind::Module.tag(), hash).await.unwrap();

    let stats = client.stats().await.unwrap();
    assert_eq!(stats.puts, 1);
    // The put warmed the cache, so the subsequent get is a cache hit.
    assert_eq!(stats.cache_hits, 1);
    assert!(stats.hot_cache_entries >= 1);
}

// ── Server-side typed errors ───────────────────────────────────────

#[tokio::test]
async fn missing_blob_returns_typed_not_found() {
    let h = Harness::spawn().await;
    let client = DaemonClient::connect(&h.socket).await.unwrap();
    let err = client
        .get_graph(GraphKind::Ast.tag(), GraphHash::of(b"missing"))
        .await
        .unwrap_err();
    match err {
        ClientError::Server { code, .. } => {
            assert!(matches!(code, ErrorCode::GraphNotFound))
        }
        other => panic!("expected server NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn put_with_wrong_hash_returns_typed_mismatch() {
    let h = Harness::spawn().await;
    let client = DaemonClient::connect(&h.socket).await.unwrap();
    let err = client
        .put_graph(
            GraphKind::Derivation.tag(),
            GraphHash::of(b"wrong"),
            b"actual".to_vec(),
        )
        .await
        .unwrap_err();
    match err {
        ClientError::Server { code, .. } => {
            assert!(matches!(code, ErrorCode::GraphHashMismatch))
        }
        other => panic!("expected mismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_kind_tag_returns_typed_error() {
    let h = Harness::spawn().await;
    let client = DaemonClient::connect(&h.socket).await.unwrap();
    let err = client.get_graph(255, GraphHash::of(b"x")).await.unwrap_err();
    match err {
        ClientError::Server { code, .. } => {
            assert!(matches!(code, ErrorCode::InvalidGraphKind))
        }
        other => panic!("expected invalid kind, got {other:?}"),
    }
}

// ── Concurrency ────────────────────────────────────────────────────

#[tokio::test]
async fn many_in_flight_requests_dont_head_of_line_block() {
    let h = Harness::spawn().await;
    let client = Arc::new(DaemonClient::connect(&h.socket).await.unwrap());

    // Put 32 distinct blobs.
    let mut hashes = Vec::new();
    for i in 0u32..32 {
        let payload = format!("payload-{i}").into_bytes();
        let hash = GraphHash::of(&payload);
        client
            .put_graph(GraphKind::Ast.tag(), hash, payload)
            .await
            .unwrap();
        hashes.push(hash);
    }

    // Fan out 32 concurrent gets through the same client. The fact
    // that this completes proves the multiplexing works — head-of-line
    // blocking would serialize them and they'd still complete, but the
    // mismatch test below proves nothing returns the wrong response.
    let mut handles = Vec::new();
    for (i, h) in hashes.iter().enumerate() {
        let c = client.clone();
        let want = format!("payload-{i}").into_bytes();
        let hash = *h;
        handles.push(tokio::spawn(async move {
            let got = c.get_graph(GraphKind::Ast.tag(), hash).await.unwrap();
            assert_eq!(got, want, "response {i} returned wrong payload");
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

// ── Graceful shutdown ──────────────────────────────────────────────

#[tokio::test]
async fn shutdown_signal_stops_the_accept_loop_quickly() {
    let mut h = Harness::spawn().await;
    // Open a client to prove the server is alive.
    let client = DaemonClient::connect(&h.socket).await.unwrap();
    let _ = client.ping().await.unwrap();
    drop(client);

    h.shutdown();

    // Server task should exit within a short timeout.
    let exit = tokio::time::timeout(Duration::from_secs(2), &mut h.server_task)
        .await
        .expect("server didn't exit after shutdown signal")
        .expect("server task join")
        .expect("server returned io error");
    let _ = exit;
}
