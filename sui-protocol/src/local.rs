//! Local protocol — daemon ↔ same-host CLI (and atticd).
//!
//! Hot path. Sub-millisecond budget for warm-cache lookups. rkyv 0.8
//! over a length-prefixed multiplex frame, transported via
//! `tokio::net::UnixStream`. The transport itself lives in the future
//! sui-daemon crate; this module ships the typed wire shapes those
//! transport layers will consume.
//!
//! ## Wire shape
//!
//! Every byte on the wire belongs to a [`WireFrame`]. The framing is:
//!
//! ```text
//! [magic : 4B = "SUI1"] [length : 4B u32 LE] [body : N bytes of rkyv-archived WireFrame]
//! ```
//!
//! The magic + length prefix is what lets a misaligned reader fail
//! fast (the rkyv access itself validates the body, so a corrupted
//! body never gets cast to a typed reference). Length includes only
//! the body — not the 8 prefix bytes. Max body length is bounded by
//! daemon configuration; defaults to 64 MiB to comfortably hold a
//! batched closure-info request without runaway.
//!
//! ## Request / response model
//!
//! Bidirectional multiplexed: each frame carries a [`RequestId`] (u64)
//! so multiple in-flight requests share one stream without
//! head-of-line blocking. Heartbeats are first-class frames so a long
//! idle connection doesn't NAT-time-out. The daemon may push
//! unsolicited events (eval-cache invalidations) under `request_id =
//! 0`, which clients route to a subscription handler.

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};

use sui_graph_store::GraphHash;

/// Magic bytes at the start of every frame on the local protocol.
/// Bumping requires a `MAX_LOCAL_PROTOCOL_VERSION` bump too.
pub const FRAME_MAGIC: [u8; 4] = *b"SUI1";

/// Multiplex correlation id. `0` is reserved for unsolicited server
/// pushes (cache invalidations, progress events).
pub type RequestId = u64;

/// One frame on the wire. All inter-process communication on the
/// local link is one or more of these.
#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    PartialEq,
)]
#[rkyv(derive(Debug))]
pub enum WireFrame {
    /// Cheap keepalive. Either side sends; receiver echoes the same
    /// nonce back as another `Heartbeat`. Failure to receive the echo
    /// within N seconds (caller-configurable) terminates the link.
    Heartbeat(Heartbeat),
    /// CLI → daemon request.
    Request {
        id: RequestId,
        body: LocalRequest,
    },
    /// Daemon → CLI response (correlates to a prior `Request.id`).
    Response {
        id: RequestId,
        body: LocalResponse,
    },
    /// Daemon → CLI event push. `id` is always `0`.
    Event { topic: String, payload: Vec<u8> },
    /// Graceful shutdown signal. Either side may send; the receiver
    /// drains pending responses then closes the connection.
    Goodbye,
}

#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
)]
#[rkyv(derive(Debug))]
pub struct Heartbeat {
    pub nonce: u64,
    /// Unix nanoseconds (u64). Wraps in year 2554 — well past every
    /// fleet's useful lifetime.
    pub sent_unix_nanos: u64,
}

/// Every request the local protocol can carry today. Append new
/// variants at the bottom — rkyv tag stability rule.
#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    PartialEq,
)]
#[rkyv(derive(Debug))]
pub enum LocalRequest {
    /// Probe — daemon returns build identity + uptime. Used for
    /// `sui daemon status`.
    Ping,
    /// Ask the daemon for the rkyv-archive bytes of a graph stored
    /// under `(kind_tag, hash)`. The daemon mmaps from
    /// sui-graph-store and forwards. Zero-copy on the daemon side; one
    /// memcpy on the CLI side (over the UDS socket).
    GetGraph {
        kind_tag: u8,
        hash: GraphHash,
    },
    /// Tell the daemon to ingest a graph — used by tend prebuild and
    /// any future producer. Daemon validates `BLAKE3(bytes) == hash`
    /// and persists via sui-graph-store.
    PutGraph {
        kind_tag: u8,
        hash: GraphHash,
        bytes: Vec<u8>,
    },
    /// Ask the daemon for a snapshot of operational counters
    /// (hot-cache size, hits, misses, last-warm timestamp).
    /// `sui daemon stats` rides on this.
    GetStats,
}

