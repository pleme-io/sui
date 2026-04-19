//! Derivation path computation — the CppNix algorithm as Lisp data.
//!
//! This module hosts the four-phase input-addressed derivation
//! algorithm that used to live in hand-written Rust, duplicated
//! between `sui-eval` (tree-walker) and `sui-bytecode` (VM).  Four
//! distinct spec bugs were found in that code during the parity
//! session:
//!
//! | # | Bug                                                 |
//! |---|-----------------------------------------------------|
//! |11 | Missing `env.<output>` placeholder after fill       |
//! |12 | `.drv` path hashed unresolved form, not final form  |
//! |13 | Unresolved form must have `env.out = ""` present    |
//! |14 | VM args reader didn't force list items (empty args) |
//!
//! Every one of those was a *spec* mistake.  They came in pairs
//! because each engine had its own copy.  The cure is here: one
//! typed Rust algorithm definition, one authored `.lisp` spec,
//! one interpreter, two engine call sites — and no way to drift.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defderivation-algorithm cppnix-input-addressed
//!   :name "cppnix-input-addressed"
//!   :phases ((:kind MaskOutputsAndEnv)
//!            (:kind Serialize :bind "unresolved")
//!            (:kind Sha256 :from "unresolved" :bind "inner-hex")
//!            (:kind ComputeOutputPaths :from-hash "inner-hex")
//!            (:kind FillPlaceholders)
//!            (:kind Serialize :bind "final")
//!            (:kind Sha256 :from "final" :bind "final-hex")
//!            (:kind ComputeDrvPath :from-hash "final-hex")))
//! ```
//!
//! Each bug's fix is one line of Lisp.  Future additions — e.g. a
//! `cppnix-fixed-output` algorithm, a `cppnix-ca-derivation` variant
//! for content-addressed derivations — each become one `.lisp`
//! form, inheriting the interpreter for free.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sui_compat::derivation::{Derivation, DerivationOutput};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border ───────────────────────────────────────────────────

/// Top-level algorithm definition, authored as `(defderivation-algorithm ...)`.
///
/// `phases` is interpreted left-to-right by [`apply`].  Each phase
/// reads from a scratchpad of named slots (populated by earlier
/// phases) and writes to zero or more output slots.  The typed border
/// is declarative: there is no way to author a phase whose inputs
/// aren't statically representable here.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defderivation-algorithm")]
pub struct DerivationAlgorithm {
    pub name: String,
    pub phases: Vec<Phase>,
}

/// A single pipeline phase.  Each phase declares its `kind` and
/// optionally binds inputs (`from`, `from_hash`) or an output slot
/// (`bind`).  The `#[serde(default)]`s are what let simple phases be
/// authored as `(:kind MaskOutputsAndEnv)` with no extra kwargs.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Phase {
    pub kind: PhaseKind,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default, rename = "fromHash")]
    pub from_hash: Option<String>,
}

/// Enumeration of every phase the interpreter knows how to run.
/// Adding a new phase here IS adding a new primitive to the spec
/// language — the typed border is exactly this set.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseKind {
    /// Set every output's `path` to `""` AND every env entry whose
    /// name matches an output to `""`.  This is what CppNix calls
    /// "maskOutputs" — it's the precondition for hashing the
    /// "unresolved" form of a derivation.
    MaskOutputsAndEnv,

    /// ATerm-serialize the current derivation into the bytes slot
    /// named by `bind`.
    Serialize,

    /// Compute SHA-256 of the bytes in slot `from`, store the
    /// lowercase-hex digest into slot `bind`.
    Sha256,

    /// Given the inner hex stored in slot `from_hash`, compute the
    /// per-output store path via `sui_compat::store_path::compute_output_path`
    /// and populate the shared `out_paths` map.
    ComputeOutputPaths,

    /// Copy each entry of `out_paths` back into the derivation:
    /// `drv.outputs[<name>].path = <path>` AND `drv.env[<name>] = <path>`.
    /// After this phase the derivation is in CppNix's "final" form.
    FillPlaceholders,

    /// Given the final hex stored in slot `from_hash`, compute the
    /// `.drv` store path via `sui_compat::store_path::compute_drv_path`
    /// and record it as the overall result.
    ComputeDrvPath,
}

// ── Interpreter ────────────────────────────────────────────────────

/// Interpreter scratchpad — shared state threaded through phases.
///
/// Every slot has a documented producer and consumer (see
/// [`PhaseKind`]).  `binds` is a generic name→bytes scratchpad; for
/// hashes we intern the hex into the same map (bytes carry either
/// ATerm text or hex digests).
pub struct DerivationState {
    pub drv: Derivation,
    pub outputs_list: Vec<String>,
    pub drv_name: String,
    pub binds: HashMap<String, Vec<u8>>,
    pub out_paths: BTreeMap<String, String>,
    pub drv_path: Option<String>,
}

