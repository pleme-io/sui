//! Content-addressed store path types.
//!
//! Nix supports several content-addressing methods for store paths.

use crate::hash::{hex, HashAlgorithm, NixHash};
use crate::store_path::{compress_hash, StorePath, StorePathError};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContentAddressError {
    #[error("invalid content address format: {0}")]
    InvalidFormat(String),
    #[error("store path error: {0}")]
    StorePath(#[from] StorePathError),
}

/// Content-addressing method.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContentAddressMethod {
    /// Text content (for derivation files and string-to-store).
    /// Format: `text:<algo>:<hash>`
    Text,
    /// Flat file hashing (no NAR wrapping).
    /// Format: `fixed:out:<algo>:<hash>`
    Flat,
    /// Recursive NAR hashing.
    /// Format: `fixed:out:r:<algo>:<hash>`
    Recursive,
}

impl std::fmt::Display for ContentAddressMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text => f.write_str("text"),
            Self::Flat => f.write_str("flat"),
            Self::Recursive => f.write_str("recursive"),
        }
    }
}

impl std::str::FromStr for ContentAddressMethod {
    type Err = ContentAddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "flat" => Ok(Self::Flat),
            "recursive" => Ok(Self::Recursive),
            _ => Err(ContentAddressError::InvalidFormat(s.to_string())),
        }
    }
}

/// A content address assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentAddress {
    pub method: ContentAddressMethod,
    pub hash: NixHash,
}

impl ContentAddress {
    /// Parse from the string format used in NarInfo CA field.
    pub fn parse(s: &str) -> Result<Self, ContentAddressError> {
        if let Some(rest) = s.strip_prefix("text:") {
            let hash = parse_hash_with_algo(rest)?;
            Ok(Self {
                method: ContentAddressMethod::Text,
                hash,
            })
        } else if let Some(rest) = s.strip_prefix("fixed:out:r:") {
            let hash = parse_hash_with_algo(rest)?;
            Ok(Self {
                method: ContentAddressMethod::Recursive,
                hash,
            })
        } else if let Some(rest) = s.strip_prefix("fixed:out:") {
            let hash = parse_hash_with_algo(rest)?;
            Ok(Self {
                method: ContentAddressMethod::Flat,
                hash,
            })
        } else {
            Err(ContentAddressError::InvalidFormat(s.to_string()))
        }
    }

    /// Serialize to the string format.
    #[must_use]
    pub fn to_nix_string(&self) -> String {
        let prefix = match self.method {
            ContentAddressMethod::Text => "text:",
            ContentAddressMethod::Flat => "fixed:out:",
            ContentAddressMethod::Recursive => "fixed:out:r:",
        };
        format!("{}{}", prefix, self.hash.to_nix_string())
    }
}

impl std::fmt::Display for ContentAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_nix_string())
    }
}

/// Compute a store path for text content (like `builtins.toFile`).
///
/// The fingerprint is: `text:<sha256hash>:<references...>:/nix/store:<name>`
pub fn compute_text_store_path(
    name: &str,
    contents: &[u8],
    references: &[String],
) -> Result<StorePath, StorePathError> {
    let content_hash = Sha256::digest(contents);

    let mut fingerprint = String::from("text:sha256:");
    fingerprint.push_str(&hex::encode(&content_hash));
    for r in references {
        fingerprint.push(':');
        fingerprint.push_str(r);
    }
    fingerprint.push_str(":/nix/store:");
    fingerprint.push_str(name);

    let path_hash = compress_hash(&Sha256::digest(fingerprint.as_bytes()), 20);
    let digest: [u8; 20] = path_hash.try_into().map_err(|_| StorePathError::InvalidHashLength {
        expected: 20,
        got: 0, // compress_hash guarantees length, so this branch is unreachable
    })?;

    Ok(StorePath {
        digest,
        name: name.to_string(),
    })
}

