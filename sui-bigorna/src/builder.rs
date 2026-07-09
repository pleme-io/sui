//! The builder topology + the typed `docker buildx` invocation builders.
//!
//! A [`BuilderTopology`] is a buildx builder described as a set of native
//! per-arch nodes (§node — native by construction). It renders to the exact
//! `docker buildx create --append` / `use` / `inspect` / `rm` argv that
//! registers those nodes, and a [`BuildSpec`] renders the `docker buildx build`
//! argv a consumer would otherwise type unchanged. Every argv is a typed
//! [`DockerBuildInvocation`] (reused from `sui-dockerfile-wrapper`) built via
//! typed `.push()`; compound flag values render through their `Display` impls —
//! there is no `format!()` of an argv anywhere.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sui_dockerfile_wrapper::DockerBuildInvocation;

use crate::cache_front::CacheFront;
use crate::node::{BuildxDriver, NativeArchNode, NodeError, NodeSpec};
use crate::platform::{Platform, PlatformList};

const DOCKER: &str = "docker";

/// A buildx builder as a set of native per-arch nodes. Native by construction:
/// every member is a [`NativeArchNode`], so the topology can never contain an
/// emulated node and "no QEMU" is a property of the type, not a runtime probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderTopology {
    name: String,
    driver: BuildxDriver,
    nodes: Vec<NativeArchNode>,
    bootstrap: bool,
    use_as_default: bool,
}

impl BuilderTopology {
    /// Validate a list of [`NodeSpec`]s into a native topology.
    ///
    /// # Errors
    ///
    /// Returns [`TopologyError::NoNodes`] if `nodes` is empty, or
    /// [`TopologyError::Node`] if any node fails the native guard (an emulated
    /// node).
    pub fn from_specs(
        name: impl Into<String>,
        driver: BuildxDriver,
        nodes: Vec<NodeSpec>,
        bootstrap: bool,
        use_as_default: bool,
    ) -> Result<Self, TopologyError> {
        if nodes.is_empty() {
            return Err(TopologyError::NoNodes);
        }
        let nodes = nodes
            .into_iter()
            .map(NativeArchNode::try_from)
            .collect::<Result<Vec<_>, NodeError>>()?;
        Ok(Self { name: name.into(), driver, nodes, bootstrap, use_as_default })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn nodes(&self) -> &[NativeArchNode] {
        &self.nodes
    }

    /// Every platform this topology serves natively.
    #[must_use]
    pub fn covered_platforms(&self) -> Vec<Platform> {
        self.nodes.iter().map(|n| n.platform().clone()).collect()
    }

    /// The emulation verdict for a requested platform set. Because the topology
    /// is native by construction, a covered platform is served natively; an
    /// uncovered one would need QEMU — which bigorna refuses to register — so it
    /// is reported as needing a native node rather than silently emulated.
    #[must_use]
    pub fn emulation_free_for(&self, requested: &[Platform]) -> EmulationVerdict {
        let missing: Vec<Platform> = requested
            .iter()
            .filter(|req| !self.nodes.iter().any(|n| n.platform().same_native_target(req)))
            .cloned()
            .collect();
        if missing.is_empty() {
            EmulationVerdict::NativeForAll
        } else {
            EmulationVerdict::NeedsNativeNode(missing)
        }
    }

    /// The ordered `docker buildx create` invocations that register this
    /// topology: one `create` for the first node, one `create --append` per
    /// subsequent node.
    #[must_use]
    pub fn create_invocations(&self) -> Vec<DockerBuildInvocation> {
        let mut out = Vec::with_capacity(self.nodes.len());
        for (i, node) in self.nodes.iter().enumerate() {
            let mut args = vec!["buildx".to_string(), "create".to_string()];
            args.push("--name".to_string());
            args.push(self.name.clone());
            if i == 0 {
                args.push("--driver".to_string());
                args.push(self.driver.as_flag().to_string());
                if self.bootstrap {
                    args.push("--bootstrap".to_string());
                }
                if self.use_as_default {
                    args.push("--use".to_string());
                }
            } else {
                args.push("--append".to_string());
            }
            args.push("--node".to_string());
            args.push(node.name().to_string());
            args.push("--platform".to_string());
            args.push(node.platform().to_string());
            for opt in node.driver_opts() {
                args.push("--driver-opt".to_string());
                args.push(opt.to_string());
            }
            if let Some(endpoint) = node.endpoint() {
                args.push(endpoint.to_string());
            }
            out.push(DockerBuildInvocation { program: DOCKER.to_string(), args });
        }
        out
    }

    /// `docker buildx use <name>`.
    #[must_use]
    pub fn use_invocation(&self) -> DockerBuildInvocation {
        DockerBuildInvocation {
            program: DOCKER.to_string(),
            args: vec!["buildx".to_string(), "use".to_string(), self.name.clone()],
        }
    }

    /// `docker buildx inspect [--bootstrap] <name>`.
    #[must_use]
    pub fn inspect_invocation(&self, bootstrap: bool) -> DockerBuildInvocation {
        inspect_invocation(&self.name, bootstrap)
    }

    /// `docker buildx rm <name>`.
    #[must_use]
    pub fn rm_invocation(&self) -> DockerBuildInvocation {
        rm_invocation(&self.name)
    }
}

/// `docker buildx inspect [--bootstrap] <builder>` — free function so a caller
/// can inspect a builder by name without a full topology.
#[must_use]
pub fn inspect_invocation(builder: &str, bootstrap: bool) -> DockerBuildInvocation {
    let mut args = vec!["buildx".to_string(), "inspect".to_string()];
    if bootstrap {
        args.push("--bootstrap".to_string());
    }
    args.push(builder.to_string());
    DockerBuildInvocation { program: DOCKER.to_string(), args }
}

/// `docker buildx rm <builder>`.
#[must_use]
pub fn rm_invocation(builder: &str) -> DockerBuildInvocation {
    DockerBuildInvocation {
        program: DOCKER.to_string(),
        args: vec!["buildx".to_string(), "rm".to_string(), builder.to_string()],
    }
}

/// The verdict on whether a requested platform set is served natively.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum EmulationVerdict {
    /// Every requested platform maps to a native node — no QEMU in the path.
    NativeForAll,
    /// One or more requested platforms have no native node; they would require
    /// emulation, which is refused. Add a native node for each.
    NeedsNativeNode(Vec<Platform>),
}

