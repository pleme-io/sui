//! canteiro — a CI run decomposed into ONE typed content-addressed DAG (M0).
//!
//! `theory/CANTEIRO.md` §5 (M0) lands exactly three things: the `CiNode` type
//! (the sole unifying type, §1), the [`decompose`] morphism
//! (`CiRun → (shigoto Dag over JobId  +  JobId→CiNode map)`, the "dandori"
//! decompose), and `CiNode` as a shigoto [`Job`] ([`CiNodeJob`]). This module
//! is that core.
//!
//! Deliberately NOT here, each named as its own later increment (tier-honest
//! per CANTEIRO §7, never rounded up):
//! - in-process wave execution via `shigoto::InProcessScheduler` + the in-pod
//!   runner entrypoint — **M1**, the node→worker crux (needs `shigoto-scheduler`
//!   + a live ARC runner). This module gives the scheduler its DAG + its Jobs;
//!   it does not drive them.
//! - the caixa `:kind Acao` authoring vocabulary (CANTEIRO §4) — **M1**.
//! - `gen_pdc::ContentAddr` as the real dedup/cache identity — **M2** (§7);
//!   [`ContentAddr`] here is an M0 placeholder over the node's action.
//! - `EnvClass` beyond `None`: `LocalStack` needs cofre cross-org secrets into
//!   the camelot-ci runners (**M1**, §5); `WarmPoolClaim` is the viveiro pool
//!   (**DESIGN**, §7). Both are typed so the axis exists from M0; neither is wired.

use std::collections::{HashMap, HashSet};

use shigoto_dag::Dag;
use shigoto_types::{JobId, JobKindId, JobScope, JobSubject};

/// The shigoto job-kind every canteiro CI node carries.
const NODE_KIND: &str = "canteiro.node";

/// M0 content address — a deterministic node identity derived from the node's
/// action. At M0 it is the raw keying material (name + command + args,
/// unit-separated); it becomes `gen_pdc::ContentAddr` (BLAKE3 over the node's
/// real build inputs, the dedup key = cache key = incremental boundary) when
/// the L1/L2/L8 legs wire in (CANTEIRO §1, §7 — the M2 leg). Kept a newtype so
/// that promotion is a type change at one site, not a fleet-wide edit.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentAddr(pub String);

/// The environment a CI node needs. M0 exercises only [`EnvClass::None`];
/// the other arms are typed so the demand axis exists from M0 but are not wired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvClass {
    /// No environment — pure build/lint/test that needs nothing live.
    None,
    /// A local akeyless stack brought up on the runner (M1 — needs cofre
    /// cross-org secrets, CANTEIRO §5).
    LocalStack,
    /// A warm live tenant claimed from the viveiro pool (DESIGN, §7). The
    /// `String` is the `EnvSpec` reference; a typed ref lands with the pool.
    WarmPoolClaim(String),
}

/// The runnable step a node executes. M0 = a command + args; the caixa
/// `:kind Acao` authoring vocabulary (CANTEIRO §4) is M1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRef {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

/// One typed CI node — the SOLE unifying type (CANTEIRO §1). A CI run is a set
/// of these; [`decompose`] turns them into a shigoto DAG.
#[derive(Debug, Clone)]
pub struct CiNode {
    /// Unique node name within its [`CiRun`] (the DAG subject).
    pub name: String,
    pub content_addr: ContentAddr,
    pub env_class: EnvClass,
    pub action: ActionRef,
    /// Names of the nodes this one depends on. Each becomes a DAG edge
    /// `dep → this`; every entry must be declared in the same [`CiRun`].
    pub deps: Vec<String>,
}

impl CiNode {
    /// Construct a node, deriving its M0 content address from the action.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        env_class: EnvClass,
        action: ActionRef,
        deps: Vec<String>,
    ) -> Self {
        let name = name.into();
        let content_addr = content_addr_for(&action);
        Self {
            name,
            content_addr,
            env_class,
            action,
            deps,
        }
    }
}

/// Deterministic M0 content-address material — injective in the action's
/// fields. No `format!()` (★★ TYPED EMISSION): parts are pushed with an ASCII
/// unit separator so no field value can forge another's boundary.
fn content_addr_for(action: &ActionRef) -> ContentAddr {
    const SEP: char = '\u{1f}';
    let mut s = String::new();
    s.push_str(&action.name);
    s.push(SEP);
    s.push_str(&action.command);
    for a in &action.args {
        s.push(SEP);
        s.push_str(a);
    }
    ContentAddr(s)
}

/// The input to [`decompose`]: a repo's CI run as a set of declared nodes.
#[derive(Debug, Clone)]
pub struct CiRun {
    pub workspace: String,
    pub repo: String,
    pub nodes: Vec<CiNode>,
}

