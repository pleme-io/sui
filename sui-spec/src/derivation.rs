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

    /// Like [`Serialize`] but replace every `input_derivations`
    /// key (a `.drv` path) with its `hashDerivationModulo` lookup
    /// from the thread-local modulo cache — CppNix's recursion
    /// over dependent derivations.  Input drvs with no cached
    /// modulo hash are passed through unchanged, which matches
    /// the parity-on-leaves case (a drv with no dependencies
    /// serialises identically in both forms).
    ///
    /// [`Serialize`]: PhaseKind::Serialize
    SerializeModulo,

    /// Record the current hex hash in `from_hash` as the modulo
    /// hash for the produced `.drv` path, so derivations that
    /// depend on this one can look it up during their own
    /// `SerializeModulo` phase.
    CacheSelfModulo,

    // ── Fixed-output / content-addressed extensions (M3 stubs) ──
    //
    // The two phase variants below name the contract for the
    // remaining cppnix store-path schemes.  Today they return
    // SpecError::Interp; the M3 implementation wires them up to
    // sui_compat::store_path's fixed-output / CA computation paths.

    /// For a fixed-output derivation: read `drv.env.outputHash`,
    /// `outputHashAlgo`, `outputHashMode` and seed the per-output
    /// fingerprint from the *output content hash* rather than the
    /// recipe hash.  Output store path follows from this seed via
    /// the same `compute_output_path` route as input-addressed.
    SeedFixedOutputHash,

    /// Mark the derivation as CA (`__contentAddressed = true`).
    /// CA outputs' paths aren't known at recipe time; the builder
    /// resolves them post-realisation and rewrites the .drv in
    /// place.  This phase plants the marker so downstream phases
    /// route correctly.
    MarkContentAddressed,

    /// For CA derivations only: write a *placeholder* output path
    /// (cppnix uses `/nix/store/<placeholder>-<name>` style); the
    /// builder substitutes real paths after a successful build.
    EmitCaPlaceholders,
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
        PhaseKind::SerializeModulo => serialize_modulo(phase, s),
        PhaseKind::CacheSelfModulo => cache_self_modulo(phase, s),
        PhaseKind::SeedFixedOutputHash => Err(SpecError::Interp {
            phase: "SeedFixedOutputHash".into(),
            message: "fixed-output derivation phase not yet implemented — \
                      M3 will wire to sui_compat::store_path::compute_fixed_output_path"
                .into(),
        }),
        PhaseKind::MarkContentAddressed => Err(SpecError::Interp {
            phase: "MarkContentAddressed".into(),
            message: "content-addressed derivation phase not yet implemented — \
                      M4 work hangs off this border"
                .into(),
        }),
        PhaseKind::EmitCaPlaceholders => Err(SpecError::Interp {
            phase: "EmitCaPlaceholders".into(),
            message: "CA placeholder emission not yet implemented (M4)".into(),
        }),
    }
}

// ── Modulo cache (for hashDerivationModulo recursion) ──────────────
//
// CppNix builds a dependent derivation's `.drv` path by hashing its
// final ATerm WITH each input derivation's `.drv` path replaced by
// that input's own recursive modulo hash.  We cache the modulo hash
// alongside every `.drv` path we produce so subsequent derivations
// that depend on it can look up the right substitute.
//
// Thread-local keeps this simple: single-threaded eval, same cache
// visible to tree-walker + VM, cleared between test runs by the
// test harness reseting thread-locals.  A production deploy that
// wanted cross-eval memoization would swap this for a persistent
// store — the interface stays the same.

use std::cell::RefCell;

