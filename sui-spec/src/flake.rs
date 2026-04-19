//! Flake result shape — declarative policy for `getFlake` / `callFlake`
//! output assembly.  CppNix parity, authored as Lisp data.
//!
//! Before this spec existed, the top-level flake attrset was built
//! in `sui-eval/src/builtins/flake_eval.rs` by hand: the code
//! inserted `_type`, `outPath`, `sourceInfo`, `inputs`, `outputs`
//! explicitly, then iterated `flake_attrs` copying anything not
//! already present.  That loop silently leaked every top-level
//! attribute from the parsed `flake.nix` (`description`, `nixConfig`,
//! ...) — which the first cross-repo sweep caught: CppNix does NOT
//! expose those keys on the flake result, and every pleme-io flake's
//! `(getFlake …)` result disagreed.
//!
//! This crate encodes the policy once:
//!
//! ```lisp
//! (defflake-shape
//!   :name                       "cppnix"
//!   :type-marker                "flake"
//!   :required-keys              ("_type" "outPath" "sourceInfo"
//!                                "inputs" "outputs")
//!   :spread-from-output-fn       t
//!   :never-leak-from-flake-body ("description" "nixConfig"))
//! ```
//!
//! The Rust caller asks the shape: "should I copy this key from
//! the flake body?", "what's the type marker string?", "am I
//! allowed to spread the outputs-fn result?".  Each question is
//! a one-method call.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

/// Top-level flake result shape policy.
///
/// `required_keys` is informational only — the Rust assembler in
/// `flake_eval.rs` is responsible for *producing* the values (they
/// all require eval-time state the spec doesn't have access to).
/// The shape exists to answer yes/no decisions the assembler makes
/// along the way.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defflake-shape")]
pub struct FlakeShape {
    pub name: String,
    #[serde(rename = "typeMarker")]
    pub type_marker: String,
    #[serde(default, rename = "requiredKeys")]
    pub required_keys: Vec<String>,
    #[serde(default, rename = "spreadFromOutputFn")]
    pub spread_from_output_fn: bool,
    #[serde(default, rename = "neverLeakFromFlakeBody")]
    pub never_leak_from_flake_body: Vec<String>,
}

impl FlakeShape {
    /// Is `key` allowed to be copied from the parsed flake body
    /// (`{ description = ...; outputs = ...; }`) onto the top-level
    /// flake result?
    ///
    /// Note: the assembler always has to consult this — the default
    /// answer under CppNix parity is "no for most keys".  Only
    /// explicitly-allowed keys (typically `inputs`) should cross
    /// over, and those are handled by named inserts, not by body
    /// iteration.
    #[must_use]
    pub fn allow_body_key(&self, key: &str) -> bool {
        !self.never_leak_from_flake_body.iter().any(|k| k == key)
    }

    /// Whether the outputs-function's returned attrset spreads
    /// (its keys merge directly) into the top-level flake result.
    /// CppNix says yes; every other policy is a departure.
    #[must_use]
    pub fn spreads_output_fn(&self) -> bool {
        self.spread_from_output_fn
    }
}

// ── Canonical spec ─────────────────────────────────────────────────

pub const CPPNIX_FLAKE_SHAPE_LISP: &str = include_str!("../specs/flake.lisp");

/// Compile the embedded canonical flake-shape spec.
///
/// # Errors
///
/// Returns an error if the compile-time spec fails to parse or
/// produces no `(defflake-shape ...)` forms.
pub fn load_canonical() -> Result<FlakeShape, SpecError> {
    let mut compiled = tatara_lisp::compile_typed::<FlakeShape>(
        CPPNIX_FLAKE_SHAPE_LISP,
    )?;
    compiled.pop().ok_or_else(|| SpecError::Load(
        "no (defflake-shape ...) forms found in canonical spec".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_spec_parses() {
        let shape = load_canonical().expect("canonical flake shape must compile");
        assert_eq!(shape.name, "cppnix");
        assert_eq!(shape.type_marker, "flake");
        assert!(shape.spread_from_output_fn);
    }

    #[test]
    fn description_does_not_leak_from_body() {
        let shape = load_canonical().unwrap();
        assert!(!shape.allow_body_key("description"),
            "description must not leak — CppNix does not expose it at the top level");
        assert!(!shape.allow_body_key("nixConfig"),
            "nixConfig must not leak — CppNix does not expose it at the top level");
    }

    #[test]
    fn inputs_and_outputs_reach_the_top() {
        let shape = load_canonical().unwrap();
        // These keys are named-inserts, not body-iterations, so the
        // question here is simply "does the spec declare them as
        // required"?
        assert!(shape.required_keys.iter().any(|k| k == "inputs"));
        assert!(shape.required_keys.iter().any(|k| k == "outputs"));
        assert!(shape.required_keys.iter().any(|k| k == "_type"));
    }
}
