//! Typed border for the binary-cache substitution protocol —
//! `cache.nixos.org`, S3-backed Attic, local file-system caches.
//!
//! When sui's builder is asked to realise a store path it doesn't
//! have locally, it consults the substituter chain.  Each
//! substituter exposes the same protocol: ask for a narinfo by
//! store-path hash, download the NAR, verify signatures, extract
//! into the store.  This module names the typed contract; sui-cache
//! + sui-store today implement it ad-hoc, M3+ work will lift the
//! impl onto this spec.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defsubstituter
//!   :name        "cache.nixos.org"
//!   :transport   Https
//!   :endpoint    "https://cache.nixos.org"
//!   :auth        None
//!   :trust-level Trusted
//!   :phases ((:kind QueryNarInfo)
//!            (:kind FetchNar :bind "nar")
//!            (:kind VerifyNarSignature :from "nar")
//!            (:kind ImportNarToStore :from "nar")))
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border ───────────────────────────────────────────────────

/// One substituter, authored as `(defsubstituter …)`.  Variants
/// cover every binary cache shape sui must consume.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defsubstituter")]
pub struct SubstituterSpec {
    /// Display name (`"cache.nixos.org"`, `"attic"`, ...).
    pub name: String,
    /// Underlying transport — determines how phases like
    /// QueryNarInfo are issued.
    pub transport: SubstituterTransport,
    /// Base endpoint URL (or path for local stores).
    pub endpoint: String,
    /// Authentication shape.
    pub auth: SubstituterAuth,
    /// Trust level — controls whether the substituter's signatures
    /// are required, optional, or only valid for trusted users.
    #[serde(rename = "trustLevel")]
    pub trust_level: TrustLevel,
    /// The substitution pipeline.  Every substituter runs the same
    /// shape: query → fetch → verify → import.
    pub phases: Vec<SubstituterPhase>,
}

/// Wire protocol for the substituter.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubstituterTransport {
    /// `https://cache.nixos.org` style: GET narinfo, GET nar.
    Https,
    /// `s3://bucket/...`: AWS S3 / R2 / MinIO direct-object store.
    S3,
    /// `file:///nix-store/`: local filesystem mirror.
    Local,
    /// Attic (Cloudflare-backed authenticated cache from Determinate).
    Attic,
    /// SSH-tunnelled remote nix-store.
    SshNixStore,
}

/// How the substituter authenticates requests.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubstituterAuth {
    /// Public, no auth (cache.nixos.org).
    None,
    /// Static API key in `Authorization: Bearer ...` (Attic).
    BearerToken,
    /// AWS SigV4 (S3 / R2).
    AwsSigV4,
    /// SSH client cert (SshNixStore).
    SshClientCert,
}

/// How much trust the substituter's signatures get.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    /// Always accept paths signed by this substituter.
    Trusted,
    /// Accept only when the requesting user is in
    /// `trusted-users`.
    TrustedUsersOnly,
    /// Reject — substituter is configured but signatures are not
    /// honored.  Used for mirror-only caches.
    Untrusted,
}

/// One phase in a substituter run.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SubstituterPhase {
    pub kind: SubstituterPhaseKind,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
}

/// Closed set of substituter phases.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubstituterPhaseKind {
    /// GET `<endpoint>/<hash>.narinfo`.  Returns metadata
    /// including the NAR URL, file size, sha256, references,
    /// signatures.  Cache key for the rest of the pipeline.
    QueryNarInfo,
    /// GET the NAR file referenced by the narinfo's `URL:` field.
    FetchNar,
    /// Verify the narinfo's `Sig:` header(s) against the
    /// substituter's trusted public-key set.  Fails closed.
    VerifyNarSignature,
    /// Decompress (xz / zstd / gz / none) per the narinfo's
    /// `Compression:` header.
    DecompressNar,
    /// Verify the decompressed NAR's sha256 against the narinfo's
    /// `NarHash:`.  Belt-and-suspenders with the signature step.
    VerifyNarHash,
    /// Extract the NAR into `/nix/store/<hash>-<name>`, populating
    /// the store metadata (size, references, deriver).
    ImportNarToStore,
    /// Recursively substitute the path's `References:` from the
    /// same substituter chain.  The substitution algorithm is a
    /// closure; this phase drives the recursion.
    RealizeReferences,
}

// ── Spec interpreter (M3.0 minimal) ────────────────────────────────

