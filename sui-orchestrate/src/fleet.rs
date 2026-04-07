//! Fleet deployment orchestration.
//!
//! Supports parallel, rolling, and canary deploy strategies.

use std::sync::Arc;

use crate::command::{CommandRunner, TokioCommandRunner};
use crate::node::{Node, NodeRegistry, NodeStatus};

/// Deploy strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum DeployStrategy {
    /// Deploy to all nodes simultaneously.
    Parallel,
    /// Deploy one node at a time, rolling forward.
    Rolling,
    /// Deploy to one node first, then the rest if healthy.
    Canary,
}

impl Default for DeployStrategy {
    fn default() -> Self {
        Self::Rolling
    }
}

impl std::fmt::Display for DeployStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parallel => f.write_str("parallel"),
            Self::Rolling => f.write_str("rolling"),
            Self::Canary => f.write_str("canary"),
        }
    }
}

impl std::str::FromStr for DeployStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "parallel" => Ok(Self::Parallel),
            "rolling" => Ok(Self::Rolling),
            "canary" => Ok(Self::Canary),
            other => Err(format!("invalid deploy strategy: {other}")),
        }
    }
}

/// Result of a fleet deployment.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeployResult {
    /// The target expression that was resolved (e.g. `@prod`, `alpha`).
    pub target: String,
    /// The strategy that was used (lowercase, e.g. "rolling").
    pub strategy: String,
    /// Total number of nodes targeted.
    pub total_nodes: usize,
    /// Number of nodes that deployed successfully.
    pub succeeded: usize,
    /// Number of nodes that failed to deploy.
    pub failed: usize,
    /// Per-node deployment results.
    pub results: Vec<NodeDeployResult>,
    /// Wall-clock duration of the entire deployment in seconds.
    pub duration_secs: f64,
}

/// Per-node deploy result.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NodeDeployResult {
    /// Hostname of the target node.
    pub hostname: String,
    /// Whether the deploy succeeded.
    pub success: bool,
    /// Combined stdout and stderr log.
    pub log: String,
    /// Wall-clock duration of this node's deploy in seconds.
    pub duration_secs: f64,
}

/// Trait for pluggable deploy execution strategies.
///
/// Implement this to define custom deployment patterns beyond the
/// built-in [`ParallelExecutor`], [`RollingExecutor`], and [`CanaryExecutor`].
#[async_trait::async_trait]
pub trait DeployExecutor: Send + Sync {
    /// Execute deployment across the given nodes.
    async fn execute(
        &self,
        nodes: &[Node],
        flake_override: Option<&str>,
        runner: &Arc<dyn CommandRunner>,
    ) -> Result<Vec<NodeDeployResult>, FleetError>;
}

/// Deploys to all nodes simultaneously via `tokio::spawn`.
pub struct ParallelExecutor;

#[async_trait::async_trait]
impl DeployExecutor for ParallelExecutor {
    async fn execute(
        &self,
        nodes: &[Node],
        flake_override: Option<&str>,
        runner: &Arc<dyn CommandRunner>,
    ) -> Result<Vec<NodeDeployResult>, FleetError> {
        Ok(deploy_parallel(nodes, flake_override, runner).await)
    }
}

/// Deploys one node at a time, sequentially.
pub struct RollingExecutor;

#[async_trait::async_trait]
impl DeployExecutor for RollingExecutor {
    async fn execute(
        &self,
        nodes: &[Node],
        flake_override: Option<&str>,
        runner: &Arc<dyn CommandRunner>,
    ) -> Result<Vec<NodeDeployResult>, FleetError> {
        let mut results = Vec::with_capacity(nodes.len());
        for node in nodes {
            let result = deploy_single_node(node, flake_override, &**runner).await;
            tracing::info!(
                "deployed {} — {}",
                result.hostname,
                if result.success { "ok" } else { "FAILED" }
            );
            results.push(result);
        }
        Ok(results)
    }
}

/// Deploys to the first node as canary; aborts if it fails, then
/// deploys the remaining nodes in parallel.
pub struct CanaryExecutor;

#[async_trait::async_trait]
impl DeployExecutor for CanaryExecutor {
    async fn execute(
        &self,
        nodes: &[Node],
        flake_override: Option<&str>,
        runner: &Arc<dyn CommandRunner>,
    ) -> Result<Vec<NodeDeployResult>, FleetError> {
        if nodes.is_empty() {
            return Ok(vec![]);
        }

        let canary = &nodes[0];
        let canary_result = deploy_single_node(canary, flake_override, &**runner).await;
        tracing::info!(
            "canary {} — {}",
            canary_result.hostname,
            if canary_result.success { "ok" } else { "FAILED" }
        );

        if !canary_result.success {
            return Err(FleetError::CanaryFailed);
        }

        let mut results = vec![canary_result];

        if nodes.len() > 1 {
            let remaining = deploy_parallel(&nodes[1..], flake_override, runner).await;
            results.extend(remaining);
        }

        Ok(results)
    }
}

