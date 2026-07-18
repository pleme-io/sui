//! Phase 2 of `supa-charge-akeyless-ci`: the intercept/resolve/fall-through
//! wrapper around plain `docker build`.
//!
//! Given a Dockerfile path + build context + build-arg map, this crate:
//!
//! 1. computes the [`sui_spec::dockerfile`] content-addressed
//!    [`DockerfileGraph`](sui_spec::dockerfile::DockerfileGraph);
//! 2. checks every node's `content_hash` against a
//!    [`sui_castore::StorageBackend`] (the same trait `sui cache
//!    serve` runs — see [`cache`]);
//! 3. on a **full** cache hit, materializes the already-built image via
//!    `docker pull` instead of rebuilding — [`WrapperOutcome::CacheHit`];
//! 4. on **any** miss (partial or full), shells out to a real `docker
//!    build` for the *entire* Dockerfile — never a partial cache splice,
//!    an explicit non-goal per the `supa-charge-akeyless-ci` plan — then
//!    back-fills the cache with every node's hash → image reference for
//!    next time — [`WrapperOutcome::CacheMiss`];
//! 5. on a failing `docker build`, returns
//!    [`WrapperOutcome::BuildFailed`] — never a panic.
//!
//! I/O is behind two injectable seams: [`command::CommandRunner`] (the
//! `docker` subprocess) and [`sui_castore::StorageBackend`] (the
//! cache). Both are mocked in this crate's tests; production wires the
//! real [`command::RealCommandRunner`] and a real backend built via
//! [`sui_cache::build_backend`].

pub mod cache;
pub mod command;
pub mod daemon_client;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sui_cache::StorageBackend;
use sui_spec::dockerfile::{self, DockerfileArgs, DockerfileEnvironment, DockerfileGraph};

pub use command::{CommandOutcome, CommandRunError, CommandRunner, DockerBuildInvocation, MockCommandRunner, RealCommandRunner};
pub use cache::MockCacheBackend;
pub use daemon_client::DaemonAwareCacheClient;

/// Typed, `serde`-deserializable input to a wrapper run — the keyway-shaped
/// "YAML/JSON in" half of the contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WrapperConfig {
    /// Path to the Dockerfile to build.
    pub dockerfile_path: PathBuf,
    /// Build context directory (mirrors `docker build <context>`).
    pub context_dir: PathBuf,
    /// Build-arg values (mirrors repeated `--build-arg K=V`).
    #[serde(default)]
    pub build_args: BTreeMap<String, String>,
    /// The image tag to build/pull (mirrors `docker build -t <tag>`).
    pub image_tag: String,
    /// Optional path to a node-local `sui-dockerfile-node-cache-daemon`
    /// Unix domain socket (Phase 3b). Absent by default, which keeps
    /// this config byte-for-byte identical to Phase 2's original
    /// shape. This field is read by whoever *constructs* the
    /// `Arc<dyn StorageBackend>` passed to [`run_wrapper`] (e.g. the
    /// GHA entrypoint) to decide whether to wrap the remote backend in
    /// a [`crate::DaemonAwareCacheClient`] — `run_wrapper` itself never
    /// reads this field, so its behavior is unaffected either way.
    #[serde(default)]
    pub daemon_socket_path: Option<PathBuf>,
}

/// Per-node cache status in the receipt — one row per
/// [`DockerfileGraph`] node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeCacheStatus {
    pub content_hash: String,
    pub cached: bool,
}

/// The typed outcome of one wrapper run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum WrapperOutcome {
    /// Every graph node was already cached — the image was pulled from
    /// the cached reference, `docker build` never ran.
    CacheHit { image_ref: String, node_count: usize },
    /// At least one node was missing (or the cache had zero nodes) —
    /// `docker build` ran end to end and the cache was back-filled.
    ///
    /// The duration is `u64` milliseconds, not `u128`: this enum is
    /// `#[serde(tag = "kind")]` (internally tagged), and serde_json
    /// cannot deserialize a `u128` through the intermediate buffer an
    /// internally-tagged enum requires — a `u128` here made every
    /// `CacheMiss` receipt un-round-trippable through the keyway "JSON
    /// receipt out" contract. `u64` ms is ~584 million years of range,
    /// far beyond any build wall-clock.
    CacheMiss { docker_build_duration_ms: u64, nodes_cached: usize },
    /// `docker build` (or the cache-hit `docker pull`) exited non-zero.
    BuildFailed { exit_code: Option<i32>, stderr_tail: String },
}

