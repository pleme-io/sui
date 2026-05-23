//! Typed border for the `/nix/store` directory layout.
//!
//! cppnix's store has a strict on-disk convention: hash component +
//! `-` + sanitised name, plus a small set of auxiliary directories
//! (`/nix/var/nix/{db,gcroots,profiles,daemon-socket,...}`).  This
//! module names the layout as a typed Lisp spec so future
//! store-implementations (sui-store today, alternate backends
//! eventually) ride on the same contract.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border ───────────────────────────────────────────────────

#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defstore-layout")]
pub struct StoreLayout {
    pub name: String,
    #[serde(rename = "storeRoot")]
    pub store_root: String,
    #[serde(rename = "stateRoot")]
    pub state_root: String,
    /// Auxiliary directories under `state_root` that must exist
    /// for nix-compat operations.
    #[serde(default)]
    pub aux_dirs: Vec<AuxDir>,
    /// Path-naming rule for entries directly under `store_root`.
    #[serde(rename = "pathFormat")]
    pub path_format: StorePathFormat,
}

/// One auxiliary directory under the store's state root.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AuxDir {
    pub name: String,
    pub purpose: AuxDirPurpose,
}

/// What each auxiliary directory holds.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuxDirPurpose {
    /// SQLite database of store metadata.
    Db,
    /// Symlinks holding paths alive through GC.
    GcRoots,
    /// Per-user profile generation symlinks.
    Profiles,
    /// Unix socket for the nix-daemon.
    DaemonSocket,
    /// Temporary build directories.
    Temp,
    /// Per-user shared state (lock files, etc.).
    UserState,
    /// Eval cache directory.
    EvalCache,
}

/// Path-naming rule for entries directly under the store root.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorePathFormat {
    /// `<hash>-<name>` where hash is 32-char nix-base32.  cppnix.
    HashDashName,
    /// `<algo>:<hash>-<name>` — multi-algo variant (CA-drv variants
    /// may eventually need this).
    AlgoColonHashDashName,
}

// ── Canonical spec ─────────────────────────────────────────────────

pub const CANONICAL_STORE_LAYOUT_LISP: &str =
    include_str!("../specs/store_layout.lisp");

/// Compile every authored store layout.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<StoreLayout>, SpecError> {
    crate::loader::load_all::<StoreLayout>(CANONICAL_STORE_LAYOUT_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_store_layouts_parse() {
        let layouts = load_canonical().expect("canonical store layouts must compile");
        assert!(!layouts.is_empty());
    }

    #[test]
    fn cppnix_layout_has_canonical_paths() {
        let layouts = load_canonical().unwrap();
        let cppnix = layouts
            .iter()
            .find(|l| l.name == "cppnix")
            .expect("cppnix layout must exist");
        assert_eq!(cppnix.store_root, "/nix/store");
        assert_eq!(cppnix.state_root, "/nix/var/nix");
        assert_eq!(cppnix.path_format, StorePathFormat::HashDashName);
    }

    #[test]
    fn cppnix_layout_includes_essential_auxdirs() {
        let layouts = load_canonical().unwrap();
        let cppnix = layouts.iter().find(|l| l.name == "cppnix").unwrap();
        let purposes: std::collections::HashSet<AuxDirPurpose> =
            cppnix.aux_dirs.iter().map(|d| d.purpose).collect();
        for required in [
            AuxDirPurpose::Db,
            AuxDirPurpose::GcRoots,
            AuxDirPurpose::Profiles,
            AuxDirPurpose::DaemonSocket,
        ] {
            assert!(
                purposes.contains(&required),
                "cppnix layout missing {required:?} aux dir",
            );
        }
    }
}
