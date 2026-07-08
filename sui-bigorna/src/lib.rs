//! `sui-bigorna` — the typed buildx-native-multi-arch primitive.
//!
//! *bigorna* (Brazilian-Portuguese: **anvil** — the surface each architecture
//! is natively hammered on). It sets up a `docker buildx` builder whose nodes
//! are real, native per-architecture build nodes, so an **unchanged**
//! `docker buildx build --platform linux/amd64,linux/arm64` dispatches each
//! platform to native metal instead of falling back to **QEMU** emulation. The
//! same builder config carries the layer cache (`--cache-from` / `--cache-to`),
//! so native-multi-arch **and** content-addressed caching land through **one**
//! integration point.
//!
//! ## What it reuses (does not rebuild)
//!
//! - The [`CommandRunner`] seam + [`DockerBuildInvocation`] from
//!   `sui-dockerfile-wrapper` — every `docker buildx …` argv is a typed
//!   invocation run through the same mockable side-effect seam, so the whole
//!   primitive is provable without ever spawning `docker` in a unit test.
//! - `sui_cache::config::BackendConfig` — a bigorna cache endpoint is bridged
//!   straight from a shipped sui storage-backend config
//!   ([`cache_front::CacheEndpoint::from_backend_config`]), so the buildx cache
//!   front points at the shipped sui tiered content-addressed store.
//!
//! ## What it owns (the net-new delta)
//!
//! - [`node::NativeArchNode`] — the guard that makes an *emulated* node
//!   unrepresentable: a node whose host arch differs from its target arch has no
//!   constructor, so a [`builder::BuilderTopology`] is native by construction
//!   ("no QEMU" is a property of the type, not a runtime probe).
//! - [`builder::BuilderTopology`] — the `docker buildx create --append` argv
//!   that registers native per-arch nodes as buildx nodes.
//! - [`cache_front::CacheFront`] — the `BuildKitCacheFront` on the same builder.
//!
//! ## The keyway surface
//!
//! [`BigornaConfig`] (YAML/JSON in) → [`plan`] / [`setup`] / [`inspect`] /
//! [`teardown`] / [`build`] → a typed JSON receipt out. [`plan`] is
//! shadow-first: it renders the exact argv it *would* run, mutating nothing.

pub mod builder;
pub mod cache_front;
pub mod inspect;
pub mod node;
pub mod platform;

use serde::{Deserialize, Serialize};
use std::time::Instant;

use sui_dockerfile_wrapper::{CommandOutcome, CommandRunError, CommandRunner, DockerBuildInvocation};

pub use builder::{
    BuildOutput, BuildSpec, BuilderTopology, EmulationVerdict, TopologyError,
};
pub use cache_front::{CacheEndpoint, CacheFront, CacheFrontError, CacheMode};
pub use inspect::{InspectReport, InspectedNode};
pub use node::{BuildxDriver, DriverOpt, NativeArchNode, NodeError, NodeSpec};
pub use platform::{Arch, Os, Platform, PlatformList, PlatformParseError};

fn default_true() -> bool {
    true
}

/// The keyway "config in" half: a builder topology described as typed data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BigornaConfig {
    /// The buildx builder name (`bigorna`).
    pub builder_name: String,
    /// The buildx driver the builder is created with.
    #[serde(default)]
    pub driver: BuildxDriver,
    /// The native per-arch nodes to register.
    pub nodes: Vec<NodeSpec>,
    /// `--bootstrap` the builder on create (start `BuildKit` immediately).
    /// Defaults to `true` so the builder is usable the moment setup returns.
    #[serde(default = "default_true")]
    pub bootstrap: bool,
    /// `--use` the builder as the current one, so an unchanged
    /// `docker buildx build` picks it up. Defaults to `true`.
    #[serde(default = "default_true")]
    pub use_as_default: bool,
}

impl BigornaConfig {
    /// Validate into a native [`BuilderTopology`].
    ///
    /// # Errors
    ///
    /// Propagates [`TopologyError`] (no nodes, or an emulated node).
    pub fn into_topology(self) -> Result<BuilderTopology, TopologyError> {
        BuilderTopology::from_specs(
            self.builder_name,
            self.driver,
            self.nodes,
            self.bootstrap,
            self.use_as_default,
        )
    }
}

