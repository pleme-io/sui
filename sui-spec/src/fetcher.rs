//! Typed border for nix's ingest layer — `builtins.fetchurl`,
//! `fetchTarball`, `fetchGit`, `fetchTree`, `path`.
//!
//! Every fetcher takes a `(url, hash)` input and produces a store
//! path.  The path is computed deterministically from the hash (it's
//! always a fixed-output derivation, see [`crate::derivation`]).
//! The difference between fetchers is the *transport* (HTTP, git
//! protocol, local fs) and the *hash mode* (Flat for single files,
//! Recursive for trees).
//!
//! Per the constructive substrate engineering pattern, the contract
//! lives here as a typed Rust border + a Lisp spec.  sui-eval's
//! `fetchers` builtin module today implements each in Rust; M3 work
//! lifts the implementations to consume this spec so the Rust side
//! is generated from the authored algorithm rather than handwritten.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (deffetcher
//!   :name        "fetchurl"
//!   :transport   Http
//!   :hash-mode   Flat
//!   :output-kind FixedOutput
//!   :phases ((:kind ValidateUrl)
//!            (:kind FetchBytes :bind "bytes")
//!            (:kind CheckHash :from "bytes")
//!            (:kind WriteToStore :from "bytes")))
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border ───────────────────────────────────────────────────

/// One fetcher authored as `(deffetcher …)`.  Variants by transport
/// + hash mode cover every cppnix builtin in the ingest layer.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "deffetcher")]
pub struct FetcherSpec {
    /// `"fetchurl"`, `"fetchTarball"`, `"fetchGit"`, `"fetchTree"`,
    /// `"path"`.
    pub name: String,
    /// Network / filesystem transport.
    pub transport: FetchTransport,
    /// Hash computation mode for the output.
    #[serde(rename = "hashMode")]
    pub hash_mode: FetchHashMode,
    /// Which derivation variant the fetcher produces.  All known
    /// nix fetchers are fixed-output — but CA-derivations may
    /// eventually let some be ContentAddressed.
    #[serde(rename = "outputKind")]
    pub output_kind: FetcherOutputKind,
    /// Phase pipeline.  Each fetcher runs phases left-to-right; the
    /// transport phase decides HOW to fetch, the hash phase decides
    /// the result's identity.
    pub phases: Vec<FetcherPhase>,
}

/// Where the fetcher reads bytes from.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FetchTransport {
    /// `fetchurl` / `fetchTarball` — plain HTTP/HTTPS GET.
    Http,
    /// `fetchGit` — git clone + checkout.  Uses sui-eval's gix
    /// integration today.
    Git,
    /// `fetchTree` — polymorphic dispatch by URL scheme; resolves
    /// to one of Http/Git/Mercurial/Path internally.
    Tree,
    /// `builtins.path` — local filesystem copy + hash.
    LocalPath,
    /// `fetchMercurial` — hg-protocol clone.  Present in cppnix
    /// behind experimental flag.
    Mercurial,
}

/// How the fetcher computes the result's content hash.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchHashMode {
    /// Hash the bytes directly.  Used for single files (e.g. a
    /// tarball downloaded by fetchurl with `unpack = false`).
    Flat,
    /// NAR-hash the unpacked tree.  Used for fetchTarball with
    /// `unpack = true`, fetchGit, fetchTree, and builtins.path
    /// on directories.
    Recursive,
    /// SRI hash format passthrough (sha256-base64=).  Modern
    /// surface; supersedes Flat/Recursive for many call sites.
    Sri,
}

/// The cppnix derivation variant the fetcher emits.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetcherOutputKind {
    /// `outputHash` + `outputHashMode` set on the derivation; path
    /// stable across reruns.  All known fetchers today.
    FixedOutput,
    /// Path computed from realised content (M4 — CA-drv).
    ContentAddressed,
}

/// One phase in a fetcher pipeline.  Flat-kwarg shape matches the
/// other spec domains.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FetcherPhase {
    pub kind: FetcherPhaseKind,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
}