impl DeployStrategy {
    /// Returns the executor that implements this strategy.
    #[must_use]
    pub fn executor(self) -> Box<dyn DeployExecutor> {
        match self {
            Self::Parallel => Box::new(ParallelExecutor),
            Self::Rolling => Box::new(RollingExecutor),
            Self::Canary => Box::new(CanaryExecutor),
        }
    }
}

/// Fleet orchestrator.
pub struct FleetOrchestrator {
    registry: NodeRegistry,
    runner: Arc<dyn CommandRunner>,
}

/// Fleet errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FleetError {
    #[error("no nodes match target: {0}")]
    NoNodes(String),
    #[error("deploy failed on {hostname}: {message}")]
    DeployFailed { hostname: String, message: String },
    #[error("command error: {0}")]
    Command(#[from] crate::command::CommandError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("canary failed — aborting remaining deploys")]
    CanaryFailed,
}

impl FleetOrchestrator {
    /// Create a new fleet orchestrator with the default command runner.
    #[must_use]
    pub fn new(registry: NodeRegistry) -> Self {
        Self {
            registry,
            runner: Arc::new(TokioCommandRunner::new()),
        }
    }

    /// Create with a custom command runner for testing.
    #[must_use]
    pub fn with_runner(registry: NodeRegistry, runner: Box<dyn CommandRunner>) -> Self {
        Self {
            registry,
            runner: Arc::from(runner),
        }
    }

    /// Returns a reference to the node registry.
    #[must_use]
    pub fn registry(&self) -> &NodeRegistry {
        &self.registry
    }

    /// Returns a mutable reference to the node registry.
    #[must_use]
    pub fn registry_mut(&mut self) -> &mut NodeRegistry {
        &mut self.registry
    }

    /// Deploy to a target using the given strategy.
    pub async fn deploy(
        &mut self,
        target: &str,
        strategy: DeployStrategy,
        flake_override: Option<&str>,
    ) -> Result<DeployResult, FleetError> {
        let executor = strategy.executor();
        let mut result = self
            .deploy_with_executor(target, &*executor, flake_override)
            .await?;
        result.strategy = strategy.to_string();
        Ok(result)
    }

    /// Deploy to a target using a custom [`DeployExecutor`].
    pub async fn deploy_with_executor(
        &mut self,
        target: &str,
        executor: &dyn DeployExecutor,
        flake_override: Option<&str>,
    ) -> Result<DeployResult, FleetError> {
        let start = std::time::Instant::now();

        let nodes: Vec<Node> = self
            .registry
            .resolve_target(target)
            .into_iter()
            .cloned()
            .collect();

        if nodes.is_empty() {
            return Err(FleetError::NoNodes(target.to_string()));
        }

        for node in &nodes {
            if let Some(n) = self.registry.get_mut(&node.hostname) {
                n.status = NodeStatus::Deploying;
            }
        }

        let results = executor.execute(&nodes, flake_override, &self.runner).await?;

        let succeeded = results.iter().filter(|r| r.success).count();
        let failed = results.iter().filter(|r| !r.success).count();

        for result in &results {
            if let Some(n) = self.registry.get_mut(&result.hostname) {
                n.status = if result.success {
                    NodeStatus::Online
                } else {
                    NodeStatus::Failed
                };
                if result.success {
                    n.last_deployed = Some(chrono::Utc::now().timestamp());
                }
            }
        }

        Ok(DeployResult {
            target: target.to_string(),
            strategy: "custom".to_string(),
            total_nodes: nodes.len(),
            succeeded,
            failed,
            results,
            duration_secs: start.elapsed().as_secs_f64(),
        })
    }
}

/// Deploy to all nodes in parallel via `tokio::spawn`.
async fn deploy_parallel(
    nodes: &[Node],
    flake_override: Option<&str>,
    runner: &Arc<dyn CommandRunner>,
) -> Vec<NodeDeployResult> {
    let mut handles = Vec::with_capacity(nodes.len());
    for node in nodes {
        let n = node.clone();
        let flake = flake_override.map(String::from);
        let runner = Arc::clone(runner);
        handles.push(tokio::spawn(async move {
            deploy_single_node(&n, flake.as_deref(), &*runner).await
        }));
    }

    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(e) => results.push(NodeDeployResult {
                hostname: "unknown".to_string(),
                success: false,
                log: format!("task panicked: {e}"),
                duration_secs: 0.0,
            }),
        }
    }
    results
}

/// Deploy to a single node via SSH + nixos-rebuild.
pub(crate) async fn deploy_single_node(
    node: &Node,
    flake_override: Option<&str>,
    runner: &dyn CommandRunner,
) -> NodeDeployResult {
    let start = std::time::Instant::now();
    let flake_ref = flake_override.unwrap_or(&node.flake_ref);
    let target = node.deploy_target();

    let rebuild_cmd = format!("{} switch --flake {flake_ref}", node.rebuild_command());

    // For local node, run directly; for remote, use SSH
    let output = if target == node.hostname && is_local_hostname(target) {
        runner.run("sh", &["-c", &rebuild_cmd]).await
    } else {
        runner
            .run(
                "ssh",
                &[
                    "-o",
                    "StrictHostKeyChecking=accept-new",
                    target,
                    &rebuild_cmd,
                ],
            )
            .await
    };

    let duration = start.elapsed().as_secs_f64();

    match output {
        Ok(out) => NodeDeployResult {
            hostname: node.hostname.clone(),
            success: out.success,
            log: out.combined_log(),
            duration_secs: duration,
        },
        Err(e) => NodeDeployResult {
            hostname: node.hostname.clone(),
            success: false,
            log: format!("failed to execute: {e}"),
            duration_secs: duration,
        },
    }
}

