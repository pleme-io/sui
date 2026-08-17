//! The VM's normalized differential render — the third member of a
//! format-locked family.
//!
//! # Why this is not `to_string_keyed` + `string_keyed_to_json`
//!
//! Two renders for a VM value already existed and **neither can be used for a
//! differential**:
//!
//! - [`VMValue::to_string_keyed`](crate::VMValue::to_string_keyed) maps an
//!   *unforced* thunk to `StringKeyedValue::Lambda`. A value the VM failed to
//!   force therefore renders exactly like a value that legitimately is a
//!   function — and on the other side of the comparison the tree-walker's real
//!   lambda renders `<<lambda>>` too. The two compare **equal**, so a fixture
//!   the VM could not evaluate reports agreement.
//! - `string_keyed_to_json` (private, in the `sui` binary at `src/main.rs`)
//!   renders a thunk as the literal string `"<thunk>"` and has neither CppNix's
//!   `outPath`/`__toString` rule nor any forcing. It disagrees with the
//!   walker's `Value::to_json` on both counts. It is **not** lifted here — see
//!   the divergence note below.
//!
//! # What this does instead
//!
//! [`render_vm`] emits the byte-identical form of
//! [`sui_eval::render::render_tree`] — CppNix float format, attrs sorted by
//! **resolved name**, the walker's string escaping, raw (unquoted) paths, and
//! the same 128-deep `<...>` cap — with one deliberate difference:
//!
//! **A residual thunk is an `Err`, never a placeholder.**
//!
//! [`VM::execute`](crate::VM::execute) already deep-forces its result, so a
//! `Thunk` still standing at render time means forcing genuinely did not
//! happen. Rendering that as `<<lambda>>` (what `to_string_keyed` does) or as
//! `"<thunk>"` (what the binary's helper does) would let it compare equal to
//! the other engine's placeholder. Refusing is the only rendering that cannot
//! launder a non-answer into an agreement.
//!
//! # The residual placeholder, stated rather than hidden
//!
//! `<<lambda>>` and `<<builtin n>>` are still emitted for values that really
//! **are** functions, exactly as `render_tree` does for the walker. Two engines
//! that both produce a function agree on the render without agreeing on the
//! body. That is a property of the walker's own differential render, not
//! something introduced here, and it is why `lang_corpus_vm.rs` additionally
//! pins that **no** agreeing corpus row contains a placeholder at all.
//!
//! # Divergence recorded, not papered over
//!
//! The binary's `string_keyed_to_json` and the walker's `Value::to_json`
//! disagree: the walker implements CppNix's rule that an attrset carrying
//! `outPath` / `__toString` serializes as that string, and it forces thunks;
//! the binary's helper does neither. This module sides with **neither** — it is
//! not a JSON renderer at all, it is the walker's *differential* render, which
//! is a third form that predates both and is the one already used to compare
//! two engines (`sui-ir/tests/common/render.rs`). The `outPath` rule is
//! therefore **out of scope here and still unreconciled between those two JSON
//! paths**; nothing in this file makes that better or worse.

use std::collections::BTreeMap;

use crate::intern::Interner;
use crate::value::{ThunkState, VMValue};

/// Render-recursion depth cap — must equal `sui_eval::render::MAX_RENDER_DEPTH`
/// and `sui_ir::render::MAX_RENDER_DEPTH`.
pub const MAX_RENDER_DEPTH: usize = 128;

/// The identical marker every engine emits past [`MAX_RENDER_DEPTH`].
pub const DEEP_SENTINEL: &str = "<...>";

/// The walker's `Display` string escaping — must equal
/// `sui_eval::render::escape_str`.
#[must_use]
pub fn escape_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render a [`VMValue`] to the normalized differential form.
///
/// # Errors
///
/// - A thunk that survived [`VM::execute`](crate::VM::execute)'s deep-force.
///   Deliberately an error and not a placeholder: see the module docs.
/// - An interner that resolves two distinct symbols to the same name, which
///   would otherwise silently drop an attribute during the sort.
pub fn render_vm(v: &VMValue, interner: &Interner) -> Result<String, String> {
    render_at(v, interner, 0)
}

