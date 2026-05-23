//! Typed border for the narinfo file format.
//!
//! When a substituter serves a NAR, the narinfo file alongside it
//! carries the metadata: store path, NAR URL, NAR hash, file
//! size, references (closure), deriver, signatures, compression
//! method.  Format is plain-text, one field per line, `Key: Value`
//! with a closed key set cppnix has stabilised since Nix 2.
//!
//! This module names the format as a typed Lisp spec so future
//! parser/emitter implementations ride on the same contract both
//! engines agree on.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defnarinfo-format
//!   :name        "cppnix-narinfo-v1"
//!   :fields      (Required Required Required Required
//!                 Optional Optional Optional Required)
//!   :field-names ("StorePath" "URL" "Compression" "FileHash"
//!                 "FileSize" "NarHash" "NarSize" "References"
//!                 "Deriver" "System" "Sig")
//!   :phases      ((:kind ParseTextFields)
//!                 (:kind ValidateRequiredFields)
//!                 (:kind ParseSignatures)
//!                 (:kind EmitTextOutput)))
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border ───────────────────────────────────────────────────

/// One narinfo format variant.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defnarinfo-format")]
pub struct NarinfoFormat {
    pub name: String,
    /// Declared fields in the order they conventionally appear.
    /// Length must match `field_names`.
    pub fields: Vec<NarinfoFieldKind>,
    /// Canonical key names — index-aligned with `fields`.
    #[serde(rename = "fieldNames")]
    pub field_names: Vec<String>,
    pub phases: Vec<NarinfoPhase>,
}

/// Whether a field is required or optional in the narinfo.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarinfoFieldKind {
    /// Must appear; parser rejects narinfo without it.
    Required,
    /// May appear or not; parser tolerates absence.
    Optional,
    /// May appear multiple times (e.g. `Sig:` for multi-signature).
    Repeatable,
}

/// One phase of narinfo handling.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NarinfoPhase {
    pub kind: NarinfoPhaseKind,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
}

/// Closed set of narinfo phases.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarinfoPhaseKind {
    /// Read raw text, split on `\n`, parse `Key: Value` per line.
    ParseTextFields,
    /// Check every Required field is present.
    ValidateRequiredFields,
    /// Parse `Sig:` lines into typed (key-name, signature) pairs.
    ParseSignatures,
    /// Parse `References:` whitespace-separated store-path list.
    ParseReferences,
    /// Validate the `NarHash:` is well-formed (sri or
    /// `sha256:<base32>`).
    ValidateNarHashShape,
    /// Emit the parsed narinfo back to text — round-trip clean
    /// byte-equality is a parser correctness invariant.
    EmitTextOutput,
}

// ── Spec interpreter (M3 stub) ─────────────────────────────────────

/// Apply the narinfo algorithm.  M3 stub.
///
/// # Errors
///
/// Always until M3.
pub fn apply(_format: &NarinfoFormat) -> Result<String, SpecError> {
    Err(SpecError::Interp {
        phase: "narinfo".into(),
        message: "narinfo spec interpreter not yet landed — sui-cache \
                  has a working parser, M3 work lifts to this border"
            .into(),
    })
}

// ── Canonical spec ─────────────────────────────────────────────────

pub const CANONICAL_NARINFO_LISP: &str = include_str!("../specs/narinfo.lisp");

/// Compile every authored narinfo format.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<NarinfoFormat>, SpecError> {
    crate::loader::load_all::<NarinfoFormat>(CANONICAL_NARINFO_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn canonical_narinfo_parses() {
        let formats = load_canonical().expect("canonical narinfo must compile");
        assert!(!formats.is_empty());
    }

    #[test]
    fn cppnix_v1_lists_required_keys() {
        let formats = load_canonical().unwrap();
        let v1 = formats
            .iter()
            .find(|f| f.name == "cppnix-narinfo-v1")
            .expect("cppnix-narinfo-v1 must exist");
        assert_eq!(v1.fields.len(), v1.field_names.len(),
            "fields/field_names length mismatch");
        // Build name → kind map for assertions.
        let by_name: HashMap<&str, NarinfoFieldKind> = v1
            .field_names
            .iter()
            .map(|s| s.as_str())
            .zip(v1.fields.iter().copied())
            .collect();
        // The four bytes-on-the-wire mandatory fields.
        for required in ["StorePath", "URL", "NarHash", "NarSize"] {
            assert_eq!(
                by_name.get(required).copied(),
                Some(NarinfoFieldKind::Required),
                "{required} must be Required in cppnix-narinfo-v1",
            );
        }
        // Sig is canonically Repeatable (multi-key signature).
        assert_eq!(
            by_name.get("Sig").copied(),
            Some(NarinfoFieldKind::Repeatable),
        );
    }

    #[test]
    fn narinfo_phases_include_text_roundtrip() {
        let formats = load_canonical().unwrap();
        for f in &formats {
            let kinds: Vec<NarinfoPhaseKind> =
                f.phases.iter().map(|p| p.kind).collect();
            assert!(kinds.contains(&NarinfoPhaseKind::ParseTextFields),
                "{}: missing ParseTextFields", f.name);
            assert!(kinds.contains(&NarinfoPhaseKind::ValidateRequiredFields),
                "{}: missing ValidateRequiredFields", f.name);
        }
    }
}
