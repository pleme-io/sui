//! The blob-upload session as a typestate FSM.
//!
//! An OCI blob upload is a three-beat protocol:
//!
//! ```text
//!   POST /v2/<name>/blobs/uploads/     → Started        (end-4a)
//!   PATCH <location> (Content-Range)   → Started (grown) (end-5)
//!   PUT   <location>?digest=<d>        → Finalized       (end-6)
//! ```
//!
//! The illegal states this module makes unrepresentable / typed-rejected:
//!
//! - **Out-of-order chunk.** A `PATCH` whose `Content-Range` start ≠ the
//!   session's current length is a typed [`UploadError::OutOfOrder`], never a
//!   silent append. (The offset is a value the FSM owns; a client cannot write
//!   past a gap.)
//! - **Wrong finalize digest.** `finalize` computes the digest of the
//!   accumulated bytes and compares it to the client's claim; a mismatch is a
//!   typed [`UploadError::DigestMismatch`] → `DIGEST_INVALID`, never a stored
//!   blob under the wrong address.
//!
//! The session accumulates bytes in RAM for M0. DESTINATION: stream chunks
//! straight into the content-addressed store keyed by a provisional id, so a
//! multi-GB layer never buffers whole — a named seam, not a hidden limit.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::digest::Digest;

/// Widen a byte length to `u64` without a lossy cast (a length never truncates,
/// but this keeps the pedantic cast lint satisfied on every target).
#[inline]
fn len_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A unique upload-session identifier (opaque to the client; it lives in the
/// `Location` header as `/v2/<name>/blobs/uploads/<id>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UploadId(String);

impl UploadId {
    /// The id string as it appears in the URL.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UploadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A typed upload failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UploadError {
    /// The session id is unknown (never started, or already finalized/cancelled).
    #[error("unknown upload session: {0}")]
    UnknownSession(String),
    /// A chunk's `Content-Range` start did not equal the session's current
    /// length — an out-of-order or gapped write.
    #[error("out-of-order chunk: session at {have} bytes, chunk starts at {got}")]
    OutOfOrder {
        /// The session's current accumulated length.
        have: u64,
        /// The chunk's declared start offset.
        got: u64,
    },
    /// The finalize digest did not match the digest of the accumulated bytes.
    #[error("digest mismatch on finalize: computed {computed}, client claimed {claimed}")]
    DigestMismatch {
        /// The digest porto computed over the received bytes.
        computed: String,
        /// The digest the client asserted in `?digest=`.
        claimed: String,
    },
}

/// One in-flight upload session's accumulated state.
///
/// This is deliberately *not* `pub` field-wise — the only way to grow it is
/// [`UploadSessions::patch`], which enforces the offset invariant. The bytes
/// cannot be appended out of order because no other code path can reach them.
struct Session {
    /// The repository this upload targets.
    name: String,
    /// The bytes received so far.
    buffer: Vec<u8>,
}

/// The set of in-flight upload sessions, keyed by [`UploadId`].
///
/// A single shared, `Mutex`-guarded map; each op is a small critical section.
/// DESTINATION: a sui-store-backed session table so a pod restart can resume an
/// upload (MAGMA-NATIVE never-lose-on-restart) — an honest interim, not hidden.
#[derive(Default)]
pub struct UploadSessions {
    sessions: Mutex<HashMap<String, Session>>,
    counter: Mutex<u64>,
}

impl UploadSessions {
    /// A fresh empty session registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new upload session for `name`, returning its id.
    ///
    /// The id is a monotonic counter rendered as a stable opaque token. It is
    /// unique per process; a durable session store (the destination) would key
    /// by a content-addressed / uuid token instead.
    #[must_use]
    pub fn start(&self, name: &str) -> UploadId {
        let id = {
            let mut c = self.counter.lock().unwrap_or_else(|e| e.into_inner());
            *c += 1;
            format!("{}-{}", name.replace('/', "_"), *c)
        };
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.insert(
            id.clone(),
            Session {
                name: name.to_string(),
                buffer: Vec::new(),
            },
        );
        UploadId(id)
    }

    /// The current accumulated length of a session (its `Range` upper bound).
    ///
    /// # Errors
    ///
    /// [`UploadError::UnknownSession`] if the id is not live.
    pub fn offset(&self, id: &str) -> Result<u64, UploadError> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .get(id)
            .map(|s| len_u64(s.buffer.len()))
            .ok_or_else(|| UploadError::UnknownSession(id.to_string()))
    }

    /// Append a chunk. `start` is the chunk's declared `Content-Range` start
    /// (or the current offset for a rangeless `PATCH`); it MUST equal the
    /// session's current length or the write is rejected out-of-order.
    ///
    /// Returns the new accumulated length.
    ///
    /// # Errors
    ///
    /// - [`UploadError::UnknownSession`] if the id is not live.
    /// - [`UploadError::OutOfOrder`] if `start` ≠ the current length.
    pub fn patch(&self, id: &str, start: u64, chunk: &[u8]) -> Result<u64, UploadError> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| UploadError::UnknownSession(id.to_string()))?;
        let have = len_u64(session.buffer.len());
        if start != have {
            return Err(UploadError::OutOfOrder { have, got: start });
        }
        session.buffer.extend_from_slice(chunk);
        Ok(len_u64(session.buffer.len()))
    }

    /// Finalize the session: optionally append a trailing chunk (the
    /// monolithic-`PUT`-with-body case), then verify the accumulated bytes'
    /// digest equals `claimed`. On success the session is consumed and its
    /// `(name, digest, bytes)` returned for the caller to store; the session id
    /// is no longer live.
    ///
    /// # Errors
    ///
    /// - [`UploadError::UnknownSession`] if the id is not live.
    /// - [`UploadError::DigestMismatch`] if the computed digest ≠ `claimed`.
    pub fn finalize(
        &self,
        id: &str,
        trailing: Option<&[u8]>,
        claimed: &Digest,
    ) -> Result<FinalizedUpload, UploadError> {
        let mut session = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions
                .remove(id)
                .ok_or_else(|| UploadError::UnknownSession(id.to_string()))?
        };
        if let Some(tail) = trailing {
            session.buffer.extend_from_slice(tail);
        }
        // Verify against the CLIENT'S claimed algorithm — a sha512 claim is
        // hashed under sha512, a sha256 claim under sha256.
        let computed = Digest::of(claimed.algorithm(), &session.buffer);
        if &computed != claimed {
            // The session is already removed; a failed finalize aborts it (the
            // client must restart). This is the fail-loud posture: a mismatch
            // never stores a blob.
            return Err(UploadError::DigestMismatch {
                computed: computed.to_wire(),
                claimed: claimed.to_wire(),
            });
        }
        Ok(FinalizedUpload {
            name: session.name,
            digest: computed,
            bytes: session.buffer,
        })
    }

    /// Cancel and drop a session (end-14 `DELETE`).
    ///
    /// # Errors
    ///
    /// [`UploadError::UnknownSession`] if the id is not live.
    pub fn cancel(&self, id: &str) -> Result<(), UploadError> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| UploadError::UnknownSession(id.to_string()))
    }
}

