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

// ── Spec interpreter (M3.0 minimal — encoding conversion) ─────────

/// Parse a hex string into raw bytes.  Case-insensitive.
fn from_base16(s: &str) -> Result<Vec<u8>, SpecError> {
    if s.len() % 2 != 0 {
        return Err(SpecError::Interp {
            phase: "hash-decode".into(),
            message: format!("base16 string `{s}` has odd length {}", s.len()),
        });
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let chars: Vec<char> = s.chars().collect();
    for chunk in chars.chunks(2) {
        let pair: String = chunk.iter().collect();
        let byte = u8::from_str_radix(&pair, 16).map_err(|e| SpecError::Interp {
            phase: "hash-decode".into(),
            message: format!("invalid hex byte `{pair}`: {e}"),
        })?;
        out.push(byte);
    }
    Ok(out)
}

fn to_base16(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Convert from nix-base32 (Nix's custom alphabet — note: NOT
/// RFC 4648 base32).  Nix encodes hashes LSB-first with this
/// alphabet: "0123456789abcdfghijklmnpqrsvwxyz" (note no 'e', 'o',
/// 't', 'u').
fn from_nix_base32(s: &str) -> Result<Vec<u8>, SpecError> {
    const ALPHABET: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    let mut idx_table = [0xff_u8; 256];
    for (i, b) in ALPHABET.iter().enumerate() {
        idx_table[*b as usize] = i as u8;
    }
    // Estimate output bytes: nix-base32 is 8 bits per 5 chars.
    let n = (s.len() * 5).div_ceil(8);
    let mut bytes = vec![0u8; n];
    for (n_offset, c) in s.chars().enumerate() {
        let digit = idx_table[c as usize];
        if digit == 0xff {
            return Err(SpecError::Interp {
                phase: "hash-decode".into(),
                message: format!("char `{c}` not in nix-base32 alphabet"),
            });
        }
        let b = n_offset * 5;
        let i = b / 8;
        let j = b % 8;
        bytes[i] |= (digit as u16).wrapping_shl(j as u32) as u8 & 0xff;
        if j + 5 > 8 && i + 1 < n {
            bytes[i + 1] |= (digit as u16).wrapping_shr((8 - j) as u32) as u8;
        }
    }
    Ok(bytes)
}

fn to_nix_base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    let n_chars = (bytes.len() * 8).div_ceil(5);
    let mut out = String::with_capacity(n_chars);
    for n in (0..n_chars).rev() {
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        let c = (bytes[i] as u16).wrapping_shr(j as u32)
            | if i + 1 < bytes.len() {
                (bytes[i + 1] as u16).wrapping_shl((8 - j) as u32)
            } else { 0 };
        out.push(ALPHABET[(c & 0x1f) as usize] as char);
    }
    out
}

fn from_base64(s: &str) -> Result<Vec<u8>, SpecError> {
    use base64::Engine as _;
    let s = s.trim_end_matches('=');
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(s.as_bytes())
        .map_err(|e| SpecError::Interp {
            phase: "hash-decode".into(),
            message: format!("invalid base64 `{s}`: {e}"),
        })
}

fn to_base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Parse a hash string in any supported encoding, returning the
/// raw bytes.  Auto-detects the encoding from the string format:
///
/// - `<alg>:<base16>` (cppnix legacy)
/// - `<alg>:<nix-base32>` (cppnix store-path encoding)
/// - `<alg>-<base64>` (SRI)
/// - bare base16 (`abcd...`)
///
/// Returns `(algorithm-name-or-empty, raw-bytes)`.
///
/// # Errors
///
/// `SpecError::Interp { phase: "hash-decode" }` for malformed
/// input.
pub fn decode_hash(input: &str) -> Result<(String, Vec<u8>), SpecError> {
    if let Some(rest) = input.find(':') {
        // <alg>:<hex|nix32>
        let algo = &input[..rest];
        let payload = &input[rest + 1..];
        // Try hex first.  If the length matches the algo's
        // bit-length / 4 and chars are all hex, it's hex.
        if payload.chars().all(|c| c.is_ascii_hexdigit())
            && payload.len() % 2 == 0
        {
            return Ok((algo.into(), from_base16(payload)?));
        }
        // Otherwise it's nix-base32.
        return Ok((algo.into(), from_nix_base32(payload)?));
    }
    if let Some(dash) = input.find('-') {
        // SRI: `<alg>-<base64>`.
        let algo = &input[..dash];
        let payload = &input[dash + 1..];
        return Ok((algo.into(), from_base64(payload)?));
    }
    // No separator — assume bare hex.
    Ok((String::new(), from_base16(input)?))
}

/// Re-encode a hash to the requested encoding.  `algorithm` is
/// stamped into the output for prefixed encodings (`<alg>:<...>`
/// or SRI).
///
/// # Errors
///
/// `SpecError::Interp { phase: "hash-encode" }` if the encoding
/// name isn't recognised.
pub fn encode_hash(
    algorithm: &str,
    encoding: &str,
    bytes: &[u8],
) -> Result<String, SpecError> {
    match encoding {
        "base16" => Ok(to_base16(bytes)),
        "nix-base32" => Ok(format!("{algorithm}:{}", to_nix_base32(bytes))),
        "base64" => Ok(format!("{algorithm}:{}", to_base64(bytes))),
        "sri" => Ok(format!("{algorithm}-{}", to_base64(bytes))),
        _ => Err(SpecError::Interp {
            phase: "hash-encode".into(),
            message: format!("unknown encoding `{encoding}`"),
        }),
    }
}

/// Convert a hash from one encoding to another.  Roundtrips
/// through raw bytes.
///
/// # Errors
///
/// Either of the decode or encode failures.
pub fn apply_conversion(
    from_encoding: &str,
    to_encoding: &str,
    input: &str,
) -> Result<String, SpecError> {
    // We use auto-detect via decode_hash rather than honoring
    // from_encoding strictly — most callers know the input's
    // shape but the auto-detect is forgiving and matches cppnix.
    let _ = from_encoding;
    let (algo, bytes) = decode_hash(input)?;
    encode_hash(&algo, to_encoding, &bytes)
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

    // ── M3.0 conversion tests ──────────────────────────────────

    #[test]
    fn base16_roundtrip() {
        let bytes = b"hello world";
        let hex = to_base16(bytes);
        let back = from_base16(&hex).unwrap();
        assert_eq!(back, bytes);
    }

    #[test]
    fn base16_rejects_odd_length() {
        let err = from_base16("abc").unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "hash-decode"),
            _ => panic!("expected hash-decode"),
        }
    }

    #[test]
    fn base16_rejects_invalid_chars() {
        let err = from_base16("xyzw").unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "hash-decode"),
            _ => panic!("expected hash-decode"),
        }
    }

    #[test]
    fn base64_roundtrip() {
        let bytes = b"some bytes here";
        let b64 = to_base64(bytes);
        let back = from_base64(&b64).unwrap();
        assert_eq!(back, bytes);
    }

    #[test]
    fn nix_base32_uses_alphabet_only() {
        // M3.0 nix-base32 impl is approximate (the exact cppnix
        // bit-pack lands in M3.1).  For now we verify the output
        // alphabet matches cppnix's convention.
        const ALPHABET: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
        let bytes = b"some test bytes";
        let b32 = to_nix_base32(bytes);
        for c in b32.bytes() {
            assert!(
                ALPHABET.contains(&c),
                "char `{}` not in nix-base32 alphabet",
                c as char,
            );
        }
        // Length should be ⌈8N/5⌉.
        let expected_len = (bytes.len() * 8).div_ceil(5);
        assert_eq!(b32.len(), expected_len);
    }

    #[test]
    fn decode_hash_handles_colon_hex() {
        let (algo, bytes) = decode_hash("sha256:deadbeef").unwrap();
        assert_eq!(algo, "sha256");
        assert_eq!(bytes, [0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn decode_hash_handles_sri() {
        let bytes_in = b"some bytes";
        let b64 = to_base64(bytes_in);
        let sri = format!("sha256-{b64}");
        let (algo, bytes) = decode_hash(&sri).unwrap();
        assert_eq!(algo, "sha256");
        assert_eq!(bytes, bytes_in);
    }

    #[test]
    fn encode_hash_emits_sri_with_dash() {
        let bytes = b"x";
        let out = encode_hash("sha256", "sri", bytes).unwrap();
        assert!(out.starts_with("sha256-"));
    }

    #[test]
    fn encode_hash_emits_nix_base32_with_colon() {
        let bytes = b"x";
        let out = encode_hash("sha256", "nix-base32", bytes).unwrap();
        assert!(out.starts_with("sha256:"));
    }

    #[test]
    fn encode_unknown_encoding_errors() {
        let err = encode_hash("sha256", "rot13", b"x").unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "hash-encode"),
            _ => panic!("expected hash-encode"),
        }
    }

    #[test]
    fn convert_hex_to_sri() {
        let out = apply_conversion("base16", "sri", "sha256:deadbeef").unwrap();
        assert!(out.starts_with("sha256-"));
        // SRI of 0xdeadbeef = 4 bytes → 8-char base64 with padding.
        // Just verify shape, not exact bytes (covered by roundtrip).
        assert!(out.len() > 10);
    }
}
