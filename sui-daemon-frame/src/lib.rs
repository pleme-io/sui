//! sui-daemon-frame — async frame codec for the rkyv-over-UDS local protocol.
//!
//! ## Wire format
//!
//! ```text
//! [magic : 4B = "SUI1"] [len : 4B u32 LE] [body : N bytes of rkyv-archived WireFrame]
//! ```
//!
//! The magic + length prefix is what lets a misaligned reader fail fast.
//! rkyv's own validation pass (`rkyv::access`) catches body-shape errors;
//! this codec only enforces the framing envelope.
//!
//! ## Scope
//!
//! This crate is **transport-agnostic**: it codes against `tokio::io::Async{Read,Write}`,
//! so the same primitives work for `UnixStream`, `TcpStream`, in-memory
//! `DuplexStream` (great for tests), or any future Bytes-stream abstraction.
//!
//! ## Reuse surface
//!
//! - `sui-daemon::graph_server` — reads frames from each accepted client.
//! - `sui-daemon-client` — writes requests + reads responses on a single
//!   multiplexed connection.
//! - Tests across the workspace — drive `DuplexStream` end-to-end without
//!   touching the filesystem.
//! - Future fuzzers — call [`read_frame`] on arbitrary bytes and assert
//!   only framing-level invariants (rkyv validation is downstream).

#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

mod codec;
mod error;

pub use codec::{read_frame, write_frame, FrameCodec, MAX_FRAME_BODY_BYTES};
pub use error::FrameError;

// Re-export the wire frame type so callers don't need both crate deps for
// the common case of "give me a frame, send me a frame."
pub use sui_protocol::WireFrame;
