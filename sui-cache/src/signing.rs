//! Ed25519 signing for narinfo metadata.
//!
//! Each cache has a key pair identified by a key name. The `CacheSigner`
//! signs the narinfo fingerprint (the canonical string that Nix signs)
//! and produces a `keyname:base64sig` string that goes into the `Sig:` field.

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use sui_compat::hash::base64_encode;
use sui_compat::narinfo::NarInfo;
use sui_compat::signature::compute_fingerprint;

/// Signs narinfo metadata with an ed25519 key pair.
#[derive(Debug)]
pub struct CacheSigner {
    /// Human-readable key name (e.g. `my-cache-1`).
    key_name: String,
    /// The ed25519 signing (secret) key.
    secret_key: SigningKey,
}

impl CacheSigner {
    /// Create a signer from a key name and raw 32-byte secret key.
    #[must_use]
    pub fn new(key_name: String, secret_key: SigningKey) -> Self {
        Self {
            key_name,
            secret_key,
        }
    }

    /// Generate a new random signing key pair.
    #[must_use]
    pub fn generate(key_name: String) -> Self {
        let secret_key = SigningKey::generate(&mut rand_core::OsRng);
        Self {
            key_name,
            secret_key,
        }
    }

    /// Return the key name.
    #[must_use]
    pub fn key_name(&self) -> &str {
        &self.key_name
    }

    /// Return the public (verifying) key.
    #[must_use]
    pub fn public_key(&self) -> VerifyingKey {
        self.secret_key.verifying_key()
    }

    /// Format the public key as `keyname:base64pubkey` for distribution.
    #[must_use]
    pub fn public_key_string(&self) -> String {
        format!(
            "{}:{}",
            self.key_name,
            base64_encode(self.public_key().as_bytes())
        )
    }

    /// Format the secret key as `keyname:base64(secret||public)` (Nix format).
    ///
    /// Nix stores signing keys as 64 bytes: the 32-byte secret seed
    /// concatenated with the 32-byte public key, then base64-encoded.
    #[must_use]
    pub fn secret_key_string(&self) -> String {
        let mut combined = Vec::with_capacity(64);
        combined.extend_from_slice(self.secret_key.as_bytes());
        combined.extend_from_slice(self.public_key().as_bytes());
        format!("{}:{}", self.key_name, base64_encode(&combined))
    }

    /// Parse a secret key from the `keyname:base64(secret||public)` format.
    ///
    /// # Errors
    ///
    /// Returns an error if the format is invalid or the base64 decoding fails.
    pub fn from_secret_key_string(s: &str) -> Result<Self, crate::CacheError> {
        let (key_name, b64) = s
            .split_once(':')
            .ok_or_else(|| crate::CacheError::Signing("missing colon in key string".to_string()))?;

        let decoded = sui_compat::hash::base64_decode(b64)
            .map_err(|_| crate::CacheError::Signing("invalid base64 in key".to_string()))?;

        if decoded.len() != 64 {
            return Err(crate::CacheError::Signing(format!(
                "expected 64 bytes, got {}",
                decoded.len()
            )));
        }

        let secret_bytes: [u8; 32] = decoded[..32]
            .try_into()
            .map_err(|_| crate::CacheError::Signing("secret key slice error".to_string()))?;

        let secret_key = SigningKey::from_bytes(&secret_bytes);

        Ok(Self {
            key_name: key_name.to_string(),
            secret_key,
        })
    }

    /// Sign a narinfo and return the signature string (`keyname:base64sig`).
    #[must_use]
    pub fn sign_narinfo(&self, info: &NarInfo) -> String {
        let fingerprint = compute_fingerprint(
            &info.store_path,
            &info.nar_hash,
            info.nar_size,
            &info.references,
        );
        let sig = self.secret_key.sign(fingerprint.as_bytes());
        format!("{}:{}", self.key_name, base64_encode(&sig.to_bytes()))
    }
}

