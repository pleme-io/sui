//! L3 lower-once flat IR for the sui evaluator (docs/SPEED.md L3).
//!
//! Per source file, one [`Program`] `{ exprs: Vec<Ir>, spans, root }` with
//! [`ExprId`]`(u32)` indices, lowered from the rnix/rowan AST **once** by
//! [`lower`] / [`lower_file`]. Phase-1 lowering is 1:1 structural — every
//! rnix `Expr` variant maps to exactly one [`Ir`] variant (parse-error nodes
//! excepted, which return a typed [`LowerError`]), so force order is
//! untouched by construction.
//!
//! # What this slice is (and is not)
//!
//! This crate is the **IR skeleton + total lowering + the differential
//! render harness** only:
//!
//! * [`lower`] is total over the parse surface: every construct either
//!   lowers or returns a typed [`LowerError`] naming the construct. No
//!   silent gaps, no panics, no placeholder Ok values.
//! * [`render::render_ir`] and [`render::render_ast`] are two *independent*
//!   walks emitting one normalized textual form; their equality over the
//!   parity corpus + property-generated expressions proves lowering loses
//!   nothing (see `tests/differential.rs`).
//!
//! **Eval-through-IR (slice 2)** lives in [`eval_ir`]: a pure-expression-
//! subset evaluator over the flat `Program` with its own minimal mirror
//! value/env/thunk types (see that module's docs for why mirrors), gated by
//! `tests/eval_differential.rs` — every corpus/supplement/seed/generated
//! expression is evaluated on BOTH engines (tree-walker as the semantic
//! oracle) and the rendered results byte-compared, with a typed shrink-only
//! known-gap allowlist. Nothing here is wired into sui-eval; the live
//! engines are untouched.
//!
//! # Id discipline
//!
//! Ids are assigned post-order: every child id is strictly less than its
//! parent's, and `root` is always the last entry. `exprs` is therefore a
//! topologically-sorted flat vector — a forward scan visits children before
//! parents, which the later precompute passes (needed-bindings, free-var
//! sets, attrset shapes) rely on.

pub mod eval_ir;
pub mod ir;
pub mod lower;
pub mod render;

pub use ir::{
    AttrName, BinOp, Binding, ExprId, Ir, Param, PathKind, PathPart, PatternEntry, Program,
    Span, StrPart, UnaryOp,
};
pub use lower::{lower, lower_file, LowerError};
