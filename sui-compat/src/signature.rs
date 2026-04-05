//! Ed25519 store path signatures.
//!
//! Nix uses Ed25519 to sign store paths. Format: `keyname:base64sig`.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use thiserror::Error;

use crate::hash::minimal_base64_encode;

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

        let signature = minimal_base64_decode(b64)
            .map_err(|_| SignatureError::Base64Decode)?;

        Ok(Self {
            key_name: key_name.to_string(),
            signature,
        })
    }

    /// Serialize to the `keyname:base64sig` format.
    pub fn to_string_repr(&self) -> String {
        format!("{}:{}", self.key_name, minimal_base64_encode(&self.signature))
    }

    /// Verify this signature against a fingerprint and public key.
    ///
    /// The fingerprint is the string that was signed, typically:
    /// `1;{storePath};{narHash};{narSize};{references}`
    pub fn verify(&self, fingerprint: &str, public_key: &[u8; 32]) -> Result<(), SignatureError> {
        let verifying_key = VerifyingKey::from_bytes(public_key)
            .map_err(|_| SignatureError::InvalidPublicKey)?;

        let sig_bytes: [u8; 64] = self.signature
            .as_slice()
            .try_into()
            .map_err(|_| SignatureError::InvalidFormat("signature must be 64 bytes".to_string()))?;

        let signature = Signature::from_bytes(&sig_bytes);

        verifying_key
            .verify(fingerprint.as_bytes(), &signature)
            .map_err(|_| SignatureError::VerificationFailed)
    }
}

/// Compute the fingerprint string that Nix signs.
///
/// Format: `1;{storePath};{narHash};{narSize};{sortedReferences}`
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
fn minimal_base64_decode(input: &str) -> Result<Vec<u8>, ()> {
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
}
