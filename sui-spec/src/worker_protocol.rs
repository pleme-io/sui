//! Typed border for the nix-daemon worker protocol.
//!
//! When a nix client (`nix build`, `nix-env`, `nix-store --realise`)
//! needs to mutate the store, it doesn't write `/nix/store` itself
//! — it connects to the `nix-daemon` over a unix socket and speaks
//! the *worker protocol*.  cppnix's `libstore/worker-protocol.cc`
//! defines ~30 opcodes covering the full read+write surface of the
//! store.
//!
//! Today sui-daemon implements this protocol in Rust; this module
//! names the wire contract as a typed Lisp spec so any future client
//! (or any third-party daemon) rides on the same authored shape.
//! Both engines (sui's client side + sui-daemon's server side) drive
//! the same spec — they cannot drift.
//!
//! ## Authoring surface
//!
//! Two keyword forms compose:
//!
//! - `(defworker-protocol ...)` declares the protocol version +
//!   handshake (magic bytes, version negotiation).
//! - `(defworker-opcode ...)` declares ONE opcode per form: numeric
//!   code, direction, request-field types, response-field types,
//!   feature gate.  ~30 opcodes today; new ones land as additional
//!   `(defworker-opcode)` forms.
//!
//! Example:
//!
//! ```lisp
//! (defworker-protocol
//!   :name "cppnix-worker-protocol"
//!   :version 35
//!   :magic-client "0x6e697863"   ;; "nixc"
//!   :magic-server "0x6478696f")  ;; "dxio"
//!
//! (defworker-opcode
//!   :name "QueryPathInfo"
//!   :code 29
//!   :direction ClientToDaemon
//!   :request-fields  (StorePath)
//!   :response-fields (PathInfo)
//!   :since-version 17)
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border — protocol envelope ──────────────────────────────

/// One worker-protocol version.  Typically there's one entry per
/// major nix release (v23, v25, v27, v33, v35).
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defworker-protocol")]
pub struct WorkerProtocol {
    pub name: String,
    pub version: u32,
    /// Client→daemon handshake magic, hex literal.
    #[serde(rename = "magicClient")]
    pub magic_client: String,
    /// Daemon→client handshake magic, hex literal.
    #[serde(rename = "magicServer")]
    pub magic_server: String,
}

// ── Typed border — opcodes ─────────────────────────────────────────

/// One worker-protocol opcode.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defworker-opcode")]
pub struct WorkerOpcode {
    pub name: String,
    pub code: u32,
    pub direction: OpcodeDirection,
    /// Field types serialised in the request body (in order).
    #[serde(rename = "requestFields")]
    pub request_fields: Vec<WireType>,
    /// Field types serialised in the response body (in order).
    #[serde(default, rename = "responseFields")]
    pub response_fields: Vec<WireType>,
    /// Earliest protocol version that supports this opcode.
    #[serde(default, rename = "sinceVersion")]
    pub since_version: u32,
    /// `true` if cppnix has deprecated this opcode.
    #[serde(default)]
    pub deprecated: bool,
}

/// Direction of an opcode call.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpcodeDirection {
    /// Client initiates; daemon responds.
    ClientToDaemon,
    /// Daemon initiates a callback (rare; used for build progress).
    DaemonToClient,
}

/// Wire-level field type — the primitive types the worker protocol
/// can serialise.  Length-prefixed strings + 8-byte LE integers
/// per cppnix's `worker-protocol.cc` `WORKER_MAGIC` framing.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireType {
    /// 8-byte little-endian unsigned integer.
    U64,
    /// Length-prefixed UTF-8 string (8-byte LE length + padded bytes).
    Str,
    /// Length-prefixed byte array.
    Bytes,
    /// 0 = false, 1 = true; encoded as U64.
    Bool,
    /// Store path — Str semantically, but the spec calls it out
    /// so the type checker can validate path-shape.
    StorePath,
    /// List of strings (8-byte LE count + N Str entries).
    StringList,
    /// List of store paths (8-byte LE count + N StorePath).
    StorePathList,
    /// `PathInfo` structured response (multi-field).
    PathInfo,
    /// `ValidPathInfo` (a richer PathInfo variant).
    ValidPathInfo,
    /// `DerivationOutputs` map (output-name → output-path).
    DerivationOutputs,
    /// `RealisationsMap`.
    RealisationsMap,
    /// `KeyedBuildResult` (status enum + log + per-output info).
    KeyedBuildResult,
    /// `Substitutables` — the substituter-side path-info map.
    Substitutables,
    /// Free-form attrset of key=value lines.
    KeyValueAttrs,
    /// Build-mode enum (Normal / Repair / Check).
    BuildMode,
}

