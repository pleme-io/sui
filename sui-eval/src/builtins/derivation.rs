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

pub fn build_derivation(arg: &Value) -> Result<Value, EvalError> {
    let forced = crate::eval::force_value(arg)?;
    let input_owned = forced.to_attrs()?;
    let input = &input_owned;

    // 1. Validate and extract derivation inputs.
    let (name, drv) = construct_derivation(input)?;

    // 2. Compute output paths and .drv path.
    let (drv_path, out_paths, mut drv) = compute_derivation_outputs(input, &name, drv)?;

    // 3. Write the .drv file to the store.
    write_derivation_to_store(&drv_path, &out_paths, &mut drv)?;

    // 4. Assemble the result attrset.
    build_derivation_result(input, &name, &drv_path, &out_paths)
}

/// Extract required/optional attributes and construct the Derivation skeleton.
fn construct_derivation(
    input: &NixAttrs,
) -> Result<(String, sui_compat::derivation::Derivation), EvalError> {
    use std::collections::BTreeMap;

    let name = force_attr_string(input, "name")?;
    let system = force_attr_string(input, "system")?;
    let builder = force_attr_string(input, "builder")?;

    let args_list: Vec<String> = if let Some(a) = input.get("args") {
        let forced_args = crate::eval::force_value(a)?;
        let list = forced_args.as_list()?;
        let mut out = Vec::with_capacity(list.len());
        for item in list {
            out.push(coerce_drv_value_to_string(item)?);
        }
        out
    } else {
        Vec::new()
    };

    let mut env_vars: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in input.iter() {
        if matches!(
            k.as_str(),
            "name" | "system" | "builder" | "args" | "outputs"
                | "__impure" | "__contentAddressed" | "__structuredAttrs"
        ) {
            continue;
        }
        let forced_v = match crate::eval::force_value(v) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(s) = coerce_drv_value_to_string_opt(&forced_v) {
            env_vars.insert(k.clone(), s);
        }
    }
    env_vars.insert("name".to_string(), name.clone());
    env_vars.insert("system".to_string(), system.clone());
    env_vars.insert("builder".to_string(), builder.clone());

    let drv = sui_compat::derivation::Derivation {
        outputs: BTreeMap::new(),
        input_derivations: BTreeMap::new(),
        input_sources: Vec::new(),
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
        let output_hash = force_attr_string(input, "outputHash")?;
        let output_hash_algo = optional_attr_string(input, "outputHashAlgo")?
            .unwrap_or_else(|| "sha256".to_string());
        let output_hash_mode = optional_attr_string(input, "outputHashMode")?
            .unwrap_or_else(|| "flat".to_string());
        let is_recursive = output_hash_mode == "recursive" || output_hash_mode == "nar";

        let out_path = sui_compat::store_path::compute_fixed_output_hash(
            &output_hash_algo, &output_hash, is_recursive, name,
        );

        drv.outputs.insert("out".to_string(), DerivationOutput {
            path: out_path.clone(),
            hash_algo: if is_recursive { format!("r:{output_hash_algo}") } else { output_hash_algo.clone() },
            hash: output_hash,
        });

        let drv_content = drv.serialize();
        let drv_path = sui_compat::store_path::compute_drv_path(drv_content.as_bytes(), name);
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
fn build_derivation_result(
    input: &NixAttrs,
    name: &str,
    drv_path: &str,
    out_paths: &std::collections::BTreeMap<String, String>,
) -> Result<Value, EvalError> {
    let mut result = input.clone();
    result.insert("type".to_string(), Value::string("derivation"));
    result.insert("drvPath".to_string(), Value::string(drv_path));

    // CppNix: drvAttrs contains the original input attributes
    result.insert("drvAttrs".to_string(), Value::Attrs(Rc::new(input.clone())));

    let primary_out = out_paths
        .get("out")
        .cloned()
        .or_else(|| out_paths.values().next().cloned())
        .unwrap_or_default();
    result.insert("outPath".to_string(), Value::string(primary_out));

    // CppNix: outputName is the primary output name
    let primary_output_name = if out_paths.contains_key("out") {
        "out"
    } else {
        out_paths.keys().next().map(|s| s.as_str()).unwrap_or("out")
    };
    result.insert("outputName".to_string(), Value::string(primary_output_name));

    // Build per-output attrsets and collect them for `all`
    let mut all_outputs: Vec<Value> = Vec::new();
    for (output_name, output_path) in out_paths {
        let mut out_attrs = NixAttrs::new();
        out_attrs.insert("outPath".to_string(), Value::string(output_path.clone()));
        out_attrs.insert("drvPath".to_string(), Value::string(drv_path));
        out_attrs.insert("type".to_string(), Value::string("derivation"));
        out_attrs.insert("outputName".to_string(), Value::string(output_name.clone()));
        out_attrs.insert("name".to_string(), Value::string(name));
        let out_val = Value::Attrs(Rc::new(out_attrs));
        all_outputs.push(out_val.clone());
        result.insert(output_name.clone(), out_val);
    }

    // CppNix: `all` is a list of all output derivation attrsets
    result.insert("all".to_string(), Value::List(Rc::new(all_outputs)));

    Ok(Value::Attrs(Rc::new(result)))
}
