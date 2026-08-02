//! D6 — **fall-through safety**, proven for every cache-side failure
//! mode: the wrapper is a *cache accelerator in front of a plain
//! `docker build`*, so for **every** way the accelerator can be
//! unavailable it degrades to a real full `docker build` and **never**
//! returns a broken or partially-spliced result.
//!
//! The five failure modes, each mapped to a mock of one injected seam
//! ([`CommandRunner`] and [`StorageBackend`], both already the crate's
//! testability contract):
//!
//! | Failure mode          | Modeled by                                              |
//! |-----------------------|---------------------------------------------------------|
//! | cache miss            | an empty [`MockCacheBackend`]                           |
//! | backend unreachable   | [`ErroringCacheBackend`] — every op returns `CacheError`|
//! | malformed spec        | a Dockerfile the scoped graph hasher rejects (`HEALTHCHECK`, unresolved `$ARG`, empty `VOLUME`, unsupported instruction) — real docker would build it |
//! | partial store         | a [`MockCacheBackend`] seeded with only *some* nodes    |
//! | daemon absent         | the [`DaemonAwareCacheClient`] wrapping an absent socket over an empty remote — it degrades to the remote, which misses |
//!
//! In every case the assertion is the same D6 invariant:
//!
//! 1. exactly one `docker build` invocation was issued to the runner
//!    (a **full** real build — never a `pull`, never a second spliced
//!    build, never zero builds);
//! 2. the receipt's outcome is `CacheMiss` (a real build ran) or
//!    `BuildFailed` (the real build itself failed) — never `CacheHit`,
//!    never a `WrapperError` propagated out of a cache-side problem;
//! 3. `receipt.docker_ran == true`.
//!
//! No real docker daemon is touched — every subprocess is the recording
//! [`MockCommandRunner`]. These are pure, deterministic, non-flaky
//! properties over the two seams.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use proptest::prelude::*;

use sui_cache::StorageBackend;
use sui_cache::CacheError;
use sui_dockerfile_wrapper::cache::MockCacheBackend;
use sui_dockerfile_wrapper::command::MockCommandRunner;
use sui_dockerfile_wrapper::{
    run_wrapper, DaemonAwareCacheClient, WrapperConfig, WrapperOutcome, WrapperReceipt,
};
use sui_spec::dockerfile::{self, DockerfileArgs, MockDockerfileEnvironment};

// ── Fleet proptest floor ────────────────────────────────────────────
const CASES: u32 = 256;

// ── The one D6 assertion, factored out ──────────────────────────────

/// Assert the D6 fall-through invariant against a completed wrapper run:
/// a single full real `docker build` was issued, no `pull` (no splice /
/// no cache-hit materialization), the outcome is a real-build one, and
/// `docker_ran` is set.
fn assert_fell_through_to_a_real_full_build(receipt: &WrapperReceipt, runner: &MockCommandRunner) {
    let recorded = runner.recorded();
    assert_eq!(
        recorded.len(),
        1,
        "exactly one docker invocation — a single full build, never a splice or a second build; got {recorded:?}",
    );
    assert_eq!(recorded[0].program, "docker");
    assert_eq!(
        recorded[0].args.first().map(String::as_str),
        Some("build"),
        "the one invocation must be a `docker build`, never a `pull` (a pull would mean a cache-hit materialization / splice); got {:?}",
        recorded[0].args,
    );
    assert!(receipt.docker_ran, "docker_ran must be true after a fall-through");
    match &receipt.outcome {
        WrapperOutcome::CacheMiss { .. } | WrapperOutcome::BuildFailed { .. } => {}
        WrapperOutcome::CacheHit { .. } => {
            panic!("a cache-side failure must NEVER produce a CacheHit — that would be a broken splice");
        }
    }
}

// ── An erroring StorageBackend (the "backend unreachable" seam) ──────

/// A [`StorageBackend`] whose every operation fails with a
/// [`CacheError`] — models a transient Redis/Postgres outage. The
/// wrapper must degrade to a plain build rather than propagate this.
struct ErroringCacheBackend {
    kind: ErrorKind,
    /// Unreachable — every verb above it errors first — but the trait requires
    /// a decision, and an empty index is the honest one for a backend that
    /// stores nothing.
    nar_refs: sui_cache::MemNarRefIndex,
}

