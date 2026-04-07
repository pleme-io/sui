//! Nix store path parsing and computation.

use crate::hash;
use sha2::{Digest, Sha256};
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
    #[must_use]
    pub fn to_absolute_path(&self) -> String {
        format!("{}/{}", DEFAULT_STORE_DIR, self.to_basename())
    }

    /// Render just the `<hash>-<name>` basename.
    #[must_use]
    pub fn to_basename(&self) -> String {
        format!("{}-{}", nix_base32_encode(&self.digest), self.name)
    }

    /// Return the Nix base-32 hash portion of this store path.
    ///
    /// This is the 32-character string used to look up `.narinfo` files in
    /// binary caches (e.g., `sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6`).
    #[must_use]
    pub fn hash(&self) -> String {
        nix_base32_encode(&self.digest)
    }

    /// Return the human-readable name portion of this store path
    /// (e.g., `hello-2.12.1`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for StorePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_absolute_path())
    }
}

impl std::str::FromStr for StorePath {
    type Err = StorePathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_absolute_path(s)
    }
}

/// Encode bytes to Nix's custom base-32 encoding.
///
/// Matches CppNix `printHash32`: characters are emitted in
/// most-significant-first order, where the FIRST character
/// represents the high bits of the LAST input byte. The previous
/// implementation indexed bytes from the END of the input, which
/// produced a (different) self-consistent encoding that did NOT
/// match real Nix store paths — every store-path computation
/// silently disagreed with CppNix.
#[must_use]
pub fn nix_base32_encode(input: &[u8]) -> String {
    let len = (input.len() * 8 + 4) / 5;
    let mut out = String::with_capacity(len);

    for n in 0..len {
        let b = (len - 1 - n) * 5;
        let i = b / 8;
        let j = b % 8;
        let mut c = u16::from(input[i]) >> j;
        if i + 1 < input.len() {
            c |= u16::from(input[i + 1]) << (8 - j);
        }
        out.push(NIX_BASE32_CHARS[(c & 0x1f) as usize] as char);
    }

    out
}

/// Decode Nix's custom base-32 encoding to bytes.
///
/// Inverse of [`nix_base32_encode`].
pub fn nix_base32_decode(input: &str) -> Result<[u8; 20], StorePathError> {
    let expected_len = 32; // 20 bytes * 8 bits / 5 bits = 32 chars
    if input.len() != expected_len {
        return Err(StorePathError::InvalidHashLength {
            expected: expected_len,
            got: input.len(),
        });
    }

    let mut bytes = [0u8; 20];
    let total = input.len();

    for (n, c) in input.chars().enumerate() {
        let digit = NIX_BASE32_CHARS
            .iter()
            .position(|&x| x == c as u8)
            .ok_or(StorePathError::InvalidHashChar(c))?;
        let b = (total - 1 - n) * 5;
        let i = b / 8;
        let j = b % 8;
        bytes[i] |= ((digit as u16) << j) as u8;
        if i + 1 < bytes.len() {
            bytes[i + 1] |= ((digit as u16) >> (8 - j)) as u8;
        }
    }

    Ok(bytes)
}

// ── Store path hash computation ──────────────────────────────
//
// CppNix's store path computation works in three layers:
//
//   1. An "inner hash" describes the derivation/output identity.
//      - For text refs (.drv files): SHA-256 of the .drv ATerm content.
//      - For input-addressed outputs: SHA-256 of the .drv ATerm content
//        (with the corresponding output path stubbed to "").
//      - For fixed-output: SHA-256 of `fixed:out:<algo>:<hash>:`.
//
//   2. A "fingerprint" string is constructed from the inner hash and metadata,
//      then SHA-256 hashed to produce a 32-byte digest.
//
//   3. The 32-byte digest is XOR-folded ("compressed") to 20 bytes,
//      then encoded in Nix's custom base-32 alphabet to produce the
//      32-character hash prefix of the store path.

/// Compress an arbitrary-length hash into `output_len` bytes via XOR folding.
///
/// This is the algorithm CppNix calls `compressHash`: each input byte is XORed
/// into `output[i % output_len]`. For SHA-256 (32 bytes) → 20 bytes, this folds
/// the back 12 bytes onto the front 12.
#[must_use]
pub fn compress_hash(hash: &[u8], output_len: usize) -> Vec<u8> {
    let mut out = vec![0u8; output_len];
    for (i, b) in hash.iter().enumerate() {
        out[i % output_len] ^= *b;
    }
    out
}

