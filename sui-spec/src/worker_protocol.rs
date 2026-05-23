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

/// Return the opcode with the given `code`.
///
/// # Errors
///
/// Returns an error if the spec fails to parse or `code` doesn't
/// match any authored opcode.
pub fn load_opcode_by_code(code: u32) -> Result<WorkerOpcode, SpecError> {
    load_canonical_opcodes()?
        .into_iter()
        .find(|o| o.code == code)
        .ok_or_else(|| SpecError::Load(format!("no (defworker-opcode) with :code {code}")))
}

// ── M3.0 dispatch interpreter ──────────────────────────────────────

/// Result of dispatching a worker-protocol opcode against a handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOutcome {
    /// The opcode that was dispatched.
    pub opcode_name: String,
    /// The numeric code.
    pub opcode_code: u32,
    /// Response body bytes the handler produced.  Empty for
    /// opcodes with no `response_fields`.
    pub response_body: Vec<u8>,
}

/// Abstract handler for the worker protocol's server side.  Each
/// opcode dispatches to a corresponding method here; the default
/// implementation returns a typed `unhandled-opcode` error so an
/// incomplete handler surfaces missing opcodes loudly.
///
/// In production, sui-daemon implements this trait against its
/// real store.  Tests can provide a mock that records dispatched
/// opcodes for verification.
pub trait WorkerProtocolHandler {
    /// Catch-all handler.  Default returns `unhandled-opcode`
    /// error.  Concrete impls override per opcode they support.
    ///
    /// # Errors
    ///
    /// Default impl always.  Concrete handlers return per-opcode
    /// errors.
    fn dispatch(&self, opcode: &WorkerOpcode, body: &[u8])
        -> Result<Vec<u8>, String>
    {
        Err(format!(
            "unhandled opcode `{}` (code {}) — handler must override dispatch \
             or implement an explicit case",
            opcode.name, opcode.code,
        ))
    }
}

/// Dispatch one wire-arriving opcode against a handler.  Looks up
/// the opcode by code in the canonical catalog, then routes to
/// `handler.dispatch`.
///
/// # Errors
///
/// - `unknown-opcode` if `code` doesn't appear in the canonical
///   opcode set.
/// - `dispatch-failed` if the handler returns an error.
pub fn apply<H: WorkerProtocolHandler>(
    code: u32,
    body: &[u8],
    handler: &H,
) -> Result<DispatchOutcome, SpecError> {
    let opcode = load_opcode_by_code(code).map_err(|_| SpecError::Interp {
        phase: "unknown-opcode".into(),
        message: format!(
            "received opcode {code} but no (defworker-opcode :code {code}) \
             is authored in the canonical worker-protocol spec",
        ),
    })?;
    let response_body = handler.dispatch(&opcode, body).map_err(|e| SpecError::Interp {
        phase: "dispatch-failed".into(),
        message: format!("opcode `{}` (code {code}): {e}", opcode.name),
    })?;
    Ok(DispatchOutcome {
        opcode_name: opcode.name,
        opcode_code: code,
        response_body,
    })
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

    // ── M3.0 dispatch tests ────────────────────────────────────

    use std::cell::RefCell;

    struct LoggingHandler {
        seen: RefCell<Vec<(String, u32)>>,
        response_for: HashMap<u32, Vec<u8>>,
    }

    impl LoggingHandler {
        fn new() -> Self {
            Self {
                seen: RefCell::new(Vec::new()),
                response_for: HashMap::new(),
            }
        }
        fn responds(mut self, code: u32, body: &[u8]) -> Self {
            self.response_for.insert(code, body.to_vec());
            self
        }
    }

    impl WorkerProtocolHandler for LoggingHandler {
        fn dispatch(&self, opcode: &WorkerOpcode, _body: &[u8])
            -> Result<Vec<u8>, String>
        {
            self.seen.borrow_mut().push((opcode.name.clone(), opcode.code));
            self.response_for
                .get(&opcode.code)
                .cloned()
                .ok_or_else(|| format!("no response configured for code {}", opcode.code))
        }
    }

    #[test]
    fn dispatch_known_opcode_succeeds() {
        let handler = LoggingHandler::new().responds(1, b"\x01");  // IsValidPath
        let outcome = apply(1, b"", &handler).unwrap();
        assert_eq!(outcome.opcode_name, "IsValidPath");
        assert_eq!(outcome.opcode_code, 1);
        assert_eq!(outcome.response_body, vec![1u8]);
        let log = handler.seen.borrow();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, "IsValidPath");
    }

    #[test]
    fn dispatch_unknown_opcode_errors() {
        let handler = LoggingHandler::new();
        let err = apply(9999, b"", &handler).unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "unknown-opcode");
                assert!(message.contains("9999"));
            }
            _ => panic!("expected unknown-opcode"),
        }
    }

    #[test]
    fn dispatch_handler_failure_surfaces_as_dispatch_failed() {
        // LoggingHandler returns Err when no response is configured.
        let handler = LoggingHandler::new();  // no responses
        let err = apply(1, b"", &handler).unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "dispatch-failed");
                assert!(message.contains("IsValidPath"));
            }
            _ => panic!("expected dispatch-failed"),
        }
    }

    #[test]
    fn load_opcode_by_code_finds_known() {
        let op = load_opcode_by_code(26).unwrap();  // QueryPathInfo
        assert_eq!(op.name, "QueryPathInfo");
    }

    #[test]
    fn load_opcode_by_code_errors_on_missing() {
        let err = load_opcode_by_code(99999).unwrap_err();
        match err {
            SpecError::Load(msg) => assert!(msg.contains("99999")),
            _ => panic!("expected Load error"),
        }
    }

    #[test]
    fn default_handler_returns_unhandled() {
        struct Empty;
        impl WorkerProtocolHandler for Empty {}
        let err = apply(1, b"", &Empty).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "dispatch-failed"),
            _ => panic!("expected dispatch-failed"),
        }
    }
}
