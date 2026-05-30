//! Typed framing errors.

use std::io;

/// Errors from the framing layer. rkyv body-shape failures bubble up as
/// [`FrameError::Decode`]; transport failures (closed socket, partial read)
/// bubble up as [`FrameError::Io`].
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] io::Error),

    /// The first four bytes of a frame didn't match the expected magic
    /// (`"SUI1"`). Either the peer isn't speaking our protocol, or the
    /// stream is desynchronized — close the connection.
    #[error("bad frame magic: expected {expected:?} got {got:?}")]
    BadMagic { expected: [u8; 4], got: [u8; 4] },

    /// The frame's declared body length exceeds the per-connection cap.
    /// Default cap is 64 MiB — large enough for a full closure-info
    /// response, small enough to catch runaway peers.
    #[error("frame body length {got} exceeds cap {cap}")]
    FrameTooLarge { got: u32, cap: u32 },

    /// rkyv refused to encode/decode the body. Almost always a
    /// version-skew bug (we read what a newer peer wrote, or wrote
    /// using a stale enum that no longer round-trips).
    #[error("rkyv body codec: {0}")]
    Decode(String),
}
