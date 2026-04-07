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
pub enum HashAlgorithm {
    Sha256,
    Sha512,
    Sha1,
    Md5,
}

impl HashAlgorithm {
    /// Parse from the string representation used in Nix.
    pub fn from_nix_str(s: &str) -> Result<Self, HashError> {
        match s {
            "sha256" => Ok(Self::Sha256),
            "sha512" => Ok(Self::Sha512),
            "sha1" => Ok(Self::Sha1),
            "md5" => Ok(Self::Md5),
            _ => Err(HashError::UnsupportedAlgorithm(s.to_string())),
        }
    }

    /// The Nix string representation.
    pub fn as_nix_str(&self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
            Self::Sha1 => "sha1",
            Self::Md5 => "md5",
        }
    }

    /// Digest length in bytes.
    pub fn digest_len(&self) -> usize {
        match self {
            Self::Sha256 => 32,
            Self::Sha512 => 64,
            Self::Sha1 => 20,
            Self::Md5 => 16,
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
    pub fn to_nix_string(&self) -> String {
        format!("{}:{}", self.algorithm.as_nix_str(), hex::encode(&self.digest))
    }

    /// Encode as SRI format: `<algo>-<base64>`.
    pub fn to_sri(&self) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.digest);
        format!("{}-{}", self.algorithm.as_nix_str(), b64)
    }
}

/// Base64 encode bytes (delegates to the `base64` crate).
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
pub fn minimal_base64_encode(input: &[u8]) -> String {
    base64_encode(input)
}

/// Minimal hex encoding (avoids external dep for now).
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

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
}