impl EmulationVerdict {
    #[must_use]
    pub fn is_native_for_all(&self) -> bool {
        matches!(self, EmulationVerdict::NativeForAll)
    }
}

/// Where a build's result goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BuildOutput {
    /// No `--load`/`--push` — build into the builder cache only (the default,
    /// and the only correct one for a genuine multi-arch build, since a
    /// multi-platform image can't be `--load`ed into the local docker store).
    #[default]
    None,
    /// `--load` — load a single-platform result into the local docker store.
    Load,
    /// `--push` — push the (multi-platform) image + its manifest list.
    Push,
    /// `--output type=oci,dest=<path>` — write an OCI archive.
    Oci { dest: PathBuf },
}

/// A `docker buildx build` invocation described as typed data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSpec {
    /// The builder to run on (`--builder`). Usually the bigorna topology name.
    pub builder: String,
    /// `--platform` list — the multi-arch request.
    pub platforms: PlatformList,
    /// `-f` Dockerfile path.
    pub dockerfile: PathBuf,
    /// The build context (positional).
    pub context: PathBuf,
    /// `-t` tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// `--build-arg K=V` pairs.
    #[serde(default)]
    pub build_args: BTreeMap<String, String>,
    /// The cache front (`--cache-from` / `--cache-to`).
    #[serde(default)]
    pub cache: CacheFront,
    /// Where the result goes.
    #[serde(default)]
    pub output: BuildOutput,
}

impl BuildSpec {
    /// Render the typed `docker buildx build …` invocation.
    #[must_use]
    pub fn invocation(&self) -> DockerBuildInvocation {
        let mut args = vec!["buildx".to_string(), "build".to_string()];
        args.push("--builder".to_string());
        args.push(self.builder.clone());
        if !self.platforms.is_empty() {
            args.push("--platform".to_string());
            args.push(self.platforms.to_string());
        }
        for ep in &self.cache.from {
            args.push("--cache-from".to_string());
            // Import token: a `local` endpoint MUST render `src=<path>` here,
            // not `dest=` — buildx rejects a `dest=` on `--cache-from` with
            // `local cache importer requires src`. Every other wire renders the
            // same either way. See `cache_front::CacheDirection`.
            args.push(ep.to_import_token());
        }
        for ep in &self.cache.to {
            args.push("--cache-to".to_string());
            args.push(ep.to_string());
        }
        for (k, v) in &self.build_args {
            args.push("--build-arg".to_string());
            let mut kv = String::with_capacity(k.len() + v.len() + 1);
            kv.push_str(k);
            kv.push('=');
            kv.push_str(v);
            args.push(kv);
        }
        match &self.output {
            BuildOutput::None => {}
            BuildOutput::Load => args.push("--load".to_string()),
            BuildOutput::Push => args.push("--push".to_string()),
            BuildOutput::Oci { dest } => {
                args.push("--output".to_string());
                let mut spec = String::from("type=oci,dest=");
                spec.push_str(&dest.display().to_string());
                args.push(spec);
            }
        }
        args.push("-f".to_string());
        args.push(self.dockerfile.display().to_string());
        for tag in &self.tags {
            args.push("-t".to_string());
            args.push(tag.clone());
        }
        args.push(self.context.display().to_string());
        DockerBuildInvocation { program: DOCKER.to_string(), args }
    }
}

