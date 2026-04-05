//! Fleet node registry and status tracking.

use std::collections::BTreeMap;

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
    pub hostname: String,
    pub flake_ref: String,
    pub ssh_target: Option<String>,
    pub system: Option<String>,
    pub groups: Vec<String>,
    pub status: NodeStatus,
    pub current_generation: Option<i64>,
    pub last_deployed: Option<i64>,
}

impl Node {
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

    pub fn with_ssh(mut self, target: &str) -> Self {
        self.ssh_target = Some(target.to_string());
        self
    }

    pub fn with_groups(mut self, groups: Vec<String>) -> Self {
        self.groups = groups;
        self
    }

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

impl NodeRegistry {
    pub fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
        }
    }

    pub fn add(&mut self, node: Node) {
        self.nodes.insert(node.hostname.clone(), node);
    }

    pub fn get(&self, hostname: &str) -> Option<&Node> {
        self.nodes.get(hostname)
    }

    pub fn get_mut(&mut self, hostname: &str) -> Option<&mut Node> {
        self.nodes.get_mut(hostname)
    }

    pub fn remove(&mut self, hostname: &str) -> Option<Node> {
        self.nodes.remove(hostname)
    }

    pub fn all(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

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

/// Fleet status counts.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StatusCounts {
    pub total: usize,
    pub online: usize,
    pub offline: usize,
    pub deploying: usize,
    pub failed: usize,
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
}
