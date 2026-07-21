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
//! **File-capable eval (slice 3)** adds path literals ([`path`] mirrors of
//! the walker's `canon_abs`/`normalize`/`resolve_import`), `import` through
//! the lower-once [`file_eval`] program cache (parse + lower ONCE per
//! canonical path, typed circular-import detection), and the [`builtins`]
//! bridge — gated by `tests/eval_differential.rs` + `tests/file_differential.rs`.
//!
//! **Pure builtin surface + search paths (slice 4)** completes the [`builtins`]
//! bridge (the whole PURE builtin set natively on `IrValue`; only store-/IO-/
//! derivation-/flake-bound names stay typed missing-builtin gaps) and resolves
//! `<name>` search paths through NIX_PATH (a hit is a `Path`, a miss a
//! catchable `Throw`, plus `builtins.nixPath` / `findFile`). Per-builtin value
//! + error differential rows and a builtin-application property generator gate
//! the surface; the NIX_PATH resolver is parity-tested in `tests/path_parity.rs`.
//!
//! # Id discipline
//!
//! Ids are assigned post-order: every child id is strictly less than its
//! parent's, and `root` is always the last entry. `exprs` is therefore a
//! topologically-sorted flat vector — a forward scan visits children before
//! parents, which the later precompute passes (needed-bindings, free-var
//! sets, attrset shapes) rely on.

pub mod builtins;
pub mod eval_ir;
pub mod file_eval;
pub mod ir;
pub mod lower;
pub mod path;
pub mod render;

pub use file_eval::{
    clear_file_caches, clear_import_value_cache, eval_ir_file, program_cache_len,
};
pub use ir::{
    AttrName, BinOp, Binding, ExprId, Ir, Param, PathKind, PathPart, PatternEntry, Program,
    Span, StrPart, UnaryOp,
};
pub use lower::{lower, lower_file, LowerError};
