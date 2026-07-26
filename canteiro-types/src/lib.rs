//! canteiro's shared CI-run types + DAG algebra (CANTEIRO §7.1-C).
//!
//! Extracted from `sui-supercacheci::canteiro` so a consumer — the caixa
//! `:kind Acao` renderer being the first — gets a genuinely SHARED,
//! compile-time-checked [`CiRun`] without pulling sui-supercacheci's execution
//! tree (tokio / shigoto-scheduler / axum / sea-orm). This crate holds the pure,
//! wire-serializable value types ([`CiNode`]/[`CiRun`]/[`ActionRef`]/
//! [`EnvClass`]/[`ContentAddr`]) and the DAG algebra over them ([`decompose`],
//! [`affected_set`], [`affected_waves`]) — everything that needs only serde +
//! the shipped pure shigoto DAG primitives. The EXECUTION half (`CiNodeJob`,
//! `run_in_process`, `run_distributed`, the emitters) stays in
//! `sui-supercacheci`, which re-exports this crate so `crate::canteiro::CiNode`
//! and every existing consumer keep resolving unchanged.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use shigoto_dag::Dag;
use shigoto_types::{JobId, JobKindId, JobScope, JobSubject};

/// The shigoto job-kind every canteiro CI node carries.
pub const NODE_KIND: &str = "canteiro.node";

/// M0 content address — a deterministic node identity derived from the node's
/// action. At M0 it is the raw keying material (name + command + args,
/// unit-separated); it becomes `gen_pdc::ContentAddr` (BLAKE3 over the node's
/// real build inputs, the dedup key = cache key = incremental boundary) when
/// the L1/L2/L8 legs wire in (CANTEIRO §1, §7 — the M2 leg). Kept a newtype so
/// that promotion is a type change at one site, not a fleet-wide edit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentAddr(pub String);

/// The content-address of a node's realized OUTPUT store-path — the hash the NAR
/// cache keys by, DISTINCT from a node's input [`ContentAddr`]. It is what a
/// cache probe maps `node → hash → StorageBackend::get_narinfo` against, and it
/// is populated by the derivation-realize morphism (CANTEIRO B-Root2's deep
/// gate: turning a node's arbitrary-shell action into a Nix derivation with a
/// content-addressed store-path output).
///
/// Carried as an `Option` on [`CiNode`] with the CONSERVATIVE default `None`
/// (see [`CiNode::output_addr`]): today NO production path sets it, because that
/// realize morphism is unbuilt — so a real `sui_castore`-backed cache probe can
/// never locate a node's output and returns `false` for every node, and NO skip
/// fires (sound by construction). It becomes `Some` only when realize lands — a
/// type change at one site (the realize step), never a fleet-wide edit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutputAddr(pub String);

/// The environment a CI node needs. M0 exercises only [`EnvClass::None`];
/// the other arms are typed so the demand axis exists from M0 but are not wired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRef {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

/// One typed CI node — the SOLE unifying type (CANTEIRO §1). A CI run is a set
/// of these; [`decompose`] turns them into a shigoto DAG.
///
/// `PartialEq`/`Eq` derive because every field here already does (`String`,
/// [`ContentAddr`], [`EnvClass`], [`ActionRef`], `Vec<String>`) — a consumer
/// that embeds a [`CiRun`] in its own `PartialEq`-deriving type (e.g. caixa's
/// `Caixa { ci: Option<CiRun>, .. }`, whose top-level manifest derives
/// `PartialEq` for its round-trip tests) needs the whole value graph to
/// implement it, not just the leaves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiNode {
    /// Unique node name within its [`CiRun`] (the DAG subject).
    pub name: String,
    pub content_addr: ContentAddr,
    pub env_class: EnvClass,
    pub action: ActionRef,
    /// Names of the nodes this one depends on. Each becomes a DAG edge
    /// `dep → this`; every entry must be declared in the same [`CiRun`].
    pub deps: Vec<String>,
    /// The repo-relative path prefixes this node consumes — its build inputs
    /// (e.g. `"sui-supercacheci/src/"`, `"Cargo.toml"`). Used by
    /// [`affected_set`] to decide whether a diff touches this node. The
    /// **CONSERVATIVE default is empty** (`CiNode::new`), and an empty-inputs
    /// node is treated as ALWAYS affected (an unknown-input node is never
    /// silently skipped — CANTEIRO §6). Declare the real prefixes with
    /// [`CiNode::with_inputs`] to opt into diff-scoped skipping.
    pub inputs: Vec<String>,
    /// The content-address of this node's realized OUTPUT store-path — the key
    /// the NAR cache serves it to descendants by, and the soundness gate a
    /// cache probe checks (CANTEIRO B-Root2, [`OutputAddr`]). `None` until the
    /// derivation-realize morphism populates it; a node with no output address
    /// can NEVER be soundly skipped (a cache probe cannot locate what it cannot
    /// name), so `None` is the conservative default. NOTHING in the shipped
    /// pipeline sets this yet — realize is the named remaining deep gate — so in
    /// practice every real node carries `None` and a store-backed probe reports
    /// it uncached, exactly like the M0 unrealized probe.
    pub output_addr: Option<OutputAddr>,
}

