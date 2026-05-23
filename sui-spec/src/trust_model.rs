//! Typed border for the nix trust model.
//!
//! Nix has three orthogonal trust axes:
//!
//! 1. **Signature trust**: which public keys are accepted for
//!    narinfo signatures?  `trusted-public-keys` setting.
//! 2. **Substituter trust**: which substituters are accepted for
//!    each user?  `trusted-substituters` setting.
//! 3. **Build trust**: which users can run builds?
//!    `trusted-users`, `allowed-users` settings.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "deftrust-model")]
pub struct TrustModel {
    pub name: String,
    /// Whose narinfo signatures are universally trusted.
    #[serde(rename = "trustedPublicKeys")]
    pub trusted_public_keys: Vec<String>,
    /// Substituter URLs the daemon will pull from regardless of
    /// requesting user.
    #[serde(rename = "trustedSubstituters")]
    pub trusted_substituters: Vec<String>,
    /// Users who can do unrestricted builds.
    #[serde(rename = "trustedUsers")]
    pub trusted_users: Vec<String>,
    /// Posture preset — names the "shape" of the policy.
    pub posture: TrustPosture,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrustPosture {
    /// Open: any user can build, any substituter is trusted.
    /// Single-user nix install.
    Permissive,
    /// Multi-user: only `trusted-users` can build; only
    /// `trusted-substituters` are pulled from.
    MultiUser,
    /// Locked-down: no substituters, builds require root, no
    /// network in build sandbox.  Compliance regimes.
    Sealed,
}

pub const CANONICAL_TRUST_LISP: &str = include_str!("../specs/trust_model.lisp");

/// Compile every authored trust model.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<TrustModel>, SpecError> {
    crate::loader::load_all::<TrustModel>(CANONICAL_TRUST_LISP)
}

/// Return the trust model whose `name` matches.
///
/// # Errors
///
/// Returns an error if the spec fails to parse or `name` is missing.
pub fn load_named(name: &str) -> Result<TrustModel, SpecError> {
    load_canonical()?
        .into_iter()
        .find(|t| t.name == name)
        .ok_or_else(|| SpecError::Load(format!("no (deftrust-model) with :name {name:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_trust_models_parse() {
        let models = load_canonical().unwrap();
        assert!(!models.is_empty());
    }

    #[test]
    fn three_postures_present() {
        let models = load_canonical().unwrap();
        let postures: std::collections::HashSet<TrustPosture> =
            models.iter().map(|m| m.posture).collect();
        for required in [TrustPosture::Permissive, TrustPosture::MultiUser, TrustPosture::Sealed] {
            assert!(postures.contains(&required), "missing posture {required:?}");
        }
    }

    #[test]
    fn sealed_has_no_substituters() {
        let m = load_named("sealed-compliance").unwrap();
        assert!(
            m.trusted_substituters.is_empty(),
            "Sealed posture must have no trusted substituters",
        );
    }
}
