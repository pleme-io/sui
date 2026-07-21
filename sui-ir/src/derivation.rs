//! `derivation` / `derivationStrict` on `IrValue` — a byte-faithful mirror of
//! the tree-walker's `sui_eval::builtins::derivation`.
//!
//! The load-bearing rule of this slice: **drift is impossible by
//! construction.** The input-addressed drvPath + output-path computation is
//! NOT re-implemented here — it routes through the exact same interpreter the
//! walker uses (`sui_spec::derivation::apply` over the Lisp-authored
//! `sui-spec/specs/derivation.lisp`), fed the exact same
//! `sui_compat::derivation::Derivation`. So the ATerm serialization + store-path
//! hashing are shared code; a leaf derivation's drvPath computed here is
//! byte-identical to the walker's and to `nix eval …drvPath`, and a deviation
//! would be a bug in the SHARED spec, not a divergent scheme.
//!
//! What this module DOES own — and must get exactly right — is the bridge from
//! `IrValue` to that `Derivation`:
//!   1. force the input attrset; extract `name` / `system` / `builder`;
//!   2. string-coerce `builder` + `args` + every remaining attr WITH string
//!      context (the context sink is [`crate::eval_ir::coerce_to_string_ctx`]);
//!   3. fold the collected context into `inputDrvs` (`Output{drv,output}`) and
//!      `inputSrcs` (`Plain` under `/nix/store/`) — this is how a consuming
//!      derivation rediscovers its dependency edges, and therefore how the
//!      drvPath picks up its inputs;
//!   4. build the result attrset whose `.drvPath` / `.outPath` / per-output
//!      `outPath` strings carry the producing drv's context, so `${pkg}` in the
//!      next derivation flows the edge onward.
//!
//! ## Honest gaps (typed, never silent — see the milestone report)
//!
//! * **Eager, not lazy.** The walker defers the drv computation behind a
//!   memoized `OnceCell` so a mutually-referential package set never forces its
//!   dependency graph until a store path is demanded. This engine computes
//!   eagerly at `derivation` call time, which forces all input attrs. For the
//!   milestone corpus (non-self-referential leaf / 2-level / multi-output) the
//!   result is byte-identical; a self-referential set (real stdenv) would
//!   blackhole here. Named, not hidden.
//! * **Fixed-output (FOD) derivations** (`outputHash` present) are a typed
//!   `Unsupported("derivation-fixed-output")` gap — the FOD hashing +
//!   `remember_modulo_hash` path is deferred to a later slice.
//! * **`__structuredAttrs = true`** is a typed
//!   `Unsupported("derivation-structured-attrs")` gap.
//! * **copy-to-store `Path` attrs** (`src = ./.`) are a typed
//!   `Unsupported("path-copy-to-store")` gap: the pure IR engine has no store
//!   to NAR-copy into, so rather than silently drop the input (which would
//!   diverge the drvPath), the whole derivation surfaces the gap.

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::eval_ir::{coerce_to_string_ctx, IrAttrs, IrContextElem, IrEvalError, IrValue};

use sui_compat::derivation::Derivation;

/// `derivation` / `derivationStrict` — both delegate here (the walker aliases
/// them too). Computes the drv eagerly and returns the CppNix-shaped result
/// attrset with context-bearing store-path fields.
pub(crate) fn build_derivation(arg: &IrValue) -> Result<IrValue, IrEvalError> {
    let forced = arg.force()?;
    let input = match &forced {
        IrValue::Attrs(a) => a.clone(),
        other => {
            return Err(IrEvalError::TypeMismatch {
                expected: "set",
                got: other.type_name(),
            })
        }
    };

    let name = force_attr_string(&input, "name")?;
    let (drv_path, out_paths) = compute_full_drv(&input, &name)?;
    build_derivation_result(&input, &name, &drv_path, &out_paths)
}

/// Force an attribute and require it to be present + string-coercible
/// (context dropped — the mirror of the walker's `force_attr_string`).
fn force_attr_string(input: &IrAttrs, key: &str) -> Result<String, IrEvalError> {
    let v = input
        .get(key)
        .ok_or_else(|| IrEvalError::AttrNotFound(key.to_string()))?;
    let forced = v.force()?;
    let (s, _ctx) = coerce_to_string_ctx(&forced, true)?;
    Ok(s)
}

/// Force an optional attr, `None` if absent.
fn optional_attr_string(input: &IrAttrs, key: &str) -> Result<Option<String>, IrEvalError> {
    match input.get(key) {
        None => Ok(None),
        Some(v) => {
            let forced = v.force()?;
            let (s, _ctx) = coerce_to_string_ctx(&forced, true)?;
            Ok(Some(s))
        }
    }
}

/// The eager half: build the `Derivation`, compute the .drv path + output
/// paths. Mirror of the walker's `compute_full_drv` minus the store write
/// (the pure engine has no store to write the `.drv` file into — a reported
/// difference, not a value difference: the drvPath bytes are the .drv's
/// content-address, computed identically).
fn compute_full_drv(
    input: &IrAttrs,
    name: &str,
) -> Result<(String, BTreeMap<String, String>), IrEvalError> {
    let drv = construct_derivation(input, name)?;
    compute_derivation_outputs(input, name, drv)
}

