//! Nix hash types and encodings.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum HashError {
    #[error("unsupported hash algorithm: {0}")]
    UnsupportedAlgorithm(String),
    #[error("invalid hash encoding")]
    InvalidEncoding,
}

/// Hash algorithms supported by Nix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HashAlgorithm {
    Sha256,
    Sha512,
    Sha1,
    Md5,
}

impl HashAlgorithm {
    /// Parse from the string representation used in Nix.
    pub fn from_nix_str(s: &str) -> Result<Self, HashError> {
        s.parse()
    }

    /// The Nix string representation.
    #[must_use]
    pub fn as_nix_str(&self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
            Self::Sha1 => "sha1",
            Self::Md5 => "md5",
        }
    }

    /// Digest length in bytes.
    #[must_use]
    pub fn digest_len(&self) -> usize {
        match self {
            Self::Sha256 => 32,
            Self::Sha512 => 64,
            Self::Sha1 => 20,
            Self::Md5 => 16,
        }
    }
}

impl std::fmt::Display for HashAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_nix_str())
    }
}

impl std::str::FromStr for HashAlgorithm {
    type Err = HashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sha256" => Ok(Self::Sha256),
            "sha512" => Ok(Self::Sha512),
            "sha1" => Ok(Self::Sha1),
            "md5" => Ok(Self::Md5),
            _ => Err(HashError::UnsupportedAlgorithm(s.to_string())),
        }
    }
}

/// A typed hash value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NixHash {
    pub algorithm: HashAlgorithm,
    pub digest: Vec<u8>,
}

impl NixHash {
    /// Create a new hash from algorithm and raw digest bytes.
    pub fn new(algorithm: HashAlgorithm, digest: Vec<u8>) -> Self {
        Self { algorithm, digest }
    }

    /// Encode as `<algo>:<base16>` (Nix's default display format).
    #[must_use]
    pub fn to_nix_string(&self) -> String {
        format!("{}:{}", self.algorithm, hex::encode(&self.digest))
    }

    /// Encode as SRI format: `<algo>-<base64>`.
    #[must_use]
    pub fn to_sri(&self) -> String {
        format!("{}-{}", self.algorithm, base64_encode(&self.digest))
    }
}

impl std::fmt::Display for NixHash {
    /// Formats as the Nix string representation (`algo:hex`).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.algorithm, hex::encode(&self.digest))
    }
}

/// Base64 encode bytes (delegates to the `base64` crate).
#[must_use]
pub fn base64_encode(input: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(input)
}

/// Base64 decode a string (delegates to the `base64` crate).
pub fn base64_decode(input: &str) -> Result<Vec<u8>, HashError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .map_err(|_| HashError::InvalidEncoding)
}

/// Base64 encode bytes (alias for [`base64_encode`] kept for backward compatibility).
#[must_use]
pub fn minimal_base64_encode(input: &[u8]) -> String {
    base64_encode(input)
}