/// The result of running one `docker buildx` invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepResult {
    /// A short label for the step (`create`, `append`).
    pub verb: String,
    /// The full argv that ran (program + args), for auditability.
    pub argv: Vec<String>,
    pub success: bool,
    pub exit_code: Option<i32>,
    /// The tail of stderr on failure (bounded); empty on success.
    pub stderr_tail: String,
}

/// The shadow-first plan: exactly the argv [`setup`] would run, mutating
/// nothing. This is the safe surface for CI gates + review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReceipt {
    pub builder: String,
    pub driver: String,
    pub covered_platforms: Vec<Platform>,
    pub emulation: EmulationVerdict,
    /// Each step's full argv, in order.
    pub invocations: Vec<Vec<String>>,
}

/// The receipt from [`setup`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupReceipt {
    pub builder: String,
    pub driver: String,
    pub covered_platforms: Vec<Platform>,
    /// The emulation verdict for the topology's own covered platforms —
    /// `NativeForAll` by construction (every node is native).
    pub emulation: EmulationVerdict,
    pub steps: Vec<StepResult>,
    /// Whether every create step succeeded.
    pub ok: bool,
}

/// The outcome of a `docker buildx build`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BuildOutcome {
    Ok,
    Failed { exit_code: Option<i32>, stderr_tail: String },
}

/// The receipt from [`build`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildReceipt {
    pub argv: Vec<String>,
    pub outcome: BuildOutcome,
    pub duration_ms: u128,
}