pub(crate) fn is_local_hostname(hostname: &str) -> bool {
    hostname == "localhost"
        || hostname == "127.0.0.1"
        || gethostname().is_some_and(|h| h == hostname)
}

pub(crate) fn gethostname() -> Option<String> {
    let mut buf = [0u8; 256];
    #[cfg(unix)]
    {
        let result = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if result == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return Some(String::from_utf8_lossy(&buf[..len]).to_string());
        }
    }
    let _ = buf;
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_registry() -> NodeRegistry {
        let mut reg = NodeRegistry::new();
        reg.add(
            Node::new("alpha", ".#alpha")
                .with_ssh("root@10.0.0.1")
                .with_groups(vec!["prod".to_string()])
                .with_system("x86_64-linux"),
        );
        reg.add(
            Node::new("beta", ".#beta")
                .with_ssh("root@10.0.0.2")
                .with_groups(vec!["prod".to_string()])
                .with_system("x86_64-linux"),
        );
        reg.add(
            Node::new("gamma", ".#gamma")
                .with_groups(vec!["staging".to_string()])
                .with_system("aarch64-darwin"),
        );
        reg
    }

    #[test]
    fn deploy_strategy_serialization() {
        let s = serde_json::to_string(&DeployStrategy::Rolling).unwrap();
        assert_eq!(s, "\"rolling\"");
        let parsed: DeployStrategy = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, DeployStrategy::Rolling);
    }

    #[test]
    fn fleet_orchestrator_creation() {
        let reg = test_registry();
        let orch = FleetOrchestrator::new(reg);
        assert_eq!(orch.registry().len(), 3);
    }

    #[test]
    fn no_nodes_error() {
        let reg = NodeRegistry::new();
        let mut orch = FleetOrchestrator::new(reg);
        let result = tokio::runtime::Runtime::new().unwrap().block_on(
            orch.deploy("nonexistent", DeployStrategy::Parallel, None),
        );
        assert!(result.is_err());
    }

    #[test]
    fn deploy_result_serialization() {
        let result = DeployResult {
            target: "@prod".to_string(),
            strategy: "rolling".to_string(),
            total_nodes: 2,
            succeeded: 2,
            failed: 0,
            results: vec![],
            duration_secs: 5.5,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"succeeded\":2"));
    }

    #[test]
    fn local_hostname_detection() {
        assert!(is_local_hostname("localhost"));
        assert!(is_local_hostname("127.0.0.1"));
    }

    // ── DeployStrategy serialization/deserialization ─────────

    #[test]
    fn deploy_strategy_parallel_serde() {
        let s = serde_json::to_string(&DeployStrategy::Parallel).unwrap();
        assert_eq!(s, "\"parallel\"");
        let parsed: DeployStrategy = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, DeployStrategy::Parallel);
    }

    #[test]
    fn deploy_strategy_canary_serde() {
        let s = serde_json::to_string(&DeployStrategy::Canary).unwrap();
        assert_eq!(s, "\"canary\"");
        let parsed: DeployStrategy = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, DeployStrategy::Canary);
    }

    // ── FleetError display ──────────────────────────────────

    #[test]
    fn fleet_error_no_nodes_display() {
        let e = FleetError::NoNodes("@ghost".to_string());
        assert!(e.to_string().contains("@ghost"));
    }

    #[test]
    fn fleet_error_deploy_failed_display() {
        let e = FleetError::DeployFailed {
            hostname: "plo".to_string(),
            message: "ssh timeout".to_string(),
        };
        let msg = e.to_string();
        assert!(msg.contains("plo"));
        assert!(msg.contains("ssh timeout"));
    }

    #[test]
    fn fleet_error_canary_failed_display() {
        let e = FleetError::CanaryFailed;
        assert!(e.to_string().contains("canary"));
    }

    // ── MockCommandRunner for fleet tests ────────────────────

    use crate::command::{CommandError, CommandOutput};

    struct MockCommandRunner {
        response: CommandOutput,
    }

    impl MockCommandRunner {
        fn succeeding() -> Self {
            Self {
                response: CommandOutput {
                    success: true,
                    stdout: "switched to generation 50\n".to_string(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            }
        }

        fn failing() -> Self {
            Self {
                response: CommandOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "build failed\n".to_string(),
                    exit_code: Some(1),
                },
            }
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for MockCommandRunner {
        async fn run(
            &self,
            _program: &str,
            _args: &[&str],
        ) -> Result<CommandOutput, CommandError> {
            Ok(self.response.clone())
        }
    }

    // ── FleetOrchestrator with MockCommandRunner ─────────────

    #[tokio::test]
    async fn deploy_rolling_with_mock_all_succeed() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        let result = orch
            .deploy("@prod", DeployStrategy::Rolling, None)
            .await
            .unwrap();

        assert_eq!(result.target, "@prod");
        assert_eq!(result.strategy, "rolling");
        assert_eq!(result.total_nodes, 2); // alpha + beta
        assert_eq!(result.succeeded, 2);
        assert_eq!(result.failed, 0);
        assert_eq!(result.results.len(), 2);
        for r in &result.results {
            assert!(r.success);
        }
    }

    #[tokio::test]
    async fn deploy_parallel_with_mock_all_succeed() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        let result = orch
            .deploy("@prod", DeployStrategy::Parallel, None)
            .await
            .unwrap();

        assert_eq!(result.succeeded, 2);
        assert_eq!(result.failed, 0);
    }

    #[tokio::test]
    async fn deploy_rolling_with_mock_all_fail() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::failing()),
        );

        let result = orch
            .deploy("@prod", DeployStrategy::Rolling, None)
            .await
            .unwrap();

        assert_eq!(result.succeeded, 0);
        assert_eq!(result.failed, 2);
        for r in &result.results {
            assert!(!r.success);
        }
    }

    #[tokio::test]
    async fn deploy_updates_node_status_on_success() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        orch.deploy("alpha", DeployStrategy::Rolling, None)
            .await
            .unwrap();

        assert_eq!(
            orch.registry().get("alpha").unwrap().status,
            NodeStatus::Online
        );
        assert!(orch.registry().get("alpha").unwrap().last_deployed.is_some());
    }

    #[tokio::test]
    async fn deploy_updates_node_status_on_failure() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::failing()),
        );

        orch.deploy("alpha", DeployStrategy::Rolling, None)
            .await
            .unwrap();

        assert_eq!(
            orch.registry().get("alpha").unwrap().status,
            NodeStatus::Failed
        );
    }

    // ── FleetOrchestrator with_runner + registry_mut ─────────

    #[test]
    fn fleet_orchestrator_registry_mut() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::new(reg);
        assert_eq!(orch.registry().len(), 3);

        orch.registry_mut().add(
            Node::new("delta", ".#delta").with_system("x86_64-linux"),
        );
        assert_eq!(orch.registry().len(), 4);
    }

    // ── NodeDeployResult serialization ───────────────────────

    #[test]
    fn node_deploy_result_serialization() {
        let result = NodeDeployResult {
            hostname: "plo".to_string(),
            success: true,
            log: "ok".to_string(),
            duration_secs: 2.5,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"hostname\":\"plo\""));
        assert!(json.contains("\"success\":true"));
    }

    // ── Canary deploy: canary fails, aborts rest ──────────────

    #[tokio::test]
    async fn deploy_canary_first_node_fails() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::failing()),
        );

        let result = orch
            .deploy("@prod", DeployStrategy::Canary, None)
            .await;

        match result {
            Err(FleetError::CanaryFailed) => {}
            other => panic!("expected CanaryFailed, got {other:?}"),
        }
    }

    // ── Canary deploy: canary succeeds, rest deploy ───────────

    #[tokio::test]
    async fn deploy_canary_success_deploys_rest() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        let result = orch
            .deploy("@prod", DeployStrategy::Canary, None)
            .await
            .unwrap();

        assert_eq!(result.total_nodes, 2);
        assert_eq!(result.succeeded, 2);
        assert_eq!(result.failed, 0);
        assert_eq!(result.strategy, "canary");
    }

    // ── Canary deploy: single node ────────────────────────────

    #[tokio::test]
    async fn deploy_canary_single_node() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        let result = orch
            .deploy("alpha", DeployStrategy::Canary, None)
            .await
            .unwrap();

        assert_eq!(result.total_nodes, 1);
        assert_eq!(result.succeeded, 1);
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].hostname, "alpha");
    }

    // ── Deploy with flake override ────────────────────────────

    #[tokio::test]
    async fn deploy_with_flake_override() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        let result = orch
            .deploy("alpha", DeployStrategy::Rolling, Some("github:my/repo#cfg"))
            .await
            .unwrap();

        assert_eq!(result.succeeded, 1);
        assert!(result.results[0].success);
    }

    // ── Deploy @all target ────────────────────────────────────

    #[tokio::test]
    async fn deploy_all_nodes() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        let result = orch
            .deploy("@all", DeployStrategy::Parallel, None)
            .await
            .unwrap();

        assert_eq!(result.total_nodes, 3);
        assert_eq!(result.succeeded, 3);
    }

    // ── Deploy sets Deploying status during deploy ────────────

    #[tokio::test]
    async fn deploy_rolling_strategy_label() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        let result = orch
            .deploy("@prod", DeployStrategy::Rolling, None)
            .await
            .unwrap();

        assert_eq!(result.strategy, "rolling");
    }

    #[tokio::test]
    async fn deploy_parallel_strategy_label() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        let result = orch
            .deploy("@prod", DeployStrategy::Parallel, None)
            .await
            .unwrap();

        assert_eq!(result.strategy, "parallel");
    }

    // ── DeployResult serde roundtrip ──────────────────────────

    #[test]
    fn deploy_result_serde_roundtrip() {
        let result = DeployResult {
            target: "@prod".to_string(),
            strategy: "rolling".to_string(),
            total_nodes: 2,
            succeeded: 1,
            failed: 1,
            results: vec![
                NodeDeployResult {
                    hostname: "a".to_string(),
                    success: true,
                    log: "ok".to_string(),
                    duration_secs: 1.0,
                },
                NodeDeployResult {
                    hostname: "b".to_string(),
                    success: false,
                    log: "err".to_string(),
                    duration_secs: 2.0,
                },
            ],
            duration_secs: 3.0,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: DeployResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.target, "@prod");
        assert_eq!(parsed.total_nodes, 2);
        assert_eq!(parsed.results.len(), 2);
    }

    // ── NodeDeployResult serde roundtrip ──────────────────────

    #[test]
    fn node_deploy_result_serde_roundtrip() {
        let result = NodeDeployResult {
            hostname: "x".to_string(),
            success: false,
            log: "failed".to_string(),
            duration_secs: 0.5,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: NodeDeployResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.hostname, "x");
        assert!(!parsed.success);
    }

    // ── Deploy staging group (darwin node) ─────────────────────

    #[tokio::test]
    async fn deploy_staging_group() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        let result = orch
            .deploy("@staging", DeployStrategy::Rolling, None)
            .await
            .unwrap();

        assert_eq!(result.total_nodes, 1);
        assert_eq!(result.results[0].hostname, "gamma");
    }

    // ── is_local_hostname ─────────────────────────────────────

    #[test]
    fn local_hostname_not_remote() {
        assert!(!is_local_hostname("10.0.0.1"));
        assert!(!is_local_hostname("remote.example.com"));
    }

    // ── FleetError from io::Error ─────────────────────────────

    #[test]
    fn fleet_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let fleet_err: FleetError = io_err.into();
        assert!(fleet_err.to_string().contains("nope"));
    }

    // ── Multiple deploys update state correctly ───────────────

    #[tokio::test]
    async fn successive_deploys_update_status() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        orch.deploy("alpha", DeployStrategy::Rolling, None)
            .await
            .unwrap();
        assert_eq!(
            orch.registry().get("alpha").unwrap().status,
            NodeStatus::Online
        );

        // Beta hasn't been deployed yet
        assert_eq!(
            orch.registry().get("beta").unwrap().status,
            NodeStatus::Unknown
        );
    }

    // ── DeployStrategy: Default + Display + FromStr ───────────

    #[test]
    fn deploy_strategy_default_is_rolling() {
        assert_eq!(DeployStrategy::default(), DeployStrategy::Rolling);
    }

    #[test]
    fn deploy_strategy_display_strings() {
        assert_eq!(DeployStrategy::Parallel.to_string(), "parallel");
        assert_eq!(DeployStrategy::Rolling.to_string(), "rolling");
        assert_eq!(DeployStrategy::Canary.to_string(), "canary");
    }

    #[test]
    fn deploy_strategy_from_str_valid() {
        use std::str::FromStr;
        assert_eq!(
            DeployStrategy::from_str("parallel").unwrap(),
            DeployStrategy::Parallel
        );
        assert_eq!(
            DeployStrategy::from_str("rolling").unwrap(),
            DeployStrategy::Rolling
        );
        assert_eq!(
            DeployStrategy::from_str("canary").unwrap(),
            DeployStrategy::Canary
        );
    }

    #[test]
    fn deploy_strategy_from_str_rejects_garbage() {
        use std::str::FromStr;
        let err = DeployStrategy::from_str("yolo").unwrap_err();
        assert!(err.contains("invalid deploy strategy"));
        assert!(err.contains("yolo"));
    }

    #[test]
    fn deploy_strategy_from_str_case_sensitive() {
        use std::str::FromStr;
        assert!(DeployStrategy::from_str("Rolling").is_err());
        assert!(DeployStrategy::from_str("CANARY").is_err());
        assert!(DeployStrategy::from_str("").is_err());
    }

    // ── DeployStrategy::executor returns the right kind ───────

    #[tokio::test]
    async fn deploy_strategy_executor_rolling_runs_serially() {
        let exec = DeployStrategy::Rolling.executor();
        let runner: Arc<dyn CommandRunner> = Arc::new(MockCommandRunner::succeeding());
        let nodes = vec![
            Node::new("a", ".#a")
                .with_ssh("root@a")
                .with_system("x86_64-linux"),
            Node::new("b", ".#b")
                .with_ssh("root@b")
                .with_system("x86_64-linux"),
        ];
        let results = exec.execute(&nodes, None, &runner).await.unwrap();
        assert_eq!(results.len(), 2);
        // Rolling preserves input order
        assert_eq!(results[0].hostname, "a");
        assert_eq!(results[1].hostname, "b");
        assert!(results.iter().all(|r| r.success));
    }

    #[tokio::test]
    async fn deploy_strategy_executor_parallel_returns_all_results() {
        let exec = DeployStrategy::Parallel.executor();
        let runner: Arc<dyn CommandRunner> = Arc::new(MockCommandRunner::succeeding());
        let nodes = vec![
            Node::new("a", ".#a")
                .with_ssh("root@a")
                .with_system("x86_64-linux"),
            Node::new("b", ".#b")
                .with_ssh("root@b")
                .with_system("x86_64-linux"),
            Node::new("c", ".#c")
                .with_ssh("root@c")
                .with_system("x86_64-linux"),
        ];
        let results = exec.execute(&nodes, None, &runner).await.unwrap();
        assert_eq!(results.len(), 3);
        let mut hostnames: Vec<&str> = results.iter().map(|r| r.hostname.as_str()).collect();
        hostnames.sort_unstable();
        assert_eq!(hostnames, vec!["a", "b", "c"]);
        assert!(results.iter().all(|r| r.success));
    }

    #[tokio::test]
    async fn deploy_strategy_executor_canary_empty_node_list() {
        let exec = DeployStrategy::Canary.executor();
        let runner: Arc<dyn CommandRunner> = Arc::new(MockCommandRunner::succeeding());
        let results = exec.execute(&[], None, &runner).await.unwrap();
        assert!(results.is_empty());
    }

    // ── ParallelExecutor / RollingExecutor / CanaryExecutor: direct usage ─

    #[tokio::test]
    async fn parallel_executor_direct_call() {
        let runner: Arc<dyn CommandRunner> = Arc::new(MockCommandRunner::succeeding());
        let nodes = vec![Node::new("only", ".#only")
            .with_ssh("root@only")
            .with_system("x86_64-linux")];
        let results = ParallelExecutor.execute(&nodes, None, &runner).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
    }

    #[tokio::test]
    async fn rolling_executor_direct_call() {
        let runner: Arc<dyn CommandRunner> = Arc::new(MockCommandRunner::succeeding());
        let nodes = vec![Node::new("only", ".#only")
            .with_ssh("root@only")
            .with_system("x86_64-linux")];
        let results = RollingExecutor.execute(&nodes, None, &runner).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].hostname, "only");
    }

    #[tokio::test]
    async fn canary_executor_direct_call_succeeds_on_single_node() {
        let runner: Arc<dyn CommandRunner> = Arc::new(MockCommandRunner::succeeding());
        let nodes = vec![Node::new("only", ".#only")
            .with_ssh("root@only")
            .with_system("x86_64-linux")];
        let results = CanaryExecutor.execute(&nodes, None, &runner).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
    }

    #[tokio::test]
    async fn canary_executor_aborts_when_first_node_fails() {
        let runner: Arc<dyn CommandRunner> = Arc::new(MockCommandRunner::failing());
        let nodes = vec![
            Node::new("a", ".#a").with_ssh("root@a").with_system("x86_64-linux"),
            Node::new("b", ".#b").with_ssh("root@b").with_system("x86_64-linux"),
        ];
        let result = CanaryExecutor.execute(&nodes, None, &runner).await;
        match result {
            Err(FleetError::CanaryFailed) => {}
            other => panic!("expected CanaryFailed, got {other:?}"),
        }
    }

    // ── Mixed/partial failure with a per-node mock ────────────

    /// Fails for any host listed in `failing_hosts`, succeeds otherwise.
    /// Recognises hosts via the SSH target embedded in the rebuild args.
    struct PartialFailureRunner {
        failing_hosts: Vec<String>,
    }

    #[async_trait::async_trait]
    impl CommandRunner for PartialFailureRunner {
        async fn run(&self, _program: &str, args: &[&str]) -> Result<CommandOutput, CommandError> {
            // The deploy_single_node call uses ssh args "ssh -o ... <target> <cmd>"
            // for remote nodes, so the SSH target sits at index 2.
            let target = args.get(2).copied().unwrap_or("");
            let fail = self.failing_hosts.iter().any(|h| target.contains(h));
            if fail {
                Ok(CommandOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: format!("forced failure for {target}\n"),
                    exit_code: Some(1),
                })
            } else {
                Ok(CommandOutput {
                    success: true,
                    stdout: format!("ok for {target}\n"),
                    stderr: String::new(),
                    exit_code: Some(0),
                })
            }
        }
    }

    #[tokio::test]
    async fn deploy_rolling_partial_failure_aggregates_results() {
        let reg = test_registry();
        let runner = PartialFailureRunner {
            failing_hosts: vec!["10.0.0.2".to_string()], // beta fails
        };
        let mut orch = FleetOrchestrator::with_runner(reg, Box::new(runner));

        let result = orch
            .deploy("@prod", DeployStrategy::Rolling, None)
            .await
            .unwrap();

        assert_eq!(result.total_nodes, 2);
        assert_eq!(result.succeeded, 1);
        assert_eq!(result.failed, 1);

        // Find each result
        let alpha = result.results.iter().find(|r| r.hostname == "alpha").unwrap();
        let beta = result.results.iter().find(|r| r.hostname == "beta").unwrap();
        assert!(alpha.success);
        assert!(!beta.success);
        assert!(beta.log.contains("forced failure"));

        // Registry status reflects per-node outcome
        assert_eq!(
            orch.registry().get("alpha").unwrap().status,
            NodeStatus::Online
        );
        assert_eq!(
            orch.registry().get("beta").unwrap().status,
            NodeStatus::Failed
        );
    }

    #[tokio::test]
    async fn deploy_parallel_partial_failure_aggregates_results() {
        let reg = test_registry();
        let runner = PartialFailureRunner {
            failing_hosts: vec!["10.0.0.1".to_string()], // alpha fails
        };
        let mut orch = FleetOrchestrator::with_runner(reg, Box::new(runner));

        let result = orch
            .deploy("@prod", DeployStrategy::Parallel, None)
            .await
            .unwrap();

        assert_eq!(result.total_nodes, 2);
        assert_eq!(result.succeeded, 1);
        assert_eq!(result.failed, 1);
    }

    // ── Rolling deploy preserves order ───────────────────────

    #[tokio::test]
    async fn deploy_rolling_results_in_target_order() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );
        // resolve_target on @all returns BTreeMap order, which is alphabetical
        let result = orch
            .deploy("@all", DeployStrategy::Rolling, None)
            .await
            .unwrap();
        let hostnames: Vec<&str> = result
            .results
            .iter()
            .map(|r| r.hostname.as_str())
            .collect();
        assert_eq!(hostnames, vec!["alpha", "beta", "gamma"]);
    }

    // ── Successful canary records canary first ────────────────

    #[tokio::test]
    async fn canary_succeeds_canary_node_recorded_first() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );
        let result = orch
            .deploy("@prod", DeployStrategy::Canary, None)
            .await
            .unwrap();
        assert_eq!(result.results.len(), 2);
        // Canary is the first node (alphabetical) — alpha
        assert_eq!(result.results[0].hostname, "alpha");
    }

    // ── Canary deploy then rollback path: failing canary leaves state ─

    #[tokio::test]
    async fn deploy_canary_failed_marks_canary_node_failed() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::failing()),
        );
        let _ = orch.deploy("@prod", DeployStrategy::Canary, None).await;
        // Both prod nodes were marked Deploying before execution; canary
        // fails before per-node status updates run, so they remain
        // in the transitional Deploying state.
        let alpha = orch.registry().get("alpha").unwrap();
        let beta = orch.registry().get("beta").unwrap();
        assert!(matches!(
            alpha.status,
            NodeStatus::Deploying | NodeStatus::Failed
        ));
        assert!(matches!(
            beta.status,
            NodeStatus::Deploying | NodeStatus::Failed
        ));
    }

    // ── deploy_with_executor: direct trait usage labels strategy "custom" ─

    #[tokio::test]
    async fn deploy_with_executor_uses_custom_label() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );
        let executor = ParallelExecutor;
        let result = orch
            .deploy_with_executor("@prod", &executor, None)
            .await
            .unwrap();
        // The high-level deploy() overrides this label, but the lower-level
        // entry point keeps the default "custom" tag.
        assert_eq!(result.strategy, "custom");
        assert_eq!(result.succeeded, 2);
    }

    // ── deploy_with_executor: empty target errors ─────────────

    #[tokio::test]
    async fn deploy_with_executor_no_nodes_errors() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );
        let executor = RollingExecutor;
        let result = orch
            .deploy_with_executor("@nonexistent-group", &executor, None)
            .await;
        match result {
            Err(FleetError::NoNodes(target)) => assert_eq!(target, "@nonexistent-group"),
            other => panic!("expected NoNodes, got {other:?}"),
        }
    }

    // ── FleetError From CommandError conversion ───────────────

    #[test]
    fn fleet_error_from_command_error() {
        let cmd_err = crate::command::CommandError::NotFound("ssh".to_string());
        let fleet_err: FleetError = cmd_err.into();
        let s = fleet_err.to_string();
        assert!(s.contains("command error"));
        assert!(s.contains("ssh"));
    }

    // ── DeployStrategy serde: every variant string ────────────

    #[test]
    fn deploy_strategy_serde_all_variants() {
        for (strat, expected) in [
            (DeployStrategy::Parallel, "\"parallel\""),
            (DeployStrategy::Rolling, "\"rolling\""),
            (DeployStrategy::Canary, "\"canary\""),
        ] {
            let json = serde_json::to_string(&strat).unwrap();
            assert_eq!(json, expected);
            let parsed: DeployStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, strat);
        }
    }

    // ── DeployResult totals match per-node sum ────────────────

    #[tokio::test]
    async fn deploy_result_totals_consistent_with_node_results() {
        let reg = test_registry();
        let runner = PartialFailureRunner {
            failing_hosts: vec!["10.0.0.2".to_string()],
        };
        let mut orch = FleetOrchestrator::with_runner(reg, Box::new(runner));
        let result = orch
            .deploy("@prod", DeployStrategy::Parallel, None)
            .await
            .unwrap();

        let succeeded = result.results.iter().filter(|r| r.success).count();
        let failed = result.results.iter().filter(|r| !r.success).count();
        assert_eq!(result.succeeded, succeeded);
        assert_eq!(result.failed, failed);
        assert_eq!(result.succeeded + result.failed, result.total_nodes);
    }

    // ── deploy() unknown target errors ────────────────────────

    #[tokio::test]
    async fn deploy_unknown_hostname_errors_no_nodes() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );
        let result = orch
            .deploy("nonexistent-host", DeployStrategy::Rolling, None)
            .await;
        match result {
            Err(FleetError::NoNodes(target)) => assert_eq!(target, "nonexistent-host"),
            other => panic!("expected NoNodes, got {other:?}"),
        }
    }

    // ── deploy_strategy hash + equality ───────────────────────

    #[test]
    fn deploy_strategy_equality_and_copy() {
        let a = DeployStrategy::Parallel;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(a, DeployStrategy::Rolling);
    }

    // ── gethostname returns Some on unix ──────────────────────

    #[cfg(unix)]
    #[test]
    fn gethostname_returns_some_on_unix() {
        let h = gethostname();
        assert!(h.is_some());
        assert!(!h.unwrap().is_empty());
    }

    // ── is_local_hostname matches host's own name ─────────────

    #[cfg(unix)]
    #[test]
    fn is_local_hostname_matches_local_machine() {
        let host = gethostname().unwrap();
        assert!(is_local_hostname(&host));
    }

    // ── is_local_hostname empty string is not local ───────────

    #[test]
    fn is_local_hostname_empty_string() {
        assert!(!is_local_hostname(""));
    }

    // ── Custom DeployExecutor proves the trait extensibility ──

    /// Always returns a synthetic "skipped" record for every node,
    /// without invoking the runner. Demonstrates that callers can
    /// supply arbitrary deployment patterns by implementing
    /// [`DeployExecutor`].
    struct SkipExecutor;

    #[async_trait::async_trait]
    impl DeployExecutor for SkipExecutor {
        async fn execute(
            &self,
            nodes: &[Node],
            _flake_override: Option<&str>,
            _runner: &Arc<dyn CommandRunner>,
        ) -> Result<Vec<NodeDeployResult>, FleetError> {
            Ok(nodes
                .iter()
                .map(|n| NodeDeployResult {
                    hostname: n.hostname.clone(),
                    success: true,
                    log: format!("skipped {}", n.hostname),
                    duration_secs: 0.0,
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn deploy_with_custom_executor_via_trait() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );
        let executor = SkipExecutor;
        let result = orch
            .deploy_with_executor("@all", &executor, None)
            .await
            .unwrap();
        assert_eq!(result.total_nodes, 3);
        assert_eq!(result.succeeded, 3);
        assert_eq!(result.strategy, "custom");
        for r in &result.results {
            assert!(r.log.contains("skipped"));
        }
    }

    /// Always rejects with a typed error. Used to test error propagation
    /// from a custom executor through deploy_with_executor.
    struct AlwaysRejectExecutor;

    #[async_trait::async_trait]
    impl DeployExecutor for AlwaysRejectExecutor {
        async fn execute(
            &self,
            _nodes: &[Node],
            _flake_override: Option<&str>,
            _runner: &Arc<dyn CommandRunner>,
        ) -> Result<Vec<NodeDeployResult>, FleetError> {
            Err(FleetError::CanaryFailed)
        }
    }

    #[tokio::test]
    async fn deploy_with_custom_executor_error_bubbles_up() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );
        let result = orch
            .deploy_with_executor("@prod", &AlwaysRejectExecutor, None)
            .await;
        match result {
            Err(FleetError::CanaryFailed) => {}
            other => panic!("expected CanaryFailed, got {other:?}"),
        }
    }

    // ── last_deployed must not be set on failure ──────────────

    #[tokio::test]
    async fn deploy_failure_does_not_set_last_deployed() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::failing()),
        );
        orch.deploy("alpha", DeployStrategy::Rolling, None)
            .await
            .unwrap();
        let alpha = orch.registry().get("alpha").unwrap();
        assert_eq!(alpha.status, NodeStatus::Failed);
        assert!(alpha.last_deployed.is_none());
    }

    // ── last_deployed is set on success ───────────────────────

    #[tokio::test]
    async fn deploy_success_sets_last_deployed_timestamp() {
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );
        let before = chrono::Utc::now().timestamp();
        orch.deploy("alpha", DeployStrategy::Rolling, None)
            .await
            .unwrap();
        let after = chrono::Utc::now().timestamp();
        let alpha = orch.registry().get("alpha").unwrap();
        let ts = alpha.last_deployed.expect("timestamp set on success");
        assert!(ts >= before);
        assert!(ts <= after);
    }

    // ── Node::deploy_target falls back to hostname when ssh unset ─

    #[test]
    fn deploy_target_falls_back_to_hostname() {
        let node = Node::new("solo", ".#solo");
        assert_eq!(node.deploy_target(), "solo");
    }

    #[test]
    fn deploy_target_uses_ssh_when_set() {
        let node = Node::new("solo", ".#solo").with_ssh("user@host.example.com");
        assert_eq!(node.deploy_target(), "user@host.example.com");
    }
}
