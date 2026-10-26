use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use sui_compat::derivation::Derivation;
use sui_compat::store_path::StorePath;
use sui_store::traits::{PathInfo, Store, StoreError, StoreResult};

use crate::sandbox::SandboxConfig;

struct Infos<'a> {
    store: &'a dyn Store,
    known: BTreeMap<String, PathInfo>,
    read_drv: &'a dyn Fn(&str) -> std::io::Result<Vec<u8>>,
}

impl Infos<'_> {
    async fn get(&mut self, path: &str) -> StoreResult<PathInfo> {
        if let Some(info) = self.known.get(path) {
            return Ok(info.clone());
        }
        let sp = StorePath::from_absolute_path(path).map_err(|e| StoreError::Internal(format!("{path}: {e}")))?;
        let info = self.store.query_path_info(&sp).await?.ok_or_else(|| StoreError::PathNotFound(path.to_string()))?;
        self.known.insert(path.to_string(), info.clone());
        Ok(info)
    }

    async fn closure(&mut self, roots: impl IntoIterator<Item = String>) -> StoreResult<BTreeSet<String>> {
        let mut seen = BTreeSet::new();
        let mut todo: Vec<String> = roots.into_iter().collect();
        while let Some(path) = todo.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            for r in self.get(&path).await?.references {
                if !seen.contains(&r) {
                    todo.push(r);
                }
            }
        }
        Ok(seen)
    }

    /// cppnix's `Store::exportReferences`: the closure of `roots`, plus the
    /// closure of every output of a derivation in it.
    async fn export(&mut self, roots: &BTreeSet<String>) -> StoreResult<BTreeSet<String>> {
        let mut paths = self.closure(roots.iter().cloned()).await?;
        for drv_path in paths.clone().iter().filter(|p| p.ends_with(".drv")) {
            let bytes = (self.read_drv)(drv_path).map_err(StoreError::Io)?;
            let drv = Derivation::parse(&bytes).map_err(|e| StoreError::Internal(format!("{drv_path}: {e}")))?;
            if drv.outputs.values().any(|o| o.path.is_empty()) {
                return Err(StoreError::Internal("exportReferences on CA derivations is not yet implemented".into()));
            }
            let outputs: Vec<String> = drv.outputs.values().map(|o| o.path.clone()).collect();
            paths.extend(self.closure(outputs).await?);
        }
        Ok(paths)
    }

    async fn path_info_json(&mut self, paths: &BTreeSet<String>) -> StoreResult<Value> {
        let mut list = Vec::new();
        for path in paths {
            let info = self.get(path).await?;
            let mut size: i64 = 0;
            for p in self.closure([path.clone()]).await? {
                size += self.get(&p).await?.nar_size;
            }
            let mut entry = serde_json::Map::new();
            if let Some(ca) = info.content_address.as_ref().filter(|c| !c.is_empty()) {
                entry.insert("ca".into(), Value::String(ca.clone()));
            }
            entry.insert("closureSize".into(), size.into());
            entry.insert("narHash".into(), Value::String(nix32_hash(&info.nar_hash)));
            entry.insert("narSize".into(), info.nar_size.into());
            entry.insert("path".into(), Value::String(path.clone()));
            let mut refs = info.references.clone();
            refs.sort();
            entry.insert("references".into(), refs.into_iter().map(Value::String).collect());
            entry.insert("valid".into(), Value::Bool(true));
            list.push(Value::Object(entry));
        }
        Ok(Value::Array(list))
    }

    async fn registration(&mut self, paths: &BTreeSet<String>) -> StoreResult<String> {
        let mut out = String::new();
        for path in paths {
            let mut refs = self.get(path).await?.references;
            refs.sort();
            out.push_str(&format!("{path}\n\n{}\n", refs.len()));
            for r in refs {
                out.push_str(&format!("{r}\n"));
            }
        }
        Ok(out)
    }
}

