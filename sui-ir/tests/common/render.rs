//! ONE normalized textual render, two engines — shared by the expression
//! differential (`eval_differential.rs`) and the file differential
//! (`file_differential.rs`).
//!
//! Both renderers are now SHIPPABLE library code (promoted here so the `SUI_IR`
//! shadow-eval latch in `sui eval` can render an `eval_ir` result live). This
//! module is a thin re-export so the differential drives the exact functions the
//! CLI does — the differential's green is the byte-neutrality proof of the lift.
//!
//! - `render_ir_value` → `sui_ir::render` (IrValue → normalized form)
//! - `render_tree`     → `sui_eval::render` (tree-walker Value → the same form)
//!
//! Both use CppNix float format, sorted attrs, the walker's string escaping, raw
//! (unquoted) paths, and a shared 128-deep cap so an infinitely-deep value
//! renders byte-identically on both engines (a match, never a stack overflow).

pub use sui_eval::render::render_tree;
pub use sui_ir::render::render_ir_value;