/// Extract required/optional attrs, string-coerce every user attr WITH context,
/// and fold the collected context into `inputDrvs`/`inputSrcs`. Byte-mirror of
/// the walker's `construct_derivation`.
fn construct_derivation(input: &IrAttrs, name: &str) -> Result<Derivation, IrEvalError> {
    use crate::eval_ir::IrStringContext;

    let system = force_attr_string(input, "system")?;

    let mut collected_ctx = IrStringContext::new();

    // `builder` carries context too (a bash-output builder path makes bash.drv
    // an inputDrv). Coerce WITH context — force_attr_string drops it.
    let builder = {
        let v = input.get("builder").ok_or_else(|| {
            IrEvalError::TypeError("derivation: missing required attribute 'builder'".into())
        })?;
        let forced = v.force()?;
        let (s, ctx) = coerce_to_string_ctx(&forced, true)?;
        collected_ctx.merge(&ctx);
        s
    };

    let args_list: Vec<String> = if let Some(a) = input.get("args") {
        let forced_args = a.force()?;
        let list = as_list(&forced_args)?;
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            let forced = item.force()?;
            let (s, ctx) = coerce_to_string_ctx(&forced, true)?;
            collected_ctx.merge(&ctx);
            out.push(s);
        }
        out
    } else {
        Vec::new()
    };

    let ignore_nulls = input
        .get("__ignoreNulls")
        .and_then(|v| v.force().ok())
        .is_some_and(|v| matches!(v, IrValue::Bool(true)));

    // `__structuredAttrs = true` fundamentally changes the env layout — a typed
    // gap, not a wrong answer (never silently mis-serialize the JSON env).
    let structured_attrs = input
        .get("__structuredAttrs")
        .and_then(|v| v.force().ok())
        .is_some_and(|v| matches!(v, IrValue::Bool(true)));
    if structured_attrs {
        return Err(IrEvalError::Unsupported("derivation-structured-attrs"));
    }

    let mut env_vars: BTreeMap<String, String> = BTreeMap::new();
    // `input.iter()` is BTreeMap-sorted, matching the walker's env self-sort.
    for (k, v) in input.iter() {
        if matches!(
            k.as_str(),
            "name" | "system" | "builder" | "args" | "__ignoreNulls" | "__impure"
                | "__contentAddressed"
        ) {
            continue;
        }
        // Mirror the walker's best-effort force-then-coerce: a force error is a
        // benign skip (a dropped dep the walker also drops); a genuine
        // throw/abort/assertion PROPAGATES; and a copy-to-store / structured
        // gap PROPAGATES as a typed gap rather than silently producing a wrong
        // drvPath (silent divergence is forbidden).
        let forced_v = match v.force() {
            Ok(fv) => fv,
            Err(_) => continue,
        };
        if ignore_nulls && matches!(forced_v, IrValue::Null) {
            continue;
        }
        match coerce_to_string_ctx(&forced_v, true) {
            Ok((s, ctx)) => {
                collected_ctx.merge(&ctx);
                env_vars.insert(k.clone(), s);
            }
            // Propagate the classes the walker propagates, PLUS the pure-engine
            // store gaps (so a `src = ./.` derivation is a typed gap, never a
            // silently-wrong drvPath).
            Err(
                e @ (IrEvalError::Throw(_)
                | IrEvalError::Abort(_)
                | IrEvalError::AssertionFailed
                | IrEvalError::Unsupported(_)
                | IrEvalError::MissingBuiltin(_)),
            ) => return Err(e),
            // A benign coerce-none (an attrs partial with no __toString/outPath)
            // drops the attr, exactly like the walker.
            Err(_) => {}
        }
    }
    env_vars.insert("name".to_string(), name.to_string());
    env_vars.insert("system".to_string(), system.clone());
    env_vars.insert("builder".to_string(), builder.clone());

    // Fold context → inputDrvs / inputSrcs (byte-mirror of the walker's fold).
    let mut input_derivations: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut input_sources: Vec<String> = Vec::new();
    for elem in collected_ctx.iter() {
        match elem {
            IrContextElem::Output { drv, output } => {
                input_derivations
                    .entry(drv.clone())
                    .or_default()
                    .push(output.clone());
            }
            IrContextElem::Plain(p) => {
                if p.starts_with("/nix/store/") && !input_sources.contains(p) {
                    input_sources.push(p.clone());
                }
            }
            IrContextElem::DrvDeep(_) => {} // not consumed into these fields
        }
    }
    for outs in input_derivations.values_mut() {
        outs.sort();
        outs.dedup();
    }

    Ok(Derivation {
        outputs: BTreeMap::new(),
        input_derivations,
        input_sources,
        system,
        builder,
        args: args_list,
        env: env_vars,
    })
}

