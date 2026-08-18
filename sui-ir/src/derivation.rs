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
//! ## Status (slice 7)
//!
//! FOD, `__structuredAttrs`, and copy-to-store `Path` attrs — all three of
//! which were typed `Unsupported(_)` gaps before — are now IMPLEMENTED here as
//! byte-faithful mirrors of the walker:
//!
//! * **Fixed-output (FOD) derivations** (`outputHash` present) —
//!   [`compute_derivation_outputs`]'s FOD branch mirrors the walker over the
//!   same `sui_compat` primitives and caches the modulo hash in the SHARED
//!   `sui_spec::derivation` table, so downstream input-addressed consumers
//!   substitute it (the fetchurl-bootstrap → stdenv chain).
//! * **`__structuredAttrs = true`** — the `__json` env var is emitted via
//!   [`ir_to_json_ctx`] (mirror of the walker's `to_json_with_context`).
//! * **copy-to-store `Path` attrs** (`src = ./.`) — NAR-hashed through
//!   `sui_compat::source` (mirror of the walker's `value.rs` copy-to-store).
//!
//! ## The one remaining honest gap (the frontier — slice 7 bisection)
//!
//! * **Eager, not lazy → lost dependency edges in the DEEP package-set
//!   fixpoint.** The walker defers the drv computation behind a memoized cell;
//!   this engine computes eagerly at `derivation` call time. For every leaf /
//!   2-level / multi-output fixture (and the whole differential corpus) the
//!   result is byte-identical, AND a real nixpkgs `hello.drvPath` is now
//!   *reachable* on `x86_64-linux` (a real `hello-2.12.2.drv`). It does NOT yet
//!   byte-match nix, because eval_ir's slice-5 overlay-fixpoint promotion drops
//!   string-context dependency EDGES the walker preserves through the deep
//!   stdenv/package fixpoint — e.g. `libxcrypt` loses its `nativeBuildInputs`
//!   (perl) edge, `openssl` loses its `postPatch` (coreutils) edge, `xgcc`
//!   keeps `LIBRARY_PATH`'s bytes but loses `zlib`'s `out` output edge — so the
//!   dependent drvPaths diverge. The env-var coercion + context fold BELOW is
//!   correct (the strings are byte-right); the edge is lost UPSTREAM in the
//!   fixpoint eval. This is the walker's own documented
//!   "materialized-attrset-with-keys partial" problem
//!   (`sui-eval/src/builtins/derivation.rs` §NOTE 2026-07-10) — "a larger
//!   architectural change" — not a defect in this module. The furthest real
//!   three-way match today is `bootstrap-stage-xgcc-stdenv-linux`; divergence
//!   first enters at `bootstrap-stage2-stdenv-linux`.

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

    let structured_attrs = input
        .get("__structuredAttrs")
        .and_then(|v| v.force().ok())
        .is_some_and(|v| matches!(v, IrValue::Bool(true)));

    let mut env_vars: BTreeMap<String, String> = BTreeMap::new();
    if structured_attrs {
        // `__structuredAttrs = true` collapses ALL attrs into one `__json` env
        // var (mirror of the walker's structured branch + `to_json_with_context`
        // via `ir_to_json_ctx`). name/system/builder go INTO the JSON; args and
        // the control flags do not; per-output vars are added by output-filling.
        let mut obj = serde_json::Map::new();
        for (k, v) in input.iter() {
            if matches!(
                k.as_str(),
                "args" | "__structuredAttrs" | "__ignoreNulls" | "__impure"
                    | "__contentAddressed"
            ) {
                continue;
            }
            let forced_v = match v.force() {
                Ok(fv) => fv,
                Err(_) => continue,
            };
            if ignore_nulls && matches!(forced_v, IrValue::Null) {
                continue;
            }
            match ir_to_json_ctx(&forced_v, &mut collected_ctx) {
                Ok(jv) => {
                    obj.insert(k.clone(), jv);
                }
                Err(
                    e @ (IrEvalError::Throw(_)
                    | IrEvalError::Abort(_)
                    | IrEvalError::AssertionFailed
                    | IrEvalError::Unsupported(_)
                    | IrEvalError::MissingBuiltin(_)),
                ) => return Err(e),
                Err(_) => {}
            }
        }
        let json =
            sui_compat::versions::nix_json_to_string(&serde_json::Value::Object(obj))
                .unwrap_or_default();
        env_vars.insert("__json".to_string(), json);
    } else {
        // `input.iter()` is BTreeMap-sorted, matching the walker's env self-sort.
        for (k, v) in input.iter() {
            if matches!(
                k.as_str(),
                "name" | "system" | "builder" | "args" | "__ignoreNulls" | "__impure"
                    | "__contentAddressed"
            ) {
                continue;
            }
            // Mirror the walker's best-effort force-then-coerce: a force error is
            // a benign skip (a dropped dep the walker also drops); a genuine
            // throw/abort/assertion PROPAGATES; and a copy-to-store / structured
            // gap PROPAGATES as a typed gap rather than silently producing a
            // wrong drvPath (silent divergence is forbidden).
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
                // Propagate the classes the walker propagates, PLUS the pure-
                // engine store gaps (so a `src = ./.` derivation is a typed gap,
                // never a silently-wrong drvPath).
                Err(
                    e @ (IrEvalError::Throw(_)
                    | IrEvalError::Abort(_)
                    | IrEvalError::AssertionFailed
                    | IrEvalError::Unsupported(_)
                    | IrEvalError::MissingBuiltin(_)),
                ) => return Err(e),
                // A benign coerce-none (an attrs partial with no __toString/
                // outPath) drops the attr, exactly like the walker.
                Err(_) => {}
            }
        }
        env_vars.insert("name".to_string(), name.to_string());
        env_vars.insert("system".to_string(), system.clone());
        env_vars.insert("builder".to_string(), builder.clone());
    }

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
    mut drv: Derivation,
) -> Result<(String, BTreeMap<String, String>), IrEvalError> {
    if input.contains_key("outputHash") {
        // Fixed-output (FOD) — byte-mirror of the walker's `compute_full_drv`
        // FOD branch (`sui-eval/src/builtins/derivation.rs`). The shared spec's
        // `SeedFixedOutputHash` phase is an M3 stub, so — exactly like the
        // walker — FOD is computed here in Rust over the SAME `sui_compat`
        // primitives, and the resulting modulo hash is cached in the SAME
        // `sui_spec::derivation` modulo table so downstream input-addressed
        // consumers substitute it (the fetchurl-bootstrap → stdenv → hello
        // chain).
        use sui_compat::derivation::DerivationOutput;

        let raw_output_hash = force_attr_string(input, "outputHash")?;
        let raw_algo = optional_attr_string(input, "outputHashAlgo")?.unwrap_or_default();
        let output_hash_mode =
            optional_attr_string(input, "outputHashMode")?.unwrap_or_else(|| "flat".to_string());
        let is_recursive = output_hash_mode == "recursive" || output_hash_mode == "nar";

        let output_hash_algo = if raw_algo.is_empty() {
            infer_algo_from_hash(&raw_output_hash).unwrap_or_else(|| "sha256".to_string())
        } else {
            raw_algo
        };

        let algo = sui_compat::hash::HashAlgorithm::from_nix_str(&output_hash_algo).map_err(|e| {
            IrEvalError::TypeError(format!(
                "derivation: invalid outputHashAlgo {output_hash_algo:?}: {e}"
            ))
        })?;
        let parsed =
            sui_compat::hash::NixHash::parse_any(algo, &raw_output_hash).map_err(|e| {
                IrEvalError::TypeError(format!(
                    "derivation: invalid outputHash {raw_output_hash:?}: {e}"
                ))
            })?;
        let output_hash_hex = parsed.to_hex();

        let out_path = sui_compat::store_path::compute_fixed_output_hash(
            &output_hash_algo,
            &output_hash_hex,
            is_recursive,
            name,
        );

        drv.outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: out_path.clone(),
                hash_algo: if is_recursive {
                    format!("r:{output_hash_algo}")
                } else {
                    output_hash_algo.clone()
                },
                hash: output_hash_hex.clone(),
            },
        );
        // CppNix hashes the FOD with `env["out"] = <out-path>` present.
        drv.env.insert("out".to_string(), out_path.clone());

        let drv_content = drv.serialize();
        let drv_refs: Vec<String> = drv
            .input_derivations
            .keys()
            .cloned()
            .chain(drv.input_sources.iter().cloned())
            .collect();
        let drv_path = sui_compat::store_path::compute_drv_path_with_refs(
            drv_content.as_bytes(),
            name,
            &drv_refs,
        );

        // R3: emit the ATerm when SUI_EMIT_DRV is set. See the twin in
        // sui-eval/src/builtins/derivation.rs for why this exists — a diverging
        // drvPath is undiagnosable without the bytes whose hash it IS.
        if let Ok(dir) = std::env::var("SUI_EMIT_DRV") {
            if !dir.is_empty() {
                let base = drv_path.rsplit('/').next().unwrap_or(&drv_path);
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(
                    std::path::Path::new(&dir).join(base),
                    drv_content.as_bytes(),
                );
            }
        }

        // FOD `hashDerivationModulo` = sha256("fixed:out:<methodAlgo>:<hex>:<out>").
        let method_algo = if is_recursive {
            format!("r:{output_hash_algo}")
        } else {
            output_hash_algo.clone()
        };
        let modulo_preimage = format!("fixed:out:{method_algo}:{output_hash_hex}:{out_path}");
        let modulo_hex: String = {
            use sha2::{Digest, Sha256};
            Sha256::digest(modulo_preimage.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        sui_spec::derivation::remember_modulo_hash(&drv_path, &modulo_hex);

        dump_drv_if_requested(name, &drv_path, &drv);
        let mut out_paths = BTreeMap::new();
        out_paths.insert("out".to_string(), out_path);
        return Ok((drv_path, out_paths));
    }

    let outputs = parse_outputs_list(input)?;
    let algo = sui_spec::derivation::load_canonical().map_err(|e| {
        IrEvalError::TypeError(format!("derivation algorithm spec failed to load: {e}"))
    })?;
    let (drv_path, out_paths, drv_final) = sui_spec::derivation::apply(&algo, drv, outputs, name)
        .map_err(|e| {
            IrEvalError::TypeError(format!("derivation algorithm interpreter failed: {e}"))
        })?;
    dump_drv_if_requested(name, &drv_path, &drv_final);
    Ok((drv_path, out_paths))
}

