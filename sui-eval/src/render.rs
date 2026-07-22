//! The tree-walker's normalized differential render.
//!
//! Renders an evaluated [`Value`](crate::Value) to the ONE normalized textual
//! form the sui↔sui differential (`sui-ir/tests/eval_differential.rs`) and the
//! `SUI_IR` shadow-eval latch (`sui eval`) byte-compare against `eval_ir`'s
//! `sui_ir::render::render_ir_value`: CppNix float format, sorted attrs, the
//! walker's string escaping, raw (unquoted) paths, and a shared depth cap so an
//! infinitely-deep value renders byte-identically on both engines (a match,
//! never a stack overflow). Deep-forcing + error-propagating (unlike `Display`,
//! which swallows a failed force — the differential must see errors as errors).
//!
//! Promoted from the test-only `sui-ir/tests/common/render.rs` into shippable
//! code so the walker's authoritative result can be compared to a shadow
//! `eval_ir` result live. The cap + escaping MUST match
//! `sui_ir::render::{MAX_RENDER_DEPTH, DEEP_SENTINEL, escape_str}`.

use crate::value::Concrete;
use crate::Value;

/// Render-recursion depth cap — must equal `sui_ir::render::MAX_RENDER_DEPTH`.
pub const MAX_RENDER_DEPTH: usize = 128;

/// The identical marker both engines emit past [`MAX_RENDER_DEPTH`].
pub const DEEP_SENTINEL: &str = "<...>";

/// The walker's `Display` string escaping.
#[must_use]
pub fn escape_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render a tree-walker [`Value`] to the normalized differential form.
///
/// # Errors
///
/// Propagates any force error as its `Display` string (the differential must
/// see errors as errors, not swallow them into `<<thunk:error>>`).
pub fn render_tree(v: &Value) -> Result<String, String> {
    render_tree_at(v, 0)
}

fn render_tree_at(v: &Value, depth: usize) -> Result<String, String> {
    if depth >= MAX_RENDER_DEPTH {
        return Ok(DEEP_SENTINEL.to_string());
    }
    let c = crate::eval::force_concrete(v).map_err(|e| e.to_string())?;
    Ok(match c {
        Concrete::Null => "null".to_string(),
        Concrete::Bool(b) => b.to_string(),
        Concrete::Int(n) => n.to_string(),
        Concrete::Float(f) => sui_compat::versions::cppnix_format_float(f),
        Concrete::String(s) => {
            let mut out = String::from("\"");
            out.push_str(&escape_str(&s));
            out.push('"');
            out
        }
        Concrete::Path(p) => p.to_string(),
        Concrete::List(items) => {
            let mut out = String::from("[ ");
            for item in items.iter() {
                out.push_str(&render_tree_at(item, depth + 1)?);
                out.push(' ');
            }
            out.push(']');
            out
        }
        Concrete::Attrs(attrs) => {
            let mut out = String::from("{ ");
            for (k, v) in attrs.iter() {
                out.push_str(&k);
                out.push_str(" = ");
                out.push_str(&render_tree_at(v, depth + 1)?);
                out.push_str("; ");
            }
            out.push('}');
            out
        }
        Concrete::Lambda(_) => "<<lambda>>".to_string(),
        Concrete::Builtin(b) => {
            let mut out = String::from("<<builtin ");
            out.push_str(b.name);
            out.push_str(">>");
            out
        }
    })
}
