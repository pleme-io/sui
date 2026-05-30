//! Graph-protocol server — accepts client connections on a Unix
//! socket, dispatches typed [`LocalRequest`]s via a [`GraphRequestHandler`],
//! and writes typed [`LocalResponse`]s back. Multiplexed per-connection
//! by [`RequestId`].
//!
//! Architecture:
//!
//! ```text
//! GraphServer::run
//!   └── UnixListener::accept loop
//!         └── per-connection task (spawn_connection)
//!               ├── (read_half) read frames → handler → (write_half) write frames
//!               ├── shared writer Mutex so handler tasks don't interleave bytes
//!               └── handler can be wrapped in any future caching layer
//! ```
//!
//! Coexists with the existing cppnix-worker-protocol server in
//! `server.rs`. Different protocol, different socket; the binary is
//! free to spin up either or both.

use std::path::PathBuf;
use std::sync::Arc;

use sui_daemon_frame::{read_frame, write_frame, FrameCodec, MAX_FRAME_BODY_BYTES};
use sui_protocol::{
    LocalRequest, RequestId, WireFrame,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::graph_handler::GraphRequestHandler;

/// Where the graph server listens. Defaults follow the XDG_RUNTIME_DIR
/// convention via tsunagu; callers can override for tests / system
/// installations.
#[derive(Debug, Clone)]
pub struct GraphServerConfig {
    pub socket_path: PathBuf,
    pub max_frame_body_bytes: u32,
}

impl GraphServerConfig {
    /// Resolve `<XDG_RUNTIME_DIR>/sui/graph.sock` via tsunagu, falling
    /// back to a tmpdir-rooted path when the runtime dir is unavailable
    /// (CI, container).
    #[must_use]
    pub fn xdg_default() -> Self {
        let path = tsunagu::SocketPath::for_app("sui-graph");
        Self {
            socket_path: path.into(),
            max_frame_body_bytes: MAX_FRAME_BODY_BYTES,
        }
    }

    /// Explicit socket path. Caller is responsible for ensuring the
    /// parent dir exists and the path isn't already bound.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self {
            socket_path: path,
            max_frame_body_bytes: MAX_FRAME_BODY_BYTES,
        }
    }

    /// Override the per-frame body cap (default 64 MiB).
    #[must_use]
    pub fn with_max_body(mut self, cap: u32) -> Self {
        self.max_frame_body_bytes = cap;
        self
    }
}

/// Runs the graph server loop.
pub struct GraphServer<H>
where
    H: GraphRequestHandler,
{
    config: GraphServerConfig,
    handler: Arc<H>,
}

impl<H> GraphServer<H>
where
    H: GraphRequestHandler,
{
    /// Build a server. Doesn't bind yet — `run` does that.
    #[must_use]
    pub fn new(config: GraphServerConfig, handler: Arc<H>) -> Self {
        Self { config, handler }
    }

    /// Bind the socket. Removes any stale bound socket at the same
    /// path first (`SO_REUSEADDR`-style cleanup that's idiomatic for
    /// UDS).
    ///
    /// # Errors
    ///
    /// Surfaces tokio's bind error if the socket can't be bound.
    pub fn bind(&self) -> std::io::Result<UnixListener> {
        // Best-effort cleanup of a stale socket left over from a crashed
        // prior instance. Ignore errors — bind will surface the real one.
        let _ = std::fs::remove_file(&self.config.socket_path);
        if let Some(parent) = self.config.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        UnixListener::bind(&self.config.socket_path)
    }

    /// Accept loop. Each accepted connection is handed to a dedicated
    /// task. The future completes when `shutdown` resolves (typically
    /// driven by a tsunagu [`Shutdown`] receiver).
    ///
    /// [`Shutdown`]: tsunagu::Shutdown
    ///
    /// # Errors
    ///
    /// Propagates the bind error; per-connection errors are logged but
    /// do not bring down the loop.
    pub async fn run<F>(self, listener: UnixListener, shutdown: F) -> std::io::Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        info!(
            target: "sui-daemon::graph",
            socket = %self.config.socket_path.display(),
            "graph server accepting"
        );
        let codec = FrameCodec::with_cap(self.config.max_frame_body_bytes);
        let handler = self.handler.clone();
        let shutdown = Box::pin(shutdown);

        let mut shutdown = shutdown;
        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, _addr)) => {
                            let handler_clone = handler.clone();
                            let _: JoinHandle<()> = tokio::spawn(async move {
                                if let Err(e) = serve_connection(stream, handler_clone, codec).await {
                                    debug!(
                                        target: "sui-daemon::graph",
                                        error = %e,
                                        "connection ended"
                                    );
                                }
                            });
                        }
                        Err(e) => {
                            warn!(
                                target: "sui-daemon::graph",
                                error = %e,
                                "accept error; continuing"
                            );
                        }
                    }
                }
                _ = &mut shutdown => {
                    info!(target: "sui-daemon::graph", "shutdown signal — closing listener");
                    break;
                }
            }
        }
        // Best-effort socket cleanup so the next bind doesn't trip on it.
        let _ = std::fs::remove_file(&self.config.socket_path);
        Ok(())
    }
}

/// Drive one connection: read → dispatch → write, in a loop, until the
/// peer hangs up or a fatal frame error occurs.
async fn serve_connection<H>(
    stream: UnixStream,
    handler: Arc<H>,
    codec: FrameCodec,
) -> Result<(), sui_daemon_frame::FrameError>
where
    H: GraphRequestHandler,
{
    let (mut read_half, write_half) = stream.into_split();
    let writer = Arc::new(Mutex::new(write_half));

    loop {
        let frame = match read_frame(&mut read_half, codec.max_body_bytes).await {
            Ok(f) => f,
            Err(sui_daemon_frame::FrameError::Io(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                // Clean disconnect.
                break;
            }
            Err(e) => return Err(e),
        };

        match frame {
            WireFrame::Request { id, body } => {
                dispatch_request(id, body, handler.clone(), writer.clone()).await;
            }
            WireFrame::Heartbeat(hb) => {
                // Echo the same nonce — keepalive contract.
                let mut w = writer.lock().await;
                write_frame(&mut *w, &WireFrame::Heartbeat(hb)).await?;
            }
            WireFrame::Goodbye => {
                debug!(target: "sui-daemon::graph", "client sent Goodbye");
                break;
            }
            WireFrame::Response { .. } | WireFrame::Event { .. } => {
                warn!(target: "sui-daemon::graph", "client sent non-request frame; ignoring");
            }
        }
    }
    Ok(())
}

/// Spawn a per-request task so concurrent in-flight requests on one
/// connection don't head-of-line block each other.
async fn dispatch_request<H>(
    id: RequestId,
    body: LocalRequest,
    handler: Arc<H>,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) where
    H: GraphRequestHandler,
{
    tokio::spawn(async move {
        let response_body = handler.handle(body).await;
        let frame = WireFrame::Response {
            id,
            body: response_body,
        };
        let mut w = writer.lock().await;
        if let Err(e) = write_frame(&mut *w, &frame).await {
            error!(
                target: "sui-daemon::graph",
                request_id = id,
                error = %e,
                "failed to write response"
            );
        }
    });
}
