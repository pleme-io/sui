//! The [`DaemonClient`] itself.
//!
//! Architecture: one open `UnixStream` split into a reader half and a
//! writer half. A background task pumps frames off the read half and
//! routes responses by `RequestId` to per-call `oneshot::Sender`s. The
//! writer half is guarded by a `Mutex<OwnedWriteHalf>` so concurrent
//! `request_*` calls serialize their writes (each frame goes out
//! atomically; nothing interleaves).
//!
//! Why split: TCP-style multiplexing where many in-flight calls share
//! one socket. The CLI may issue dozens of `GetGraph` lookups in
//! parallel for one `nix flake show` — head-of-line blocking on
//! response order would erase the whole win of pipelining.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sui_daemon_frame::{read_frame, write_frame, MAX_FRAME_BODY_BYTES};
use sui_graph_store::GraphHash;
use sui_protocol::{
    ErrorCode, LocalRequest, LocalResponse, RequestId, StatsSnapshot, WireFrame,
};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, trace, warn};

use crate::error::ClientError;

/// Typed ping response.
#[derive(Debug, Clone, Copy)]
pub struct PingPong {
    pub build_id: [u8; 32],
    pub uptime_seconds: u64,
}

/// Typed put acknowledgement.
pub type PutAck = GraphHash;

/// Re-export of the protocol's stats snapshot for caller convenience.
pub type Stats = StatsSnapshot;

/// Typed multiplexed client for the graph-server.
pub struct DaemonClient {
    next_id: AtomicU64,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    in_flight: Arc<Mutex<HashMap<RequestId, oneshot::Sender<LocalResponse>>>>,
    _reader_task: JoinHandle<()>,
}

