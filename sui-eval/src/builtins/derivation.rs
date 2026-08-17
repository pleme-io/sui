//! Derivation builtins: derivation, derivationStrict, build_derivation.
//!
//! The `derivation` and `derivationStrict` builtins both delegate to
//! `build_derivation`, which:
//!   1. Forces the input attrset and pulls out the special attributes.
//!   2. Coerces all other attributes to strings for the env map.
//!   3. Builds an in-memory `Derivation` for ATerm serialization.
//!   4. Computes the .drv path and output paths.
//!   5. Returns an attrset matching CppNix's interface.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_builtin(builtins, "derivation", |args| {
        build_derivation(&args[0])
    });

    // derivationStrict — alias to derivation
    register_builtin(builtins, "derivationStrict", |args| {
        build_derivation(&args[0])
    });
}

/// `SUI_PARITY_STRICT` — the byte-parity campaign's M0 un-blinding collector.
///
/// The env-loop force-error swallow (both the flat-env and `__structuredAttrs`
/// paths below) is DELIBERATELY a best-effort skip — surfacing it as a hard
/// error regresses `libxcrypt` and a dozen clean value-diverges into aborts
/// (measured; see the `NOTE (2026-07-10)` at the flat-env swallow). But the
/// swallow HIDES stacked parity roots: a diverging package like ffmpeg/neovim
/// shows up only as a generic "value diverges", never as the *named* dropped
/// dependency + the force error that dropped it.
///
/// This collector un-blinds them **for enumeration only** — when
/// `SUI_PARITY_STRICT` is set in the environment, each swallowed drop is
/// recorded onto a thread-local ledger (drv name, attr key, whether it was the
/// flat-env or structured-attrs path, the force-error string) INSTEAD OF being
/// lost, while the `continue` behaviour stays byte-for-byte identical. A strict
/// run therefore NAMES the exact roots a package stacks on, with zero change to
/// the default drvPath outcome (the parity gate stays green).
///
/// Tier: this is an *enumeration instrument*, not an invariant seal — it makes
/// the hidden roots observable so each can be closed at its load-bearing cause.
pub mod parity_strict {
    use std::cell::RefCell;

    /// Which force-error swallow site dropped the dependency.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum DropSite {
        /// The flat-env attr loop (`derivation.rs` NOTE 2026-07-10 swallow).
        FlatEnv,
        /// The `__structuredAttrs` `__json` attr loop.
        StructuredAttrs,
    }

    /// One swallowed force-error: the drv whose env was being built, the attr
    /// key that failed to force + coerce, the swallow site, and the force error.
    #[derive(Clone, Debug)]
    pub struct DroppedDep {
        pub drv: String,
        pub attr: String,
        pub site: DropSite,
        pub force_err: String,
    }

    thread_local! {
        static LEDGER: RefCell<Vec<DroppedDep>> = const { RefCell::new(Vec::new()) };
    }

    /// True iff `SUI_PARITY_STRICT` is set — the ONLY gate. When false, `record`
    /// is a no-op and the swallow behaves exactly as before (default unchanged).
    #[inline]
    pub fn enabled() -> bool {
        std::env::var_os("SUI_PARITY_STRICT").is_some()
    }

    /// Record a swallowed force-error. No-op unless `enabled()`. Never changes
    /// control flow at the call site — the caller still `continue`s.
    pub fn record(drv: &str, attr: &str, site: DropSite, force_err: &str) {
        if !enabled() {
            return;
        }
        LEDGER.with(|l| {
            l.borrow_mut().push(DroppedDep {
                drv: drv.to_string(),
                attr: attr.to_string(),
                site,
                force_err: force_err.to_string(),
            })
        });
    }

    /// Drain + return the accumulated drops (clears the ledger). Used by the
    /// strict enumeration tooling after an eval to report the measured roots.
    pub fn drain() -> Vec<DroppedDep> {
        LEDGER.with(|l| std::mem::take(&mut *l.borrow_mut()))
    }

    /// Non-destructive count of pending drops (for a quick "did the swallow
    /// fire?" check without consuming the ledger).
    pub fn len() -> usize {
        LEDGER.with(|l| l.borrow().len())
    }
}

