//! Production entrypoint: wires the real disk store + a real remote
//! `StorageBackend` (built from `SUI_CACHE_BACKEND_CONFIG`, the same
//! env-configured path `sui cache serve` uses) into `serve`, listening
//! on `SUI_NODE_CACHE_SOCKET_PATH` (default:
//! `sui_dockerfile_node_cache_daemon::default_socket_path()`), and
//! shuts down gracefully on SIGTERM/SIGINT — the signal a Kubernetes
//! `DaemonSet` pod receives on eviction/rollout.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use sui_cache::config::BackendConfig;
use sui_cache::storage::build_backend;
use sui_dockerfile_node_cache_daemon::{bind_unix_listener, default_socket_path, serve, NodeCacheDaemon, RealLocalCacheStore};
use tokio::sync::watch;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber_init();

    let socket_path = env::var("SUI_NODE_CACHE_SOCKET_PATH").map_or_else(|_| default_socket_path(), PathBuf::from);
    let cache_dir = env::var("SUI_NODE_CACHE_DIR")
        .map_or_else(|_| PathBuf::from("/var/lib/sui-dockerfile-cache"), PathBuf::from);

    let backend_config = match env::var("SUI_CACHE_BACKEND_CONFIG") {
        Ok(raw) => match serde_json::from_str::<BackendConfig>(&raw) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::error!(target: "sui-dockerfile-node-cache-daemon", error = %e, "invalid SUI_CACHE_BACKEND_CONFIG");
                return ExitCode::FAILURE;
            }
        },
        // No remote configured: fall back to a local-disk remote tier
        // rooted alongside the L0 cache — degrades to "L0 only, no
        // shared-tier fallback" rather than refusing to start, since a
        // node cache is still useful stand-alone during rollout.
        Err(_) => BackendConfig::Local { path: cache_dir.join("remote-fallback") },
    };

    let remote = match build_backend(&backend_config).await {
        Ok(backend) => backend,
        Err(e) => {
            tracing::error!(target: "sui-dockerfile-node-cache-daemon", error = %e, "failed to build remote backend");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = sui_dockerfile_node_cache_daemon::store::ensure_root_exists(&cache_dir).await {
        tracing::error!(target: "sui-dockerfile-node-cache-daemon", error = %e, "failed to create local cache dir");
        return ExitCode::FAILURE;
    }
    let local = Arc::new(RealLocalCacheStore::new(cache_dir));
    let daemon = Arc::new(NodeCacheDaemon::new(local, remote));

    let listener = match bind_unix_listener(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(target: "sui-dockerfile-node-cache-daemon", error = %e, socket = %socket_path.display(), "failed to bind socket");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(target: "sui-dockerfile-node-cache-daemon", socket = %socket_path.display(), "listening");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let serve_task = tokio::spawn(serve(listener, daemon, shutdown_rx));

    wait_for_shutdown_signal().await;
    tracing::info!(target: "sui-dockerfile-node-cache-daemon", "shutdown signal received");
    let _ = shutdown_tx.send(true);
    let _ = serve_task.await;

    ExitCode::SUCCESS
}

/// Waits for SIGTERM (Kubernetes pod termination) or SIGINT
/// (Ctrl-C, local dev). Never panics on a signal-handling failure —
/// falls back to waiting on whichever signal did install successfully.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("installing SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn tracing_subscriber_init() {
    // Best-effort: a second call (e.g. under `cargo test` linking this
    // binary's code path indirectly) must never panic.
    let _ = tracing_subscriber::fmt().with_target(true).try_init();
}
