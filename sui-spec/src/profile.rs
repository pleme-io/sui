//! Typed border for nix-env / nix profile generations.
//!
//! A profile is `/nix/var/nix/profiles/<name>` — a symlink to the
//! current generation, with sibling `<name>-<N>-link` symlinks for
//! historical generations.  `nix-env -i`, `nix profile install`,
//! `home-manager activate` all maintain generations.  This module
//! names the structure.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defprofile-format")]
pub struct ProfileFormat {
    pub name: String,
    pub kind: ProfileKind,
    /// Pattern for generation-N symlinks (cppnix uses
    /// `<profile>-<N>-link`).
    #[serde(rename = "generationLinkPattern")]
    pub generation_link_pattern: String,
    /// Pattern for the manifest file (cppnix profile.* nests a
    /// JSON manifest after Nix 2.4).
    #[serde(rename = "manifestPath")]
    pub manifest_path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfileKind {
    /// `/nix/var/nix/profiles/<name>` — system-level profile (root
    /// profile, system profile).
    System,
    /// `~/.nix-profile/` (legacy) or
    /// `~/.local/state/nix/profiles/profile/` (per-user 2.4+).
    User,
    /// nix-shell / nix develop ephemeral profile.  Cleaned at exit.
    Ephemeral,
}

pub const CANONICAL_PROFILE_LISP: &str = include_str!("../specs/profile.lisp");

/// Compile every authored profile format.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<ProfileFormat>, SpecError> {
    crate::loader::load_all::<ProfileFormat>(CANONICAL_PROFILE_LISP)
}

// ── M3.0 generation-link helpers ───────────────────────────────────

/// Compute the generation-link path for a profile + generation
/// number, substituting the format's `<profile>` and `<N>`
/// placeholders.
#[must_use]
pub fn generation_link(format: &ProfileFormat, profile: &str, n: u32) -> String {
    format
        .generation_link_pattern
        .replace("<profile>", profile)
        .replace("<N>", &n.to_string())
}

/// Compute the next generation link given the current generation
/// number.  Convenience wrapper around [`generation_link`].
#[must_use]
pub fn next_generation(format: &ProfileFormat, profile: &str, current: u32) -> String {
    generation_link(format, profile, current + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_profile_parses() {
        let formats = load_canonical().unwrap();
        assert!(!formats.is_empty());
    }

    // ── M3.0 generation-link tests ─────────────────────────────

    fn system_fmt() -> ProfileFormat {
        load_canonical().unwrap().into_iter()
            .find(|f| f.name == "cppnix-system-profile").unwrap()
    }

    #[test]
    fn generation_link_substitutes_placeholders() {
        let f = system_fmt();
        let link = generation_link(&f, "system", 42);
        assert_eq!(link, "system-42-link");
    }

    #[test]
    fn next_generation_increments() {
        let f = system_fmt();
        let next = next_generation(&f, "system", 41);
        assert_eq!(next, "system-42-link");
    }

    #[test]
    fn three_profile_kinds_present() {
        let formats = load_canonical().unwrap();
        let kinds: std::collections::HashSet<ProfileKind> =
            formats.iter().map(|f| f.kind).collect();
        for required in [ProfileKind::System, ProfileKind::User, ProfileKind::Ephemeral] {
            assert!(kinds.contains(&required), "missing profile kind {required:?}");
        }
    }
}
