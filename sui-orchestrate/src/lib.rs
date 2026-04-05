//! System and fleet orchestration for Nix deployments.
//!
//! Replaces darwin-rebuild, nixos-rebuild, deploy-rs, and colmena.

pub mod fleet;
pub mod node;
pub mod system;

pub use fleet::{DeployStrategy, FleetOrchestrator};
pub use node::{Node, NodeStatus};
pub use system::{RebuildAction, RebuildResult, SystemOrchestrator};
