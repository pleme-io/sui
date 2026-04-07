//! Fleet node registry and status tracking.

use std::collections::BTreeMap;

/// Errors from node lifecycle operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NodeError {
    /// The requested node was not found in the registry.
    #[error("node not found: {hostname}")]
    NotFound {
        /// Hostname that was looked up.
        hostname: String,
    },
    /// A node with this hostname is already registered.
    #[error("node already registered: {hostname}")]
    AlreadyRegistered {
        /// Hostname of the duplicate node.
        hostname: String,
    },
}

/// Status of a fleet node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum NodeStatus {
    Online,
    Offline,
    Deploying,
    Failed,
    Unknown,
}

impl Default for NodeStatus {
    fn default() -> Self {
        Self::Unknown
    }
}

impl std::fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Online => write!(f, "online"),
            Self::Offline => write!(f, "offline"),
            Self::Deploying => write!(f, "deploying"),
            Self::Failed => write!(f, "failed"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

impl NodeStatus {
    /// Returns `true` if the node is reachable and not in a failure state.
    #[must_use]
    pub const fn is_healthy(self) -> bool {
        matches!(self, Self::Online)
    }

    /// Returns `true` if the node is in a transitional state (deploying).
    #[must_use]
    pub const fn is_transitional(self) -> bool {
        matches!(self, Self::Deploying)
    }
}

impl std::str::FromStr for NodeStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "online" => Ok(Self::Online),
            "offline" => Ok(Self::Offline),
            "deploying" => Ok(Self::Deploying),
            "failed" => Ok(Self::Failed),
            "unknown" => Ok(Self::Unknown),
            other => Err(format!("invalid node status: {other}")),
        }
    }
}

/// A fleet node definition.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Node {
    /// The hostname used to identify this node.
    pub hostname: String,
    /// Flake reference for this node's configuration (e.g. `.#alpha`).
    pub flake_ref: String,
    /// Optional SSH target (e.g. `root@10.0.0.1`); falls back to hostname.
    pub ssh_target: Option<String>,
    /// Optional Nix system string (e.g. `x86_64-linux`, `aarch64-darwin`).
    pub system: Option<String>,
    /// Groups this node belongs to (used for `@group` target resolution).
    pub groups: Vec<String>,
    /// Current status of the node.
    pub status: NodeStatus,
    /// The current system generation number, if known.
    pub current_generation: Option<i64>,
    /// Unix timestamp of the last successful deployment.
    pub last_deployed: Option<i64>,
    /// Hostnames this node depends on. When deploying with
    /// [`DeployOrder::Dependency`](crate::fleet::DeployOrder), each node listed
    /// here is deployed before this node. `#[serde(default)]` so existing
    /// serialized nodes parse without modification.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

impl Node {
    /// Create a new node with the given hostname and flake reference.
    #[must_use]
    pub fn new(hostname: &str, flake_ref: &str) -> Self {
        Self {
            hostname: hostname.to_string(),
            flake_ref: flake_ref.to_string(),
            ssh_target: None,
            system: None,
            groups: vec![],
            status: NodeStatus::Unknown,
            current_generation: None,
            last_deployed: None,
            depends_on: vec![],
        }
    }

    /// Set the dependency list for this node.
    ///
    /// Each entry should be the `hostname` of another node in the same
    /// registry. When deploying with [`DeployOrder::Dependency`](crate::fleet::DeployOrder),
    /// dependencies are deployed before this node.
    #[must_use]
    pub fn with_depends_on(mut self, deps: Vec<String>) -> Self {
        self.depends_on = deps;
        self
    }

    /// Set the SSH target for remote deployments.
    #[must_use]
    pub fn with_ssh(mut self, target: &str) -> Self {
        self.ssh_target = Some(target.to_string());
        self
    }

    /// Set the groups this node belongs to.
    #[must_use]
    pub fn with_groups(mut self, groups: Vec<String>) -> Self {
        self.groups = groups;
        self
    }

    /// Set the Nix system string (e.g. `x86_64-linux`).
    #[must_use]
    pub fn with_system(mut self, system: &str) -> Self {
        self.system = Some(system.to_string());
        self
    }

    /// The SSH target for deployment (user@host or just host).
    #[must_use]
    pub fn deploy_target(&self) -> &str {
        self.ssh_target.as_deref().unwrap_or(&self.hostname)
    }

