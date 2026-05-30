//! Version-negotiation handshake.
//!
//! Every connection opens with both sides exchanging a
//! [`VersionHandshake`] — their max-supported version + their min-
//! supported version (the floor of their backwards-compat window).
//! Each side then takes `min(my_max, their_max)` as the negotiated
//! version. If that's below either side's `min`, the connection is
//! refused with a typed error.
//!
//! Discipline: every breaking change to a wire type bumps
//! [`MAX_LOCAL_PROTOCOL_VERSION`] and (if it's been long enough since
//! the last bump) [`MIN_LOCAL_PROTOCOL_VERSION`]. The window in
//! between is the support contract.

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use rkyv::rancor;

/// Maximum local-protocol version this build can speak.
///
/// Bump this when a breaking change lands. Bumping past
/// [`MIN_LOCAL_PROTOCOL_VERSION`] sunsets older daemons; do that on a
/// known cadence (quarterly is the current target).
pub const MAX_LOCAL_PROTOCOL_VERSION: u16 = 1;

/// Minimum local-protocol version this build understands. Older peers
/// negotiating below this floor are refused.
///
/// Keep within the support window of `MAX_LOCAL_PROTOCOL_VERSION` —
/// initially they're equal (v1 only); bump `MIN` only when an older
/// version is genuinely sunset.
pub const MIN_LOCAL_PROTOCOL_VERSION: u16 = 1;

/// First-frame body each side sends. Carries the peer's supported
/// version window so the other side can compute the negotiated
/// version (or refuse).
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
pub struct VersionHandshake {
    /// Max protocol version this peer can speak.
    pub max_version: u16,
    /// Min protocol version this peer understands.
    pub min_version: u16,
    /// Stable build identity — operators read this in `sui-daemon
    /// status` output. Free-form; never load-bearing for negotiation.
    pub build_id: [u8; 32],
}

/// Outcome of a successful negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedVersion {
    pub version: u16,
}

impl VersionHandshake {
    /// Construct the local side of the handshake using this build's
    /// compile-time version window.
    #[must_use]
    pub fn local(build_id: [u8; 32]) -> Self {
        Self {
            max_version: MAX_LOCAL_PROTOCOL_VERSION,
            min_version: MIN_LOCAL_PROTOCOL_VERSION,
            build_id,
        }
    }

    /// Compute the negotiated version against a peer's handshake.
    ///
    /// # Errors
    ///
    /// Returns `None` when the two windows don't overlap (peer too
    /// old to talk to us, or we're too old to talk to them).
    #[must_use]
    pub fn negotiate(&self, peer: &VersionHandshake) -> Option<NegotiatedVersion> {
        let candidate = self.max_version.min(peer.max_version);
        if candidate >= self.min_version && candidate >= peer.min_version {
            Some(NegotiatedVersion { version: candidate })
        } else {
            None
        }
    }

    /// Validate-and-cast helper for callers that just received the
    /// archived handshake from the wire. Wraps the rkyv access
    /// machinery so call sites don't reinvent it.
    ///
    /// # Errors
    ///
    /// Propagates the rkyv validation error verbatim — typically a
    /// length/alignment/tag mismatch means the peer isn't speaking
    /// our protocol at all.
    pub fn access(bytes: &[u8]) -> Result<&ArchivedVersionHandshake, rancor::Error> {
        rkyv::access::<ArchivedVersionHandshake, rancor::Error>(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(id: u8) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = id;
        a
    }

    #[test]
    fn equal_windows_pick_the_max() {
        let a = VersionHandshake::local(build(1));
        let b = VersionHandshake::local(build(2));
        let neg = a.negotiate(&b).unwrap();
        assert_eq!(neg.version, MAX_LOCAL_PROTOCOL_VERSION);
    }

    #[test]
    fn overlapping_window_picks_the_intersection_max() {
        let a = VersionHandshake {
            max_version: 5,
            min_version: 3,
            build_id: build(1),
        };
        let b = VersionHandshake {
            max_version: 4,
            min_version: 2,
            build_id: build(2),
        };
        let neg = a.negotiate(&b).unwrap();
        assert_eq!(neg.version, 4);
    }

    #[test]
    fn disjoint_windows_refuse() {
        let old = VersionHandshake {
            max_version: 2,
            min_version: 1,
            build_id: build(1),
        };
        let modern = VersionHandshake {
            max_version: 5,
            min_version: 4,
            build_id: build(2),
        };
        assert!(old.negotiate(&modern).is_none());
        assert!(modern.negotiate(&old).is_none());
    }

    #[test]
    fn handshake_roundtrips_via_rkyv() {
        let h = VersionHandshake::local(build(7));
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&h).unwrap();
        let arc = VersionHandshake::access(&bytes).unwrap();
        assert_eq!(arc.max_version, h.max_version);
        assert_eq!(arc.min_version, h.min_version);
        assert_eq!(arc.build_id, h.build_id);
    }
}
