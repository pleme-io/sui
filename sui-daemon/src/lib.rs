//! Nix daemon replacement — worker protocol server.
//!
//! Clean-room implementation of the Nix worker protocol over Unix sockets.
//!
//! # Architecture
//!
//! ```text
//! UnixListener (server.rs)
//!     └── per-connection task
//!             └── Connection (connection/)
//!                     ├── wire.rs       — async LE-u64 / padded-string I/O
//!                     ├── handshake.rs  — magic + version negotiation
//!                     └── dispatch.rs   — opcode dispatch loop + handlers
//! ```
//!
//! The daemon listens on a Unix socket (default: `/nix/var/nix/daemon-socket/socket`),
//! accepts client connections, and handles the binary worker protocol. Each
//! connection gets a dedicated tokio task.
//!
//! # Trust
//!
//! Trust is determined from Unix peer credentials (UID). Root and the daemon's
//! own UID are considered trusted. The [`PeerCredentials`] trait abstracts
//! credential retrieval for testability.

pub mod config;
pub mod connection;
pub mod server;
pub mod trust;

// ── L1 graph protocol (additive, coexists with cppnix worker protocol) ──
//
// New surface on a separate socket. The existing cppnix-worker protocol
// (`server.rs` + `connection/`) stays the only producer of the
// `/nix/var/nix/daemon-socket/socket` endpoint; nothing about its
// behavior changes. The graph_server below speaks the rkyv-over-UDS
// sui-protocol on its own XDG-runtime socket.
pub mod graph_handler;
pub mod graph_server;
pub mod hot_cache;

pub use config::SuiDaemonConfig;
pub use connection::{Connection, ConnectionError};
pub use server::{DaemonConfig, DaemonError, DaemonServer, DEFAULT_SOCKET_PATH, xdg_socket_path};
pub use trust::{PeerCredentials, SystemPeerCredentials, TrustLevel};

pub use graph_handler::{
    build_id_from_label, GraphHandler, GraphRequestHandler, StatsTracker,
};
pub use graph_server::{GraphServer, GraphServerConfig};
pub use hot_cache::{LruHotCache, DEFAULT_CAPACITY};
