//! cppnix's `nix-store --query` (nix-store.cc `opQuery`) over a [`LocalStore`].

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::local::LocalStore;
use crate::traits::{Store, StoreError, StoreResult};

/// One query kind; cppnix accepts exactly one per invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Outputs,
    Requisites { include_outputs: bool },
    References,
    Referrers,
    ReferrersClosure,
    Deriver,
    ValidDerivers,
    Binding(String),
    Hash,
    Size,
    Roots,
}

/// cppnix's `followLinksToStorePath`: a path, or a symlink chain, resolved to
/// the store path that contains it.
///
/// # Errors
///
/// When the chain does not end inside `store_dir`.
pub fn follow_links_to_store_path(arg: &str, store_dir: &str) -> StoreResult<String> {
    let mut path = PathBuf::from(arg);
    for _ in 0..64 {
        if let Some(store_path) = store_path_prefix(&path, store_dir) {
            return Ok(store_path);
        }
        let target = std::fs::read_link(&path).map_err(|_| StoreError::PathNotFound(format!("path '{arg}' is not in the Nix store")))?;
        path = if target.is_absolute() { target } else { path.parent().unwrap_or(Path::new("/")).join(target) };
    }
    Err(StoreError::PathNotFound(format!("path '{arg}' is not in the Nix store")))
}

fn store_path_prefix(path: &Path, store_dir: &str) -> Option<String> {
    let rest = path.to_str()?.strip_prefix(store_dir)?.strip_prefix('/')?;
    let name = rest.split('/').next().filter(|n| !n.is_empty())?;
    Some([store_dir, "/", name].concat())
}

/// Paths ordered so every path follows the paths it references, as cppnix
/// prints `topoSortPaths` reversed.
#[must_use]
pub fn dependencies_first(paths: &BTreeSet<String>, references: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut out = Vec::new();
    let mut done = HashSet::new();
    for root in paths {
        let mut stack = vec![(root.clone(), false)];
        while let Some((p, expanded)) = stack.pop() {
            if expanded {
                out.push(p);
                continue;
            }
            if !done.insert(p.clone()) {
                continue;
            }
            stack.push((p.clone(), true));
            for r in references.get(&p).into_iter().flatten() {
                if r != &p && paths.contains(r) && !done.contains(r) {
                    stack.push((r.clone(), false));
                }
            }
        }
    }
    out
}

/// A `sha256:<hex>` (or already nix32) NAR hash as cppnix prints it:
/// `sha256:<nix32>`.
#[must_use]
pub fn nar_hash_nix32(hash: &str) -> String {
    let digest = hash.strip_prefix("sha256:").unwrap_or(hash);
    let bytes: Option<Vec<u8>> = (digest.len() == 64)
        .then(|| (0..64).step_by(2).map(|i| u8::from_str_radix(&digest[i..i + 2], 16).ok()).collect())
        .flatten();
    match bytes {
        Some(b) => ["sha256:", &sui_compat::store_path::nix_base32_encode(&b)].concat(),
        None => ["sha256:", digest].concat(),
    }
}

impl LocalStore {
    async fn info(&self, path: &str) -> StoreResult<crate::traits::PathInfo> {
        let model = self.find_by_path(path).await?.ok_or_else(|| StoreError::PathNotFound(format!("path '{path}' is not valid")))?;
        self.model_to_path_info(&model).await
    }

    async fn use_outputs(&self, path: &str, use_output: bool) -> StoreResult<Vec<String>> {
        if !use_output || !path.ends_with(".drv") {
            return Ok(vec![path.to_string()]);
        }
        let bytes = std::fs::read(path)?;
        let drv = sui_compat::derivation::Derivation::parse(&bytes).map_err(|e| StoreError::Internal(format!("{path}: {e}")))?;
        Ok(drv.outputs.values().map(|o| o.path.clone()).collect())
    }