impl CiRun {
    /// The shigoto [`JobId`] for a node in this run: scope = the repo, kind =
    /// the canteiro node kind, subject = the pinned node name.
    #[must_use]
    pub fn job_id(&self, node_name: &str) -> JobId {
        JobId {
            scope: JobScope::Repo {
                workspace: self.workspace.clone(),
                repo: self.repo.clone(),
            },
            kind: JobKindId::new(NODE_KIND),
            subject: JobSubject::Pinned(node_name.to_string()),
        }
    }
}

/// The decompose target: the shipped shigoto [`Dag`] (monomorphic over
/// [`JobId`]) PLUS the consumer-owned `JobId → CiNode` map. CANTEIRO §1:
/// "`Dag<CiNode>` is shorthand for exactly this pair."
pub struct CanteiroDag {
    pub dag: Dag,
    pub nodes: HashMap<JobId, CiNode>,
}

impl CanteiroDag {
    /// Topological order of the nodes (parents before children). `decompose`
    /// already rejects cycles, so an error here is an internal invariant break.
    ///
    /// # Errors
    /// Returns the shigoto DAG error if the graph is unexpectedly cyclic.
    pub fn topo_order(&self) -> Result<Vec<JobId>, shigoto_dag::DagError> {
        self.dag.toposort()
    }
}

/// Every illegal CI-run shape is a typed rejection, never a silent bad DAG.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecomposeError {
    #[error("duplicate node name: {0}")]
    DuplicateNode(String),
    #[error("node {node} depends on undeclared node {dep}")]
    UnknownDep { node: String, dep: String },
    #[error("the CI run has a dependency cycle")]
    Cycle,
}

/// **dandori** — the decompose morphism (CANTEIRO §1). A total function from a
/// [`CiRun`] to a shigoto [`Dag`] over [`JobId`] plus the `JobId → CiNode` map.
///
/// # Errors
/// - [`DecomposeError::DuplicateNode`] — two nodes share a name.
/// - [`DecomposeError::UnknownDep`] — a `deps` entry names no declared node.
/// - [`DecomposeError::Cycle`] — the dependency graph is cyclic.
pub fn decompose(run: &CiRun) -> Result<CanteiroDag, DecomposeError> {
    // Names must be unique — they are the DAG subjects.
    let mut names: HashSet<&str> = HashSet::new();
    for n in &run.nodes {
        if !names.insert(n.name.as_str()) {
            return Err(DecomposeError::DuplicateNode(n.name.clone()));
        }
    }

    let mut dag = Dag::new();
    let mut nodes = HashMap::new();

    // First pass: every node is a DAG node + a map entry.
    for n in &run.nodes {
        let id = run.job_id(&n.name);
        dag.ensure_node(id.clone());
        nodes.insert(id, n.clone());
    }

    // Second pass: edges dep → node, validating each dep is declared.
    for n in &run.nodes {
        let to = run.job_id(&n.name);
        for dep in &n.deps {
            if !names.contains(dep.as_str()) {
                return Err(DecomposeError::UnknownDep {
                    node: n.name.clone(),
                    dep: dep.clone(),
                });
            }
            dag.add_edge(run.job_id(dep), to.clone());
        }
    }

    // Reject cycles up front so consumers get a total, schedulable DAG.
    dag.toposort().map_err(|_| DecomposeError::Cycle)?;

    Ok(CanteiroDag { dag, nodes })
}

/// A [`CiNode`] with its action made runnable — the executable half of "CiNode
/// introduced as a shigoto Job" (CANTEIRO §1). `execute` runs the node's action
/// as a subprocess.
///
/// M0 exposes this as an **inherent** `async fn`, not a `shigoto::Job` impl:
/// the doctrine puts in-process wave execution via `shigoto::InProcessScheduler`
/// at **M1** (CANTEIRO §5), and the shigoto-`Job` trait's `async fn` in the
/// published `shigoto-types 0.1.10` needs the desugared-RPITIT impl form to
/// consume cross-crate on rustc 1.9x — a real, isolated M1 wiring detail, not
/// smuggled into M0. So M0 proves "a node's action runs green"; M1 wires it
/// onto the scheduler as a `Job`/`RecordingJob`.
pub struct CiNodeJob {
    node: CiNode,
}

impl CiNodeJob {
    #[must_use]
    pub fn new(node: CiNode) -> Self {
        Self { node }
    }

    #[must_use]
    pub fn node(&self) -> &CiNode {
        &self.node
    }