/// Errors from the bigorna driver surface.
#[derive(Debug, thiserror::Error)]
pub enum BigornaError {
    #[error(transparent)]
    Topology(#[from] TopologyError),
    #[error(transparent)]
    CacheFront(#[from] CacheFrontError),
    #[error("spawning docker: {0}")]
    Command(#[from] CommandRunError),
    #[error("`docker buildx {verb}` exited {exit_code:?}: {stderr_tail}")]
    BuildxFailed { verb: String, exit_code: Option<i32>, stderr_tail: String },
}

fn full_argv(inv: &DockerBuildInvocation) -> Vec<String> {
    let mut v = Vec::with_capacity(inv.args.len() + 1);
    v.push(inv.program.clone());
    v.extend(inv.args.iter().cloned());
    v
}

fn step_of(verb: &str, inv: &DockerBuildInvocation, outcome: &CommandOutcome) -> StepResult {
    StepResult {
        verb: verb.to_string(),
        argv: full_argv(inv),
        success: outcome.success,
        exit_code: outcome.exit_code,
        stderr_tail: if outcome.success { String::new() } else { outcome.stderr_tail(4096) },
    }
}

/// Render the shadow-first plan for a config: the argv [`setup`] would run,
/// without running any of it.
///
/// # Errors
///
/// Propagates [`TopologyError`] (no nodes, or an emulated node).
pub fn plan(config: BigornaConfig) -> Result<PlanReceipt, BigornaError> {
    let topology = config.into_topology()?;
    let covered = topology.covered_platforms();
    let emulation = topology.emulation_free_for(&covered);
    let invocations = topology.create_invocations().iter().map(full_argv).collect();
    Ok(PlanReceipt {
        builder: topology.name().to_string(),
        driver: driver_label(&topology),
        covered_platforms: covered,
        emulation,
        invocations,
    })
}

/// Set up the builder: run the `docker buildx create --append` sequence through
/// the runner, stopping at the first failed step (a later `--append` to a
/// builder that never got created would only error). Never panics — a non-zero
/// buildx exit is a `success: false` step, not an error; only a spawn failure
/// is an `Err`.
///
/// # Errors
///
/// Returns [`BigornaError::Topology`] if the config doesn't validate, or
/// [`BigornaError::Command`] if `docker` could not be spawned at all.
pub fn setup<R: CommandRunner>(
    config: BigornaConfig,
    runner: &R,
) -> Result<SetupReceipt, BigornaError> {
    let topology = config.into_topology()?;
    let covered = topology.covered_platforms();
    let emulation = topology.emulation_free_for(&covered);
    let driver = driver_label(&topology);

    let mut steps = Vec::new();
    let mut ok = true;
    for (i, inv) in topology.create_invocations().iter().enumerate() {
        let verb = if i == 0 { "create" } else { "append" };
        let outcome = runner.run(inv)?;
        let step = step_of(verb, inv, &outcome);
        let succeeded = step.success;
        steps.push(step);
        if !succeeded {
            ok = false;
            break;
        }
    }

    Ok(SetupReceipt {
        builder: topology.name().to_string(),
        driver,
        covered_platforms: covered,
        emulation,
        steps,
        ok,
    })
}

/// Inspect a builder by name, parsing the real `docker buildx inspect` output
/// into a typed [`InspectReport`].
///
/// # Errors
///
/// [`BigornaError::Command`] if `docker` can't be spawned, or
/// [`BigornaError::BuildxFailed`] if inspect exits non-zero (e.g. no such
/// builder).
pub fn inspect<R: CommandRunner>(
    builder: &str,
    bootstrap: bool,
    runner: &R,
) -> Result<InspectReport, BigornaError> {
    let inv = builder::inspect_invocation(builder, bootstrap);
    let outcome = runner.run(&inv)?;
    if !outcome.success {
        return Err(BigornaError::BuildxFailed {
            verb: "inspect".to_string(),
            exit_code: outcome.exit_code,
            stderr_tail: outcome.stderr_tail(4096),
        });
    }
    Ok(InspectReport::parse(&String::from_utf8_lossy(&outcome.stdout)))
}

/// Tear a builder down (`docker buildx rm <builder>`).
///
/// # Errors
///
/// [`BigornaError::Command`] on spawn failure.
pub fn teardown<R: CommandRunner>(builder: &str, runner: &R) -> Result<StepResult, BigornaError> {
    let inv = builder::rm_invocation(builder);
    let outcome = runner.run(&inv)?;
    Ok(step_of("rm", &inv, &outcome))
}

/// Drive a `docker buildx build` for a [`BuildSpec`]. This is the typed build
/// driver (a consumer's own unchanged `docker buildx build` works identically
/// once the builder is `--use`d). Never panics — a failed build is a typed
/// [`BuildOutcome::Failed`] receipt.
///
/// # Errors
///
/// [`BigornaError::Command`] on spawn failure.
pub fn build<R: CommandRunner>(
    spec: &BuildSpec,
    runner: &R,
) -> Result<BuildReceipt, BigornaError> {
    let inv = spec.invocation();
    let started = Instant::now();
    let outcome = runner.run(&inv)?;
    let duration_ms = started.elapsed().as_millis();
    let outcome = if outcome.success {
        BuildOutcome::Ok
    } else {
        BuildOutcome::Failed {
            exit_code: outcome.exit_code,
            stderr_tail: outcome.stderr_tail(4096),
        }
    };
    Ok(BuildReceipt { argv: full_argv(&inv), outcome, duration_ms })
}

/// The driver label a topology was built with. Recovered from the first
/// create invocation so the receipt is honest about the driver used.
fn driver_label(topology: &BuilderTopology) -> String {
    topology
        .create_invocations()
        .first()
        .and_then(|inv| {
            inv.args
                .iter()
                .position(|a| a == "--driver")
                .and_then(|i| inv.args.get(i + 1))
                .cloned()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_dockerfile_wrapper::MockCommandRunner;

    fn config() -> BigornaConfig {
        BigornaConfig {
            builder_name: "bigorna".to_string(),
            driver: BuildxDriver::DockerContainer,
            nodes: vec![
                NodeSpec {
                    name: "bigorna-amd64".to_string(),
                    platform: Platform::linux_amd64(),
                    host_arch: Arch::Amd64,
                    endpoint: Some("amd64-ctx".to_string()),
                    driver_opts: vec![],
                },
                NodeSpec {
                    name: "bigorna-arm64".to_string(),
                    platform: Platform::linux_arm64(),
                    host_arch: Arch::Arm64,
                    endpoint: Some("arm64-ctx".to_string()),
                    driver_opts: vec![],
                },
            ],
            bootstrap: true,
            use_as_default: true,
        }
    }

    #[test]
    fn plan_is_side_effect_free_and_native_for_all() {
        let receipt = plan(config()).unwrap();
        assert_eq!(receipt.builder, "bigorna");
        assert_eq!(receipt.driver, "docker-container");
        assert_eq!(receipt.invocations.len(), 2);
        assert!(receipt.emulation.is_native_for_all());
        // Two native nodes, no emulated node anywhere in the plan.
        assert_eq!(receipt.covered_platforms.len(), 2);
    }

    #[test]
    fn setup_runs_every_create_step_through_the_seam() {
        let runner = MockCommandRunner::new();
        let receipt = setup(config(), &runner).unwrap();
        assert!(receipt.ok);
        assert_eq!(receipt.steps.len(), 2);
        assert_eq!(receipt.steps[0].verb, "create");
        assert_eq!(receipt.steps[1].verb, "append");
        assert!(receipt.emulation.is_native_for_all());
        // Exactly two real buildx invocations were recorded by the mock.
        assert_eq!(runner.recorded().len(), 2);
    }

    #[test]
    fn setup_stops_at_the_first_failed_create() {
        let runner = MockCommandRunner::with_outcome(CommandOutcome {
            success: false,
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"error: builder already exists".to_vec(),
        });
        let receipt = setup(config(), &runner).unwrap();
        assert!(!receipt.ok);
        // First step failed → the append never ran.
        assert_eq!(receipt.steps.len(), 1);
        assert!(!receipt.steps[0].success);
        assert!(receipt.steps[0].stderr_tail.contains("already exists"));
        assert_eq!(runner.recorded().len(), 1);
    }

    #[test]
    fn an_emulated_node_config_never_reaches_a_runner() {
        // host arm64 targeting amd64 == QEMU node == rejected before any docker call.
        let mut cfg = config();
        cfg.nodes[0].host_arch = Arch::Arm64; // amd64 platform, arm64 host
        let runner = MockCommandRunner::new();
        let err = setup(cfg, &runner).unwrap_err();
        assert!(matches!(err, BigornaError::Topology(TopologyError::Node(NodeError::Emulated { .. }))));
        assert_eq!(runner.recorded().len(), 0, "no docker call for an emulated topology");
    }

    #[test]
    fn inspect_parses_a_running_builder_and_errors_on_missing() {
        // Success path: a real-looking inspect stdout.
        let ok_runner = MockCommandRunner::with_outcome(CommandOutcome {
            success: true,
            exit_code: Some(0),
            stdout: b"\
Name:          bigorna
Driver:        docker-container

Nodes:
Name:             bigorna-arm64
Status:           running
Platforms:        linux/arm64*
"
            .to_vec(),
            stderr: Vec::new(),
        });
        let report = inspect("bigorna", true, &ok_runner).unwrap();
        assert_eq!(report.builder, "bigorna");
        assert!(report.has_running_node_for(&Platform::linux_arm64()));

        // Missing builder: non-zero exit → typed error, never a fake report.
        let miss_runner = MockCommandRunner::with_outcome(CommandOutcome {
            success: false,
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"ERROR: no builder \"nope\" found".to_vec(),
        });
        let err = inspect("nope", false, &miss_runner).unwrap_err();
        assert!(matches!(err, BigornaError::BuildxFailed { .. }));
    }

    #[test]
    fn build_receipt_is_typed_and_never_panics_on_failure() {
        let spec = BuildSpec {
            builder: "bigorna".to_string(),
            platforms: PlatformList(vec![Platform::linux_amd64(), Platform::linux_arm64()]),
            dockerfile: std::path::PathBuf::from("Dockerfile"),
            context: std::path::PathBuf::from("."),
            tags: vec!["ghcr.io/pleme-io/example:test".to_string()],
            build_args: std::collections::BTreeMap::new(),
            cache: CacheFront::default(),
            output: BuildOutput::None,
        };
        let runner = MockCommandRunner::with_outcome(CommandOutcome {
            success: false,
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"error: failed to solve".to_vec(),
        });
        let receipt = build(&spec, &runner).unwrap();
        match receipt.outcome {
            BuildOutcome::Failed { exit_code, stderr_tail } => {
                assert_eq!(exit_code, Some(1));
                assert!(stderr_tail.contains("failed to solve"));
            }
            BuildOutcome::Ok => panic!("expected a failed build"),
        }
    }

    #[test]
    fn config_roundtrips_through_yaml() {
        let cfg = config();
        let yaml = serde_yaml_ng::to_string(&cfg).unwrap();
        let back: BigornaConfig = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(back, cfg);
    }
}
