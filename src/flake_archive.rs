use std::collections::BTreeMap;

use serde_json::{Map, Value};
use sui_compat::flake::{FlakeLock, InputRef, LockedInput};

pub fn input_tree(
    lock: &FlakeLock,
    node: &str,
    store_path: &mut dyn FnMut(&str, &LockedInput) -> Result<Option<String>, String>,
) -> Result<Value, String> {
    let mut memo = BTreeMap::new();
    walk(lock, node, store_path, &mut memo)
}

fn walk(
    lock: &FlakeLock,
    node: &str,
    store_path: &mut dyn FnMut(&str, &LockedInput) -> Result<Option<String>, String>,
    memo: &mut BTreeMap<String, Option<String>>,
) -> Result<Value, String> {
    let mut inputs = Map::new();
    let Some(current) = lock.nodes.get(node) else {
        return Err(format!("flake.lock has no node `{node}`"));
    };
    for (name, edge) in &current.inputs {
        let InputRef::Direct(target) = edge else { continue };
        let child = lock.nodes.get(target).ok_or_else(|| format!("flake.lock has no node `{target}`"))?;
        let mut entry = Map::new();
        if let Value::Object(sub) = walk(lock, target, store_path, memo)? {
            entry.extend(sub);
        }
        if let Some(locked) = child.locked.as_ref().filter(|l| !is_relative(l)) {
            let path = match memo.get(target) {
                Some(p) => p.clone(),
                None => {
                    let p = store_path(target, locked)?;
                    memo.insert(target.clone(), p.clone());
                    p
                }
            };
            if let Some(path) = path {
                entry.insert("path".into(), Value::String(path));
            }
        }
        inputs.insert(name.clone(), Value::Object(entry));
    }
    let mut out = Map::new();
    out.insert("inputs".into(), Value::Object(inputs));
    Ok(Value::Object(out))
}

fn is_relative(locked: &LockedInput) -> bool {
    locked.source_type == "path" && locked.path.as_deref().is_some_and(|p| !p.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tree_follows_direct_edges_only_and_names_each_input_once_per_edge_as_cppnix() {
        let lock = FlakeLock::parse(
            r#"{"version":7,"root":"root","nodes":{
              "root":{"inputs":{"a":"a","b":"b","c":["a"]}},
              "a":{"locked":{"type":"github","owner":"o","repo":"a","rev":"1","narHash":"sha256-A"},"inputs":{"n":"n"}},
              "b":{"locked":{"type":"path","path":"./sub"},"inputs":{"n":"n"}},
              "n":{"locked":{"type":"github","owner":"o","repo":"n","rev":"2","narHash":"sha256-N"}}
            }}"#,
        )
        .unwrap();
        let mut calls = Vec::new();
        let tree = input_tree(&lock, "root", &mut |name, _| {
            calls.push(name.to_string());
            Ok(Some(format!("/nix/store/{name}-source")))
        })
        .unwrap();
        assert_eq!(
            tree,
            serde_json::json!({"inputs": {
                "a": {"path": "/nix/store/a-source", "inputs": {"n": {"path": "/nix/store/n-source", "inputs": {}}}},
                "b": {"inputs": {"n": {"path": "/nix/store/n-source", "inputs": {}}}}
            }})
        );
        assert_eq!(
            tree.to_string(),
            r#"{"inputs":{"a":{"inputs":{"n":{"inputs":{},"path":"/nix/store/n-source"}},"path":"/nix/store/a-source"},"b":{"inputs":{"n":{"inputs":{},"path":"/nix/store/n-source"}}}}}"#
        );
        assert_eq!(calls, vec!["n", "a"]);
        assert!(input_tree(&lock, "missing", &mut |_, _| Ok(None)).is_err());
        assert!(input_tree(&lock, "root", &mut |_, _| Err("offline".into())).is_err());
    }
}
