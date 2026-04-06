//! Nix flake.lock (v7) parser and input-graph resolver.
//!
//! Parses the JSON lock file that Nix writes, builds an adjacency map of the
//! input graph, resolves `follows` references, and exposes a typed
//! `resolve_input` walk.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ── Error type ──────────────────────────────────────────────

/// Errors that can occur while parsing or resolving a flake lock file.
#[derive(Debug, thiserror::Error)]
pub enum FlakeLockError {
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported lock version {found} (expected {expected})")]
    UnsupportedVersion { expected: u32, found: u32 },
    #[error("missing root node `{0}`")]
    MissingRoot(String),
    #[error("node not found: {0}")]
    NodeNotFound(String),
    #[error("follows resolution failed for path {path:?} starting from `{from}`")]
    FollowsFailed { from: String, path: Vec<String> },
}

// ── Core types ──────────────────────────────────────────────

/// A parsed and validated flake.lock file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlakeLock {
    /// All nodes keyed by their name.
    pub nodes: BTreeMap<String, FlakeNode>,
    /// Name of the root node (usually `"root"`).
    pub root: String,
    /// Lock file schema version (must be 7).
    pub version: u32,
}

/// A single node in the input graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlakeNode {
    /// Inputs — maps input name to either a direct node reference or a follows
    /// path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, InputRef>,
    /// Pinned revision information (absent on the root node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<LockedInput>,
    /// Original (un-resolved) input reference (absent on the root node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<OriginalInput>,
    /// Whether this node is a flake (defaults to `true` when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flake: Option<bool>,
    /// Unknown fields (e.g. `parent` for path-typed flakes) — captured
    /// via `serde(flatten)` so they round-trip even when sui-compat
    /// doesn't know about them yet.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// A reference to another node in the input graph.
///
/// In the JSON encoding:
/// - A plain string (`"nixpkgs"`) means *direct* node reference.
/// - An array of strings (`["nixpkgs"]`) means *follows* — walk the path
///   starting from the **parent of the current node** (resolved later).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputRef {
    /// Direct reference to a named node.
    Direct(String),
    /// Follows path — resolve through the parent's input chain.
    Follows(Vec<String>),
}

