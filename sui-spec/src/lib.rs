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
//!
//! More domains will land here as we identify them.  Rule of thumb:
//! if the body of a function is "here is what CppNix does", that
//! function belongs in this crate as a spec + interpreter, not in
//! the engine.

pub mod derivation;
pub mod flake;
pub mod loader;
pub mod error;
pub mod probe;

pub use error::SpecError;
