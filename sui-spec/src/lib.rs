//! sui-spec — declarative Lisp-authored specs for CppNix-parity behaviors.
//!
//! ## Why this crate exists
//!
//! Every bug we found on the road to drop-in replacement was a *spec*
//! bug, not an implementation bug.  "Unresolved form has env.out =
//! ''", "hash the final form not the unresolved one", "mask env
//! entries whose names match outputs" — these are statements about
//! CppNix behavior, and they were living inside imperative Rust
//! functions that happened to be duplicated between the tree-walker
//! (`sui-eval`) and the bytecode VM (`sui-bytecode`).  The copies
//! drifted independently.
//!
//! This crate is the cure.
//!
//! ## The pattern
//!
//! > Rust is structure and provability. Lisp is transformation,
//! > rendering, simulation, and in-flight mutation. The pair can
//! > **generate each other**: a Rust type is the typed border for
//! > a Lisp authoring surface; a Lisp spec is the free middle that
//! > renders out to any morphism (ATerm bytes, JSON attrsets, probe
//! > matrices) a substrate needs.
//!
//! Every domain below is a `#[derive(TataraDomain)]` Rust struct
//! (the hard, typed border) paired with a `.lisp` spec file (the
//! free-middle authoring surface).  Interpreters in this crate
//! consume the typed spec and emit the morphism renderings both
//! engines call.  Change the Lisp, both engines change together.
//! The tree-walker and the VM **cannot** diverge because they read
//! the same authored spec.
//!
//! ## Inventory
//!
//! - [`derivation`] — input-addressed + fixed-output derivation path
//!   algorithms.  Was: 50 lines of imperative Rust in each of two
//!   engines (4 bugs found this session).  Now: one `.lisp` spec
//!   and one interpreter.
//! - [`flake`] — top-level flake result shape policy.  Prevents the
//!   "leak `description` / `nixConfig`" class of bug.
//! - [`probe`] — single-expression cross-engine parity probes.
//!   Includes the [`probe::Probe`] type, the original
//!   `parity_probes.lisp` corpus, and the `builtin_smoke_probes.lisp`
//!   corpus (one probe per sui builtin module).
//! - [`rebuild`] — host-aware multi-stage rebuild parity probes.
//!   The typed substrate that lets `sui-sweep` (and future operator
//!   surfaces) shadow a real `fleet rebuild` end-to-end without ever
//!   mutating the system.
//! - [`parity`] — the [`parity::ParityCheck`] trait every typed
//!   domain implements, plus [`parity::ShadowReport`] / [`parity::Verdict`]
//!   / [`parity::ProbeContext`].  This is the second-site abstraction:
//!   solve once, both [`probe::Probe`] and [`rebuild::RebuildProbe`]
//!   ride on it, and the future `sui rebuild-shadow` subcommand
//!   reuses the same trait without re-authoring the sweep loop.
//! - [`exec`] — typed dual-subprocess runner.  NO SHELL.  Mandatory
//!   timeout.  Captured output as a typed struct.
//! - [`sweep`] — library entry point for the shadow-sweep loop.
//!   Both the `sui-sweep` binary and the `sui rebuild-shadow`
//!   subcommand (future) wrap [`sweep::run`].
//!
//! More domains will land here as we identify them.  Rule of thumb:
//! if the body of a function is "here is what CppNix does", that
//! function belongs in this crate as a spec + interpreter, not in
//! the engine.

pub mod activation_script;
pub mod ast_graph;
pub mod catalog;
pub mod cli;
pub mod cli_coverage;
pub mod derivation;
pub mod error;
pub mod eval_cache;
pub mod exec;
pub mod fetcher;
pub mod flake;
pub mod gc;
pub mod hash;
pub mod loader;
pub mod lock_file;
pub mod lockfile_graph;
pub mod module_compiler;
pub mod module_graph;
pub mod module_solver;
pub mod module_system;
pub mod nix_replacement_coverage;
pub mod nar;
pub mod narinfo;
pub mod operator_view;
pub mod parity;
pub mod store_analyze;
pub mod store_diff;
pub mod store_inventory;
pub mod store_ops;
pub mod store_query;
pub mod store_recipe;
pub mod store_transform;
pub mod probe;
pub mod profile;
pub mod realisation;
pub mod rebuild;
pub mod registry;
pub mod sandbox;
pub mod spec_trait;
pub mod store_layout;
pub mod style;
pub mod substituter;
pub mod sweep;
pub mod trust_model;
pub mod worker_protocol;

pub use spec_trait::{HasName, Spec};

pub use error::SpecError;
pub use parity::{ParityCheck, ProbeContext, ProbeKind, ShadowReport, Verdict};
pub use sweep::{Corpus, SweepConfig};
