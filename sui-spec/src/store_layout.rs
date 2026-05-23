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

// ── M3.0 store-path validator ──────────────────────────────────────

/// Validate that a path conforms to the layout's `path_format`
/// rule + lives under the layout's `store_root`.
///
/// # Errors
///
/// - `store-path-not-rooted` if `path` doesn't start with
///   `store_root + "/"`.
/// - `store-path-bad-format` if the entry name doesn't match
///   `<hash>-<name>` (or the algo-colon variant).
pub fn validate_path(layout: &StoreLayout, path: &str) -> Result<(), SpecError> {
    let prefix = format!("{}/", layout.store_root);
    let Some(entry) = path.strip_prefix(&prefix) else {
        return Err(SpecError::Interp {
            phase: "store-path-not-rooted".into(),
            message: format!(
                "path `{path}` doesn't live under store root `{}`",
                layout.store_root,
            ),
        });
    };
    // The entry may have a trailing /subdir; take only the
    // top-level component.
    let top = entry.split('/').next().unwrap_or(entry);
    match layout.path_format {
        StorePathFormat::HashDashName => {
            // hash is the 32-char nix-base32 prefix; followed by
            // `-` + name.
            let Some(dash) = top.find('-') else {
                return Err(SpecError::Interp {
                    phase: "store-path-bad-format".into(),
                    message: format!(
                        "entry `{top}` missing `-` separator (HashDashName)",
                    ),
                });
            };
            if dash != 32 {
                return Err(SpecError::Interp {
                    phase: "store-path-bad-format".into(),
                    message: format!(
                        "entry `{top}` has hash component of length {dash}, expected 32",
                    ),
                });
            }
            Ok(())
        }
        StorePathFormat::AlgoColonHashDashName => {
            // alg:hash-name
            let Some(colon) = top.find(':') else {
                return Err(SpecError::Interp {
                    phase: "store-path-bad-format".into(),
                    message: format!(
                        "entry `{top}` missing `:` separator (AlgoColonHashDashName)",
                    ),
                });
            };
            let rest = &top[colon + 1..];
            if !rest.contains('-') {
                return Err(SpecError::Interp {
                    phase: "store-path-bad-format".into(),
                    message: format!("entry `{top}` missing `-` after algo prefix"),
                });
            }
            Ok(())
        }
    }
}

/// Compute the absolute path of an auxiliary directory inside the
/// layout's state root.  Returns `None` if the layout doesn't
/// declare the requested purpose.
#[must_use]
pub fn aux_dir_path(layout: &StoreLayout, purpose: AuxDirPurpose) -> Option<String> {
    layout
        .aux_dirs
        .iter()
        .find(|d| d.purpose == purpose)
        .map(|d| format!("{}/{}", layout.state_root, d.name))
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

    // ── M3.0 validator tests ───────────────────────────────────

    fn cppnix() -> StoreLayout {
        load_canonical().unwrap().into_iter()
            .find(|l| l.name == "cppnix").unwrap()
    }

    #[test]
    fn validate_path_accepts_well_formed() {
        let layout = cppnix();
        validate_path(&layout, "/nix/store/0000000000000000000000000000abcd-hello").unwrap();
        validate_path(&layout, "/nix/store/0000000000000000000000000000abcd-hello/bin/hello").unwrap();
    }

    #[test]
    fn validate_path_rejects_unrooted() {
        let layout = cppnix();
        let err = validate_path(&layout, "/tmp/foo").unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "store-path-not-rooted"),
            _ => panic!("expected store-path-not-rooted"),
        }
    }

    #[test]
    fn validate_path_rejects_missing_dash() {
        let layout = cppnix();
        let err = validate_path(&layout, "/nix/store/no_separator_here").unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "store-path-bad-format"),
            _ => panic!("expected store-path-bad-format"),
        }
    }

    #[test]
    fn aux_dir_path_resolves_canonical_purposes() {
        let layout = cppnix();
        assert_eq!(
            aux_dir_path(&layout, AuxDirPurpose::Db).as_deref(),
            Some("/nix/var/nix/db"),
        );
        assert_eq!(
            aux_dir_path(&layout, AuxDirPurpose::GcRoots).as_deref(),
            Some("/nix/var/nix/gcroots"),
        );
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
