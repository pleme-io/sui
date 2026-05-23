//! Typed border for the flake registry — the lookup that resolves
//! short input refs (`nixpkgs`, `github:owner/repo`) to concrete
//! `from` → `to` mappings.
//!
//! Cppnix has three registries (system, user, flake-local), with
//! precedence order.  Each is a JSON file with `version` + `flakes`
//! (list of entries: `from`, `to`, `exact`).

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defregistry-format")]
pub struct RegistryFormat {
    pub name: String,
    pub version: u32,
    pub scope: RegistryScope,
    /// Precedence rank — lower = checked first.  cppnix:
    /// flake-local 0, user 1, system 2.
    pub precedence: u32,
    /// Path the registry conventionally lives at.
    #[serde(rename = "defaultPath")]
    pub default_path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegistryScope {
    /// Per-flake `nixConfig.flake-registry` entries.
    FlakeLocal,
    /// `~/.config/nix/registry.json`.
    User,
    /// `/etc/nix/registry.json`.
    System,
    /// The upstream flake registry from `flake-registry` setting
    /// (typically `https://channels.nixos.org/flake-registry.json`).
    Global,
}

pub const CANONICAL_REGISTRY_LISP: &str =
    include_str!("../specs/registry.lisp");

/// Compile every authored registry format.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<RegistryFormat>, SpecError> {
    crate::loader::load_all::<RegistryFormat>(CANONICAL_REGISTRY_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_registry_parses() {
        let formats = load_canonical().unwrap();
        assert!(!formats.is_empty());
    }

    #[test]
    fn all_four_scopes_present() {
        let formats = load_canonical().unwrap();
        let scopes: std::collections::HashSet<RegistryScope> =
            formats.iter().map(|f| f.scope).collect();
        for required in [
            RegistryScope::FlakeLocal,
            RegistryScope::User,
            RegistryScope::System,
            RegistryScope::Global,
        ] {
            assert!(
                scopes.contains(&required),
                "missing registry scope {required:?}",
            );
        }
    }

    #[test]
    fn precedence_order_matches_cppnix() {
        let formats = load_canonical().unwrap();
        let prec = |s: RegistryScope| -> u32 {
            formats.iter().find(|f| f.scope == s).unwrap().precedence
        };
        // cppnix order: flake-local < user < system < global
        assert!(prec(RegistryScope::FlakeLocal) < prec(RegistryScope::User));
        assert!(prec(RegistryScope::User) < prec(RegistryScope::System));
        assert!(prec(RegistryScope::System) < prec(RegistryScope::Global));
    }
}