/// Errors building a topology.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TopologyError {
    #[error("a builder topology needs at least one native node")]
    NoNodes,
    #[error(transparent)]
    Node(#[from] NodeError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_front::{CacheEndpoint, CacheMode};
    use crate::node::DriverOpt;

    fn two_node_specs() -> Vec<NodeSpec> {
        vec![
            NodeSpec {
                name: "bigorna-amd64".to_string(),
                platform: Platform::linux_amd64(),
                host_arch: crate::platform::Arch::Amd64,
                endpoint: Some("amd64-context".to_string()),
                driver_opts: vec![],
            },
            NodeSpec {
                name: "bigorna-arm64".to_string(),
                platform: Platform::linux_arm64(),
                host_arch: crate::platform::Arch::Arm64,
                endpoint: Some("arm64-context".to_string()),
                driver_opts: vec![],
            },
        ]
    }

    #[test]
    fn create_invocations_are_typed_argv_not_string_concat() {
        let topo = BuilderTopology::from_specs(
            "bigorna",
            BuildxDriver::DockerContainer,
            two_node_specs(),
            true,
            true,
        )
        .unwrap();
        let invs = topo.create_invocations();
        assert_eq!(invs.len(), 2);

        // First node: create --name bigorna --driver docker-container
        //             --bootstrap --use --node bigorna-amd64
        //             --platform linux/amd64 amd64-context
        assert_eq!(
            invs[0].args,
            vec![
                "buildx", "create", "--name", "bigorna", "--driver", "docker-container",
                "--bootstrap", "--use", "--node", "bigorna-amd64",
                "--platform", "linux/amd64", "amd64-context",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );

        // Second node: create --name bigorna --append --node bigorna-arm64
        //              --platform linux/arm64 arm64-context
        assert_eq!(
            invs[1].args,
            vec![
                "buildx", "create", "--name", "bigorna", "--append", "--node",
                "bigorna-arm64", "--platform", "linux/arm64", "arm64-context",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn kubernetes_driver_opts_render_a_nodeselector() {
        let specs = vec![NodeSpec {
            name: "bigorna-arm64".to_string(),
            platform: Platform::linux_arm64(),
            host_arch: crate::platform::Arch::Arm64,
            endpoint: None,
            driver_opts: vec![DriverOpt::new("nodeselector", "kubernetes.io/arch=arm64")],
        }];
        let topo =
            BuilderTopology::from_specs("bigorna", BuildxDriver::Kubernetes, specs, false, false)
                .unwrap();
        let invs = topo.create_invocations();
        assert!(invs[0].args.contains(&"--driver-opt".to_string()));
        assert!(invs[0].args.contains(&"nodeselector=kubernetes.io/arch=arm64".to_string()));
        assert!(invs[0].args.contains(&"kubernetes".to_string()));
    }

    #[test]
    fn emulation_verdict_is_native_for_covered_platforms() {
        let topo = BuilderTopology::from_specs(
            "bigorna",
            BuildxDriver::DockerContainer,
            two_node_specs(),
            false,
            false,
        )
        .unwrap();
        let requested = vec![Platform::linux_amd64(), Platform::linux_arm64()];
        assert_eq!(topo.emulation_free_for(&requested), EmulationVerdict::NativeForAll);

        // Requesting a platform with no native node → reported, never emulated.
        let requested = vec![Platform::linux_amd64(), "linux/s390x".parse().unwrap()];
        match topo.emulation_free_for(&requested) {
            EmulationVerdict::NeedsNativeNode(missing) => {
                assert_eq!(missing, vec!["linux/s390x".parse().unwrap()]);
            }
            EmulationVerdict::NativeForAll => panic!("s390x has no native node"),
        }
    }

    #[test]
    fn build_invocation_carries_platforms_and_cache_on_one_surface() {
        let spec = BuildSpec {
            builder: "bigorna".to_string(),
            platforms: PlatformList(vec![Platform::linux_amd64(), Platform::linux_arm64()]),
            dockerfile: PathBuf::from("Dockerfile"),
            context: PathBuf::from("."),
            tags: vec!["ghcr.io/pleme-io/example:test".to_string()],
            build_args: BTreeMap::new(),
            cache: CacheFront {
                from: vec![CacheEndpoint::Registry {
                    r#ref: "ghcr.io/pleme-io/camada:buildcache".to_string(),
                    mode: None,
                }],
                to: vec![CacheEndpoint::Registry {
                    r#ref: "ghcr.io/pleme-io/camada:buildcache".to_string(),
                    mode: Some(CacheMode::Max),
                }],
            },
            output: BuildOutput::Push,
        };
        let inv = spec.invocation();
        // native multi-arch (--platform) + cache (--cache-from/to) on ONE argv.
        assert_eq!(inv.args[0], "buildx");
        assert_eq!(inv.args[1], "build");
        assert!(inv.args.contains(&"--platform".to_string()));
        assert!(inv.args.contains(&"linux/amd64,linux/arm64".to_string()));
        assert!(inv.args.contains(&"--cache-from".to_string()));
        assert!(inv.args.contains(&"--cache-to".to_string()));
        assert!(inv.args.contains(&"--push".to_string()));
        assert_eq!(inv.args.last().unwrap(), ".");
    }

    #[test]
    fn empty_topology_is_rejected() {
        let err = BuilderTopology::from_specs(
            "bigorna",
            BuildxDriver::DockerContainer,
            vec![],
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(err, TopologyError::NoNodes);
    }
}
