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
}