// ── Canonical spec ─────────────────────────────────────────────────

pub const CANONICAL_WORKER_PROTOCOL_LISP: &str =
    include_str!("../specs/worker_protocol.lisp");

/// Compile the canonical worker-protocol version envelope(s).
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical_protocols() -> Result<Vec<WorkerProtocol>, SpecError> {
    crate::loader::load_all::<WorkerProtocol>(CANONICAL_WORKER_PROTOCOL_LISP)
}

/// Compile every authored opcode.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical_opcodes() -> Result<Vec<WorkerOpcode>, SpecError> {
    crate::loader::load_all::<WorkerOpcode>(CANONICAL_WORKER_PROTOCOL_LISP)
}

/// Return the opcode whose `name` matches.
///
/// # Errors
///
/// Returns an error if the spec fails to parse or `name` is missing.
pub fn load_opcode_named(name: &str) -> Result<WorkerOpcode, SpecError> {
    load_canonical_opcodes()?
        .into_iter()
        .find(|o| o.name == name)
        .ok_or_else(|| SpecError::Load(format!("no (defworker-opcode) with :name {name:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn canonical_protocol_parses() {
        let protos = load_canonical_protocols().unwrap();
        assert!(!protos.is_empty());
    }

    #[test]
    fn canonical_opcodes_parse() {
        let opcodes = load_canonical_opcodes().unwrap();
        assert!(
            opcodes.len() >= 25,
            "canonical worker protocol must declare at least 25 opcodes, got {}",
            opcodes.len(),
        );
    }

    #[test]
    fn opcodes_have_unique_codes() {
        let opcodes = load_canonical_opcodes().unwrap();
        let mut by_code: HashMap<u32, Vec<&str>> = HashMap::new();
        for op in &opcodes {
            by_code.entry(op.code).or_default().push(op.name.as_str());
        }
        for (code, names) in &by_code {
            assert_eq!(
                names.len(),
                1,
                "opcode {code} has multiple authored entries: {names:?}",
            );
        }
    }

    #[test]
    fn essential_opcodes_present() {
        let opcodes = load_canonical_opcodes().unwrap();
        let names: HashSet<&str> = opcodes.iter().map(|o| o.name.as_str()).collect();
        // The opcodes every client speaks at least once per session.
        for required in [
            "IsValidPath",
            "QueryPathInfo",
            "AddToStore",
            "BuildPaths",
            "BuildDerivation",
            "QueryReferrers",
            "QueryValidPaths",
            "CollectGarbage",
            "NarFromPath",
            "AddTempRoot",
        ] {
            assert!(
                names.contains(required),
                "canonical worker protocol missing opcode `{required}`",
            );
        }
    }

    #[test]
    fn query_path_info_has_correct_shape() {
        let op = load_opcode_named("QueryPathInfo").unwrap();
        assert_eq!(op.code, 26);
        assert_eq!(op.direction, OpcodeDirection::ClientToDaemon);
        assert_eq!(op.request_fields, vec![WireType::StorePath]);
    }

    #[test]
    fn ca_realisation_opcodes_present() {
        // CA-drv realisation lookup landed in protocol v32+.
        let names: HashSet<String> = load_canonical_opcodes()
            .unwrap()
            .into_iter()
            .map(|o| o.name)
            .collect();
        assert!(names.contains("QueryRealisation"));
        assert!(names.contains("RegisterDrvOutput"));
    }
}