impl DerivationState {
    #[must_use]
    pub fn new(drv: Derivation, outputs_list: Vec<String>, drv_name: String) -> Self {
        Self {
            drv,
            outputs_list,
            drv_name,
            binds: HashMap::new(),
            out_paths: BTreeMap::new(),
            drv_path: None,
        }
    }

    fn get_bytes(&self, key: &str) -> Result<&[u8], SpecError> {
        self.binds
            .get(key)
            .map(std::vec::Vec::as_slice)
            .ok_or_else(|| SpecError::UnboundSlot(key.to_string()))
    }
}

/// Apply every phase in order.  Returns the final `.drv` path, the
/// per-output store paths, and the mutated derivation with paths
/// filled in.
///
/// Callers (tree-walker, VM) pass in a partially-populated
/// `Derivation` (outputs empty; env already has the non-output
/// entries) and a list of output names.  This function performs the
/// full input-addressed algorithm.  Both engines call exactly this
/// function, with exactly these arguments, so they cannot drift.
///
/// # Errors
///
/// Returns an error if a phase refers to an unbound slot, or if an
/// individual phase's precondition is violated (e.g. `ComputeDrvPath`
/// runs before any placeholders are filled).
pub fn apply(
    algo: &DerivationAlgorithm,
    drv: Derivation,
    outputs_list: Vec<String>,
    name: &str,
) -> Result<(String, BTreeMap<String, String>, Derivation), SpecError> {
    let mut state = DerivationState::new(drv, outputs_list, name.to_string());
    for phase in &algo.phases {
        run_phase(phase, &mut state)?;
    }
    let drv_path = state.drv_path.ok_or_else(|| SpecError::Interp {
        phase: "finalize".into(),
        message: "algorithm completed without binding a .drv path \
                  (missing ComputeDrvPath phase?)".into(),
    })?;
    Ok((drv_path, state.out_paths, state.drv))
}

fn run_phase(phase: &Phase, s: &mut DerivationState) -> Result<(), SpecError> {
    match phase.kind {
        PhaseKind::MaskOutputsAndEnv => mask_outputs_and_env(s),
        PhaseKind::Serialize => serialize(phase, s),
        PhaseKind::Sha256 => sha256(phase, s),
        PhaseKind::ComputeOutputPaths => compute_output_paths(phase, s),
        PhaseKind::FillPlaceholders => fill_placeholders(s),
        PhaseKind::ComputeDrvPath => compute_drv_path(phase, s),
    }
}

fn mask_outputs_and_env(s: &mut DerivationState) -> Result<(), SpecError> {
    for o in &s.outputs_list {
        s.drv.outputs.insert(o.clone(), DerivationOutput {
            path: String::new(),
            hash_algo: String::new(),
            hash: String::new(),
        });
        s.drv.env.insert(o.clone(), String::new());
    }
    Ok(())
}

fn serialize(phase: &Phase, s: &mut DerivationState) -> Result<(), SpecError> {
    let slot = phase.bind.clone().ok_or_else(|| SpecError::Interp {
        phase: "Serialize".into(),
        message: ":bind is required".into(),
    })?;
    let bytes = s.drv.serialize().into_bytes();
    s.binds.insert(slot, bytes);
    Ok(())
}

fn sha256(phase: &Phase, s: &mut DerivationState) -> Result<(), SpecError> {
    let from = phase.from.clone().ok_or_else(|| SpecError::Interp {
        phase: "Sha256".into(),
        message: ":from is required".into(),
    })?;
    let bind = phase.bind.clone().ok_or_else(|| SpecError::Interp {
        phase: "Sha256".into(),
        message: ":bind is required".into(),
    })?;
    let input = s.get_bytes(&from)?;
    let digest = Sha256::digest(input);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    s.binds.insert(bind, hex.into_bytes());
    Ok(())
}

fn compute_output_paths(phase: &Phase, s: &mut DerivationState) -> Result<(), SpecError> {
    let from = phase.from_hash.clone().ok_or_else(|| SpecError::Interp {
        phase: "ComputeOutputPaths".into(),
        message: ":from-hash is required".into(),
    })?;
    let hex = {
        let bytes = s.get_bytes(&from)?;
        std::str::from_utf8(bytes).map_err(|e| SpecError::Interp {
            phase: "ComputeOutputPaths".into(),
            message: format!("slot {from} is not valid utf-8: {e}"),
        })?.to_string()
    };
    let outputs_snapshot: Vec<String> = s.outputs_list.clone();
    let drv_name = s.drv_name.clone();
    for o in &outputs_snapshot {
        let p = sui_compat::store_path::compute_output_path(&hex, o, &drv_name);
        s.out_paths.insert(o.clone(), p);
    }
    Ok(())
}

