//! Ed25519 store path signatures.
//!
//! Nix uses Ed25519 to sign store paths. Format: `keyname:base64sig`.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use thiserror::Error;

use crate::hash::base64_encode;

/// Trait for verifying store path signatures.
///
/// Implementations can use different Ed25519 backends (ed25519-dalek, ring, aws-lc-rs)
/// or implement custom trust policies (multi-key quorum, key rotation, HSM).
pub trait SignatureVerifier: Send + Sync {
    /// Verify a signature against a fingerprint and public key.
    fn verify(&self, fingerprint: &[u8], signature: &[u8], public_key: &[u8]) -> Result<(), SignatureError>;
}

/// Default Ed25519 signature verifier using ed25519-dalek.
pub struct Ed25519Verifier;

impl SignatureVerifier for Ed25519Verifier {
    fn verify(&self, fingerprint: &[u8], signature: &[u8], public_key: &[u8]) -> Result<(), SignatureError> {
        let key_bytes: [u8; 32] = public_key.try_into()
            .map_err(|_| SignatureError::InvalidPublicKey)?;
        let sig_bytes: [u8; 64] = signature.try_into()
            .map_err(|_| SignatureError::InvalidFormat("signature must be 64 bytes".to_string()))?;

        let verifying_key = VerifyingKey::from_bytes(&key_bytes)
            .map_err(|_| SignatureError::InvalidPublicKey)?;
        let sig = Signature::from_bytes(&sig_bytes);

        verifying_key.verify(fingerprint, &sig)
            .map_err(|_| SignatureError::VerificationFailed)
    }
}

#[derive(Debug, Error)]
pub enum SignatureError {
    #[error("invalid signature format: {0}")]
    InvalidFormat(String),
    #[error("base64 decode error")]
    Base64Decode,
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("verification failed")]
    VerificationFailed,
}

/// A parsed store path signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePathSignature {
    /// The key name (e.g., "cache.nixos.org-1").
    pub key_name: String,
    /// The raw Ed25519 signature bytes (64 bytes).
    pub signature: Vec<u8>,
}

impl StorePathSignature {
    /// Parse a signature from the `keyname:base64sig` format.
    pub fn parse(s: &str) -> Result<Self, SignatureError> {
        let (key_name, b64) = s
            .split_once(':')
            .ok_or_else(|| SignatureError::InvalidFormat("missing colon".to_string()))?;

        let signature = b64_decode(b64)
            .map_err(|_| SignatureError::Base64Decode)?;

        Ok(Self {
            key_name: key_name.to_string(),
            signature,
        })
    }

    /// Serialize to the `keyname:base64sig` format.
    #[must_use]
    pub fn to_string_repr(&self) -> String {
        self.to_string()
    }

    /// Verify this signature against a fingerprint and public key.
    ///
    /// Uses the default Ed25519 verifier. For custom verification strategies,
    /// use `verify_with()` with a custom `SignatureVerifier` implementation.
    pub fn verify(&self, fingerprint: &str, public_key: &[u8; 32]) -> Result<(), SignatureError> {
        self.verify_with(fingerprint, public_key, &Ed25519Verifier)
    }

    /// Verify using a custom `SignatureVerifier` implementation.
    pub fn verify_with(
        &self,
        fingerprint: &str,
        public_key: &[u8],
        verifier: &dyn SignatureVerifier,
    ) -> Result<(), SignatureError> {
        verifier.verify(fingerprint.as_bytes(), &self.signature, public_key)
    }
}

impl std::fmt::Display for StorePathSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.key_name, base64_encode(&self.signature))
    }
}

/// Compute the fingerprint string that Nix signs.
///
/// Format: `1;{storePath};{narHash};{narSize};{sortedReferences}`
#[must_use]
pub fn compute_fingerprint(
    store_path: &str,
    nar_hash: &str,
    nar_size: u64,
    references: &[String],
) -> String {
    let refs = references.join(",");
    format!("1;{store_path};{nar_hash};{nar_size};{refs}")
}

