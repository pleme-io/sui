//! ONE normalized textual render, two engines — shared by the expression
//! differential (`eval_differential.rs`) and the file differential
//! (`file_differential.rs`). Identical leaf formatting on both sides:
//! CppNix float format, sorted attrs, the walker's string escaping, raw
//! (unquoted) paths.

use sui_ir::eval_ir::{IrEvalError, IrValue};

pub fn escape_str(s: &str) -> String {
    // Byte-identical to the walker's Display escaping.
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render a TREE-WALKER value: deep-forcing, error-propagating (unlike the
/// walker's `Display`, which swallows a failed force as `<<thunk:error>>` —
/// the differential must see errors as errors).
pub fn render_tree(v: &sui_eval::Value) -> Result<String, String> {
    use sui_eval::value::Concrete;
    let c = sui_eval::eval::force_concrete(v).map_err(|e| e.to_string())?;
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
                out.push_str(&render_tree(item)?);
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
                out.push_str(&render_tree(v)?);
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

/// Render an IR value through the SAME normalized form.
pub fn render_ir_value(v: &IrValue) -> Result<String, IrEvalError> {
    let f = v.force()?;
    Ok(match f {
        IrValue::Null => "null".to_string(),
        IrValue::Bool(b) => b.to_string(),
        IrValue::Int(n) => n.to_string(),
        IrValue::Float(x) => sui_compat::versions::cppnix_format_float(x),
        IrValue::Str(s) => {
            let mut out = String::from("\"");
            out.push_str(&escape_str(&s));
            out.push('"');
            out
        }
        IrValue::Path(p) => (*p).to_string(),
        IrValue::List(items) => {
            let mut out = String::from("[ ");
            for item in items.iter() {
                out.push_str(&render_ir_value(item)?);
                out.push(' ');
            }
            out.push(']');
            out
        }
        IrValue::Attrs(attrs) => {
            let mut out = String::from("{ ");
            for (k, v) in attrs.iter() {
                out.push_str(k);
                out.push_str(" = ");
                out.push_str(&render_ir_value(v)?);
                out.push_str("; ");
            }
            out.push('}');
            out
        }
        IrValue::Lambda(_) => "<<lambda>>".to_string(),
        IrValue::Builtin(kind, captured) => {
            let mut out = String::from("<<builtin ");
            out.push_str(kind.display_name(captured.len()));
            out.push_str(">>");
            out
        }
        IrValue::Thunk(_) => unreachable!("force() returned a thunk"),
    })
}
