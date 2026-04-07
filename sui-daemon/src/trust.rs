//! Trust level determination from Unix peer credentials.

/// Trust level for a connected client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrustLevel {
    /// Client runs as root or the daemon user — allowed to perform all operations.
    Trusted,
    /// Client is an unprivileged user — restricted operations.
    NotTrusted,
}

/// Abstraction over peer credential retrieval for testability.
pub trait PeerCredentials {
    /// Return the current process's effective UID.
    fn current_uid(&self) -> u32;
}

/// Default [`PeerCredentials`] backed by `libc::getuid()`.
pub struct SystemPeerCredentials;

impl PeerCredentials for SystemPeerCredentials {
    fn current_uid(&self) -> u32 {
        unsafe { libc::getuid() }
    }
}

impl TrustLevel {
    /// Determine trust level from the peer's effective UID.
    ///
    /// UID 0 (root) or the daemon's own UID are considered trusted.
    #[must_use]
    pub fn from_uid(peer_uid: u32, creds: &dyn PeerCredentials) -> Self {
        let my_uid = creds.current_uid();
        if peer_uid == 0 || peer_uid == my_uid {
            Self::Trusted
        } else {
            Self::NotTrusted
        }
    }
}

/// Wire encoding: `Trusted = 1`, `NotTrusted = 2` (matches Nix daemon protocol).
impl From<TrustLevel> for u64 {
    fn from(level: TrustLevel) -> Self {
        match level {
            TrustLevel::Trusted => 1,
            TrustLevel::NotTrusted => 2,
        }
    }
}

impl std::fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trusted => write!(f, "trusted"),
            Self::NotTrusted => write!(f, "not-trusted"),
        }
    }
}