/// The fully-computed drv, memoized behind the lazy `derivation` result's
/// fields.  Produced once by `compute_full_drv` and shared by `drvPath` /
/// `outPath` / every output / `all`.
struct ComputedDrv {
    drv_path: String,
    out_paths: std::collections::BTreeMap<String, String>,
}

/// The eager half: force the inputs, build the ATerm, compute the .drv path +
/// output paths, and write the .drv to the store.  Split out of
/// `build_derivation` so it can be deferred behind a memoized lazy thunk (nix's
/// `derivation` returns an attrset with a LAZY `.drvPath`/`.outPath`; only
/// `derivationStrict` computes eagerly).  Deferring this is what makes the
/// bootstrap perl↔libxcrypt fixpoint resolve: forcing a derivation VALUE to
/// WHNF no longer forces its dependency attrs, so a mutually-referential
/// package set never manufactures a same-thunk Blackhole re-entry that nix
/// avoids by never forcing the deps until a store path is demanded.
fn compute_full_drv(input: &NixAttrs, name: &str) -> Result<ComputedDrv, EvalError> {
    let (_name, drv) = construct_derivation(input)?;
    let (drv_path, out_paths, mut drv) = compute_derivation_outputs(input, name, drv)?;
    write_derivation_to_store(&drv_path, &out_paths, &mut drv)?;
    // R3 (moved 2026-08-11): the emitter belongs HERE, where the FINAL path and
    // drv are both in hand. Its first home was inside `compute_derivation_outputs`,
    // which not every derivation routes through — 455 ATerms emitted while the one
    // under investigation never appeared, which read as a fourth construction path
    // and was really a misplaced probe. `compute_full_drv` is the single funnel.
    if let Ok(dir) = std::env::var("SUI_EMIT_DRV") {
        if !dir.is_empty() {
            let base = drv_path.rsplit('/').next().unwrap_or(&drv_path);
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(std::path::Path::new(&dir).join(base), drv.serialize().as_bytes());
        }
    }

    // Diagnostic: `SUI_DUMP_DRV=<name>` (or `all`) dumps the fully-computed
    // derivation — inputDrvs / inputSrcs / env / outputs / args / the ATerm —
    // to stderr so a computed drv can be field-diffed against
    // `nix derivation show`. Zero cost when unset. Load-bearing parity tool,
    // not a behavior change.
    if let Some(want) = std::env::var_os("SUI_DUMP_DRV") {
        let want = want.to_string_lossy();
        if want == "all" || name.contains(want.as_ref()) {
            eprintln!("[SUI_DUMP_DRV] === {name} ===");
            eprintln!("[SUI_DUMP_DRV] drvPath = {drv_path}");
            eprintln!("[SUI_DUMP_DRV] outputs = {out_paths:#?}");
            eprintln!("[SUI_DUMP_DRV] inputDrvs = {:#?}", drv.input_derivations);
            eprintln!("[SUI_DUMP_DRV] inputSrcs = {:#?}", drv.input_sources);
            eprintln!("[SUI_DUMP_DRV] builder = {}", drv.builder);
            eprintln!("[SUI_DUMP_DRV] args = {:#?}", drv.args);
            eprintln!("[SUI_DUMP_DRV] env = {:#?}", drv.env);
            eprintln!("[SUI_DUMP_DRV] ATerm = {}", drv.serialize());
        }
    }

    Ok(ComputedDrv { drv_path, out_paths })
}

pub fn build_derivation(arg: &Value) -> Result<Value, EvalError> {
    let forced = crate::eval::force_value(arg)?;
    let input_owned = forced.to_attrs()?;
    let input = &input_owned;

    // `name` is the ONLY input attr the lazy result needs eagerly (it labels
    // the outputs + names the store path); nix forces `name` (and `system`) at
    // `derivation` time too.  Everything else — every dependency attr — is
    // deferred into `compute_full_drv`, forced only when a store path is
    // actually demanded.
    let name = force_attr_string(input, "name")?;

    // 4. Assemble the result attrset with LAZY computed fields.
    build_derivation_result(input, &name)
}