#[derive(Debug, Clone, Copy)]
enum ErrorKind {
    Io,
    NarInfo,
    Signing,
    NotImplemented,
}

impl ErroringCacheBackend {
    fn err(&self) -> CacheError {
        match self.kind {
            ErrorKind::Io => {
                CacheError::Io(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "redis down"))
            }
            ErrorKind::NarInfo => CacheError::NarInfo("corrupt narinfo".to_string()),
            ErrorKind::Signing => CacheError::Signing("bad key".to_string()),
            ErrorKind::NotImplemented => CacheError::NotImplemented("backend offline"),
        }
    }
}

#[async_trait]
impl StorageBackend for ErroringCacheBackend {
    async fn get_narinfo(&self, _hash: &str) -> Result<Option<String>, CacheError> {
        Err(self.err())
    }
    async fn put_narinfo_record(&self, _hash: &str, _content: &str) -> Result<(), CacheError> {
        Err(self.err())
    }
    async fn delete_narinfo_record(&self, _hash: &str) -> Result<(), CacheError> {
        Err(self.err())
    }
    async fn delete_nar_record(&self, _nar_path: &str) -> Result<(), CacheError> {
        Err(self.err())
    }
    fn nar_ref_index(&self) -> &dyn sui_cache::NarRefIndex {
        &self.nar_refs
    }
    async fn get_nar(&self, _path: &str) -> Result<Option<Vec<u8>>, CacheError> {
        Err(self.err())
    }
    async fn put_nar(&self, _path: &str, _data: &[u8]) -> Result<(), CacheError> {
        Err(self.err())
    }
    async fn list_narinfos(&self) -> Result<Vec<String>, CacheError> {
        Err(self.err())
    }
    /// An in-memory test double holds whole values by construction. The
    /// declaration is required precisely so a *production* backend cannot
    /// inherit this path by omission.
    fn nar_residency(&self) -> sui_cache::NarResidency {
        sui_cache::NarResidency::WholeValue
    }
}

// ── Fixtures + helpers ───────────────────────────────────────────────

const DOCKERFILE_PATH: &str = "Dockerfile";

fn config() -> WrapperConfig {
    WrapperConfig {
        dockerfile_path: PathBuf::from(DOCKERFILE_PATH),
        context_dir: PathBuf::from("."),
        build_args: BTreeMap::new(),
        image_tag: "example/image:test".to_string(),
        daemon_socket_path: None,
    }
}

fn env_for(text: &str) -> MockDockerfileEnvironment {
    MockDockerfileEnvironment::default().with_dockerfile(DOCKERFILE_PATH, text)
}

// A generator for well-formed (parseable) random Dockerfiles — used for
// the cache-miss + partial-store modes where the graph must actually
// compute so we can seed the cache.
fn arb_wellformed_dockerfile() -> impl Strategy<Value = String> {
    let instr = prop_oneof![
        "[a-z0-9]{1,10}".prop_map(|t| {
            let mut s = String::from("RUN echo ");
            s.push_str(&t);
            s
        }),
        "[a-z0-9]{1,10}".prop_map(|t| {
            let mut s = String::from("WORKDIR /");
            s.push_str(&t);
            s
        }),
        "[a-z0-9]{1,10}".prop_map(|t| {
            let mut s = String::from("CMD [\"");
            s.push_str(&t);
            s.push_str("\"]");
            s
        }),
    ];
    prop::collection::vec(instr, 1..7).prop_map(|body| {
        let mut text = String::from("FROM debian:bookworm-slim\n");
        for line in body {
            text.push_str(&line);
            text.push('\n');
        }
        text
    })
}

