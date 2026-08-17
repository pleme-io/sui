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

// ── HttpTransport — typed blocking-HTTP boundary ────────────────────
//
// Lifted from `sui/src/main.rs` after three call sites consumed
// the same inline `http_get` helper.  The trait separates the
// IO boundary from the dispatch logic so tests can substitute a
// `MockTransport` while production wires the `UreqTransport`.

/// Typed blocking-HTTP boundary.  Consumers parse / hash the
/// returned bytes themselves; the transport handles only the
/// network round-trip.
pub trait HttpTransport {
    /// Fetch the bytes at `url`.  Returns the full body on
    /// success, a typed error otherwise.
    ///
    /// # Errors
    ///
    /// Implementations return their own error category; the
    /// fetcher wraps with `SpecError::Interp { phase: "http-get" }`
    /// when adapting.
    fn get(&self, url: &str) -> Result<Vec<u8>, HttpError>;
}

/// Typed transport error.  Constructors at the impl boundary
/// classify the failure so callers can branch on category.
///
/// # The classification was dead code until 2026-08-17
///
/// `NotFound` and `Forbidden` have existed here since the trait was lifted, and
/// **no production transport ever constructed either.** `FsTransport` produced
/// them from `io::ErrorKind` and `MockTransport` from a lookup miss, so both
/// arms were exercised only by tests; every real HTTP outcome — 403, 404, 429,
/// 500 — collapsed into `NetworkFailure(String)` at the two `ureq` call sites.
/// The enum advertised a distinction the live code did not make, which is the
/// most expensive shape a type can have: a reader checks the definition, sees
/// the arms, and reasonably concludes the information is available.
///
/// `Throttled` and `UnexpectedStatus` are new. `Throttled` is separate from
/// every other arm because its remedy is unlike theirs — another network egress
/// can fetch identical bytes, verifiable against a pinned hash — and
/// `UnexpectedStatus` exists so an unhandled code still reaches the caller as a
/// number rather than as prose.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HttpError {
    BadUrl(String),
    UnsupportedScheme(String),
    /// A genuine transport failure — DNS, TLS, connect timeout. **Not** a
    /// status: a response that arrived and said "no" is one of the arms below.
    NetworkFailure(String),
    NotFound(String),
    Forbidden(String),
    /// The upstream refused to serve content it has (429, or a 403 that carries
    /// `Retry-After`).  `retry_after` is the server's own advice in seconds.
    Throttled {
        url: String,
        status: u16,
        retry_after: Option<u64>,
    },
    /// Any other non-2xx, code preserved.
    UnexpectedStatus { url: String, status: u16 },
    BodyReadFailure(String),
    IoError(String),
}

impl HttpError {
    /// Whether a different network egress could fetch the identical bytes.
    /// True only for a throttle — a 403/404 is refused identically anywhere.
    #[must_use]
    pub fn is_throttled(&self) -> bool {
        matches!(self, Self::Throttled { .. })
    }

    /// Parse a raw `Retry-After` header value into seconds from now.
    ///
    /// The sibling of [`HttpError::from_status`], and it exists for the same
    /// reason that method's doc gives: **shared by every transport impl so it
    /// cannot drift between them.** `from_status` was already shared; the
    /// *parsing* was not, and both transports carried a byte-identical
    /// `s.trim().parse::<u64>().ok()` — delta-seconds only.
    ///
    /// Delegating to `handan` closes the gap that duplication hid: RFC 9110
    /// §10.2.3 defines **two** `Retry-After` forms, and the **HTTP-date** one
    /// (`Sun, 06 Nov 1994 08:49:37 GMT`) was read as *no header at all*. That is
    /// worse here than a missed optimisation, because
    /// [`HttpError::from_status`] treats `retry_after.is_some()` as the witness
    /// that a `403` is a throttle rather than a credential fault — so a
    /// date-form `403` was being classified `Forbidden`, sending an operator to
    /// replace a token that was fine. Exactly the misdiagnosis the `from_status`
    /// comment below warns about, arriving through the parser instead of the
    /// classifier.
    ///
    /// A date already past yields `0` (retry now), never an underflow.
    #[must_use]
    pub fn retry_after_secs(value: &str) -> Option<u64> {
        handan::parse_retry_after(value).map(|a| a.delay_now().as_secs())
    }

