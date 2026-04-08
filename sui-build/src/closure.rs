//! Build closure computation — topological sort of derivation dependencies.
//!
//! Given a target `.drv` path, [`BuildClosure::compute`] recursively parses
//! all input derivations and returns them in topological order (leaves first,
//! target last). This is the order in which builds must execute to satisfy
//! all dependencies.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use sui_compat::derivation::Derivation;

use crate::traits::BuildError;

/// A computed build closure — derivations in topological build order.
#[derive(Debug, Clone)]
pub struct BuildClosure {
    /// Derivations in topological order (leaves first, target last).
    /// Each entry is `(drv_path, parsed_derivation)`.
    pub derivations: Vec<(String, Derivation)>,
}

impl BuildClosure {
    /// Parse a `.drv` file and recursively compute its full build closure.
    ///
    /// Returns derivations in topological order (leaves first, target last).
    /// Diamond dependencies are handled — each `.drv` is parsed only once.
    ///
    /// # Errors
    ///
    /// Returns `BuildError::Derivation` if a `.drv` file cannot be read or parsed,
    /// or if a dependency cycle is detected.
    pub fn compute(drv_path: &str) -> Result<Self, BuildError> {
        let mut parsed: BTreeMap<String, Derivation> = BTreeMap::new();
        let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        // Recursively parse all derivations
        Self::parse_recursive(drv_path, &mut parsed, &mut edges)?;

        // Topological sort using Kahn's algorithm
        let sorted = Self::topo_sort(&parsed, &edges)?;

        let derivations = sorted
            .into_iter()
            .map(|path| {
                let drv = parsed.remove(&path).ok_or_else(|| {
                    BuildError::Derivation(format!(
                        "internal error: topo_sort produced path not in parsed set: {path}"
                    ))
                })?;
                Ok((path, drv))
            })
            .collect::<Result<Vec<_>, BuildError>>()?;

        Ok(Self { derivations })
    }

    /// The final (target) derivation — always the last element.
    ///
    /// Returns an error if the closure is empty (should never happen for a
    /// closure produced by [`compute`]).
    pub fn try_target(&self) -> Result<&(String, Derivation), BuildError> {
        self.derivations.last().ok_or_else(|| {
            BuildError::Derivation("internal error: build closure is empty".into())
        })
    }

    /// The final (target) derivation — always the last element.
    ///
    /// # Panics
    ///
    /// Panics if the closure is empty (should never happen for a valid closure).
    /// Prefer [`try_target`] for fallible callers.
    #[must_use]
    pub fn target(&self) -> &(String, Derivation) {
        self.derivations.last().expect("closure is never empty")
    }

    /// Number of derivations in the closure.
    #[must_use]
    pub fn len(&self) -> usize {
        self.derivations.len()
    }