impl DaemonClient {
    /// Connect to the daemon's graph-server socket. Starts the reader
    /// task before returning; the connection is fully bidirectional
    /// the moment this function yields.
    ///
    /// # Errors
    ///
    /// [`ClientError::Connect`] if the socket can't be opened.
    pub async fn connect(socket: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket).await.map_err(|e| ClientError::Connect {
            path: socket.display().to_string(),
            source: e,
        })?;
        Ok(Self::from_stream(stream))
    }

    /// Build a client from an already-open [`UnixStream`]. Useful when
    /// the caller wants to control connect-time options (timeouts,
    /// abstract-socket paths) or when wiring against `DuplexStream` in
    /// tests.
    #[must_use]
    pub fn from_stream(stream: UnixStream) -> Self {
        let (read_half, write_half) = stream.into_split();
        let in_flight: Arc<Mutex<HashMap<RequestId, oneshot::Sender<LocalResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let in_flight_clone = in_flight.clone();
        let reader_task = tokio::spawn(async move {
            reader_loop(read_half, in_flight_clone).await;
        });
        Self {
            next_id: AtomicU64::new(1), // 0 is reserved for server pushes
            writer: Arc::new(Mutex::new(write_half)),
            in_flight,
            _reader_task: reader_task,
        }
    }

    /// Allocate a fresh request id. Skips 0 (reserved for server pushes).
    fn next_request_id(&self) -> RequestId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        } else {
            id
        }
    }

    /// Send a typed request and await its typed response. The core
    /// primitive every typed method below builds on. Concurrent calls
    /// are safe; each gets its own RequestId + oneshot channel.
    async fn request(&self, body: LocalRequest) -> Result<LocalResponse, ClientError> {
        let id = self.next_request_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.in_flight.lock().await;
            map.insert(id, tx);
        }

        let frame = WireFrame::Request { id, body };
        {
            let mut w = self.writer.lock().await;
            write_frame(&mut *w, &frame).await?;
        }
        trace!(target: "sui-daemon-client", request_id = id, "request sent");

        match rx.await {
            Ok(resp) => Ok(resp),
            Err(_) => {
                // Reader task dropped our sender — connection died.
                self.in_flight.lock().await.remove(&id);
                Err(ClientError::ConnectionClosed)
            }
        }
    }

    // ── Typed accessors ────────────────────────────────────────────

    /// Probe the daemon. Returns its build id + uptime.
    ///
    /// # Errors
    ///
    /// See [`ClientError`].
    pub async fn ping(&self) -> Result<PingPong, ClientError> {
        match self.request(LocalRequest::Ping).await? {
            LocalResponse::Pong {
                build_id,
                uptime_seconds,
            } => Ok(PingPong {
                build_id,
                uptime_seconds,
            }),
            LocalResponse::Error(e) => Err(map_server_error(e)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Fetch a graph blob by `(kind_tag, hash)`. The daemon mmaps from
    /// sui-graph-store and forwards the bytes; the client gets them in
    /// one memcpy via the UDS socket.
    ///
    /// # Errors
    ///
    /// See [`ClientError`].
    pub async fn get_graph(
        &self,
        kind_tag: u8,
        hash: GraphHash,
    ) -> Result<Vec<u8>, ClientError> {
        match self.request(LocalRequest::GetGraph { kind_tag, hash }).await? {
            LocalResponse::GraphBytes(b) => Ok(b),
            LocalResponse::Error(e) => Err(map_server_error(e)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Ingest a graph blob into the daemon's GraphStore. Daemon validates
    /// `BLAKE3(bytes) == hash` before persisting.
    ///
    /// # Errors
    ///
    /// See [`ClientError`].
    pub async fn put_graph(
        &self,
        kind_tag: u8,
        hash: GraphHash,
        bytes: Vec<u8>,
    ) -> Result<PutAck, ClientError> {
        match self
            .request(LocalRequest::PutGraph {
                kind_tag,
                hash,
                bytes,
            })
            .await?
        {
            LocalResponse::GraphStored { hash } => Ok(hash),
            LocalResponse::Error(e) => Err(map_server_error(e)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Snapshot of the daemon's operational counters.
    ///
    /// # Errors
    ///
    /// See [`ClientError`].
    pub async fn stats(&self) -> Result<Stats, ClientError> {
        match self.request(LocalRequest::GetStats).await? {
            LocalResponse::Stats(s) => Ok(s),
            LocalResponse::Error(e) => Err(map_server_error(e)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }
}

fn map_server_error(e: sui_protocol::LocalError) -> ClientError {
    ClientError::Server {
        code: e.code,
        message: e.message,
    }
}

async fn reader_loop(
    mut read_half: tokio::net::unix::OwnedReadHalf,
    in_flight: Arc<Mutex<HashMap<RequestId, oneshot::Sender<LocalResponse>>>>,
) {
    loop {
        match read_frame(&mut read_half, MAX_FRAME_BODY_BYTES).await {
            Ok(frame) => match frame {
                WireFrame::Response { id, body } => {
                    let tx = {
                        let mut map = in_flight.lock().await;
                        map.remove(&id)
                    };
                    if let Some(tx) = tx {
                        let _ = tx.send(body);
                    } else {
                        warn!(target: "sui-daemon-client", request_id = id, "no in-flight match");
                    }
                }
                WireFrame::Event { topic, payload } => {
                    // Future: route to a subscriber registry. For now,
                    // surface as trace.
                    trace!(
                        target: "sui-daemon-client",
                        topic = topic,
                        payload_bytes = payload.len(),
                        "server-push event (no subscriber wired yet)"
                    );
                }
                WireFrame::Heartbeat(_) | WireFrame::Goodbye | WireFrame::Request { .. } => {
                    // Client doesn't act on these. Goodbye triggers
                    // graceful shutdown by closing the loop; the others
                    // are noise.
                    if matches!(frame, WireFrame::Goodbye) {
                        debug!(target: "sui-daemon-client", "server sent Goodbye");
                        break;
                    }
                }
            },
            Err(e) => {
                // Any read error terminates the loop. In-flight senders
                // drop, callers see `ConnectionClosed`.
                debug!(target: "sui-daemon-client", error = %e, "reader_loop ended");
                break;
            }
        }
    }
    // Drop senders so anyone awaiting wakes up with the
    // canonical `ConnectionClosed` error.
    let mut map = in_flight.lock().await;
    map.clear();
    // `_ = ErrorCode::Internal;` — touch the enum so the import stays
    // load-bearing in case all matches are otherwise inferred.
    let _ = ErrorCode::Internal;
}
