//! Nix builder with sandboxed execution.
//!
//! Supports Linux namespaces and macOS sandbox-exec.

pub mod reference_scan;
pub mod sandbox;
pub mod traits;

pub use traits::{BuildError, BuildLog, BuildResult, BuildState, Builder};
