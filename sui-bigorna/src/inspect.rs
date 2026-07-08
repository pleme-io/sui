//! Faithful parse of `docker buildx inspect` output.
//!
//! Inspect is bigorna's *liveness confirmation*: it proves the builder exists
//! and each declared node is present + running + advertising its platforms. It
//! is deliberately NOT where the anti-QEMU guarantee lives — that guarantee is
//! structural (a [`super::builder::BuilderTopology`] is native by construction,
//! §node). Inspect confirms the topology was actually created; the topology
//! proves it is emulation-free.

use serde::{Deserialize, Serialize};

use crate::platform::Platform;

/// One node as reported by `docker buildx inspect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectedNode {
    pub name: String,
    pub status: String,
    /// Platforms the node advertises. Tokens that don't parse into the closed
    /// [`Platform`] set (e.g. an exotic arch) are skipped rather than failing
    /// the whole parse — inspect is best-effort observation, not the parse
    /// boundary.
    pub platforms: Vec<Platform>,
}

impl InspectedNode {
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.status.eq_ignore_ascii_case("running")
    }
}

/// The parsed inspect report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectReport {
    pub builder: String,
    pub driver: String,
    pub nodes: Vec<InspectedNode>,
}

impl InspectReport {
    /// Parse the stdout of `docker buildx inspect <builder>`.
    ///
    /// The format is a flat `Key: Value` block for the builder, then a `Nodes:`
    /// header followed by one `Key: Value` block per node. This parser tracks
    /// the current node once the header is seen and skips the assorted
    /// `Labels:` / `GC Policy` sub-blocks.
    #[must_use]
    pub fn parse(stdout: &str) -> Self {
        let mut builder = String::new();
        let mut driver = String::new();
        let mut nodes: Vec<InspectedNode> = Vec::new();
        let mut in_nodes = false;

        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();

            if !in_nodes {
                match key {
                    "Name" => builder = value.to_string(),
                    "Driver" => driver = value.to_string(),
                    "Nodes" => in_nodes = true,
                    _ => {}
                }
                continue;
            }

            match key {
                // A new node block starts at each "Name:" inside the Nodes section.
                "Name" => nodes.push(InspectedNode {
                    name: value.to_string(),
                    status: String::new(),
                    platforms: Vec::new(),
                }),
                "Status" => {
                    if let Some(node) = nodes.last_mut() {
                        node.status = value.to_string();
                    }
                }
                "Platforms" => {
                    if let Some(node) = nodes.last_mut() {
                        node.platforms = parse_platform_list(value);
                    }
                }
                _ => {}
            }
        }

        Self { builder, driver, nodes }
    }

    /// Every distinct platform advertised across running nodes.
    #[must_use]
    pub fn running_platforms(&self) -> Vec<Platform> {
        let mut out: Vec<Platform> = Vec::new();
        for node in self.nodes.iter().filter(|n| n.is_running()) {
            for p in &node.platforms {
                if !out.contains(p) {
                    out.push(p.clone());
                }
            }
        }
        out
    }

    /// Whether a running node advertises a native target matching `platform`.
    #[must_use]
    pub fn has_running_node_for(&self, platform: &Platform) -> bool {
        self.nodes
            .iter()
            .filter(|n| n.is_running())
            .any(|n| n.platforms.iter().any(|p| p.same_native_target(platform)))
    }
}

/// Parse a `Platforms:` value: comma-separated, each token possibly suffixed
/// with `*` (preferred) or wrapped like `linux/amd64 (+2)`. Unparseable tokens
/// are skipped.
fn parse_platform_list(value: &str) -> Vec<Platform> {
    value
        .split(',')
        .filter_map(|tok| {
            let cleaned = tok.trim().trim_end_matches('*').trim();
            // Drop a trailing " (+N)" microarch-count annotation.
            let head = cleaned.split_whitespace().next().unwrap_or(cleaned);
            head.parse::<Platform>().ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A REAL `docker buildx inspect default` capture on the arm64 workstation
    // (podman-backed, BuildKit v0.30.0). The single default node is native
    // arm64; amd64 is only reachable via QEMU on this host.
    const REAL_DEFAULT_INSPECT: &str = "\
Name:          default
Driver:        docker-container
Last Activity: 2026-07-08 18:51:48 +0000 UTC

Nodes:
Name:             default
Endpoint:         default
Status:           running
BuildKit version: v0.30.0
Platforms:        linux/arm64, linux/amd64, linux/amd64/v2, linux/386
Labels:
 org.mobyproject.buildkit.worker.executor:         oci
GC Policy rule#0:
 All:            false
";

    #[test]
    fn parses_the_real_default_builder() {
        let report = InspectReport::parse(REAL_DEFAULT_INSPECT);
        assert_eq!(report.builder, "default");
        assert_eq!(report.driver, "docker-container");
        assert_eq!(report.nodes.len(), 1);
        let node = &report.nodes[0];
        assert_eq!(node.name, "default");
        assert!(node.is_running());
        assert!(node.platforms.contains(&Platform::linux_arm64()));
        assert!(node.platforms.contains(&Platform::linux_amd64()));
    }

    #[test]
    fn parses_a_two_node_native_builder() {
        let out = "\
Name:          bigorna
Driver:        docker-container

Nodes:
Name:             bigorna-amd64
Status:           running
Platforms:        linux/amd64*, linux/amd64/v2
Name:             bigorna-arm64
Status:           running
Platforms:        linux/arm64*, linux/arm64/v8
";
        let report = InspectReport::parse(out);
        assert_eq!(report.nodes.len(), 2);
        assert!(report.has_running_node_for(&Platform::linux_amd64()));
        assert!(report.has_running_node_for(&Platform::linux_arm64()));
        let running = report.running_platforms();
        assert!(running.contains(&Platform::linux_amd64()));
        assert!(running.contains(&Platform::linux_arm64()));
    }

    #[test]
    fn star_and_microarch_annotations_are_stripped() {
        let list = parse_platform_list("linux/amd64* , linux/arm64 (+2), linux/386");
        assert_eq!(
            list,
            vec![
                Platform::linux_amd64(),
                Platform::linux_arm64(),
                "linux/386".parse().unwrap(),
            ]
        );
    }
}