    async fn closure(&self, start: &[String], referrers: bool, include_outputs: bool) -> StoreResult<BTreeSet<String>> {
        let mut seen = BTreeSet::new();
        let mut queue: Vec<String> = start.to_vec();
        while let Some(p) = queue.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            let next = if referrers { self.referrers_of(&p).await? } else { self.info(&p).await?.references };
            queue.extend(next);
            if include_outputs && p.ends_with(".drv") {
                for out in self.use_outputs(&p, true).await? {
                    if self.find_by_path(&out).await?.is_some() {
                        queue.push(out);
                    }
                }
            }
        }
        Ok(seen)
    }

    async fn ordered(&self, paths: BTreeSet<String>) -> StoreResult<Vec<String>> {
        let mut references = HashMap::new();
        for p in &paths {
            references.insert(p.clone(), self.info(p).await.map(|i| i.references).unwrap_or_default());
        }
        Ok(dependencies_first(&paths, &references))
    }

    /// cppnix's `opQuery`: the lines `nix-store --query` prints.
    ///
    /// # Errors
    ///
    /// When an argument is not a valid store path, or the store cannot be read.
    pub async fn query(&self, query: &Query, args: &[String], use_output: bool) -> StoreResult<Vec<String>> {
        let store_dir = self.store_dir().to_string();
        let mut resolved = Vec::new();
        for a in args {
            resolved.push(follow_links_to_store_path(a, &store_dir)?);
        }
        let mut lines = Vec::new();
        match query {
            Query::Outputs => {
                for p in &resolved {
                    lines.extend(self.use_outputs(p, true).await?);
                }
            }
            Query::Requisites { .. } | Query::References | Query::Referrers | Query::ReferrersClosure => {
                let mut set = BTreeSet::new();
                for p in &resolved {
                    for j in self.use_outputs(p, use_output).await? {
                        match query {
                            Query::Requisites { include_outputs } => set.extend(self.closure(&[j], false, *include_outputs).await?),
                            Query::References => set.extend(self.info(&j).await?.references),
                            Query::Referrers => set.extend(self.referrers_of(&j).await?),
                            _ => set.extend(self.closure(&[j], true, false).await?),
                        }
                    }
                }
                lines = self.ordered(set).await?;
            }
            Query::Deriver => {
                for p in &resolved {
                    lines.push(self.info(p).await?.deriver.filter(|d| !d.is_empty()).unwrap_or_else(|| "unknown-deriver".into()));
                }
            }
            Query::ValidDerivers => {
                let mut set = BTreeSet::new();
                for p in &resolved {
                    let sp = sui_compat::store_path::StorePath::from_absolute_path(p).map_err(|e| StoreError::Internal(e.to_string()))?;
                    set.extend(self.query_valid_derivers(&sp).await?);
                }
                lines = self.ordered(set).await?;
            }
            Query::Binding(name) => {
                for p in &resolved {
                    let drv_path = if p.ends_with(".drv") { p.clone() } else { self.info(p).await?.deriver.unwrap_or_default() };
                    let bytes = std::fs::read(&drv_path)?;
                    let drv = sui_compat::derivation::Derivation::parse(&bytes).map_err(|e| StoreError::Internal(format!("{drv_path}: {e}")))?;
                    let value = drv.env.get(name).ok_or_else(|| {
                        StoreError::Internal(format!("derivation '{drv_path}' has no environment binding named '{name}'"))
                    })?;
                    lines.push(value.clone());
                }
            }
            Query::Hash | Query::Size => {
                for p in &resolved {
                    for j in self.use_outputs(p, use_output).await? {
                        let info = self.info(&j).await?;
                        lines.push(if *query == Query::Hash { nar_hash_nix32(&info.nar_hash) } else { info.nar_size.to_string() });
                    }
                }
            }
            Query::Roots => {
                let state_dir = PathBuf::from(crate::local::state_dir_for_store(&store_dir));
                let mut roots = crate::local::find_root_links(&state_dir, &store_dir);
                roots.extend(crate::local::find_runtime_roots(&store_dir).into_iter().map(|(p, src)| (src, p)));
                roots.extend(crate::local::find_temp_roots(&state_dir, &store_dir).into_iter().map(|(p, src)| (src, p)));
                let wanted: BTreeSet<String> = resolved.iter().cloned().collect();
                let mut found = BTreeSet::new();
                for (link, target) in roots {
                    let Some(target) = store_path_prefix(Path::new(&target), &store_dir) else { continue };
                    if self.find_by_path(&target).await?.is_none() {
                        continue;
                    }
                    let reach = self.closure(std::slice::from_ref(&target), false, false).await?;
                    if !reach.is_disjoint(&wanted) {
                        found.insert([link.as_str(), " -> ", &target].concat());
                    }
                }
                lines.extend(found);
            }
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::PathInfo;

    #[test]
    fn a_symlink_chain_resolves_to_its_store_path() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("nix/store");
        let target = store.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x/bin/tool");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "").unwrap();
        let link = tmp.path().join("result");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let s = store.to_str().unwrap();
        assert_eq!(follow_links_to_store_path(link.to_str().unwrap(), s).unwrap(), [s, "/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x"].concat());
        assert!(follow_links_to_store_path("/etc/hosts", s).is_err());
    }

    #[test]
    fn dependencies_print_before_what_needs_them() {
        let set: BTreeSet<String> = ["a", "b", "c"].iter().map(|s| (*s).to_string()).collect();
        let refs: HashMap<String, Vec<String>> =
            [("a".into(), vec!["b".into()]), ("b".into(), vec!["c".into(), "b".into()]), ("c".into(), vec![])].into_iter().collect();
        assert_eq!(dependencies_first(&set, &refs), ["c", "b", "a"]);
    }

    #[test]
    fn a_nar_hash_prints_as_nix32_as_cppnix_prints_it() {
        assert_eq!(
            nar_hash_nix32("sha256:fc65445c43edb02c3af05e136a25ce46791d86bd2fee6522bf67e356ca5d5835"),
            "sha256:0daqbp55dqv7pwi6bvigpn31sya6rqjnl4syy0x2rc7d8df48rgw",
            "bash-5.3p3: the DB's hash, and cppnix 2.31.5's `nix-store -q --hash` of the same path"
        );
    }

    #[tokio::test]
    async fn queries_answer_as_cppnix_over_a_temp_store() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nix/store");
        std::fs::create_dir_all(tmp.path().join("nix/var/nix/gcroots")).unwrap();
        let store = LocalStore::open_in_memory_with_dir(dir.to_str().unwrap()).await.unwrap();
        let path = |h: char, n: &str| format!("{}/{}-{n}", dir.display(), h.to_string().repeat(32));
        let (lib, app, other) = (path('a', "lib"), path('b', "app"), path('c', "other"));
        let hex = ["sha256:", &"ab".repeat(32)].concat();
        for (p, refs, size) in [(&lib, vec![], 7), (&app, vec![lib.clone()], 9), (&other, vec![app.clone()], 3)] {
            std::fs::create_dir_all(p).unwrap();
            let info = PathInfo { path: p.clone(), nar_hash: hex.clone(), nar_size: size, references: refs, ..PathInfo::default() };
            store.register_path(&info).await.unwrap();
        }
        std::os::unix::fs::symlink(&app, tmp.path().join("nix/var/nix/gcroots/keep")).unwrap();
        let q = |query: Query, arg: &str| {
            let store = &store;
            let arg = arg.to_string();
            async move { store.query(&query, &[arg], false).await.unwrap() }
        };
        assert_eq!(q(Query::Requisites { include_outputs: false }, &other).await, [lib.clone(), app.clone(), other.clone()]);
        assert_eq!(q(Query::References, &app).await, [lib.clone()]);
        assert_eq!(q(Query::Referrers, &lib).await, [app.clone()]);
        assert_eq!(q(Query::ReferrersClosure, &lib).await, [lib.clone(), app.clone(), other.clone()]);
        assert_eq!(q(Query::Size, &app).await, ["9"]);
        assert_eq!(q(Query::Hash, &app).await, [nar_hash_nix32(&hex)]);
        assert_eq!(q(Query::Deriver, &app).await, ["unknown-deriver"]);
        assert_eq!(q(Query::Outputs, &app).await, [app.clone()]);
        let roots = q(Query::Roots, &lib).await;
        assert_eq!(roots.len(), 1);
        assert!(roots[0].ends_with(&[" -> ", app.as_str()].concat()), "{roots:?}");
        assert!(q(Query::Roots, &other).await.is_empty());
    }

    #[tokio::test]
    async fn a_derivation_answers_through_its_outputs_and_its_deriver() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nix/store");
        std::fs::create_dir_all(&dir).unwrap();
        let store = LocalStore::open_in_memory_with_dir(dir.to_str().unwrap()).await.unwrap();
        let path = |h: char, n: &str| format!("{}/{}-{n}", dir.display(), h.to_string().repeat(32));
        let (lib, app, drv) = (path('a', "lib"), path('b', "app"), path('d', "app.drv"));
        std::fs::write(
            &drv,
            format!(r#"Derive([("out","{app}","","")],[],[],"x","/bin/sh",[],[("name","app"),("out","{app}")])"#),
        )
        .unwrap();
        let hex = ["sha256:", &"ab".repeat(32)].concat();
        for (p, refs, size, deriver) in [(&lib, vec![], 7, None), (&drv, vec![], 1, None), (&app, vec![lib.clone()], 9, Some(drv.clone()))] {
            if !p.ends_with(".drv") {
                std::fs::create_dir_all(p).unwrap();
            }
            let info = PathInfo { path: p.clone(), nar_hash: hex.clone(), nar_size: size, references: refs, deriver, ..PathInfo::default() };
            store.register_path(&info).await.unwrap();
        }
        let one = |s: &String| vec![s.clone()];
        assert_eq!(store.query(&Query::Outputs, &one(&drv), false).await.unwrap(), [app.clone()]);
        assert_eq!(store.query(&Query::Size, &one(&drv), true).await.unwrap(), ["9"], "-u reads the output");
        assert_eq!(store.query(&Query::Size, &one(&drv), false).await.unwrap(), ["1"], "without -u, the .drv itself");
        assert_eq!(
            store.query(&Query::Requisites { include_outputs: true }, &one(&drv), false).await.unwrap(),
            [lib.clone(), app.clone(), drv.clone()],
            "unrelated paths in store-path order, each after what it references (cppnix topoSortPaths, printed reversed)"
        );
        assert_eq!(store.query(&Query::Requisites { include_outputs: false }, &one(&drv), false).await.unwrap(), [drv.clone()]);
        assert_eq!(store.query(&Query::Deriver, &one(&app), false).await.unwrap(), [drv.clone()]);
        assert_eq!(store.query(&Query::Binding("name".into()), &one(&app), false).await.unwrap(), ["app"]);
    }
}
