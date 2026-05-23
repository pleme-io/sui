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

/// Parsed store path components.  cppnix store paths decompose
/// into `<hash>-<name>` (HashDashName) or `<algo>:<hash>-<name>`
/// (AlgoColonHashDashName), optionally followed by `/subpath`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedStorePath {
    /// Optional algorithm prefix (`sha256` etc.) — only set when
    /// the layout's path_format is AlgoColonHashDashName.
    pub algorithm: Option<String>,
    /// The hash component (cppnix: 32-char nix-base32).
    pub hash: String,
    /// The name component (everything after the hash separator
    /// up to a possible `/subpath`).
    pub name: String,
    /// Optional sub-path beneath the top-level store entry.
    /// `/nix/store/<hash>-<name>/bin/hello` → `bin/hello`.
    pub sub_path: Option<String>,
}

/// Decompose a store path into its typed components.
///
/// Sibling of [`validate_path`] — `validate_path` returns Ok/Err;
/// `parse_path` returns the actual structure so operators and
/// downstream tooling can inspect it.
///
/// # Errors
///
/// Returns the same typed errors `validate_path` does
/// (`store-path-not-rooted`, `store-path-bad-format`).
pub fn parse_path(layout: &StoreLayout, path: &str) -> Result<ParsedStorePath, SpecError> {
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

    // Split top-level entry from optional sub-path.
    let (top, sub_path) = match entry.split_once('/') {
        Some((t, rest)) => (t, Some(rest.to_string())),
        None => (entry, None),
    };

    match layout.path_format {
        StorePathFormat::HashDashName => {
            // <hash>-<name>
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
            Ok(ParsedStorePath {
                algorithm: None,
                hash: top[..dash].to_string(),
                name: top[dash + 1..].to_string(),
                sub_path,
            })
        }
        StorePathFormat::AlgoColonHashDashName => {
            // <algo>:<hash>-<name>
            let Some(colon) = top.find(':') else {
                return Err(SpecError::Interp {
                    phase: "store-path-bad-format".into(),
                    message: format!(
                        "entry `{top}` missing `:` separator (AlgoColonHashDashName)",
                    ),
                });
            };
            let (algo, rest) = top.split_at(colon);
            let rest = &rest[1..]; // skip ':'
            let Some(dash) = rest.find('-') else {
                return Err(SpecError::Interp {
                    phase: "store-path-bad-format".into(),
                    message: format!("entry `{top}` missing `-` after algo prefix"),
                });
            };
            Ok(ParsedStorePath {
                algorithm: Some(algo.to_string()),
                hash: rest[..dash].to_string(),
                name: rest[dash + 1..].to_string(),
                sub_path,
            })
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

    // ── parse_path tests ────────────────────────────────────────

    #[test]
    fn parse_path_decomposes_hash_dash_name() {
        let layout = cppnix();
        let parsed = parse_path(
            &layout,
            "/nix/store/0000000000000000000000000000abcd-hello-2.12",
        ).unwrap();
        assert_eq!(parsed.algorithm, None);
        assert_eq!(parsed.hash, "0000000000000000000000000000abcd");
        assert_eq!(parsed.name, "hello-2.12");
        assert_eq!(parsed.sub_path, None);
    }

    #[test]
    fn parse_path_captures_sub_path() {
        let layout = cppnix();
        let parsed = parse_path(
            &layout,
            "/nix/store/0000000000000000000000000000abcd-hello/bin/hello",
        ).unwrap();
        assert_eq!(parsed.sub_path.as_deref(), Some("bin/hello"));
    }

    #[test]
    fn parse_path_errors_for_unrooted() {
        let layout = cppnix();
        let err = parse_path(&layout, "/tmp/not-in-store").unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => {
                assert_eq!(phase, "store-path-not-rooted");
            }
            _ => panic!("expected Interp error"),
        }
    }

    #[test]
    fn parse_path_errors_for_short_hash() {
        let layout = cppnix();
        let err = parse_path(&layout, "/nix/store/short-hello").unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => {
                assert_eq!(phase, "store-path-bad-format");
            }
            _ => panic!("expected Interp error"),
        }
    }

    #[test]
    fn parse_path_errors_when_dash_missing() {
        let layout = cppnix();
        // 32 chars, no dash.
        let err = parse_path(
            &layout,
            "/nix/store/00000000000000000000000000000000",
        ).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => {
                assert_eq!(phase, "store-path-bad-format");
            }
            _ => panic!("expected Interp error"),
        }
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
