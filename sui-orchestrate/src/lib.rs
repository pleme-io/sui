//! System and fleet orchestration for Nix deployments.
//!
//! Replaces darwin-rebuild, nixos-rebuild, deploy-rs, and colmena.

pub mod command;
pub mod fleet;
pub mod node;
/// The continuous system-convergence loop — the Viggy Method applied to this
/// node's own OS state: the node is kept **always rebuilt into place**,
/// streaming the atomic profile-swap link change whenever its declared flake
/// drifts from what is activated. See [`reconcile`] for the full seven-beat.
pub mod reconcile;
pub mod system;

pub use command::{CommandError, CommandOutput, CommandRunner, TokioCommandRunner};
pub use reconcile::driver::{ReconcileDriver, Trigger, shutdown_signal};
pub use reconcile::env::{LocalReconcileEnv, ReconcileEnvironment, ReconcileError};
pub use reconcile::{
    Controller, ReconcileConfig, ReconcileOutcome, ReconcileResult, ReconcileVerdict,
    SystemInPlace, SystemReconciler,
};
pub use fleet::{
    CanaryExecutor, DeployExecutor, DeployOrder, DeployResult, DeployStrategy, FleetError,
    FleetOrchestrator, NodeDeployResult, ParallelExecutor, RollingExecutor, topo_sort,
};
pub use node::{Node, NodeError, NodeRegistry, NodeStatus, StatusCounts};
pub use system::{
    GenerationInfo, Platform, RebuildAction, RebuildResult, SubstituterConfig, SwitchPlan,
    SystemError, SystemOrchestrator, build_caches, classify_activate_user, get_substituters,
    realize_bound, realize_drv, running_as_root,
};
