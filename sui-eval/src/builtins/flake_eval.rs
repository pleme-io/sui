//! Flake evaluation pipeline.
//!
//! Implements the in-process equivalent of `nix eval --raw '(builtins.getFlake
//! "<dir>")'` for path-based flake references.

use crate::value::*;

thread_local! {
    pub(crate) static FLAKE_EVAL_DEPTH: std::cell::RefCell<u32> = const { std::cell::RefCell::new(0) };
}

pub(crate) const MAX_FLAKE_EVAL_DEPTH: u32 = 50;

/// Evaluate a flake directory — reads flake.nix, parses flake.lock, resolves
/// inputs, calls `outputs(inputs)`, and returns the merged result attrset.
///
/// This is the native implementation of `builtins.getFlake` for path-based
/// references.  External callers (orchestrate, CLI) can use this to evaluate
/// a local flake without shelling out to `nix eval`.
pub fn evaluate_flake(flake_dir: &std::path::Path) -> Result<Value, EvalError> {
    let depth = FLAKE_EVAL_DEPTH.with(|d| {
        let mut d = d.borrow_mut();
        *d += 1;
        *d
    });

    if depth > MAX_FLAKE_EVAL_DEPTH {
        FLAKE_EVAL_DEPTH.with(|d| *d.borrow_mut() -= 1);
        return Err(EvalError::RecursionLimit(
            format!(
                "maximum flake evaluation depth ({MAX_FLAKE_EVAL_DEPTH}) exceeded at {}",
                flake_dir.display()
            ),
        ));
    }

    let result = evaluate_flake_inner(flake_dir);
    FLAKE_EVAL_DEPTH.with(|d| *d.borrow_mut() -= 1);
    result
}