fn nix32_hash(hash: &str) -> String {
    let Some((algo, digest)) = hash.split_once(':') else { return hash.to_string() };
    let Ok(algorithm) = algo.parse::<sui_compat::hash::HashAlgorithm>() else { return hash.to_string() };
    sui_compat::hash::NixHash::parse_any(algorithm, digest).map_or_else(|_| hash.to_string(), |h| format!("{algo}:{}", h.to_nix32()))
}

fn flatten(value: &Value, into: &mut BTreeSet<String>) {
    match value {
        Value::String(s) => {
            into.insert(s.clone());
        }
        Value::Array(items) => items.iter().for_each(|v| flatten(v, into)),
        _ => {}
    }
}

fn store_root(path: &str) -> StoreResult<String> {
    let rest = path
        .strip_prefix("/nix/store/")
        .ok_or_else(|| StoreError::Internal(format!("'exportReferencesGraph' contains a non-store path '{path}'")))?;
    Ok(format!("/nix/store/{}", rest.split('/').next().unwrap_or_default()))
}

/// The files a builder finds in its build directory, as cppnix writes them:
/// passAsFile values, or `.attrs.json` / `.attrs.sh` for structured attrs,
/// with every `exportReferencesGraph` entry filled from the store.
///
/// # Errors
/// A path the store does not hold, or a malformed `exportReferencesGraph`.
pub async fn attr_files(store: &dyn Store, drv: &Derivation, build_dir: &str) -> StoreResult<Vec<(String, String)>> {
    attr_files_with(store, drv, build_dir, &|p| std::fs::read(p)).await
}

