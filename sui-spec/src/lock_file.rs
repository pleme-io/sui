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

// ── M3.0 lock-file parser ──────────────────────────────────────────

/// Parsed flake.lock — typed envelope.  Individual node entries
/// stay as `serde_json::Value` for M3.0 since their shape varies
/// across versions; M3.1 will lift each variant to a typed struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLockFile {
    pub version: u32,
    pub root: String,
    pub nodes: serde_json::Map<String, serde_json::Value>,
}

/// Parse a `flake.lock` JSON payload against a format spec.
///
/// # Errors
///
/// - `lockfile-parse` for malformed JSON.
/// - `lockfile-version-mismatch` if the file's `version` field
///   doesn't match the spec's declared version.
/// - `lockfile-missing-required` if a Required field is absent.
pub fn parse(text: &str, format: &LockFileFormat) -> Result<ParsedLockFile, SpecError> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| SpecError::Interp {
        phase: "lockfile-parse".into(),
        message: format!("malformed JSON: {e}"),
    })?;
    let obj = value.as_object().ok_or_else(|| SpecError::Interp {
        phase: "lockfile-parse".into(),
        message: "top-level value is not a JSON object".into(),
    })?;

    // Required-field check per the format spec.
    for field in &format.required_fields {
        if !obj.contains_key(field) {
            return Err(SpecError::Interp {
                phase: "lockfile-missing-required".into(),
                message: format!(
                    "flake.lock missing required field `{field}` per format `{}`",
                    format.name,
                ),
            });
        }
    }

    let version = obj
        .get("version")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .ok_or_else(|| SpecError::Interp {
            phase: "lockfile-parse".into(),
            message: "`version` field is missing or not an integer".into(),
        })?;

    if version != format.version {
        return Err(SpecError::Interp {
            phase: "lockfile-version-mismatch".into(),
            message: format!(
                "flake.lock declares version {version}, format `{}` expects {}",
                format.name, format.version,
            ),
        });
    }

    let root = obj
        .get("root")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| SpecError::Interp {
            phase: "lockfile-parse".into(),
            message: "`root` field is missing or not a string".into(),
        })?;

    let nodes = obj
        .get("nodes")
        .and_then(|v| v.as_object())
        .cloned()
        .ok_or_else(|| SpecError::Interp {
            phase: "lockfile-parse".into(),
            message: "`nodes` field is missing or not an object".into(),
        })?;

    Ok(ParsedLockFile { version, root, nodes })
}

/// Names of every input that's a direct edge from `root` in the
/// node graph.  Helpful for `nix flake metadata`-style introspection
/// without reading the whole flake.
///
/// # Errors
///
/// Returns `lockfile-no-root-node` if the lockfile's declared
/// `root` doesn't appear in the `nodes` map.
pub fn root_inputs(parsed: &ParsedLockFile) -> Result<Vec<String>, SpecError> {
    let root_node = parsed.nodes.get(&parsed.root).ok_or_else(|| SpecError::Interp {
        phase: "lockfile-no-root-node".into(),
        message: format!(
            "lockfile declares root `{}` but no node by that name exists",
            parsed.root,
        ),
    })?;
    let inputs = match root_node.get("inputs") {
        Some(serde_json::Value::Object(o)) => o,
        _ => return Ok(Vec::new()),  // root with no inputs is valid
    };
    Ok(inputs.keys().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_lock_file_parses() {
        let formats = load_canonical().unwrap();
        assert!(!formats.is_empty());
    }

    // ── M3.0 parser tests ──────────────────────────────────────

    const CANONICAL_LOCK_FILE: &str = r#"
    {
      "version": 7,
      "root": "root",
      "nodes": {
        "root": {
          "inputs": { "nixpkgs": "nixpkgs", "tatara": "tatara" }
        },
        "nixpkgs": {
          "locked": {
            "narHash": "sha256:abc",
            "rev": "deadbeef",
            "type": "github"
          },
          "original": { "owner": "NixOS", "repo": "nixpkgs", "type": "github" }
        },
        "tatara": {
          "locked": { "narHash": "sha256:def", "rev": "cafef00d", "type": "github" }
        }
      }
    }"#;

    fn fmt() -> LockFileFormat {
        load_canonical().unwrap().into_iter()
            .find(|f| f.name == "cppnix-flake-lock-v7").unwrap()
    }

    #[test]
    fn parse_canonical_lock_file() {
        let parsed = parse(CANONICAL_LOCK_FILE, &fmt()).unwrap();
        assert_eq!(parsed.version, 7);
        assert_eq!(parsed.root, "root");
        assert_eq!(parsed.nodes.len(), 3);
        assert!(parsed.nodes.contains_key("nixpkgs"));
    }

    #[test]
    fn root_inputs_returns_direct_edges() {
        let parsed = parse(CANONICAL_LOCK_FILE, &fmt()).unwrap();
        let inputs = root_inputs(&parsed).unwrap();
        assert!(inputs.iter().any(|s| s == "nixpkgs"));
        assert!(inputs.iter().any(|s| s == "tatara"));
        assert_eq!(inputs.len(), 2);
    }

    #[test]
    fn version_mismatch_errors() {
        let v5 = r#"{ "version": 5, "root": "root", "nodes": {} }"#;
        let err = parse(v5, &fmt()).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "lockfile-version-mismatch"),
            _ => panic!("expected version-mismatch"),
        }
    }

    #[test]
    fn malformed_json_errors() {
        let bad = "not json at all";
        let err = parse(bad, &fmt()).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "lockfile-parse"),
            _ => panic!("expected lockfile-parse"),
        }
    }

    #[test]
    fn missing_required_field_errors() {
        let no_root = r#"{ "version": 7, "nodes": {} }"#;
        let err = parse(no_root, &fmt()).unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "lockfile-missing-required");
                assert!(message.contains("root"));
            }
            _ => panic!("expected missing-required"),
        }
    }

    #[test]
    fn missing_root_node_errors() {
        let dangling = r#"{
            "version": 7,
            "root": "nonexistent",
            "nodes": { "other": {} }
        }"#;
        let parsed = parse(dangling, &fmt()).unwrap();
        let err = root_inputs(&parsed).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "lockfile-no-root-node"),
            _ => panic!("expected no-root-node"),
        }
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