fn evaluate_flake_inner(flake_dir: &std::path::Path) -> Result<Value, EvalError> {
    let flake_nix = flake_dir.join("flake.nix");
    let flake_lock_path = flake_dir.join("flake.lock");

    // 1. Read and evaluate flake.nix.
    let source = std::fs::read_to_string(&flake_nix).map_err(|e| {
        EvalError::IoError {
            context: format!("getFlake: {}", flake_nix.display()),
            message: e.to_string(),
        }
    })?;
    let _flake_file_guard = crate::eval::push_eval_file(flake_nix.clone());
    let flake_value = crate::eval::eval_with_file(&source, Some(flake_nix.clone()))?;
    let flake_attrs = flake_value.to_attrs()?.clone();

    // 2. Pull out the outputs function (required by every flake).
    let outputs_value = flake_attrs
        .get("outputs")
        .ok_or_else(|| EvalError::AttrNotFound("outputs".into()))?
        .clone();
    let outputs_fn = crate::eval::force_value(&outputs_value)?;

    // 3. Parse flake.lock if it exists.
    let lock = if flake_lock_path.exists() {
        let lock_content = std::fs::read_to_string(&flake_lock_path).map_err(|e| {
            EvalError::IoError {
                context: format!("getFlake: {}", flake_lock_path.display()),
                message: e.to_string(),
            }
        })?;
        Some(
            sui_compat::flake::FlakeLock::parse(&lock_content)
                .map_err(|e| EvalError::TypeError(format!("getFlake: invalid flake.lock: {e}")))?,
        )
    } else {
        None
    };

    // 3b. Create the content-addressed input fetcher.
    let fetcher = crate::fetcher::InputFetcher::new();

    // 4. Resolve every direct input.
    let self_path = flake_dir.to_string_lossy().to_string();
    let mut resolved_inputs = NixAttrs::new();

    if let Some(ref lock) = lock
        && let Ok(root_node) = lock.root_node() {
            let input_names: Vec<String> = root_node.inputs.keys().cloned().collect();
            for input_name in input_names {
                let segments = [input_name.as_str()];
                let Ok(node) = lock.resolve_input(&segments) else {
                    continue;
                };

                let mut input_val = NixAttrs::new();

                let out_path = if let Some(ref locked) = node.locked {
                    if locked.source_type == "path" {
                        locked.path.clone().unwrap_or_default()
                    } else {
                        match fetcher.fetch(locked) {
                            Ok(fetched_path) => fetched_path.to_string_lossy().to_string(),
                            Err(e) => {
                                return Err(EvalError::IoError {
                                    context: format!("fetch flake input '{input_name}'"),
                                    message: e.to_string(),
                                });
                            }
                        }
                    }
                } else {
                    format!("/nix/store/flake-input-{input_name}")
                };
                input_val.insert("outPath".to_string(), Value::string(out_path.clone()));

                if let Some(ref locked) = node.locked {
                    if let Some(ref rev) = locked.rev {
                        input_val.insert("rev".to_string(), Value::string(rev.clone()));
                        let short: String = rev.chars().take(7).collect();
                        input_val.insert("shortRev".to_string(), Value::string(short));
                    }
                    if let Some(ref nar_hash) = locked.nar_hash {
                        input_val.insert(
                            "narHash".to_string(),
                            Value::string(nar_hash.clone()),
                        );
                    }
                    if let Some(last_modified) = locked.last_modified {
                        input_val.insert(
                            "lastModified".to_string(),
                            Value::Int(last_modified as i64),
                        );
                    }

                    let mut source_info = NixAttrs::new();
                    source_info.insert("outPath".to_string(), Value::string(out_path.clone()));
                    if let Some(ref rev) = locked.rev {
                        source_info.insert("rev".to_string(), Value::string(rev.clone()));
                    }
                    if let Some(ref nar_hash) = locked.nar_hash {
                        source_info.insert(
                            "narHash".to_string(),
                            Value::string(nar_hash.clone()),
                        );
                    }
                    if let Some(last_modified) = locked.last_modified {
                        source_info.insert(
                            "lastModified".to_string(),
                            Value::Int(last_modified as i64),
                        );
                    }
                    input_val.insert("sourceInfo".to_string(), Value::Attrs(Rc::new(source_info)));
                }

                let is_flake = node.flake.unwrap_or(true);
                if is_flake {
                    let input_dir = std::path::Path::new(&out_path);
                    if input_dir.join("flake.nix").exists() {
                        let immediate = input_val;
                        let dir = input_dir.to_path_buf();
                        let thunk = Thunk::new_native(move || {
                            let mut merged = immediate;
                            let flake_result = evaluate_flake(&dir)?;
                            if let Value::Attrs(ref flake_out_attrs) = flake_result {
                                for (k, v) in flake_out_attrs.iter() {
                                    if !merged.contains_key(&k) {
                                        merged.insert(k.clone(), v.clone());
                                    }
                                }
                            }
                            Ok(Value::Attrs(Rc::new(merged)))
                        });
                        resolved_inputs.insert(input_name, Value::Thunk(thunk));
                        continue;
                    }
                }

                resolved_inputs.insert(input_name, Value::Attrs(Rc::new(input_val)));
            }
        }

    // 4b. Fill in stub entries for declared-but-unresolved inputs.
    if let Some(inputs_value) = flake_attrs.get("inputs")
        && let Ok(inputs_forced) = crate::eval::force_value(inputs_value)
            && let Value::Attrs(declared_inputs) = inputs_forced {
                for key in declared_inputs.keys() {
                    if !resolved_inputs.contains_key(&key) {
                        let mut stub = NixAttrs::new();
                        stub.insert(
                            "outPath".to_string(),
                            Value::string(format!("/nix/store/flake-input-{key}")),
                        );
                        resolved_inputs.insert(key.clone(), Value::Attrs(Rc::new(stub)));
                    }
                }
            }

    // 5. Build `self`.
    let mut self_attrs = NixAttrs::new();
    self_attrs.insert("outPath".to_string(), Value::string(self_path.clone()));
    self_attrs.insert("sourceInfo".to_string(), Value::Attrs(Rc::new(NixAttrs::new())));
    self_attrs.insert("inputs".to_string(), Value::Attrs(Rc::new(resolved_inputs.clone())));
    for (k, v) in flake_attrs.iter() {
        if k != "outputs" && k != "inputs" {
            self_attrs.insert(k.clone(), v.clone());
        }
    }

    // 6. Build arguments for `outputs`.
    let mut outputs_args = NixAttrs::new();
    outputs_args.insert("self".to_string(), Value::Attrs(Rc::new(self_attrs)));
    for (k, v) in resolved_inputs.iter() {
        outputs_args.insert(k.clone(), v.clone());
    }

    // 7. Call outputs(args).
    let result = crate::eval::apply(outputs_fn, Value::Attrs(Rc::new(outputs_args)))?;
    let result = crate::eval::force_value(&result)?;

    // 8. Build the final flake value.
    //
    // Shape policy lives in `sui-spec/specs/flake.lisp` as a
    // `(defflake-shape :name "cppnix" …)` form.  We consult the
    // spec for the type marker, the spread-outputs rule, and the
    // never-leak denylist — so changes to CppNix's flake shape are
    // one-line Lisp edits, not Rust surgery.  (Previously this
    // function was the drift surface for leak bugs like the
    // `description`-at-top-level regression.)
    let shape = sui_spec::flake::load_canonical().map_err(|e| {
        EvalError::TypeError(format!("flake shape spec failed to load: {e}"))
    })?;
    let mut final_attrs = NixAttrs::new();
    final_attrs.insert("_type".to_string(), Value::string(shape.type_marker.clone()));
    final_attrs.insert("outPath".to_string(), Value::string(self_path));
    final_attrs.insert("sourceInfo".to_string(), Value::Attrs(Rc::new(NixAttrs::new())));
    final_attrs.insert("inputs".to_string(), Value::Attrs(Rc::new(resolved_inputs)));
    final_attrs.insert("outputs".to_string(), result.clone());

    if shape.spreads_output_fn() {
        if let Value::Attrs(out_attrs) = &result {
            for (k, v) in out_attrs.iter() {
                if !final_attrs.contains_key(k.as_str()) {
                    final_attrs.insert(k.clone(), v.clone());
                }
            }
        }
    }

    Ok(Value::Attrs(Rc::new(final_attrs)))
}