impl CiNode {
    /// Construct a node, deriving its M0 content address from the action.
    ///
    /// Inputs default to empty — i.e. the CONSERVATIVE always-affected class
    /// (see [`CiNode::inputs`]); declare real input prefixes with
    /// [`CiNode::with_inputs`] to opt into affected-set skipping. This keeps
    /// the M0/M1 callers (`canteiro-run`, `canteiro-emit-gha`, the existing
    /// tests) compiling unchanged.
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
            inputs: Vec::new(),
            output_addr: None,
        }
    }

    /// Declare the repo-relative path prefixes this node consumes (its build
    /// [`inputs`](CiNode::inputs)), returning `self` for a builder-style chain
    /// over [`CiNode::new`]. Declaring inputs opts the node OUT of the
    /// conservative always-affected default: it is then affected only when a
    /// changed file falls under one of these prefixes (or a DAG ancestor is).
    #[must_use]
    pub fn with_inputs(mut self, inputs: Vec<String>) -> Self {
        self.inputs = inputs;
        self
    }

    /// Declare this node's realized OUTPUT content-address (its store-path hash)
    /// — the key a cache probe serves it by, and the soundness gate a skip
    /// requires (see [`CiNode::output_addr`]/[`OutputAddr`]). This is what the
    /// derivation-realize morphism (CANTEIRO B-Root2) calls once it produces a
    /// store-path output; until that morphism exists it is never called on a
    /// real node, so no node is skippable. Builder over [`CiNode::new`].
    #[must_use]
    pub fn with_output_addr(mut self, addr: OutputAddr) -> Self {
        self.output_addr = Some(addr);
        self
    }
}

/// Deterministic M0 content-address material — injective in the action's
/// fields. No `format!()` (★★ TYPED EMISSION): parts are pushed with an ASCII
/// unit separator so no field value can forge another's boundary.
pub fn content_addr_for(action: &ActionRef) -> ContentAddr {
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
///
/// `PartialEq`/`Eq` derive for the same reason as [`CiNode`] — every field
/// (`String`, `Vec<CiNode>`) already implements it, and a consumer embedding
/// this in its own `PartialEq`-deriving type needs the whole graph to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Whether a diff touches this node's declared inputs — the **directly-affected**
/// predicate (CANTEIRO §6). CONSERVATIVE by construction: a node with NO
/// declared inputs is ALWAYS directly affected, so an unknown-input node is
/// never silently skipped. Otherwise a node is directly affected iff one of its
/// input prefixes is a prefix of some changed file.
fn node_directly_affected(node: &CiNode, changed_files: &[String]) -> bool {
    node.inputs.is_empty()
        || node
            .inputs
            .iter()
            .any(|prefix| changed_files.iter().any(|f| f.starts_with(prefix.as_str())))
}

/// The affected set over an already-decomposed DAG (the shared engine behind
/// [`affected_set`] + [`affected_waves`], so a run is decomposed once per call).
///
/// Reuses the shipped [`Dag`] surface only — [`Dag::toposort`] for a
/// parents-before-children order and [`Dag::predecessors`] for each node's
/// direct upstreams — and forward-propagates affectedness in that order: a node
/// is affected iff it is directly affected OR any direct predecessor is already
/// marked affected. Because topological order decides every predecessor before
/// its children, this single pass yields exactly the directly-affected nodes
/// UNION all their DAG descendants (transitive), with no bespoke graph walk.
fn affected_set_on(
    cd: &CanteiroDag,
    changed_files: &[String],
) -> Result<HashSet<JobId>, DecomposeError> {
    // decompose already proved acyclicity; map defensively rather than assume.
    let order = cd.dag.toposort().map_err(|_| DecomposeError::Cycle)?;
    let mut affected: HashSet<JobId> = HashSet::new();
    for id in &order {
        // Every DAG node has a map entry (decompose builds them in lockstep);
        // skip defensively rather than panic if that invariant ever breaks.
        let Some(node) = cd.nodes.get(id) else {
            continue;
        };
        let directly = node_directly_affected(node, changed_files);
        let via_ancestor = cd
            .dag
            .predecessors(id)
            .iter()
            .any(|p| affected.contains(p));
        if directly || via_ancestor {
            affected.insert(id.clone());
        }
    }
    Ok(affected)
}

/// **The affected-set intelligence** (CANTEIRO §6 M2, "build only what the diff
/// touched"). Decompose `run`, then return the [`JobId`]s that a diff of
/// `changed_files` forces to re-run: the DIRECTLY-affected nodes (those whose
/// declared input prefixes cover a changed file — plus, CONSERVATIVELY, every
/// node with NO declared inputs, which is always affected) UNION all their DAG
/// DESCENDANTS (a rebuilt node forces every downstream consumer to re-run).
///
/// Descendants are computed by reusing the shipped [`Dag`] surface
/// ([`Dag::toposort`] + [`Dag::predecessors`]) via a topological forward-
/// propagation — no re-implemented graph traversal (see [`affected_set_on`]).
///
/// # Errors
/// - [`DecomposeError`] — the run's shape is illegal (cycle / bad dep / dup),
///   surfaced by the initial [`decompose`].
pub fn affected_set(
    run: &CiRun,
    changed_files: &[String],
) -> Result<HashSet<JobId>, DecomposeError> {
    let cd = decompose(run)?;
    affected_set_on(&cd, changed_files)
}

/// The only-affected execution ORDER: decompose `run`, compute its
/// [`affected_set`] for `changed_files`, and feed that subset to the SHIPPED
/// [`Dag::waves`] restriction — the topological waves containing ONLY the
/// affected nodes, in dependency order. Wave partitioning is not re-implemented;
/// this composes the shipped `waves(Some(&affected))` (see CANTEIRO §6 — this is
/// what feeds the multi-worker dispatch so unaffected nodes never schedule).
///
/// # Errors
/// - [`DecomposeError`] — the run's shape is illegal (cycle / bad dep / dup).
pub fn affected_waves(
    run: &CiRun,
    changed_files: &[String],
) -> Result<Vec<Vec<JobId>>, DecomposeError> {
    let cd = decompose(run)?;
    let affected = affected_set_on(&cd, changed_files)?;
    cd.dag
        .waves(Some(&affected))
        .map_err(|_| DecomposeError::Cycle)
}
