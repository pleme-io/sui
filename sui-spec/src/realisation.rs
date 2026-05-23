//! Typed border for content-addressed derivation realisations.
//!
//! When a CA-drv is built, its output's actual store path depends
//! on the realised content's hash.  The mapping from drv path +
//! output name to realised store path is the *realisation*.
//! cppnix serialises realisations as JSON to `/nix/var/nix/realisations/`
//! and serves them via the substituter protocol alongside narinfo
//! files.
//!
//! This module names the realisation format as a typed Lisp spec.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defrealisation-format")]
pub struct RealisationFormat {
    pub name: String,
    pub version: u32,
    pub encoding: RealisationEncoding,
    #[serde(rename = "requiredFields")]
    pub required_fields: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealisationEncoding {
    /// JSON file format used by cppnix.
    JsonText,
}

pub const CANONICAL_REALISATION_LISP: &str =
    include_str!("../specs/realisation.lisp");

/// Compile every authored realisation format.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<RealisationFormat>, SpecError> {
    crate::loader::load_all::<RealisationFormat>(CANONICAL_REALISATION_LISP)
}

// ── M3.0 realisation parser ────────────────────────────────────────

/// Parsed CA-drv realisation record.  The mapping from
/// `<drv-path>!<output-name>` to the realised store path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRealisation {
    /// `<drv-path>!<output-name>` identifier.
    pub id: String,
    /// Realised store path (the actual output bytes' location).
    pub out_path: String,
    /// Signatures attesting to this realisation.
    pub signatures: Vec<String>,
    /// Dependent realisations this one references.
    pub dependent_realisations: Vec<String>,
}

/// Parse a realisation JSON record against a format spec.
///
/// # Errors
///
/// - `realisation-parse` for malformed JSON.
/// - `realisation-missing-required` for absent required fields.
pub fn parse(text: &str, format: &RealisationFormat) -> Result<ParsedRealisation, SpecError> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| SpecError::Interp {
        phase: "realisation-parse".into(),
        message: format!("malformed JSON: {e}"),
    })?;
    let obj = value.as_object().ok_or_else(|| SpecError::Interp {
        phase: "realisation-parse".into(),
        message: "top-level value is not a JSON object".into(),
    })?;
    for field in &format.required_fields {
        if !obj.contains_key(field) {
            return Err(SpecError::Interp {
                phase: "realisation-missing-required".into(),
                message: format!(
                    "realisation missing required field `{field}` per format `{}`",
                    format.name,
                ),
            });
        }
    }
    Ok(ParsedRealisation {
        id: obj.get("id").and_then(|v| v.as_str()).unwrap_or_default().into(),
        out_path: obj.get("outPath").and_then(|v| v.as_str()).unwrap_or_default().into(),
        signatures: obj
            .get("signatures")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        dependent_realisations: obj
            .get("dependentRealisations")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_realisation_parses() {
        let formats = load_canonical().unwrap();
        assert!(!formats.is_empty());
    }

    #[test]
    fn cppnix_realisation_v1_has_essential_fields() {
        let formats = load_canonical().unwrap();
        let v1 = formats
            .iter()
            .find(|f| f.name == "cppnix-realisation-v1")
            .unwrap();
        for required in ["id", "outPath", "signatures", "dependentRealisations"] {
            assert!(
                v1.required_fields.iter().any(|f| f == required),
                "realisation v1 missing field {required}",
            );
        }
    }

    // ── M3.0 parser tests ──────────────────────────────────────

    fn fmt() -> RealisationFormat {
        load_canonical().unwrap().into_iter()
            .find(|f| f.name == "cppnix-realisation-v1").unwrap()
    }

    #[test]
    fn parse_canonical_realisation() {
        let json = r#"{
            "id": "sha256:abc!out",
            "outPath": "/nix/store/abc-hello",
            "signatures": ["cache.nixos.org-1:sig"],
            "dependentRealisations": ["sha256:def!out"]
        }"#;
        let parsed = parse(json, &fmt()).unwrap();
        assert_eq!(parsed.id, "sha256:abc!out");
        assert_eq!(parsed.out_path, "/nix/store/abc-hello");
        assert_eq!(parsed.signatures.len(), 1);
        assert_eq!(parsed.dependent_realisations, vec!["sha256:def!out".to_string()]);
    }

    #[test]
    fn missing_required_field_errors() {
        let json = r#"{ "id": "sha256:abc!out" }"#;
        let err = parse(json, &fmt()).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "realisation-missing-required"),
            _ => panic!("expected missing-required"),
        }
    }

    #[test]
    fn malformed_json_errors() {
        let err = parse("not json", &fmt()).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "realisation-parse"),
            _ => panic!("expected realisation-parse"),
        }
    }
}