impl std::str::FromStr for TrustLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "trusted" => Ok(Self::Trusted),
            "not-trusted" => Ok(Self::NotTrusted),
            other => Err(format!("unknown trust level: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock credentials provider for testing.
    struct MockCredentials {
        uid: u32,
    }

    impl PeerCredentials for MockCredentials {
        fn current_uid(&self) -> u32 {
            self.uid
        }
    }

    #[test]
    fn root_is_trusted() {
        let creds = MockCredentials { uid: 1000 };
        assert_eq!(TrustLevel::from_uid(0, &creds), TrustLevel::Trusted);
    }

    #[test]
    fn own_uid_is_trusted() {
        let creds = MockCredentials { uid: 501 };
        assert_eq!(TrustLevel::from_uid(501, &creds), TrustLevel::Trusted);
    }

    #[test]
    fn other_uid_is_not_trusted() {
        let creds = MockCredentials { uid: 501 };
        assert_eq!(TrustLevel::from_uid(1000, &creds), TrustLevel::NotTrusted);
    }

    #[test]
    fn system_peer_credentials_works() {
        let creds = SystemPeerCredentials;
        let my_uid = creds.current_uid();
        // Our own UID should make us trusted
        assert_eq!(TrustLevel::from_uid(my_uid, &creds), TrustLevel::Trusted);
    }

    #[test]
    fn display_trust_levels() {
        assert_eq!(TrustLevel::Trusted.to_string(), "trusted");
        assert_eq!(TrustLevel::NotTrusted.to_string(), "not-trusted");
    }

    #[test]
    fn trust_level_clone_and_copy() {
        let t = TrustLevel::Trusted;
        let t2 = t;
        assert_eq!(t, t2);
    }

    #[test]
    fn trust_level_debug_format() {
        assert_eq!(format!("{:?}", TrustLevel::Trusted), "Trusted");
        assert_eq!(format!("{:?}", TrustLevel::NotTrusted), "NotTrusted");
    }

    #[test]
    fn root_uid_always_trusted_regardless_of_daemon_uid() {
        for daemon_uid in [0, 500, 1000, 65534] {
            let creds = MockCredentials { uid: daemon_uid };
            assert_eq!(
                TrustLevel::from_uid(0, &creds),
                TrustLevel::Trusted,
                "root should be trusted when daemon runs as UID {daemon_uid}"
            );
        }
    }

    #[test]
    fn different_non_root_uid_is_not_trusted() {
        for (peer, daemon) in [(1000, 501), (502, 501), (65534, 0)] {
            if peer == 0 {
                continue;
            }
            let creds = MockCredentials { uid: daemon };
            if peer != daemon {
                assert_eq!(
                    TrustLevel::from_uid(peer, &creds),
                    TrustLevel::NotTrusted,
                    "peer {peer} should not be trusted when daemon is {daemon}"
                );
            }
        }
    }

    // ── Wire-encoding round-trip ──────────────────────────────

    #[test]
    fn trust_level_into_u64_trusted() {
        let v: u64 = TrustLevel::Trusted.into();
        assert_eq!(v, 1, "Trusted must wire-encode as 1");
    }

    #[test]
    fn trust_level_into_u64_not_trusted() {
        let v: u64 = TrustLevel::NotTrusted.into();
        assert_eq!(v, 2, "NotTrusted must wire-encode as 2");
    }

    #[test]
    fn trust_level_wire_encoding_unique() {
        // Ensure no collision between variants on the wire.
        let trusted: u64 = TrustLevel::Trusted.into();
        let not_trusted: u64 = TrustLevel::NotTrusted.into();
        assert_ne!(trusted, not_trusted);
        // And neither is zero (zero is not a valid encoded variant).
        assert_ne!(trusted, 0);
        assert_ne!(not_trusted, 0);
    }

    // ── FromStr ────────────────────────────────────────────────

    #[test]
    fn from_str_trusted() {
        let parsed: TrustLevel = "trusted".parse().unwrap();
        assert_eq!(parsed, TrustLevel::Trusted);
    }

    #[test]
    fn from_str_not_trusted() {
        let parsed: TrustLevel = "not-trusted".parse().unwrap();
        assert_eq!(parsed, TrustLevel::NotTrusted);
    }

    #[test]
    fn from_str_unknown_returns_err() {
        let parsed: Result<TrustLevel, _> = "bogus".parse();
        let err = parsed.unwrap_err();
        assert!(err.contains("bogus"), "error should mention the bad value");
    }

    #[test]
    fn from_str_empty_returns_err() {
        let parsed: Result<TrustLevel, _> = "".parse();
        assert!(parsed.is_err());
    }

    #[test]
    fn from_str_case_sensitive() {
        // The FromStr impl is case-sensitive — uppercase should not match.
        let upper: Result<TrustLevel, _> = "TRUSTED".parse();
        assert!(upper.is_err(), "FromStr should be case sensitive");
        let mixed: Result<TrustLevel, _> = "Trusted".parse();
        assert!(mixed.is_err());
    }

    #[test]
    fn display_from_str_round_trip() {
        for level in [TrustLevel::Trusted, TrustLevel::NotTrusted] {
            let s = level.to_string();
            let parsed: TrustLevel = s.parse().unwrap();
            assert_eq!(parsed, level, "round-trip via Display/FromStr");
        }
    }

    // ── Saturation tests for unusual UIDs ──────────────────────

    #[test]
    fn max_uid_not_trusted_unless_matches() {
        let creds = MockCredentials { uid: 1000 };
        // u32::MAX is not root and not the daemon UID -> not trusted.
        assert_eq!(
            TrustLevel::from_uid(u32::MAX, &creds),
            TrustLevel::NotTrusted
        );
    }

    #[test]
    fn max_uid_matching_daemon_is_trusted() {
        let creds = MockCredentials { uid: u32::MAX };
        assert_eq!(
            TrustLevel::from_uid(u32::MAX, &creds),
            TrustLevel::Trusted
        );
    }

    #[test]
    fn root_trusted_when_daemon_is_root() {
        let creds = MockCredentials { uid: 0 };
        assert_eq!(TrustLevel::from_uid(0, &creds), TrustLevel::Trusted);
    }
}