    /// Classify a status code plus an optional `Retry-After` into an arm.
    ///
    /// Pure, and shared by every transport impl so the mapping cannot drift
    /// between them — the drift that left this enum's arms unreachable.
    #[must_use]
    pub fn from_status(url: &str, status: u16, retry_after: Option<u64>) -> Self {
        // A 403 carrying Retry-After is GitHub's secondary rate limit, not an
        // authorization failure. Believe the header over the code: reading a
        // throttle as a credential fault sends an operator to replace a token
        // that is fine.
        if status == 429 || (status == 403 && retry_after.is_some()) {
            Self::Throttled {
                url: url.to_string(),
                status,
                retry_after,
            }
        } else if status == 404 {
            Self::NotFound(url.to_string())
        } else if status == 401 || status == 403 {
            Self::Forbidden(url.to_string())
        } else {
            Self::UnexpectedStatus {
                url: url.to_string(),
                status,
            }
        }
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadUrl(m)            => write!(f, "bad URL: {m}"),
            Self::UnsupportedScheme(s) => write!(f, "unsupported scheme `{s}`"),
            Self::NetworkFailure(m)    => write!(f, "network: {m}"),
            Self::NotFound(m)          => write!(f, "not found: {m}"),
            Self::Forbidden(m)         => write!(f, "forbidden: {m}"),
            Self::BodyReadFailure(m)   => write!(f, "body read: {m}"),
            // Distinct bytes per arm is the contract (★★ kotae), so these two
            // must not read like `NotFound`/`Forbidden` or like each other.
            Self::Throttled { url, status, retry_after } => match retry_after {
                Some(s) => write!(f, "throttled by {url} (HTTP {status}), retry after {s}s"),
                None => write!(f, "throttled by {url} (HTTP {status})"),
            },
            Self::UnexpectedStatus { url, status } => {
                write!(f, "{url} returned HTTP {status}")
            }
            Self::IoError(m)           => write!(f, "io: {m}"),
        }
    }
}

impl std::error::Error for HttpError {}

/// Filesystem-backed transport — resolves `file://` URLs.
/// Always available; no dependency on a network stack.
pub struct FsTransport;

impl HttpTransport for FsTransport {
    fn get(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        let parsed = url::Url::parse(url).map_err(|e| HttpError::BadUrl(e.to_string()))?;
        if parsed.scheme() != "file" {
            return Err(HttpError::UnsupportedScheme(parsed.scheme().to_string()));
        }
        let path = parsed.to_file_path()
            .map_err(|_| HttpError::BadUrl(format!("non-file URL `{url}`")))?;
        std::fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => HttpError::NotFound(path.display().to_string()),
            std::io::ErrorKind::PermissionDenied => HttpError::Forbidden(path.display().to_string()),
            _ => HttpError::IoError(e.to_string()),
        })
    }
}

/// Mock transport for tests — yields canned responses keyed by URL.
/// Lookup misses produce `NotFound`.
#[derive(Default)]
pub struct MockTransport {
    pub responses: std::collections::HashMap<String, Vec<u8>>,
}

impl MockTransport {
    /// Add a canned response for `url`.
    pub fn with(mut self, url: &str, bytes: Vec<u8>) -> Self {
        self.responses.insert(url.to_string(), bytes);
        self
    }
}

impl HttpTransport for MockTransport {
    fn get(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        self.responses.get(url).cloned()
            .ok_or_else(|| HttpError::NotFound(url.to_string()))
    }
}

/// A dispatcher that picks the right transport based on the
/// URL scheme — `file://` → [`FsTransport`], everything else
/// → the provided remote transport.
pub struct SchemeRouter<R: HttpTransport> {
    pub remote: R,
}

impl<R: HttpTransport> SchemeRouter<R> {
    pub fn new(remote: R) -> Self { Self { remote } }
}

