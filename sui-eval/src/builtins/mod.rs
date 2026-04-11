//! Core Nix builtins.
//!
//! Builtins are registered in [`register`] which populates the global
//! `builtins` attribute set and the top-level default scope. Simple
//! single-argument builtins (type-checking predicates, `ceil`, `floor`,
//! etc.) are described declaratively via [`BuiltinSpec`] and a static
//! slice, keeping registration compact and self-documenting. More
//! complex builtins (curried, multi-stage, I/O) are still registered
//! with the imperative [`register_builtin`] / [`register_curried`]
//! helpers.
//!
//! The evaluator uses Tvix-style lazy evaluation with `Rc<RefCell<ThunkRepr>>`
//! thunks.  Curried builtins capture these non-Send/Sync values in `Rc`
//! closures — this is intentional for the single-threaded evaluator and
//! safe because the evaluator is single-threaded.

use std::rc::Rc;

use crate::value::*;

// ── Sub-modules ──────────────────────────────────────────────
//
// Each sub-module registers a logical group of builtins via a
// `register(&mut NixAttrs)` function. The main `register()` below
// calls them all.
mod arithmetic;
mod attrs;
mod coerce;
mod context;
mod control;
mod convert;
mod convert_helpers;
mod derivation;
mod fetchers;
mod flake;
mod flake_eval;
mod flake_parse;
mod helpers;
mod import_cache;
mod lists;
mod misc;
mod nav;
mod paths;
mod strings;
mod sui_ext;
mod types;
mod versions;

// ── Re-exports for sub-modules (via `use super::*`) ─────────
//
// Sub-modules use `use super::*;` to pull in the types and helpers
// they need.  These re-exports make all shared items available.
pub(crate) use coerce::*;
pub(crate) use convert_helpers::*;
pub(crate) use fetchers::{
    base64_encode, fetch_git, fetch_mercurial, fetch_tree, fetch_url_bytes,
    format_unix_yyyymmddhhmmss, git_result_attrs, hex_to_bytes,
};
pub(crate) use flake_parse::{flake_ref_to_string, parse_flake_ref};
pub(crate) use helpers::*;
pub(crate) use versions::{compare_versions, parse_drv_name, split_version};

// ── Public API ──────────────────────────────────────────────
pub use derivation::build_derivation;
pub use flake_eval::{evaluate_flake, evaluate_flake_attr};
pub(crate) use flake_eval::{FLAKE_EVAL_DEPTH, MAX_FLAKE_EVAL_DEPTH};
pub use import_cache::clear_import_cache;
pub(crate) use import_cache::IMPORT_CACHE;
pub use nav::{navigate_attrs, parse_nix_path, resolve_search_path};

/// Look up a tree-walker builtin by name and call it with the given args.
///
/// This is the core dispatch for the builtin bridge: the bytecode VM calls
/// this (via the bridge callback) when it encounters a builtin it doesn't
/// implement natively.
///
/// The builtins registry is cached in a thread-local to avoid rebuilding
/// it on every bridge call.
pub fn call_builtin_by_name(name: &str, args: &[Value]) -> Result<Value, EvalError> {
    use std::cell::RefCell;

    thread_local! {
        static BUILTIN_REGISTRY: RefCell<Option<NixAttrs>> = const { RefCell::new(None) };
    }

    BUILTIN_REGISTRY.with(|reg| {
        let mut borrow = reg.borrow_mut();
        if borrow.is_none() {
            let mut attrs = NixAttrs::new();
            types::register(&mut attrs);
            arithmetic::register(&mut attrs);
            lists::register(&mut attrs);
            attrs::register(&mut attrs);
            strings::register(&mut attrs);
            convert::register(&mut attrs);
            control::register(&mut attrs);
            context::register(&mut attrs);
            paths::register(&mut attrs);
            fetchers::register(&mut attrs);
            flake::register(&mut attrs);
            derivation::register(&mut attrs);
            versions::register(&mut attrs);
            misc::register(&mut attrs);
            *borrow = Some(attrs);
        }

        let attrs = borrow.as_ref().unwrap();
        let builtin_val = attrs.get(name).ok_or_else(|| {
            EvalError::type_error(format!("bridge: unknown builtin '{name}'"))
        })?;

        match builtin_val {
            Value::Builtin(bf) => {
                // For curried builtins (arity 2+), we need to handle
                // partial application. The first call returns a partial,
                // and if we have 2 args, we apply the partial to the second.
                if args.len() == 1 {
                    (bf.func)(args)
                } else if args.len() == 2 {
                    // Apply first arg, get partial, apply second arg
                    let partial = (bf.func)(&args[..1])?;
                    match partial {
                        Value::Builtin(ref pf) => (pf.func)(&args[1..]),
                        // If first application returned a non-function,
                        // the builtin is single-arg and we have extra args
                        _ => Ok(partial),
                    }
                } else if args.is_empty() {
                    Err(EvalError::type_error(
                        format!("bridge: builtin '{name}' called with no arguments"),
                    ))
                } else {
                    // 3+ args: chain partial applications
                    let mut result = (bf.func)(&args[..1])?;
                    for arg in &args[1..] {
                        match result {
                            Value::Builtin(ref pf) => {
                                result = (pf.func)(&[arg.clone()])?;
                            }
                            _ => break,
                        }
                    }
                    Ok(result)
                }
            }
            _ => Err(EvalError::type_error(
                format!("bridge: '{name}' is not a builtin function"),
            )),
        }
    })
}

