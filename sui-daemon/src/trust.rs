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
}