fn render_at(v: &VMValue, interner: &Interner, depth: usize) -> Result<String, String> {
    if depth >= MAX_RENDER_DEPTH {
        return Ok(DEEP_SENTINEL.to_string());
    }
    Ok(match v {
        VMValue::Null => "null".to_string(),
        VMValue::Bool(b) => b.to_string(),
        VMValue::Int(n) => n.to_string(),
        VMValue::Float(f) => sui_compat::versions::cppnix_format_float(*f),
        VMValue::String(s) => {
            let mut out = String::from("\"");
            out.push_str(&escape_str(s));
            out.push('"');
            out
        }
        VMValue::Path(p) => p.clone(),
        VMValue::List(items) => {
            let mut out = String::from("[ ");
            for item in items {
                out.push_str(&render_at(item, interner, depth + 1)?);
                out.push(' ');
            }
            out.push(']');
            out
        }
        VMValue::Attrs(attrs) => {
            // The VM keys attrsets by `Symbol`, whose `Ord` is *interning
            // order* — the order names were first seen, which varies with the
            // program text. The walker sorts by NAME. Iterating the VM's
            // `BTreeMap<Symbol, _>` directly would therefore emit a different
            // key order for the same attrset and report a divergence on every
            // multi-key set in the corpus: ~all of it, none of it real.
            let mut by_name: BTreeMap<String, &VMValue> = BTreeMap::new();
            for (sym, val) in attrs {
                by_name.insert(interner.resolve(*sym).to_string(), val);
            }
            // Anti-vacuity for the re-key: a `BTreeMap` insert on a duplicate
            // name silently DROPS an attribute, so a broken interner would
            // shrink the set and still render cleanly. Two symbols resolving to
            // one name is an interner bug; refuse rather than render fewer
            // attrs than the VM produced.
            if by_name.len() != attrs.len() {
                return Err(format!(
                    "interner resolved {} symbols to {} distinct names — an \
                     attribute would be silently dropped by the name sort",
                    attrs.len(),
                    by_name.len()
                ));
            }
            let mut out = String::from("{ ");
            for (k, val) in by_name {
                out.push_str(&k);
                out.push_str(" = ");
                out.push_str(&render_at(val, interner, depth + 1)?);
                out.push_str("; ");
            }
            out.push('}');
            out
        }
        VMValue::Closure(_) => "<<lambda>>".to_string(),
        VMValue::Builtin(b) => {
            let mut out = String::from("<<builtin ");
            out.push_str(b.name);
            out.push_str(">>");
            out
        }
        VMValue::HigherOrderBuiltin(h) => format!("<<builtin {:?}>>", h.op),
        VMValue::Thunk(t) => {
            // Read the memoized value without consuming it: `Cell::take` leaves
            // `None` behind, so the state must be put back or the thunk is
            // corrupted for the next reader.
            let state = t.state.take();
            let done = match &state {
                Some(ThunkState::Done(inner)) => Some(inner.clone()),
                _ => None,
            };
            t.state.set(state);
            match done {
                Some(inner) => render_at(&inner, interner, depth + 1)?,
                // THE ANTI-PLACEHOLDER RULE. `VM::execute` deep-forces before
                // returning, so an unforced thunk here is a real failure to
                // evaluate. `to_string_keyed` would render it `<<lambda>>` and
                // it would compare EQUAL to the walker's lambda; the binary's
                // JSON helper would render `"<thunk>"` and it would compare
                // equal to another `"<thunk>"`. Both launder a non-answer into
                // agreement. An error cannot.
                None => {
                    return Err(
                        "unforced thunk survived VM deep-force — refusing to render a \
                         placeholder, because a placeholder compares EQUAL to the other \
                         engine's placeholder and reports agreement where neither engine \
                         produced a value"
                            .to_string(),
                    );
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::VMThunk;

    fn interner() -> Interner {
        Interner::new()
    }

    #[test]
    fn scalars_match_the_walkers_forms() {
        let i = interner();
        assert_eq!(render_vm(&VMValue::Null, &i).unwrap(), "null");
        assert_eq!(render_vm(&VMValue::Bool(true), &i).unwrap(), "true");
        assert_eq!(render_vm(&VMValue::Int(-3), &i).unwrap(), "-3");
        assert_eq!(
            render_vm(&VMValue::String("a\"b\\c".into()), &i).unwrap(),
            "\"a\\\"b\\\\c\""
        );
        assert_eq!(render_vm(&VMValue::Path("/x/y".into()), &i).unwrap(), "/x/y");
    }

    #[test]
    fn attrs_sort_by_resolved_name_not_symbol_id() {
        // Intern in an order that makes symbol-id order DISAGREE with name
        // order: "zzz" first, so its Symbol sorts before "aaa"'s.
        let mut i = interner();
        let z = i.intern("zzz");
        let a = i.intern("aaa");
        let mut attrs = BTreeMap::new();
        attrs.insert(z, VMValue::Int(1));
        attrs.insert(a, VMValue::Int(2));
        // If this rendered in Symbol order it would read `{ zzz = 1; aaa = 2; }`
        // and every multi-key corpus fixture would falsely diverge.
        assert_eq!(
            render_vm(&VMValue::Attrs(attrs), &i).unwrap(),
            "{ aaa = 2; zzz = 1; }"
        );
    }

    #[test]
    fn an_unforced_thunk_is_an_error_not_a_placeholder() {
        // The load-bearing test of this module. A pending thunk must NOT
        // render as `<<lambda>>` / `"<thunk>"` — those compare equal to the
        // other engine's placeholder.
        let i = interner();
        let t = VMThunk::new(std::rc::Rc::new(crate::chunk::Chunk::new()), Vec::new());
        let err = render_vm(&VMValue::Thunk(t), &i).unwrap_err();
        assert!(
            err.contains("unforced thunk"),
            "expected a refusal, got: {err}"
        );
    }

    #[test]
    fn a_forced_thunk_renders_its_value() {
        let i = interner();
        let t = VMThunk::new_done(VMValue::Int(7));
        assert_eq!(render_vm(&VMValue::Thunk(t), &i).unwrap(), "7");
    }

    #[test]
    fn the_depth_cap_matches_the_other_engines() {
        // A mismatched cap would make deep values diverge for a reason that is
        // purely about rendering.
        assert_eq!(MAX_RENDER_DEPTH, sui_eval_render_depth_pin());
        assert_eq!(DEEP_SENTINEL, "<...>");
    }

    /// The walker's constant, restated. `sui-eval` is only a dev-dependency
    /// here (see `Cargo.toml`'s publish-cycle note), so this cannot read
    /// `sui_eval::render::MAX_RENDER_DEPTH` from library code; the corpus test,
    /// which does have `sui-eval`, asserts the two are equal for real.
    fn sui_eval_render_depth_pin() -> usize {
        128
    }
}