/// Register all builtins into the environment.
pub fn register(env: &mut Env) {
    let mut builtins_set = NixAttrs::new();

    // Delegate to sub-modules.
    types::register(&mut builtins_set);
    arithmetic::register(&mut builtins_set);
    lists::register(&mut builtins_set);
    attrs::register(&mut builtins_set);
    strings::register(&mut builtins_set);
    convert::register(&mut builtins_set);
    control::register(&mut builtins_set);
    context::register(&mut builtins_set);
    paths::register(&mut builtins_set);
    fetchers::register(&mut builtins_set);
    flake::register(&mut builtins_set);
    derivation::register(&mut builtins_set);
    versions::register(&mut builtins_set);
    misc::register(&mut builtins_set);

    // ── Constants ────────────────────────────────────────

    builtins_set.insert("storeDir".to_string(), Value::string("/nix/store"));

    // Populate `builtins.nixPath` from the NIX_PATH environment variable.
    let nix_path_value: Value = {
        let entries = parse_nix_path(&std::env::var("NIX_PATH").unwrap_or_default());
        let list: Vec<Value> = entries
            .into_iter()
            .map(|(prefix, path)| {
                let mut a = NixAttrs::new();
                a.insert("prefix".to_string(), Value::string(prefix));
                a.insert("path".to_string(), Value::string(path));
                Value::Attrs(Rc::new(a))
            })
            .collect();
        Value::list(list)
    };
    builtins_set.insert("nixPath".to_string(), nix_path_value);

    // true/false/null as builtins
    builtins_set.insert("true".to_string(), Value::Bool(true));
    builtins_set.insert("false".to_string(), Value::Bool(false));
    builtins_set.insert("null".to_string(), Value::Null);
    builtins_set.insert("nixVersion".to_string(), Value::string("2.24.0"));
    builtins_set.insert("currentSystem".to_string(), Value::string(current_system()));
    builtins_set.insert("langVersion".to_string(), Value::Int(6));

    // ── builtins.sui.* — sui-specific extensions ─────────
    let mut sui_ext_set = NixAttrs::new();
    sui_ext::register(&mut sui_ext_set);
    builtins_set.insert("sui".to_string(), Value::Attrs(Rc::new(sui_ext_set)));

    // ── builtins.builtins (self-reference) ───────────────
    let builtins_snapshot = Value::Attrs(Rc::new(builtins_set.clone()));
    builtins_set.insert("builtins".to_string(), builtins_snapshot);

    env.bind("builtins".to_string(), Value::Attrs(Rc::new(builtins_set.clone())));

    const DEFAULT_SCOPE: &[&str] = &[
        "abort",
        "baseNameOf",
        "derivation",
        "derivationStrict",
        "dirOf",
        "false",
        "fetchTarball",
        "import",
        "isNull",
        "map",
        "null",
        "removeAttrs",
        "scopedImport",
        "throw",
        "toString",
        "true",
    ];
    for name in DEFAULT_SCOPE {
        if let Some(v) = builtins_set.get(name) {
            env.bind((*name).to_string(), v.clone());
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