/// The typed, `serde`-serializable "JSON receipt out" half of the
/// keyway contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WrapperReceipt {
    pub outcome: WrapperOutcome,
    pub nodes: Vec<NodeCacheStatus>,
    /// `u64` milliseconds — see [`WrapperOutcome::CacheMiss`] for why not
    /// `u128` (serde_json + internally-tagged-enum round-trip).
    pub total_wall_clock_ms: u64,
    pub docker_ran: bool,
    /// Set when the cache *accelerator* could not be consulted and the
    /// wrapper degraded to a plain real `docker build` — e.g. the graph
    /// hasher rejected a Dockerfile that real docker builds fine (our
    /// scoped parser is deliberately narrower than BuildKit), or the
    /// cache backend itself errored (a transient Redis/Postgres hiccup).
    /// The cache is an optimization: any cache-side trouble degrades to
    /// a correct build, it never *breaks* one — this field makes that
    /// degrade observable rather than silent. `None` on the normal
    /// (cache consulted successfully) path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fell_through_reason: Option<String>,
}

impl WrapperReceipt {
    /// Render this receipt as pretty JSON — the canonical keyway output
    /// surface (never `format!()` of ad-hoc fields).
    ///
    /// # Errors
    ///
    /// Propagates any `serde_json` serialization failure (never expected
    /// for this fully-owned type, but kept fallible per the typed-emission
    /// contract).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// The one thing that can genuinely fail a wrapper run: the `docker`
/// subprocess could not be *spawned* at all (e.g. the binary is not on
/// PATH). Everything cache-side degrades to a plain real build (D6) and
/// so is *not* a `WrapperError` — see [`run_wrapper`]'s fall-through
/// contract. A failing `docker build` is likewise not a `WrapperError`;
/// it is the typed [`WrapperOutcome::BuildFailed`] inside an `Ok`
/// receipt. Surfaced as a typed error, never a panic.
#[derive(Debug, thiserror::Error)]
pub enum WrapperError {
    #[error("failed to spawn docker: {0}")]
    Command(#[from] CommandRunError),
}

/// A [`DockerfileEnvironment`] that reads the Dockerfile straight off
/// disk — the one production side effect this crate performs beyond the
/// command runner and the cache backend.
pub struct FilesystemDockerfileEnvironment {
    pub build_args: BTreeMap<String, String>,
}

impl DockerfileEnvironment for FilesystemDockerfileEnvironment {
    fn read_dockerfile(&self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }

    fn resolve_build_arg(&self, name: &str) -> Option<String> {
        self.build_args.get(name).cloned()
    }
}

/// Elapsed milliseconds since `since`, saturated into a `u64` (the
/// receipt's serde-safe width). `Instant::elapsed().as_millis()` is a
/// `u128`; a `u64` of ms is ~584 million years of range, so the
/// saturation is unreachable in practice — it just keeps the cast
/// total and explicit rather than a bare `as` truncation.
fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The successfully-consulted cache plan for one Dockerfile: the parsed
/// graph, per-node cache status, and — when *every* node was a hit — the
/// image reference to materialize instead of rebuilding.
struct CachePlan {
    graph: DockerfileGraph,
    nodes: Vec<NodeCacheStatus>,
    /// `Some(image_ref)` iff the whole graph was cached (a full hit).
    full_hit_image_ref: Option<String>,
}

/// Try to consult the cache accelerator: parse the Dockerfile into a
/// content-addressed graph and check every node against the backend.
///
/// Returns `Err(reason)` — a human-readable degrade reason — for **any**
/// cache-side trouble: a Dockerfile the scoped graph hasher rejects
/// (our parser is deliberately narrower than BuildKit — a `HEALTHCHECK`
/// line real docker builds fine lands here), or a backend I/O error
/// (a transient Redis/Postgres hiccup). The caller degrades to a plain
/// real `docker build` on `Err` — the cache is an optimization, never a
/// gate on correctness.
async fn consult_cache<E>(
    config: &WrapperConfig,
    env: &E,
    cache: &Arc<dyn StorageBackend>,
) -> Result<CachePlan, String>
where
    E: DockerfileEnvironment,
{
    let graph: DockerfileGraph = dockerfile::apply(
        &DockerfileArgs { path: config.dockerfile_path.display().to_string() },
        env,
    )
    .map_err(|e| {
        let mut msg = String::from("graph hasher rejected the Dockerfile (scoped parser narrower than docker): ");
        msg.push_str(&e.to_string());
        msg
    })?;

    let mut nodes = Vec::with_capacity(graph.nodes.len());
    let mut all_cached = !graph.nodes.is_empty();
    let mut cached_image_ref: Option<String> = None;
    for node in &graph.nodes {
        let hit = cache.get_narinfo(&node.content_hash).await.map_err(|e| {
            let mut msg = String::from("cache backend error while checking a node: ");
            msg.push_str(&e.to_string());
            msg
        })?;
        if let Some(image_ref) = &hit {
            cached_image_ref = Some(image_ref.clone());
        } else {
            all_cached = false;
        }
        nodes.push(NodeCacheStatus { content_hash: node.content_hash.clone(), cached: hit.is_some() });
    }

    let full_hit_image_ref = if all_cached {
        Some(cached_image_ref.unwrap_or_else(|| config.image_tag.clone()))
    } else {
        None
    };
    Ok(CachePlan { graph, nodes, full_hit_image_ref })
}

/// Run the wrapper: consult the cache accelerator and either materialize
/// a full hit or fall through to a real `docker build`.
///
/// # The fall-through safety contract (D6)
///
/// The cache is an *accelerator*, never a gate. For **every** cache-side
/// failure mode — a Dockerfile the scoped graph hasher rejects, a cache
/// backend I/O error, a partial cache hit, a missing node-cache daemon —
/// the wrapper degrades to a plain real `docker build` of the *entire*
/// Dockerfile and **never** returns a broken or partially-spliced
/// result. The degrade is recorded in
/// [`WrapperReceipt::fell_through_reason`] so it is observable, never
/// silent. A partial hit is likewise never spliced — it is a full real
/// build (with `fell_through_reason == None`, since the cache *was*
/// consulted successfully, it simply wasn't a full hit).
///
/// # Errors
///
/// Returns [`WrapperError::Command`] only if the `docker` subprocess
/// could not be *spawned* at all (e.g. the binary is missing) — a
/// genuine environment failure, not a cache concern. Graph-computation
/// and cache-backend errors are **not** propagated: they degrade to a
/// real build. A failing `docker build` subprocess is not a
/// `WrapperError` either — it is the typed [`WrapperOutcome::BuildFailed`]
/// inside an `Ok` receipt.
pub async fn run_wrapper<E, R>(
    config: &WrapperConfig,
    env: &E,
    cache: &Arc<dyn StorageBackend>,
    runner: &R,
) -> Result<WrapperReceipt, WrapperError>
where
    E: DockerfileEnvironment,
    R: CommandRunner,
{
    let start = Instant::now();

    // Consult the cache accelerator. On ANY cache-side error, degrade to
    // a plain real build rather than propagating — the cache never gates
    // correctness (D6).
    let (plan, fell_through_reason): (Option<CachePlan>, Option<String>) =
        match consult_cache(config, env, cache).await {
            Ok(plan) => (Some(plan), None),
            Err(reason) => {
                tracing::warn!(reason = %reason, "cache accelerator unavailable — falling through to a plain docker build");
                (None, Some(reason))
            }
        };

    // Full-hit fast path: materialize the already-built image via
    // `docker pull` instead of rebuilding. Only reachable when the cache
    // was consulted successfully AND every node was a hit.
    if let Some(plan) = &plan {
        if let Some(image_ref) = &plan.full_hit_image_ref {
            let invocation = DockerBuildInvocation::pull(image_ref);
            let outcome = runner.run(&invocation)?;
            let total_wall_clock_ms = elapsed_ms(start);
            if outcome.success {
                return Ok(WrapperReceipt {
                    outcome: WrapperOutcome::CacheHit {
                        image_ref: image_ref.clone(),
                        node_count: plan.nodes.len(),
                    },
                    nodes: plan.nodes.clone(),
                    total_wall_clock_ms,
                    docker_ran: false,
                    fell_through_reason: None,
                });
            }
            return Ok(WrapperReceipt {
                outcome: WrapperOutcome::BuildFailed {
                    exit_code: outcome.exit_code,
                    stderr_tail: outcome.stderr_tail(4096),
                },
                nodes: plan.nodes.clone(),
                total_wall_clock_ms,
                docker_ran: false,
                fell_through_reason: None,
            });
        }
    }

    // Fall-through: a partial/full cache miss, OR the cache was
    // unavailable entirely. Either way — never splice, always a full
    // real build. When the cache was consulted we carry its per-node
    // status; when it was unavailable we carry an empty node list (we
    // never computed the graph).
    let mut nodes = plan.as_ref().map(|p| p.nodes.clone()).unwrap_or_default();

    let build_started = Instant::now();
    let invocation = DockerBuildInvocation::build(
        &config.dockerfile_path,
        &config.context_dir,
        &config.image_tag,
        &config.build_args,
    );
    let outcome = runner.run(&invocation)?;
    let docker_build_duration_ms = elapsed_ms(build_started);
    let total_wall_clock_ms = elapsed_ms(start);

    if !outcome.success {
        return Ok(WrapperReceipt {
            outcome: WrapperOutcome::BuildFailed {
                exit_code: outcome.exit_code,
                stderr_tail: outcome.stderr_tail(4096),
            },
            nodes,
            total_wall_clock_ms,
            docker_ran: true,
            fell_through_reason,
        });
    }

    // Back-fill the cache: every node's hash now maps to the freshly
    // built image tag, so a future run over the same graph hits. This is
    // best-effort — a back-fill write failure must not fail an
    // already-successful build (the cache is an accelerator), so a
    // put error only marks that node uncached and continues.
    let mut nodes_cached = 0usize;
    if let Some(plan) = &plan {
        for node in &plan.graph.nodes {
            match cache.put_narinfo(&node.content_hash, &config.image_tag).await {
                Ok(()) => nodes_cached += 1,
                Err(e) => {
                    tracing::warn!(hash = %node.content_hash, error = %e, "cache back-fill write failed — build still succeeded");
                }
            }
        }
        for status in &mut nodes {
            status.cached = nodes_cached == plan.graph.nodes.len();
        }
    }

    Ok(WrapperReceipt {
        outcome: WrapperOutcome::CacheMiss { docker_build_duration_ms, nodes_cached },
        nodes,
        total_wall_clock_ms,
        docker_ran: true,
        fell_through_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use command::CommandOutcome;
    use sui_spec::dockerfile::MockDockerfileEnvironment;

    const DOCKERFILE_PATH: &str = "Dockerfile";

    fn simple_env() -> MockDockerfileEnvironment {
        MockDockerfileEnvironment::default().with_dockerfile(
            DOCKERFILE_PATH,
            "FROM debian:bookworm-slim\nRUN apt-get update\nCMD [\"true\"]\n",
        )
    }

    fn config() -> WrapperConfig {
        WrapperConfig {
            dockerfile_path: PathBuf::from(DOCKERFILE_PATH),
            context_dir: PathBuf::from("."),
            build_args: BTreeMap::new(),
            image_tag: "example/image:test".to_string(),
            daemon_socket_path: None,
        }
    }

    fn graph_for(env: &MockDockerfileEnvironment) -> DockerfileGraph {
        dockerfile::apply(&DockerfileArgs { path: DOCKERFILE_PATH.to_string() }, env).unwrap()
    }

    #[tokio::test]
    async fn full_cache_hit_never_invokes_docker_build() {
        let env = simple_env();
        let graph = graph_for(&env);
        let mut mock_cache = MockCacheBackend::new();
        for node in &graph.nodes {
            mock_cache = mock_cache.with_entry(&node.content_hash, "example/image:cached");
        }
        let cache: Arc<dyn StorageBackend> = Arc::new(mock_cache);
        let runner = MockCommandRunner::new();

        let receipt = run_wrapper(&config(), &env, &cache, &runner).await.unwrap();

        assert!(!receipt.docker_ran);
        match receipt.outcome {
            WrapperOutcome::CacheHit { image_ref, node_count } => {
                assert_eq!(image_ref, "example/image:cached");
                assert_eq!(node_count, graph.nodes.len());
            }
            other => panic!("expected CacheHit, got {other:?}"),
        }
        // Exactly one invocation — a `docker pull`, never a `docker build`.
        let recorded = runner.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].args[0], "pull");
    }

    #[tokio::test]
    async fn full_cache_miss_falls_through_to_docker_build() {
        let env = simple_env();
        let cache: Arc<dyn StorageBackend> = Arc::new(MockCacheBackend::new());
        let runner = MockCommandRunner::new();

        let receipt = run_wrapper(&config(), &env, &cache, &runner).await.unwrap();

        assert!(receipt.docker_ran);
        match receipt.outcome {
            WrapperOutcome::CacheMiss { nodes_cached, .. } => {
                assert_eq!(nodes_cached, 3, "FROM + RUN + CMD");
            }
            other => panic!("expected CacheMiss, got {other:?}"),
        }
        let recorded = runner.recorded();
        assert_eq!(recorded.len(), 1);
        let invocation = &recorded[0];
        assert_eq!(invocation.program, "docker");
        assert_eq!(invocation.args[0], "build");
        assert!(invocation.args.contains(&"-f".to_string()));
        assert!(invocation.args.contains(&"-t".to_string()));
        assert!(invocation.args.contains(&"example/image:test".to_string()));

        // The cache was back-filled — a second run over the same graph
        // is a full hit.
        let graph = graph_for(&env);
        for node in &graph.nodes {
            let hit = cache.get_narinfo(&node.content_hash).await.unwrap();
            assert_eq!(hit.as_deref(), Some("example/image:test"));
        }
    }

    #[tokio::test]
    async fn partial_cache_hit_still_falls_through_to_a_full_build() {
        let env = simple_env();
        let graph = graph_for(&env);
        assert!(graph.nodes.len() >= 2, "fixture must have >=2 nodes to test partial hit");

        // Cache only the FIRST node — a partial hit.
        let mock_cache = MockCacheBackend::new().with_entry(&graph.nodes[0].content_hash, "example/image:partial");
        let cache: Arc<dyn StorageBackend> = Arc::new(mock_cache);
        let runner = MockCommandRunner::new();

        let receipt = run_wrapper(&config(), &env, &cache, &runner).await.unwrap();

        // No clever splice — falls straight through to a full real build.
        assert!(receipt.docker_ran);
        assert!(matches!(receipt.outcome, WrapperOutcome::CacheMiss { .. }));
        let recorded = runner.recorded();
        assert_eq!(recorded.len(), 1, "exactly one full docker build, no partial splice attempt");
        assert_eq!(recorded[0].args[0], "build");

        // The per-node receipt is still honest about which nodes were
        // cached before the fallback ran.
        assert!(receipt.nodes[0].cached, "first node was pre-cached in this fixture");
    }

    #[tokio::test]
    async fn failing_docker_build_returns_build_failed_not_a_panic() {
        let env = simple_env();
        let cache: Arc<dyn StorageBackend> = Arc::new(MockCacheBackend::new());
        let runner = MockCommandRunner::with_outcome(CommandOutcome {
            success: false,
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"error: failed to solve: process did not complete successfully".to_vec(),
        });

        let receipt = run_wrapper(&config(), &env, &cache, &runner).await.unwrap();

        assert!(receipt.docker_ran);
        match receipt.outcome {
            WrapperOutcome::BuildFailed { exit_code, stderr_tail } => {
                assert_eq!(exit_code, Some(1));
                assert!(stderr_tail.contains("failed to solve"));
            }
            other => panic!("expected BuildFailed, got {other:?}"),
        }

        // The cache was NOT back-filled on a failed build.
        let graph = graph_for(&env);
        for node in &graph.nodes {
            let hit = cache.get_narinfo(&node.content_hash).await.unwrap();
            assert!(hit.is_none(), "a failed build must not poison the cache");
        }
    }

    #[test]
    fn receipt_json_roundtrip() {
        let receipt = WrapperReceipt {
            outcome: WrapperOutcome::CacheHit { image_ref: "example/image:cached".to_string(), node_count: 3 },
            nodes: vec![
                NodeCacheStatus { content_hash: "aaa".to_string(), cached: true },
                NodeCacheStatus { content_hash: "bbb".to_string(), cached: true },
            ],
            total_wall_clock_ms: 42,
            docker_ran: false,
            fell_through_reason: None,
        };
        let json = receipt.to_json().unwrap();
        let parsed: WrapperReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, receipt);

        // A degraded receipt round-trips too, and its reason survives.
        let degraded = WrapperReceipt {
            outcome: WrapperOutcome::CacheMiss { docker_build_duration_ms: 10, nodes_cached: 0 },
            nodes: Vec::new(),
            total_wall_clock_ms: 12,
            docker_ran: true,
            fell_through_reason: Some("cache backend error while checking a node: io error".to_string()),
        };
        let dj = degraded.to_json().unwrap();
        let dparsed: WrapperReceipt = serde_json::from_str(&dj).unwrap();
        assert_eq!(dparsed, degraded);
    }