/// Locked (pinned) revision of a flake input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedInput {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default, rename = "narHash", skip_serializing_if = "Option::is_none")]
    pub nar_hash: Option<String>,
    #[serde(
        default,
        rename = "lastModified",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_modified: Option<u64>,
    /// For `type = "path"` inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// For `type = "tarball"` or `type = "file"` inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// For specific git refs (e.g. `"refs/heads/main"`).
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Git directory (subdir within repo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Any other fields nix decides to add in the future (e.g.
    /// `revCount`, `submodules`, `shallow`). Flattened so they
    /// round-trip without losing data.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Original (un-locked) input specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalInput {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Branch/tag reference (e.g. `"nixos-unstable"`).
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Unknown fields (for forward compatibility).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

// ── Parsing ─────────────────────────────────────────────────

const SUPPORTED_VERSION: u32 = 7;

impl FlakeLock {
    /// Parse a `flake.lock` from its JSON text.
    pub fn parse(json: &str) -> Result<Self, FlakeLockError> {
        let lock: FlakeLock = serde_json::from_str(json)?;
        if lock.version != SUPPORTED_VERSION {
            return Err(FlakeLockError::UnsupportedVersion {
                expected: SUPPORTED_VERSION,
                found: lock.version,
            });
        }
        if !lock.nodes.contains_key(&lock.root) {
            return Err(FlakeLockError::MissingRoot(lock.root.clone()));
        }
        Ok(lock)
    }

    /// Serialize the lock back to pretty-printed JSON.
    pub fn to_json(&self) -> Result<String, FlakeLockError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Get the root node.
    pub fn root_node(&self) -> Result<&FlakeNode, FlakeLockError> {
        self.nodes
            .get(&self.root)
            .ok_or_else(|| FlakeLockError::MissingRoot(self.root.clone()))
    }

    /// Get a node by name.
    pub fn get_node(&self, name: &str) -> Result<&FlakeNode, FlakeLockError> {
        self.nodes
            .get(name)
            .ok_or_else(|| FlakeLockError::NodeNotFound(name.to_string()))
    }

    /// Return the direct inputs of the root node as `(input_name, node_name)` pairs,
    /// resolving follows along the way.
    pub fn root_inputs(&self) -> Result<Vec<(String, String)>, FlakeLockError> {
        let root = self.root_node()?;
        let mut out = Vec::new();
        for (input_name, input_ref) in &root.inputs {
            let resolved = self.resolve_ref(&self.root, input_ref)?;
            out.push((input_name.clone(), resolved));
        }
        Ok(out)
    }

    /// Resolve an `InputRef` to a concrete node name.
    ///
    /// - `Direct(name)` simply returns `name`.
    /// - `Follows(path)` walks the path from the **root** node (Nix semantics:
    ///   `["nixpkgs"]` means "follow root's nixpkgs input"; `["utils", "nixpkgs"]`
    ///   means "follow root -> utils -> nixpkgs").
    pub fn resolve_ref(
        &self,
        _parent: &str,
        input_ref: &InputRef,
    ) -> Result<String, FlakeLockError> {
        match input_ref {
            InputRef::Direct(name) => {
                if self.nodes.contains_key(name) {
                    Ok(name.clone())
                } else {
                    Err(FlakeLockError::NodeNotFound(name.clone()))
                }
            }
            InputRef::Follows(path) => self.resolve_follows_path(path),
        }
    }

    /// Walk a follows path starting from the root node.
    ///
    /// A path like `["nixpkgs"]` means: look up `root.inputs["nixpkgs"]` and
    /// resolve it. A path like `["utils", "systems"]` means: look up
    /// `root.inputs["utils"]`, find that node, then look up its `inputs["systems"]`.
    fn resolve_follows_path(&self, path: &[String]) -> Result<String, FlakeLockError> {
        if path.is_empty() {
            return Err(FlakeLockError::FollowsFailed {
                from: self.root.clone(),
                path: vec![],
            });
        }

        let mut current_node_name = self.root.clone();

        for segment in path {
            let node = self.nodes.get(&current_node_name).ok_or_else(|| {
                FlakeLockError::FollowsFailed {
                    from: current_node_name.clone(),
                    path: path.to_vec(),
                }
            })?;

            let input_ref =
                node.inputs.get(segment).ok_or_else(|| FlakeLockError::FollowsFailed {
                    from: current_node_name.clone(),
                    path: path.to_vec(),
                })?;

            // Recurse — the input itself could be another follows or a direct ref.
            current_node_name = match input_ref {
                InputRef::Direct(name) => name.clone(),
                InputRef::Follows(inner_path) => self.resolve_follows_path(inner_path)?,
            };
        }

        Ok(current_node_name)
    }

    /// Walk the input graph from the root following a dotted-style path.
    ///
    /// `resolve_input(&["utils", "nixpkgs"])` starts at root, enters the
    /// `utils` input, then enters that node's `nixpkgs` input, resolving any
    /// follows along the way.
    pub fn resolve_input(&self, path: &[&str]) -> Result<&FlakeNode, FlakeLockError> {
        let mut current_name = self.root.clone();

        for &segment in path {
            let node = self.nodes.get(&current_name).ok_or_else(|| {
                FlakeLockError::NodeNotFound(current_name.clone())
            })?;

            let input_ref = node.inputs.get(segment).ok_or_else(|| {
                FlakeLockError::NodeNotFound(format!("{current_name}.inputs.{segment}"))
            })?;

            current_name = self.resolve_ref(&current_name, input_ref)?;
        }

        self.nodes
            .get(&current_name)
            .ok_or_else(|| FlakeLockError::NodeNotFound(current_name))
    }

    /// Build an adjacency list representation of the full input graph.
    ///
    /// Returns `node_name -> [(input_name, resolved_target_node)]`.
    /// Follows are resolved; any unresolvable edges are silently skipped.
    pub fn adjacency_map(&self) -> BTreeMap<String, Vec<(String, String)>> {
        let mut map: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();

        for (node_name, node) in &self.nodes {
            let mut edges = Vec::new();
            for (input_name, input_ref) in &node.inputs {
                if let Ok(target) = self.resolve_ref(node_name, input_ref) {
                    edges.push((input_name.clone(), target));
                }
            }
            map.insert(node_name.clone(), edges);
        }

        map
    }
}

// ── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Fixtures ────────────────────────────────────────

    /// Minimal flake.lock — root with one direct input.
    fn minimal_lock_json() -> &'static str {
        r#"{
  "nodes": {
    "nixpkgs": {
      "locked": {
        "lastModified": 1700000000,
        "narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        "owner": "nixos",
        "repo": "nixpkgs",
        "rev": "abc123def456abc123def456abc123def456abc1",
        "type": "github"
      },
      "original": {
        "owner": "nixos",
        "ref": "nixos-unstable",
        "repo": "nixpkgs",
        "type": "github"
      }
    },
    "root": {
      "inputs": {
        "nixpkgs": "nixpkgs"
      }
    }
  },
  "root": "root",
  "version": 7
}"#
    }

    /// Flake.lock with follows: `utils` follows root's `nixpkgs`.
    fn follows_lock_json() -> &'static str {
        r#"{
  "nodes": {
    "nixpkgs": {
      "locked": {
        "lastModified": 1700000000,
        "narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        "owner": "nixos",
        "repo": "nixpkgs",
        "rev": "abc123def456abc123def456abc123def456abc1",
        "type": "github"
      },
      "original": {
        "owner": "nixos",
        "ref": "nixos-unstable",
        "repo": "nixpkgs",
        "type": "github"
      }
    },
    "root": {
      "inputs": {
        "nixpkgs": "nixpkgs",
        "utils": "utils"
      }
    },
    "systems": {
      "locked": {
        "lastModified": 1699999999,
        "narHash": "sha256-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=",
        "owner": "nix-systems",
        "repo": "default",
        "rev": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb1",
        "type": "github"
      },
      "original": {
        "owner": "nix-systems",
        "repo": "default",
        "type": "github"
      }
    },
    "utils": {
      "inputs": {
        "nixpkgs": [
          "nixpkgs"
        ],
        "systems": "systems"
      },
      "locked": {
        "lastModified": 1699999998,
        "narHash": "sha256-CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC=",
        "owner": "numtide",
        "repo": "flake-utils",
        "rev": "ccccccccccccccccccccccccccccccccccccccc1",
        "type": "github"
      },
      "original": {
        "owner": "numtide",
        "repo": "flake-utils",
        "type": "github"
      }
    }
  },
  "root": "root",
  "version": 7
}"#
    }

    /// Multi-level follows: `bar.nixpkgs` follows `["foo", "nixpkgs"]`,
    /// and `foo.nixpkgs` follows `["nixpkgs"]`.
    fn deep_follows_json() -> &'static str {
        r#"{
  "nodes": {
    "nixpkgs": {
      "locked": {
        "lastModified": 1700000000,
        "narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        "owner": "nixos",
        "repo": "nixpkgs",
        "rev": "abc123",
        "type": "github"
      },
      "original": {
        "owner": "nixos",
        "ref": "nixos-unstable",
        "repo": "nixpkgs",
        "type": "github"
      }
    },
    "root": {
      "inputs": {
        "bar": "bar",
        "foo": "foo",
        "nixpkgs": "nixpkgs"
      }
    },
    "foo": {
      "inputs": {
        "nixpkgs": [
          "nixpkgs"
        ]
      },
      "locked": {
        "lastModified": 1700000001,
        "narHash": "sha256-FOO",
        "owner": "example",
        "repo": "foo",
        "rev": "foofoo",
        "type": "github"
      },
      "original": {
        "owner": "example",
        "repo": "foo",
        "type": "github"
      }
    },
    "bar": {
      "inputs": {
        "nixpkgs": [
          "foo",
          "nixpkgs"
        ]
      },
      "locked": {
        "lastModified": 1700000002,
        "narHash": "sha256-BAR",
        "owner": "example",
        "repo": "bar",
        "rev": "barbar",
        "type": "github"
      },
      "original": {
        "owner": "example",
        "repo": "bar",
        "type": "github"
      }
    }
  },
  "root": "root",
  "version": 7
}"#
    }

    // ── Parse minimal ───────────────────────────────────

    #[test]
    fn parse_minimal_lock() {
        let lock = FlakeLock::parse(minimal_lock_json()).expect("parse failed");
        assert_eq!(lock.version, 7);
        assert_eq!(lock.root, "root");
        assert_eq!(lock.nodes.len(), 2);
        assert!(lock.nodes.contains_key("root"));
        assert!(lock.nodes.contains_key("nixpkgs"));
    }

    #[test]
    fn minimal_root_node_has_no_locked() {
        let lock = FlakeLock::parse(minimal_lock_json()).unwrap();
        let root = lock.root_node().unwrap();
        assert!(root.locked.is_none());
        assert!(root.original.is_none());
    }

    #[test]
    fn minimal_nixpkgs_locked_fields() {
        let lock = FlakeLock::parse(minimal_lock_json()).unwrap();
        let nixpkgs = lock.get_node("nixpkgs").unwrap();
        let locked = nixpkgs.locked.as_ref().expect("missing locked");
        assert_eq!(locked.source_type, "github");
        assert_eq!(locked.owner.as_deref(), Some("nixos"));
        assert_eq!(locked.repo.as_deref(), Some("nixpkgs"));
        assert_eq!(
            locked.rev.as_deref(),
            Some("abc123def456abc123def456abc123def456abc1"),
        );
        assert_eq!(locked.last_modified, Some(1_700_000_000));
        assert!(locked.nar_hash.is_some());
    }

    #[test]
    fn minimal_nixpkgs_original_fields() {
        let lock = FlakeLock::parse(minimal_lock_json()).unwrap();
        let nixpkgs = lock.get_node("nixpkgs").unwrap();
        let original = nixpkgs.original.as_ref().expect("missing original");
        assert_eq!(original.source_type, "github");
        assert_eq!(original.owner.as_deref(), Some("nixos"));
        assert_eq!(original.repo.as_deref(), Some("nixpkgs"));
        assert_eq!(original.git_ref.as_deref(), Some("nixos-unstable"));
    }

    #[test]
    fn minimal_root_inputs() {
        let lock = FlakeLock::parse(minimal_lock_json()).unwrap();
        let inputs = lock.root_inputs().unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0], ("nixpkgs".to_string(), "nixpkgs".to_string()));
    }

    // ── Parse with follows ──────────────────────────────

    #[test]
    fn parse_follows_lock() {
        let lock = FlakeLock::parse(follows_lock_json()).expect("parse failed");
        assert_eq!(lock.nodes.len(), 4); // root, nixpkgs, utils, systems
    }

    #[test]
    fn follows_utils_nixpkgs_resolves_to_root_nixpkgs() {
        let lock = FlakeLock::parse(follows_lock_json()).unwrap();
        let utils = lock.get_node("utils").unwrap();
        let nixpkgs_ref = &utils.inputs["nixpkgs"];
        assert_eq!(nixpkgs_ref, &InputRef::Follows(vec!["nixpkgs".to_string()]));

        // Resolve through the API.
        let resolved = lock.resolve_ref("utils", nixpkgs_ref).unwrap();
        assert_eq!(resolved, "nixpkgs");
    }

    #[test]
    fn resolve_input_walk_utils_nixpkgs() {
        let lock = FlakeLock::parse(follows_lock_json()).unwrap();
        // Walk root -> utils -> nixpkgs. The follows should land on the root
        // nixpkgs node.
        let node = lock.resolve_input(&["utils", "nixpkgs"]).unwrap();
        let locked = node.locked.as_ref().unwrap();
        assert_eq!(locked.owner.as_deref(), Some("nixos"));
        assert_eq!(locked.repo.as_deref(), Some("nixpkgs"));
    }

    #[test]
    fn resolve_input_walk_utils_systems() {
        let lock = FlakeLock::parse(follows_lock_json()).unwrap();
        let node = lock.resolve_input(&["utils", "systems"]).unwrap();
        let locked = node.locked.as_ref().unwrap();
        assert_eq!(locked.owner.as_deref(), Some("nix-systems"));
        assert_eq!(locked.repo.as_deref(), Some("default"));
    }

    // ── Deep follows ────────────────────────────────────

    #[test]
    fn deep_follows_bar_nixpkgs_resolves_through_foo() {
        let lock = FlakeLock::parse(deep_follows_json()).unwrap();

        // bar.nixpkgs follows ["foo", "nixpkgs"] which means:
        //   root -> foo -> nixpkgs
        // foo.nixpkgs follows ["nixpkgs"] which means:
        //   root -> nixpkgs
        // So bar.nixpkgs should ultimately resolve to the root nixpkgs node.
        let node = lock.resolve_input(&["bar", "nixpkgs"]).unwrap();
        let locked = node.locked.as_ref().unwrap();
        assert_eq!(locked.owner.as_deref(), Some("nixos"));
        assert_eq!(locked.rev.as_deref(), Some("abc123"));
    }

    #[test]
    fn deep_follows_foo_nixpkgs_resolves_to_root() {
        let lock = FlakeLock::parse(deep_follows_json()).unwrap();
        let node = lock.resolve_input(&["foo", "nixpkgs"]).unwrap();
        let locked = node.locked.as_ref().unwrap();
        assert_eq!(locked.owner.as_deref(), Some("nixos"));
    }

    // ── Adjacency map ───────────────────────────────────

    #[test]
    fn adjacency_map_follows_lock() {
        let lock = FlakeLock::parse(follows_lock_json()).unwrap();
        let adj = lock.adjacency_map();

        // root -> nixpkgs, utils
        let root_edges = &adj["root"];
        assert_eq!(root_edges.len(), 2);
        assert!(root_edges.contains(&("nixpkgs".to_string(), "nixpkgs".to_string())));
        assert!(root_edges.contains(&("utils".to_string(), "utils".to_string())));

        // utils -> nixpkgs (resolved from follows), systems
        let utils_edges = &adj["utils"];
        assert_eq!(utils_edges.len(), 2);
        assert!(utils_edges.contains(&("nixpkgs".to_string(), "nixpkgs".to_string())));
        assert!(utils_edges.contains(&("systems".to_string(), "systems".to_string())));

        // leaf nodes have no edges
        assert!(adj["nixpkgs"].is_empty());
        assert!(adj["systems"].is_empty());
    }

    // ── Error handling ──────────────────────────────────

    #[test]
    fn rejects_unsupported_version() {
        let json = r#"{ "nodes": { "root": { "inputs": {} } }, "root": "root", "version": 6 }"#;
        let err = FlakeLock::parse(json).unwrap_err();
        assert!(matches!(err, FlakeLockError::UnsupportedVersion { found: 6, .. }));
    }

    #[test]
    fn rejects_missing_root_node() {
        let json = r#"{ "nodes": { "x": {} }, "root": "root", "version": 7 }"#;
        let err = FlakeLock::parse(json).unwrap_err();
        assert!(matches!(err, FlakeLockError::MissingRoot(_)));
    }

    #[test]
    fn get_node_missing_returns_error() {
        let lock = FlakeLock::parse(minimal_lock_json()).unwrap();
        assert!(lock.get_node("nonexistent").is_err());
    }

    #[test]
    fn resolve_input_missing_segment_returns_error() {
        let lock = FlakeLock::parse(minimal_lock_json()).unwrap();
        let result = lock.resolve_input(&["nonexistent"]);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_ref_direct_missing_node_returns_error() {
        let lock = FlakeLock::parse(minimal_lock_json()).unwrap();
        let result = lock.resolve_ref("root", &InputRef::Direct("ghost".to_string()));
        assert!(result.is_err());
    }

    #[test]
    fn resolve_follows_empty_path_returns_error() {
        let lock = FlakeLock::parse(minimal_lock_json()).unwrap();
        let result = lock.resolve_ref("root", &InputRef::Follows(vec![]));
        assert!(result.is_err());
    }

    #[test]
    fn resolve_follows_bad_segment_returns_error() {
        let lock = FlakeLock::parse(follows_lock_json()).unwrap();
        let result = lock.resolve_ref(
            "utils",
            &InputRef::Follows(vec!["nonexistent".to_string()]),
        );
        assert!(result.is_err());
    }

    // ── Roundtrip (serialize → parse) ───────────────────

    #[test]
    fn roundtrip_minimal() {
        let original = FlakeLock::parse(minimal_lock_json()).unwrap();
        let json = original.to_json().unwrap();
        let reparsed = FlakeLock::parse(&json).unwrap();

        assert_eq!(reparsed.version, original.version);
        assert_eq!(reparsed.root, original.root);
        assert_eq!(reparsed.nodes.len(), original.nodes.len());

        // Verify locked data survives the trip.
        let np = reparsed.get_node("nixpkgs").unwrap();
        let locked = np.locked.as_ref().unwrap();
        assert_eq!(locked.rev.as_deref(), Some("abc123def456abc123def456abc123def456abc1"));
    }

    #[test]
    fn roundtrip_with_follows() {
        let original = FlakeLock::parse(follows_lock_json()).unwrap();
        let json = original.to_json().unwrap();
        let reparsed = FlakeLock::parse(&json).unwrap();

        assert_eq!(reparsed.nodes.len(), original.nodes.len());

        // Follows survived — utils.inputs.nixpkgs is still a follows path.
        let utils = reparsed.get_node("utils").unwrap();
        assert_eq!(
            utils.inputs["nixpkgs"],
            InputRef::Follows(vec!["nixpkgs".to_string()]),
        );

        // Resolution still works after roundtrip.
        let node = reparsed.resolve_input(&["utils", "nixpkgs"]).unwrap();
        assert_eq!(
            node.locked.as_ref().unwrap().owner.as_deref(),
            Some("nixos"),
        );
    }

    // ── Real-world-ish: non-flake input ─────────────────

    #[test]
    fn parse_non_flake_input() {
        let json = r#"{
  "nodes": {
    "data": {
      "flake": false,
      "locked": {
        "lastModified": 1700000000,
        "narHash": "sha256-DATA",
        "owner": "someone",
        "repo": "data-files",
        "rev": "deadbeef",
        "type": "github"
      },
      "original": {
        "owner": "someone",
        "repo": "data-files",
        "type": "github"
      }
    },
    "root": {
      "inputs": {
        "data": "data"
      }
    }
  },
  "root": "root",
  "version": 7
}"#;
        let lock = FlakeLock::parse(json).unwrap();
        let data = lock.get_node("data").unwrap();
        assert_eq!(data.flake, Some(false));
    }

    // ── InputRef serde ──────────────────────────────────

    #[test]
    fn input_ref_direct_deserialize() {
        let v: InputRef = serde_json::from_str(r#""nixpkgs""#).unwrap();
        assert_eq!(v, InputRef::Direct("nixpkgs".to_string()));
    }

    #[test]
    fn input_ref_follows_deserialize() {
        let v: InputRef = serde_json::from_str(r#"["nixpkgs"]"#).unwrap();
        assert_eq!(v, InputRef::Follows(vec!["nixpkgs".to_string()]));
    }

    #[test]
    fn input_ref_follows_multi_segment_deserialize() {
        let v: InputRef = serde_json::from_str(r#"["foo", "nixpkgs"]"#).unwrap();
        assert_eq!(
            v,
            InputRef::Follows(vec!["foo".to_string(), "nixpkgs".to_string()]),
        );
    }

    #[test]
    fn input_ref_direct_roundtrip() {
        let original = InputRef::Direct("nixpkgs".to_string());
        let json = serde_json::to_string(&original).unwrap();
        let reparsed: InputRef = serde_json::from_str(&json).unwrap();
        assert_eq!(original, reparsed);
    }

    #[test]
    fn input_ref_follows_roundtrip() {
        let original = InputRef::Follows(vec!["foo".to_string(), "bar".to_string()]);
        let json = serde_json::to_string(&original).unwrap();
        let reparsed: InputRef = serde_json::from_str(&json).unwrap();
        assert_eq!(original, reparsed);
    }

    // ── Path-type inputs ────────────────────────────────

    #[test]
    fn parse_path_type_locked_input() {
        let json = r#"{
  "nodes": {
    "local": {
      "locked": {
        "lastModified": 1700000000,
        "narHash": "sha256-PATH",
        "path": "/home/user/my-flake",
        "type": "path"
      },
      "original": {
        "type": "path",
        "url": "/home/user/my-flake"
      }
    },
    "root": {
      "inputs": {
        "local": "local"
      }
    }
  },
  "root": "root",
  "version": 7
}"#;
        let lock = FlakeLock::parse(json).unwrap();
        let local = lock.get_node("local").unwrap();
        let locked = local.locked.as_ref().unwrap();
        assert_eq!(locked.source_type, "path");
        assert_eq!(locked.path.as_deref(), Some("/home/user/my-flake"));
    }

    // ── Large graph: multiple follows chains ────────────

    #[test]
    fn multiple_inputs_follow_same_target() {
        let json = r#"{
  "nodes": {
    "nixpkgs": {
      "locked": {
        "lastModified": 1700000000,
        "narHash": "sha256-NP",
        "owner": "nixos",
        "repo": "nixpkgs",
        "rev": "aaa",
        "type": "github"
      },
      "original": { "owner": "nixos", "repo": "nixpkgs", "type": "github" }
    },
    "root": {
      "inputs": {
        "a": "a",
        "b": "b",
        "nixpkgs": "nixpkgs"
      }
    },
    "a": {
      "inputs": { "nixpkgs": ["nixpkgs"] },
      "locked": {
        "lastModified": 1, "narHash": "sha256-A", "owner": "x", "repo": "a", "rev": "a1", "type": "github"
      },
      "original": { "owner": "x", "repo": "a", "type": "github" }
    },
    "b": {
      "inputs": { "nixpkgs": ["nixpkgs"] },
      "locked": {
        "lastModified": 2, "narHash": "sha256-B", "owner": "x", "repo": "b", "rev": "b1", "type": "github"
      },
      "original": { "owner": "x", "repo": "b", "type": "github" }
    }
  },
  "root": "root",
  "version": 7
}"#;
        let lock = FlakeLock::parse(json).unwrap();

        // Both a and b follow root's nixpkgs.
        let a_np = lock.resolve_input(&["a", "nixpkgs"]).unwrap();
        let b_np = lock.resolve_input(&["b", "nixpkgs"]).unwrap();

        assert_eq!(
            a_np.locked.as_ref().unwrap().rev.as_deref(),
            Some("aaa"),
        );
        assert_eq!(
            b_np.locked.as_ref().unwrap().rev.as_deref(),
            Some("aaa"),
        );
    }

    // ── Malformed JSON ──────────────────────────────────

    #[test]
    fn invalid_json_returns_error() {
        assert!(FlakeLock::parse("not json").is_err());
    }

    #[test]
    fn empty_object_returns_error() {
        // Missing required fields.
        assert!(FlakeLock::parse("{}").is_err());
    }
}