    /// Returns `true` if this node runs a Darwin (macOS) system.
    #[must_use]
    pub fn is_darwin(&self) -> bool {
        matches!(
            self.system.as_deref(),
            Some("aarch64-darwin") | Some("x86_64-darwin")
        )
    }

    /// Returns the appropriate rebuild command for this node's system.
    #[must_use]
    pub fn rebuild_command(&self) -> &'static str {
        if self.is_darwin() {
            "darwin-rebuild"
        } else {
            "nixos-rebuild"
        }
    }
}

/// Fleet node registry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeRegistry {
    nodes: BTreeMap<String, Node>,
}

impl Default for NodeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeRegistry {
    /// Create an empty node registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
        }
    }

    /// Returns `true` if a node with the given hostname exists.
    #[must_use]
    pub fn contains(&self, hostname: &str) -> bool {
        self.nodes.contains_key(hostname)
    }

    /// Add a node to the registry, keyed by its hostname.
    ///
    /// Overwrites any existing node with the same hostname.
    pub fn add(&mut self, node: Node) {
        self.nodes.insert(node.hostname.clone(), node);
    }

    /// Add a node, returning an error if the hostname is already present.
    pub fn try_add(&mut self, node: Node) -> Result<(), NodeError> {
        if self.nodes.contains_key(&node.hostname) {
            return Err(NodeError::AlreadyRegistered {
                hostname: node.hostname,
            });
        }
        self.nodes.insert(node.hostname.clone(), node);
        Ok(())
    }

    /// Look up a node by hostname.
    #[must_use]
    pub fn get(&self, hostname: &str) -> Option<&Node> {
        self.nodes.get(hostname)
    }

    /// Look up a node by hostname, returning a typed error if absent.
    pub fn get_or_err(&self, hostname: &str) -> Result<&Node, NodeError> {
        self.nodes.get(hostname).ok_or_else(|| NodeError::NotFound {
            hostname: hostname.to_owned(),
        })
    }

    /// Look up a node mutably by hostname.
    pub fn get_mut(&mut self, hostname: &str) -> Option<&mut Node> {
        self.nodes.get_mut(hostname)
    }

    /// Look up a node mutably, returning a typed error if absent.
    pub fn get_mut_or_err(&mut self, hostname: &str) -> Result<&mut Node, NodeError> {
        self.nodes
            .get_mut(hostname)
            .ok_or_else(|| NodeError::NotFound {
                hostname: hostname.to_owned(),
            })
    }

    /// Remove a node by hostname, returning it if found.
    pub fn remove(&mut self, hostname: &str) -> Option<Node> {
        self.nodes.remove(hostname)
    }

    /// Remove a node by hostname, returning a typed error if absent.
    pub fn remove_or_err(&mut self, hostname: &str) -> Result<Node, NodeError> {
        self.nodes.remove(hostname).ok_or_else(|| NodeError::NotFound {
            hostname: hostname.to_owned(),
        })
    }

    /// Iterate over all nodes in sorted (hostname) order.
    pub fn all(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    /// Iterate over hostnames in sorted order.
    pub fn hostnames(&self) -> impl Iterator<Item = &str> {
        self.nodes.keys().map(String::as_str)
    }

    /// Iterate over nodes matching a given status.
    pub fn by_status(&self, status: NodeStatus) -> impl Iterator<Item = &Node> {
        self.nodes.values().filter(move |n| n.status == status)
    }

    /// Returns the number of nodes in the registry.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns `true` if the registry contains no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Resolve a target string to a list of nodes.
    ///
    /// - `@group` — all nodes in the group
    /// - `hostname` — single node
    /// - `@all` — all nodes
    pub fn resolve_target(&self, target: &str) -> Vec<&Node> {
        if target == "@all" {
            return self.nodes.values().collect();
        }
        if let Some(group) = target.strip_prefix('@') {
            return self
                .nodes
                .values()
                .filter(|n| n.groups.iter().any(|g| g == group))
                .collect();
        }
        self.nodes.get(target).into_iter().collect()
    }

    /// Count nodes by status.
    #[must_use]
    pub fn status_counts(&self) -> StatusCounts {
        let mut counts = StatusCounts::default();
        for node in self.nodes.values() {
            match node.status {
                NodeStatus::Online => counts.online += 1,
                NodeStatus::Offline => counts.offline += 1,
                NodeStatus::Deploying => counts.deploying += 1,
                NodeStatus::Failed => counts.failed += 1,
                NodeStatus::Unknown => counts.unknown += 1,
            }
        }
        counts.total = self.nodes.len();
        counts
    }
}

