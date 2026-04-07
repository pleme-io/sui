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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    Online,
    Offline,
    Deploying,
    Failed,
    Unknown,
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

/// A fleet node definition.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
}

impl Node {
    /// Create a new node with the given hostname and flake reference.
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
        }
    }

    /// Set the SSH target for remote deployments.
    pub fn with_ssh(mut self, target: &str) -> Self {
        self.ssh_target = Some(target.to_string());
        self
    }

    /// Set the groups this node belongs to.
    pub fn with_groups(mut self, groups: Vec<String>) -> Self {
        self.groups = groups;
        self
    }

    /// Set the Nix system string (e.g. `x86_64-linux`).
    pub fn with_system(mut self, system: &str) -> Self {
        self.system = Some(system.to_string());
        self
    }

    /// The SSH target for deployment (user@host or just host).
    pub fn deploy_target(&self) -> &str {
        self.ssh_target.as_deref().unwrap_or(&self.hostname)
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

    /// Returns the number of nodes in the registry.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns `true` if the registry contains no nodes.
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
}