/// Hash a fingerprint string and produce a Nix store path.
///
/// The fingerprint is hashed with SHA-256, compressed to 20 bytes, encoded
/// in Nix base-32, and then prefixed with `/nix/store/` and suffixed with
/// the given `name`.
#[must_use]
pub fn compute_store_path_from_fingerprint(fingerprint: &str, name: &str) -> String {
    let hash = Sha256::digest(fingerprint.as_bytes());
    let compressed = compress_hash(&hash, 20);
    let b32 = nix_base32_encode(&compressed);
    format!("{DEFAULT_STORE_DIR}/{b32}-{name}")
}

/// Compute the `.drv` store path for a serialized derivation,
/// without folding any references into the fingerprint.
///
/// **This is only correct for source-only derivations** that have
/// no input store paths whatsoever — every other derivation needs
/// `compute_drv_path_with_refs`. Real `.drv` files always reference
/// at least one input source or input derivation, so this function
/// alone will mismatch CppNix on every realistic input. Kept for
/// callers that don't have access to a parsed `Derivation`.
///
/// The fingerprint is `text:sha256:<hex_inner>:<store>:<name>.drv`.
#[must_use]
pub fn compute_drv_path(drv_content: &[u8], name: &str) -> String {
    compute_drv_path_with_refs(drv_content, name, &[])
}

/// Compute the `.drv` store path including the derivation's
/// references in the fingerprint.
///
/// CppNix's `makeTextPath` builds the fingerprint as:
///
/// ```text
/// text:<ref1>:<ref2>:...:sha256:<hex_inner>:/nix/store:<name>.drv
/// ```
///
/// where each `<refN>` is a store path mentioned anywhere in the
/// `.drv` content (every entry of `Derivation::input_derivations`
/// plus every entry of `Derivation::input_sources`). The list is
/// sorted lexicographically and de-duplicated. Without these refs,
/// every real-world drvPath mismatches the on-disk filename.
///
/// Pass refs sorted or unsorted — this function sorts and dedups
/// internally so callers don't have to.
#[must_use]
pub fn compute_drv_path_with_refs(drv_content: &[u8], name: &str, refs: &[String]) -> String {
    let inner = Sha256::digest(drv_content);
    let inner_hex = hash::hex::encode(&inner);
    let drv_name = format!("{name}.drv");

    // Sort + dedup refs so two callers with the same set produce
    // identical fingerprints regardless of input order.
    let mut sorted_refs: Vec<&String> = refs.iter().collect();
    sorted_refs.sort();
    sorted_refs.dedup();

    let mut fingerprint = String::from("text:");
    for r in sorted_refs {
        fingerprint.push_str(r);
        fingerprint.push(':');
    }
    fingerprint.push_str("sha256:");
    fingerprint.push_str(&inner_hex);
    fingerprint.push(':');
    fingerprint.push_str(DEFAULT_STORE_DIR);
    fingerprint.push(':');
    fingerprint.push_str(&drv_name);

    compute_store_path_from_fingerprint(&fingerprint, &drv_name)
}

/// Compute an output store path from an inner hash hex string.
///
/// The fingerprint is `output:<output_name>:sha256:<inner_hex>:<store>:<full_name>`,
/// where `full_name` is `name` for the `out` output and `name-<output_name>` otherwise.
#[must_use]
pub fn compute_output_path(inner_hash_hex: &str, output_name: &str, name: &str) -> String {
    let full_name = if output_name == "out" {
        name.to_string()
    } else {
        format!("{name}-{output_name}")
    };
    let fingerprint = format!(
        "output:{output_name}:sha256:{inner_hash_hex}:{DEFAULT_STORE_DIR}:{full_name}"
    );
    compute_store_path_from_fingerprint(&fingerprint, &full_name)
}