/// Inputs to a substitution run.
pub struct SubstituteArgs {
    /// The store-path hash component (the 32-char base32 prefix
    /// of `/nix/store/<hash>-<name>`).
    pub store_path_hash: String,
    /// Optional display name for logs / store-write.
    pub name_hint: Option<String>,
}

/// Result of a substitution run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubstituteOutcome {
    /// The store path the substituted content landed at.
    pub store_path: String,
    /// The NAR hash recorded for this path (from the narinfo).
    pub nar_hash: String,
    /// Reference closure pulled in by this substitution.
    pub references: Vec<String>,
}

/// One narinfo record produced by `query_narinfo` and consumed by
/// the rest of the pipeline.  Typed border for the wire format
/// declared in [`crate::narinfo`].
#[derive(Debug, Clone, Default)]
pub struct NarInfoRecord {
    pub store_path: String,
    pub url: String,
    pub compression: String,
    pub file_hash: String,
    pub nar_hash: String,
    pub nar_size: u64,
    pub references: Vec<String>,
    /// `Sig:` lines from the narinfo, in document order.
    pub signatures: Vec<String>,
}

/// Abstract IO for the substituter.  Pattern parallel to
/// [`crate::fetcher::FetcherEnvironment`] — tests pass a mock,
/// production passes an HTTP + sig-verify + store-write impl.
pub trait SubstituterEnvironment {
    /// GET `<endpoint>/<hash>.narinfo`.  Returns the parsed record
    /// on hit, `None` on 404 / no-such-path.
    ///
    /// # Errors
    ///
    /// As above.
    fn query_narinfo(&self, endpoint: &str, hash: &str)
        -> Result<Option<NarInfoRecord>, String>;

    /// GET the NAR bytes referenced by the narinfo's `url` field.
    fn fetch_nar(&self, endpoint: &str, url: &str)
        -> Result<Vec<u8>, String>;

    /// Verify the narinfo's signatures against the substituter's
    /// trusted-keys set.  Returns true iff at least one signature
    /// validates.  Default impl trusts everything — production
    /// must override.
    fn verify_signatures(&self, _record: &NarInfoRecord) -> Result<bool, String> {
        Ok(true)
    }

    /// Decompress NAR bytes per the `compression` field.  Returns
    /// the raw NAR.
    fn decompress(&self, compression: &str, bytes: &[u8])
        -> Result<Vec<u8>, String>;

    /// Verify the decompressed bytes' hash matches the narinfo's
    /// `nar_hash` field.
    fn verify_nar_hash(&self, expected: &str, bytes: &[u8])
        -> Result<bool, String>;

    /// Import the NAR into the store at its declared path.
    /// Returns the full store path.
    fn import_nar(&self, store_path: &str, nar_bytes: &[u8])
        -> Result<String, String>;
}

