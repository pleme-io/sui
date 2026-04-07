//! Trust level determination from Unix peer credentials.

/// Trust level for a connected client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub fn from_uid(peer_uid: u32, creds: &dyn PeerCredentials) -> Self {
        let my_uid = creds.current_uid();
        if peer_uid == 0 || peer_uid == my_uid {
            Self::Trusted
        } else {
            Self::NotTrusted
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
}