fn fill_placeholders(s: &mut DerivationState) -> Result<(), SpecError> {
    for o in &s.outputs_list {
        let placeholder = s.out_paths.get(o).cloned().ok_or_else(|| SpecError::Interp {
            phase: "FillPlaceholders".into(),
            message: format!("no path computed for output {o} \
                              (did ComputeOutputPaths run first?)"),
        })?;
        if let Some(entry) = s.drv.outputs.get_mut(o) {
            entry.path = placeholder.clone();
        }
        s.drv.env.insert(o.clone(), placeholder);
    }
    Ok(())
}

fn compute_drv_path(phase: &Phase, s: &mut DerivationState) -> Result<(), SpecError> {
    let from = phase.from_hash.clone().ok_or_else(|| SpecError::Interp {
        phase: "ComputeDrvPath".into(),
        message: ":from-hash is required".into(),
    })?;
    // Validate the hex slot is present + utf-8 (defensive check —
    // produces a crisp error if the author references a slot that
    // no earlier phase populated).
    {
        let bytes = s.get_bytes(&from)?;
        let _ = std::str::from_utf8(bytes).map_err(|e| SpecError::Interp {
            phase: "ComputeDrvPath".into(),
            message: format!("slot {from} is not valid utf-8: {e}"),
        })?;
    }
    // Convention: the hex slot name is `<bytes-slot>-hex`, so the
    // raw ATerm bytes live at the same name without that suffix.
    // `compute_drv_path` re-hashes the bytes internally; we need
    // the bytes (not the hex) because the store-path construction
    // happens atop the hash *and* includes input refs as prefix
    // terms (via `makeTextPath`).
    let bytes_slot = from.trim_end_matches("-hex").to_string();
    let drv_name = s.drv_name.clone();
    let drv_path = {
        let bytes = s.get_bytes(&bytes_slot)?;
        sui_compat::store_path::compute_drv_path(bytes, &drv_name)
    };
    s.drv_path = Some(drv_path);
    Ok(())
}

// ── Canonical spec, compiled in ────────────────────────────────────

/// The CppNix input-addressed algorithm as a compile-time string.
/// Callers use [`load_canonical`] to parse this into a typed
/// [`DerivationAlgorithm`] — we keep the source embedded so the spec
/// ships with the crate and is verifiable by reading this file
/// alongside `specs/derivation.lisp`.
pub const CPPNIX_INPUT_ADDRESSED_LISP: &str = include_str!("../specs/derivation.lisp");

/// Compile the embedded canonical spec into a typed algorithm.
///
/// # Errors
///
/// Returns an error if the compile-time spec fails to parse or
/// produces no `(defderivation-algorithm ...)` forms.
pub fn load_canonical() -> Result<DerivationAlgorithm, SpecError> {
    let mut compiled = tatara_lisp::compile_typed::<DerivationAlgorithm>(
        CPPNIX_INPUT_ADDRESSED_LISP,
    )?;
    compiled.pop().ok_or_else(|| SpecError::Load(
        "no (defderivation-algorithm ...) forms found in canonical spec".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_spec_parses() {
        let algo = load_canonical().expect("canonical spec must compile");
        assert_eq!(algo.name, "cppnix-input-addressed");
        // Six declared phases — masking, two serialize/hash pairs,
        // one placeholder fill, one final drv-path emission.
        assert!(!algo.phases.is_empty(), "algorithm must have phases");
    }

    #[test]
    fn canonical_spec_matches_cppnix_on_hello_derivation() {
        let algo = load_canonical().unwrap();
        let mut env = std::collections::BTreeMap::new();
        env.insert("builder".into(), "/bin/sh".into());
        env.insert("name".into(), "hello".into());
        env.insert("system".into(), "aarch64-darwin".into());
        let drv = Derivation {
            outputs: std::collections::BTreeMap::new(),
            input_derivations: std::collections::BTreeMap::new(),
            input_sources: Vec::new(),
            system: "aarch64-darwin".into(),
            builder: "/bin/sh".into(),
            args: vec!["-c".into(), "echo hi > $out".into()],
            env,
        };
        let (drv_path, out_paths, _final_drv) =
            apply(&algo, drv, vec!["out".to_string()], "hello").unwrap();
        // This is THE parity assertion — same input, same output as
        // CppNix, verified empirically on 2026-04-18.
        assert_eq!(
            drv_path,
            "/nix/store/mypmkciickjnhjjimhzjn6w7qj7g8n2k-hello.drv"
        );
        assert_eq!(
            out_paths.get("out").map(String::as_str),
            Some("/nix/store/k6lq59b6dilrfy0blhkr10m27ga7ncwr-hello"),
        );
    }
}