/// Closed set of fetcher phases.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetcherPhaseKind {
    /// Reject malformed URLs / disallowed schemes.  Run before any
    /// network call.
    ValidateUrl,
    /// Resolve a flake-input reference (`github:owner/repo/ref`)
    /// into a concrete URL + commit pinned by the registry.
    /// Skipped for direct-URL fetchers.
    ResolveRegistryRef,
    /// Fetch raw bytes from the transport.  Binds to `:bind`.
    FetchBytes,
    /// Unpack a tarball / git-bundle into a tree.  Skipped for
    /// flat fetchers.
    Unpack,
    /// Compute the content hash of `:from` and verify against the
    /// declared hash.  Mismatch is fatal.
    CheckHash,
    /// Write the fetched content into the store at the computed
    /// FOD path.  Binds the store path.
    WriteToStore,
    /// Cache the fetched bytes (or their hash) in the eval cache
    /// keyed by URL.  Enables fast re-eval without re-fetching.
    CacheLookup,
    /// Emit an `<input>.narHash` style attribute that downstream
    /// flake-eval consumes.
    EmitNarHash,
}

// ── Spec interpreter (M3.0 minimal — fetchurl path) ───────────────

/// Inputs to a fetcher run.
pub struct FetchArgs {
    pub url: String,
    pub declared_hash: Option<String>,
    pub name_hint: Option<String>,
}

/// Result of a fetcher run — the store path the fetched content
/// landed at, plus its content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutcome {
    pub store_path: String,
    pub nar_hash: String,
}

/// Abstract IO environment for the fetcher.  Tests pass a mock
/// implementation that returns canned HTTP responses + a virtual
/// store; production uses a real HTTP client + filesystem.
///
/// Per the prime directive: trait-driven IO means the interpreter
/// is pure-logic and trivially testable.  When sui-eval consumes
/// this layer it ships a `FetcherEnvironment` impl that wraps
/// `ureq` (HTTP) + `sui_store::LocalStore` (the store).
pub trait FetcherEnvironment {
    /// Fetch bytes from a URL.  Returns the raw response body on
    /// success.
    ///
    /// # Errors
    ///
    /// Implementations return their own error which the fetcher
    /// converts to `SpecError::Interp { phase: "fetch-bytes" }`.
    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String>;

    /// Compute the SHA-256 of bytes, encoded as
    /// `sha256:<lowercase-hex>` for compatibility with the declared
    /// hash format.  Implementations may use sha2 or the hardware
    /// path; the spec only requires byte-exact equivalence.
    fn hash_bytes(&self, bytes: &[u8]) -> String;

    /// Persist bytes to the store at the FOD-derived path for the
    /// given name.  Returns the full `/nix/store/...` path.
    ///
    /// # Errors
    ///
    /// As above.
    fn write_to_store(&self, name: &str, bytes: &[u8]) -> Result<String, String>;