thread_local! {
    static MODULO_CACHE: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

/// Record a `.drv` path → modulo hash binding, so dependent
/// derivations can look it up later.
pub fn remember_modulo_hash(drv_path: &str, modulo_hex: &str) {
    MODULO_CACHE.with(|c| {
        c.borrow_mut().insert(drv_path.to_string(), modulo_hex.to_string());
    });
}

/// Look up the modulo hash for `drv_path`.  Returns the `drv_path`
/// unchanged if no entry exists — which is correct for leaves
/// (derivations with no inputs produce the same bytes in both
/// normal and modulo forms).
#[must_use]
pub fn modulo_of(drv_path: &str) -> String {
    MODULO_CACHE.with(|c| {
        c.borrow().get(drv_path).cloned().unwrap_or_else(|| drv_path.to_string())
    })
}

/// Clear the cache.  Intended for test isolation.
pub fn reset_modulo_cache() {
    MODULO_CACHE.with(|c| c.borrow_mut().clear());
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
    // `compute_drv_path_with_refs` re-hashes the bytes internally
    // but ALSO folds the derivation's input refs (input_derivations
    // + input_sources) into the fingerprint — `makeTextPath` style.
    // Without refs, any derivation that references another drv or
    // a /nix/store source disagrees with CppNix (discovered while
    // wiring transitive parity).
    let bytes_slot = from.trim_end_matches("-hex").to_string();
    let drv_name = s.drv_name.clone();
    let refs: Vec<String> = s.drv.input_derivations
        .keys()
        .cloned()
        .chain(s.drv.input_sources.iter().cloned())
        .collect();
    let drv_path = {
        let bytes = s.get_bytes(&bytes_slot)?;
        sui_compat::store_path::compute_drv_path_with_refs(bytes, &drv_name, &refs)
    };
    s.drv_path = Some(drv_path);
    Ok(())
}

fn serialize_modulo(phase: &Phase, s: &mut DerivationState) -> Result<(), SpecError> {
    let slot = phase.bind.clone().ok_or_else(|| SpecError::Interp {
        phase: "SerializeModulo".into(),
        message: ":bind is required".into(),
    })?;
    let bytes = s.drv.serialize_modulo(|drv_path| modulo_of(drv_path)).into_bytes();
    s.binds.insert(slot, bytes);
    Ok(())
}

fn cache_self_modulo(phase: &Phase, s: &mut DerivationState) -> Result<(), SpecError> {
    let from = phase.from_hash.clone().ok_or_else(|| SpecError::Interp {
        phase: "CacheSelfModulo".into(),
        message: ":from-hash is required".into(),
    })?;
    let drv_path = s.drv_path.clone().ok_or_else(|| SpecError::Interp {
        phase: "CacheSelfModulo".into(),
        message: "no drv path bound yet (run ComputeDrvPath first)".into(),
    })?;
    let hex = {
        let bytes = s.get_bytes(&from)?;
        std::str::from_utf8(bytes).map_err(|e| SpecError::Interp {
            phase: "CacheSelfModulo".into(),
            message: format!("slot {from} is not valid utf-8: {e}"),
        })?.to_string()
    };
    remember_modulo_hash(&drv_path, &hex);
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
/// Returns the `cppnix-input-addressed` algorithm specifically; for
/// the FOD or CA variants see [`load_named`].
///
/// # Errors
///
/// Returns an error if the compile-time spec fails to parse or
/// the input-addressed algorithm is missing from the corpus.
pub fn load_canonical() -> Result<DerivationAlgorithm, SpecError> {
    load_named("cppnix-input-addressed")
}

/// Compile the canonical spec and return every authored algorithm.
///
/// # Errors
///
/// Returns an error if the spec fails to parse.
pub fn load_all_canonical() -> Result<Vec<DerivationAlgorithm>, SpecError> {
    Ok(tatara_lisp::compile_typed::<DerivationAlgorithm>(
        CPPNIX_INPUT_ADDRESSED_LISP,
    )?)
}

/// Compile the canonical spec and return the algorithm whose `name`
/// matches.  Today's named algorithms:
///
/// - `"cppnix-input-addressed"` — the default builder path.
/// - `"cppnix-fixed-output"` — fetchurl/fetchTarball-style FODs (M3 stub).
/// - `"cppnix-content-addressed"` — CA-drv experimental (M4 stub).
///
/// # Errors
///
/// Returns an error if the spec fails to parse or `name` is missing.
pub fn load_named(name: &str) -> Result<DerivationAlgorithm, SpecError> {
    let all = load_all_canonical()?;
    all.into_iter()
        .find(|a| a.name == name)
        .ok_or_else(|| SpecError::Load(
            format!("no (defderivation-algorithm) with :name {name:?}"),
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
    fn fixed_output_algorithm_parses() {
        let algo = load_named("cppnix-fixed-output")
            .expect("FOD algorithm must compile");
        let kinds: Vec<PhaseKind> = algo.phases.iter().map(|p| p.kind).collect();
        assert!(kinds.contains(&PhaseKind::SeedFixedOutputHash));
        assert!(kinds.contains(&PhaseKind::ComputeDrvPath));
    }

    #[test]
    fn content_addressed_algorithm_parses() {
        let algo = load_named("cppnix-content-addressed")
            .expect("CA-drv algorithm must compile");
        let kinds: Vec<PhaseKind> = algo.phases.iter().map(|p| p.kind).collect();
        assert!(kinds.contains(&PhaseKind::MarkContentAddressed));
        assert!(kinds.contains(&PhaseKind::EmitCaPlaceholders));
    }

    #[test]
    fn all_canonical_algorithms_load() {
        let all = load_all_canonical().expect("all algos must compile");
        let names: std::collections::HashSet<&str> =
            all.iter().map(|a| a.name.as_str()).collect();
        for required in [
            "cppnix-input-addressed",
            "cppnix-fixed-output",
            "cppnix-content-addressed",
        ] {
            assert!(
                names.contains(required),
                "canonical corpus missing algorithm `{required}`",
            );
        }
    }

    #[test]
    fn fod_apply_returns_typed_not_yet() {
        let algo = load_named("cppnix-fixed-output").unwrap();
        let mut env = std::collections::BTreeMap::new();
        env.insert("outputHash".into(), "sha256-abc123".into());
        let drv = Derivation {
            outputs: std::collections::BTreeMap::new(),
            input_derivations: std::collections::BTreeMap::new(),
            input_sources: Vec::new(),
            system: "aarch64-darwin".into(),
            builder: "/bin/sh".into(),
            args: vec![],
            env,
        };
        let err = apply(&algo, drv, vec!["out".into()], "fixed-output-test")
            .expect_err("FOD apply must surface typed not-yet");
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "SeedFixedOutputHash");
                assert!(message.contains("M3"));
            }
            _ => panic!("expected SpecError::Interp, got {err:?}"),
        }
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