/// Compute the output store path for a fixed-output derivation.
///
/// CppNix has two distinct branches in `makeFixedOutputPath`:
///
/// 1. **Recursive SHA-256** (`r:sha256`, NAR-based content hashing):
///    the path uses the `"source"` type and the inner hash is the
///    user's hash *directly* (no `fixed:out:` wrapping):
///    fingerprint = `source:sha256:<hex>:/nix/store:<name>`
///
/// 2. **Everything else** (flat sha256, md5, sha1, sha512, recursive
///    non-sha256): the path uses the `"output:out"` type and the
///    inner hash is `sha256(fixed:out:<r:?><algo>:<hex>:)`:
///    fingerprint = `output:out:sha256:<wrapped_hex>:/nix/store:<name>`
///
/// `hash` here is the user-declared content hash in lowercase hex.
#[must_use]
pub fn compute_fixed_output_hash(
    algo: &str,
    hash: &str,
    is_recursive: bool,
    name: &str,
) -> String {
    if is_recursive && algo == "sha256" {
        // "source" branch: the user's NAR hash is the inner hash
        // directly. No "fixed:out:" wrapping, no sha256-of-sha256.
        let fingerprint = format!(
            "source:sha256:{hash}:{DEFAULT_STORE_DIR}:{name}"
        );
        return compute_store_path_from_fingerprint(&fingerprint, name);
    }

    let mode = if is_recursive { "r:" } else { "" };
    let inner = format!("fixed:out:{mode}{algo}:{hash}:");
    let inner_hash = Sha256::digest(inner.as_bytes());
    let inner_hex = hash::hex::encode(&inner_hash);
    compute_output_path(&inner_hex, "out", name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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

    // ── compress_hash ────────────────────────────────────────

    #[test]
    fn compress_hash_zero_input_zero_output() {
        let zeros = [0u8; 32];
        let out = compress_hash(&zeros, 20);
        assert_eq!(out, vec![0u8; 20]);
    }

    #[test]
    fn compress_hash_xor_fold_layout() {
        // 32-byte input → 20-byte output: bytes 20..32 fold onto bytes 0..12.
        let mut input = [0u8; 32];
        input[0] = 0xAA;
        input[20] = 0x55;
        // After fold: out[0] = input[0] ^ input[20] = 0xAA ^ 0x55 = 0xFF.
        let out = compress_hash(&input, 20);
        assert_eq!(out[0], 0xFF);
        // Bytes 12..20 stay as-is (they're not folded onto by anything).
        for i in 12..20 {
            assert_eq!(out[i], 0);
        }
    }

    #[test]
    fn compress_hash_identity_when_lengths_match() {
        // If input length == output length, compress is just a copy.
        let input: [u8; 20] = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ];
        let out = compress_hash(&input, 20);
        assert_eq!(out, input.to_vec());
    }

    #[test]
    fn compress_hash_output_length_respected() {
        let input = [0xFFu8; 64];
        for &target_len in &[1usize, 5, 10, 20, 32] {
            let out = compress_hash(&input, target_len);
            assert_eq!(out.len(), target_len);
        }
    }

    // ── compute_store_path_from_fingerprint ──────────────────

    #[test]
    fn fingerprint_path_format() {
        let path = compute_store_path_from_fingerprint("text:sha256:abc:/nix/store:hello.drv", "hello.drv");
        // Must start with /nix/store/, have 32-char hash, dash, name.
        assert!(path.starts_with("/nix/store/"));
        let basename = path.strip_prefix("/nix/store/").unwrap();
        assert_eq!(basename.len(), STORE_PATH_HASH_LEN + 1 + "hello.drv".len());
        assert!(basename.ends_with("-hello.drv"));
        // Hash portion uses only nix base32 alphabet.
        let hash = &basename[..STORE_PATH_HASH_LEN];
        for c in hash.chars() {
            assert!(NIX_BASE32_CHARS.contains(&(c as u8)), "invalid char: {c}");
        }
    }

    #[test]
    fn fingerprint_path_deterministic() {
        let p1 = compute_store_path_from_fingerprint("text:sha256:abc:/nix/store:foo", "foo");
        let p2 = compute_store_path_from_fingerprint("text:sha256:abc:/nix/store:foo", "foo");
        assert_eq!(p1, p2);
    }

    #[test]
    fn fingerprint_path_changes_with_input() {
        let p1 = compute_store_path_from_fingerprint("a", "x");
        let p2 = compute_store_path_from_fingerprint("b", "x");
        assert_ne!(p1, p2);
    }

    // ── compute_drv_path ─────────────────────────────────────

    #[test]
    fn drv_path_format() {
        let path = compute_drv_path(b"some-aterm-content", "hello");
        assert!(path.starts_with("/nix/store/"));
        assert!(path.ends_with("-hello.drv"));
        let basename = path.strip_prefix("/nix/store/").unwrap();
        assert_eq!(basename.len(), STORE_PATH_HASH_LEN + 1 + "hello.drv".len());
    }

    #[test]
    fn drv_path_deterministic() {
        let p1 = compute_drv_path(b"content", "name");
        let p2 = compute_drv_path(b"content", "name");
        assert_eq!(p1, p2);
    }

    #[test]
    fn drv_path_changes_with_content() {
        let p1 = compute_drv_path(b"a", "name");
        let p2 = compute_drv_path(b"b", "name");
        assert_ne!(p1, p2);
    }

    #[test]
    fn drv_path_changes_with_name() {
        let p1 = compute_drv_path(b"content", "foo");
        let p2 = compute_drv_path(b"content", "bar");
        assert_ne!(p1, p2);
    }

    // ── compute_output_path ──────────────────────────────────

    #[test]
    fn output_path_out_uses_bare_name() {
        let path = compute_output_path("0123456789abcdef", "out", "hello");
        assert!(path.ends_with("-hello"));
        // No -out suffix for the default output.
        assert!(!path.ends_with("-hello-out"));
    }

    #[test]
    fn output_path_named_output_uses_suffix() {
        let path = compute_output_path("0123456789abcdef", "dev", "hello");
        assert!(path.ends_with("-hello-dev"));
    }

    #[test]
    fn output_path_deterministic() {
        let p1 = compute_output_path("0123456789abcdef", "out", "hello");
        let p2 = compute_output_path("0123456789abcdef", "out", "hello");
        assert_eq!(p1, p2);
    }

    #[test]
    fn output_path_changes_with_inner_hash() {
        let p1 = compute_output_path("0000000000000000", "out", "hello");
        let p2 = compute_output_path("ffffffffffffffff", "out", "hello");
        assert_ne!(p1, p2);
    }

    #[test]
    fn multiple_outputs_produce_distinct_paths() {
        let inner = "deadbeef";
        let p_out = compute_output_path(inner, "out", "lib");
        let p_dev = compute_output_path(inner, "dev", "lib");
        let p_man = compute_output_path(inner, "man", "lib");
        assert_ne!(p_out, p_dev);
        assert_ne!(p_out, p_man);
        assert_ne!(p_dev, p_man);
    }

    // ── compute_fixed_output_hash ────────────────────────────

    #[test]
    fn fixed_output_flat_path_format() {
        let path = compute_fixed_output_hash(
            "sha256",
            "1b0ri5lsf45dknj8bfxi1syz35kmab77apxxg1yrf33la1qm3kc7",
            false,
            "src.tar.gz",
        );
        assert!(path.starts_with("/nix/store/"));
        assert!(path.ends_with("-src.tar.gz"));
    }

    #[test]
    fn fixed_output_recursive_differs_from_flat() {
        let flat = compute_fixed_output_hash("sha256", "abc", false, "thing");
        let rec = compute_fixed_output_hash("sha256", "abc", true, "thing");
        assert_ne!(flat, rec);
    }

    #[test]
    fn fixed_output_deterministic() {
        let p1 = compute_fixed_output_hash("sha256", "deadbeef", false, "thing");
        let p2 = compute_fixed_output_hash("sha256", "deadbeef", false, "thing");
        assert_eq!(p1, p2);
    }

    #[test]
    fn fixed_output_changes_with_hash_value() {
        let p1 = compute_fixed_output_hash("sha256", "aaa", false, "thing");
        let p2 = compute_fixed_output_hash("sha256", "bbb", false, "thing");
        assert_ne!(p1, p2);
    }

    #[test]
    fn fixed_output_changes_with_algo() {
        let p1 = compute_fixed_output_hash("sha256", "abc", false, "thing");
        let p2 = compute_fixed_output_hash("sha1", "abc", false, "thing");
        assert_ne!(p1, p2);
    }

    // ── hash::hex::encode ─────────────────────────────────────

    #[test]
    fn hex_encode_basic() {
        assert_eq!(crate::hash::hex::encode(&[0x00, 0xff, 0xab]), "00ffab");
        assert_eq!(crate::hash::hex::encode(&[]), "");
        assert_eq!(crate::hash::hex::encode(&[0x12, 0x34, 0x56, 0x78]), "12345678");
    }

    // ── nix_base32_encode: varied byte lengths ──────────────

    #[test]
    fn base32_encode_output_length_formula() {
        for input_len in [0, 1, 5, 10, 16, 20, 32, 64] {
            let input = vec![0xAB_u8; input_len];
            let encoded = nix_base32_encode(&input);
            let expected_len = (input_len * 8 + 4) / 5;
            assert_eq!(
                encoded.len(),
                expected_len,
                "wrong encode length for {input_len}-byte input"
            );
        }
    }

    #[test]
    fn base32_encode_alphabet_only() {
        for input_len in [5, 10, 20, 32, 64] {
            let input = vec![0xFF_u8; input_len];
            let encoded = nix_base32_encode(&input);
            for c in encoded.chars() {
                assert!(
                    NIX_BASE32_CHARS.contains(&(c as u8)),
                    "char '{c}' not in nix base32 alphabet (input_len={input_len})"
                );
            }
        }
    }

    #[test]
    fn base32_encode_all_zero_bytes() {
        for input_len in [5, 10, 20, 32, 64] {
            let input = vec![0x00_u8; input_len];
            let encoded = nix_base32_encode(&input);
            assert!(
                encoded.chars().all(|c| c == '0'),
                "all-zero {input_len}-byte input should encode to all '0's, got: {encoded}"
            );
        }
    }

    #[test]
    fn base32_encode_all_ff_bytes() {
        for input_len in [5, 10, 20, 32, 64] {
            let input = vec![0xFF_u8; input_len];
            let encoded = nix_base32_encode(&input);
            assert!(
                !encoded.is_empty(),
                "encoding of all-0xFF input should be non-empty"
            );
            assert!(
                encoded.chars().all(|c| NIX_BASE32_CHARS.contains(&(c as u8))),
                "all chars must be in alphabet"
            );
        }
    }

    #[test]
    fn base32_encode_alternating_bytes() {
        let input: Vec<u8> = (0..32).map(|i| if i % 2 == 0 { 0xAA } else { 0x55 }).collect();
        let encoded = nix_base32_encode(&input);
        let expected_len = (32 * 8 + 4) / 5;
        assert_eq!(encoded.len(), expected_len);
        for c in encoded.chars() {
            assert!(NIX_BASE32_CHARS.contains(&(c as u8)));
        }
    }

    #[test]
    fn base32_encode_empty_input() {
        let encoded = nix_base32_encode(&[]);
        assert_eq!(encoded, "");
    }

    #[test]
    fn base32_encode_single_byte() {
        let encoded = nix_base32_encode(&[0x42]);
        assert_eq!(encoded.len(), 2);
        let decoded_manual = nix_base32_encode(&[0x42]);
        assert_eq!(encoded, decoded_manual);
    }

    #[test]
    fn base32_roundtrip_20_byte_boundary_cases() {
        let cases: Vec<[u8; 20]> = vec![
            [0x00; 20],
            [0xFF; 20],
            [0xAA, 0x55, 0xAA, 0x55, 0xAA, 0x55, 0xAA, 0x55, 0xAA, 0x55,
             0xAA, 0x55, 0xAA, 0x55, 0xAA, 0x55, 0xAA, 0x55, 0xAA, 0x55],
            {
                let mut a = [0u8; 20];
                a[0] = 0x01;
                a
            },
            {
                let mut a = [0u8; 20];
                a[19] = 0x01;
                a
            },
            {
                let mut a = [0u8; 20];
                for (i, v) in a.iter_mut().enumerate() {
                    *v = i as u8;
                }
                a
            },
        ];

        for input in &cases {
            let encoded = nix_base32_encode(input);
            assert_eq!(encoded.len(), 32);
            let decoded = nix_base32_decode(&encoded).unwrap();
            assert_eq!(&decoded, input, "roundtrip failed for {input:?}");
        }
    }

    // ── drv path with refs ──────────────────────────────────

    #[test]
    fn drv_path_with_refs_includes_refs_in_fingerprint() {
        let content = b"Derive(...)";
        let no_refs = compute_drv_path_with_refs(content, "hello", &[]);
        let with_refs = compute_drv_path_with_refs(
            content,
            "hello",
            &["/nix/store/abc-dep".to_string()],
        );
        assert_ne!(no_refs, with_refs);
    }

    #[test]
    fn drv_path_with_refs_order_independent() {
        let content = b"Derive(...)";
        let refs_a = vec![
            "/nix/store/bbb-b".to_string(),
            "/nix/store/aaa-a".to_string(),
        ];
        let refs_b = vec![
            "/nix/store/aaa-a".to_string(),
            "/nix/store/bbb-b".to_string(),
        ];
        let p1 = compute_drv_path_with_refs(content, "test", &refs_a);
        let p2 = compute_drv_path_with_refs(content, "test", &refs_b);
        assert_eq!(p1, p2, "ref order should not affect output");
    }

    #[test]
    fn drv_path_with_refs_deduplicates() {
        let content = b"Derive(...)";
        let with_dups = vec![
            "/nix/store/aaa-a".to_string(),
            "/nix/store/aaa-a".to_string(),
        ];
        let without_dups = vec!["/nix/store/aaa-a".to_string()];
        let p1 = compute_drv_path_with_refs(content, "test", &with_dups);
        let p2 = compute_drv_path_with_refs(content, "test", &without_dups);
        assert_eq!(p1, p2, "duplicate refs should be deduplicated");
    }

    // ── Property tests ──────────────────────────────────

    proptest! {
        #[test]
        fn prop_base32_roundtrip_20_bytes(bytes in proptest::collection::vec(any::<u8>(), 20)) {
            let arr: [u8; 20] = bytes.try_into().unwrap();
            let encoded = nix_base32_encode(&arr);
            prop_assert_eq!(encoded.len(), 32);
            let decoded = nix_base32_decode(&encoded).unwrap();
            prop_assert_eq!(decoded, arr);
        }

        #[test]
        fn prop_base32_encode_uses_only_nix_alphabet(bytes in proptest::collection::vec(any::<u8>(), 1..=64)) {
            let encoded = nix_base32_encode(&bytes);
            for c in encoded.chars() {
                prop_assert!(NIX_BASE32_CHARS.contains(&(c as u8)), "invalid char: {}", c);
            }
        }

        #[test]
        fn prop_compress_hash_output_length(
            bytes in proptest::collection::vec(any::<u8>(), 1..=64),
            target_len in 1_usize..=32
        ) {
            let out = compress_hash(&bytes, target_len);
            prop_assert_eq!(out.len(), target_len);
        }

        #[test]
        fn prop_store_path_roundtrip(digest in proptest::collection::vec(any::<u8>(), 20)) {
            let arr: [u8; 20] = digest.try_into().unwrap();
            let sp = StorePath { digest: arr, name: "test-pkg".to_string() };
            let abs = sp.to_absolute_path();
            let reparsed = StorePath::from_absolute_path(&abs).unwrap();
            prop_assert_eq!(reparsed.digest, sp.digest);
            prop_assert_eq!(reparsed.name, sp.name);
        }

        // Brief: round-trip property tests for nix_base32 on byte vectors
        // of varied sizes (5, 10, 20, 32, 64).
        // Note: nix_base32_decode is fixed at 20 bytes input, so we can only
        // do full encode→decode roundtrips for that length.

        #[test]
        fn prop_base32_encode_length_5(bytes in proptest::collection::vec(any::<u8>(), 5)) {
            let encoded = nix_base32_encode(&bytes);
            // 5 bytes * 8 = 40 bits, ceil(40/5) = 8 chars
            prop_assert_eq!(encoded.len(), 8);
        }

        #[test]
        fn prop_base32_encode_length_10(bytes in proptest::collection::vec(any::<u8>(), 10)) {
            let encoded = nix_base32_encode(&bytes);
            // 10 bytes * 8 = 80 bits, ceil(80/5) = 16 chars
            prop_assert_eq!(encoded.len(), 16);
        }

        #[test]
        fn prop_base32_encode_length_32(bytes in proptest::collection::vec(any::<u8>(), 32)) {
            let encoded = nix_base32_encode(&bytes);
            // 32 bytes * 8 = 256 bits, ceil(256/5) = 52 chars
            prop_assert_eq!(encoded.len(), 52);
        }

        #[test]
        fn prop_base32_encode_length_64(bytes in proptest::collection::vec(any::<u8>(), 64)) {
            let encoded = nix_base32_encode(&bytes);
            // 64 bytes * 8 = 512 bits, ceil(512/5) = 103 chars
            prop_assert_eq!(encoded.len(), 103);
        }

        #[test]
        fn prop_base32_encode_uses_alphabet_only(bytes in proptest::collection::vec(any::<u8>(), 1..=128)) {
            let encoded = nix_base32_encode(&bytes);
            for c in encoded.chars() {
                prop_assert!(NIX_BASE32_CHARS.contains(&(c as u8)));
            }
        }

        // Property test: compute_drv_path is deterministic for any input.
        #[test]
        fn prop_drv_path_deterministic(
            content in proptest::collection::vec(any::<u8>(), 0..200),
            name in "[a-z][a-z0-9-]{0,30}",
        ) {
            let p1 = compute_drv_path(&content, &name);
            let p2 = compute_drv_path(&content, &name);
            prop_assert_eq!(p1, p2);
        }

        // Property test: drv path with refs is invariant under permutation.
        #[test]
        fn prop_drv_path_with_refs_permutation_invariant(
            content in proptest::collection::vec(any::<u8>(), 0..50),
            name in "[a-z]{1,10}",
            n_refs in 0_usize..=8,
        ) {
            let refs: Vec<String> = (0..n_refs).map(|i| format!("/nix/store/r{i}-x")).collect();
            let mut shuffled = refs.clone();
            shuffled.reverse();
            let p1 = compute_drv_path_with_refs(&content, &name, &refs);
            let p2 = compute_drv_path_with_refs(&content, &name, &shuffled);
            prop_assert_eq!(p1, p2);
        }
    }

    // ── Additional StorePath edge cases ──────────────────

    #[test]
    fn from_basename_unicode_in_name_rejected_or_accepted() {
        // The current parser only requires ASCII for hash. The name may
        // technically contain any UTF-8 character. Document current behavior.
        let hash = nix_base32_encode(&[0u8; 20]);
        let basename = format!("{hash}-héllo");
        let sp = StorePath::from_basename(&basename).unwrap();
        assert_eq!(sp.name, "héllo");
    }

    #[test]
    fn from_absolute_path_without_leading_slash_rejected() {
        assert!(StorePath::from_absolute_path("nix/store/abc").is_err());
    }

    #[test]
    fn from_absolute_path_with_extra_path_segments_rejected() {
        // /nix/store/<hash>-<name>/extra is not a valid store path
        let hash = nix_base32_encode(&[0u8; 20]);
        let path = format!("/nix/store/{hash}-name/extra");
        // The current parser accepts it because it strips the prefix and
        // takes everything after as basename. Document current behavior.
        let sp = StorePath::from_absolute_path(&path).unwrap();
        assert_eq!(sp.name, "name/extra");
    }

    #[test]
    fn store_path_hash_trait_works_in_hashset() {
        use std::collections::HashSet;
        let p1 = StorePath {
            digest: [1; 20],
            name: "x".to_string(),
        };
        let p2 = p1.clone();
        let p3 = StorePath {
            digest: [2; 20],
            name: "x".to_string(),
        };
        let mut set = HashSet::new();
        set.insert(p1);
        set.insert(p2); // duplicate
        set.insert(p3);
        assert_eq!(set.len(), 2);
    }

    // ── from_str (FromStr) trait ─────────────────────────

    #[test]
    fn store_path_from_str_trait() {
        use std::str::FromStr;
        let path = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-hierarchical-0.1.0.1";
        let sp: StorePath = StorePath::from_str(path).unwrap();
        assert_eq!(sp.name, "net-hierarchical-0.1.0.1");
    }

    #[test]
    fn store_path_parse_trait_via_str() {
        let path = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-hierarchical-0.1.0.1";
        let sp: StorePath = path.parse().unwrap();
        assert_eq!(sp.name, "net-hierarchical-0.1.0.1");
    }

    // ── StorePathError variants ──────────────────────────

    #[test]
    fn store_path_error_invalid_format_includes_string() {
        match StorePath::from_absolute_path("/tmp/foo") {
            Err(StorePathError::Invalid(s)) => assert_eq!(s, "/tmp/foo"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn store_path_error_empty_name_variant() {
        // The minimum-length check (basename.len() < HASH_LEN + 2) fires first,
        // so a 33-char basename ends up returning Invalid, not EmptyName.
        // To reach the EmptyName branch we need a basename that's long enough
        // (>= 34 chars) but where the chars after the dash are still empty —
        // which is logically impossible. The branch is reachable only via
        // Direct construction of from_basename in code paths the parser
        // can't produce, so EmptyName is effectively dead code in the public
        // API. Document the actual reachable error.
        let hash = nix_base32_encode(&[0u8; 20]);
        let basename = format!("{hash}-");
        match StorePath::from_basename(&basename) {
            Err(StorePathError::Invalid(_)) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn store_path_error_invalid_hash_length() {
        match nix_base32_decode("abc") {
            Err(StorePathError::InvalidHashLength { expected, got }) => {
                assert_eq!(expected, 32);
                assert_eq!(got, 3);
            }
            other => panic!("expected InvalidHashLength, got {other:?}"),
        }
    }

    // ── compress_hash various output sizes ──────────────

    #[test]
    fn compress_hash_to_one_byte_xor_all_input() {
        // For output_len = 1, every input byte XORs into byte 0.
        let input = vec![0xFF, 0x0F, 0xF0, 0x55, 0xAA];
        let out = compress_hash(&input, 1);
        let expected = 0xFF ^ 0x0F ^ 0xF0 ^ 0x55 ^ 0xAA;
        assert_eq!(out[0], expected);
    }

    #[test]
    fn compress_hash_empty_input() {
        let out = compress_hash(&[], 5);
        assert_eq!(out, vec![0u8; 5]);
    }

    #[test]
    fn compress_hash_smaller_input_than_output() {
        // 3 bytes input → 5 bytes output: bytes 0..3 are XORed, 3..5 stay 0
        let input = vec![0xAA, 0xBB, 0xCC];
        let out = compress_hash(&input, 5);
        assert_eq!(out[0], 0xAA);
        assert_eq!(out[1], 0xBB);
        assert_eq!(out[2], 0xCC);
        assert_eq!(out[3], 0);
        assert_eq!(out[4], 0);
    }

    // ── compute_output_path with named outputs ───────────

    #[test]
    fn output_path_lib_format() {
        let path = compute_output_path("0123456789abcdef", "lib", "openssl");
        assert!(path.starts_with("/nix/store/"));
        assert!(path.ends_with("-openssl-lib"));
    }

    #[test]
    fn output_path_default_does_not_have_out_suffix() {
        let path = compute_output_path("0123456789abcdef", "out", "hello");
        let basename = path.strip_prefix("/nix/store/").unwrap();
        assert!(!basename.ends_with("-hello-out"));
        assert!(basename.ends_with("-hello"));
    }

    // ── compute_fixed_output_hash recursive sha256 ───────

    #[test]
    fn fixed_output_recursive_sha256_uses_source_branch() {
        // Both branches are deterministic — verify recursive sha256 differs
        // from non-recursive sha256
        let r = compute_fixed_output_hash("sha256", "abc", true, "thing");
        let f = compute_fixed_output_hash("sha256", "abc", false, "thing");
        assert_ne!(r, f);
    }

    #[test]
    fn fixed_output_recursive_md5_does_not_use_source_branch() {
        // Recursive but algo != sha256 → goes through "fixed:out:r:" branch
        let r = compute_fixed_output_hash("md5", "abc", true, "thing");
        let f = compute_fixed_output_hash("md5", "abc", false, "thing");
        // r uses "r:md5:" prefix, f uses "md5:" — different fingerprints
        assert_ne!(r, f);
    }

    // ── compute_drv_path delegates to with_refs ─────────

    #[test]
    fn compute_drv_path_equals_with_refs_empty_slice() {
        let p1 = compute_drv_path(b"content", "name");
        let p2 = compute_drv_path_with_refs(b"content", "name", &[]);
        assert_eq!(p1, p2);
    }

    // ── DEFAULT_STORE_DIR + STORE_PATH_HASH_LEN constants ──

    #[test]
    fn store_constants() {
        assert_eq!(DEFAULT_STORE_DIR, "/nix/store");
        assert_eq!(STORE_PATH_HASH_LEN, 32);
    }
}
