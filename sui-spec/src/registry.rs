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

// ── M3.0 registry resolver ─────────────────────────────────────────

/// One registry entry — a `from` → `to` mapping with optional
/// `exact` flag (cppnix lockfile semantics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    pub from: String,
    pub to: String,
    pub exact: bool,
}

/// Resolved registry entries grouped by scope, in precedence
/// order (lowest first wins).
pub type Registries = Vec<(RegistryScope, Vec<RegistryEntry>)>;

/// Resolve a flake reference through the precedence chain.
/// Returns the FIRST match (lowest-precedence scope wins per
/// cppnix convention).
///
/// # Errors
///
/// `registry-unresolved` if no scope has a matching entry.
pub fn resolve(
    registries: &Registries,
    flake_ref: &str,
) -> Result<RegistryEntry, SpecError> {
    let mut sorted: Vec<&(RegistryScope, Vec<RegistryEntry>)> =
        registries.iter().collect();
    sorted.sort_by_key(|(scope, _)| scope_precedence(*scope));
    for (_, entries) in sorted {
        for entry in entries {
            if entry.from == flake_ref {
                return Ok(entry.clone());
            }
        }
    }
    Err(SpecError::Interp {
        phase: "registry-unresolved".into(),
        message: format!("no registry entry for `{flake_ref}` across any scope"),
    })
}

fn scope_precedence(scope: RegistryScope) -> u32 {
    match scope {
        RegistryScope::FlakeLocal => 0,
        RegistryScope::User       => 1,
        RegistryScope::System     => 2,
        RegistryScope::Global     => 3,
    }
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

    // ── M3.0 resolver tests ────────────────────────────────────

    fn entry(from: &str, to: &str) -> RegistryEntry {
        RegistryEntry { from: from.into(), to: to.into(), exact: false }
    }

    #[test]
    fn resolve_finds_lowest_precedence_match() {
        let registries: Registries = vec![
            (RegistryScope::Global, vec![entry("nixpkgs", "github:NixOS/nixpkgs/global")]),
            (RegistryScope::User,   vec![entry("nixpkgs", "github:NixOS/nixpkgs/user")]),
            (RegistryScope::FlakeLocal, vec![entry("nixpkgs", "github:NixOS/nixpkgs/local")]),
        ];
        let resolved = resolve(&registries, "nixpkgs").unwrap();
        assert_eq!(resolved.to, "github:NixOS/nixpkgs/local");
    }

    #[test]
    fn resolve_falls_through_to_system_when_local_absent() {
        let registries: Registries = vec![
            (RegistryScope::User, vec![entry("home-manager", "github:nix-community/home-manager")]),
            (RegistryScope::System, vec![entry("home-manager", "github:nix-community/system-hm")]),
        ];
        let resolved = resolve(&registries, "home-manager").unwrap();
        assert_eq!(resolved.to, "github:nix-community/home-manager");
    }

    #[test]
    fn resolve_errors_on_unknown_input() {
        let registries: Registries = vec![
            (RegistryScope::Global, vec![entry("known", "github:x/y")]),
        ];
        let err = resolve(&registries, "unknown").unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "registry-unresolved"),
            _ => panic!("expected registry-unresolved"),
        }
    }
}