/// Base64 decode using the `base64` crate.
fn b64_decode(input: &str) -> Result<Vec<u8>, ()> {
    crate::hash::base64_decode(input).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn parse_signature() {
        // 64 bytes = 86 base64 chars + "==" padding
        let sig_str = "cache.nixos.org-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
        let sig = StorePathSignature::parse(sig_str).unwrap();
        assert_eq!(sig.key_name, "cache.nixos.org-1");
        assert_eq!(sig.signature.len(), 64);
    }

    #[test]
    fn roundtrip_signature_format() {
        let sig = StorePathSignature {
            key_name: "test-key-1".to_string(),
            signature: vec![0u8; 64],
        };
        let s = sig.to_string_repr();
        let parsed = StorePathSignature::parse(&s).unwrap();
        assert_eq!(parsed.key_name, sig.key_name);
        assert_eq!(parsed.signature, sig.signature);
    }

    #[test]
    fn sign_and_verify() {
        use ed25519_dalek::Signer;

        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let verifying_key = signing_key.verifying_key();

        let fingerprint = compute_fingerprint(
            "/nix/store/abc-hello-2.12.1",
            "sha256:deadbeef",
            226552,
            &["glibc-2.37".to_string()],
        );

        let sig = signing_key.sign(fingerprint.as_bytes());

        let store_sig = StorePathSignature {
            key_name: "test-key".to_string(),
            signature: sig.to_bytes().to_vec(),
        };

        assert!(store_sig.verify(&fingerprint, verifying_key.as_bytes()).is_ok());
    }

    #[test]
    fn verify_wrong_fingerprint() {
        use ed25519_dalek::Signer;

        let signing_key = SigningKey::from_bytes(&[2u8; 32]);
        let verifying_key = signing_key.verifying_key();

        let fingerprint = "1;/nix/store/abc;sha256:aaa;100;";
        let sig = signing_key.sign(fingerprint.as_bytes());

        let store_sig = StorePathSignature {
            key_name: "test-key".to_string(),
            signature: sig.to_bytes().to_vec(),
        };

        assert!(store_sig.verify("wrong fingerprint", verifying_key.as_bytes()).is_err());
    }

    #[test]
    fn compute_fingerprint_format() {
        let fp = compute_fingerprint(
            "/nix/store/abc-hello",
            "sha256:deadbeef",
            1024,
            &["dep1".to_string(), "dep2".to_string()],
        );
        assert_eq!(fp, "1;/nix/store/abc-hello;sha256:deadbeef;1024;dep1,dep2");
    }

    #[test]
    fn invalid_signature_format() {
        assert!(StorePathSignature::parse("no-colon-here").is_err());
    }

    #[test]
    fn multiple_signatures_on_same_path() {
        use ed25519_dalek::Signer;

        let fingerprint = compute_fingerprint(
            "/nix/store/abc-hello-2.12.1",
            "sha256:deadbeef",
            226552,
            &["glibc-2.37".to_string()],
        );

        // Two different signing keys
        let key1 = SigningKey::from_bytes(&[1u8; 32]);
        let key2 = SigningKey::from_bytes(&[2u8; 32]);
        let vk1 = key1.verifying_key();
        let vk2 = key2.verifying_key();

        let sig1 = StorePathSignature {
            key_name: "cache.nixos.org-1".to_string(),
            signature: key1.sign(fingerprint.as_bytes()).to_bytes().to_vec(),
        };
        let sig2 = StorePathSignature {
            key_name: "my-cache-1".to_string(),
            signature: key2.sign(fingerprint.as_bytes()).to_bytes().to_vec(),
        };

        // Both should verify against their own key
        assert!(sig1.verify(&fingerprint, vk1.as_bytes()).is_ok());
        assert!(sig2.verify(&fingerprint, vk2.as_bytes()).is_ok());

        // Cross-verification should fail
        assert!(sig1.verify(&fingerprint, vk2.as_bytes()).is_err());
        assert!(sig2.verify(&fingerprint, vk1.as_bytes()).is_err());
    }

    #[test]
    fn signature_with_very_long_key_name() {
        let long_name = "a".repeat(500);
        let sig = StorePathSignature {
            key_name: long_name.clone(),
            signature: vec![0u8; 64],
        };
        let s = sig.to_string_repr();
        assert!(s.starts_with(&long_name));
        let parsed = StorePathSignature::parse(&s).unwrap();
        assert_eq!(parsed.key_name, long_name);
    }

    #[test]
    fn fingerprint_with_empty_references() {
        let fp = compute_fingerprint(
            "/nix/store/abc-hello",
            "sha256:deadbeef",
            1024,
            &[],
        );
        assert_eq!(fp, "1;/nix/store/abc-hello;sha256:deadbeef;1024;");
    }

    #[test]
    fn fingerprint_with_many_references() {
        let refs: Vec<String> = (0..20)
            .map(|i| format!("dep-{i:02}"))
            .collect();
        let fp = compute_fingerprint(
            "/nix/store/abc-hello",
            "sha256:deadbeef",
            1024,
            &refs,
        );
        // Should contain all 20 references comma-separated
        let parts: Vec<&str> = fp.split(';').collect();
        assert_eq!(parts.len(), 5);
        let ref_part = parts[4];
        let ref_entries: Vec<&str> = ref_part.split(',').collect();
        assert_eq!(ref_entries.len(), 20);
        assert_eq!(ref_entries[0], "dep-00");
        assert_eq!(ref_entries[19], "dep-19");
    }

    #[test]
    fn signature_roundtrip_with_nonzero_bytes() {
        let sig = StorePathSignature {
            key_name: "test-key-1".to_string(),
            signature: (0..64).collect::<Vec<u8>>(),
        };
        let s = sig.to_string_repr();
        let parsed = StorePathSignature::parse(&s).unwrap();
        assert_eq!(parsed.key_name, sig.key_name);
        assert_eq!(parsed.signature, sig.signature);
    }

    // ── Mock verifiers ────────────────────────────────────

    struct AlwaysValidVerifier;
    impl SignatureVerifier for AlwaysValidVerifier {
        fn verify(&self, _: &[u8], _: &[u8], _: &[u8]) -> Result<(), SignatureError> { Ok(()) }
    }

    struct AlwaysInvalidVerifier;
    impl SignatureVerifier for AlwaysInvalidVerifier {
        fn verify(&self, _: &[u8], _: &[u8], _: &[u8]) -> Result<(), SignatureError> {
            Err(SignatureError::VerificationFailed)
        }
    }

    #[test]
    fn verify_with_always_valid() {
        let sig = StorePathSignature { key_name: "k".into(), signature: vec![0; 64] };
        assert!(sig.verify_with("fp", &[0; 32], &AlwaysValidVerifier).is_ok());
    }

    #[test]
    fn verify_with_always_invalid() {
        let sig = StorePathSignature { key_name: "k".into(), signature: vec![0; 64] };
        assert!(sig.verify_with("fp", &[0; 32], &AlwaysInvalidVerifier).is_err());
    }

    #[test]
    fn verifier_object_safe() {
        fn _assert(_: &dyn SignatureVerifier) {}
        _assert(&AlwaysValidVerifier);
    }

    // ── StorePathSignature error cases ───────────────────

    #[test]
    fn parse_empty_string() {
        assert!(StorePathSignature::parse("").is_err());
    }

    #[test]
    fn parse_only_colon() {
        let sig = StorePathSignature::parse(":");
        // ":" has empty key_name and empty base64 → decode yields empty vec
        assert!(sig.is_ok());
        let s = sig.unwrap();
        assert_eq!(s.key_name, "");
        assert!(s.signature.is_empty());
    }

    #[test]
    fn parse_invalid_base64_after_colon() {
        let result = StorePathSignature::parse("key:!!!not-base64!!!");
        assert!(result.is_err());
    }

    // ── compute_fingerprint edge cases ──────────────────

    #[test]
    fn fingerprint_with_single_reference() {
        let fp = compute_fingerprint("/nix/store/abc", "sha256:xxx", 500, &["dep".to_string()]);
        assert_eq!(fp, "1;/nix/store/abc;sha256:xxx;500;dep");
    }

    #[test]
    fn fingerprint_with_zero_nar_size() {
        let fp = compute_fingerprint("/nix/store/empty", "sha256:000", 0, &[]);
        assert_eq!(fp, "1;/nix/store/empty;sha256:000;0;");
    }

    #[test]
    fn fingerprint_with_large_nar_size() {
        let fp = compute_fingerprint("/nix/store/big", "sha256:aaa", u64::MAX, &[]);
        assert!(fp.contains(&u64::MAX.to_string()));
    }

    // ── Ed25519Verifier direct tests ────────────────────

    #[test]
    fn ed25519_verifier_invalid_key_length() {
        let verifier = Ed25519Verifier;
        let result = verifier.verify(b"data", &[0; 64], &[0; 16]);
        assert!(result.is_err());
    }

    #[test]
    fn ed25519_verifier_invalid_signature_length() {
        let verifier = Ed25519Verifier;
        let result = verifier.verify(b"data", &[0; 32], &[0; 32]);
        assert!(result.is_err());
    }

    // ── to_string_repr / parse roundtrip ────────────────

    #[test]
    fn to_string_repr_format() {
        let sig = StorePathSignature {
            key_name: "cache.nixos.org-1".to_string(),
            signature: vec![1; 64],
        };
        let s = sig.to_string_repr();
        assert!(s.starts_with("cache.nixos.org-1:"));
        assert!(s.len() > "cache.nixos.org-1:".len());
    }

    // ── Display trait ────────────────────────────────────

    #[test]
    fn display_trait_matches_to_string_repr() {
        let sig = StorePathSignature {
            key_name: "k".to_string(),
            signature: vec![0xab; 64],
        };
        let displayed = format!("{sig}");
        let manual = sig.to_string_repr();
        assert_eq!(displayed, manual);
    }

    // ── Tampered signatures ──────────────────────────────

    #[test]
    fn verify_with_tampered_signature() {
        use ed25519_dalek::Signer;
        let signing_key = SigningKey::from_bytes(&[3u8; 32]);
        let verifying_key = signing_key.verifying_key();

        let fingerprint = "1;/nix/store/abc;sha256:def;100;";
        let mut sig_bytes = signing_key.sign(fingerprint.as_bytes()).to_bytes().to_vec();

        // Flip a bit in the signature
        sig_bytes[0] ^= 0x01;

        let store_sig = StorePathSignature {
            key_name: "k".to_string(),
            signature: sig_bytes,
        };

        let result = store_sig.verify(fingerprint, verifying_key.as_bytes());
        assert!(result.is_err());
        match result {
            Err(SignatureError::VerificationFailed) => {}
            other => panic!("expected VerificationFailed, got {other:?}"),
        }
    }

    #[test]
    fn verify_with_truncated_signature_fails() {
        let store_sig = StorePathSignature {
            key_name: "k".to_string(),
            signature: vec![0u8; 32], // half size
        };
        let result = store_sig.verify("fp", &[0u8; 32]);
        assert!(result.is_err());
    }

    #[test]
    fn verify_with_oversized_signature_fails() {
        let store_sig = StorePathSignature {
            key_name: "k".to_string(),
            signature: vec![0u8; 128], // too big
        };
        let result = store_sig.verify("fp", &[0u8; 32]);
        assert!(result.is_err());
    }

    // ── Tampered public keys ─────────────────────────────

    #[test]
    fn verify_with_zero_public_key_fails() {
        use ed25519_dalek::Signer;
        let signing_key = SigningKey::from_bytes(&[5u8; 32]);
        let fingerprint = "fingerprint";
        let sig = signing_key.sign(fingerprint.as_bytes());

        let store_sig = StorePathSignature {
            key_name: "k".to_string(),
            signature: sig.to_bytes().to_vec(),
        };

        // All-zero key — even if technically a valid Ed25519 point form,
        // the signature should not verify against it.
        let result = store_sig.verify(fingerprint, &[0u8; 32]);
        assert!(result.is_err());
    }

    #[test]
    fn verify_with_random_public_key_fails() {
        use ed25519_dalek::Signer;
        let signing_key = SigningKey::from_bytes(&[6u8; 32]);
        let other_key = SigningKey::from_bytes(&[7u8; 32]);

        let fingerprint = "fp";
        let sig = signing_key.sign(fingerprint.as_bytes());
        let store_sig = StorePathSignature {
            key_name: "k".to_string(),
            signature: sig.to_bytes().to_vec(),
        };

        let result = store_sig.verify(fingerprint, other_key.verifying_key().as_bytes());
        assert!(result.is_err());
    }

    // ── parse() error variants ───────────────────────────

    #[test]
    fn parse_signature_no_colon_error_variant() {
        match StorePathSignature::parse("just-a-key-name") {
            Err(SignatureError::InvalidFormat(s)) => assert!(s.contains("colon")),
            other => panic!("expected InvalidFormat, got {other:?}"),
        }
    }

    #[test]
    fn parse_signature_invalid_base64_error_variant() {
        match StorePathSignature::parse("k:!!!not-base64!!!") {
            Err(SignatureError::Base64Decode) => {}
            other => panic!("expected Base64Decode, got {other:?}"),
        }
    }

    // ── compute_fingerprint various inputs ───────────────

    #[test]
    fn fingerprint_format_with_spaces_in_path() {
        let fp = compute_fingerprint("/nix/store/abc with spaces", "sha256:abc", 1, &[]);
        assert_eq!(fp, "1;/nix/store/abc with spaces;sha256:abc;1;");
    }

    #[test]
    fn fingerprint_with_three_references_comma_separated() {
        let refs = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let fp = compute_fingerprint("/p", "h", 1, &refs);
        assert!(fp.ends_with(";a,b,c"));
    }

    // ── Ed25519Verifier trait method tests ───────────────

    #[test]
    fn ed25519_verifier_correct_size_inputs_with_invalid_data() {
        let verifier = Ed25519Verifier;
        // Correct sizes (64-byte sig, 32-byte key) but garbage data
        let result = verifier.verify(b"data", &[0u8; 64], &[0u8; 32]);
        // Should fail at verification step since data doesn't match
        assert!(result.is_err());
    }

    #[test]
    fn ed25519_verifier_short_key_returns_invalid_public_key() {
        let verifier = Ed25519Verifier;
        let result = verifier.verify(b"data", &[0u8; 64], &[0u8; 16]);
        match result {
            Err(SignatureError::InvalidPublicKey) => {}
            other => panic!("expected InvalidPublicKey, got {other:?}"),
        }
    }

    #[test]
    fn ed25519_verifier_short_signature_returns_invalid_format() {
        let verifier = Ed25519Verifier;
        let result = verifier.verify(b"data", &[0u8; 32], &[0u8; 32]);
        match result {
            Err(SignatureError::InvalidFormat(s)) => assert!(s.contains("64 bytes")),
            other => panic!("expected InvalidFormat, got {other:?}"),
        }
    }

    // ── verify_with custom verifier ──────────────────────

    struct CountingVerifier {
        count: std::cell::Cell<usize>,
    }

    impl CountingVerifier {
        fn new() -> Self {
            Self { count: std::cell::Cell::new(0) }
        }
    }

    // SAFETY: Cell is not Sync, so we can't use it through Send + Sync.
    // For the test we use atomics instead.
    struct AtomicCountingVerifier {
        count: std::sync::atomic::AtomicUsize,
    }

    impl AtomicCountingVerifier {
        fn new() -> Self {
            Self { count: std::sync::atomic::AtomicUsize::new(0) }
        }
        fn count(&self) -> usize {
            self.count.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl SignatureVerifier for AtomicCountingVerifier {
        fn verify(&self, _: &[u8], _: &[u8], _: &[u8]) -> Result<(), SignatureError> {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn verify_with_custom_verifier_invokes_verify_method() {
        let counter = AtomicCountingVerifier::new();
        let sig = StorePathSignature {
            key_name: "k".to_string(),
            signature: vec![0u8; 64],
        };
        sig.verify_with("fp", &[0u8; 32], &counter).unwrap();
        sig.verify_with("fp", &[0u8; 32], &counter).unwrap();
        assert_eq!(counter.count(), 2);
    }

    // Counts unused but non-blocking — silence dead-code warning.
    #[test]
    fn _counting_verifier_unused_field_workaround() {
        let v = CountingVerifier::new();
        v.count.set(1);
        assert_eq!(v.count.get(), 1);
    }

    // ── Roundtrip via Display ────────────────────────────

    #[test]
    fn signature_clone_eq() {
        let sig = StorePathSignature {
            key_name: "k".to_string(),
            signature: vec![1, 2, 3, 4, 5],
        };
        let cloned = sig.clone();
        assert_eq!(sig, cloned);
    }

    // ── compute_fingerprint with very long inputs ────────

    #[test]
    fn fingerprint_with_long_store_path_and_hash() {
        let path = format!("/nix/store/{}", "a".repeat(200));
        let hash = format!("sha256:{}", "0".repeat(64));
        let fp = compute_fingerprint(&path, &hash, 999_999_999, &[]);
        assert!(fp.contains(&path));
        assert!(fp.contains(&hash));
    }

    // ── Real signing-then-verify with multiple key bytes ──

    #[test]
    fn sign_verify_multiple_distinct_keys() {
        use ed25519_dalek::Signer;
        for seed_byte in [10u8, 20, 30, 40, 50] {
            let signing_key = SigningKey::from_bytes(&[seed_byte; 32]);
            let verifying_key = signing_key.verifying_key();
            let fingerprint = format!("1;/nix/store/p-{seed_byte};sha256:h;100;");
            let sig = signing_key.sign(fingerprint.as_bytes());
            let store_sig = StorePathSignature {
                key_name: format!("k{seed_byte}"),
                signature: sig.to_bytes().to_vec(),
            };
            assert!(
                store_sig.verify(&fingerprint, verifying_key.as_bytes()).is_ok(),
                "key seed {seed_byte} should verify",
            );
        }
    }

    // ── parse + verify integration ───────────────────────

    #[test]
    fn parse_then_verify_roundtrip() {
        use ed25519_dalek::Signer;
        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();

        let fingerprint = "1;/nix/store/abc;sha256:def;1024;";
        let raw_sig = signing_key.sign(fingerprint.as_bytes());
        let original = StorePathSignature {
            key_name: "test-k".to_string(),
            signature: raw_sig.to_bytes().to_vec(),
        };

        // Serialize, then parse, then verify
        let s = original.to_string_repr();
        let parsed = StorePathSignature::parse(&s).unwrap();
        assert_eq!(parsed, original);

        assert!(parsed.verify(fingerprint, verifying_key.as_bytes()).is_ok());
    }

    // ── b64_decode wrapper test ──────────────────────────

    #[test]
    fn b64_decode_helper_returns_error_on_garbage() {
        // Test the b64_decode helper indirectly via parse()
        let result = StorePathSignature::parse("k:zzz!!!");
        assert!(result.is_err());
    }
}