/// Extract required/optional attributes and construct the Derivation skeleton.
///
/// Walks each user-provided attribute, string-coerces it for the
/// builder env, AND collects the string's context (drv-path +
/// output references + plain paths).  The collected context is
/// funnelled into `input_derivations` / `input_sources` — the
/// fields CppNix uses to compute the dependent's drvPath via
/// `hashDerivationModulo`.
fn construct_derivation(
    input: &NixAttrs,
) -> Result<(String, sui_compat::derivation::Derivation), EvalError> {
    use std::collections::BTreeMap;
    use crate::value::{ContextElement, StringContext};

    let name = force_attr_string(input, "name")?;
    let system = force_attr_string(input, "system")?;

    // Accumulate context across every coerce call.  Each element
    // ends up in one of three places on the Derivation:
    //   - Output { drv, output } → input_derivations[drv].push(output)
    //   - Plain(path) starting with /nix/store/ → input_sources
    //   - Plain(path) elsewhere                  → ignored (not a store ref)
    let mut collected_ctx = StringContext::new();

    // The `builder` carries string context too: e.g. a bash builder path is an
    // OUTPUT of bash.drv, so bash.drv must appear in inputDrvs.  Coerce it WITH
    // context (force_attr_string DROPS context) and fold that in — otherwise
    // every stdenv derivation is missing its builder's input drv, diverging the
    // drv hash from nix.
    let builder = {
        let v = input.get("builder").ok_or_else(|| {
            EvalError::TypeError("derivation: missing required attribute 'builder'".into())
        })?;
        let (s, ctx) = coerce_drv_value_to_string_with_context(v)?;
        collected_ctx.merge(&ctx);
        s
    };

    let args_list: Vec<String> = if let Some(a) = input.get("args") {
        let forced_args = crate::eval::force_value(a)?;
        let list = forced_args.as_list()?;
        let mut out = Vec::with_capacity(list.len());
        for item in list {
            let (s, ctx) = coerce_drv_value_to_string_with_context(item)?;
            collected_ctx.merge(&ctx);
            out.push(s);
        }
        out
    } else {
        Vec::new()
    };

    // `__ignoreNulls = true` (CppNix): attrs whose value is null are dropped
    // from the env, and `__ignoreNulls` itself is consumed (never emitted).
    // Every stdenv package sets this, so without it every stdenv drv env
    // carried an extra `__ignoreNulls` + any null attr (e.g. `userHook`),
    // diverging the modulo hash.
    let ignore_nulls = input
        .get("__ignoreNulls")
        .and_then(|v| crate::eval::force_value(v).ok())
        .is_some_and(|v| matches!(v, Value::Bool(true)));

    // `__structuredAttrs = true` fundamentally changes the env: instead of each
    // attr coerced to a flat env var, CppNix collapses ALL attrs into a single
    // `__json` env var (a typed JSON object — lists stay lists, bools stay bools,
    // derivations become their outPath) and drops the flat vars. name/builder/
    // system/outputs go INTO the JSON; `args` and `__structuredAttrs` do not;
    // the per-output env vars (out/dev/…) are added later by output-filling.
    let structured_attrs = input
        .get("__structuredAttrs")
        .and_then(|v| crate::eval::force_value(v).ok())
        .is_some_and(|v| matches!(v, Value::Bool(true)));

    let mut env_vars: BTreeMap<String, String> = BTreeMap::new();
    if structured_attrs {
        let mut obj = serde_json::Map::new();
        for (k, v) in input.iter() {
            if matches!(
                k.as_str(),
                "args" | "__structuredAttrs" | "__ignoreNulls" | "__impure" | "__contentAddressed"
            ) {
                continue;
            }
            let forced_v = match crate::eval::force_value(v) {
                Ok(v) => v,
                Err(_e) => {
                    // SUI_PARITY_STRICT: un-blind the structured-attrs drop too.
                    parity_strict::record(
                        &name,
                        &k,
                        parity_strict::DropSite::StructuredAttrs,
                        &format!("{_e:?}"),
                    );
                    continue;
                }
            };
            // `__ignoreNulls` drops null attrs from the JSON too (every stdenv
            // mkDerivation sets it) — without this, a null attr like `userHook`
            // leaks into `__json` and diverges the hash.
            if ignore_nulls && matches!(forced_v, Value::Null) {
                continue;
            }
            // The `Err` arm used to have no `else`: a value that forced fine
            // but could not be JSON-encoded vanished from `__json` with no
            // record anywhere — not in the ledger, not behind SUI_DEBUG_DRV.
            //
            // That is the exact structural sibling of the flat-env coerce-none
            // hole closed below (see the `None =>` arm), and it matters more,
            // not less: `__structuredAttrs` is set by every modern
            // `mkDerivation`, `__json` is an env var the drv hashes over, and a
            // dropped key changes the hash silently. A strict run reported
            // "clean eval" while this path was discarding attrs.
            match forced_v.to_json_with_context(&mut collected_ctx) {
                Ok(jv) => {
                    obj.insert(k.clone(), jv);
                }
                Err(e) => {
                    if std::env::var_os("SUI_DEBUG_DRV").is_some() {
                        eprintln!(
                            "[SUI_DEBUG_DRV] drv={name} attr={k} JSON-ENCODE-ERR type={} err={e:?}",
                            forced_v.type_name()
                        );
                    }
                    parity_strict::record(
                        &name,
                        &k,
                        parity_strict::DropSite::StructuredAttrs,
                        &format!("json-encode-err type={} {e:?}", forced_v.type_name()),
                    );
                }
            }
        }
        let json = serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or_default();
        env_vars.insert("__json".to_string(), json);
    } else {
        // Feeds `env_vars: BTreeMap` which self-sorts, so the sorted `iter()`
        // is redundant with the BTreeMap's own ordering — byte-neutral.
        for (k, v) in input.iter_unsorted() {
            // Excluded from env: `name`/`system`/`builder` (re-inserted below from
            // the coerced locals); `args` (structural, not an env var); the control
            // flags CppNix consumes rather than emits (`__ignoreNulls`, `__impure`,
            // `__contentAddressed`).  NOT excluded: `outputs` (coerced to
            // "out bin dev") and `__structuredAttrs` (coerced to "" for a
            // non-structured drv) — CppNix emits both, and dropping either diverged
            // the modulo hash.
            if matches!(
                k.as_str(),
                "name" | "system" | "builder" | "args"
                    | "__ignoreNulls" | "__impure" | "__contentAddressed"
            ) {
                continue;
            }
            // NOTE (2026-07-10): this force-error path is DELIBERATELY still a
            // best-effort skip, NOT a surfaced error, even after the
            // overlay-fixpoint promotion fix.  The promotion lands the real
            // partial for the native-system fixpoint (`libxcrypt` — the perl
            // dep is no longer dropped, sui now byte-matches nix jb9k6090…),
            // but on cross-system paths the promoted empty-attrs partial can
            // still fail to coerce in a dep position that nix itself tolerates
            // by dropping.  Surfacing the error there regresses `libxcrypt`
            // ITSELF back to a `throw` and turns a dozen clean value-diverges
            // into hard errors (measured).  The correct terminal fix is the
            // materialized-attrset-with-keys partial (so the coerce succeeds),
            // which is a larger architectural change than this increment; until
            // then the skip stays.  `SUI_DEBUG_DRV` surfaces the dropped attr
            // for diagnosis without changing the result.
            let forced_v = match crate::eval::force_value(v) {
                Ok(v) => v,
                Err(_e) => {
                    if std::env::var_os("SUI_DEBUG_DRV").is_some() {
                        eprintln!("[SUI_DEBUG_DRV] drv={name} attr={k} FORCE-ERR: {_e:?}");
                    }
                    // SUI_PARITY_STRICT: un-blind this dropped dep for
                    // enumeration. No-op unless strict; the `continue` below is
                    // unchanged, so the default drvPath outcome is identical.
                    parity_strict::record(
                        &name,
                        &k,
                        parity_strict::DropSite::FlatEnv,
                        &format!("{_e:?}"),
                    );
                    continue;
                }
            };
            if ignore_nulls && matches!(forced_v, Value::Null) {
                continue;
            }
            // A genuine throw/abort/assert (e.g. check-meta's unsupported-
            // platform assertion on a linux-only build input) PROPAGATES here
            // via `?`, matching CppNix's derivationStrict; a benign coerce-none
            // (empty-attrs partial = TypeError) drops the attr as before.
            match coerce_drv_value_to_string_opt_with_context(&forced_v)? {
                Some((s, ctx)) => {
                    collected_ctx.merge(&ctx);
                    env_vars.insert(k.clone(), s);
                }
                None => {
                    if std::env::var_os("SUI_DEBUG_DRV").is_some() {
                        eprintln!("[SUI_DEBUG_DRV] drv={name} attr={k} COERCE-NONE type={}", forced_v.type_name());
                    }
                    // Record the drop for SUI_PARITY_STRICT — previously only the
                    // FORCE-ERR path recorded, so a strict run never enumerated a
                    // coerce-none drop (the fan-out's secondary gap).
                    parity_strict::record(
                        &name,
                        &k,
                        parity_strict::DropSite::FlatEnv,
                        &format!("coerce-none type={}", forced_v.type_name()),
                    );
                }
            }
        }
        env_vars.insert("name".to_string(), name.clone());
        env_vars.insert("system".to_string(), system.clone());
        env_vars.insert("builder".to_string(), builder.clone());
    }

    // Fold context into input_derivations / input_sources.
    let mut input_derivations: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut input_sources: Vec<String> = Vec::new();
    for elem in collected_ctx.iter() {
        match elem {
            ContextElement::Output { drv, output } => {
                input_derivations
                    .entry(drv.to_string())
                    .or_default()
                    .push(output.to_string());
            }
            ContextElement::Plain(p) => {
                let s = p.to_string();
                if s.starts_with("/nix/store/") && !input_sources.contains(&s) {
                    input_sources.push(s);
                }
            }
            _ => {}  // `DrvDeep` etc. not consumed into these fields
        }
    }
    // Deduplicate + sort output names per drv (CppNix canonicalizes).
    for outs in input_derivations.values_mut() {
        outs.sort();
        outs.dedup();
    }

    let drv = sui_compat::derivation::Derivation {
        outputs: BTreeMap::new(),
        input_derivations,
        input_sources,
        system,
        builder,
        args: args_list,
        env: env_vars,
    };

    Ok((name, drv))
}

