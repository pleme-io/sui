//! sui-protocol — wire-type substrate for every IPC link in sui.
//!
//! ## Scope
//!
//! Today this crate ships the **local** protocol that the future
//! sui-daemon will speak with same-host CLIs and atticd: rkyv 0.8 over
//! a length-prefixed multiplex frame, over `tokio::net::UnixStream`.
//! It deliberately stops short of a daemon implementation — the goal is
//! to anchor the wire types so subsequent PRs (the daemon, the client
//! library, the CLI integration) all build on a single typed seam.
//!
//! ## Roadmap (queued for follow-up PRs)
//!
//! | Link | Wire format | Framing | Transport |
//! |---|---|---|---|
//! | daemon ↔ same-host CLI | rkyv 0.8 (this crate, today) | length-prefixed multiplex | `tokio::net::UnixStream` |
//! | daemon ↔ same-host atticd | rkyv 0.8 metadata + raw chunks | same multiplex | `UnixStream` (or shm ring later) |
//! | daemon ↔ remote-daemon | protobuf, REAPI-shaped | gRPC | `tonic` over HTTPS (Tailscale identity) |
//! | daemon ↔ remote-atticd | protobuf, tvix-castore-shaped | gRPC streaming | `tonic` over HTTPS |
//! | fleet events | JSON/CBOR | NATS subjects | `async-nats` (existing) |
//!
//! Each new wire surface lands as a new `mod` here and shares the
//! [`WireFrame`] envelope discipline: every connection starts with a
//! magic-bytes + version-negotiation handshake, the server downgrades
//! to `min(my_max, their_max)`, and we commit to an N-version compat
//! window in writing. Nix's worker-protocol pain (per Tweag's
//! "Re-implementing the Nix protocol in Rust" post-mortem) was the
//! *absence* of a self-describing schema — sui won't repeat it.
//!
//! ## Behavior contract
//!
//! This crate ships **wire types and the version-negotiation
//! primitives only**. No I/O, no daemon, no client connection logic.
//! Subsequent crates depend on this for the wire surface and supply
//! the I/O loop themselves. That separation means a future "fuzz the
//! wire protocol" tool, or a future "drive the daemon from a
//! WebAssembly client", or a future "swap UnixStream for tcp", all
//! reuse the same types unchanged.

#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod local;
pub mod version;

pub use local::{
    Heartbeat, LocalError, LocalRequest, LocalResponse, RequestId, WireFrame,
    FRAME_MAGIC,
};
pub use version::{NegotiatedVersion, VersionHandshake, MAX_LOCAL_PROTOCOL_VERSION, MIN_LOCAL_PROTOCOL_VERSION};