    /// Returns `true` if the closure is empty (should never happen).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.derivations.is_empty()
    }

    /// Recursively parse derivation files, building the dependency graph.
    fn parse_recursive(
        drv_path: &str,
        parsed: &mut BTreeMap<String, Derivation>,
        edges: &mut BTreeMap<String, BTreeSet<String>>,
    ) -> Result<(), BuildError> {
        if parsed.contains_key(drv_path) {
            return Ok(());
        }

        let data = std::fs::read(drv_path).map_err(|e| {
            BuildError::Derivation(format!("cannot read {drv_path}: {e}"))
        })?;

        let drv = Derivation::parse(&data).map_err(|e| {
            BuildError::Derivation(format!("cannot parse {drv_path}: {e}"))
        })?;

        // Record edges: this drv depends on its input derivations
        let deps: BTreeSet<String> = drv.input_derivations.keys().cloned().collect();
        edges.insert(drv_path.to_string(), deps.clone());
        parsed.insert(drv_path.to_string(), drv);

        // Recurse into dependencies
        for dep_path in deps {
            Self::parse_recursive(&dep_path, parsed, edges)?;
        }

        Ok(())
    }

    /// Kahn's algorithm — topological sort with cycle detection.
    fn topo_sort(
        parsed: &BTreeMap<String, Derivation>,
        edges: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<Vec<String>, BuildError> {
        // Compute in-degree for each node
        let mut in_degree: BTreeMap<String, usize> = BTreeMap::new();
        for key in parsed.keys() {
            in_degree.entry(key.clone()).or_insert(0);
        }
        for deps in edges.values() {
            for dep in deps {
                // Only count edges to nodes we know about
                if parsed.contains_key(dep) {
                    *in_degree.entry(dep.clone()).or_insert(0) += 0;
                }
            }
        }

        // Count incoming edges properly
        // If A depends on B, then B has an incoming edge from A's perspective
        // But for build order, we want: B must be built before A
        // So B->A in the "must build before" direction
        // In Kahn's: in_degree[node] = number of deps that must be built first
        let mut in_deg: BTreeMap<String, usize> = BTreeMap::new();
        for key in parsed.keys() {
            in_deg.entry(key.clone()).or_insert(0);
        }
        for (node, deps) in edges {
            // `node` depends on each `dep`, so node's in-degree is deps.len()
            // (counting only deps that are in our parsed set)
            let count = deps.iter().filter(|d| parsed.contains_key(*d)).count();
            *in_deg.entry(node.clone()).or_insert(0) = count;
        }

        let mut queue: VecDeque<String> = VecDeque::new();
        for (node, &deg) in &in_deg {
            if deg == 0 {
                queue.push_back(node.clone());
            }
        }

        // Build reverse adjacency: for each dep, which nodes depend on it?
        let mut reverse: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (node, deps) in edges {
            for dep in deps {
                if parsed.contains_key(dep) {
                    reverse.entry(dep.clone()).or_default().push(node.clone());
                }
            }
        }

        let mut sorted = Vec::new();
        while let Some(node) = queue.pop_front() {
            sorted.push(node.clone());
            if let Some(dependents) = reverse.get(&node) {
                for dependent in dependents {
                    if let Some(deg) = in_deg.get_mut(dependent) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(dependent.clone());
                        }
                    }
                }
            }
        }

        if sorted.len() != parsed.len() {
            return Err(BuildError::Derivation(
                "dependency cycle detected in derivation closure".to_string(),
            ));
        }

        Ok(sorted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::path::Path;
    use sui_compat::derivation::DerivationOutput;

    /// Helper to create a minimal derivation with given input derivations.
    fn make_drv(
        name: &str,
        input_drvs: &[(&str, &[&str])],
    ) -> Derivation {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: format!("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0-{name}"),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );

        let input_derivations: BTreeMap<String, Vec<String>> = input_drvs
            .iter()
            .map(|(path, outs)| {
                (path.to_string(), outs.iter().map(|s| s.to_string()).collect())
            })
            .collect();

        Derivation {
            outputs,
            input_derivations,
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "true".to_string()],
            env: BTreeMap::new(),
        }
    }

    /// Write a derivation to a temp file and return the path.
    fn write_drv(dir: &Path, name: &str, drv: &Derivation) -> String {
        let path = dir.join(format!("{name}.drv"));
        let serialized = drv.serialize();
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(serialized.as_bytes()).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn single_derivation_no_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let drv = make_drv("hello", &[]);
        let path = write_drv(tmp.path(), "hello", &drv);

        let closure = BuildClosure::compute(&path).unwrap();
        assert_eq!(closure.len(), 1);
        assert_eq!(closure.target().0, path);
        assert_eq!(closure.target().1, drv);
    }

    #[test]
    fn linear_chain_a_depends_on_b_depends_on_c() {
        let tmp = tempfile::tempdir().unwrap();

        // C has no deps
        let drv_c = make_drv("c", &[]);
        let path_c = write_drv(tmp.path(), "c", &drv_c);

        // B depends on C
        let drv_b = make_drv("b", &[(&path_c, &["out"])]);
        let path_b = write_drv(tmp.path(), "b", &drv_b);

        // A depends on B
        let drv_a = make_drv("a", &[(&path_b, &["out"])]);
        let path_a = write_drv(tmp.path(), "a", &drv_a);

        let closure = BuildClosure::compute(&path_a).unwrap();
        assert_eq!(closure.len(), 3);

        // C must come before B, B before A
        let positions: BTreeMap<&str, usize> = closure
            .derivations
            .iter()
            .enumerate()
            .map(|(i, (p, _))| (p.as_str(), i))
            .collect();

        assert!(positions[path_c.as_str()] < positions[path_b.as_str()]);
        assert!(positions[path_b.as_str()] < positions[path_a.as_str()]);

        // Target is A
        assert_eq!(closure.target().0, path_a);
    }

    #[test]
    fn diamond_dependency() {
        let tmp = tempfile::tempdir().unwrap();

        // D has no deps (shared leaf)
        let drv_d = make_drv("d", &[]);
        let path_d = write_drv(tmp.path(), "d", &drv_d);

        // B depends on D
        let drv_b = make_drv("b", &[(&path_d, &["out"])]);
        let path_b = write_drv(tmp.path(), "b", &drv_b);

        // C depends on D
        let drv_c = make_drv("c", &[(&path_d, &["out"])]);
        let path_c = write_drv(tmp.path(), "c", &drv_c);

        // A depends on B and C
        let drv_a = make_drv("a", &[(&path_b, &["out"]), (&path_c, &["out"])]);
        let path_a = write_drv(tmp.path(), "a", &drv_a);

        let closure = BuildClosure::compute(&path_a).unwrap();
        assert_eq!(closure.len(), 4); // D, B, C, A (D only once)

        let positions: BTreeMap<&str, usize> = closure
            .derivations
            .iter()
            .enumerate()
            .map(|(i, (p, _))| (p.as_str(), i))
            .collect();

        // D must come before B and C
        assert!(positions[path_d.as_str()] < positions[path_b.as_str()]);
        assert!(positions[path_d.as_str()] < positions[path_c.as_str()]);
        // B and C must come before A
        assert!(positions[path_b.as_str()] < positions[path_a.as_str()]);
        assert!(positions[path_c.as_str()] < positions[path_a.as_str()]);
    }

    #[test]
    fn missing_drv_file_returns_error() {
        let result = BuildClosure::compute("/nonexistent/path/to/missing.drv");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("cannot read"));
    }

    #[test]
    fn invalid_drv_file_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.drv");
        std::fs::write(&path, b"this is not a valid derivation").unwrap();

        let result = BuildClosure::compute(path.to_str().unwrap());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("cannot parse"));
    }

    #[test]
    fn closure_is_not_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let drv = make_drv("solo", &[]);
        let path = write_drv(tmp.path(), "solo", &drv);

        let closure = BuildClosure::compute(&path).unwrap();
        assert!(!closure.is_empty());
    }

    #[test]
    fn closure_len_matches_derivations_count() {
        let tmp = tempfile::tempdir().unwrap();

        let drv_leaf = make_drv("leaf", &[]);
        let path_leaf = write_drv(tmp.path(), "leaf", &drv_leaf);

        let drv_root = make_drv("root", &[(&path_leaf, &["out"])]);
        let path_root = write_drv(tmp.path(), "root", &drv_root);

        let closure = BuildClosure::compute(&path_root).unwrap();
        assert_eq!(closure.len(), closure.derivations.len());
        assert_eq!(closure.len(), 2);
    }

    #[test]
    fn wider_diamond_five_nodes() {
        // E is a shared leaf, B/C/D all depend on E, A depends on B/C/D
        let tmp = tempfile::tempdir().unwrap();

        let drv_e = make_drv("e", &[]);
        let path_e = write_drv(tmp.path(), "e", &drv_e);

        let drv_b = make_drv("b", &[(&path_e, &["out"])]);
        let path_b = write_drv(tmp.path(), "b", &drv_b);

        let drv_c = make_drv("c", &[(&path_e, &["out"])]);
        let path_c = write_drv(tmp.path(), "c", &drv_c);

        let drv_d = make_drv("d", &[(&path_e, &["out"])]);
        let path_d = write_drv(tmp.path(), "d", &drv_d);

        let drv_a = make_drv("a", &[
            (&path_b, &["out"]),
            (&path_c, &["out"]),
            (&path_d, &["out"]),
        ]);
        let path_a = write_drv(tmp.path(), "a", &drv_a);

        let closure = BuildClosure::compute(&path_a).unwrap();
        assert_eq!(closure.len(), 5);

        let positions: BTreeMap<&str, usize> = closure
            .derivations
            .iter()
            .enumerate()
            .map(|(i, (p, _))| (p.as_str(), i))
            .collect();

        // E must be first (only leaf)
        assert_eq!(positions[path_e.as_str()], 0);
        // A must be last (target)
        assert_eq!(closure.target().0, path_a);
    }

    #[test]
    fn deep_chain_five_levels() {
        let tmp = tempfile::tempdir().unwrap();

        let drv_e = make_drv("e", &[]);
        let path_e = write_drv(tmp.path(), "e", &drv_e);

        let drv_d = make_drv("d", &[(&path_e, &["out"])]);
        let path_d = write_drv(tmp.path(), "d", &drv_d);

        let drv_c = make_drv("c", &[(&path_d, &["out"])]);
        let path_c = write_drv(tmp.path(), "c", &drv_c);

        let drv_b = make_drv("b", &[(&path_c, &["out"])]);
        let path_b = write_drv(tmp.path(), "b", &drv_b);

        let drv_a = make_drv("a", &[(&path_b, &["out"])]);
        let path_a = write_drv(tmp.path(), "a", &drv_a);

        let closure = BuildClosure::compute(&path_a).unwrap();
        assert_eq!(closure.len(), 5);

        // Must be in strict order: E, D, C, B, A
        let paths: Vec<&str> = closure.derivations.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths[0], path_e);
        assert_eq!(paths[1], path_d);
        assert_eq!(paths[2], path_c);
        assert_eq!(paths[3], path_b);
        assert_eq!(paths[4], path_a);
    }

    #[test]
    fn cycle_detection() {
        // We can't create a true filesystem cycle easily (A depends on B, B
        // depends on A) because we'd need to write both files, and the second
        // would reference the first. Let's test the topo_sort directly.
        let mut parsed = BTreeMap::new();
        parsed.insert("a".to_string(), make_drv("a", &[("b", &["out"])]));
        parsed.insert("b".to_string(), make_drv("b", &[("a", &["out"])]));

        let mut edges = BTreeMap::new();
        edges.insert("a".to_string(), BTreeSet::from(["b".to_string()]));
        edges.insert("b".to_string(), BTreeSet::from(["a".to_string()]));

        let result = BuildClosure::topo_sort(&parsed, &edges);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cycle"));
    }

    #[test]
    fn target_returns_last_derivation() {
        let tmp = tempfile::tempdir().unwrap();

        let drv_b = make_drv("dep", &[]);
        let path_b = write_drv(tmp.path(), "dep", &drv_b);

        let drv_a = make_drv("target", &[(&path_b, &["out"])]);
        let path_a = write_drv(tmp.path(), "target", &drv_a);

        let closure = BuildClosure::compute(&path_a).unwrap();
        let (target_path, target_drv) = closure.target();
        assert_eq!(target_path, &path_a);
        assert_eq!(target_drv, &drv_a);
    }

    #[test]
    fn try_target_returns_ok_for_valid_closure() {
        let tmp = tempfile::tempdir().unwrap();
        let drv = make_drv("hello", &[]);
        let path = write_drv(tmp.path(), "hello", &drv);
        let closure = BuildClosure::compute(&path).unwrap();
        let result = closure.try_target();
        assert!(result.is_ok());
        assert_eq!(&result.unwrap().0, &path);
    }

    #[test]
    fn try_target_returns_err_for_empty_closure() {
        let empty = BuildClosure {
            derivations: vec![],
        };
        let result = empty.try_target();
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("internal error"),
            "expected internal error for empty closure"
        );
    }
}