/// Every response the local protocol can carry today.
#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    PartialEq,
)]
#[rkyv(derive(Debug))]
pub enum LocalResponse {
    Pong {
        build_id: [u8; 32],
        uptime_seconds: u64,
    },
    /// Graph bytes (rkyv archive, ready to mmap-cast).
    GraphBytes(Vec<u8>),
    /// Confirmation of a successful `PutGraph`.
    GraphStored {
        hash: GraphHash,
    },
    /// Error returned for any failing request. Carries a typed code
    /// + free-form message; CLI surfaces both.
    Error(LocalError),
    /// Operational counters snapshot.
    Stats(StatsSnapshot),
}

/// Daemon-side errors. Codes are stable IDs; messages are operator-
/// readable strings (English; not localized).
#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
)]
#[rkyv(derive(Debug))]
pub struct LocalError {
    pub code: ErrorCode,
    pub message: String,
}

/// Stable error codes. Append-only.
#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
)]
#[rkyv(derive(Debug))]
pub enum ErrorCode {
    GraphNotFound,
    GraphHashMismatch,
    InvalidGraphKind,
    StoreUnavailable,
    Internal,
}

#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
)]
#[rkyv(derive(Debug))]
pub struct StatsSnapshot {
    pub hot_cache_entries: u64,
    pub hot_cache_bytes: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub puts: u64,
    pub uptime_seconds: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn ping_pong_roundtrips() {
        let frame = WireFrame::Request {
            id: 42,
            body: LocalRequest::Ping,
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&frame).unwrap();
        let archived = rkyv::access::<ArchivedWireFrame, rkyv::rancor::Error>(&bytes).unwrap();
        // The Archived form mirrors the layout; assert structurally
        // by matching the variant.
        match archived {
            ArchivedWireFrame::Request { id, body } => {
                assert_eq!(id.to_native(), 42);
                assert!(matches!(body, ArchivedLocalRequest::Ping));
            }
            _ => panic!("expected Request"),
        }
    }

    #[test]
    fn get_graph_request_carries_kind_and_hash() {
        let h = GraphHash::of(b"sample");
        let frame = WireFrame::Request {
            id: 1,
            body: LocalRequest::GetGraph {
                kind_tag: 1,
                hash: h,
            },
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&frame).unwrap();
        let archived = rkyv::access::<ArchivedWireFrame, rkyv::rancor::Error>(&bytes).unwrap();
        match archived {
            ArchivedWireFrame::Request { body, .. } => match body {
                ArchivedLocalRequest::GetGraph { kind_tag, hash } => {
                    assert_eq!(*kind_tag, 1);
                    assert_eq!(hash.0, h.0);
                }
                _ => panic!("expected GetGraph"),
            },
            _ => panic!("expected Request"),
        }
    }

    #[test]
    fn error_response_carries_code() {
        let frame = WireFrame::Response {
            id: 7,
            body: LocalResponse::Error(LocalError {
                code: ErrorCode::GraphNotFound,
                message: "blob missing".into(),
            }),
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&frame).unwrap();
        let archived = rkyv::access::<ArchivedWireFrame, rkyv::rancor::Error>(&bytes).unwrap();
        match archived {
            ArchivedWireFrame::Response { body, .. } => match body {
                ArchivedLocalResponse::Error(err) => {
                    assert!(matches!(err.code, ArchivedErrorCode::GraphNotFound));
                    assert_eq!(err.message.as_str(), "blob missing");
                }
                _ => panic!("expected Error response"),
            },
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn frame_magic_is_fixed() {
        assert_eq!(&FRAME_MAGIC, b"SUI1");
    }
}
