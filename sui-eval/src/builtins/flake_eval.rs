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
                    input_val.insert("sourceInfo".to_string(), Value::Attrs(source_info));
                }

                let is_flake = node.flake.unwrap_or(true);
                if is_flake {
                    let input_dir = std::path::Path::new(&out_path);
                    if input_dir.join("flake.nix").exists() {
                        let immediate = input_val;
                        let dir = input_dir.to_path_buf();
                        let thunk = Thunk::new_native(move || {
                            let mut merged = immediate;
                            if let Ok(flake_result) = evaluate_flake(&dir) {
                                if let Value::Attrs(ref flake_out_attrs) = flake_result {
                                    for (k, v) in flake_out_attrs.iter() {
                                        if !merged.contains_key(k) {
                                            merged.insert(k.clone(), v.clone());
                                        }
                                    }
                                }
                            }
                            Ok(Value::Attrs(merged))
                        });
                        resolved_inputs.insert(input_name, Value::Thunk(thunk));
                        continue;
                    }
                }

                resolved_inputs.insert(input_name, Value::Attrs(input_val));
            }
        }

    // 4b. Fill in stub entries for declared-but-unresolved inputs.
    if let Some(inputs_value) = flake_attrs.get("inputs")
        && let Ok(inputs_forced) = crate::eval::force_value(inputs_value)
            && let Value::Attrs(declared_inputs) = inputs_forced {
                for key in declared_inputs.keys() {
                    if !resolved_inputs.contains_key(key) {
                        let mut stub = NixAttrs::new();
                        stub.insert(
                            "outPath".to_string(),
                            Value::string(format!("/nix/store/flake-input-{key}")),
                        );
                        resolved_inputs.insert(key.clone(), Value::Attrs(stub));
                    }
                }
            }

    // 5. Build `self`.
    let mut self_attrs = NixAttrs::new();
    self_attrs.insert("outPath".to_string(), Value::string(self_path.clone()));
    self_attrs.insert("sourceInfo".to_string(), Value::Attrs(NixAttrs::new()));
    self_attrs.insert("inputs".to_string(), Value::Attrs(resolved_inputs.clone()));
    for (k, v) in flake_attrs.iter() {
        if k != "outputs" && k != "inputs" {
            self_attrs.insert(k.clone(), v.clone());
        }
    }

    // 6. Build arguments for `outputs`.
    let mut outputs_args = NixAttrs::new();
    outputs_args.insert("self".to_string(), Value::Attrs(self_attrs));
    for (k, v) in resolved_inputs.iter() {
        outputs_args.insert(k.clone(), v.clone());
    }

    // 7. Call outputs(args).
    let result = crate::eval::apply(outputs_fn, Value::Attrs(outputs_args))?;
    let result = crate::eval::force_value(&result)?;

    // 8. Build the final flake value.
    let mut final_attrs = NixAttrs::new();
    final_attrs.insert("outPath".to_string(), Value::string(self_path));
    final_attrs.insert("sourceInfo".to_string(), Value::Attrs(NixAttrs::new()));
    final_attrs.insert("inputs".to_string(), Value::Attrs(resolved_inputs));

    for (k, v) in flake_attrs.iter() {
        if k != "outputs" && !final_attrs.contains_key(k) {
            final_attrs.insert(k.clone(), v.clone());
        }
    }

    if let Value::Attrs(out_attrs) = result {
        for (k, v) in out_attrs.iter() {
            final_attrs.insert(k.clone(), v.clone());
        }
    }

    Ok(Value::Attrs(final_attrs))
}
