//! Typed border for the build sandbox.
//!
//! cppnix's sandbox isolates each build from the host filesystem +
//! network.  The shape varies by platform: Linux uses mount/user
//! namespaces + seccomp; macOS uses `sandbox-exec` profiles;
//! cppnix without sandbox support runs builds with only chroot.
//!
//! Today sui-build/src/sandbox.rs implements per-platform sandboxes;
//! this module names the typed contract so policy + capabilities
//! are explicit + spec-driven.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defsandbox-spec
//!   :name "cppnix-linux-strict"
//!   :platform Linux
//!   :isolation-tier Strict
//!   :allowed-paths ("/nix/store" "/tmp/<build-id>" "/dev/null")
//!   :network-allowed false
//!   :seccomp-profile "deny-network-syscalls"
//!   :user-namespacing true)
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border ───────────────────────────────────────────────────

#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defsandbox-spec")]
pub struct SandboxSpec {
    pub name: String,
    pub platform: SandboxPlatform,
    #[serde(rename = "isolationTier")]
    pub isolation_tier: IsolationTier,
    #[serde(default, rename = "allowedPaths")]
    pub allowed_paths: Vec<String>,
    #[serde(default, rename = "networkAllowed")]
    pub network_allowed: bool,
    #[serde(default, rename = "seccompProfile")]
    pub seccomp_profile: Option<String>,
    #[serde(default, rename = "userNamespacing")]
    pub user_namespacing: bool,
}

/// Target platform for the sandbox.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxPlatform {
    /// mount/user/pid namespaces + seccomp filter.
    Linux,
    /// `sandbox-exec` with a profile.
    Darwin,
    /// No sandbox; build runs with only chroot (legacy, untrusted).
    NoSandbox,
}

/// How strictly the sandbox isolates the build.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationTier {
    /// No network, no host paths, no devices beyond /dev/null.
    /// Bit-for-bit reproducible builds.
    Strict,
    /// Network allowed (for FOD fetches).  Otherwise like Strict.
    Relaxed,
    /// Permissive — only sandboxed at the chroot level, no seccomp.
    /// Used for builds that need host capabilities (Darwin .app).
    Permissive,
    /// No isolation at all.  Used for `:requires :no-sandbox` drvs.
    Off,
}

// ── Canonical spec ─────────────────────────────────────────────────

pub const CANONICAL_SANDBOX_LISP: &str = include_str!("../specs/sandbox.lisp");

/// Compile every authored sandbox spec.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<SandboxSpec>, SpecError> {
    crate::loader::load_all::<SandboxSpec>(CANONICAL_SANDBOX_LISP)
}

/// Return the sandbox spec whose `name` matches.
///
/// # Errors
///
/// Returns an error if the spec fails to parse or `name` is missing.
pub fn load_named(name: &str) -> Result<SandboxSpec, SpecError> {
    load_canonical()?
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| SpecError::Load(format!("no (defsandbox-spec) with :name {name:?}")))
}

// ── M3.0 sandbox policy checker ────────────────────────────────────

/// Check whether a path is allowed under a sandbox spec.  Used by
/// the builder to validate any bind-mount or file-access request.
#[must_use]
pub fn path_allowed(spec: &SandboxSpec, path: &str) -> bool {
    spec.allowed_paths.iter().any(|allowed| {
        path == allowed || path.starts_with(&format!("{allowed}/"))
    })
}

/// Check whether a build derivation's `__noSandbox` /
/// `__sandboxAllowNetwork` setting is compatible with the
/// declared sandbox spec.
///
/// # Errors
///
/// `sandbox-policy-violation` if the derivation requests
/// capabilities the spec doesn't grant.
pub fn check_drv_compat(
    spec: &SandboxSpec,
    requires_network: bool,
    requires_no_sandbox: bool,
) -> Result<(), SpecError> {
    if requires_no_sandbox && spec.isolation_tier != IsolationTier::Off {
        return Err(SpecError::Interp {
            phase: "sandbox-policy-violation".into(),
            message: format!(
                "derivation requires no-sandbox but spec `{}` is tier {:?}",
                spec.name, spec.isolation_tier,
            ),
        });
    }
    if requires_network && !spec.network_allowed {
        return Err(SpecError::Interp {
            phase: "sandbox-policy-violation".into(),
            message: format!(
                "derivation requires network but spec `{}` blocks it",
                spec.name,
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_sandboxes_parse() {
        let specs = load_canonical().expect("canonical sandbox specs must compile");
        assert!(!specs.is_empty());
    }

    #[test]
    fn strict_linux_blocks_network() {
        let s = load_named("cppnix-linux-strict").unwrap();
        assert_eq!(s.platform, SandboxPlatform::Linux);
        assert_eq!(s.isolation_tier, IsolationTier::Strict);
        assert!(!s.network_allowed);
        assert!(s.user_namespacing);
    }

    #[test]
    fn fod_sandbox_allows_network() {
        // Fixed-output derivations need network for fetchurl.
        let s = load_named("cppnix-linux-fod").unwrap();
        assert!(s.network_allowed);
        assert_eq!(s.isolation_tier, IsolationTier::Relaxed);
    }

    #[test]
    fn darwin_sandbox_targets_darwin() {
        let s = load_named("cppnix-darwin-strict").unwrap();
        assert_eq!(s.platform, SandboxPlatform::Darwin);
    }

    // ── M3.0 policy tests ──────────────────────────────────────

    #[test]
    fn path_allowed_matches_exact_and_descendants() {
        let s = load_named("cppnix-linux-strict").unwrap();
        assert!(path_allowed(&s, "/nix/store"));
        assert!(path_allowed(&s, "/nix/store/abc-hello"));
        assert!(!path_allowed(&s, "/etc/hosts"));
        assert!(!path_allowed(&s, "/home/user"));
    }

    #[test]
    fn strict_sandbox_blocks_network_drv() {
        let s = load_named("cppnix-linux-strict").unwrap();
        let err = check_drv_compat(&s, true, false).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "sandbox-policy-violation"),
            _ => panic!("expected policy-violation"),
        }
    }

    #[test]
    fn fod_sandbox_allows_network_drv() {
        let s = load_named("cppnix-linux-fod").unwrap();
        check_drv_compat(&s, true, false).unwrap();
    }

    #[test]
    fn no_sandbox_drv_blocked_by_strict_spec() {
        let s = load_named("cppnix-linux-strict").unwrap();
        let err = check_drv_compat(&s, false, true).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "sandbox-policy-violation"),
            _ => panic!("expected policy-violation"),
        }
    }
}