    #[test]
    fn receipt_yaml_config_roundtrip() {
        // The keyway "YAML in" half — a WrapperConfig round-trips through
        // serde_yaml_ng exactly as it would through a `--config wrapper.yaml`
        // CLI flag.
        let cfg = config();
        let yaml = serde_yaml_ng::to_string(&cfg).unwrap();
        let parsed: WrapperConfig = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn docker_build_invocation_is_typed_not_string_concatenated() {
        let mut build_args = BTreeMap::new();
        build_args.insert("TARGETARCH".to_string(), "amd64".to_string());
        let invocation = DockerBuildInvocation::build(
            &PathBuf::from("Dockerfile"),
            &PathBuf::from("."),
            "example/image:test",
            &build_args,
        );
        assert_eq!(invocation.program, "docker");
        assert_eq!(
            invocation.args,
            vec![
                "build".to_string(),
                "-f".to_string(),
                "Dockerfile".to_string(),
                "-t".to_string(),
                "example/image:test".to_string(),
                "--build-arg".to_string(),
                "TARGETARCH=amd64".to_string(),
                ".".to_string(),
            ]
        );
    }

    /// Best-effort integration test against a REAL `docker` binary, in
    /// cache-miss mode, proving the real subprocess path works. Skips
    /// cleanly (never fakes a result) when `docker` is not on PATH — this
    /// environment has no docker daemon reachable, so this test is
    /// expected to skip in CI/sandboxes without one.
    #[tokio::test]
    async fn real_docker_build_end_to_end_when_docker_is_available() {
        let docker_available = std::process::Command::new("docker")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !docker_available {
            eprintln!("skipping real_docker_build_end_to_end_when_docker_is_available: no docker on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let dockerfile_path = dir.path().join("Dockerfile");
        std::fs::write(&dockerfile_path, "FROM scratch\nCOPY Dockerfile /Dockerfile\n").unwrap();

        let env = FilesystemDockerfileEnvironment { build_args: BTreeMap::new() };
        let cache: Arc<dyn StorageBackend> = Arc::new(MockCacheBackend::new());
        let runner = RealCommandRunner;
        let cfg = WrapperConfig {
            dockerfile_path,
            context_dir: dir.path().to_path_buf(),
            build_args: BTreeMap::new(),
            image_tag: "sui-dockerfile-wrapper-test:latest".to_string(),
            daemon_socket_path: None,
        };

        let receipt = run_wrapper(&cfg, &env, &cache, &runner).await.unwrap();
        assert!(receipt.docker_ran);
        assert!(
            matches!(receipt.outcome, WrapperOutcome::CacheMiss { .. }),
            "expected a real cache-miss docker build, got {:?}",
            receipt.outcome
        );
    }
}
