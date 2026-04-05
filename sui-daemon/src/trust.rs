//! Trust level determination from Unix peer credentials.

/// Trust level for a connected client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    /// Client runs as root or the daemon user — allowed to perform all operations.
    Trusted,
    /// Client is an unprivileged user — restricted operations.
    NotTrusted,
}

impl TrustLevel {
    /// Determine trust level from the peer's effective UID.
    ///
    /// UID 0 (root) or the daemon's own UID are considered trusted.
    pub fn from_uid(peer_uid: u32) -> Self {
        let my_uid = unsafe { libc::getuid() };
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

    #[test]
    fn root_is_trusted() {
        assert_eq!(TrustLevel::from_uid(0), TrustLevel::Trusted);
    }

    #[test]
    fn own_uid_is_trusted() {
        let my_uid = unsafe { libc::getuid() };
        assert_eq!(TrustLevel::from_uid(my_uid), TrustLevel::Trusted);
    }

    #[test]
    fn display_trust_levels() {
        assert_eq!(TrustLevel::Trusted.to_string(), "trusted");
        assert_eq!(TrustLevel::NotTrusted.to_string(), "not-trusted");
    }
}
