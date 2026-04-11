//! Nix builder with sandboxed execution.
//!
//! This crate provides:
//!
//! - [`Builder`] — async trait for running derivation builds
//! - [`BuildState`] — state machine tracking build lifecycle
//! - [`BuildOutcome`] — typed build result (Success / Failure / Cancelled)
//! - [`BuildLog`] — structured log accumulator
//! - [`sandbox`] — platform-specific sandbox abstractions
//! - [`reference_scan`] — Aho-Corasick based store path reference detection
//! - [`closure`] — topological sort of derivation build closures
//! - [`local_builder`] — concrete builder executing derivations locally

pub mod closure;
pub mod convergence_builder;
pub mod local_builder;
pub mod reference_scan;
pub mod sandbox;
pub mod traits;

pub use closure::BuildClosure;
pub use convergence_builder::{ConvergenceBuilder, ConvergenceMetadata};
pub use local_builder::LocalBuilder;
pub use traits::{BuildError, BuildLog, BuildOutcome, BuildResult, BuildState, Builder};
