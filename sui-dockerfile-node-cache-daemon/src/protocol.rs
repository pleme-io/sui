//! Typed request/response protocol for the node-local cache daemon.
//!
//! Deliberately **not** REST/`format!()`-built URLs: every message is a
//! `serde`-serializable Rust enum. The wire framing is a small
//! length-prefixed envelope (`[len: u32 LE][body: JSON bytes]`) over
//! whatever `AsyncRead`/`AsyncWrite` transport the caller supplies (a
//! `UnixStream` in production, an in-memory duplex pipe in tests) — the
//! same "transport-agnostic frame, JSON/CBOR body" shape `sui-protocol`
//! already established for the graph daemon, kept independent here so
//! this crate has zero dependency on that daemon's graph-specific wire
//! types.
//!
//! JSON (not bincode) was chosen for the body encoding: this crate adds
//! no new workspace dependency (`serde_json` is already pulled in by
//! every consumer of this protocol), the messages are small and
//! infrequent (one per cache lookup, not a hot inner loop), and JSON's
//! self-describing shape makes `sui-dockerfile-node-cache-daemon
//! --version`-style debugging trivial with `nc`/`socat` during
//! incident response.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::DaemonError;

/// Maximum body length a single frame may declare. 8 MiB is generously
/// larger than any `CachedArtifact` this daemon stores (an image
/// reference string), catching a desynchronized or hostile peer before
/// it forces a runaway allocation.
pub const MAX_MESSAGE_BYTES: u32 = 8 * 1024 * 1024;

/// The cache payload: today, an image reference string — the same
/// shape [`sui_cache::storage::StorageBackend::get_narinfo`] already
/// returns for a Dockerfile-graph node hit. Kept as its own typed
/// struct (rather than a bare `String`) so a future phase can widen it
/// (e.g. add a `size_bytes` or `built_at`) without an incompatible wire
/// break at the enum level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedArtifact {
    pub image_ref: String,
}

/// Outcome of a `Warm` request — never blocks the caller on the
/// network fetch; reports only what could be determined synchronously.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WarmStatus {
    /// The content hash was already present in the local L0 store —
    /// nothing to do.
    AlreadyLocal,
    /// Not present locally; a background fetch-and-persist from the
    /// remote tier has been scheduled (fire-and-forget).
    FetchScheduled,
}

/// A request from a client (the daemon-aware wrapper client, or any
/// other same-node caller) to the node cache daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonRequest {
    /// Look up a content hash. Checks the local L0 store first; on a
    /// local miss, the daemon reaches into the remote tier itself and
    /// persists the result locally before responding — the caller
    /// never needs to know which tier actually served it.
    Get { content_hash: String },
    /// Persist an artifact under a content hash in the local L0 store
    /// only (fast local write-back). Callers that also need the
    /// shared remote tier updated (today: `sui-dockerfile-wrapper`'s
    /// cache back-fill on a miss) write to the remote tier themselves
    /// — this daemon does not implicitly forward `Put`s upstream.
    Put { content_hash: String, artifact: CachedArtifact },
    /// Ask the daemon to proactively ensure a content hash is present
    /// locally, without blocking on the fetch. Fire-and-forget;
    /// callers that need to know the artifact is ready should follow
    /// up with a `Get`.
    Warm { content_hash: String },
}

/// A response from the node cache daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonResponse {
    Get { artifact: Option<CachedArtifact> },
    Put { ok: bool },
    Warm { status: WarmStatus },
    /// The daemon itself failed to service the request (e.g. the
    /// remote tier errored on a local-miss fetch). Distinct from a
    /// clean `Get { artifact: None }`, which just means "not cached
    /// anywhere" — this means "couldn't even determine that".
    Error { message: String },
}

/// Write one length-prefixed JSON message to `w`.
///
/// # Errors
///
/// Returns [`DaemonError::Io`] on a transport failure or
/// [`DaemonError::Protocol`] if the message serializes to a body
/// larger than [`MAX_MESSAGE_BYTES`].
pub async fn write_message<W, T>(w: &mut W, message: &T) -> Result<(), DaemonError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(message).map_err(|e| DaemonError::Protocol(e.to_string()))?;
    let len: u32 = body
        .len()
        .try_into()
        .map_err(|_| DaemonError::Protocol("message body too large to frame".to_string()))?;
    if len > MAX_MESSAGE_BYTES {
        return Err(DaemonError::Protocol(format!(
            "message body {len} bytes exceeds cap {MAX_MESSAGE_BYTES}"
        )));
    }
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one length-prefixed JSON message from `r`.
///
/// # Errors
///
/// Returns [`DaemonError::Io`] on a transport failure (including a
/// clean EOF before any bytes arrive) or [`DaemonError::Protocol`] if
/// the declared length exceeds [`MAX_MESSAGE_BYTES`] or the body fails
/// to deserialize.
pub async fn read_message<R, T>(r: &mut R) -> Result<T, DaemonError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_MESSAGE_BYTES {
        return Err(DaemonError::Protocol(format!(
            "declared message length {len} exceeds cap {MAX_MESSAGE_BYTES}"
        )));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(|e| DaemonError::Protocol(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn request_round_trips_over_a_duplex_pipe() {
        let (mut a, mut b) = duplex(4096);
        let req = DaemonRequest::Get { content_hash: "abc123".to_string() };
        write_message(&mut a, &req).await.unwrap();
        let got: DaemonRequest = read_message(&mut b).await.unwrap();
        assert_eq!(got, req);
    }

    #[tokio::test]
    async fn response_round_trips_over_a_duplex_pipe() {
        let (mut a, mut b) = duplex(4096);
        let resp = DaemonResponse::Get {
            artifact: Some(CachedArtifact { image_ref: "example/image:cached".to_string() }),
        };
        write_message(&mut a, &resp).await.unwrap();
        let got: DaemonResponse = read_message(&mut b).await.unwrap();
        assert_eq!(got, resp);
    }

    #[test]
    fn cached_artifact_json_roundtrip() {
        let artifact = CachedArtifact { image_ref: "example/image:v1".to_string() };
        let json = serde_json::to_string(&artifact).unwrap();
        let parsed: CachedArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, artifact);
    }
}