impl<'a> IntoIterator for &'a NodeRegistry {
    type Item = (&'a String, &'a Node);
    type IntoIter = std::collections::btree_map::Iter<'a, String, Node>;

    fn into_iter(self) -> Self::IntoIter {
        self.nodes.iter()
    }
}

impl IntoIterator for NodeRegistry {
    type Item = (String, Node);
    type IntoIter = std::collections::btree_map::IntoIter<String, Node>;

    fn into_iter(self) -> Self::IntoIter {
        self.nodes.into_iter()
    }
}

impl std::iter::FromIterator<Node> for NodeRegistry {
    fn from_iter<I: IntoIterator<Item = Node>>(iter: I) -> Self {
        let mut registry = Self::new();
        for node in iter {
            registry.add(node);
        }
        registry
    }
}

impl Extend<Node> for NodeRegistry {
    fn extend<I: IntoIterator<Item = Node>>(&mut self, iter: I) {
        for node in iter {
            self.add(node);
        }
    }
}

/// Aggregate counts of node statuses across a fleet.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatusCounts {
    /// Total number of nodes.
    pub total: usize,
    /// Nodes in [`NodeStatus::Online`] state.
    pub online: usize,
    /// Nodes in [`NodeStatus::Offline`] state.
    pub offline: usize,
    /// Nodes in [`NodeStatus::Deploying`] state.
    pub deploying: usize,
    /// Nodes in [`NodeStatus::Failed`] state.
    pub failed: usize,
    /// Nodes in [`NodeStatus::Unknown`] state.
    pub unknown: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_registry() -> NodeRegistry {
        let mut reg = NodeRegistry::new();
        reg.add(
            Node::new("plo", ".#plo")
                .with_ssh("luis@plo.local")
                .with_groups(vec!["production".to_string(), "k3s".to_string()])
                .with_system("x86_64-linux"),
        );
        reg.add(
            Node::new("zek", ".#zek")
                .with_ssh("luis@zek.local")
                .with_groups(vec!["staging".to_string(), "k3s".to_string()])
                .with_system("x86_64-linux"),
        );
        reg.add(
            Node::new("cid", ".#cid")
                .with_groups(vec!["darwin".to_string()])
                .with_system("aarch64-darwin"),
        );
        reg
    }

    #[test]
    fn registry_basics() {
        let reg = sample_registry();
        assert_eq!(reg.len(), 3);
        assert!(reg.get("plo").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn resolve_single_node() {
        let reg = sample_registry();
        let nodes = reg.resolve_target("plo");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].hostname, "plo");
    }

    #[test]
    fn resolve_group() {
        let reg = sample_registry();
        let nodes = reg.resolve_target("@k3s");
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn resolve_all() {
        let reg = sample_registry();
        let nodes = reg.resolve_target("@all");
        assert_eq!(nodes.len(), 3);
    }

    #[test]
    fn resolve_nonexistent() {
        let reg = sample_registry();
        let nodes = reg.resolve_target("ghost");
        assert!(nodes.is_empty());
        let nodes = reg.resolve_target("@nonexistent");
        assert!(nodes.is_empty());
    }

    #[test]
    fn status_counts() {
        let mut reg = sample_registry();
        reg.get_mut("plo").unwrap().status = NodeStatus::Online;
        reg.get_mut("zek").unwrap().status = NodeStatus::Online;
        reg.get_mut("cid").unwrap().status = NodeStatus::Offline;

        let counts = reg.status_counts();
        assert_eq!(counts.total, 3);
        assert_eq!(counts.online, 2);
        assert_eq!(counts.offline, 1);
    }

    #[test]
    fn deploy_target() {
        let reg = sample_registry();
        assert_eq!(reg.get("plo").unwrap().deploy_target(), "luis@plo.local");
        assert_eq!(reg.get("cid").unwrap().deploy_target(), "cid");
    }

    #[test]
    fn node_serialization() {
        let node = Node::new("test", ".#test").with_ssh("user@host");
        let json = serde_json::to_string(&node).unwrap();
        let parsed: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.hostname, "test");
        assert_eq!(parsed.ssh_target, Some("user@host".to_string()));
    }

    #[test]
    fn node_builder() {
        let node = Node::new("n1", ".#n1")
            .with_ssh("root@10.0.0.1")
            .with_groups(vec!["prod".to_string()])
            .with_system("x86_64-linux");
        assert_eq!(node.deploy_target(), "root@10.0.0.1");
        assert_eq!(node.groups, vec!["prod"]);
        assert_eq!(node.system, Some("x86_64-linux".to_string()));
    }

    // ── NodeStatus Display ────────────────────────────────────

    #[test]
    fn node_status_display() {
        assert_eq!(NodeStatus::Online.to_string(), "online");
        assert_eq!(NodeStatus::Offline.to_string(), "offline");
        assert_eq!(NodeStatus::Deploying.to_string(), "deploying");
        assert_eq!(NodeStatus::Failed.to_string(), "failed");
        assert_eq!(NodeStatus::Unknown.to_string(), "unknown");
    }

    // ── NodeStatus serde roundtrip ────────────────────────────

    #[test]
    fn node_status_serde_roundtrip() {
        for status in [
            NodeStatus::Online,
            NodeStatus::Offline,
            NodeStatus::Deploying,
            NodeStatus::Failed,
            NodeStatus::Unknown,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: NodeStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    // ── NodeRegistry Default ──────────────────────────────────

    #[test]
    fn node_registry_default() {
        let reg = NodeRegistry::default();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    // ── NodeRegistry remove ───────────────────────────────────

    #[test]
    fn registry_remove() {
        let mut reg = sample_registry();
        assert_eq!(reg.len(), 3);
        let removed = reg.remove("plo");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().hostname, "plo");
        assert_eq!(reg.len(), 2);
        assert!(reg.get("plo").is_none());
    }

    #[test]
    fn registry_remove_nonexistent() {
        let mut reg = sample_registry();
        let removed = reg.remove("ghost");
        assert!(removed.is_none());
        assert_eq!(reg.len(), 3);
    }

    // ── NodeRegistry is_empty ─────────────────────────────────

    #[test]
    fn registry_is_empty() {
        let reg = NodeRegistry::new();
        assert!(reg.is_empty());

        let mut reg = NodeRegistry::new();
        reg.add(Node::new("a", ".#a"));
        assert!(!reg.is_empty());
    }

    // ── NodeRegistry all() iteration order ────────────────────

    #[test]
    fn registry_all_sorted_order() {
        let reg = sample_registry();
        let hostnames: Vec<&str> = reg.all().map(|n| n.hostname.as_str()).collect();
        assert_eq!(hostnames, vec!["cid", "plo", "zek"]);
    }

    // ── NodeRegistry serde roundtrip ──────────────────────────

    #[test]
    fn registry_serde_roundtrip() {
        let reg = sample_registry();
        let json = serde_json::to_string(&reg).unwrap();
        let parsed: NodeRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 3);
        assert!(parsed.get("plo").is_some());
        assert_eq!(
            parsed.get("plo").unwrap().ssh_target,
            Some("luis@plo.local".to_string())
        );
    }

    // ── StatusCounts default ──────────────────────────────────

    #[test]
    fn status_counts_default() {
        let counts = StatusCounts::default();
        assert_eq!(counts.total, 0);
        assert_eq!(counts.online, 0);
        assert_eq!(counts.offline, 0);
        assert_eq!(counts.deploying, 0);
        assert_eq!(counts.failed, 0);
        assert_eq!(counts.unknown, 0);
    }

    // ── StatusCounts serde roundtrip ──────────────────────────

    #[test]
    fn status_counts_serde_roundtrip() {
        let counts = StatusCounts {
            total: 5,
            online: 3,
            offline: 1,
            deploying: 0,
            failed: 1,
            unknown: 0,
        };
        let json = serde_json::to_string(&counts).unwrap();
        let parsed: StatusCounts = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, counts);
    }

    // ── StatusCounts all states ───────────────────────────────

    #[test]
    fn status_counts_all_states() {
        let mut reg = NodeRegistry::new();
        reg.add(Node::new("a", ".#a"));
        reg.add(Node::new("b", ".#b"));
        reg.add(Node::new("c", ".#c"));
        reg.add(Node::new("d", ".#d"));
        reg.add(Node::new("e", ".#e"));

        reg.get_mut("a").unwrap().status = NodeStatus::Online;
        reg.get_mut("b").unwrap().status = NodeStatus::Offline;
        reg.get_mut("c").unwrap().status = NodeStatus::Deploying;
        reg.get_mut("d").unwrap().status = NodeStatus::Failed;
        // "e" stays Unknown

        let counts = reg.status_counts();
        assert_eq!(counts.total, 5);
        assert_eq!(counts.online, 1);
        assert_eq!(counts.offline, 1);
        assert_eq!(counts.deploying, 1);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.unknown, 1);
    }

    // ── Node defaults ─────────────────────────────────────────

    #[test]
    fn node_new_defaults() {
        let node = Node::new("host", ".#host");
        assert_eq!(node.hostname, "host");
        assert_eq!(node.flake_ref, ".#host");
        assert_eq!(node.ssh_target, None);
        assert_eq!(node.system, None);
        assert!(node.groups.is_empty());
        assert_eq!(node.status, NodeStatus::Unknown);
        assert_eq!(node.current_generation, None);
        assert_eq!(node.last_deployed, None);
        assert!(node.depends_on.is_empty());
    }

    // ── resolve_target with multiple groups ───────────────────

    #[test]
    fn resolve_target_group_overlap() {
        let reg = sample_registry();
        let k3s = reg.resolve_target("@k3s");
        assert_eq!(k3s.len(), 2);
        let hostnames: Vec<&str> = k3s.iter().map(|n| n.hostname.as_str()).collect();
        assert!(hostnames.contains(&"plo"));
        assert!(hostnames.contains(&"zek"));
    }

    // ── Node overwrite in registry ────────────────────────────

    #[test]
    fn registry_add_overwrites_existing() {
        let mut reg = NodeRegistry::new();
        reg.add(Node::new("host", ".#old"));
        reg.add(Node::new("host", ".#new"));
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.get("host").unwrap().flake_ref, ".#new");
    }

    // ── try_add: detects duplicates ───────────────────────────

    #[test]
    fn try_add_succeeds_on_first_insert() {
        let mut reg = NodeRegistry::new();
        let result = reg.try_add(Node::new("alpha", ".#alpha"));
        assert!(result.is_ok());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn try_add_rejects_duplicate_hostname() {
        let mut reg = NodeRegistry::new();
        reg.try_add(Node::new("alpha", ".#alpha-v1")).unwrap();
        let result = reg.try_add(Node::new("alpha", ".#alpha-v2"));
        match result {
            Err(NodeError::AlreadyRegistered { hostname }) => {
                assert_eq!(hostname, "alpha");
            }
            other => panic!("expected AlreadyRegistered, got {other:?}"),
        }
        // First insert is preserved
        assert_eq!(reg.get("alpha").unwrap().flake_ref, ".#alpha-v1");
        assert_eq!(reg.len(), 1);
    }

    // ── get_or_err / get_mut_or_err / remove_or_err ───────────

    #[test]
    fn get_or_err_returns_node_when_present() {
        let reg = sample_registry();
        let node = reg.get_or_err("plo").unwrap();
        assert_eq!(node.hostname, "plo");
    }

    #[test]
    fn get_or_err_returns_not_found_when_absent() {
        let reg = sample_registry();
        let result = reg.get_or_err("ghost");
        match result {
            Err(NodeError::NotFound { hostname }) => {
                assert_eq!(hostname, "ghost");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn get_mut_or_err_mutates_existing_node() {
        let mut reg = sample_registry();
        let node = reg.get_mut_or_err("plo").unwrap();
        node.status = NodeStatus::Online;
        node.current_generation = Some(42);
        assert_eq!(reg.get("plo").unwrap().status, NodeStatus::Online);
        assert_eq!(reg.get("plo").unwrap().current_generation, Some(42));
    }

    #[test]
    fn get_mut_or_err_errors_for_missing_node() {
        let mut reg = sample_registry();
        let result = reg.get_mut_or_err("ghost");
        match result {
            Err(NodeError::NotFound { hostname }) => assert_eq!(hostname, "ghost"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn remove_or_err_returns_owned_node() {
        let mut reg = sample_registry();
        let node = reg.remove_or_err("plo").unwrap();
        assert_eq!(node.hostname, "plo");
        assert!(!reg.contains("plo"));
    }

    #[test]
    fn remove_or_err_errors_for_missing_node() {
        let mut reg = sample_registry();
        let result = reg.remove_or_err("ghost");
        match result {
            Err(NodeError::NotFound { hostname }) => assert_eq!(hostname, "ghost"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    // ── NodeError display strings ─────────────────────────────

    #[test]
    fn node_error_not_found_display() {
        let e = NodeError::NotFound {
            hostname: "missing".to_string(),
        };
        let s = e.to_string();
        assert!(s.contains("not found"));
        assert!(s.contains("missing"));
    }

    #[test]
    fn node_error_already_registered_display() {
        let e = NodeError::AlreadyRegistered {
            hostname: "dup".to_string(),
        };
        let s = e.to_string();
        assert!(s.contains("already registered"));
        assert!(s.contains("dup"));
    }

    // ── contains ──────────────────────────────────────────────

    #[test]
    fn contains_returns_true_for_present_node() {
        let reg = sample_registry();
        assert!(reg.contains("plo"));
        assert!(reg.contains("zek"));
        assert!(reg.contains("cid"));
    }

    #[test]
    fn contains_returns_false_for_absent_node() {
        let reg = sample_registry();
        assert!(!reg.contains("ghost"));
        assert!(!reg.contains(""));
    }

    // ── hostnames() iteration order ───────────────────────────

    #[test]
    fn hostnames_yields_sorted_order() {
        let reg = sample_registry();
        let hostnames: Vec<&str> = reg.hostnames().collect();
        assert_eq!(hostnames, vec!["cid", "plo", "zek"]);
    }

    // ── by_status ─────────────────────────────────────────────

    #[test]
    fn by_status_filters_only_matching_nodes() {
        let mut reg = sample_registry();
        reg.get_mut("plo").unwrap().status = NodeStatus::Online;
        reg.get_mut("zek").unwrap().status = NodeStatus::Failed;
        // cid stays Unknown

        let online: Vec<&Node> = reg.by_status(NodeStatus::Online).collect();
        assert_eq!(online.len(), 1);
        assert_eq!(online[0].hostname, "plo");

        let failed: Vec<&Node> = reg.by_status(NodeStatus::Failed).collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].hostname, "zek");

        let unknown: Vec<&Node> = reg.by_status(NodeStatus::Unknown).collect();
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].hostname, "cid");
    }

    // ── IntoIterator (&NodeRegistry) ──────────────────────────

    #[test]
    fn into_iterator_borrowed_yields_pairs() {
        let reg = sample_registry();
        let pairs: Vec<(&String, &Node)> = (&reg).into_iter().collect();
        assert_eq!(pairs.len(), 3);
        // BTreeMap iteration is sorted
        assert_eq!(pairs[0].0, "cid");
        assert_eq!(pairs[1].0, "plo");
        assert_eq!(pairs[2].0, "zek");
    }

    // ── IntoIterator (owned NodeRegistry) ─────────────────────

    #[test]
    fn into_iterator_owned_yields_owned_pairs() {
        let reg = sample_registry();
        let pairs: Vec<(String, Node)> = reg.into_iter().collect();
        assert_eq!(pairs.len(), 3);
        let hostnames: Vec<&str> = pairs.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(hostnames, vec!["cid", "plo", "zek"]);
    }

    // ── FromIterator<Node> ────────────────────────────────────

    #[test]
    fn from_iterator_constructs_registry() {
        let nodes = vec![
            Node::new("zeta", ".#zeta"),
            Node::new("alpha", ".#alpha"),
            Node::new("delta", ".#delta"),
        ];
        let reg: NodeRegistry = nodes.into_iter().collect();
        assert_eq!(reg.len(), 3);
        // sorted ordering preserved
        let hostnames: Vec<&str> = reg.hostnames().collect();
        assert_eq!(hostnames, vec!["alpha", "delta", "zeta"]);
    }

    #[test]
    fn from_iterator_collapses_duplicates() {
        let nodes = vec![
            Node::new("host", ".#first"),
            Node::new("host", ".#second"),
        ];
        let reg: NodeRegistry = nodes.into_iter().collect();
        assert_eq!(reg.len(), 1);
        // last write wins (matches add() semantics)
        assert_eq!(reg.get("host").unwrap().flake_ref, ".#second");
    }

    // ── Extend<Node> ──────────────────────────────────────────

    #[test]
    fn extend_appends_to_existing_registry() {
        let mut reg = NodeRegistry::new();
        reg.add(Node::new("alpha", ".#alpha"));
        reg.extend(vec![
            Node::new("beta", ".#beta"),
            Node::new("gamma", ".#gamma"),
        ]);
        assert_eq!(reg.len(), 3);
        assert!(reg.contains("alpha"));
        assert!(reg.contains("beta"));
        assert!(reg.contains("gamma"));
    }

    // ── NodeStatus FromStr ────────────────────────────────────

    #[test]
    fn node_status_from_str_valid_values() {
        use std::str::FromStr;
        assert_eq!(NodeStatus::from_str("online").unwrap(), NodeStatus::Online);
        assert_eq!(NodeStatus::from_str("offline").unwrap(), NodeStatus::Offline);
        assert_eq!(NodeStatus::from_str("deploying").unwrap(), NodeStatus::Deploying);
        assert_eq!(NodeStatus::from_str("failed").unwrap(), NodeStatus::Failed);
        assert_eq!(NodeStatus::from_str("unknown").unwrap(), NodeStatus::Unknown);
    }

    #[test]
    fn node_status_from_str_rejects_garbage() {
        use std::str::FromStr;
        let err = NodeStatus::from_str("garbage").unwrap_err();
        assert!(err.contains("invalid node status"));
        assert!(err.contains("garbage"));
    }

    #[test]
    fn node_status_from_str_is_case_sensitive() {
        use std::str::FromStr;
        assert!(NodeStatus::from_str("Online").is_err());
        assert!(NodeStatus::from_str("ONLINE").is_err());
        assert!(NodeStatus::from_str("").is_err());
    }

    // ── NodeStatus is_healthy / is_transitional ───────────────

    #[test]
    fn node_status_is_healthy_only_for_online() {
        assert!(NodeStatus::Online.is_healthy());
        assert!(!NodeStatus::Offline.is_healthy());
        assert!(!NodeStatus::Deploying.is_healthy());
        assert!(!NodeStatus::Failed.is_healthy());
        assert!(!NodeStatus::Unknown.is_healthy());
    }

    #[test]
    fn node_status_is_transitional_only_for_deploying() {
        assert!(!NodeStatus::Online.is_transitional());
        assert!(!NodeStatus::Offline.is_transitional());
        assert!(NodeStatus::Deploying.is_transitional());
        assert!(!NodeStatus::Failed.is_transitional());
        assert!(!NodeStatus::Unknown.is_transitional());
    }

    // ── NodeStatus default ────────────────────────────────────

    #[test]
    fn node_status_default_is_unknown() {
        assert_eq!(NodeStatus::default(), NodeStatus::Unknown);
    }

    // ── Node::is_darwin ───────────────────────────────────────

    #[test]
    fn node_is_darwin_for_aarch64_darwin() {
        let node = Node::new("mac", ".#mac").with_system("aarch64-darwin");
        assert!(node.is_darwin());
    }

    #[test]
    fn node_is_darwin_for_x86_64_darwin() {
        let node = Node::new("mac", ".#mac").with_system("x86_64-darwin");
        assert!(node.is_darwin());
    }

    #[test]
    fn node_is_darwin_false_for_linux() {
        let node = Node::new("nix", ".#nix").with_system("x86_64-linux");
        assert!(!node.is_darwin());
    }

    #[test]
    fn node_is_darwin_false_when_system_unset() {
        let node = Node::new("ghost", ".#ghost");
        assert!(!node.is_darwin());
    }

    // ── Node::rebuild_command ─────────────────────────────────

    #[test]
    fn node_rebuild_command_darwin() {
        let node = Node::new("cid", ".#cid").with_system("aarch64-darwin");
        assert_eq!(node.rebuild_command(), "darwin-rebuild");
    }

    #[test]
    fn node_rebuild_command_nixos() {
        let node = Node::new("plo", ".#plo").with_system("x86_64-linux");
        assert_eq!(node.rebuild_command(), "nixos-rebuild");
    }

    #[test]
    fn node_rebuild_command_defaults_to_nixos_when_system_unset() {
        let node = Node::new("ghost", ".#ghost");
        assert_eq!(node.rebuild_command(), "nixos-rebuild");
    }
}
