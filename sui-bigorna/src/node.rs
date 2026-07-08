//! The native per-arch build node — and the guard that makes an *emulated*
//! node unrepresentable in a native topology.
//!
//! The whole point of `bigorna` is to register a real, native build node for
//! each target architecture so `docker buildx build --platform amd64,arm64`
//! dispatches to native metal instead of QEMU. That guarantee is structural,
//! not a runtime check: [`NativeArchNode`] can only be constructed when the
//! node's *host* architecture equals its *target* architecture. A node that
//! would target an arch it doesn't natively run (i.e. an emulated node) has no
//! constructor — [`NativeArchNode::try_new`] rejects it at the boundary, so a
//! [`super::builder::BuilderTopology`] built from these nodes contains no
//! emulated members by construction. There is deliberately no `EmulatedNode`
//! type and no `QemuFallbackNode`.

use serde::{Deserialize, Serialize};

use crate::platform::{Arch, Platform};

/// The buildx driver a builder is created with. Selects *how* the native nodes
/// are reached — a closed set, so a new driver is a compile error, never a
/// silently-mishandled string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BuildxDriver {
    /// `docker-container` — `BuildKit` in a container reached via a docker
    /// context (the local-proof + single-host shape).
    #[default]
    DockerContainer,
    /// `kubernetes` — one `BuildKit` pod per node, arch-pinned by a
    /// `nodeselector` driver-opt (the in-cluster shape).
    Kubernetes,
    /// `remote` — a `BuildKit` daemon reached over a `tcp://`/`unix://`
    /// endpoint (the append-native-endpoint shape).
    Remote,
}

impl BuildxDriver {
    /// The token buildx expects after `--driver`.
    #[must_use]
    pub fn as_flag(self) -> &'static str {
        match self {
            BuildxDriver::DockerContainer => "docker-container",
            BuildxDriver::Kubernetes => "kubernetes",
            BuildxDriver::Remote => "remote",
        }
    }
}

/// A single `--driver-opt key=value` pair (e.g.
/// `nodeselector=kubernetes.io/arch=arm64`). Rendering the `key=value` join
/// lives in the [`std::fmt::Display`] impl, not a `format!()` at the call
/// site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverOpt {
    pub key: String,
    pub value: String,
}

impl DriverOpt {
    #[must_use]
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self { key: key.into(), value: value.into() }
    }
}

impl std::fmt::Display for DriverOpt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={}", self.key, self.value)
    }
}

/// The serde-facing description of a node, before the native-vs-emulated guard
/// runs. This is the shape a YAML/JSON config carries; it becomes a
/// [`NativeArchNode`] only by passing through [`NativeArchNode::try_from`],
/// which is the parse boundary that rejects an emulated node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSpec {
    /// buildx node name (`bigorna-amd64`).
    pub name: String,
    /// The platform this node builds natively.
    pub platform: Platform,
    /// The host architecture this node actually runs on. MUST equal
    /// `platform.arch` — the guard that keeps the node native.
    pub host_arch: Arch,
    /// The append endpoint (a docker context name for `docker-container`, a
    /// `tcp://`/`unix://` URL for `remote`). `None` for the create-time first
    /// node and for `kubernetes` (arch is chosen by `driver_opts`).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Per-node `--driver-opt` pairs (e.g. the k8s `nodeselector`).
    #[serde(default)]
    pub driver_opts: Vec<DriverOpt>,
}

/// A build node proven native: its host architecture equals its target
/// architecture. Construction is the only gate; once you hold one, it is
/// native by type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeArchNode {
    name: String,
    platform: Platform,
    host_arch: Arch,
    endpoint: Option<String>,
    driver_opts: Vec<DriverOpt>,
}

impl NativeArchNode {
    /// Construct a native node, rejecting a host/target arch mismatch (an
    /// emulated node) at the boundary.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::Emulated`] when `host_arch != platform.arch` — the
    /// only way an emulated node is ever surfaced, and never as a constructed
    /// value.
    pub fn try_new(
        name: impl Into<String>,
        platform: Platform,
        host_arch: Arch,
        endpoint: Option<String>,
        driver_opts: Vec<DriverOpt>,
    ) -> Result<Self, NodeError> {
        let name = name.into();
        if host_arch != platform.arch {
            return Err(NodeError::Emulated {
                node: name,
                host_arch,
                target_arch: platform.arch,
            });
        }
        Ok(Self { name, platform, host_arch, endpoint, driver_opts })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn platform(&self) -> &Platform {
        &self.platform
    }

    #[must_use]
    pub fn host_arch(&self) -> Arch {
        self.host_arch
    }

    #[must_use]
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    #[must_use]
    pub fn driver_opts(&self) -> &[DriverOpt] {
        &self.driver_opts
    }
}

impl TryFrom<NodeSpec> for NativeArchNode {
    type Error = NodeError;

    fn try_from(spec: NodeSpec) -> Result<Self, Self::Error> {
        Self::try_new(spec.name, spec.platform, spec.host_arch, spec.endpoint, spec.driver_opts)
    }
}

/// Errors from the node guard.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NodeError {
    #[error(
        "node `{node}` would run on host arch {host_arch} but targets {target_arch}: \
         that is an EMULATED (QEMU) node, which bigorna refuses to register — \
         provide a native {target_arch} node instead"
    )]
    Emulated {
        node: String,
        host_arch: Arch,
        target_arch: Arch,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_native_node_constructs() {
        let node = NativeArchNode::try_new(
            "bigorna-arm64",
            Platform::linux_arm64(),
            Arch::Arm64,
            None,
            vec![],
        )
        .unwrap();
        assert_eq!(node.name(), "bigorna-arm64");
        assert_eq!(node.host_arch(), Arch::Arm64);
    }

    #[test]
    fn an_emulated_node_has_no_constructor() {
        // host arm64 targeting amd64 == QEMU == refused at the boundary.
        let err = NativeArchNode::try_new(
            "bigorna-amd64-emulated",
            Platform::linux_amd64(),
            Arch::Arm64,
            None,
            vec![],
        )
        .unwrap_err();
        assert_eq!(
            err,
            NodeError::Emulated {
                node: "bigorna-amd64-emulated".to_string(),
                host_arch: Arch::Arm64,
                target_arch: Arch::Amd64,
            }
        );
    }

    #[test]
    fn node_spec_deserializes_and_guards() {
        let yaml = "\
name: bigorna-arm64
platform: linux/arm64
host_arch: arm64
";
        let spec: NodeSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let node = NativeArchNode::try_from(spec).unwrap();
        assert_eq!(node.platform(), &Platform::linux_arm64());
    }

    #[test]
    fn driver_opt_renders_key_equals_value() {
        let opt = DriverOpt::new("nodeselector", "kubernetes.io/arch=arm64");
        assert_eq!(opt.to_string(), "nodeselector=kubernetes.io/arch=arm64");
    }
}
