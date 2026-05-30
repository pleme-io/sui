//! Typed client errors.

use sui_protocol::ErrorCode;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("connect to daemon at {path}: {source}")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("frame codec: {0}")]
    Frame(#[from] sui_daemon_frame::FrameError),

    /// The reader task closed before the response arrived. Usually
    /// means the daemon hung up, crashed, or the connection was
    /// interrupted mid-request.
    #[error("connection closed before response arrived")]
    ConnectionClosed,

    /// The daemon returned a typed error for this request.
    #[error("daemon returned {code:?}: {message}")]
    Server { code: ErrorCode, message: String },

    /// Got a response shape we weren't expecting (e.g. a `Stats`
    /// response to a `Ping` request). Indicates protocol corruption
    /// or a daemon bug — close the connection.
    #[error("daemon returned unexpected response shape for this request")]
    UnexpectedResponse,

    /// Tokio task join error — only possible if the worker tasks panic.
    #[error("internal task: {0}")]
    Join(#[from] tokio::task::JoinError),
}