// ── Cached attribute evaluation ──────────────────────────────

/// Evaluate a flake and navigate to a specific attribute, with caching.
///
/// If the derivation path for `(lock_hash, source_hash, attr_path)` is already
/// in the drv cache, returns a synthetic derivation attrset without evaluating
/// the flake (near-zero memory). Otherwise, evaluates normally and caches the
/// result for future lookups.
pub fn evaluate_flake_attr(
    flake_dir: &std::path::Path,
    attr_path: &[&str],
) -> Result<Value, EvalError> {
    let lock_path = flake_dir.join("flake.lock");
    let source_path = flake_dir.join("flake.nix");

    // Compute cache keys from file content.
    let lock_hash = std::fs::read(&lock_path)
        .ok()
        .map(|c| crate::drv_cache::DrvCache::hash_bytes(&c));
    let source_hash = std::fs::read(&source_path)
        .ok()
        .map(|c| crate::drv_cache::DrvCache::hash_bytes(&c));
    let attr_key = attr_path.join(".");

    // Check drv cache.
    if let (Some(lh), Some(sh)) = (&lock_hash, &source_hash) {
        if let Some(entry) = crate::drv_cache::with_cache(|cache| cache.get(lh, sh, &attr_key)) {
            tracing::info!(
                attr_path = %attr_key,
                out_path = %entry.out_path,
                "drv cache hit — skipping full evaluation"
            );
            return Ok(synthetic_drv_value(&entry));
        }
    }

    // Cache miss — full evaluation.
    tracing::info!(attr_path = %attr_key, "drv cache miss — evaluating flake");
    let flake_result = evaluate_flake(flake_dir)?;

    // Navigate to the target attribute.
    let target = navigate_attr_path(&flake_result, attr_path)?;

    // If the result is a derivation, cache it.
    if let (Some(lh), Some(sh)) = (&lock_hash, &source_hash) {
        if let Value::Attrs(ref attrs) = target {
            let drv_path = attrs.get("drvPath").and_then(|v| v.as_string().ok());
            let out_path = attrs.get("outPath").and_then(|v| v.as_string().ok());
            if let (Some(dp), Some(op)) = (drv_path, out_path) {
                crate::drv_cache::with_cache_mut(|cache| {
                    let entry = crate::drv_cache::DrvCacheEntry {
                        drv_path: dp.to_string(),
                        out_path: op.to_string(),
                    };
                    if let Err(e) = cache.put(lh, sh, &attr_key, &entry) {
                        tracing::warn!(error = %e, "Failed to cache derivation path");
                    } else {
                        tracing::info!(attr_path = %attr_key, out_path = %op, "Cached derivation path");
                    }
                });
            }
        }
    }

    Ok(target)
}

/// Navigate an attribute path like `["packages", "x86_64-linux", "default"]`
/// through a Value, forcing thunks at each level.
fn navigate_attr_path(value: &Value, path: &[&str]) -> Result<Value, EvalError> {
    let mut current = crate::eval::force_value(value)?;
    for segment in path {
        let attrs = current.as_attrs().map_err(|_| {
            EvalError::TypeError(format!(
                "expected attrset at '.{segment}', got {}",
                current.type_name()
            ))
        })?;
        let next = attrs.get(*segment).ok_or_else(|| {
            EvalError::AttrNotFound((*segment).to_string())
        })?;
        current = crate::eval::force_value(next)?;
    }
    Ok(current)
}

/// Build a synthetic derivation Value from cached paths.
/// The caller only needs `drvPath`, `outPath`, and `type = "derivation"`.
fn synthetic_drv_value(entry: &crate::drv_cache::DrvCacheEntry) -> Value {
    let mut attrs = NixAttrs::new();
    attrs.insert("type".to_string(), Value::string("derivation"));
    attrs.insert("drvPath".to_string(), Value::string(entry.drv_path.clone()));
    attrs.insert("outPath".to_string(), Value::string(entry.out_path.clone()));
    // Extract name from store path: /nix/store/hash-name → name
    if let Some(name) = entry.out_path.rsplit('/').next().and_then(|b| b.split_once('-').map(|(_, n)| n)) {
        attrs.insert("name".to_string(), Value::string(name));
    }
    Value::Attrs(Rc::new(attrs))
}
