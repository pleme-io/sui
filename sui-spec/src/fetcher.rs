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

// ── Spec interpreter (M3 stub) ─────────────────────────────────────

/// Inputs to a fetcher run.  M3 implementation fills the scratchpad
/// as phases run.
pub struct FetchArgs {
    pub url: String,
    pub declared_hash: Option<String>,
    pub name_hint: Option<String>,
}

/// Apply the fetcher algorithm.  M3 stub — returns typed
/// `not-yet-implemented` so any consumer surfaces the gap.
///
/// # Errors
///
/// Always returns `SpecError::Interp { phase: "fetcher" }` until M3.
pub fn apply(_spec: &FetcherSpec, _args: FetchArgs) -> Result<String, SpecError> {
    Err(SpecError::Interp {
        phase: "fetcher".into(),
        message: "fetcher spec interpreter not yet implemented — \
                  M3 work lifts sui-eval/src/builtins/fetchers.rs \
                  to consume this typed spec".into(),
    })
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

    #[test]
    fn apply_is_typed_not_yet() {
        let f = load_named("fetchurl").unwrap();
        let err = apply(
            &f,
            FetchArgs {
                url: "https://example.com/x.tar.gz".into(),
                declared_hash: None,
                name_hint: None,
            },
        )
        .expect_err("apply must return error until M3");
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "fetcher");
                assert!(message.contains("not yet implemented"));
            }
            _ => panic!("expected SpecError::Interp"),
        }
    }
}
