//! Nix daemon replacement — worker protocol server.
//!
//! Clean-room implementation of the Nix worker protocol over Unix sockets.
//!
//! # Architecture
//!
//! ```text
//! UnixListener (server.rs)
//!     └── per-connection task
//!             └── Connection (connection.rs)
//!                     ├── handshake()   — magic + version negotiation
//!                     └── run()         — opcode dispatch loop
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

pub use config::SuiDaemonConfig;
pub use server::{DaemonConfig, DaemonError, DaemonServer, DEFAULT_SOCKET_PATH, xdg_socket_path};
pub use trust::{PeerCredentials, SystemPeerCredentials, TrustLevel};