// A generator for Dockerfiles the *scoped* graph hasher rejects but a
// real `docker build` would accept (or at least attempt) — the
// "malformed spec (to us)" mode. Each is a real reason `apply` returns
// `Err(SpecError)`.
fn arb_hasher_rejected_dockerfile() -> impl Strategy<Value = String> {
    prop_oneof![
        // HEALTHCHECK — a perfectly valid docker instruction the scoped
        // parser does not handle.
        Just("FROM debian:bookworm-slim\nHEALTHCHECK CMD curl -f http://localhost/ || exit 1\n".to_string()),
        // An unresolved $ARG reference — the hasher errors; docker would
        // expand it (to empty) and build.
        Just("FROM debian:bookworm-slim\nRUN echo $NEVER_DECLARED\n".to_string()),
        // Empty VOLUME — hasher rejects; not our concern to validate.
        Just("FROM debian:bookworm-slim\nVOLUME\n".to_string()),
        // Another unsupported instruction.
        Just("FROM debian:bookworm-slim\nSHELL [\"/bin/bash\", \"-c\"]\n".to_string()),
    ]
}

fn graph_hashes(text: &str) -> Vec<String> {
    let env = env_for(text);
    dockerfile::apply(&DockerfileArgs { path: DOCKERFILE_PATH.to_string() }, &env)
        .expect("well-formed fixture must parse")
        .nodes
        .into_iter()
        .map(|n| n.content_hash)
        .collect()
}

