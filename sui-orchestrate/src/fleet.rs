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

/// Order in which nodes are deployed.
///
/// Strategies (parallel/rolling/canary) decide *how* to execute each node;
/// `DeployOrder` decides *in what sequence* nodes are presented to the executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum DeployOrder {
    /// Order by hostname (the historical default — `BTreeMap` iteration order).
    Alphabetical,
    /// Topological order using `Node::depends_on` — leaves (no deps) first,
    /// roots (most depended-on) last. Cycles return [`FleetError::Cycle`].
    Dependency,
}

impl Default for DeployOrder {
    fn default() -> Self {
        Self::Alphabetical
    }
}

impl std::fmt::Display for DeployOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alphabetical => f.write_str("alphabetical"),
            Self::Dependency => f.write_str("dependency"),
        }
    }
}

impl std::str::FromStr for DeployOrder {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "alphabetical" => Ok(Self::Alphabetical),
            "dependency" => Ok(Self::Dependency),
            other => Err(format!("invalid deploy order: {other}")),
        }
    }
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
    /// Dependency-order resolution detected a cycle.
    #[error("dependency cycle detected among nodes: {nodes:?}")]
    Cycle {
        /// Hostnames participating in the cycle (the unresolved set).
        nodes: Vec<String>,
    },
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
        self.deploy_with_order(target, strategy, DeployOrder::Alphabetical, flake_override)
            .await
    }

    /// Deploy to a target with an explicit [`DeployOrder`].
    ///
    /// `DeployOrder::Alphabetical` matches the historical default; the
    /// `Dependency` variant performs a topological sort over `Node::depends_on`
    /// before handing nodes to the executor — leaves first, roots last.
    pub async fn deploy_with_order(
        &mut self,
        target: &str,
        strategy: DeployStrategy,
        order: DeployOrder,
        flake_override: Option<&str>,
    ) -> Result<DeployResult, FleetError> {
        let executor = strategy.executor();
        let mut result = self
            .deploy_with_executor_ordered(target, &*executor, order, flake_override)
            .await?;
        result.strategy = strategy.to_string();
        Ok(result)
    }

    /// Deploy to a target using a custom [`DeployExecutor`].
    ///
    /// Equivalent to [`deploy_with_executor_ordered`](Self::deploy_with_executor_ordered)
    /// with [`DeployOrder::Alphabetical`].
    pub async fn deploy_with_executor(
        &mut self,
        target: &str,
        executor: &dyn DeployExecutor,
        flake_override: Option<&str>,
    ) -> Result<DeployResult, FleetError> {
        self.deploy_with_executor_ordered(
            target,
            executor,
            DeployOrder::Alphabetical,
            flake_override,
        )
        .await
    }

    /// Deploy to a target using a custom [`DeployExecutor`] and an explicit
    /// [`DeployOrder`].
    ///
    /// # Errors
    ///
    /// - [`FleetError::NoNodes`] if `target` resolves to zero nodes.
    /// - [`FleetError::Cycle`] if `order` is [`DeployOrder::Dependency`] and
    ///   the resolved nodes form a dependency cycle.
    /// - [`FleetError::DeployFailed`] if a *single-node* deploy fails — i.e.
    ///   `target` resolved to exactly one node and that deploy did not
    ///   succeed. Multi-node deploys always return `Ok(DeployResult)`; the
    ///   per-node failures are visible via `succeeded` / `failed` / `results`.
    /// - Errors propagated from the underlying [`DeployExecutor::execute`].
    pub async fn deploy_with_executor_ordered(
        &mut self,
        target: &str,
        executor: &dyn DeployExecutor,
        order: DeployOrder,
        flake_override: Option<&str>,
    ) -> Result<DeployResult, FleetError> {
        let start = std::time::Instant::now();

        let mut nodes: Vec<Node> = self
            .registry
            .resolve_target(target)
            .into_iter()
            .cloned()
            .collect();

        if nodes.is_empty() {
            return Err(FleetError::NoNodes(target.to_string()));
        }

        // Apply deploy order before handing nodes to the executor.
        if order == DeployOrder::Dependency {
            nodes = topo_sort(nodes)?;
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

        // Single-node deploys surface a typed `DeployFailed` error rather than
        // hiding the failure inside an `Ok(DeployResult { failed: 1 })`. This
        // matches caller expectations for one-shot deploys (e.g. `sui deploy plo`).
        // Multi-node deploys keep returning `Ok` so callers can inspect partial
        // success and decide how to proceed.
        if nodes.len() == 1 && failed == 1 {
            let failed_result = results
                .into_iter()
                .next()
                .expect("single-node deploy must have one result");
            return Err(FleetError::DeployFailed {
                hostname: failed_result.hostname,
                message: failed_result.log,
            });
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

/// Topologically sort `nodes` so that each node appears after every node it
/// depends on (via `Node::depends_on`). Leaves (nodes with no dependencies in
/// the input set) come first; roots (most depended-on) come last.
///
/// Dependencies on hostnames *not present* in the input set are ignored — this
/// keeps `@group` deploys working when only a subset of the fleet is targeted.
/// Within each "rank" (nodes whose dependencies are already satisfied), output
/// is alphabetical for determinism.
///
/// Returns [`FleetError::Cycle`] if the dependency graph contains a cycle. The
/// `nodes` field of the error contains the unresolved hostnames.
pub fn topo_sort(nodes: Vec<Node>) -> Result<Vec<Node>, FleetError> {
    use std::collections::{BTreeMap, BTreeSet};

    // Build a hostname → Node lookup. Using BTreeMap gives us deterministic
    // alphabetical iteration when picking the next leaf to emit.
    let mut by_name: BTreeMap<String, Node> = BTreeMap::new();
    for node in nodes {
        by_name.insert(node.hostname.clone(), node);
    }
    let present: BTreeSet<String> = by_name.keys().cloned().collect();

    // in_degree[N] = number of N's dependencies that are still in the set.
    let mut in_degree: BTreeMap<String, usize> = BTreeMap::new();
    // dependents[N] = nodes that list N in their depends_on (edges N → M).
    let mut dependents: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (name, node) in &by_name {
        let deps_in_set: Vec<&String> = node
            .depends_on
            .iter()
            .filter(|d| present.contains(*d))
            .collect();
        in_degree.insert(name.clone(), deps_in_set.len());
        for dep in deps_in_set {
            dependents
                .entry(dep.clone())
                .or_default()
                .push(name.clone());
        }
    }

    // Seed the queue with all leaves (in-degree 0) in *descending* order so
    // that `pop()` (LIFO) yields the alphabetically smallest leaf first.
    let mut queue: Vec<String> = in_degree
        .iter()
        .filter_map(|(name, deg)| if *deg == 0 { Some(name.clone()) } else { None })
        .collect();
    queue.sort_by(|a, b| b.cmp(a));

    let mut sorted: Vec<Node> = Vec::with_capacity(by_name.len());
    while let Some(name) = queue.pop() {
        let node = by_name
            .remove(&name)
            .expect("queued name must still be in the map");
        sorted.push(node);

        if let Some(children) = dependents.get(&name) {
            for child in children {
                if let Some(deg) = in_degree.get_mut(child) {
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 {
                        queue.push(child.clone());
                    }
                }
            }
        }
        // Re-sort descending so pop() continues to yield smallest-first.
        // For typical fleet sizes (< 100 nodes) this overhead is negligible.
        queue.sort_by(|a, b| b.cmp(a));
    }

    if !by_name.is_empty() {
        // Whatever remains is unreachable from any leaf — cycle.
        let mut unresolved: Vec<String> = by_name.into_keys().collect();
        unresolved.sort();
        return Err(FleetError::Cycle { nodes: unresolved });
    }

    Ok(sorted)
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

        // Single-node deploy failure is now surfaced as DeployFailed; the
        // registry still gets updated to Failed before the error is returned.
        let err = orch
            .deploy("alpha", DeployStrategy::Rolling, None)
            .await
            .expect_err("single-node deploy should surface DeployFailed");
        match err {
            FleetError::DeployFailed { hostname, .. } => assert_eq!(hostname, "alpha"),
            other => panic!("expected DeployFailed, got {other:?}"),
        }

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

    // ── DeployOrder enum basics ───────────────────────────────

    #[test]
    fn deploy_order_default_is_alphabetical() {
        assert_eq!(DeployOrder::default(), DeployOrder::Alphabetical);
    }

    #[test]
    fn deploy_order_display() {
        assert_eq!(DeployOrder::Alphabetical.to_string(), "alphabetical");
        assert_eq!(DeployOrder::Dependency.to_string(), "dependency");
    }

    #[test]
    fn deploy_order_parse() {
        assert_eq!(
            "alphabetical".parse::<DeployOrder>().unwrap(),
            DeployOrder::Alphabetical
        );
        assert_eq!(
            "dependency".parse::<DeployOrder>().unwrap(),
            DeployOrder::Dependency
        );
        assert!("nonsense".parse::<DeployOrder>().is_err());
    }

    #[test]
    fn deploy_order_serde_roundtrip() {
        for order in [DeployOrder::Alphabetical, DeployOrder::Dependency] {
            let json = serde_json::to_string(&order).unwrap();
            let parsed: DeployOrder = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, order);
        }
    }

    // ── topo_sort: happy path ─────────────────────────────────

    #[test]
    fn topo_sort_linear_chain() {
        // c depends on b, b depends on a → expect [a, b, c].
        let nodes = vec![
            Node::new("c", ".#c").with_depends_on(vec!["b".to_string()]),
            Node::new("a", ".#a"),
            Node::new("b", ".#b").with_depends_on(vec!["a".to_string()]),
        ];
        let sorted = topo_sort(nodes).unwrap();
        let names: Vec<&str> = sorted.iter().map(|n| n.hostname.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn topo_sort_diamond() {
        // a → {b, c} → d. b and c are independent, so they sort alphabetically.
        let nodes = vec![
            Node::new("d", ".#d")
                .with_depends_on(vec!["b".to_string(), "c".to_string()]),
            Node::new("a", ".#a"),
            Node::new("b", ".#b").with_depends_on(vec!["a".to_string()]),
            Node::new("c", ".#c").with_depends_on(vec!["a".to_string()]),
        ];
        let sorted = topo_sort(nodes).unwrap();
        let names: Vec<&str> = sorted.iter().map(|n| n.hostname.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn topo_sort_independent_nodes_alphabetical() {
        // No edges → pure alphabetical order.
        let nodes = vec![
            Node::new("zeta", ".#zeta"),
            Node::new("alpha", ".#alpha"),
            Node::new("mu", ".#mu"),
        ];
        let sorted = topo_sort(nodes).unwrap();
        let names: Vec<&str> = sorted.iter().map(|n| n.hostname.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mu", "zeta"]);
    }

    // ── topo_sort: error paths ────────────────────────────────

    #[test]
    fn topo_sort_simple_cycle() {
        // a depends on b, b depends on a → cycle.
        let nodes = vec![
            Node::new("a", ".#a").with_depends_on(vec!["b".to_string()]),
            Node::new("b", ".#b").with_depends_on(vec!["a".to_string()]),
        ];
        match topo_sort(nodes) {
            Err(FleetError::Cycle { nodes }) => {
                assert_eq!(nodes, vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn topo_sort_self_cycle() {
        // a depends on itself → cycle.
        let nodes = vec![Node::new("a", ".#a").with_depends_on(vec!["a".to_string()])];
        match topo_sort(nodes) {
            Err(FleetError::Cycle { nodes }) => {
                assert_eq!(nodes, vec!["a".to_string()]);
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn topo_sort_three_node_cycle_with_leaf() {
        // a → b → c → a (cycle), plus d (leaf). d emits, a/b/c remain.
        let nodes = vec![
            Node::new("a", ".#a").with_depends_on(vec!["c".to_string()]),
            Node::new("b", ".#b").with_depends_on(vec!["a".to_string()]),
            Node::new("c", ".#c").with_depends_on(vec!["b".to_string()]),
            Node::new("d", ".#d"),
        ];
        match topo_sort(nodes) {
            Err(FleetError::Cycle { nodes }) => {
                assert_eq!(
                    nodes,
                    vec!["a".to_string(), "b".to_string(), "c".to_string()]
                );
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    // ── topo_sort: edge cases ─────────────────────────────────

    #[test]
    fn topo_sort_empty_input() {
        let sorted = topo_sort(vec![]).unwrap();
        assert!(sorted.is_empty());
    }

    #[test]
    fn topo_sort_single_node() {
        let sorted = topo_sort(vec![Node::new("only", ".#only")]).unwrap();
        assert_eq!(sorted.len(), 1);
        assert_eq!(sorted[0].hostname, "only");
    }

    #[test]
    fn topo_sort_single_node_with_depends_on_outside_set() {
        // Dependency on a hostname not present in the input is silently ignored.
        let sorted = topo_sort(vec![
            Node::new("only", ".#only").with_depends_on(vec!["nonexistent".to_string()]),
        ])
        .unwrap();
        assert_eq!(sorted.len(), 1);
        assert_eq!(sorted[0].hostname, "only");
    }

    #[test]
    fn topo_sort_ignores_deps_outside_input_set() {
        // Useful for @group deploys: when only a subset of the fleet is
        // targeted, deps on out-of-set nodes do not block sorting.
        let nodes = vec![
            Node::new("a", ".#a").with_depends_on(vec!["external".to_string()]),
            Node::new("b", ".#b").with_depends_on(vec!["a".to_string()]),
        ];
        let sorted = topo_sort(nodes).unwrap();
        let names: Vec<&str> = sorted.iter().map(|n| n.hostname.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    // ── deploy_with_order integration ─────────────────────────

    fn dependency_test_registry() -> NodeRegistry {
        let mut reg = NodeRegistry::new();
        reg.add(
            Node::new("db", ".#db")
                .with_groups(vec!["infra".to_string()])
                .with_system("x86_64-linux"),
        );
        reg.add(
            Node::new("api", ".#api")
                .with_groups(vec!["infra".to_string()])
                .with_system("x86_64-linux")
                .with_depends_on(vec!["db".to_string()]),
        );
        reg.add(
            Node::new("web", ".#web")
                .with_groups(vec!["infra".to_string()])
                .with_system("x86_64-linux")
                .with_depends_on(vec!["api".to_string()]),
        );
        reg
    }

    #[tokio::test]
    async fn deploy_with_order_dependency_orders_nodes() {
        // Use a recording runner to verify the deploy sequence.
        struct OrderRecordingRunner {
            sequence: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        }

        #[async_trait::async_trait]
        impl CommandRunner for OrderRecordingRunner {
            async fn run(
                &self,
                _program: &str,
                args: &[&str],
            ) -> Result<CommandOutput, CommandError> {
                // The remote rebuild command embeds `.#<hostname>` via the
                // flake_ref; the SSH target hostname appears as one of the args.
                // Record the first arg that looks like a known node.
                for arg in args {
                    if matches!(*arg, "db" | "api" | "web") {
                        self.sequence.lock().unwrap().push((*arg).to_string());
                        break;
                    }
                    if arg.contains(".#db") {
                        self.sequence.lock().unwrap().push("db".to_string());
                        break;
                    }
                    if arg.contains(".#api") {
                        self.sequence.lock().unwrap().push("api".to_string());
                        break;
                    }
                    if arg.contains(".#web") {
                        self.sequence.lock().unwrap().push("web".to_string());
                        break;
                    }
                }
                Ok(CommandOutput {
                    success: true,
                    stdout: "ok\n".to_string(),
                    stderr: String::new(),
                    exit_code: Some(0),
                })
            }
        }

        let sequence = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let runner = OrderRecordingRunner {
            sequence: std::sync::Arc::clone(&sequence),
        };

        let reg = dependency_test_registry();
        let mut orch = FleetOrchestrator::with_runner(reg, Box::new(runner));

        let result = orch
            .deploy_with_order(
                "@infra",
                DeployStrategy::Rolling,
                DeployOrder::Dependency,
                None,
            )
            .await
            .unwrap();

        assert_eq!(result.total_nodes, 3);
        assert_eq!(result.succeeded, 3);

        // The recorded result-order in DeployResult.results matches deploy order
        // for the rolling executor. Check that, too.
        let result_order: Vec<&str> =
            result.results.iter().map(|r| r.hostname.as_str()).collect();
        assert_eq!(result_order, vec!["db", "api", "web"]);

        let recorded = sequence.lock().unwrap().clone();
        assert_eq!(recorded, vec!["db", "api", "web"]);
    }

    #[tokio::test]
    async fn deploy_with_order_alphabetical_matches_default() {
        // The Alphabetical order should match the historical default behavior.
        let reg = dependency_test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        let result = orch
            .deploy_with_order(
                "@infra",
                DeployStrategy::Rolling,
                DeployOrder::Alphabetical,
                None,
            )
            .await
            .unwrap();

        let names: Vec<&str> = result.results.iter().map(|r| r.hostname.as_str()).collect();
        // BTreeMap iteration order: api, db, web
        assert_eq!(names, vec!["api", "db", "web"]);
    }

    #[tokio::test]
    async fn deploy_with_order_dependency_cycle_returns_error() {
        let mut reg = NodeRegistry::new();
        reg.add(
            Node::new("a", ".#a")
                .with_groups(vec!["loop".to_string()])
                .with_depends_on(vec!["b".to_string()]),
        );
        reg.add(
            Node::new("b", ".#b")
                .with_groups(vec!["loop".to_string()])
                .with_depends_on(vec!["a".to_string()]),
        );

        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        let err = orch
            .deploy_with_order(
                "@loop",
                DeployStrategy::Rolling,
                DeployOrder::Dependency,
                None,
            )
            .await
            .expect_err("dependency cycle should surface FleetError::Cycle");
        match err {
            FleetError::Cycle { nodes } => {
                assert_eq!(nodes, vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    // ── FleetError::Cycle Display ─────────────────────────────

    #[test]
    fn fleet_error_cycle_display() {
        let e = FleetError::Cycle {
            nodes: vec!["a".to_string(), "b".to_string()],
        };
        let msg = e.to_string();
        assert!(msg.contains("cycle"));
        assert!(msg.contains("a"));
        assert!(msg.contains("b"));
    }

    // ── FleetError from io::Error ─────────────────────────────

    #[test]
    fn fleet_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let fleet_err: FleetError = io_err.into();
        assert!(fleet_err.to_string().contains("nope"));
    }

    // ── Single-node deploy DeployFailed wiring ────────────────

    #[tokio::test]
    async fn single_node_deploy_success_returns_ok() {
        // Happy path: single-node deploy that succeeds still returns Ok.
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::succeeding()),
        );

        let result = orch
            .deploy("alpha", DeployStrategy::Rolling, None)
            .await
            .expect("successful single-node deploy should be Ok");
        assert_eq!(result.total_nodes, 1);
        assert_eq!(result.succeeded, 1);
        assert_eq!(result.failed, 0);
    }

    #[tokio::test]
    async fn single_node_deploy_failure_returns_deploy_failed() {
        // Error path: single-node deploy failure surfaces DeployFailed with
        // the failing hostname and the captured log message.
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::failing()),
        );

        let err = orch
            .deploy("beta", DeployStrategy::Parallel, None)
            .await
            .expect_err("single-node failure should be DeployFailed");

        match err {
            FleetError::DeployFailed { hostname, message } => {
                assert_eq!(hostname, "beta");
                assert!(message.contains("build failed"));
            }
            other => panic!("expected DeployFailed, got {other:?}"),
        }

        // Status was still flipped before the error returned.
        assert_eq!(
            orch.registry().get("beta").unwrap().status,
            NodeStatus::Failed
        );
    }

    #[tokio::test]
    async fn multi_node_all_fail_still_returns_ok() {
        // Edge case: multi-node deploys never surface DeployFailed even when
        // every node fails — callers inspect succeeded/failed in the result.
        let reg = test_registry();
        let mut orch = FleetOrchestrator::with_runner(
            reg,
            Box::new(MockCommandRunner::failing()),
        );

        let result = orch
            .deploy("@prod", DeployStrategy::Rolling, None)
            .await
            .expect("multi-node deploy should always return Ok");
        assert_eq!(result.total_nodes, 2);
        assert_eq!(result.succeeded, 0);
        assert_eq!(result.failed, 2);
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
