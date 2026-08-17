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
mod flake_registry;
mod helpers;
mod import_cache;
mod lists;
mod misc;
mod activation_script_bridge;
mod bridge_helpers;
mod hash_bridge;
mod lock_file_bridge;
mod module_system_bridge;
mod narinfo_bridge;
mod nav;
mod realisation_bridge;
mod registry_bridge;
mod paths;
mod strings;
mod convergence;
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
pub(crate) use sui_compat::versions::{compare_versions, parse_drv_name, split_version};

// ── Public API ──────────────────────────────────────────────
pub use derivation::build_derivation;
/// `SUI_PARITY_STRICT` un-blinding collector — enumerate swallowed
/// force-error drops for the byte-parity campaign (default unchanged).
pub use derivation::parity_strict;
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
            convergence::register(&mut attrs);
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
    // Impersonate the version of the nix sui produces byte-parity against.
    // Rationale (why a stale value silently forks the derivation graph) lives
    // with the constant. It moved to sui-compat because this fix had ALREADY
    // been made here and never reached the bytecode VM, which sat at the stale
    // "2.24.0" — three engines hand-listing one fact.
    builtins_set.insert(
        "nixVersion".to_string(),
        Value::string(sui_compat::versions::IMPERSONATED_NIX_VERSION),
    );
    builtins_set.insert("currentSystem".to_string(), Value::string(current_system()));
    builtins_set.insert(
        "langVersion".to_string(),
        Value::Int(sui_compat::versions::LANG_VERSION),
    );

    // ── builtins.sui.* — sui-specific extensions ─────────
    let mut sui_ext_set = NixAttrs::new();
    sui_ext::register(&mut sui_ext_set);
    convergence::register(&mut sui_ext_set);
    module_system_bridge::register(&mut sui_ext_set);
    activation_script_bridge::register(&mut sui_ext_set);
    hash_bridge::register(&mut sui_ext_set);
    lock_file_bridge::register(&mut sui_ext_set);
    narinfo_bridge::register(&mut sui_ext_set);
    registry_bridge::register(&mut sui_ext_set);
    realisation_bridge::register(&mut sui_ext_set);
    builtins_set.insert("sui".to_string(), Value::Attrs(Rc::new(sui_ext_set)));

    // ── builtins.builtins (self-reference) ───────────────
    let builtins_snapshot = Value::Attrs(Rc::new(builtins_set.clone()));
    builtins_set.insert("builtins".to_string(), builtins_snapshot);

    env.bind("builtins".to_string(), Value::Attrs(Rc::new(builtins_set.clone())));

    // The bare-identifier scope, DERIVED from the one shared list rather than
    // hand-written here.
    //
    // This was a local `const DEFAULT_SCOPE` of 21 names — one of three
    // hand-maintained copies (walker, sui-ir, and the VM's
    // `is_global_builtin`), and they had drifted: **this copy was missing
    // `break`**, so `with { break = "LIB"; }; break` evaluated to `"LIB"` here
    // while nix and the VM both say `false`. A genuine nix global cannot be
    // shadowed by a `with`, so letting one through changes what a program
    // means. Measured: `builtins.typeOf break` → `lambda` on nix 2.31.5.
    //
    // Same shape as the `nixVersion` drift and the `builtins` name-set drift,
    // and the same fix: one source, N derived consumers. Rationale for the
    // individual entries (why the fetchers, why `placeholder`) now lives with
    // the list in `sui_compat::scope`.
    //
    // `builtins` is bound above rather than through this loop, so only the
    // three literal names join the callable set here.
    let default_scope: Vec<&str> = sui_compat::scope::CALLABLE_GLOBALS
        .iter()
        .copied()
        .chain(["true", "false", "null"])
        .collect();
    for name in &default_scope {
        if let Some(v) = builtins_set.get(name) {
            env.bind((*name).to_string(), v.clone());
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
