//! Nix builder with sandboxed execution.
//!
//! This crate provides:
//!
//! - [`Builder`] — async trait for running derivation builds
//! - [`BuildState`] — state machine tracking build lifecycle
//! - [`BuildLog`] — structured log accumulator
//! - [`sandbox`] — platform-specific sandbox abstractions
//! - [`reference_scan`] — Aho-Corasick based store path reference detection

pub mod reference_scan;
pub mod sandbox;
pub mod traits;

pub use traits::{BuildError, BuildLog, BuildResult, BuildState, Builder};
