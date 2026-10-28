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

pub trait UserDb {
    fn user_name(&self, uid: u32) -> Option<String>;
    fn group_name(&self, gid: u32) -> Option<String>;
    fn group_members(&self, group: &str) -> Option<Vec<String>>;
}

pub struct SystemUserDb;

impl UserDb for SystemUserDb {
    fn user_name(&self, uid: u32) -> Option<String> {
        let pw = unsafe { libc::getpwuid(uid) };
        if pw.is_null() {
            return None;
        }
        Some(unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) }.to_string_lossy().into_owned())
    }

    fn group_name(&self, gid: u32) -> Option<String> {
        let gr = unsafe { libc::getgrgid(gid) };
        if gr.is_null() {
            return None;
        }
        Some(unsafe { std::ffi::CStr::from_ptr((*gr).gr_name) }.to_string_lossy().into_owned())
    }

    fn group_members(&self, group: &str) -> Option<Vec<String>> {
        let name = std::ffi::CString::new(group).ok()?;
        let gr = unsafe { libc::getgrnam(name.as_ptr()) };
        if gr.is_null() {
            return None;
        }
        let mut out = Vec::new();
        let mut mem = unsafe { (*gr).gr_mem };
        while !mem.is_null() && !unsafe { *mem }.is_null() {
            out.push(unsafe { std::ffi::CStr::from_ptr(*mem) }.to_string_lossy().into_owned());
            mem = unsafe { mem.add(1) };
        }
        Some(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPolicy {
    pub trusted_users: Vec<String>,
    pub allowed_users: Vec<String>,
    pub build_users_group: Option<String>,
}

impl Default for UserPolicy {
    fn default() -> Self {
        Self {
            trusted_users: vec!["root".to_string()],
            allowed_users: vec!["*".to_string()],
            build_users_group: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("user '{0}' is not allowed to connect to the Nix daemon")]
pub struct NotAllowed(pub String);

impl UserPolicy {
    #[must_use]
    pub fn from_nix_config(config: &sui_compat::nix_conf::NixConfig) -> Self {
        let mut policy = Self::default();
        if let Some(v) = config.strings("trusted-users") {
            policy.trusted_users = v;
        }
        if let Some(v) = config.strings("allowed-users") {
            policy.allowed_users = v;
        }
        policy.build_users_group = config.get("build-users-group").map(str::trim).filter(|g| !g.is_empty()).map(String::from);
        policy
    }

    pub fn admit(&self, peer_uid: u32, peer_gid: u32, creds: &dyn PeerCredentials, db: &dyn UserDb) -> Result<TrustLevel, NotAllowed> {
        let user = db.user_name(peer_uid).unwrap_or_else(|| peer_uid.to_string());
        let group = db.group_name(peer_gid).unwrap_or_else(|| peer_gid.to_string());
        let trusted = TrustLevel::from_uid(peer_uid, creds) == TrustLevel::Trusted
            || match_user(&user, &group, &self.trusted_users, db);
        let in_build_group = self.build_users_group.as_deref() == Some(group.as_str());
        if (!trusted && !match_user(&user, &group, &self.allowed_users, db)) || (in_build_group && peer_uid != creds.current_uid()) {
            return Err(NotAllowed(user));
        }
        Ok(if trusted { TrustLevel::Trusted } else { TrustLevel::NotTrusted })
    }
}

fn match_user(user: &str, group: &str, users: &[String], db: &dyn UserDb) -> bool {
    if users.iter().any(|u| u == "*" || u == user) {
        return true;
    }
    users.iter().filter_map(|u| u.strip_prefix('@')).any(|g| {
        g == group || db.group_members(g).is_some_and(|m| m.iter().any(|x| x == user))
    })
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    struct Creds(u32);
    impl PeerCredentials for Creds {
        fn current_uid(&self) -> u32 {
            self.0
        }
    }

    struct Db;
    impl UserDb for Db {
        fn user_name(&self, uid: u32) -> Option<String> {
            match uid {
                0 => Some("root".into()),
                501 => Some("drzzln".into()),
                502 => Some("guest".into()),
                503 => Some("ops".into()),
                300 => Some("_nixbld1".into()),
                _ => None,
            }
        }
        fn group_name(&self, gid: u32) -> Option<String> {
            match gid {
                0 => Some("wheel".into()),
                20 => Some("staff".into()),
                30000 => Some("nixbld".into()),
                _ => None,
            }
        }
        fn group_members(&self, group: &str) -> Option<Vec<String>> {
            match group {
                "admin" => Some(vec!["root".into(), "ops".into()]),
                _ => None,
            }
        }
    }

    fn policy(trusted: &[&str], allowed: &[&str]) -> UserPolicy {
        UserPolicy {
            trusted_users: trusted.iter().map(|s| (*s).to_string()).collect(),
            allowed_users: allowed.iter().map(|s| (*s).to_string()).collect(),
            build_users_group: Some("nixbld".into()),
        }
    }

    #[test]
    fn a_named_trusted_user_is_trusted_by_a_root_daemon() {
        let p = policy(&["root", "drzzln"], &["*"]);
        assert_eq!(p.admit(501, 20, &Creds(0), &Db), Ok(TrustLevel::Trusted));
    }

    #[test]
    fn a_group_member_is_trusted_through_at_group() {
        let p = policy(&["root", "@admin"], &["*"]);
        assert_eq!(p.admit(503, 20, &Creds(0), &Db), Ok(TrustLevel::Trusted));
        assert_eq!(p.admit(502, 20, &Creds(0), &Db), Ok(TrustLevel::NotTrusted));
    }

    #[test]
    fn a_primary_group_matches_at_group() {
        let p = policy(&["@staff"], &[]);
        assert_eq!(p.admit(502, 20, &Creds(0), &Db), Ok(TrustLevel::Trusted));
    }

    #[test]
    fn a_user_neither_trusted_nor_allowed_is_refused_by_name() {
        let p = policy(&["root"], &["drzzln"]);
        assert_eq!(p.admit(502, 20, &Creds(0), &Db), Err(NotAllowed("guest".into())));
        assert_eq!(p.admit(501, 20, &Creds(0), &Db), Ok(TrustLevel::NotTrusted));
    }

    #[test]
    fn the_build_users_group_is_refused() {
        let p = policy(&["root"], &["*"]);
        assert!(p.admit(300, 30000, &Creds(0), &Db).is_err());
    }

    #[test]
    fn the_daemon_user_stays_trusted() {
        let p = policy(&[], &[]);
        assert_eq!(p.admit(501, 20, &Creds(501), &Db), Ok(TrustLevel::Trusted));
    }

    #[test]
    fn the_policy_reads_nix_conf_lists() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("nix.conf");
        std::fs::write(&conf, "trusted-users = root @admin\nextra-trusted-users = drzzln\nallowed-users = *\nbuild-users-group = nixbld\n").unwrap();
        let cfg = sui_compat::nix_conf::NixConfig::load_from(&sui_compat::nix_conf::ConfigSources {
            system: conf,
            user: Vec::new(),
            nix_config: None,
        })
        .unwrap();
        let p = UserPolicy::from_nix_config(&cfg);
        assert_eq!(p.trusted_users, ["root", "@admin", "drzzln"]);
        assert_eq!(p.allowed_users, ["*"]);
        assert_eq!(p.build_users_group.as_deref(), Some("nixbld"));
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