/// Verify that a narinfo signature is valid against a public key string.
///
/// The public key string is in `keyname:base64pubkey` format.
pub fn verify_narinfo_signature(
    info: &NarInfo,
    signature: &str,
    public_key_str: &str,
) -> Result<bool, crate::CacheError> {
    use ed25519_dalek::Verifier;
    use sui_compat::signature::StorePathSignature;

    let parsed_sig = StorePathSignature::parse(signature)
        .map_err(|e| crate::CacheError::Signing(format!("bad signature: {e}")))?;

    let (pk_name, pk_b64) = public_key_str
        .split_once(':')
        .ok_or_else(|| crate::CacheError::Signing("bad public key format".to_string()))?;

    // Key names must match.
    if parsed_sig.key_name != pk_name {
        return Ok(false);
    }

    let pk_bytes = sui_compat::hash::base64_decode(pk_b64)
        .map_err(|_| crate::CacheError::Signing("bad base64 in public key".to_string()))?;

    if pk_bytes.len() != 32 {
        return Err(crate::CacheError::Signing(format!(
            "public key: expected 32 bytes, got {}",
            pk_bytes.len()
        )));
    }

    let pk_array: [u8; 32] = pk_bytes
        .try_into()
        .map_err(|_| crate::CacheError::Signing("public key conversion error".to_string()))?;

    let verifying_key = VerifyingKey::from_bytes(&pk_array)
        .map_err(|_| crate::CacheError::Signing("invalid public key".to_string()))?;

    let fingerprint = compute_fingerprint(
        &info.store_path,
        &info.nar_hash,
        info.nar_size,
        &info.references,
    );

    let sig_array: [u8; 64] = parsed_sig
        .signature
        .try_into()
        .map_err(|_| crate::CacheError::Signing("signature must be 64 bytes".to_string()))?;

    let ed_sig = ed25519_dalek::Signature::from_bytes(&sig_array);

    Ok(verifying_key
        .verify(fingerprint.as_bytes(), &ed_sig)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_narinfo() -> NarInfo {
        NarInfo {
            store_path: "/nix/store/abc-hello-1.0".to_string(),
            url: "nar/abc.nar.xz".to_string(),
            compression: "xz".to_string(),
            file_hash: "sha256:1111".to_string(),
            file_size: 1000,
            nar_hash: "sha256:2222".to_string(),
            nar_size: 5000,
            references: vec!["dep-1".to_string()],
            deriver: Some("abc.drv".to_string()),
            signatures: vec![],
            ca: None,
        }
    }

    #[test]
    fn generate_and_sign() {
        let signer = CacheSigner::generate("test-cache-1".to_string());
        let info = make_test_narinfo();
        let sig = signer.sign_narinfo(&info);
        assert!(sig.starts_with("test-cache-1:"));
        assert!(sig.len() > "test-cache-1:".len());
    }

    #[test]
    fn sign_and_verify() {
        let signer = CacheSigner::generate("test-cache-1".to_string());
        let info = make_test_narinfo();
        let sig = signer.sign_narinfo(&info);
        let pk_str = signer.public_key_string();
        let valid = verify_narinfo_signature(&info, &sig, &pk_str).unwrap();
        assert!(valid);
    }

    #[test]
    fn verify_wrong_key_fails() {
        let signer = CacheSigner::generate("cache-a".to_string());
        let other = CacheSigner::generate("cache-b".to_string());
        let info = make_test_narinfo();
        let sig = signer.sign_narinfo(&info);
        // Verify with the wrong key name (different key).
        let pk_str = other.public_key_string();
        let valid = verify_narinfo_signature(&info, &sig, &pk_str).unwrap();
        assert!(!valid);
    }

    #[test]
    fn verify_tampered_narinfo_fails() {
        let signer = CacheSigner::generate("cache-1".to_string());
        let info = make_test_narinfo();
        let sig = signer.sign_narinfo(&info);
        let pk_str = signer.public_key_string();

        // Tamper with the narinfo.
        let mut tampered = info.clone();
        tampered.nar_size = 9999;

        let valid = verify_narinfo_signature(&tampered, &sig, &pk_str).unwrap();
        assert!(!valid);
    }

    #[test]
    fn public_key_string_format() {
        let signer = CacheSigner::generate("my-cache-1".to_string());
        let pk_str = signer.public_key_string();
        assert!(pk_str.starts_with("my-cache-1:"));
        // Base64 of 32 bytes = 44 chars.
        let b64_part = pk_str.strip_prefix("my-cache-1:").unwrap();
        assert_eq!(b64_part.len(), 44);
    }

    #[test]
    fn secret_key_string_roundtrip() {
        let signer = CacheSigner::generate("roundtrip-key".to_string());
        let sk_str = signer.secret_key_string();
        let restored = CacheSigner::from_secret_key_string(&sk_str).unwrap();
        assert_eq!(restored.key_name(), "roundtrip-key");
        // Sign the same narinfo and verify they produce the same signature.
        let info = make_test_narinfo();
        assert_eq!(signer.sign_narinfo(&info), restored.sign_narinfo(&info));
    }

    #[test]
    fn from_secret_key_string_bad_format() {
        let result = CacheSigner::from_secret_key_string("no-colon-here");
        assert!(result.is_err());
    }

    #[test]
    fn from_secret_key_string_bad_base64() {
        let result = CacheSigner::from_secret_key_string("key:!!!bad!!!");
        assert!(result.is_err());
    }

    #[test]
    fn from_secret_key_string_wrong_length() {
        let result = CacheSigner::from_secret_key_string("key:AAAA");
        assert!(result.is_err());
    }

    #[test]
    fn key_name_accessor() {
        let signer = CacheSigner::generate("my-name".to_string());
        assert_eq!(signer.key_name(), "my-name");
    }

    #[test]
    fn sign_narinfo_with_no_references() {
        let signer = CacheSigner::generate("k".to_string());
        let mut info = make_test_narinfo();
        info.references.clear();
        let sig = signer.sign_narinfo(&info);
        let pk = signer.public_key_string();
        assert!(verify_narinfo_signature(&info, &sig, &pk).unwrap());
    }

    #[test]
    fn sign_narinfo_with_many_references() {
        let signer = CacheSigner::generate("k".to_string());
        let mut info = make_test_narinfo();
        info.references = (0..20).map(|i| format!("ref-{i:03}")).collect();
        let sig = signer.sign_narinfo(&info);
        let pk = signer.public_key_string();
        assert!(verify_narinfo_signature(&info, &sig, &pk).unwrap());
    }

    #[test]
    fn verify_bad_signature_string() {
        let info = make_test_narinfo();
        let result = verify_narinfo_signature(&info, "no-colon", "key:AAAA");
        assert!(result.is_err());
    }

    #[test]
    fn verify_bad_public_key_string() {
        let signer = CacheSigner::generate("k".to_string());
        let info = make_test_narinfo();
        let sig = signer.sign_narinfo(&info);
        let result = verify_narinfo_signature(&info, &sig, "no-colon-pk");
        assert!(result.is_err());
    }

    #[test]
    fn deterministic_signatures() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let signer = CacheSigner::new("det".to_string(), sk);
        let info = make_test_narinfo();
        let sig1 = signer.sign_narinfo(&info);
        let sig2 = signer.sign_narinfo(&info);
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn new_from_known_key() {
        let sk = SigningKey::from_bytes(&[1u8; 32]);
        let signer = CacheSigner::new("known-key".to_string(), sk);
        assert_eq!(signer.key_name(), "known-key");
        let info = make_test_narinfo();
        let sig = signer.sign_narinfo(&info);
        assert!(sig.starts_with("known-key:"));
    }
}