/// Gated parity diagnostic (inert unless `SUI_IR_DUMP_DRV` is set): print every
/// constructed derivation's `name` + computed `drvPath` (and, under
/// `SUI_IR_DUMP_ATERM`, its full ATerm) so eval_ir's derivation closure can be
/// byte-diffed against nix's on-disk `.drv` files. This is the tool that
/// bisected the slice-7 deep-fixpoint edge-drop frontier (aligning eval_ir's
/// hello closure against nix's, drv-by-drv, to the shallowest diverging build);
/// future parity slices reuse it. Zero cost when the env var is unset.
fn dump_drv_if_requested(name: &str, drv_path: &str, drv: &Derivation) {
    // The env vars are read ONCE and cached, so the default (unset) path is a
    // single relaxed atomic load per call — no per-derivation getenv on the hot
    // path (a hello eval constructs ~460 derivations).
    use std::sync::OnceLock;
    static FILTER: OnceLock<Option<String>> = OnceLock::new();
    static ATERM: OnceLock<bool> = OnceLock::new();
    let Some(filter) = FILTER.get_or_init(|| std::env::var("SUI_IR_DUMP_DRV").ok()) else {
        return;
    };
    let matches = filter == "ALL" || filter.split(',').any(|f| !f.is_empty() && name.contains(f));
    if !matches {
        return;
    }
    eprintln!("[[DRV]] name={name} path={drv_path}");
    if *ATERM.get_or_init(|| std::env::var_os("SUI_IR_DUMP_ATERM").is_some()) {
        eprintln!("[[ATERM]] {}", drv.serialize());
    }
}

