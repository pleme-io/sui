//! Nix store path parsing and computation.

use thiserror::Error;

/// Length of the hash part in a store path (32 chars in Nix base-32).
pub const STORE_PATH_HASH_LEN: usize = 32;

/// Default store directory.
pub const DEFAULT_STORE_DIR: &str = "/nix/store";

/// Nix's custom base-32 alphabet (not RFC 4648).
const NIX_BASE32_CHARS: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

#[derive(Debug, Error)]
pub enum StorePathError {
    #[error("invalid store path: {0}")]
    Invalid(String),
    #[error("invalid hash length: expected {expected}, got {got}")]
    InvalidHashLength { expected: usize, got: usize },
    #[error("invalid character in hash: {0}")]
    InvalidHashChar(char),
    #[error("empty name")]
    EmptyName,
}

/// A validated Nix store path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StorePath {
    /// The 20-byte hash digest.
    pub digest: [u8; 20],
    /// The human-readable name portion.
    pub name: String,
}

impl StorePath {
    /// Parse a store path from a string like `/nix/store/<hash>-<name>`.
    pub fn from_absolute_path(path: &str) -> Result<Self, StorePathError> {
        let rest = path
            .strip_prefix(DEFAULT_STORE_DIR)
            .and_then(|s| s.strip_prefix('/'))
            .ok_or_else(|| StorePathError::Invalid(path.to_string()))?;

        Self::from_basename(rest)
    }

    /// Parse from just the `<hash>-<name>` portion.
    pub fn from_basename(basename: &str) -> Result<Self, StorePathError> {
        if basename.len() < STORE_PATH_HASH_LEN + 2 {
            return Err(StorePathError::Invalid(basename.to_string()));
        }

        let hash_str = &basename[..STORE_PATH_HASH_LEN];
        let sep = basename.as_bytes()[STORE_PATH_HASH_LEN];
        let name = &basename[STORE_PATH_HASH_LEN + 1..];

        if sep != b'-' {
            return Err(StorePathError::Invalid(basename.to_string()));
        }
        if name.is_empty() {
            return Err(StorePathError::EmptyName);
        }

        let digest = nix_base32_decode(hash_str)?;

        Ok(Self {
            digest,
            name: name.to_string(),
        })
    }

    /// Render the full absolute path.
    pub fn to_absolute_path(&self) -> String {
        format!("{}/{}", DEFAULT_STORE_DIR, self.to_basename())
    }

    /// Render just the `<hash>-<name>` basename.
    pub fn to_basename(&self) -> String {
        format!("{}-{}", nix_base32_encode(&self.digest), self.name)
    }
}

impl std::fmt::Display for StorePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_absolute_path())
    }
}

/// Encode bytes to Nix's custom base-32 encoding.
pub fn nix_base32_encode(input: &[u8]) -> String {
    let len = (input.len() * 8 + 4) / 5;
    let mut out = vec![0u8; len];

    for i in 0..len {
        let bit_offset = i * 5;
        let byte_idx = bit_offset / 8;
        let bit_idx = bit_offset % 8;

        let mut val = u16::from(input[input.len() - 1 - byte_idx]) >> bit_idx;
        if byte_idx + 1 < input.len() {
            val |= u16::from(input[input.len() - 2 - byte_idx]) << (8 - bit_idx);
        }

        out[len - 1 - i] = NIX_BASE32_CHARS[(val & 0x1f) as usize];
    }

    String::from_utf8(out).expect("base32 chars are ASCII")
}

