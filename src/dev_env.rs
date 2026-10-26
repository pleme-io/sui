use std::collections::BTreeMap;

use serde_json::Value;
use sui_compat::derivation::Derivation;

pub const GET_ENV_SH: &str = concat!("\n", include_str!("get-env.sh"));

pub const IGNORED_VARS: &[&str] = &[
    "BASHOPTS",
    "HOME",
    "NIX_BUILD_TOP",
    "NIX_ENFORCE_PURITY",
    "NIX_LOG_FD",
    "NIX_REMOTE",
    "PPID",
    "SHELLOPTS",
    "SSL_CERT_FILE",
    "TEMP",
    "TEMPDIR",
    "TERM",
    "TMP",
    "TMPDIR",
    "TZ",
    "UID",
];

const SAVED_VARS: &[&str] = &["PATH", "XDG_DATA_DIRS"];

#[must_use]
pub fn text_path(name: &str, text: &str) -> String {
    let hex = sui_compat::hash::NixHash::of(sui_compat::hash::HashAlgorithm::Sha256, text.as_bytes()).to_hex();
    sui_compat::store_path::compute_store_path_from_fingerprint(&format!("text:sha256:{hex}:/nix/store:{name}"), name)
}

#[must_use]
pub fn get_env_sh_path() -> String {
    text_path("get-env.sh", GET_ENV_SH)
}

fn drv_name(drv_path: &str) -> String {
    let base = drv_path.rsplit('/').next().unwrap_or(drv_path);
    let base = base.strip_suffix(".drv").unwrap_or(base);
    base.split_once('-').map_or(base, |(_, name)| name).to_string()
}

