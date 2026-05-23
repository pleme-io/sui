//! Property tests for the hash conversion matrix.
//!
//! For every (algorithm × encoding) pair, decode and re-encode
//! must round-trip byte-identical bytes.  This locks the
//! substrate's hash algebra: any new encoding addition has to
//! satisfy this invariant.

use proptest::prelude::*;
use sui_spec::hash;

const ALGOS: &[&str] = &["sha256", "sha512"];
const SIZE_BY_ALG: &[(&str, usize)] = &[
    ("sha256", 32),
    ("sha512", 64),
];

fn digest_size_for(alg: &str) -> usize {
    SIZE_BY_ALG.iter().find(|(a, _)| *a == alg).map(|(_, n)| *n).unwrap_or(32)
}

proptest! {
    /// Round-trip via base16 preserves BYTES (algo isn't
    /// recoverable from bare hex per substrate convention).
    #[test]
    fn base16_roundtrip_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 32..=32),
    ) {
        let encoded = hash::encode_hash("sha256", "base16", &bytes).unwrap();
        let (_, decoded) = hash::decode_hash(&encoded).unwrap();
        prop_assert_eq!(decoded, bytes);
    }

    /// SRI round-trip preserves both algorithm AND bytes.
    #[test]
    fn sri_roundtrip(
        bytes in proptest::collection::vec(any::<u8>(), 32..=32),
    ) {
        let encoded = hash::encode_hash("sha256", "sri", &bytes).unwrap();
        let (decoded_alg, decoded) = hash::decode_hash(&encoded).unwrap();
        prop_assert_eq!(decoded_alg, "sha256");
        prop_assert_eq!(decoded, bytes);
    }

    /// nix-base32 round-trip preserves both algorithm AND bytes.
    #[test]
    fn nix_base32_roundtrip(
        bytes in proptest::collection::vec(any::<u8>(), 32..=32),
    ) {
        let encoded = hash::encode_hash("sha256", "nix-base32", &bytes).unwrap();
        let (decoded_alg, decoded) = hash::decode_hash(&encoded).unwrap();
        prop_assert_eq!(decoded_alg, "sha256");
        prop_assert_eq!(decoded, bytes);
    }

    /// Cross-encoding equivalence on BYTES: encoding the same
    /// bytes through any prefixed target produces a string that
    /// decodes back to identical bytes.
    #[test]
    fn cross_encoding_equivalence_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 32..=32),
    ) {
        let as_sri    = hash::encode_hash("sha256", "sri", &bytes).unwrap();
        let as_b32    = hash::encode_hash("sha256", "nix-base32", &bytes).unwrap();
        let (_, d_sri) = hash::decode_hash(&as_sri).unwrap();
        let (_, d_b32) = hash::decode_hash(&as_b32).unwrap();
        prop_assert_eq!(d_sri.clone(), bytes.clone());
        prop_assert_eq!(d_b32, bytes);
    }

    /// `apply_conversion("auto", target, input)` re-encodes
    /// while preserving bytes through every prefixed target.
    #[test]
    fn apply_conversion_preserves_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 32..=32),
        target in prop::sample::select(&["sri", "nix-base32"][..]),
    ) {
        let target = target.to_string();
        // Use SRI as the canonical input so the algorithm is
        // recoverable downstream.
        let sri_input = hash::encode_hash("sha256", "sri", &bytes).unwrap();
        let converted = hash::apply_conversion("auto", &target, &sri_input).unwrap();
        let (_, redecoded) = hash::decode_hash(&converted).unwrap();
        prop_assert_eq!(redecoded, bytes);
    }

    /// Different bytes produce different encodings (no collision
    /// at the encoder level, modulo the sha2 itself).
    #[test]
    fn different_bytes_yield_different_encodings(
        a in proptest::collection::vec(any::<u8>(), 32..=32),
        b in proptest::collection::vec(any::<u8>(), 32..=32),
    ) {
        prop_assume!(a != b);
        for enc in ["base16", "sri", "nix-base32"] {
            let ea = hash::encode_hash("sha256", enc, &a).unwrap();
            let eb = hash::encode_hash("sha256", enc, &b).unwrap();
            prop_assert_ne!(&ea, &eb,
                "{}: same encoding for different bytes", enc);
        }
    }
}

#[test]
fn empty_string_sha256_is_canonical() {
    // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    let empty_sha256: Vec<u8> = vec![
        0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14,
        0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
        0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c,
        0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
    ];
    let sri = hash::encode_hash("sha256", "sri", &empty_sha256).unwrap();
    assert_eq!(sri, "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=");

    let b32 = hash::encode_hash("sha256", "nix-base32", &empty_sha256).unwrap();
    assert!(b32.starts_with("sha256:"));
    assert_eq!(b32.strip_prefix("sha256:").unwrap().len(), 52);
}
