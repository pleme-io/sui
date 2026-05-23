//! Typed border for nix's hash representation.
//!
//! Every hash in nix has an algorithm (sha1/sha256/sha512/md5/blake3)
//! and an encoding (base16 hex, nix-base32, RFC4648 base64, SRI
//! `<algo>-<base64>=` format).  cppnix accepts hashes in any of these
//! shapes; the canonical wire form depends on context (NAR signatures
//! use base32, narinfo `NarHash:` uses sha256-base32, SRI is the
//! flake-input default).
//!
//! This module names the algorithm registry + encoding registry as
//! typed Lisp specs so conversion code rides on a typed contract.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defhash-algorithm
//!   :name "sha256"
//!   :bit-length 256
//!   :weakness Strong
//!   :nix-prefix "sha256")
//!
//! (defhash-encoding
//!   :name "nix-base32"
//!   :alphabet "0123456789abcdfghijklmnpqrsvwxyz"
//!   :preferred-by-nix-for (NarHash StorePathHash))
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border — algorithms ──────────────────────────────────────

/// One hash algorithm.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defhash-algorithm")]
pub struct HashAlgorithm {
    pub name: String,
    #[serde(rename = "bitLength")]
    pub bit_length: u32,
    pub weakness: HashWeakness,
    #[serde(rename = "nixPrefix")]
    pub nix_prefix: String,
}

/// Cryptographic weakness — informational, drives some sui-policy
/// decisions (e.g. refusing weak-hash flake-input signatures).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashWeakness {
    /// Sha256, Sha512, Blake3.
    Strong,
    /// Sha1 — collision attacks demonstrated; accepted only when
    /// already-encoded in legacy artifacts.
    Deprecated,
    /// Md5 — broken; accept only for backward-compat reads of
    /// pre-Nix-2 derivations.
    Broken,
}

// ── Typed border — encodings ───────────────────────────────────────

/// One hash encoding (the format the hash bytes are rendered as).
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defhash-encoding")]
pub struct HashEncoding {
    pub name: String,
    /// Alphabet (display order).  For SRI, this is the field name
    /// since the alphabet is base64 + the `=` padding.
    pub alphabet: String,
    /// Contexts where this encoding is the canonical nix wire shape.
    #[serde(default, rename = "preferredByNixFor")]
    pub preferred_by_nix_for: Vec<NixHashContext>,
}

/// The contexts in which nix renders a hash.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NixHashContext {
    /// `NarHash:` line in narinfo.
    NarHash,
    /// Store-path hash component (`abc123...` in `/nix/store/abc123-name`).
    StorePathHash,
    /// `Sig:` line in narinfo.
    NarSignature,
    /// `outputHash` of a fixed-output derivation.
    FodOutputHash,
    /// `narHash` of a flake input in flake.lock.
    FlakeInputNarHash,
    /// SRI hash on a flake input (`sha256-...`).
    FlakeInputSri,
}

// ── Spec interpreter (today: just parse + verify) ─────────────────

/// Apply a hash conversion.  Today this is a stub; M3 will wire to
/// `sui_compat::store_path` hash conversion helpers.
///
/// # Errors
///
/// Always returns `SpecError::Interp` until M3.
pub fn apply_conversion(
    _from_encoding: &str,
    _to_encoding: &str,
    _input: &str,
) -> Result<String, SpecError> {
    Err(SpecError::Interp {
        phase: "hash-convert".into(),
        message: "hash spec converter not yet landed — sui-compat \
                  has working sha2/base32 today, M3 work lifts to this \
                  typed border".into(),
    })
}

// ── Canonical specs ────────────────────────────────────────────────

pub const CANONICAL_HASH_LISP: &str = include_str!("../specs/hash.lisp");

/// Compile every authored hash algorithm.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical_algorithms() -> Result<Vec<HashAlgorithm>, SpecError> {
    crate::loader::load_all::<HashAlgorithm>(CANONICAL_HASH_LISP)
}

/// Compile every authored hash encoding.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical_encodings() -> Result<Vec<HashEncoding>, SpecError> {
    crate::loader::load_all::<HashEncoding>(CANONICAL_HASH_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn canonical_algorithms_parse() {
        let algos = load_canonical_algorithms().unwrap();
        assert!(!algos.is_empty());
        let names: HashSet<&str> = algos.iter().map(|a| a.name.as_str()).collect();
        for required in ["sha1", "sha256", "sha512", "md5", "blake3"] {
            assert!(
                names.contains(required),
                "canonical hash algos missing `{required}`",
            );
        }
    }

    #[test]
    fn sha256_is_strong() {
        let algos = load_canonical_algorithms().unwrap();
        let sha256 = algos.iter().find(|a| a.name == "sha256").unwrap();
        assert_eq!(sha256.weakness, HashWeakness::Strong);
        assert_eq!(sha256.bit_length, 256);
    }

    #[test]
    fn md5_is_broken() {
        let algos = load_canonical_algorithms().unwrap();
        let md5 = algos.iter().find(|a| a.name == "md5").unwrap();
        assert_eq!(md5.weakness, HashWeakness::Broken);
    }

    #[test]
    fn canonical_encodings_parse() {
        let encs = load_canonical_encodings().unwrap();
        let names: HashSet<&str> = encs.iter().map(|e| e.name.as_str()).collect();
        for required in ["base16", "nix-base32", "base64", "sri"] {
            assert!(
                names.contains(required),
                "canonical encodings missing `{required}`",
            );
        }
    }

    #[test]
    fn nix_base32_is_preferred_for_storepath() {
        let encs = load_canonical_encodings().unwrap();
        let b32 = encs.iter().find(|e| e.name == "nix-base32").unwrap();
        assert!(
            b32.preferred_by_nix_for.contains(&NixHashContext::StorePathHash),
            "nix-base32 must be the canonical store-path encoding",
        );
    }

    #[test]
    fn sri_is_preferred_for_flake_input() {
        let encs = load_canonical_encodings().unwrap();
        let sri = encs.iter().find(|e| e.name == "sri").unwrap();
        assert!(
            sri.preferred_by_nix_for.contains(&NixHashContext::FlakeInputSri),
            "sri must be the canonical flake-input hash format",
        );
    }
}