/// Decode Nix's custom base-32 encoding to bytes.
pub fn nix_base32_decode(input: &str) -> Result<[u8; 20], StorePathError> {
    let expected_len = 32; // 20 bytes * 8 bits / 5 bits = 32 chars
    if input.len() != expected_len {
        return Err(StorePathError::InvalidHashLength {
            expected: expected_len,
            got: input.len(),
        });
    }

    let mut bytes = [0u8; 20];

    for (i, c) in input.chars().rev().enumerate() {
        let digit = NIX_BASE32_CHARS
            .iter()
            .position(|&x| x == c as u8)
            .ok_or(StorePathError::InvalidHashChar(c))?;

        let bit_offset = i * 5;
        let byte_idx = bit_offset / 8;
        let bit_idx = bit_offset % 8;

        bytes[bytes.len() - 1 - byte_idx] |= (digit as u8) << bit_idx;
        if bit_idx > 3 && byte_idx + 1 < bytes.len() {
            bytes[bytes.len() - 2 - byte_idx] |= (digit as u8) >> (8 - bit_idx);
        }
    }

    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_absolute_path() {
        let path = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-hierarchical-0.1.0.1";
        let sp = StorePath::from_absolute_path(path).unwrap();
        assert_eq!(sp.name, "net-hierarchical-0.1.0.1");
        assert_eq!(sp.to_absolute_path(), path);
    }

    #[test]
    fn roundtrip_base32() {
        let input = [0u8; 20];
        let encoded = nix_base32_encode(&input);
        assert_eq!(encoded.len(), 32);
        let decoded = nix_base32_decode(&encoded).unwrap();
        assert_eq!(decoded, input);
    }

    #[test]
    fn reject_invalid_path() {
        assert!(StorePath::from_absolute_path("/tmp/foo").is_err());
        assert!(StorePath::from_absolute_path("/nix/store/short").is_err());
    }

    #[test]
    fn invalid_base32_chars_e_and_u() {
        // Nix base32 alphabet is "0123456789abcdfghijklmnpqrsvwxyz"
        // Letters 'e' and 'u' are NOT in the alphabet
        let with_e = "00bgd045z0d4icpbc2yye4gx48ak44la";
        assert!(nix_base32_decode(with_e).is_err());

        let with_u = "00bgd045z0d4icpbc2yyu4gx48ak44la";
        assert!(nix_base32_decode(with_u).is_err());

        // Verify that 'e' and 'u' produce InvalidHashChar errors
        match nix_base32_decode("e0000000000000000000000000000000") {
            Err(StorePathError::InvalidHashChar(c)) => assert_eq!(c, 'e'),
            other => panic!("expected InvalidHashChar('e'), got {other:?}"),
        }
        match nix_base32_decode("u0000000000000000000000000000000") {
            Err(StorePathError::InvalidHashChar(c)) => assert_eq!(c, 'u'),
            other => panic!("expected InvalidHashChar('u'), got {other:?}"),
        }
    }

    #[test]
    fn path_with_minimum_valid_name_length() {
        // Name must be at least 1 character
        let hash = nix_base32_encode(&[0u8; 20]);
        let basename = format!("{hash}-x");
        let sp = StorePath::from_basename(&basename).unwrap();
        assert_eq!(sp.name, "x");
    }

    #[test]
    fn path_with_special_characters_in_name() {
        let hash = nix_base32_encode(&[1u8; 20]);
        // Nix names can contain dots, hyphens, underscores, plus
        let basename = format!("{hash}-my-pkg_v1.2.3+git");
        let sp = StorePath::from_basename(&basename).unwrap();
        assert_eq!(sp.name, "my-pkg_v1.2.3+git");
    }

    #[test]
    fn basename_roundtrip_real_world_examples() {
        let examples = [
            "00bgd045z0d4icpbc2yyz4gx48ak44la-net-hierarchical-0.1.0.1",
            "3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37-8",
            "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        ];

        for basename in examples {
            let sp = StorePath::from_basename(basename).unwrap();
            assert_eq!(sp.to_basename(), basename, "roundtrip failed for {basename}");
        }
    }

    #[test]
    fn store_path_display_trait() {
        let path_str = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-hierarchical-0.1.0.1";
        let sp = StorePath::from_absolute_path(path_str).unwrap();
        let displayed = format!("{sp}");
        assert_eq!(displayed, path_str);
    }

    #[test]
    fn empty_name_rejected() {
        let hash = nix_base32_encode(&[0u8; 20]);
        // Construct a basename with hash + dash but no name
        let basename = format!("{hash}-");
        assert!(StorePath::from_basename(&basename).is_err());
    }

    #[test]
    fn base32_encode_decode_roundtrip_various() {
        let test_cases: [[u8; 20]; 4] = [
            [0u8; 20],
            [0xff; 20],
            [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc,
             0xba, 0x98, 0x76, 0x54, 0x32, 0x10, 0xde, 0xad, 0xbe, 0xef],
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20],
        ];

        for input in &test_cases {
            let encoded = nix_base32_encode(input);
            assert_eq!(encoded.len(), 32);
            let decoded = nix_base32_decode(&encoded).unwrap();
            assert_eq!(&decoded, input);
        }
    }

    #[test]
    fn wrong_hash_length_rejected() {
        // Too short
        assert!(nix_base32_decode("abc").is_err());
        // Too long
        assert!(nix_base32_decode("000000000000000000000000000000000").is_err());
        // Check error variant
        match nix_base32_decode("abc") {
            Err(StorePathError::InvalidHashLength { expected: 32, got: 3 }) => {}
            other => panic!("expected InvalidHashLength, got {other:?}"),
        }
    }
}
