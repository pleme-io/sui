//! System and fleet orchestration for Nix deployments.
//!
//! Replaces darwin-rebuild, nixos-rebuild, deploy-rs, and colmena.

pub mod command;
pub mod fleet;
pub mod node;
pub mod system;

pub use command::{CommandError, CommandOutput, CommandRunner, TokioCommandRunner};
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