/// Compute .drv path and output paths (handles both FOD and input-addressed).
fn compute_derivation_outputs(
    input: &NixAttrs,
    name: &str,
    mut drv: sui_compat::derivation::Derivation,
) -> Result<(String, std::collections::BTreeMap<String, String>, sui_compat::derivation::Derivation), EvalError> {
    use std::collections::BTreeMap;
    use sui_compat::derivation::DerivationOutput;

    let is_fod = input.contains_key("outputHash");

    if is_fod {
        let raw_output_hash = force_attr_string(input, "outputHash")?;
        let raw_algo = optional_attr_string(input, "outputHashAlgo")?
            .unwrap_or_default();
        let output_hash_mode = optional_attr_string(input, "outputHashMode")?
            .unwrap_or_else(|| "flat".to_string());
        let is_recursive = output_hash_mode == "recursive" || output_hash_mode == "nar";

        // CppNix accepts empty `outputHashAlgo` when the hash is
        // in SRI format — the algo is inferred from the `<algo>-`
        // prefix.  Fall back to the SRI prefix; otherwise sha256.
        let output_hash_algo = if raw_algo.is_empty() {
            infer_algo_from_hash(&raw_output_hash).unwrap_or_else(|| "sha256".to_string())
        } else {
            raw_algo
        };

        // Normalize user-supplied hash (hex / nix-base32 / SRI) to
        // lowercase hex — `compute_fixed_output_hash` documents
        // that its `hash` parameter is hex.  Passing the raw user
        // string broke FOD outPath parity with CppNix when the
        // operator wrote nix-base32 (the most common case for
        // `outputHash` literals).  See sui-compat::hash::NixHash::parse_any.
        let algo = sui_compat::hash::HashAlgorithm::from_nix_str(&output_hash_algo)
            .map_err(|e| EvalError::TypeError(
                format!("derivation: invalid outputHashAlgo {output_hash_algo:?}: {e}"),
            ))?;
        let parsed = sui_compat::hash::NixHash::parse_any(algo, &raw_output_hash)
            .map_err(|e| EvalError::TypeError(
                format!("derivation: invalid outputHash {raw_output_hash:?}: {e}"),
            ))?;
        let output_hash_hex = parsed.to_hex();

        let out_path = sui_compat::store_path::compute_fixed_output_hash(
            &output_hash_algo, &output_hash_hex, is_recursive, name,
        );

        drv.outputs.insert("out".to_string(), DerivationOutput {
            path: out_path.clone(),
            hash_algo: if is_recursive { format!("r:{output_hash_algo}") } else { output_hash_algo.clone() },
            hash: output_hash_hex.clone(),
        });

        // CppNix hashes the FOD with `env["out"] = <out-path>` present (the
        // input-addressed spec's FillOutputs phase sets it; this hand-rolled
        // fixed-output branch skipped it) — without it the FOD drvPath diverges
        // from nix while its outPath already matches.
        drv.env.insert("out".to_string(), out_path.clone());

        let drv_content = drv.serialize();
        // Fold the .drv's references (its inputDrvs + inputSrcs) into the store
        // path — CppNix's makeTextPath does this for EVERY derivation, including
        // fixed-output ones. A fetchurl FOD consumes curl / mirrors-list / stdenv
        // as inputDrvs, so without the refs its .drv path diverges from nix. (A
        // bare FOD with no inputs has an empty ref set, so the simple FOD case
        // matched even while this hid — it took a FOD WITH inputDrvs to expose.)
        let drv_refs: Vec<String> = drv.input_derivations.keys().cloned()
            .chain(drv.input_sources.iter().cloned())
            .collect();
        let drv_path = sui_compat::store_path::compute_drv_path_with_refs(
            drv_content.as_bytes(), name, &drv_refs);


        // CppNix `hashDerivationModulo` for a FIXED-OUTPUT derivation is NOT the
        // input-addressed ATerm hash — it is the special
        // sha256("fixed:out:<methodAlgo>:<hashHex>:<outPath>"). Cache it against
        // this FOD's drv path so every input-addressed derivation that consumes
        // this FOD substitutes the correct modulo hash. Without it `modulo_of`
        // returns the FOD's drv path unchanged, so the consumer's output path —
        // and everything transitively above it (all of nixpkgs, whose stdenv
        // bootstrap consumes fetchurl FODs) — diverges from nix even though the
        // FOD's own drv path already matches.
        let method_algo = if is_recursive {
            format!("r:{output_hash_algo}")
        } else {
            output_hash_algo.clone()
        };
        let modulo_preimage = format!("fixed:out:{method_algo}:{output_hash_hex}:{out_path}");
        let modulo_hex: String = {
            use sha2::{Digest, Sha256};
            Sha256::digest(modulo_preimage.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
        };
        if std::env::var_os("SUI_DUMP_MODULO").is_some() {
            eprintln!("[SUI_DUMP_MODULO] (FOD) {drv_path} = {modulo_hex}");
        }
        sui_spec::derivation::remember_modulo_hash(&drv_path, &modulo_hex);

        let mut out_paths = BTreeMap::new();
        out_paths.insert("out".to_string(), out_path);
        Ok((drv_path, out_paths, drv))
    } else {
        // Input-addressed derivation path computation is now spec-
        // driven: the algorithm lives in `sui-spec/specs/derivation.lisp`
        // and is interpreted by `sui_spec::derivation::apply`.
        //
        // Why: four bugs we fixed earlier this session (#11–#14) were
        // all *specification* mistakes — "mask env entries whose
        // names match outputs", "hash the final form for .drv-path",
        // etc.  Each bug existed in two copies (this file + the VM)
        // and drifted independently.  Moving the algorithm into a
        // single Lisp-authored spec eliminates the drift surface:
        // both engines call exactly the function below, fed by
        // exactly the same spec file.
        let outputs = parse_outputs_list(input)?;
        let algo = sui_spec::derivation::load_canonical()
            .map_err(|e| EvalError::TypeError(
                format!("derivation algorithm spec failed to load: {e}")
            ))?;
        sui_spec::derivation::apply(&algo, drv, outputs, name)
            .map_err(|e| EvalError::TypeError(
                format!("derivation algorithm interpreter failed: {e}")
            ))
    }
}

/// Parse the optional `outputs` attribute (defaults to `["out"]`).
/// Infer the hash algorithm from an SRI-formatted hash string
/// (`<algo>-<base64>`).  Returns `None` for hex / nix-base32 input
/// since those don't carry algo metadata.
fn infer_algo_from_hash(hash: &str) -> Option<String> {
    for algo in ["sha256", "sha512", "sha1", "md5"] {
        if hash.starts_with(&format!("{algo}-")) {
            return Some(algo.to_string());
        }
    }
    None
}

fn parse_outputs_list(input: &NixAttrs) -> Result<Vec<String>, EvalError> {
    if let Some(o) = input.get("outputs") {
        let forced_o = crate::eval::force_value(o)?;
        let list = forced_o.as_list()?;
        let mut out = Vec::with_capacity(list.len());
        for item in list {
            let s = crate::eval::force_value(item)?
                .as_string()
                .map_err(|_| EvalError::TypeError("derivation: outputs entries must be strings".into()))?
                .to_string();
            out.push(s);
        }
        if out.is_empty() {
            return Err(EvalError::TypeError("derivation: outputs list must not be empty".into()));
        }
        Ok(out)
    } else {
        Ok(vec!["out".to_string()])
    }
}

/// Write the final .drv file to the store (with fallback for permission errors).
fn write_derivation_to_store(
    drv_path: &str,
    out_paths: &std::collections::BTreeMap<String, String>,
    drv: &mut sui_compat::derivation::Derivation,
) -> Result<(), EvalError> {
    for (output_name, output_path) in out_paths {
        if let Some(output) = drv.outputs.get_mut(output_name)
            && output.path.is_empty() {
                output.path.clone_from(output_path);
            }
        drv.env.insert(output_name.clone(), output_path.clone());
    }

    let drv_content_final = drv.serialize();

    let store_dir = std::env::var("SUI_STORE_DIR")
        .unwrap_or_else(|_| "/nix/store".to_string());
    let disk_path = if store_dir != "/nix/store" {
        drv_path.replacen("/nix/store", &store_dir, 1)
    } else {
        drv_path.to_string()
    };

    let drv_file = std::path::Path::new(&disk_path);
    if !drv_file.exists() {
        if let Some(parent) = drv_file.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        match std::fs::write(drv_file, drv_content_final.as_bytes()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let fallback_dir = std::env::temp_dir().join("sui-drv-cache");
                std::fs::create_dir_all(&fallback_dir).ok();
                let fallback_path = fallback_dir.join(drv_file.file_name().unwrap_or_default());
                if let Err(e2) = std::fs::write(&fallback_path, drv_content_final.as_bytes()) {
                    tracing::warn!("failed to write .drv to both {} and {}: {e}, {e2}", drv_path, fallback_path.display());
                } else {
                    tracing::debug!("wrote .drv to fallback: {}", fallback_path.display());
                }
            }
            Err(e) => {
                return Err(EvalError::IoError {
                    context: format!("writing derivation {drv_path}"),
                    message: e.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Assemble the result attrset from computed derivation paths.
///
/// Every path-valued string built here carries a [`StringContext`]
/// so that downstream coercions (another derivation's `args` or
/// env-var interpolations) can rediscover the producing drv/output
/// and populate `input_derivations` correctly.  Without this
/// context, `${pkg}` interpolation would silently lose the
/// dependency edge — which is exactly the bug that made transitive
/// derivations disagree with CppNix on drvPath.
fn build_derivation_result(
    input: &NixAttrs,
    name: &str,
) -> Result<Value, EvalError> {
    use crate::value::{NixString, StringContext};

    // Shared, memoized handle to the (expensive, dep-forcing) drv computation.
    // The FIRST time any store-path field (`drvPath` / `outPath` / an output's
    // path / `all`) is forced, `compute_full_drv` runs once and caches its
    // result here; every other field reuses it.  This is the whole point of the
    // laziness: forcing the derivation VALUE to WHNF touches none of this, so a
    // mutually-referential package set never forces its dependency graph until a
    // store path is actually demanded — matching nix, where `derivation attrs`
    // returns an attrset with a lazy `.drvPath`.
    let computed: Rc<std::cell::OnceCell<Rc<ComputedDrv>>> =
        Rc::new(std::cell::OnceCell::new());
    let input_shared = Rc::new(input.clone());
    let name_shared: Rc<str> = Rc::from(name);
    let get_computed = {
        let computed = computed.clone();
        let input_shared = input_shared.clone();
        let name_shared = name_shared.clone();
        move || -> Result<Rc<ComputedDrv>, EvalError> {
            if let Some(c) = computed.get() {
                return Ok(c.clone());
            }
            let c = Rc::new(compute_full_drv(&input_shared, &name_shared)?);
            // `OnceCell::set` fails only if already set by a re-entrant force;
            // in that case prefer the already-cached value.
            let _ = computed.set(c.clone());
            Ok(computed.get().cloned().unwrap_or(c))
        }
    };

    // `.drvPath`: a lazy native thunk that computes the drv on first force and
    // returns the drv-path string with deep-drv context.
    let drv_path_thunk = {
        let get_computed = get_computed.clone();
        Value::Thunk(crate::value::Thunk::new_native(move || {
            let c = get_computed()?;
            let mut ctx = StringContext::new();
            ctx.add_drv_deep(c.drv_path.clone());
            Ok(Value::String(Rc::new(NixString::with_context(&c.drv_path, ctx))))
        }))
    };

    // The default output name comes from the `outputs` INPUT list (cheap; no
    // dep forcing) — CppNix: the FIRST declared output, defaulting to "out".
    let primary_output_name = parse_outputs_list(input)?
        .into_iter()
        .next()
        .unwrap_or_else(|| "out".to_string());

    // `.outPath`: lazy — the primary output's store path with output context.
    let out_path_thunk = {
        let get_computed = get_computed.clone();
        let primary = primary_output_name.clone();
        Value::Thunk(crate::value::Thunk::new_native(move || {
            let c = get_computed()?;
            let p = c.out_paths.get(&primary).cloned().unwrap_or_default();
            let mut ctx = StringContext::new();
            ctx.add_output(c.drv_path.clone(), primary.clone());
            Ok(Value::String(Rc::new(NixString::with_context(&p, ctx))))
        }))
    };

    let mut result = (*input_shared).clone();
    result.insert("type".to_string(), Value::string("derivation"));
    result.insert("drvPath".to_string(), drv_path_thunk.clone());
    // CppNix: drvAttrs contains the original input attributes
    result.insert("drvAttrs".to_string(), Value::Attrs(input_shared.clone()));
    result.insert("outPath".to_string(), out_path_thunk);
    result.insert("outputName".to_string(), Value::string(primary_output_name.clone()));

    // Per-output attrsets + `all`.  The OUTPUT NAMES are known from the input
    // `outputs` list (no dep forcing); each output's `outPath` is lazy.
    let output_names = parse_outputs_list(input)?;
    let output_names = if output_names.is_empty() {
        vec!["out".to_string()]
    } else {
        output_names
    };
    let mut all_outputs: Vec<Value> = Vec::new();
    for output_name in &output_names {
        let mut out_attrs = NixAttrs::new();
        let out_path_field = {
            let get_computed = get_computed.clone();
            let output_name = output_name.clone();
            Value::Thunk(crate::value::Thunk::new_native(move || {
                let c = get_computed()?;
                let p = c.out_paths.get(&output_name).cloned().unwrap_or_default();
                let mut ctx = StringContext::new();
                ctx.add_output(c.drv_path.clone(), output_name.clone());
                Ok(Value::String(Rc::new(NixString::with_context(&p, ctx))))
            }))
        };
        out_attrs.insert("outPath".to_string(), out_path_field);
        out_attrs.insert("drvPath".to_string(), drv_path_thunk.clone());
        out_attrs.insert("type".to_string(), Value::string("derivation"));
        out_attrs.insert("outputName".to_string(), Value::string(output_name.clone()));
        out_attrs.insert("name".to_string(), Value::string(&*name_shared));
        let out_val = Value::Attrs(Rc::new(out_attrs));
        all_outputs.push(out_val.clone());
        result.insert(output_name.clone(), out_val);
    }

    // CppNix: `all` is a list of all output derivation attrsets
    result.insert("all".to_string(), Value::List(Rc::new(NixList::new(all_outputs))));

    Ok(Value::Attrs(Rc::new(result)))
}