/// Compute the .drv path + output paths. Input-addressed routes through the
/// shared Lisp-authored spec interpreter (byte-identical to the walker);
/// fixed-output is a typed gap.
fn compute_derivation_outputs(
    input: &IrAttrs,
    name: &str,
    drv: Derivation,
) -> Result<(String, BTreeMap<String, String>), IrEvalError> {
    if input.contains_key("outputHash") {
        // FOD hashing + the modulo cache are deferred — a typed gap, never a
        // wrong answer.
        let _ = optional_attr_string(input, "outputHash")?;
        return Err(IrEvalError::Unsupported("derivation-fixed-output"));
    }

    let outputs = parse_outputs_list(input)?;
    let algo = sui_spec::derivation::load_canonical().map_err(|e| {
        IrEvalError::TypeError(format!("derivation algorithm spec failed to load: {e}"))
    })?;
    let (drv_path, out_paths, _drv) = sui_spec::derivation::apply(&algo, drv, outputs, name)
        .map_err(|e| {
            IrEvalError::TypeError(format!("derivation algorithm interpreter failed: {e}"))
        })?;
    Ok((drv_path, out_paths))
}

/// Parse the optional `outputs` list (defaults to `["out"]`). Mirror of the
/// walker's `parse_outputs_list`.
fn parse_outputs_list(input: &IrAttrs) -> Result<Vec<String>, IrEvalError> {
    if let Some(o) = input.get("outputs") {
        let forced_o = o.force()?;
        let list = as_list(&forced_o)?;
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            let forced = item.force()?;
            let s = match &forced {
                IrValue::Str(s, _) => (**s).clone(),
                other => {
                    return Err(IrEvalError::TypeMismatch {
                        expected: "string",
                        got: other.type_name(),
                    })
                }
            };
            out.push(s);
        }
        if out.is_empty() {
            return Err(IrEvalError::TypeError(
                "derivation: outputs list must not be empty".into(),
            ));
        }
        Ok(out)
    } else {
        Ok(vec!["out".to_string()])
    }
}

/// Assemble the CppNix-shaped result attrset. Every store-path string carries
/// context so a consuming derivation rediscovers the edge:
///   * `.drvPath`  → `DrvDeep(drv_path)`
///   * `.outPath`  → `Output(drv_path, primary_output)`
///   * per-output `outPath` → `Output(drv_path, output_name)`
/// Byte-mirror of the walker's `build_derivation_result`, but eager (the
/// strings are computed values, not lazy thunks — byte-identical output).
fn build_derivation_result(
    input: &IrAttrs,
    name: &str,
    drv_path: &str,
    out_paths: &BTreeMap<String, String>,
) -> Result<IrValue, IrEvalError> {
    use crate::eval_ir::IrStringContext;

    let output_names = parse_outputs_list(input)?;
    let primary = output_names
        .first()
        .cloned()
        .unwrap_or_else(|| "out".to_string());

    let drv_path_val = {
        let mut ctx = IrStringContext::new();
        ctx.add_drv_deep(drv_path.to_string());
        IrValue::string_with_context(drv_path.to_string(), ctx)
    };

    let out_path_val = {
        let p = out_paths.get(&primary).cloned().unwrap_or_default();
        let mut ctx = IrStringContext::new();
        ctx.add_output(drv_path.to_string(), primary.clone());
        IrValue::string_with_context(p, ctx)
    };

    // Start from the input attrs (CppNix: the result IS the input plus the
    // computed fields; `drvAttrs` preserves the originals).
    let mut result = (*input).clone();
    result.insert("type".to_string(), IrValue::string("derivation"));
    result.insert("drvPath".to_string(), drv_path_val.clone());
    result.insert("drvAttrs".to_string(), IrValue::Attrs(Rc::new((*input).clone())));
    result.insert("outPath".to_string(), out_path_val);
    result.insert("outputName".to_string(), IrValue::string(primary.clone()));

    let mut all_outputs: Vec<IrValue> = Vec::with_capacity(output_names.len());
    for output_name in &output_names {
        let mut out_attrs = IrAttrs::new();
        let p = out_paths.get(output_name).cloned().unwrap_or_default();
        let mut ctx = IrStringContext::new();
        ctx.add_output(drv_path.to_string(), output_name.clone());
        out_attrs.insert(
            "outPath".to_string(),
            IrValue::string_with_context(p, ctx),
        );
        out_attrs.insert("drvPath".to_string(), drv_path_val.clone());
        out_attrs.insert("type".to_string(), IrValue::string("derivation"));
        out_attrs.insert("outputName".to_string(), IrValue::string(output_name.clone()));
        out_attrs.insert("name".to_string(), IrValue::string(name));
        let out_val = IrValue::Attrs(Rc::new(out_attrs));
        all_outputs.push(out_val.clone());
        result.insert(output_name.clone(), out_val);
    }

    result.insert("all".to_string(), IrValue::List(Rc::new(all_outputs)));

    Ok(IrValue::Attrs(Rc::new(result)))
}

/// Local list accessor (avoids leaking `builtins::as_list` across modules).
fn as_list(v: &IrValue) -> Result<Rc<Vec<IrValue>>, IrEvalError> {
    match v {
        IrValue::List(items) => Ok(items.clone()),
        other => Err(IrEvalError::TypeMismatch {
            expected: "list",
            got: other.type_name(),
        }),
    }
}