async fn attr_files_with(
    store: &dyn Store,
    drv: &Derivation,
    build_dir: &str,
    read_drv: &dyn Fn(&str) -> std::io::Result<Vec<u8>>,
) -> StoreResult<Vec<(String, String)>> {
    let mut infos = Infos { store, known: BTreeMap::new(), read_drv };
    if let Some(mut json) = SandboxConfig::structured_attrs(drv) {
        let graph: Vec<(String, BTreeSet<String>)> = json
            .get("exportReferencesGraph")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| {
                        let mut set = BTreeSet::new();
                        flatten(v, &mut set);
                        (k.clone(), set)
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (key, roots) in graph {
            let roots = roots.iter().map(|p| store_root(p)).collect::<StoreResult<BTreeSet<_>>>()?;
            let paths = infos.export(&roots).await?;
            json.insert(key, infos.path_info_json(&paths).await?);
        }
        return Ok(vec![
            (format!("{build_dir}/.attrs.json"), Value::Object(json.clone()).to_string()),
            (format!("{build_dir}/.attrs.sh"), SandboxConfig::structured_attrs_shell(&json)),
        ]);
    }
    let mut files = SandboxConfig::attr_files(drv, build_dir);
    let tokens: Vec<&str> = drv.env.get("exportReferencesGraph").map_or("", String::as_str).split_whitespace().collect();
    if tokens.len() % 2 != 0 {
        return Err(StoreError::Internal(format!("odd number of tokens in 'exportReferencesGraph': '{}'", tokens.join(" "))));
    }
    for pair in tokens.chunks(2) {
        let (file, path) = (pair[0], pair[1]);
        let valid_name = file.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && file.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
        if !valid_name {
            return Err(StoreError::Internal(format!("invalid file name '{file}' in 'exportReferencesGraph'")));
        }
        let paths = infos.export(&BTreeSet::from([store_root(path)?])).await?;
        files.push((format!("{build_dir}/{file}"), infos.registration(&paths).await?));
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use sui_compat::derivation::DerivationOutput;

    const A: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a";
    const B: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b";
    const C: &str = "/nix/store/cccccccccccccccccccccccccccccccc-c";

    fn info(path: &str, size: i64, refs: Vec<String>, ca: Option<&str>, hash_digit: char) -> PathInfo {
        PathInfo {
            path: path.into(),
            nar_hash: format!("sha256:{}", hash_digit.to_string().repeat(64)),
            nar_size: size,
            references: refs,
            deriver: None,
            signatures: vec![],
            registration_time: 0,
            content_address: ca.map(str::to_owned),
        }
    }

    async fn store() -> crate::test_store::MockStore {
        crate::test_store::MockStore::new()
            .with_path(info(B, 10, vec![], Some("text:sha256:x"), '1'))
            .with_path(info(A, 100, vec![B.to_string(), A.to_string()], None, '0'))
            .with_path(info(C, 1, vec![], None, '1'))
    }

    fn drv(env: &[(&str, &str)]) -> Derivation {
        let mut outputs = BTreeMap::new();
        outputs.insert("out".into(), DerivationOutput { path: "/nix/store/o".into(), hash_algo: String::new(), hash: String::new() });
        Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "aarch64-darwin".into(),
            builder: "/bin/sh".into(),
            args: vec![],
            env: env.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
        }
    }

    #[tokio::test]
    async fn structured_export_references_graph_is_cppnix_path_info_json() {
        let store = store().await;
        let json = format!(r#"{{"exportReferencesGraph":{{"closure":["{A}/bin/x"]}}}}"#);
        let files: BTreeMap<String, String> = attr_files(&store, &drv(&[("__json", &json)]), "/b").await.unwrap().into_iter().collect();
        let attrs: Value = serde_json::from_str(&files["/b/.attrs.json"]).unwrap();
        assert_eq!(
            attrs["closure"],
            serde_json::json!([
                {"closureSize": 110, "narHash": format!("sha256:{}", sui_compat::hash::NixHash::parse_any(sui_compat::hash::HashAlgorithm::Sha256, &"0".repeat(64)).unwrap().to_nix32()), "narSize": 100, "path": A, "references": [A, B], "valid": true},
                {"ca": "text:sha256:x", "closureSize": 10, "narHash": format!("sha256:{}", sui_compat::hash::NixHash::parse_any(sui_compat::hash::HashAlgorithm::Sha256, &"1".repeat(64)).unwrap().to_nix32()), "narSize": 10, "path": B, "references": [], "valid": true}
            ])
        );
        assert!(!files["/b/.attrs.sh"].contains("closure"));
    }

    #[tokio::test]
    async fn plain_export_references_graph_is_a_validity_registration() {
        let store = store().await;
        let files: BTreeMap<String, String> = attr_files(&store, &drv(&[("exportReferencesGraph", &format!("graph {A}"))]), "/b")
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(files["/b/graph"], format!("{A}\n\n2\n{A}\n{B}\n{B}\n\n0\n"));
        for bad in [format!("graph {A} extra"), format!("9bad {A}"), "graph /tmp/x".to_string()] {
            assert!(attr_files(&store, &drv(&[("exportReferencesGraph", &bad)]), "/b").await.is_err(), "{bad}");
        }
        assert!(attr_files(&store, &drv(&[("exportReferencesGraph", &format!("g {C}"))]), "/b").await.is_ok());
    }

    #[tokio::test]
    async fn a_derivation_in_the_graph_brings_its_outputs_closures() {
        let d = "/nix/store/dddddddddddddddddddddddddddddddd-d.drv";
        let store = store().await.with_path(info(d, 5, vec![], None, '2'));
        let with_output = |path: &str| {
            let mut x = drv(&[]);
            x.outputs.get_mut("out").unwrap().path = path.to_string();
            x.serialize().into_bytes()
        };
        let env = [("exportReferencesGraph", format!("g {d}"))];
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let read_a = move |_: &str| Ok(with_output(A));
        let files: BTreeMap<String, String> = attr_files_with(&store, &drv(&env), "/b", &read_a).await.unwrap().into_iter().collect();
        assert_eq!(files["/b/g"], format!("{A}\n\n2\n{A}\n{B}\n{B}\n\n0\n{d}\n\n0\n"));
        let read_floating = move |_: &str| Ok(with_output(""));
        assert!(attr_files_with(&store, &drv(&env), "/b", &read_floating).await.is_err());
    }
}