/// Parse `<algo>:<hex-hash>` format.
fn parse_hash_with_algo(s: &str) -> Result<NixHash, ContentAddressError> {
    let (algo_str, hash_hex) = s
        .split_once(':')
        .ok_or_else(|| ContentAddressError::InvalidFormat(s.to_string()))?;

    let algorithm = HashAlgorithm::from_nix_str(algo_str)
        .map_err(|e| ContentAddressError::InvalidFormat(e.to_string()))?;

    let digest = hex::decode(hash_hex)
        .map_err(|_| ContentAddressError::InvalidFormat(format!("invalid hex: {hash_hex}")))?;

    Ok(NixHash::new(algorithm, digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_ca() {
        let ca = ContentAddress::parse("text:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855").unwrap();
        assert_eq!(ca.method, ContentAddressMethod::Text);
        assert_eq!(ca.hash.algorithm, HashAlgorithm::Sha256);
    }

    #[test]
    fn parse_fixed_flat() {
        let ca = ContentAddress::parse("fixed:out:sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789").unwrap();
        assert_eq!(ca.method, ContentAddressMethod::Flat);
    }

    #[test]
    fn parse_fixed_recursive() {
        let ca = ContentAddress::parse("fixed:out:r:sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789").unwrap();
        assert_eq!(ca.method, ContentAddressMethod::Recursive);
    }

    #[test]
    fn roundtrip_ca() {
        let ca = ContentAddress {
            method: ContentAddressMethod::Recursive,
            hash: NixHash::new(HashAlgorithm::Sha256, vec![0xab; 32]),
        };
        let s = ca.to_nix_string();
        let parsed = ContentAddress::parse(&s).unwrap();
        assert_eq!(parsed, ca);
    }

    #[test]
    fn text_store_path_deterministic() {
        let path1 = compute_text_store_path("test.txt", b"hello", &[]).unwrap();
        let path2 = compute_text_store_path("test.txt", b"hello", &[]).unwrap();
        assert_eq!(path1, path2);

        // Different content produces different path
        let path3 = compute_text_store_path("test.txt", b"world", &[]).unwrap();
        assert_ne!(path1.digest, path3.digest);
    }

    #[test]
    fn text_store_path_format() {
        let path = compute_text_store_path("hello.txt", b"Hello, World!", &[]).unwrap();
        let abs = path.to_absolute_path();
        assert!(abs.starts_with("/nix/store/"));
        assert!(abs.ends_with("-hello.txt"));
        // Hash portion should be 32 chars
        let basename = abs.strip_prefix("/nix/store/").unwrap();
        let hash_part = &basename[..32];
        assert_eq!(hash_part.len(), 32);
    }

    #[test]
    fn compress_hash_xor_fold() {
        let hash = vec![0xff; 32];
        let compressed = compress_hash(&hash, 20);
        assert_eq!(compressed.len(), 20);
        // 32 bytes XOR-folded to 20: first 12 bytes get XOR'd with bytes 20-31
        // 0xff ^ 0xff = 0 for those 12, rest stays 0xff
        for &b in &compressed[..12] {
            assert_eq!(b, 0);
        }
        for &b in &compressed[12..] {
            assert_eq!(b, 0xff);
        }
    }

    #[test]
    fn invalid_format() {
        assert!(ContentAddress::parse("garbage").is_err());
        assert!(ContentAddress::parse("text:").is_err());
        assert!(ContentAddress::parse("fixed:out:badformat").is_err());
    }

    #[test]
    fn all_three_ca_method_types_roundtrip() {
        let methods = [
            (ContentAddressMethod::Text, "text:"),
            (ContentAddressMethod::Flat, "fixed:out:"),
            (ContentAddressMethod::Recursive, "fixed:out:r:"),
        ];
        for (method, expected_prefix) in methods {
            let ca = ContentAddress {
                method: method.clone(),
                hash: NixHash::new(HashAlgorithm::Sha256, vec![0xcd; 32]),
            };
            let s = ca.to_nix_string();
            assert!(s.starts_with(expected_prefix), "failed for {method:?}: {s}");
            let parsed = ContentAddress::parse(&s).unwrap();
            assert_eq!(parsed, ca);
        }
    }

    #[test]
    fn invalid_prefix_error() {
        match ContentAddress::parse("nope:sha256:abc") {
            Err(ContentAddressError::InvalidFormat(s)) => {
                assert_eq!(s, "nope:sha256:abc");
            }
            other => panic!("expected InvalidFormat, got {other:?}"),
        }

        // "fixed:" without "out:" is invalid
        match ContentAddress::parse("fixed:sha256:abc") {
            Err(ContentAddressError::InvalidFormat(_)) => {}
            other => panic!("expected InvalidFormat, got {other:?}"),
        }
    }

    #[test]
    fn hash_with_all_algorithms() {
        let algos = [
            (HashAlgorithm::Sha256, 32),
            (HashAlgorithm::Sha512, 64),
            (HashAlgorithm::Sha1, 20),
            (HashAlgorithm::Md5, 16),
        ];
        for (algo, digest_len) in algos {
            let ca = ContentAddress {
                method: ContentAddressMethod::Recursive,
                hash: NixHash::new(algo, vec![0x42; digest_len]),
            };
            let s = ca.to_nix_string();
            let parsed = ContentAddress::parse(&s).unwrap();
            assert_eq!(parsed.hash.algorithm, algo);
            assert_eq!(parsed.hash.digest.len(), digest_len);
            assert_eq!(parsed, ca);
        }
    }

    #[test]
    fn text_store_path_with_references() {
        let refs = vec![
            "/nix/store/aaa-glibc-2.37".to_string(),
            "/nix/store/bbb-bash-5.2".to_string(),
        ];
        let path = compute_text_store_path("test.txt", b"hello", &refs).unwrap();
        let abs = path.to_absolute_path();
        assert!(abs.starts_with("/nix/store/"));
        assert!(abs.ends_with("-test.txt"));

        // Different references should produce a different path
        let path_no_refs = compute_text_store_path("test.txt", b"hello", &[]).unwrap();
        assert_ne!(path.digest, path_no_refs.digest);
    }

    // ── compute_text_store_path → StorePath can be used with Store ─

    #[test]
    fn text_store_path_roundtrips_through_absolute_path() {
        let sp = compute_text_store_path("my-config.txt", b"config data", &[]).unwrap();
        let abs = sp.to_absolute_path();

        // Parse it back — verifies the StorePath is valid
        let reparsed = StorePath::from_absolute_path(&abs).unwrap();
        assert_eq!(reparsed.name, "my-config.txt");
        assert_eq!(reparsed.digest, sp.digest);
        assert_eq!(reparsed.to_absolute_path(), abs);
    }

    #[test]
    fn text_store_path_basename_roundtrip() {
        let sp = compute_text_store_path("script.sh", b"#!/bin/sh\necho hi", &[]).unwrap();
        let basename = sp.to_basename();

        let reparsed = StorePath::from_basename(&basename).unwrap();
        assert_eq!(reparsed, sp);
    }

    // ── ContentAddress parse → roundtrip through NarInfo ────

    #[test]
    fn content_address_roundtrip_through_narinfo() {
        use crate::narinfo::NarInfo;

        let ca_str = "fixed:out:r:sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let ca = ContentAddress::parse(ca_str).unwrap();

        // Put CA into a NarInfo, serialize, reparse, extract CA
        let narinfo = NarInfo {
            store_path: "/nix/store/abc-test".to_string(),
            url: "nar/test.nar".to_string(),
            compression: "none".to_string(),
            file_hash: "sha256:000".to_string(),
            file_size: 100,
            nar_hash: "sha256:111".to_string(),
            nar_size: 200,
            references: vec![],
            deriver: None,
            signatures: vec![],
            ca: Some(ca.to_nix_string()),
        };

        let serialized = narinfo.serialize();
        let reparsed = NarInfo::parse(&serialized).unwrap();
        let ca_reparsed = ContentAddress::parse(reparsed.ca.as_ref().unwrap()).unwrap();

        assert_eq!(ca_reparsed.method, ca.method);
        assert_eq!(ca_reparsed.hash.algorithm, ca.hash.algorithm);
        assert_eq!(ca_reparsed.hash.digest, ca.hash.digest);
    }

    #[test]
    fn compute_text_store_path_different_names_differ() {
        let p1 = compute_text_store_path("a.txt", b"same", &[]).unwrap();
        let p2 = compute_text_store_path("b.txt", b"same", &[]).unwrap();
        assert_ne!(p1.digest, p2.digest);
        assert_ne!(p1.name, p2.name);
    }

    #[test]
    fn compute_text_store_path_empty_content() {
        let sp = compute_text_store_path("empty", b"", &[]).unwrap();
        let abs = sp.to_absolute_path();
        assert!(abs.starts_with("/nix/store/"));
        assert!(abs.ends_with("-empty"));
    }

    #[test]
    fn parse_content_address_missing_hash() {
        assert!(ContentAddress::parse("text:sha256:").is_ok());
        assert!(ContentAddress::parse("text:sha256").is_err());
    }

    #[test]
    fn content_address_method_display() {
        let text_ca = ContentAddress {
            method: ContentAddressMethod::Text,
            hash: NixHash::new(HashAlgorithm::Sha256, vec![0; 32]),
        };
        let s = text_ca.to_nix_string();
        assert!(s.starts_with("text:sha256:"));
    }

    #[test]
    fn text_content_address_roundtrip_through_narinfo() {
        use crate::narinfo::NarInfo;

        let ca_str = "text:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let ca = ContentAddress::parse(ca_str).unwrap();
        assert_eq!(ca.method, ContentAddressMethod::Text);

        let narinfo = NarInfo {
            store_path: "/nix/store/empty-text".to_string(),
            url: "nar/empty.nar".to_string(),
            compression: "none".to_string(),
            file_hash: "sha256:000".to_string(),
            file_size: 0,
            nar_hash: "sha256:000".to_string(),
            nar_size: 0,
            references: vec![],
            deriver: None,
            signatures: vec![],
            ca: Some(ca.to_nix_string()),
        };

        let serialized = narinfo.serialize();
        let reparsed = NarInfo::parse(&serialized).unwrap();
        let ca_reparsed = ContentAddress::parse(reparsed.ca.as_ref().unwrap()).unwrap();
        assert_eq!(ca_reparsed, ca);
    }

    #[test]
    fn flat_content_address_roundtrip_through_narinfo() {
        use crate::narinfo::NarInfo;

        let ca = ContentAddress {
            method: ContentAddressMethod::Flat,
            hash: NixHash::new(HashAlgorithm::Sha256, vec![0x42; 32]),
        };

        let narinfo = NarInfo {
            store_path: "/nix/store/flat-file".to_string(),
            url: "nar/flat.nar".to_string(),
            compression: "none".to_string(),
            file_hash: "sha256:000".to_string(),
            file_size: 100,
            nar_hash: "sha256:000".to_string(),
            nar_size: 100,
            references: vec![],
            deriver: None,
            signatures: vec![],
            ca: Some(ca.to_nix_string()),
        };

        let serialized = narinfo.serialize();
        let reparsed = NarInfo::parse(&serialized).unwrap();
        let ca_reparsed = ContentAddress::parse(reparsed.ca.as_ref().unwrap()).unwrap();
        assert_eq!(ca_reparsed, ca);
    }
}
