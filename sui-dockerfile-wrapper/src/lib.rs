//! Phase 2 of `supa-charge-akeyless-ci`: the intercept/resolve/fall-through
//! wrapper around plain `docker build`.
//!
//! Given a Dockerfile path + build context + build-arg map, this crate:
//!
//! 1. computes the [`sui_spec::dockerfile`] content-addressed
//!    [`DockerfileGraph`](sui_spec::dockerfile::DockerfileGraph);
//! 2. checks every node's `content_hash` against a
//!    [`sui_cache::storage::StorageBackend`] (the same trait `sui cache
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
//! `docker` subprocess) and [`sui_cache::storage::StorageBackend`] (the
//! cache). Both are mocked in this crate's tests; production wires the
//! real [`command::RealCommandRunner`] and a real backend built via
//! [`sui_cache::storage::build_backend`].

pub mod cache;
pub mod command;
pub mod daemon_client;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sui_cache::storage::StorageBackend;
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
    CacheMiss { docker_build_duration_ms: u128, nodes_cached: usize },
    /// `docker build` (or the cache-hit `docker pull`) exited non-zero.
    BuildFailed { exit_code: Option<i32>, stderr_tail: String },
}

/// The typed, `serde`-serializable "JSON receipt out" half of the
/// keyway contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WrapperReceipt {
    pub outcome: WrapperOutcome,
    pub nodes: Vec<NodeCacheStatus>,
    pub total_wall_clock_ms: u128,
    pub docker_ran: bool,
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

/// Errors reading/parsing the Dockerfile graph — surfaced as a typed
/// error, never a panic.
#[derive(Debug, thiserror::Error)]
pub enum WrapperError {
    #[error("computing the dockerfile graph: {0}")]
    Graph(#[from] sui_spec::SpecError),
    #[error("cache backend error: {0}")]
    Cache(#[from] sui_cache::CacheError),
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

/// Run the wrapper: compute the graph, check the cache, and either
/// materialize a hit or fall through to a real build.
///
/// # Errors
///
/// Returns [`WrapperError`] if the Dockerfile can't be parsed into a
/// graph, or if the cache backend itself errors (a cache *miss* is not
/// an error — only a backend I/O failure is). A failing `docker build`
/// subprocess is *not* a `WrapperError` — it is the typed
/// [`WrapperOutcome::BuildFailed`] inside an `Ok` receipt.
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

    let graph: DockerfileGraph = dockerfile::apply(
        &DockerfileArgs { path: config.dockerfile_path.display().to_string() },
        env,
    )?;

    let mut nodes = Vec::with_capacity(graph.nodes.len());
    let mut all_cached = !graph.nodes.is_empty();
    let mut cached_image_ref: Option<String> = None;
    for node in &graph.nodes {
        let hit = cache.get_narinfo(&node.content_hash).await?;
        if let Some(image_ref) = &hit {
            cached_image_ref = Some(image_ref.clone());
        } else {
            all_cached = false;
        }
        nodes.push(NodeCacheStatus { content_hash: node.content_hash.clone(), cached: hit.is_some() });
    }

    if all_cached {
        // Full hit: materialize the already-built image rather than
        // rebuilding. The image reference is whatever the last node's
        // cache entry recorded on the miss path that produced it.
        let image_ref = cached_image_ref.unwrap_or_else(|| config.image_tag.clone());
        let invocation = DockerBuildInvocation::pull(&image_ref);
        let outcome = runner.run(&invocation)?;
        let total_wall_clock_ms = start.elapsed().as_millis();
        if outcome.success {
            return Ok(WrapperReceipt {
                outcome: WrapperOutcome::CacheHit { image_ref, node_count: nodes.len() },
                nodes,
                total_wall_clock_ms,
                docker_ran: false,
            });
        }
        return Ok(WrapperReceipt {
            outcome: WrapperOutcome::BuildFailed {
                exit_code: outcome.exit_code,
                stderr_tail: outcome.stderr_tail(4096),
            },
            nodes,
            total_wall_clock_ms,
            docker_ran: false,
        });
    }

    // Partial or full miss: never splice, always a full real build.
    let build_started = Instant::now();
    let invocation = DockerBuildInvocation::build(
        &config.dockerfile_path,
        &config.context_dir,
        &config.image_tag,
        &config.build_args,
    );
    let outcome = runner.run(&invocation)?;
    let docker_build_duration_ms = build_started.elapsed().as_millis();
    let total_wall_clock_ms = start.elapsed().as_millis();

    if !outcome.success {
        return Ok(WrapperReceipt {
            outcome: WrapperOutcome::BuildFailed {
                exit_code: outcome.exit_code,
                stderr_tail: outcome.stderr_tail(4096),
            },
            nodes,
            total_wall_clock_ms,
            docker_ran: true,
        });
    }

    // Back-fill the cache: every node's hash now maps to the freshly
    // built image tag, so a future run over the same graph hits.
    let mut nodes_cached = 0usize;
    for node in &graph.nodes {
        cache.put_narinfo(&node.content_hash, &config.image_tag).await?;
        nodes_cached += 1;
    }
    for status in &mut nodes {
        status.cached = true;
    }

    Ok(WrapperReceipt {
        outcome: WrapperOutcome::CacheMiss { docker_build_duration_ms, nodes_cached },
        nodes,
        total_wall_clock_ms,
        docker_ran: true,
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
        };
        let json = receipt.to_json().unwrap();
        let parsed: WrapperReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, receipt);
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