    /// Optional cache lookup — if the bytes are already in the
    /// store under this hash, skip fetching.  Returns
    /// `Ok(Some(store_path))` on hit, `Ok(None)` on miss.  Default
    /// impl always misses, which is correct but suboptimal.
    fn cache_lookup(&self, _name: &str, _declared_hash: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// Apply a fetcher spec.  M3.0 implementation: supports the
/// fetchurl transport (Http + Flat hash mode).  Other transports
/// return a typed `not-yet-implemented` error.
///
/// # Errors
///
/// - `SpecError::Interp { phase: "url-validate" }` for malformed URLs.
/// - `SpecError::Interp { phase: "fetch-bytes" }` if the environment
///   couldn't fetch.
/// - `SpecError::Interp { phase: "hash-mismatch" }` if the declared
///   hash doesn't match the fetched content's hash.
/// - `SpecError::Interp { phase: "write-to-store" }` on store
///   failure.
/// - `SpecError::Interp { phase: "fetcher-unimplemented" }` for
///   non-fetchurl transports (M3.1+).
pub fn apply<E: FetcherEnvironment>(
    spec: &FetcherSpec,
    args: &FetchArgs,
    env: &E,
) -> Result<FetchOutcome, SpecError> {
    // M3.0 only handles Http + Flat (i.e., fetchurl).  Others
    // surface as typed "not yet" errors.
    if spec.transport != FetchTransport::Http
        || spec.hash_mode != FetchHashMode::Flat
    {
        return Err(SpecError::Interp {
            phase: "fetcher-unimplemented".into(),
            message: format!(
                "fetcher `{}` (transport {:?}, hash-mode {:?}) — \
                 M3.0 supports only Http+Flat (fetchurl).  M3.1+ \
                 implementations land per-transport.",
                spec.name, spec.transport, spec.hash_mode,
            ),
        });
    }

    let name = args.name_hint.as_deref().unwrap_or("download");

    // Drive the authored phase pipeline.
    for phase in &spec.phases {
        match phase.kind {
            FetcherPhaseKind::ValidateUrl => validate_url(&args.url)?,
            FetcherPhaseKind::ResolveRegistryRef => {
                // fetchurl doesn't take a registry ref — no-op.
            }
            FetcherPhaseKind::CacheLookup => {
                if let Some(declared) = args.declared_hash.as_deref() {
                    let hit = env
                        .cache_lookup(name, declared)
                        .map_err(|e| SpecError::Interp {
                            phase: "cache-lookup".into(),
                            message: e,
                        })?;
                    if let Some(path) = hit {
                        return Ok(FetchOutcome {
                            store_path: path,
                            nar_hash: declared.to_string(),
                        });
                    }
                }
            }
            FetcherPhaseKind::FetchBytes => {
                // Handled by the FetchBytes → CheckHash → WriteToStore
                // chain below; we run them in one block because they
                // share the in-memory body buffer.
            }
            FetcherPhaseKind::Unpack => {
                // Flat-hash fetchers don't unpack; no-op.
            }
            FetcherPhaseKind::CheckHash | FetcherPhaseKind::WriteToStore
            | FetcherPhaseKind::EmitNarHash => {
                // See block below.
            }
        }
    }

    // Drive the core fetch → hash → store chain.
    let bytes = env.fetch_bytes(&args.url).map_err(|e| SpecError::Interp {
        phase: "fetch-bytes".into(),
        message: format!("fetching `{}`: {e}", args.url),
    })?;

    let computed = env.hash_bytes(&bytes);
    if let Some(declared) = args.declared_hash.as_deref() {
        if declared != computed {
            return Err(SpecError::Interp {
                phase: "hash-mismatch".into(),
                message: format!(
                    "hash mismatch for `{}`: declared {declared}, got {computed}",
                    args.url,
                ),
            });
        }
    }

    let store_path = env
        .write_to_store(name, &bytes)
        .map_err(|e| SpecError::Interp {
            phase: "write-to-store".into(),
            message: format!("writing `{name}`: {e}"),
        })?;

    Ok(FetchOutcome { store_path, nar_hash: computed })
}

fn validate_url(url: &str) -> Result<(), SpecError> {
    if url.is_empty() {
        return Err(SpecError::Interp {
            phase: "url-validate".into(),
            message: "url is empty".into(),
        });
    }
    let allowed = ["http://", "https://", "file://"];
    if !allowed.iter().any(|p| url.starts_with(p)) {
        return Err(SpecError::Interp {
            phase: "url-validate".into(),
            message: format!(
                "url `{url}` uses an unsupported scheme \
                 (allowed: http://, https://, file://)",
            ),
        });
    }
    Ok(())
}

// ── Canonical spec ─────────────────────────────────────────────────

pub const CANONICAL_FETCHERS_LISP: &str = include_str!("../specs/fetchers.lisp");

/// Compile every authored fetcher spec.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<FetcherSpec>, SpecError> {
    crate::loader::load_all::<FetcherSpec>(CANONICAL_FETCHERS_LISP)
}

/// Return the fetcher whose `name` matches.
///
/// # Errors
///
/// Returns an error if the spec fails to parse or `name` is missing.
pub fn load_named(name: &str) -> Result<FetcherSpec, SpecError> {
    load_canonical()?
        .into_iter()
        .find(|f| f.name == name)
        .ok_or_else(|| SpecError::Load(format!("no (deffetcher) with :name {name:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn canonical_fetchers_parse() {
        let specs = load_canonical().expect("canonical fetchers must compile");
        assert!(!specs.is_empty());
    }

    #[test]
    fn every_cppnix_fetcher_named() {
        let specs = load_canonical().unwrap();
        let names: HashSet<&str> = specs.iter().map(|f| f.name.as_str()).collect();
        // The five cppnix builtins.* fetchers.  If any is missing,
        // the ingest layer is incomplete.
        for required in ["fetchurl", "fetchTarball", "fetchGit", "fetchTree", "path"] {
            assert!(
                names.contains(required),
                "canonical fetcher corpus missing `{required}`",
            );
        }
    }

    #[test]
    fn fetchurl_uses_http_flat() {
        let f = load_named("fetchurl").unwrap();
        assert_eq!(f.transport, FetchTransport::Http);
        assert_eq!(f.hash_mode, FetchHashMode::Flat);
        assert_eq!(f.output_kind, FetcherOutputKind::FixedOutput);
    }

    #[test]
    fn fetchgit_uses_git_recursive() {
        let f = load_named("fetchGit").unwrap();
        assert_eq!(f.transport, FetchTransport::Git);
        assert_eq!(f.hash_mode, FetchHashMode::Recursive);
    }

    #[test]
    fn every_fetcher_has_validate_and_writetostore() {
        let specs = load_canonical().unwrap();
        for spec in &specs {
            let kinds: Vec<FetcherPhaseKind> =
                spec.phases.iter().map(|p| p.kind).collect();
            assert!(
                kinds.contains(&FetcherPhaseKind::ValidateUrl)
                    || spec.transport == FetchTransport::LocalPath,
                "{}: every network fetcher must ValidateUrl",
                spec.name,
            );
            assert!(
                kinds.contains(&FetcherPhaseKind::WriteToStore),
                "{}: missing WriteToStore",
                spec.name,
            );
        }
    }

    // ── M3.0 fetcher interpreter tests ─────────────────────────

    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Mock environment for fetcher tests.  Backed by HashMaps —
    /// pre-load with URL→bytes pairs, then assert on what the
    /// fetcher recorded.
    struct MockEnv {
        responses: HashMap<String, Vec<u8>>,
        store: RefCell<HashMap<String, Vec<u8>>>,
    }

    impl MockEnv {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
                store: RefCell::new(HashMap::new()),
            }
        }
        fn with_response(mut self, url: &str, body: &[u8]) -> Self {
            self.responses.insert(url.into(), body.to_vec());
            self
        }
    }

    impl FetcherEnvironment for MockEnv {
        fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| format!("no canned response for {url}"))
        }
        fn hash_bytes(&self, bytes: &[u8]) -> String {
            // Deterministic stand-in — just length-prefixed hex of
            // the first byte.  Real env uses sha2.  Good enough to
            // prove the fetcher routes correctly.
            let first = bytes.first().copied().unwrap_or(0);
            format!("sha256:test-{}-{:02x}", bytes.len(), first)
        }
        fn write_to_store(&self, name: &str, bytes: &[u8]) -> Result<String, String> {
            let path = format!("/nix/store/abc-{name}");
            self.store.borrow_mut().insert(path.clone(), bytes.to_vec());
            Ok(path)
        }
    }

    #[test]
    fn fetchurl_happy_path() {
        let spec = load_named("fetchurl").unwrap();
        let env = MockEnv::new()
            .with_response("https://example.com/hello.tar", b"hello\n");
        let args = FetchArgs {
            url: "https://example.com/hello.tar".into(),
            declared_hash: None,
            name_hint: Some("hello.tar".into()),
        };
        let outcome = apply(&spec, &args, &env).unwrap();
        assert_eq!(outcome.store_path, "/nix/store/abc-hello.tar");
        assert!(outcome.nar_hash.starts_with("sha256:"));
        // The store recorded our bytes.
        assert_eq!(
            env.store.borrow().get("/nix/store/abc-hello.tar"),
            Some(&b"hello\n".to_vec()),
        );
    }

    #[test]
    fn fetchurl_rejects_malformed_url() {
        let spec = load_named("fetchurl").unwrap();
        let env = MockEnv::new();
        let args = FetchArgs {
            url: "ftp://example.com/x".into(),
            declared_hash: None,
            name_hint: None,
        };
        let err = apply(&spec, &args, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "url-validate"),
            _ => panic!("expected url-validate error"),
        }
    }

    #[test]
    fn fetchurl_rejects_empty_url() {
        let spec = load_named("fetchurl").unwrap();
        let env = MockEnv::new();
        let args = FetchArgs {
            url: String::new(),
            declared_hash: None,
            name_hint: None,
        };
        let err = apply(&spec, &args, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "url-validate"),
            _ => panic!("expected url-validate"),
        }
    }

    #[test]
    fn fetchurl_verifies_declared_hash() {
        let spec = load_named("fetchurl").unwrap();
        let env = MockEnv::new()
            .with_response("https://example.com/x", b"hello");
        // The mock env's hash_bytes returns "sha256:test-5-68" for "hello".
        // Test with a deliberately wrong declared hash.
        let args = FetchArgs {
            url: "https://example.com/x".into(),
            declared_hash: Some("sha256:fake-hash".into()),
            name_hint: Some("x".into()),
        };
        let err = apply(&spec, &args, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "hash-mismatch");
                assert!(message.contains("fake-hash"));
            }
            _ => panic!("expected hash-mismatch"),
        }
    }

    #[test]
    fn fetchurl_accepts_matching_hash() {
        let spec = load_named("fetchurl").unwrap();
        let body = b"hello";
        let env = MockEnv::new().with_response("https://example.com/x", body);
        // Pre-compute the expected hash from the mock.
        let expected = env.hash_bytes(body);
        let args = FetchArgs {
            url: "https://example.com/x".into(),
            declared_hash: Some(expected.clone()),
            name_hint: Some("x".into()),
        };
        let outcome = apply(&spec, &args, &env).unwrap();
        assert_eq!(outcome.nar_hash, expected);
    }

    #[test]
    fn cache_hit_short_circuits_fetch() {
        struct CacheHitEnv;
        impl FetcherEnvironment for CacheHitEnv {
            fn fetch_bytes(&self, _: &str) -> Result<Vec<u8>, String> {
                Err("fetch should NOT have been called on cache hit".into())
            }
            fn hash_bytes(&self, _: &[u8]) -> String { unreachable!() }
            fn write_to_store(&self, _: &str, _: &[u8]) -> Result<String, String> {
                unreachable!()
            }
            fn cache_lookup(&self, _: &str, h: &str) -> Result<Option<String>, String> {
                Ok(Some(format!("/nix/store/cached-{h}")))
            }
        }
        let spec = load_named("fetchurl").unwrap();
        let args = FetchArgs {
            url: "https://example.com/x".into(),
            declared_hash: Some("sha256:abc".into()),
            name_hint: Some("x".into()),
        };
        let outcome = apply(&spec, &args, &CacheHitEnv).unwrap();
        assert_eq!(outcome.store_path, "/nix/store/cached-sha256:abc");
    }

    #[test]
    fn non_fetchurl_transport_returns_typed_not_yet() {
        // fetchGit uses Git transport — M3.0 doesn't implement.
        let spec = load_named("fetchGit").unwrap();
        let env = MockEnv::new();
        let args = FetchArgs {
            url: "https://example.com/repo.git".into(),
            declared_hash: None,
            name_hint: Some("repo".into()),
        };
        let err = apply(&spec, &args, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "fetcher-unimplemented");
                assert!(message.contains("Git"));
            }
            _ => panic!("expected fetcher-unimplemented"),
        }
    }
}
