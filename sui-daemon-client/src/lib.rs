//! sui-daemon-client — typed multiplexed client for the graph-server.
//!
//! ## Use
//!
//! ```no_run
//! # async fn _ex() -> Result<(), sui_daemon_client::ClientError> {
//! use sui_daemon_client::DaemonClient;
//! use std::path::Path;
//!
//! let client = DaemonClient::connect(Path::new("/run/sui/graph.sock")).await?;
//! let pong = client.ping().await?;
//! println!("daemon uptime: {}s", pong.uptime_seconds);
//! # Ok(()) }
//! ```
//!
//! ## Behavior contract
//!
//! * One [`DaemonClient`] owns one open `UnixStream`.
//! * Requests are multiplexed by [`sui_protocol::RequestId`]. A client may
//!   have many in-flight requests on the same connection — responses
//!   are routed back via an internal `HashMap<RequestId, oneshot::Sender>`.
//! * The connection is split into a reader task and a writer task on
//!   construction; both terminate when the client drops or the peer
//!   closes the socket.
//! * Errors are typed via [`ClientError`]. Transport failures close the
//!   connection; per-request errors (from the server) come back as
//!   [`ClientError::Server`] with the typed [`sui_protocol::ErrorCode`].
//!
//! ## Reuse surface
//!
//! - sui CLI — `sui flake show` / `sui eval` consult the daemon for
//!   warm-cache hits.
//! - tend prebuild — pushes built closures into the daemon's GraphStore.
//! - tests across the workspace — drive the daemon end-to-end without
//!   touching system sockets.

#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

mod client;
mod error;

pub use client::{DaemonClient, PingPong, PutAck, Stats};
pub use error::ClientError;