/// Context-collecting JSON serializer for `__structuredAttrs` — mirror of the
/// walker's `Value::to_json_with_context`. A derivation (attrs with `outPath`/
/// `__toString`) and a `Path` serialize to their copy-to-store string WITH
/// context merged into `ctx`, so an output reference inside `__json` still
/// flows into `inputDrvs`/`inputSrcs`.
fn ir_to_json_ctx(
    v: &IrValue,
    ctx: &mut crate::eval_ir::IrStringContext,
) -> Result<serde_json::Value, IrEvalError> {
    let forced = v.force()?;
    Ok(match &forced {
        IrValue::Null => serde_json::Value::Null,
        IrValue::Bool(b) => serde_json::Value::Bool(*b),
        IrValue::Int(n) => serde_json::json!(*n),
        IrValue::Float(f) => serde_json::json!(*f),
        IrValue::Str(s, c) => {
            if let Some(c) = c {
                ctx.merge(c);
            }
            serde_json::Value::String((**s).clone())
        }
        IrValue::Path(_) => {
            let (s, c) = coerce_to_string_ctx(&forced, true)?;
            ctx.merge(&c);
            serde_json::Value::String(s)
        }
        IrValue::List(items) => {
            let mut arr = Vec::with_capacity(items.len());
            for item in items.iter() {
                arr.push(ir_to_json_ctx(item, ctx)?);
            }
            serde_json::Value::Array(arr)
        }
        IrValue::Attrs(attrs) => {
            if attrs.contains_key("__toString") || attrs.contains_key("outPath") {
                let (s, c) = coerce_to_string_ctx(&forced, true)?;
                ctx.merge(&c);
                return Ok(serde_json::Value::String(s));
            }
            let mut map = serde_json::Map::new();
            for (k, val) in attrs.iter() {
                map.insert(k.clone(), ir_to_json_ctx(val, ctx)?);
            }
            serde_json::Value::Object(map)
        }
        IrValue::Lambda(_) | IrValue::Builtin(..) => {
            return Err(IrEvalError::TypeError(format!(
                "cannot serialize {} to JSON (__structuredAttrs)",
                forced.type_name()
            )));
        }
        IrValue::Thunk(_) => unreachable!("force() returned a thunk"),
    })
}

/// Infer the hash algorithm from an SRI-formatted hash (`<algo>-<base64>`);
/// `None` for hex / nix-base32 (no algo metadata). Mirror of the walker's.
fn infer_algo_from_hash(hash: &str) -> Option<String> {
    for algo in ["sha256", "sha512", "sha1", "md5"] {
        if hash.starts_with(&format!("{algo}-")) {
            return Some(algo.to_string());
        }
    }
    None
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