impl<R: HttpTransport> HttpTransport for SchemeRouter<R> {
    fn get(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        let parsed = url::Url::parse(url).map_err(|e| HttpError::BadUrl(e.to_string()))?;
        match parsed.scheme() {
            "file" => FsTransport.get(url),
            "http" | "https" => self.remote.get(url),
            other => Err(HttpError::UnsupportedScheme(other.to_string())),
        }
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
mod status_classification_tests {
    use super::*;

    const URL: &str = "https://api.github.com/repos/o/r/tarball/deadbeef";

    #[test]
    fn from_status_maps_every_documented_case() {
        // Table-driven so a new arm cannot be added without a row, and so the
        // 429/403 pair sits visibly adjacent — it is the distinction that
        // matters and the one most easily got backwards.
        let cases: &[(u16, Option<u64>, &str)] = &[
            (429, Some(120), "throttle with the server's own advice"),
            (429, None, "throttle with no advice — still a throttle"),
            (403, Some(60), "GitHub's secondary rate limit, spelled 403"),
            (403, None, "a real authorization failure"),
            (401, None, "unauthenticated"),
            (404, None, "absent, or invisible to this credential"),
            (500, None, "an unhandled status, code preserved"),
            (503, Some(5), "unhandled status that happens to advise a retry"),
        ];
        for (status, retry, why) in cases {
            let e = HttpError::from_status(URL, *status, *retry);
            match (*status, *retry) {
                (429, r) => assert!(
                    matches!(&e, HttpError::Throttled { retry_after, .. } if *retry_after == r),
                    "{why}: got {e:?}"
                ),
                (403, Some(_)) => assert!(e.is_throttled(), "{why}: got {e:?}"),
                (403 | 401, None) => assert!(matches!(e, HttpError::Forbidden(_)), "{why}: got {e:?}"),
                (404, _) => assert!(matches!(e, HttpError::NotFound(_)), "{why}: got {e:?}"),
                (s, _) => assert!(
                    matches!(&e, HttpError::UnexpectedStatus { status, .. } if *status == s),
                    "{why}: got {e:?}"
                ),
            }
        }
    }

    #[test]
    fn only_a_throttle_claims_another_egress_would_help() {
        // The load-bearing predicate. A 403/404 is refused identically from any
        // host, so answering true there would fund a second fetch path that
        // cannot work.
        assert!(HttpError::from_status(URL, 429, None).is_throttled());
        assert!(HttpError::from_status(URL, 403, Some(1)).is_throttled());
        for s in [401, 403, 404, 500, 503] {
            assert!(
                !HttpError::from_status(URL, s, None).is_throttled(),
                "HTTP {s} without Retry-After must not claim recoverability"
            );
        }
        assert!(!HttpError::NetworkFailure("dns".into()).is_throttled());
    }

    #[test]
    fn no_two_arms_render_the_same_bytes_at_one_status() {
        // ★★ kotae, tested at a CONSTANT status so the comparison is about the
        // arms rather than about an interpolated number. (The sibling test in
        // sui-eval was first written the other way and proved blind under a
        // red run — same mistake is available here.)
        let variants = [
            HttpError::Throttled { url: URL.into(), status: 404, retry_after: None },
            HttpError::Throttled { url: URL.into(), status: 404, retry_after: Some(9) },
            HttpError::NotFound(URL.into()),
            HttpError::Forbidden(URL.into()),
            HttpError::UnexpectedStatus { url: URL.into(), status: 404 },
            HttpError::NetworkFailure(URL.into()),
            HttpError::BodyReadFailure(URL.into()),
        ];
        for (i, a) in variants.iter().enumerate() {
            for b in variants.iter().skip(i + 1) {
                assert_ne!(a.to_string(), b.to_string(), "{a:?} and {b:?} read alike");
            }
        }
    }

    #[test]
    fn the_previously_dead_arms_are_now_reachable_from_a_status() {
        // The regression guard for the actual defect: NotFound and Forbidden
        // existed for months and no production transport could produce them.
        // Anything that returns to routing status→NetworkFailure fails here.
        assert!(matches!(HttpError::from_status(URL, 404, None), HttpError::NotFound(_)));
        assert!(matches!(HttpError::from_status(URL, 403, None), HttpError::Forbidden(_)));
        assert!(!matches!(
            HttpError::from_status(URL, 404, None),
            HttpError::NetworkFailure(_)
        ));
    }

    /// Both RFC 9110 `Retry-After` forms are read, and the HTTP-date form
    /// reaches the classifier rather than being dropped.
    ///
    /// This is not a parsing nicety. `from_status` uses `retry_after.is_some()`
    /// as its witness that a `403` is a throttle and not a credential fault, so
    /// while the parser handled delta-seconds only, a **date-form `403` was
    /// classified `Forbidden`** — sending an operator to replace a token that
    /// was fine. Same misdiagnosis `from_status`'s own comment warns about,
    /// arriving through the parser instead of the classifier.
    ///
    /// The date asserted is in the PAST on purpose: `retry_after_secs` resolves
    /// against the system clock, so a future date would pass today and start
    /// failing when it arrives. A past date yields `0` for every possible "now".
    #[test]
    fn the_http_date_retry_after_form_reaches_the_classifier() {
        // delta-seconds — the form that always worked
        assert_eq!(HttpError::retry_after_secs("120"), Some(120));
        // HTTP-date — previously dropped entirely
        assert_eq!(
            HttpError::retry_after_secs("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(0),
            "a past date means retry now, not no-header"
        );
        // ...and unreadable values still decline, so callers fall back to backoff
        assert_eq!(HttpError::retry_after_secs("soon"), None);

        // The consequence that matters: a 403 whose advice came in date form is
        // a throttle, not a Forbidden.
        let advice = HttpError::retry_after_secs("Sun, 06 Nov 1994 08:49:37 GMT");
        assert!(
            HttpError::from_status(URL, 403, advice).is_throttled(),
            "a date-form 403 is GitHub's secondary rate limit, not a credential fault"
        );
        // Unchanged: a 403 with no advice at all is still a real denial.
        assert!(matches!(
            HttpError::from_status(URL, 403, None),
            HttpError::Forbidden(_)
        ));
    }
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
