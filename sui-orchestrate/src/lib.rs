//! System and fleet orchestration for Nix deployments.
//!
//! Replaces darwin-rebuild, nixos-rebuild, deploy-rs, and colmena.

pub mod command;
pub mod fleet;
pub mod node;
pub mod system;

pub use command::{CommandError, CommandOutput, CommandRunner, TokioCommandRunner};
pub use fleet::{
    CanaryExecutor, DeployExecutor, DeployStrategy, FleetOrchestrator, ParallelExecutor,
    RollingExecutor,
};
pub use node::{Node, NodeError, NodeStatus};
pub use system::{RebuildAction, RebuildResult, SystemOrchestrator};