/// The verified result of a finalized upload: the bytes are proven to hash to
/// `digest` (the caller stores them at that address).
#[derive(Debug, Clone)]
pub struct FinalizedUpload {
    /// The repository the upload targeted.
    pub name: String,
    /// The verified content-address of the bytes.
    pub digest: Digest,
    /// The complete blob bytes.
    pub bytes: Vec<u8>,
}

/// Parse a `Content-Range: <start>-<end>` header into `(start, end)`.
///
/// OCI chunk ranges are `start-end` (inclusive end). Returns `None` on any
/// malformed shape (the caller maps that to `BLOB_UPLOAD_INVALID`).
#[must_use]
pub fn parse_content_range(raw: &str) -> Option<(u64, u64)> {
    let (start, end) = raw.split_once('-')?;
    Some((start.trim().parse().ok()?, end.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_compat::hash::HashAlgorithm;

    #[test]
    fn ordered_chunks_accumulate() {
        let sessions = UploadSessions::new();
        let id = sessions.start("lib/x");
        assert_eq!(sessions.offset(id.as_str()).unwrap(), 0);
        assert_eq!(sessions.patch(id.as_str(), 0, b"hello ").unwrap(), 6);
        assert_eq!(sessions.patch(id.as_str(), 6, b"world").unwrap(), 11);
        let d = Digest::of(HashAlgorithm::Sha256, b"hello world");
        let fin = sessions.finalize(id.as_str(), None, &d).unwrap();
        assert_eq!(fin.bytes, b"hello world");
        assert_eq!(fin.digest, d);
    }

    #[test]
    fn out_of_order_chunk_is_typed_rejection() {
        let sessions = UploadSessions::new();
        let id = sessions.start("lib/x");
        sessions.patch(id.as_str(), 0, b"abc").unwrap();
        // Next chunk claims to start at 5, but session is at 3 → OutOfOrder.
        let err = sessions.patch(id.as_str(), 5, b"xyz").unwrap_err();
        assert_eq!(err, UploadError::OutOfOrder { have: 3, got: 5 });
    }

    #[test]
    fn wrong_finalize_digest_is_typed_rejection() {
        let sessions = UploadSessions::new();
        let id = sessions.start("lib/x");
        sessions.patch(id.as_str(), 0, b"actual bytes").unwrap();
        let wrong = Digest::of(HashAlgorithm::Sha256, b"different bytes");
        let err = sessions.finalize(id.as_str(), None, &wrong).unwrap_err();
        assert!(matches!(err, UploadError::DigestMismatch { .. }));
        // And the session was consumed — a retry is UnknownSession.
        assert!(matches!(
            sessions.offset(id.as_str()),
            Err(UploadError::UnknownSession(_))
        ));
    }

    #[test]
    fn monolithic_finalize_with_trailing_body() {
        let sessions = UploadSessions::new();
        let id = sessions.start("lib/x");
        // No PATCH — the whole body arrives on the finalize PUT.
        let d = Digest::of(HashAlgorithm::Sha256, b"whole blob");
        let fin = sessions.finalize(id.as_str(), Some(b"whole blob"), &d).unwrap();
        assert_eq!(fin.bytes, b"whole blob");
    }

    #[test]
    fn unknown_session_ops_are_typed() {
        let sessions = UploadSessions::new();
        assert!(matches!(
            sessions.offset("nope"),
            Err(UploadError::UnknownSession(_))
        ));
        assert!(matches!(
            sessions.cancel("nope"),
            Err(UploadError::UnknownSession(_))
        ));
    }

    #[test]
    fn content_range_parses() {
        assert_eq!(parse_content_range("0-1023"), Some((0, 1023)));
        assert_eq!(parse_content_range("1024-2047"), Some((1024, 2047)));
        assert_eq!(parse_content_range("bad"), None);
        assert_eq!(parse_content_range("0-"), None);
    }
}