// ═══════════════════════════════════════════════════════════════════
//  Failure mode 1 — CACHE MISS (empty backend)
// ═══════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn cache_miss_falls_through_to_a_real_full_build(text in arb_wellformed_dockerfile()) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let env = env_for(&text);
            let cache: Arc<dyn StorageBackend> = Arc::new(MockCacheBackend::new());
            let runner = MockCommandRunner::new();

            let receipt = run_wrapper(&config(), &env, &cache, &runner).await.unwrap();

            assert_fell_through_to_a_real_full_build(&receipt, &runner);
            // A genuine miss (not a degrade) — the cache was consulted
            // successfully, so no fell_through_reason is recorded.
            prop_assert!(receipt.fell_through_reason.is_none(),
                "a plain cache miss is not a degrade — the cache was consulted fine");
            prop_assert!(matches!(receipt.outcome, WrapperOutcome::CacheMiss { .. }), "expected a CacheMiss outcome after a real build");
            Ok(())
        })?;
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Failure mode 2 — BACKEND UNREACHABLE (every op errors)
// ═══════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn backend_error_falls_through_to_a_real_full_build(
        text in arb_wellformed_dockerfile(),
        kind in prop_oneof![
            Just(ErrorKind::Io),
            Just(ErrorKind::NarInfo),
            Just(ErrorKind::Signing),
            Just(ErrorKind::NotImplemented),
        ],
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let env = env_for(&text);
            let cache: Arc<dyn StorageBackend> = Arc::new(ErroringCacheBackend {
                kind,
                nar_refs: sui_cache::MemNarRefIndex::new(),
            });
            let runner = MockCommandRunner::new();

            // A backend outage must NEVER be a `WrapperError` — the whole
            // D6 point. It degrades to a real build.
            let receipt = run_wrapper(&config(), &env, &cache, &runner).await
                .expect("a cache backend error must NOT propagate as a WrapperError — it degrades");

            assert_fell_through_to_a_real_full_build(&receipt, &runner);
            prop_assert!(receipt.fell_through_reason.is_some(),
                "a backend outage is a degrade — it must record a fell_through_reason");
            prop_assert!(
                receipt.fell_through_reason.as_deref().unwrap().contains("cache backend error"),
                "the degrade reason must name the backend error, got {:?}",
                receipt.fell_through_reason,
            );
            Ok(())
        })?;
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Failure mode 3 — MALFORMED SPEC (hasher stricter than docker)
// ═══════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn hasher_rejected_dockerfile_falls_through_to_a_real_full_build(
        text in arb_hasher_rejected_dockerfile(),
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Sanity: this fixture genuinely makes the hasher error, so
            // we are truly exercising the degrade path (not a happy graph).
            let env = env_for(&text);
            prop_assert!(
                dockerfile::apply(&DockerfileArgs { path: DOCKERFILE_PATH.to_string() }, &env).is_err(),
                "fixture must be one the scoped hasher rejects",
            );

            let cache: Arc<dyn StorageBackend> = Arc::new(MockCacheBackend::new());
            let runner = MockCommandRunner::new();

            let receipt = run_wrapper(&config(), &env, &cache, &runner).await
                .expect("a Dockerfile our parser rejects but docker builds must NOT error — it degrades");

            assert_fell_through_to_a_real_full_build(&receipt, &runner);
            prop_assert!(receipt.fell_through_reason.is_some(),
                "a hasher rejection is a degrade — record why");
            prop_assert!(
                receipt.fell_through_reason.as_deref().unwrap().contains("graph hasher rejected"),
                "the degrade reason must name the hasher rejection, got {:?}",
                receipt.fell_through_reason,
            );
            // No graph ⇒ no per-node status was computed.
            prop_assert!(receipt.nodes.is_empty(),
                "a rejected Dockerfile has no computed node graph");
            Ok(())
        })?;
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Failure mode 4 — PARTIAL STORE (some nodes cached, not all)
// ═══════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn partial_store_falls_through_without_splicing(
        text in arb_wellformed_dockerfile(),
        idx in any::<prop::sample::Index>(),
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let hashes = graph_hashes(&text);
            prop_assume!(hashes.len() >= 2); // need a strict-subset seed

            // Seed exactly ONE node (a strict partial hit).
            let seed_at = idx.index(hashes.len());
            let mut mock = MockCacheBackend::new();
            mock = mock.with_entry(&hashes[seed_at], "example/image:partial");
            let cache: Arc<dyn StorageBackend> = Arc::new(mock);
            let runner = MockCommandRunner::new();

            let env = env_for(&text);
            let receipt = run_wrapper(&config(), &env, &cache, &runner).await.unwrap();

            // The whole D6 non-goal: a partial hit is NEVER spliced — it
            // is one full real build.
            assert_fell_through_to_a_real_full_build(&receipt, &runner);
            prop_assert!(matches!(receipt.outcome, WrapperOutcome::CacheMiss { .. }), "expected a CacheMiss outcome after a real build");
            prop_assert!(receipt.fell_through_reason.is_none(),
                "a partial hit is a normal miss (cache consulted fine), not a degrade");
            Ok(())
        })?;
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Failure mode 5 — DAEMON ABSENT (node-cache daemon socket missing)
// ═══════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn absent_node_cache_daemon_falls_through_via_remote(text in arb_wellformed_dockerfile()) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // A DaemonAwareCacheClient pointed at a socket path that does
            // not exist, over an EMPTY remote backend. With no daemon and
            // an empty remote, every node misses → a real full build.
            let missing_socket = PathBuf::from("/nonexistent/sui-node-cache-daemon.sock");
            let remote: Arc<dyn StorageBackend> = Arc::new(MockCacheBackend::new());
            let client = DaemonAwareCacheClient::new(Some(missing_socket), remote);
            let cache: Arc<dyn StorageBackend> = Arc::new(client);
            let runner = MockCommandRunner::new();

            let env = env_for(&text);
            let receipt = run_wrapper(&config(), &env, &cache, &runner).await.unwrap();

            assert_fell_through_to_a_real_full_build(&receipt, &runner);
            prop_assert!(matches!(receipt.outcome, WrapperOutcome::CacheMiss { .. }), "expected a CacheMiss outcome after a real build");
            Ok(())
        })?;
    }
}

// ── A tiny table test cross-checking the assertion helper's own
//    negative case: a genuine FULL hit must NOT satisfy the
//    fall-through invariant (guards against the helper being vacuously
//    true). Not a failure mode — a guard on the guard. ────────────────

#[tokio::test]
async fn control_full_hit_does_not_fall_through() {
    let text = "FROM debian:bookworm-slim\nRUN echo one\nCMD [\"x\"]\n";
    let hashes = graph_hashes(text);
    let mut mock = MockCacheBackend::new();
    for h in &hashes {
        mock = mock.with_entry(h, "example/image:cached");
    }
    let cache: Arc<dyn StorageBackend> = Arc::new(mock);
    let runner = MockCommandRunner::new();
    let env = env_for(text);

    let receipt = run_wrapper(&config(), &env, &cache, &runner).await.unwrap();

    // This IS a full hit — it must materialize via `pull`, never build.
    assert!(!receipt.docker_ran, "a full hit must not run docker build");
    assert!(matches!(receipt.outcome, WrapperOutcome::CacheHit { .. }));
    let recorded = runner.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].args.first().map(String::as_str), Some("pull"));
}