/// cppnix's `getDerivationEnvironment`: the derivation rebuilt to run
/// get-env.sh, with its checks removed and its outputs rehashed.
///
/// # Errors
/// A builder that is not bash, or an input derivation `load` cannot read.
pub fn env_derivation(
    drv_path: &str,
    drv: &Derivation,
    load: &mut impl FnMut(&str) -> Result<Derivation, sui_compat::derivation::DerivationError>,
) -> Result<(String, Derivation), String> {
    if drv.builder.rsplit('/').next() != Some("bash") {
        return Err("'nix develop' only works on derivations that use 'bash' as their builder".into());
    }
    let get_env = get_env_sh_path();
    let mut env_drv = drv.clone();
    env_drv.args = vec![get_env.clone()];
    for key in ["allowedReferences", "allowedRequisites", "disallowedReferences", "disallowedRequisites", "name"] {
        env_drv.env.remove(key);
    }
    let name = format!("{}-env", drv_name(drv_path));
    env_drv.env.insert("name".into(), name.clone());
    if !env_drv.input_sources.contains(&get_env) {
        env_drv.input_sources.push(get_env);
        env_drv.input_sources.sort();
    }
    for (output, out) in &mut env_drv.outputs {
        out.path.clear();
        env_drv.env.insert(output.clone(), String::new());
    }
    let hashes = sui_compat::derivation::output_hashes_modulo(&env_drv, true, load, &mut std::collections::HashMap::new())
        .map_err(|e| e.to_string())?;
    let outputs: Vec<String> = env_drv.outputs.keys().cloned().collect();
    for output in outputs {
        let path = sui_compat::store_path::compute_output_path(&hashes[&output], &output, &name);
        env_drv.env.insert(output.clone(), path.clone());
        if let Some(o) = env_drv.outputs.get_mut(&output) {
            o.path = path;
        }
    }
    let refs: Vec<String> = env_drv.input_derivations.keys().cloned().chain(env_drv.input_sources.iter().cloned()).collect();
    let env_drv_path = sui_compat::store_path::compute_drv_path_with_refs(env_drv.serialize().as_bytes(), &name, &refs);
    Ok((env_drv_path, env_drv))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Var {
    String { exported: bool, value: String },
    Array(Vec<String>),
    Associative(BTreeMap<String, String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildEnvironment {
    pub vars: BTreeMap<String, Var>,
    pub bash_functions: BTreeMap<String, String>,
    pub structured_attrs: Option<(String, String)>,
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

impl BuildEnvironment {
    /// # Errors
    /// Text that is not get-env.sh's JSON.
    pub fn parse(text: &str) -> Result<Self, String> {
        let json: Value = serde_json::from_str(text).map_err(|e| format!("get-env.sh output: {e}"))?;
        let mut vars = BTreeMap::new();
        if let Some(map) = json.get("variables").and_then(Value::as_object) {
            for (name, info) in map {
                let value = &info["value"];
                let var = match info["type"].as_str() {
                    Some(t @ ("var" | "exported")) => Var::String {
                        exported: t == "exported",
                        value: value.as_str().unwrap_or_default().to_string(),
                    },
                    Some("array") => Var::Array(strings(value)),
                    Some("associative") => Var::Associative(
                        value
                            .as_object()
                            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect())
                            .unwrap_or_default(),
                    ),
                    _ => continue,
                };
                vars.insert(name.clone(), var);
            }
        }
        let bash_functions = json
            .get("bashFunctions")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect())
            .unwrap_or_default();
        let structured_attrs = json.get("structuredAttrs").map(|s| {
            (
                s[".attrs.json"].as_str().unwrap_or_default().to_string(),
                s[".attrs.sh"].as_str().unwrap_or_default().to_string(),
            )
        });
        Ok(Self { vars, bash_functions, structured_attrs })
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        let mut vars = BTreeMap::new();
        for (name, var) in &self.vars {
            let info = match var {
                Var::String { exported, value } => {
                    serde_json::json!({ "type": if *exported { "exported" } else { "var" }, "value": value })
                }
                Var::Array(items) => serde_json::json!({ "type": "array", "value": items }),
                Var::Associative(map) => serde_json::json!({ "type": "associative", "value": map }),
            };
            vars.insert(name.clone(), info);
        }
        let mut out = BTreeMap::new();
        out.insert("bashFunctions", serde_json::to_value(&self.bash_functions).unwrap_or_default());
        if let Some((json, sh)) = &self.structured_attrs {
            out.insert("structuredAttrs", serde_json::json!({ ".attrs.json": json, ".attrs.sh": sh }));
        }
        out.insert("variables", serde_json::to_value(&vars).unwrap_or_default());
        serde_json::to_string(&out).unwrap_or_default()
    }

    #[must_use]
    pub fn to_bash(&self) -> String {
        let mut out = String::new();
        for (name, var) in &self.vars {
            if IGNORED_VARS.contains(&name.as_str()) {
                continue;
            }
            match var {
                Var::String { exported, value } => {
                    out.push_str(&format!("{name}={}\n", escape(value)));
                    if *exported {
                        out.push_str(&format!("export {name}\n"));
                    }
                }
                Var::Array(items) => {
                    out.push_str(&format!("declare -a {name}=("));
                    for item in items {
                        out.push_str(&format!("{} ", escape(item)));
                    }
                    out.push_str(")\n");
                }
                Var::Associative(map) => {
                    out.push_str(&format!("declare -A {name}=("));
                    for (k, v) in map {
                        out.push_str(&format!("[{}]={} ", escape(k), escape(v)));
                    }
                    out.push_str(")\n");
                }
            }
        }
        for (name, def) in &self.bash_functions {
            out.push_str(&format!("{name} ()\n{{\n{def}}}\n"));
        }
        out
    }

    fn string(&self, name: &str) -> Option<&str> {
        match self.vars.get(name)? {
            Var::String { value, .. } => Some(value),
            _ => None,
        }
    }

    /// cppnix's `makeRcScript`: the sourced script, with output paths moved
    /// to `outputs_dir` and structured-attrs files written to `tmp_dir`.
    ///
    /// # Errors
    /// A structured-attrs file that cannot be written.
    pub fn rc_script(&self, tmp_dir: &std::path::Path, outputs_dir: &std::path::Path) -> std::io::Result<String> {
        let mut out = String::from("unset shellHook\n");
        for var in SAVED_VARS {
            out.push_str(&format!("{var}=${{{var}:-}}\nnix_saved_{var}=\"${var}\"\n"));
        }
        out.push_str(&self.to_bash());
        for var in SAVED_VARS {
            out.push_str(&format!("{var}=\"${var}${{nix_saved_{var}:+:$nix_saved_{var}}}\"\n"));
        }
        out.push_str("export NIX_BUILD_TOP=\"$(mktemp -d -t nix-shell.XXXXXX)\"\n");
        for var in ["TMP", "TMPDIR", "TEMP", "TEMPDIR"] {
            out.push_str(&format!("export {var}=\"$NIX_BUILD_TOP\"\n"));
        }
        out.push_str("eval \"${shellHook:-}\"\n");

        let mut rewrites: BTreeMap<String, String> = BTreeMap::new();
        let dir = |name: &str| outputs_dir.join(name).to_string_lossy().into_owned();
        match self.vars.get("outputs") {
            Some(Var::Associative(map)) if self.structured_attrs.is_some() => {
                for (name, from) in map {
                    rewrites.insert(from.clone(), dir(name));
                }
            }
            Some(Var::String { value, .. }) => {
                for name in value.split_whitespace() {
                    if let Some(from) = self.string(name) {
                        rewrites.insert(from.to_string(), dir(name));
                    }
                }
            }
            Some(Var::Array(names)) => {
                for name in names {
                    if let Some(from) = self.string(name) {
                        rewrites.insert(from.to_string(), dir(name));
                    }
                }
            }
            Some(Var::Associative(map)) => {
                for name in map.keys() {
                    if let Some(from) = self.string(name) {
                        rewrites.insert(from.to_string(), dir(name));
                    }
                }
            }
            None => {}
        }
        if let Some((json, sh)) = &self.structured_attrs {
            for (ext, var, content) in [("sh", "NIX_ATTRS_SH_FILE", sh), ("json", "NIX_ATTRS_JSON_FILE", json)] {
                let target = tmp_dir.join(format!(".attrs.{ext}"));
                std::fs::write(&target, content)?;
                if let Some(from) = self.string(var) {
                    rewrites.insert(from.to_string(), target.to_string_lossy().into_owned());
                }
            }
        }
        for (from, to) in &rewrites {
            if from != to && !from.is_empty() {
                out = out.replace(from.as_str(), to);
            }
        }
        Ok(out)
    }
}

#[must_use]
pub fn escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_compat::derivation::DerivationOutput;

    #[test]
    fn get_env_sh_lands_at_cppnix_text_path() {
        assert_eq!(text_path("hello", "hello"), text_path("hello", "hello"));
        assert_eq!(get_env_sh_path(), "/nix/store/08i4419qr06fflq7qp20rxnrhw2c06vq-get-env.sh");
        assert_ne!(text_path("a", "x"), text_path("a", "y"));
    }

    #[test]
    fn the_env_derivation_runs_get_env_with_checks_removed_and_rehashed_outputs() {
        let mut env = BTreeMap::new();
        for (k, v) in [("name", "hello-1"), ("out", "/nix/store/old-hello-1"), ("allowedReferences", ""), ("keep", "k")] {
            env.insert(k.to_string(), v.to_string());
        }
        let mut outputs = BTreeMap::new();
        outputs.insert("out".into(), DerivationOutput { path: "/nix/store/old-hello-1".into(), hash_algo: String::new(), hash: String::new() });
        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "aarch64-darwin".into(),
            builder: "/nix/store/b-bash/bin/bash".into(),
            args: vec!["-e".into(), "builder.sh".into()],
            env,
        };
        let (path, env_drv) = env_derivation("/nix/store/aaaa-hello-1.drv", &drv, &mut |_| unreachable!()).unwrap();
        assert!(path.ends_with("-hello-1-env.drv"), "{path}");
        assert_eq!(env_drv.args, vec![get_env_sh_path()]);
        assert_eq!(env_drv.input_sources, vec![get_env_sh_path()]);
        assert_eq!(env_drv.env["name"], "hello-1-env");
        assert!(!env_drv.env.contains_key("allowedReferences"));
        assert_eq!(env_drv.env["keep"], "k");
        assert!(env_drv.outputs["out"].path.ends_with("-hello-1-env"));
        assert_eq!(env_drv.env["out"], env_drv.outputs["out"].path);
        let mut not_bash = drv.clone();
        not_bash.builder = "/bin/sh".into();
        assert!(env_derivation("/nix/store/aaaa-hello-1.drv", &not_bash, &mut |_| unreachable!()).is_err());
    }

    #[test]
    fn the_rc_script_is_cppnix_make_rc_script() {
        let env = BuildEnvironment::parse(
            r#"{"bashFunctions":{"f":"    echo hi\n"},"variables":{
              "HOME":{"type":"exported","value":"/homeless"},
              "out":{"type":"exported","value":"/nix/store/x-env"},
              "outputs":{"type":"var","value":"out"},
              "q":{"type":"var","value":"it's"},
              "a":{"type":"array","value":["1","2"]},
              "m":{"type":"associative","value":{"k":"v"}}}}"#,
        )
        .unwrap();
        let script = env.rc_script(std::path::Path::new("/t"), std::path::Path::new("/w/outputs")).unwrap();
        assert_eq!(
            script,
            concat!(
                "unset shellHook\n",
                "PATH=${PATH:-}\nnix_saved_PATH=\"$PATH\"\n",
                "XDG_DATA_DIRS=${XDG_DATA_DIRS:-}\nnix_saved_XDG_DATA_DIRS=\"$XDG_DATA_DIRS\"\n",
                "declare -a a=('1' '2' )\n",
                "declare -A m=(['k']='v' )\n",
                "out='/w/outputs/out'\nexport out\n",
                "outputs='out'\n",
                "q='it'\\''s'\n",
                "f ()\n{\n    echo hi\n}\n",
                "PATH=\"$PATH${nix_saved_PATH:+:$nix_saved_PATH}\"\n",
                "XDG_DATA_DIRS=\"$XDG_DATA_DIRS${nix_saved_XDG_DATA_DIRS:+:$nix_saved_XDG_DATA_DIRS}\"\n",
                "export NIX_BUILD_TOP=\"$(mktemp -d -t nix-shell.XXXXXX)\"\n",
                "export TMP=\"$NIX_BUILD_TOP\"\nexport TMPDIR=\"$NIX_BUILD_TOP\"\nexport TEMP=\"$NIX_BUILD_TOP\"\nexport TEMPDIR=\"$NIX_BUILD_TOP\"\n",
                "eval \"${shellHook:-}\"\n",
            )
        );
        assert_eq!(BuildEnvironment::parse(&env.to_json()).unwrap(), env);
    }
}