    /// Run the node's action as a subprocess.
    ///
    /// # Errors
    /// - [`CiNodeError::Spawn`] — the command could not be spawned.
    /// - [`CiNodeError::NonZero`] — the command ran but exited non-zero.
    pub async fn execute(&self) -> Result<(), CiNodeError> {
        let a = &self.node.action;
        let status = std::process::Command::new(&a.command)
            .args(&a.args)
            .status()
            .map_err(|source| CiNodeError::Spawn {
                name: self.node.name.clone(),
                command: a.command.clone(),
                source,
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(CiNodeError::NonZero {
                name: self.node.name.clone(),
                code: status.code().unwrap_or(-1),
            })
        }
    }
}

/// Typed execution error for a CI node.
#[derive(Debug, thiserror::Error)]
pub enum CiNodeError {
    #[error("spawning `{command}` for node `{name}` failed: {source}")]
    Spawn {
        name: String,
        command: String,
        source: std::io::Error,
    },
    #[error("node `{name}` exited non-zero (status {code})")]
    NonZero { name: String, code: i32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(name: &str, cmd: &str) -> ActionRef {
        ActionRef {
            name: name.to_string(),
            command: cmd.to_string(),
            args: vec![],
        }
    }

    /// The M0 canonical shape: a 2-node `build → test` run, both `env=None`.
    fn build_test_run() -> CiRun {
        let build = CiNode::new("build", EnvClass::None, action("build", "true"), vec![]);
        let test = CiNode::new(
            "test",
            EnvClass::None,
            action("test", "true"),
            vec!["build".to_string()],
        );
        CiRun {
            workspace: "pleme-io".into(),
            repo: "example".into(),
            nodes: vec![build, test],
        }
    }

    #[test]
    fn decompose_2node_build_test_orders_build_before_test() {
        let run = build_test_run();
        let cd = decompose(&run).expect("decompose");
        assert_eq!(cd.nodes.len(), 2);

        let order = cd.topo_order().expect("acyclic");
        let build_id = run.job_id("build");
        let test_id = run.job_id("test");
        let bi = order.iter().position(|j| *j == build_id).unwrap();
        let ti = order.iter().position(|j| *j == test_id).unwrap();
        assert!(bi < ti, "build must be scheduled before test");

        assert_eq!(cd.nodes.get(&build_id).unwrap().name, "build");
        assert!(matches!(
            cd.nodes.get(&test_id).unwrap().env_class,
            EnvClass::None
        ));
    }

    #[test]
    fn rejects_duplicate_node() {
        let n1 = CiNode::new("dup", EnvClass::None, action("a", "true"), vec![]);
        let n2 = CiNode::new("dup", EnvClass::None, action("b", "true"), vec![]);
        let run = CiRun {
            workspace: "w".into(),
            repo: "r".into(),
            nodes: vec![n1, n2],
        };
        let Err(e) = decompose(&run) else {
            panic!("expected a decompose error");
        };
        assert_eq!(e, DecomposeError::DuplicateNode("dup".into()));
    }

    #[test]
    fn rejects_unknown_dep() {
        let n = CiNode::new("test", EnvClass::None, action("t", "true"), vec!["nope".into()]);
        let run = CiRun {
            workspace: "w".into(),
            repo: "r".into(),
            nodes: vec![n],
        };
        let Err(e) = decompose(&run) else {
            panic!("expected a decompose error");
        };
        assert_eq!(
            e,
            DecomposeError::UnknownDep {
                node: "test".into(),
                dep: "nope".into()
            }
        );
    }

    #[test]
    fn rejects_cycle() {
        let a = CiNode::new("a", EnvClass::None, action("a", "true"), vec!["b".into()]);
        let b = CiNode::new("b", EnvClass::None, action("b", "true"), vec!["a".into()]);
        let run = CiRun {
            workspace: "w".into(),
            repo: "r".into(),
            nodes: vec![a, b],
        };
        let Err(e) = decompose(&run) else {
            panic!("expected a decompose error");
        };
        assert_eq!(e, DecomposeError::Cycle);
    }

    #[test]
    fn content_addr_is_deterministic_and_injective_in_args() {
        let a1 = content_addr_for(&action("build", "cargo"));
        let a2 = content_addr_for(&action("build", "cargo"));
        assert_eq!(a1, a2, "same action → same address");
        let a3 = content_addr_for(&ActionRef {
            name: "build".into(),
            command: "cargo".into(),
            args: vec!["test".into()],
        });
        assert_ne!(a1, a3, "different args → different address");
    }

    #[tokio::test]
    async fn cinode_job_executes_a_green_command() {
        // Proves a CiNode IS an executable shigoto Job: `true` exits 0.
        let run = build_test_run();
        let node = run.nodes[0].clone();
        let job = CiNodeJob::new(node);
        assert!(job.execute().await.is_ok());
    }

    #[tokio::test]
    async fn cinode_job_surfaces_a_nonzero_exit() {
        let node = CiNode::new("fail", EnvClass::None, action("fail", "false"), vec![]);
        let job = CiNodeJob::new(node);
        assert!(matches!(
            job.execute().await,
            Err(CiNodeError::NonZero { .. })
        ));
    }
}
