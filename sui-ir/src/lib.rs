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
//! **No eval-through-IR yet.** That is the next, separately-gated slice —
//! dual-engine (`--ir` behind a flag), full parity corpus byte-diffed on
//! both engines, before any flip. Nothing in this crate is wired into
//! sui-eval; the live engines are untouched.
//!
//! # Id discipline
//!
//! Ids are assigned post-order: every child id is strictly less than its
//! parent's, and `root` is always the last entry. `exprs` is therefore a
//! topologically-sorted flat vector — a forward scan visits children before
//! parents, which the later precompute passes (needed-bindings, free-var
//! sets, attrset shapes) rely on.

pub mod ir;
pub mod lower;
pub mod render;

pub use ir::{
    AttrName, BinOp, Binding, ExprId, Ir, Param, PathKind, PathPart, PatternEntry, Program,
    Span, StrPart, UnaryOp,
};
pub use lower::{lower, lower_file, LowerError};