/// Apply a substituter spec.  M3.0: drives the canonical phase
/// pipeline declared in `defsubstituter`.  Trust-level enforcement
/// runs at the VerifyNarSignature phase — `TrustLevel::Untrusted`
/// substituters skip signature checks; `Trusted` and
/// `TrustedUsersOnly` require at least one valid signature.
///
/// # Errors
///
/// - `query-narinfo`, `fetch-nar`, `verify-signature`,
///   `decompress`, `verify-nar-hash`, `import-nar` for the
///   corresponding phase failures.
/// - `narinfo-not-found` if the substituter doesn't have the path.
/// - `signature-required` for trusted substituters with no valid
///   signature on the record.
pub fn apply<E: SubstituterEnvironment>(
    spec: &SubstituterSpec,
    args: &SubstituteArgs,
    env: &E,
) -> Result<SubstituteOutcome, SpecError> {
    // Query narinfo first — always the first wire step.
    let narinfo = env
        .query_narinfo(&spec.endpoint, &args.store_path_hash)
        .map_err(|e| SpecError::Interp {
            phase: "query-narinfo".into(),
            message: format!("{}: {e}", spec.name),
        })?
        .ok_or_else(|| SpecError::Interp {
            phase: "narinfo-not-found".into(),
            message: format!(
                "substituter `{}` has no path for hash `{}`",
                spec.name, args.store_path_hash,
            ),
        })?;

    let mut nar_bytes: Option<Vec<u8>> = None;
    let mut decompressed: Option<Vec<u8>> = None;
    let mut import_path: Option<String> = None;

    for phase in &spec.phases {
        match phase.kind {
            SubstituterPhaseKind::QueryNarInfo => {
                // Already done above.  Re-running is idempotent
                // but wasteful; M3.0 leaves it as a no-op here.
            }
            SubstituterPhaseKind::FetchNar => {
                let bytes = env
                    .fetch_nar(&spec.endpoint, &narinfo.url)
                    .map_err(|e| SpecError::Interp {
                        phase: "fetch-nar".into(),
                        message: format!("{}: {e}", spec.name),
                    })?;
                nar_bytes = Some(bytes);
            }
            SubstituterPhaseKind::VerifyNarSignature => {
                if spec.trust_level == TrustLevel::Untrusted {
                    continue;  // skip signature check
                }
                let ok = env
                    .verify_signatures(&narinfo)
                    .map_err(|e| SpecError::Interp {
                        phase: "verify-signature".into(),
                        message: format!("{}: {e}", spec.name),
                    })?;
                if !ok {
                    return Err(SpecError::Interp {
                        phase: "signature-required".into(),
                        message: format!(
                            "substituter `{}` requires a valid signature \
                             but none verified for `{}`",
                            spec.name, narinfo.store_path,
                        ),
                    });
                }
            }
            SubstituterPhaseKind::DecompressNar => {
                let Some(bytes) = nar_bytes.as_deref() else {
                    return Err(SpecError::Interp {
                        phase: "phase-order".into(),
                        message: format!(
                            "{}: DecompressNar phase ran before FetchNar — \
                             phase ordering broken",
                            spec.name,
                        ),
                    });
                };
                let out = env
                    .decompress(&narinfo.compression, bytes)
                    .map_err(|e| SpecError::Interp {
                        phase: "decompress".into(),
                        message: format!("{}: {e}", spec.name),
                    })?;
                decompressed = Some(out);
            }
            SubstituterPhaseKind::VerifyNarHash => {
                let Some(bytes) = decompressed.as_deref().or(nar_bytes.as_deref()) else {
                    return Err(SpecError::Interp {
                        phase: "phase-order".into(),
                        message: format!(
                            "{}: VerifyNarHash ran without FetchNar bytes",
                            spec.name,
                        ),
                    });
                };
                let ok = env
                    .verify_nar_hash(&narinfo.nar_hash, bytes)
                    .map_err(|e| SpecError::Interp {
                        phase: "verify-nar-hash".into(),
                        message: format!("{}: {e}", spec.name),
                    })?;
                if !ok {
                    return Err(SpecError::Interp {
                        phase: "nar-hash-mismatch".into(),
                        message: format!(
                            "{}: NAR hash mismatch for `{}` (expected `{}`)",
                            spec.name, narinfo.store_path, narinfo.nar_hash,
                        ),
                    });
                }
            }
            SubstituterPhaseKind::ImportNarToStore => {
                let Some(bytes) = decompressed.as_deref().or(nar_bytes.as_deref()) else {
                    return Err(SpecError::Interp {
                        phase: "phase-order".into(),
                        message: format!(
                            "{}: ImportNarToStore ran without bytes",
                            spec.name,
                        ),
                    });
                };
                let path = env
                    .import_nar(&narinfo.store_path, bytes)
                    .map_err(|e| SpecError::Interp {
                        phase: "import-nar".into(),
                        message: format!("{}: {e}", spec.name),
                    })?;
                import_path = Some(path);
            }
            SubstituterPhaseKind::RealizeReferences => {
                // M3.0: the substituter only resolves the *direct*
                // path.  Walking References + recursively
                // substituting is the caller's job (the closure
                // walker outside this function).  Future M3.x
                // wires it up via a callback.
            }
        }
    }

    Ok(SubstituteOutcome {
        store_path: import_path.unwrap_or(narinfo.store_path),
        nar_hash: narinfo.nar_hash,
        references: narinfo.references,
    })
}

// ── Canonical spec ─────────────────────────────────────────────────

pub const CANONICAL_SUBSTITUTERS_LISP: &str =
    include_str!("../specs/substituters.lisp");

/// Compile every authored substituter spec.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<SubstituterSpec>, SpecError> {
    crate::loader::load_all::<SubstituterSpec>(CANONICAL_SUBSTITUTERS_LISP)
}

