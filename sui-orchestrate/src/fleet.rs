//! Fleet deployment orchestration.
//!
//! Supports parallel, rolling, and canary deploy strategies.

use std::sync::Arc;

use crate::command::{CommandRunner, TokioCommandRunner};
use crate::node::{Node, NodeRegistry, NodeStatus};

/// Deploy strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

        // Mark all as deploying
        for node in &nodes {
            if let Some(n) = self.registry.get_mut(&node.hostname) {
                n.status = NodeStatus::Deploying;
            }
        }

        let results = match strategy {
            DeployStrategy::Parallel => self.deploy_parallel(&nodes, flake_override).await,
            DeployStrategy::Rolling => self.deploy_rolling(&nodes, flake_override).await,
            DeployStrategy::Canary => self.deploy_canary(&nodes, flake_override).await?,
        };

        let succeeded = results.iter().filter(|r| r.success).count();
        let failed = results.iter().filter(|r| !r.success).count();

        // Update node statuses
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
            strategy: strategy.to_string(),
            total_nodes: nodes.len(),
            succeeded,
            failed,
            results,
            duration_secs: start.elapsed().as_secs_f64(),
        })
    }

    /// Deploy to all nodes in parallel.
    async fn deploy_parallel(
        &self,
        nodes: &[Node],
        flake_override: Option<&str>,
    ) -> Vec<NodeDeployResult> {
        let mut handles = Vec::new();
        for node in nodes {
            let n = node.clone();
            let flake = flake_override.map(String::from);
            let runner = Arc::clone(&self.runner);
            handles.push(tokio::spawn(async move {
                deploy_single_node(&n, flake.as_deref(), &*runner).await
            }));
        }

        let mut results = Vec::new();
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

    /// Deploy one node at a time.
    async fn deploy_rolling(
        &self,
        nodes: &[Node],
        flake_override: Option<&str>,
    ) -> Vec<NodeDeployResult> {
        let mut results = Vec::new();
        for node in nodes {
            let result = deploy_single_node(node, flake_override, &*self.runner).await;
            tracing::info!(
                "deployed {} — {}",
                result.hostname,
                if result.success { "ok" } else { "FAILED" }
            );
            results.push(result);
        }
        results
    }

    /// Deploy canary first, then remaining if healthy.
    async fn deploy_canary(
        &self,
        nodes: &[Node],
        flake_override: Option<&str>,
    ) -> Result<Vec<NodeDeployResult>, FleetError> {
        if nodes.is_empty() {
            return Ok(vec![]);
        }

        // First node is canary
        let canary = &nodes[0];
        let canary_result = deploy_single_node(canary, flake_override, &*self.runner).await;
        tracing::info!(
            "canary {} — {}",
            canary_result.hostname,
            if canary_result.success { "ok" } else { "FAILED" }
        );

        if !canary_result.success {
            return Err(FleetError::CanaryFailed);
        }

        let mut results = vec![canary_result];

        // Deploy remaining in parallel
        if nodes.len() > 1 {
            let remaining = self.deploy_parallel(&nodes[1..], flake_override).await;
            results.extend(remaining);
        }

        Ok(results)
    }
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

    // Build the remote rebuild command
    let rebuild_cmd = if node.system.as_deref() == Some("aarch64-darwin")
        || node.system.as_deref() == Some("x86_64-darwin")
    {
        format!("darwin-rebuild switch --flake {flake_ref}")
    } else {
        format!("nixos-rebuild switch --flake {flake_ref}")
    };

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
}
