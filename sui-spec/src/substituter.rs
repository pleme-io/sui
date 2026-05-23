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

// ── Spec interpreter (M3 stub) ─────────────────────────────────────

/// Inputs to a substitution run.  M3 will replace these with the
/// typed wire values.
pub struct SubstituteArgs {
    pub store_path_hash: String,
    pub name_hint: Option<String>,
}

/// Apply the substituter spec.  M3 stub — returns typed
/// `not-yet-implemented`.
///
/// # Errors
///
/// Always until M3.
pub fn apply(_spec: &SubstituterSpec, _args: SubstituteArgs) -> Result<String, SpecError> {
    Err(SpecError::Interp {
        phase: "substituter".into(),
        message: "substituter spec interpreter not yet implemented — \
                  M3 work lifts sui-store + sui-cache to consume this \
                  typed border".into(),
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

    #[test]
    fn apply_is_typed_not_yet() {
        let s = load_named("cache.nixos.org").unwrap();
        let err = apply(
            &s,
            SubstituteArgs {
                store_path_hash: "abc123".into(),
                name_hint: Some("hello".into()),
            },
        )
        .expect_err("apply must return error until M3");
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "substituter");
                assert!(message.contains("not yet implemented"));
            }
            _ => panic!("expected SpecError::Interp"),
        }
    }
}
