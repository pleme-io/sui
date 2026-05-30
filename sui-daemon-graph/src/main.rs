//! sui-daemon-graph — the installable graph-server binary.
//!
//! ## What it does
//!
//! 1. Opens / creates a [`GraphStore`] at the configured root (default
//!    `/var/lib/sui/graph-store`, or `$XDG_DATA_HOME/sui/graph-store`
//!    for user-scope installations).
//! 2. Wraps the store in an [`LruHotCache`] (default 1024 entries).
//! 3. Stands up a [`GraphServer`] on a Unix socket (default resolved
//!    via tsunagu's `SocketPath::for_app("sui-graph")` — typically
//!    `$XDG_RUNTIME_DIR/sui-graph.sock`).
//! 4. Installs signal handlers for SIGINT + SIGTERM that drive a
//!    [`Shutdown`] controller — the server's accept loop exits within
//!    its drain budget when either fires.
//!
//! Coexists peacefully with the existing cppnix-worker-protocol
//! `sui-daemon` (different socket, different protocol). Both can run
//! on the same machine; nothing about either depends on the other.
//!
//! [`Shutdown`]: tsunagu::Shutdown

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use sui_daemon::{
    build_id_from_label, GraphHandler, GraphServer, GraphServerConfig, LruHotCache, StatsTracker,
};
use sui_graph_store::GraphStore;
use tracing::{info, warn};

/// CLI surface. Operators override defaults via env (the same vars
/// the nix profile sets in the systemd unit) or via the equivalent
/// command-line flags for ad-hoc invocation.
#[derive(Debug, Parser)]
#[command(
    name = "sui-daemon-graph",
    about = "Run the sui graph-server (rkyv-over-UDS L1 substrate)",
    version
)]
struct Args {
    /// Filesystem root of the GraphStore. On rio this is normally a
    /// dedicated ZFS dataset (`pool/sui/graph-store`).
    #[arg(long, env = "SUI_GRAPH_STORE_ROOT", default_value = "/var/lib/sui/graph-store")]
    store_root: PathBuf,

    /// Unix-socket path for client connections. Defaults to tsunagu's
    /// `SocketPath::for_app("sui-graph")`.
    #[arg(long, env = "SUI_GRAPH_SOCKET")]
    socket: Option<PathBuf>,

    /// Max body bytes per wire frame. Default 64 MiB — large enough
    /// for batched closure-info, small enough to catch runaway peers.
    #[arg(long, env = "SUI_GRAPH_MAX_FRAME_BYTES", default_value_t = sui_daemon_frame_default_max())]
    max_frame_bytes: u32,

    /// LRU capacity. Default 1024 entries.
    #[arg(long, env = "SUI_GRAPH_CACHE_CAPACITY", default_value_t = sui_daemon::DEFAULT_CAPACITY)]
    cache_capacity: usize,

    /// Build label, mixed into the build_id returned by Ping. Operators
    /// see this in `sui-graph-cli status` output.
    #[arg(long, env = "SUI_GRAPH_BUILD_LABEL", default_value = "sui-daemon-graph")]
    build_label: String,
}

const fn sui_daemon_frame_default_max() -> u32 {
    sui_daemon_frame::MAX_FRAME_BODY_BYTES
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let args = Args::parse();

    info!(
        target: "sui-daemon-graph",
        store_root = %args.store_root.display(),
        cache_capacity = args.cache_capacity,
        "starting"
    );

    let store = GraphStore::open(args.store_root.clone())
        .with_context(|| format!("open GraphStore at {}", args.store_root.display()))?;

    let cap = std::num::NonZeroUsize::new(args.cache_capacity)
        .context("cache capacity must be > 0")?;
    let cache = Arc::new(LruHotCache::with_capacity(cap));

    let mut stats = StatsTracker::default();
    stats.mark_started();
    let stats = Arc::new(stats);

    let handler = Arc::new(GraphHandler::new(
        store,
        cache,
        stats,
        build_id_from_label(&args.build_label),
    ));

    let socket = args
        .socket
        .unwrap_or_else(|| PathBuf::from(tsunagu::SocketPath::for_app("sui-graph")));
    let config = GraphServerConfig::at(socket).with_max_body(args.max_frame_bytes);

    let server = GraphServer::new(config.clone(), handler);
    let listener = server
        .bind()
        .with_context(|| format!("bind UDS at {}", config.socket_path.display()))?;

    info!(
        target: "sui-daemon-graph",
        socket = %config.socket_path.display(),
        "listening; signal SIGINT or SIGTERM to shut down"
    );

    let shutdown = signal_shutdown();
    server
        .run(listener, shutdown)
        .await
        .context("graph_server accept loop")?;

    info!(target: "sui-daemon-graph", "stopped");
    Ok(())
}

/// Build a shutdown future that fires on the first of SIGINT or
/// SIGTERM. Standard Linux daemon idiom.
async fn signal_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            warn!(target: "sui-daemon-graph", error = %e, "could not register SIGTERM handler");
            // Block forever if we can't register; SIGINT may still fire.
            std::future::pending::<()>().await;
            return;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            warn!(target: "sui-daemon-graph", error = %e, "could not register SIGINT handler");
            sigterm.recv().await;
            return;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => info!(target: "sui-daemon-graph", "SIGTERM received"),
        _ = sigint.recv() => info!(target: "sui-daemon-graph", "SIGINT received"),
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("SUI_DAEMON_GRAPH_LOG")
        .or_else(|_| EnvFilter::try_from_env("RUST_LOG"))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_level(true)
        .init();
}