/// Return the substituter whose `name` matches.
///
/// # Errors
///
/// Returns an error if the spec fails to parse or `name` is missing.
pub fn load_named(name: &str) -> Result<SubstituterSpec, SpecError> {
    load_canonical()?
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| SpecError::Load(format!("no (defsubstituter) with :name {name:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_substituters_parse() {
        let specs = load_canonical().expect("canonical substituters must compile");
        assert!(!specs.is_empty());
    }

    #[test]
    fn cache_nixos_org_is_https_none() {
        let s = load_named("cache.nixos.org").unwrap();
        assert_eq!(s.transport, SubstituterTransport::Https);
        assert_eq!(s.auth, SubstituterAuth::None);
        assert_eq!(s.trust_level, TrustLevel::Trusted);
    }

    #[test]
    fn attic_is_https_or_attic_bearer() {
        let s = load_named("attic").unwrap();
        // Modern Attic uses HTTPS at the wire level, with bearer
        // auth.  Either transport tag is acceptable.
        assert!(
            matches!(s.transport, SubstituterTransport::Https | SubstituterTransport::Attic),
            "got transport {:?}",
            s.transport,
        );
        assert_eq!(s.auth, SubstituterAuth::BearerToken);
    }

    #[test]
    fn every_substituter_has_pipeline_essentials() {
        let specs = load_canonical().unwrap();
        for spec in &specs {
            let kinds: Vec<SubstituterPhaseKind> =
                spec.phases.iter().map(|p| p.kind).collect();
            for required in [
                SubstituterPhaseKind::QueryNarInfo,
                SubstituterPhaseKind::FetchNar,
                SubstituterPhaseKind::ImportNarToStore,
            ] {
                assert!(
                    kinds.contains(&required),
                    "{}: missing required phase {required:?}",
                    spec.name,
                );
            }
        }
    }

    #[test]
    fn trusted_substituters_verify_signatures() {
        let specs = load_canonical().unwrap();
        for spec in &specs {
            if matches!(spec.trust_level, TrustLevel::Trusted) {
                let kinds: Vec<SubstituterPhaseKind> =
                    spec.phases.iter().map(|p| p.kind).collect();
                assert!(
                    kinds.contains(&SubstituterPhaseKind::VerifyNarSignature),
                    "{}: trusted substituter must verify signatures",
                    spec.name,
                );
            }
        }
    }

    // ── M3.0 substituter interpreter tests ─────────────────────

    use std::cell::RefCell;
    use std::collections::HashMap;

    struct MockEnv {
        narinfos: HashMap<String, NarInfoRecord>,
        nars: HashMap<String, Vec<u8>>,
        store: RefCell<HashMap<String, Vec<u8>>>,
        verify_sig_returns: bool,
        nar_hash_matches: bool,
    }

    impl MockEnv {
        fn new() -> Self {
            Self {
                narinfos: HashMap::new(),
                nars: HashMap::new(),
                store: RefCell::new(HashMap::new()),
                verify_sig_returns: true,
                nar_hash_matches: true,
            }
        }
        fn with_path(mut self, hash: &str, record: NarInfoRecord, nar: Vec<u8>) -> Self {
            self.nars.insert(record.url.clone(), nar);
            self.narinfos.insert(hash.into(), record);
            self
        }
    }

    impl SubstituterEnvironment for MockEnv {
        fn query_narinfo(&self, _endpoint: &str, hash: &str)
            -> Result<Option<NarInfoRecord>, String>
        {
            Ok(self.narinfos.get(hash).cloned())
        }
        fn fetch_nar(&self, _endpoint: &str, url: &str) -> Result<Vec<u8>, String> {
            self.nars.get(url).cloned().ok_or_else(|| format!("no nar at {url}"))
        }
        fn verify_signatures(&self, _record: &NarInfoRecord) -> Result<bool, String> {
            Ok(self.verify_sig_returns)
        }
        fn decompress(&self, _compression: &str, bytes: &[u8])
            -> Result<Vec<u8>, String>
        {
            // Mock: pretend everything is already decompressed.
            Ok(bytes.to_vec())
        }
        fn verify_nar_hash(&self, _expected: &str, _bytes: &[u8])
            -> Result<bool, String>
        {
            Ok(self.nar_hash_matches)
        }
        fn import_nar(&self, store_path: &str, nar_bytes: &[u8])
            -> Result<String, String>
        {
            self.store.borrow_mut().insert(store_path.into(), nar_bytes.to_vec());
            Ok(store_path.into())
        }
    }

    fn rec(hash: &str, store_path: &str, nar: &str) -> NarInfoRecord {
        NarInfoRecord {
            store_path: store_path.into(),
            url: format!("nar/{hash}.nar.xz"),
            compression: "xz".into(),
            file_hash: format!("sha256:test-{hash}-file"),
            nar_hash: format!("sha256:test-{hash}-nar"),
            nar_size: nar.len() as u64,
            references: Vec::new(),
            signatures: vec![format!("cache.nixos.org-1:test-sig-{hash}")],
        }
    }

    #[test]
    fn substitute_happy_path_imports_to_store() {
        let spec = load_named("cache.nixos.org").unwrap();
        let env = MockEnv::new().with_path(
            "abc123",
            rec("abc123", "/nix/store/abc123-hello", "nar bytes"),
            b"nar bytes".to_vec(),
        );
        let args = SubstituteArgs {
            store_path_hash: "abc123".into(),
            name_hint: Some("hello".into()),
        };
        let outcome = apply(&spec, &args, &env).unwrap();
        assert_eq!(outcome.store_path, "/nix/store/abc123-hello");
        assert!(outcome.nar_hash.contains("abc123"));
        // The store recorded the bytes.
        assert!(env.store.borrow().contains_key("/nix/store/abc123-hello"));
    }

    #[test]
    fn substitute_missing_path_errors() {
        let spec = load_named("cache.nixos.org").unwrap();
        let env = MockEnv::new();
        let args = SubstituteArgs {
            store_path_hash: "nonexistent".into(),
            name_hint: None,
        };
        let err = apply(&spec, &args, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "narinfo-not-found"),
            _ => panic!("expected narinfo-not-found"),
        }
    }

    #[test]
    fn trusted_substituter_with_bad_signature_errors() {
        let spec = load_named("cache.nixos.org").unwrap(); // TrustLevel::Trusted
        let mut env = MockEnv::new().with_path(
            "abc",
            rec("abc", "/nix/store/abc-x", "bytes"),
            b"bytes".to_vec(),
        );
        env.verify_sig_returns = false;
        let args = SubstituteArgs {
            store_path_hash: "abc".into(),
            name_hint: None,
        };
        let err = apply(&spec, &args, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "signature-required"),
            _ => panic!("expected signature-required"),
        }
    }

    #[test]
    fn untrusted_substituter_skips_signature_check() {
        let spec = load_named("local-mirror").unwrap();  // TrustLevel::Untrusted
        let mut env = MockEnv::new().with_path(
            "xyz",
            rec("xyz", "/nix/store/xyz-y", "bytes"),
            b"bytes".to_vec(),
        );
        env.verify_sig_returns = false;
        let args = SubstituteArgs {
            store_path_hash: "xyz".into(),
            name_hint: None,
        };
        // Untrusted: bad signature is fine, import succeeds.
        let outcome = apply(&spec, &args, &env).unwrap();
        assert_eq!(outcome.store_path, "/nix/store/xyz-y");
    }

    #[test]
    fn nar_hash_mismatch_errors() {
        let spec = load_named("cache.nixos.org").unwrap();
        let mut env = MockEnv::new().with_path(
            "def",
            rec("def", "/nix/store/def-z", "bytes"),
            b"bytes".to_vec(),
        );
        env.nar_hash_matches = false;
        let args = SubstituteArgs {
            store_path_hash: "def".into(),
            name_hint: None,
        };
        let err = apply(&spec, &args, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "nar-hash-mismatch"),
            _ => panic!("expected nar-hash-mismatch"),
        }
    }

    #[test]
    fn references_returned_for_closure_walk() {
        let spec = load_named("cache.nixos.org").unwrap();
        let mut record = rec("ghi", "/nix/store/ghi-app", "bytes");
        record.references = vec![
            "/nix/store/dep1".into(),
            "/nix/store/dep2".into(),
        ];
        let env = MockEnv::new().with_path("ghi", record, b"bytes".to_vec());
        let args = SubstituteArgs {
            store_path_hash: "ghi".into(),
            name_hint: None,
        };
        let outcome = apply(&spec, &args, &env).unwrap();
        assert_eq!(outcome.references.len(), 2);
        assert!(outcome.references.contains(&"/nix/store/dep1".into()));
    }
}
