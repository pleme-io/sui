//! Typed border for `flake.lock` — the on-disk representation of
//! resolved flake inputs.
//!
//! Every flake-using consumer ships a `flake.lock` next to its
//! `flake.nix`.  Cppnix's format is JSON with `version`, `root`,
//! `nodes` (one entry per input, keyed by name) and edges between
//! nodes for transitive resolution.  This module names the format
//! as a typed Lisp spec.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (deflock-file-format
//!   :name "cppnix-flake-lock-v7"
//!   :version 7
//!   :encoding JsonText
//!   :required-fields ("version" "root" "nodes")
//!   :node-fields ("inputs" "locked" "original")
//!   :phases ((:kind ParseJson)
//!            (:kind ValidateVersion)
//!            (:kind ValidateNodeGraph)
//!            (:kind ResolveTransitiveInputs)))
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "deflock-file-format")]
pub struct LockFileFormat {
    pub name: String,
    pub version: u32,
    pub encoding: LockFileEncoding,
    #[serde(rename = "requiredFields")]
    pub required_fields: Vec<String>,
    #[serde(rename = "nodeFields")]
    pub node_fields: Vec<String>,
    pub phases: Vec<LockFilePhase>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockFileEncoding {
    /// Top-level JSON object on disk.  cppnix.
    JsonText,
    /// Hypothetical CBOR/MsgPack variant.
    Binary,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LockFilePhase {
    pub kind: LockFilePhaseKind,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockFilePhaseKind {
    /// Parse the JSON envelope.
    ParseJson,
    /// Verify `version` matches the spec.
    ValidateVersion,
    /// Walk the `nodes` graph: every edge points at a known node,
    /// no cycles except through `root`, every node has a
    /// `locked` field with a narHash (except `root`).
    ValidateNodeGraph,
    /// Compute transitive input closure from `root` outward,
    /// surfacing the same node-set every consumer should see.
    ResolveTransitiveInputs,
    /// Emit the lock file back to canonical JSON form
    /// (round-trip clean property).
    EmitCanonicalJson,
}

pub const CANONICAL_LOCK_FILE_LISP: &str =
    include_str!("../specs/lock_file.lisp");

/// Compile every authored lock-file format.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<LockFileFormat>, SpecError> {
    crate::loader::load_all::<LockFileFormat>(CANONICAL_LOCK_FILE_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_lock_file_parses() {
        let formats = load_canonical().unwrap();
        assert!(!formats.is_empty());
    }

    #[test]
    fn v7_format_has_canonical_fields() {
        let formats = load_canonical().unwrap();
        let v7 = formats
            .iter()
            .find(|f| f.name == "cppnix-flake-lock-v7")
            .unwrap();
        assert_eq!(v7.version, 7);
        for required in ["version", "root", "nodes"] {
            assert!(
                v7.required_fields.iter().any(|f| f == required),
                "v7 missing required field {required}",
            );
        }
        for node_field in ["inputs", "locked", "original"] {
            assert!(
                v7.node_fields.iter().any(|f| f == node_field),
                "v7 missing node-field {node_field}",
            );
        }
    }
}