/// Minimal hex encoding (avoids external dep for now).
pub(crate) mod hex {
    /// Encode bytes as lowercase hexadecimal.
    #[must_use]
    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Decode a lowercase hexadecimal string to bytes.
    pub fn decode(s: &str) -> Result<Vec<u8>, super::HashError> {
        if s.len() % 2 != 0 {
            return Err(super::HashError::InvalidEncoding);
        }
        (0..s.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&s[i..i + 2], 16)
                    .map_err(|_| super::HashError::InvalidEncoding)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algorithm_roundtrip() {
        for algo in [HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha1, HashAlgorithm::Md5] {
            let parsed = HashAlgorithm::from_nix_str(algo.as_nix_str()).unwrap();
            assert_eq!(parsed, algo);
        }
    }

    #[test]
    fn nix_string_format() {
        let hash = NixHash::new(HashAlgorithm::Sha256, vec![0xab; 32]);
        let s = hash.to_nix_string();
        assert!(s.starts_with("sha256:"));
        assert_eq!(s.len(), 7 + 64); // "sha256:" + 64 hex chars
    }

    #[test]
    fn sri_roundtrip() {
        // Encode as SRI then parse back through base64 decode
        let digest = vec![0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
                          0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
                          0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
                          0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let hash = NixHash::new(HashAlgorithm::Sha256, digest.clone());
        let sri = hash.to_sri();
        assert!(sri.starts_with("sha256-"));
        // The base64 portion should decode back to the original digest
        let b64_part = sri.strip_prefix("sha256-").unwrap();
        // Manually verify the base64 is non-empty and well-formed
        assert!(!b64_part.is_empty());
        // 32 bytes -> ceil(32/3)*4 = 44 base64 chars
        assert_eq!(b64_part.len(), 44);
    }

    #[test]
    fn invalid_algorithm_string() {
        assert!(HashAlgorithm::from_nix_str("blake2b").is_err());
        assert!(HashAlgorithm::from_nix_str("SHA256").is_err());
        assert!(HashAlgorithm::from_nix_str("").is_err());
        assert!(HashAlgorithm::from_nix_str("sha-256").is_err());

        match HashAlgorithm::from_nix_str("unknown") {
            Err(HashError::UnsupportedAlgorithm(s)) => assert_eq!(s, "unknown"),
            other => panic!("expected UnsupportedAlgorithm, got {other:?}"),
        }
    }

    #[test]
    fn hash_digest_length_per_algorithm() {
        assert_eq!(HashAlgorithm::Sha256.digest_len(), 32);
        assert_eq!(HashAlgorithm::Sha512.digest_len(), 64);
        assert_eq!(HashAlgorithm::Sha1.digest_len(), 20);
        assert_eq!(HashAlgorithm::Md5.digest_len(), 16);
    }

    #[test]
    fn hex_encode_decode_roundtrip() {
        let original = vec![0x00, 0x11, 0x22, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let encoded = hex::encode(&original);
        assert_eq!(encoded, "001122aabbccddeeff");
        let decoded = hex::decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn hex_decode_odd_length() {
        assert!(hex::decode("abc").is_err());
    }

    #[test]
    fn hex_decode_invalid_chars() {
        assert!(hex::decode("zzzz").is_err());
        assert!(hex::decode("gg").is_err());
    }

    #[test]
    fn hex_roundtrip_all_byte_values() {
        let all_bytes: Vec<u8> = (0..=255).collect();
        let encoded = hex::encode(&all_bytes);
        let decoded = hex::decode(&encoded).unwrap();
        assert_eq!(decoded, all_bytes);
    }

    #[test]
    fn empty_digest_handling() {
        let hash = NixHash::new(HashAlgorithm::Sha256, vec![]);
        let nix_str = hash.to_nix_string();
        assert_eq!(nix_str, "sha256:");

        let sri = hash.to_sri();
        assert_eq!(sri, "sha256-");
    }

    #[test]
    fn base64_encode_known_vectors() {
        // RFC 4648 test vectors
        assert_eq!(minimal_base64_encode(b""), "");
        assert_eq!(minimal_base64_encode(b"f"), "Zg==");
        assert_eq!(minimal_base64_encode(b"fo"), "Zm8=");
        assert_eq!(minimal_base64_encode(b"foo"), "Zm9v");
        assert_eq!(minimal_base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(minimal_base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(minimal_base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn nix_string_with_all_algorithms() {
        for algo in [HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha1, HashAlgorithm::Md5] {
            let digest = vec![0xab; algo.digest_len()];
            let hash = NixHash::new(algo, digest);
            let s = hash.to_nix_string();
            let expected_prefix = format!("{}:", algo.as_nix_str());
            assert!(s.starts_with(&expected_prefix), "failed for {algo:?}");
            let hex_part = s.strip_prefix(&expected_prefix).unwrap();
            assert_eq!(hex_part.len(), algo.digest_len() * 2);
        }
    }

    // ── base64_decode ────────────────────────────────────

    #[test]
    fn base64_decode_known_vectors() {
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn base64_decode_invalid_input() {
        assert!(base64_decode("!!!invalid!!!").is_err());
    }

    #[test]
    fn base64_roundtrip_binary() {
        let data: Vec<u8> = (0..=255).collect();
        let encoded = base64_encode(&data);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    // ── SRI format ───────────────────────────────────────

    #[test]
    fn sri_format_all_algorithms() {
        for algo in [HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha1, HashAlgorithm::Md5] {
            let digest = vec![0x42; algo.digest_len()];
            let hash = NixHash::new(algo, digest);
            let sri = hash.to_sri();
            let prefix = format!("{}-", algo.as_nix_str());
            assert!(sri.starts_with(&prefix), "SRI for {algo:?} should start with {prefix}");
        }
    }

    #[test]
    fn sri_base64_decode_matches_digest() {
        let digest = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04,
                          0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C,
                          0x0D, 0x0E, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x14,
                          0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C];
        let hash = NixHash::new(HashAlgorithm::Sha256, digest.clone());
        let sri = hash.to_sri();
        let b64_part = sri.strip_prefix("sha256-").unwrap();
        let decoded = base64_decode(b64_part).unwrap();
        assert_eq!(decoded, digest);
    }

    // ── hex edge cases ───────────────────────────────────

    #[test]
    fn hex_encode_empty() {
        assert_eq!(hex::encode(&[]), "");
    }

    #[test]
    fn hex_decode_empty() {
        assert_eq!(hex::decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hex_decode_accepts_uppercase() {
        assert_eq!(hex::decode("AABB").unwrap(), vec![0xAA, 0xBB]);
    }

    // ── NixHash equality ─────────────────────────────────

    #[test]
    fn nix_hash_equality() {
        let h1 = NixHash::new(HashAlgorithm::Sha256, vec![1; 32]);
        let h2 = NixHash::new(HashAlgorithm::Sha256, vec![1; 32]);
        let h3 = NixHash::new(HashAlgorithm::Sha256, vec![2; 32]);
        let h4 = NixHash::new(HashAlgorithm::Sha1, vec![1; 20]);
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_ne!(h1, h4);
    }

    // ── HashAlgorithm Copy + Clone ───────────────────────

    #[test]
    fn hash_algorithm_is_copy() {
        let a = HashAlgorithm::Sha256;
        let b = a;
        assert_eq!(a, b);
    }

    // ── HashAlgorithm Display ────────────────────────────

    #[test]
    fn hash_algorithm_display_strings() {
        assert_eq!(format!("{}", HashAlgorithm::Sha256), "sha256");
        assert_eq!(format!("{}", HashAlgorithm::Sha512), "sha512");
        assert_eq!(format!("{}", HashAlgorithm::Sha1), "sha1");
        assert_eq!(format!("{}", HashAlgorithm::Md5), "md5");
    }

    #[test]
    fn hash_algorithm_from_str_via_parse() {
        // Tests the FromStr impl path
        let algo: HashAlgorithm = "sha256".parse().unwrap();
        assert_eq!(algo, HashAlgorithm::Sha256);

        let algo: HashAlgorithm = "sha512".parse().unwrap();
        assert_eq!(algo, HashAlgorithm::Sha512);

        let algo: HashAlgorithm = "sha1".parse().unwrap();
        assert_eq!(algo, HashAlgorithm::Sha1);

        let algo: HashAlgorithm = "md5".parse().unwrap();
        assert_eq!(algo, HashAlgorithm::Md5);

        let result: Result<HashAlgorithm, _> = "blake2b".parse();
        assert!(result.is_err());
    }

    // ── NixHash Display ──────────────────────────────────

    #[test]
    fn nix_hash_display_format_matches_to_nix_string() {
        let hash = NixHash::new(HashAlgorithm::Sha256, vec![0xab, 0xcd, 0xef]);
        let displayed = format!("{hash}");
        let manual = hash.to_nix_string();
        assert_eq!(displayed, manual);
        assert_eq!(displayed, "sha256:abcdef");
    }

    // ── NixHash::new constructor ─────────────────────────

    #[test]
    fn nix_hash_new_stores_fields() {
        let h = NixHash::new(HashAlgorithm::Sha512, vec![1, 2, 3]);
        assert_eq!(h.algorithm, HashAlgorithm::Sha512);
        assert_eq!(h.digest, vec![1, 2, 3]);
    }

    // ── minimal_base64_encode is alias ───────────────────

    #[test]
    fn minimal_base64_encode_matches_base64_encode() {
        let data = b"hello world";
        assert_eq!(minimal_base64_encode(data), base64_encode(data));
    }

    // ── base64 with padding patterns ─────────────────────

    #[test]
    fn base64_encode_no_padding_needed() {
        // 3 bytes → 4 chars, no padding
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn base64_encode_one_padding() {
        // 4 bytes → "==" pattern? actually 4 bytes → 8 chars with no padding (mod 3 = 1)
        // 1 byte → "Zg==" (2 padding)
        assert_eq!(base64_encode(b"f"), "Zg==");
    }

    #[test]
    fn base64_encode_two_padding() {
        // 2 bytes → 4 chars with 1 padding
        assert_eq!(base64_encode(b"fo"), "Zm8=");
    }

    // ── hex error variants ───────────────────────────────

    #[test]
    fn hex_decode_odd_length_error_variant() {
        match hex::decode("a") {
            Err(HashError::InvalidEncoding) => {}
            other => panic!("expected InvalidEncoding, got {other:?}"),
        }
    }

    #[test]
    fn hex_decode_invalid_chars_error_variant() {
        match hex::decode("zz") {
            Err(HashError::InvalidEncoding) => {}
            other => panic!("expected InvalidEncoding, got {other:?}"),
        }
    }

    // ── HashError Display ────────────────────────────────

    #[test]
    fn hash_error_display_strings() {
        let err = HashError::UnsupportedAlgorithm("blake3".to_string());
        let s = format!("{err}");
        assert!(s.contains("blake3"));

        let err = HashError::InvalidEncoding;
        let s = format!("{err}");
        assert!(s.contains("invalid"));
    }

    // ── digest length matches each algo ──────────────────

    #[test]
    fn digest_len_for_all_algorithms() {
        let cases = [
            (HashAlgorithm::Md5, 16),
            (HashAlgorithm::Sha1, 20),
            (HashAlgorithm::Sha256, 32),
            (HashAlgorithm::Sha512, 64),
        ];
        for (algo, expected_len) in cases {
            assert_eq!(algo.digest_len(), expected_len);
        }
    }

    // ── Roundtrip via Display + FromStr ──────────────────

    #[test]
    fn algorithm_display_roundtrip_through_from_nix_str() {
        for algo in [
            HashAlgorithm::Sha256,
            HashAlgorithm::Sha512,
            HashAlgorithm::Sha1,
            HashAlgorithm::Md5,
        ] {
            let s = format!("{algo}");
            let parsed = HashAlgorithm::from_nix_str(&s).unwrap();
            assert_eq!(parsed, algo);
        }
    }

    // ── Hex roundtrip varied lengths ─────────────────────

    #[test]
    fn hex_roundtrip_lengths_5_10_20_32_64() {
        for len in [5, 10, 20, 32, 64] {
            let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let encoded = hex::encode(&data);
            assert_eq!(encoded.len(), len * 2);
            let decoded = hex::decode(&encoded).unwrap();
            assert_eq!(decoded, data);
        }
    }

    // ── base64 roundtrip varied lengths ──────────────────

    #[test]
    fn base64_roundtrip_lengths_1_through_10() {
        for len in 1..=10 {
            let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let encoded = base64_encode(&data);
            let decoded = base64_decode(&encoded).unwrap();
            assert_eq!(decoded, data, "failed for length {len}");
        }
    }

    // ── HashAlgorithm hash + equality ────────────────────

    #[test]
    fn hash_algorithm_in_hashset() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(HashAlgorithm::Sha256);
        set.insert(HashAlgorithm::Sha256);
        set.insert(HashAlgorithm::Md5);
        assert_eq!(set.len(), 2);
        assert!(set.contains(&HashAlgorithm::Sha256));
        assert!(set.contains(&HashAlgorithm::Md5));
        assert!(!set.contains(&HashAlgorithm::Sha512));
    }

    // ── NixHash with empty digest still serializes ──────

    #[test]
    fn nix_hash_empty_digest_to_sri() {
        let hash = NixHash::new(HashAlgorithm::Md5, vec![]);
        let sri = hash.to_sri();
        assert_eq!(sri, "md5-");
    }

    #[test]
    fn nix_hash_empty_digest_display() {
        let hash = NixHash::new(HashAlgorithm::Sha1, vec![]);
        assert_eq!(format!("{hash}"), "sha1:");
    }

    // ── hex::encode known vectors ────────────────────────

    #[test]
    fn hex_encode_known_vectors() {
        assert_eq!(hex::encode(b"\x00"), "00");
        assert_eq!(hex::encode(b"\xff"), "ff");
        assert_eq!(hex::encode(b"\x01\x02\x03\x04"), "01020304");
        assert_eq!(hex::encode(b"\xde\xad\xbe\xef"), "deadbeef");
    }

    #[test]
    fn hex_encode_lowercase_only() {
        let encoded = hex::encode(&[0xAB, 0xCD, 0xEF]);
        // The output should be all lowercase
        assert_eq!(encoded, "abcdef");
        assert!(encoded.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    // ── base64_decode error variant ──────────────────────

    #[test]
    fn base64_decode_invalid_returns_invalid_encoding() {
        match base64_decode("@@@") {
            Err(HashError::InvalidEncoding) => {}
            other => panic!("expected InvalidEncoding, got {other:?}"),
        }
    }

    // ── Sha512 known sri/hex output ──────────────────────

    #[test]
    fn sha512_full_digest_sri_length() {
        let hash = NixHash::new(HashAlgorithm::Sha512, vec![0xAA; 64]);
        let sri = hash.to_sri();
        let b64 = sri.strip_prefix("sha512-").unwrap();
        // 64 bytes → ceil(64/3)*4 = 88 chars
        assert_eq!(b64.len(), 88);
    }

    #[test]
    fn sha1_full_digest_hex_length() {
        let hash = NixHash::new(HashAlgorithm::Sha1, vec![0x55; 20]);
        let hex_part = hash.to_nix_string();
        let h = hex_part.strip_prefix("sha1:").unwrap();
        assert_eq!(h.len(), 40); // 20 bytes * 2 hex chars
    }
}
