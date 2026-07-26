//! canteiro — a CI run decomposed into ONE typed content-addressed DAG, then
//! **executed in-process, wave by wave** (M0 + M1).
//!
//! `theory/CANTEIRO.md` §5. **M0** lands the `CiNode` type (the sole unifying
//! type, §1), the [`decompose`] morphism (`CiRun → (shigoto Dag over JobId  +
//! JobId→CiNode map)`, the "dandori" decompose), and `CiNode` as a runnable job
//! ([`CiNodeJob`]). **M1** (this increment) wires that job onto the shipped
//! shigoto runtime: [`CiNodeJob`] is a real [`RecordingJob`] whose derived
//! [`JobId`] equals [`CiRun::job_id`], and [`run_in_process`] registers every
//! node into an [`InProcessScheduler`] and ticks the decomposed DAG to terminal
//! in dependency order — the "shigoto executes the CiNode DAG in-process" proof.
//! It is a lib-level driver; the in-pod `canteiro run` binary is the one
//! remaining wiring step (the runner packaging, not new logic).
//!
//! Deliberately NOT here, each named as its own later increment (tier-honest
//! per CANTEIRO §7, never rounded up):
//! - the in-pod `canteiro run` **binary** + its ARC-runner packaging — the
//!   entrypoint logic ships as [`run_in_process`]; a `[[bin]]` (or a
//!   `sui`-subcommand) over it, and a real live ARC run, are the operator-gated
//!   next step. This module proves the execution in-process, off any cluster.
//! - a **fail-fast gate** — the shipped runtime's `AllUpstreamsTerminal` gate
//!   *releases* a deadlettered node's descendants (SHIGOTO §VII.4), so a failed
//!   `build` does not skip a dependent `test`. Making a failed ancestor SKIP
//!   its descendants is a registered `Gate`, a named follow-up, not M1.
//! - the caixa `:kind Acao` authoring vocabulary (CANTEIRO §4).
//! - `gen_pdc::ContentAddr` as the real dedup/cache identity — **M2** (§7);
//!   [`ContentAddr`] here is an M0 placeholder over the node's action.
//! - `EnvClass` beyond `None`: `LocalStack` needs cofre cross-org secrets into
//!   the camelot-ci runners (§5); `WarmPoolClaim` is the viveiro pool
//!   (**DESIGN**, §7). Both are typed so the axis exists from M0; neither is wired.

use std::sync::Arc;

use async_trait::async_trait;
use shigoto_scheduler::{InProcessScheduler, Scheduler};
use shigoto_types::{
    ErasedJob, JobPhase, JobScope, JobSubject, OutputSink, RecordingJob, SkipReason,
};

// The pure CI-run types + DAG algebra live in the lean `canteiro-types` crate
// (CANTEIRO §7.1-C), re-exported here so `crate::canteiro::CiNode` etc. and every
// existing consumer (canteiro_dist / canteiro_skip / canteiro_gha / canteiro_spec,
// the bins, and the tests below) resolve unchanged. This module keeps the
// EXECUTION half below — `CiNodeJob` + `run_in_process` — which needs
// shigoto-scheduler + tokio and so stays out of the lean shared crate.
pub use canteiro_types::{
    affected_set, affected_waves, content_addr_for, decompose, ActionRef, CanteiroDag, CiNode,
    CiRun, ContentAddr, DecomposeError, EnvClass, NODE_KIND,
};

/// A [`CiNode`] made runnable as a real shigoto [`RecordingJob`] — the
/// executable half of "CiNode introduced as a shigoto Job" (CANTEIRO §1).
///
/// It carries the run's `workspace` + `repo` alongside the node because those
/// live on the [`CiRun`], not on a bare [`CiNode`]; carrying them is what lets
/// the derived [`JobId`] (blanket `RecordingJob → Job`: `{scope, kind, subject}`)
/// equal [`CiRun::job_id`] exactly, so a job registered into the scheduler keys
/// against the matching node in the decomposed [`Dag`]. The `#[async_trait]`
/// attribute matches how `shigoto-types 0.1.10` declares the trait (the desugar
/// adds the `'_`/`Send` bounds a bare `async fn` would mismatch — E0195).
///
/// [`execute`](CiNodeJob::execute) is the inherent subprocess runner (kept for
/// direct use + the per-node unit tests); [`RecordingJob::execute_body`]
/// delegates to it so the scheduler drives exactly the same action.
pub struct CiNodeJob {
    workspace: String,
    repo: String,
    node: CiNode,
}

impl CiNodeJob {
    /// Build a runnable job for a node within a run identified by
    /// `workspace` + `repo` (so the derived id matches [`CiRun::job_id`]).
    #[must_use]
    pub fn new(workspace: impl Into<String>, repo: impl Into<String>, node: CiNode) -> Self {
        Self {
            workspace: workspace.into(),
            repo: repo.into(),
            node,
        }
    }

    /// Build a runnable job for a node in the context of its [`CiRun`].
    #[must_use]
    pub fn for_run(run: &CiRun, node: CiNode) -> Self {
        Self::new(run.workspace.clone(), run.repo.clone(), node)
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

#[async_trait]
impl RecordingJob for CiNodeJob {
    type Output = ();
    type Error = CiNodeError;
    /// Must equal [`CiRun::job_id`]'s kind so the derived id matches the DAG node.
    const KIND: &'static str = NODE_KIND;

    fn scope(&self) -> JobScope {
        JobScope::Repo {
            workspace: self.workspace.clone(),
            repo: self.repo.clone(),
        }
    }

    fn subject(&self) -> JobSubject {
        JobSubject::Pinned(self.node.name.clone())
    }

    fn output_sink(&self) -> Option<&Arc<dyn OutputSink<Self::Output>>> {
        None
    }

    async fn execute_body(&self) -> Result<Self::Output, Self::Error> {
        self.execute().await
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

/// The terminal verdict a node reaches after a [`run_in_process`] drive — the
/// typed projection of the scheduler's terminal [`JobPhase`] set onto exactly
/// the three outcomes a CI node can end in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeVerdict {
    /// The node's action ran and exited zero (`JobPhase::Succeeded`).
    Succeeded,
    /// The node's action failed / exhausted retries (`JobPhase::Deadlettered`).
    Failed,
    /// The node never ran because an ancestor failed / was gated out
    /// (`JobPhase::Skipped`), carrying the shigoto [`SkipReason`].
    Skipped(SkipReason),
}

impl NodeVerdict {
    /// Whether this verdict is the green terminal.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, NodeVerdict::Succeeded)
    }
}

/// The typed report of an in-process run: each node's name paired with the
/// terminal [`NodeVerdict`] it reached, plus the number of scheduler ticks the
/// drive took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// One entry per declared node, in the run's declaration order.
    pub nodes: Vec<(String, NodeVerdict)>,
    /// How many `tick`s the drive needed to reach an all-terminal DAG.
    pub ticks: u32,
}

impl RunReport {
    /// The whole run is green iff every node succeeded.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.nodes.iter().all(|(_, v)| v.is_success())
    }

    /// The verdict for a named node, if it was part of the run.
    #[must_use]
    pub fn verdict(&self, name: &str) -> Option<&NodeVerdict> {
        self.nodes
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }
}

/// Every way an in-process run can fail to produce a report.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The run's shape was rejected before scheduling (cycle / bad dep / dup).
    #[error("decompose failed: {0}")]
    Decompose(#[from] DecomposeError),
    /// The scheduler surfaced an internal FSM/topology error during a tick.
    #[error("scheduler tick failed: {0}")]
    Schedule(#[from] shigoto_scheduler::SchedulerError),
    /// The DAG did not reach an all-terminal state within the tick bound — a
    /// safety valve against an unexpected non-converging graph (never silently
    /// loops forever).
    #[error("run did not converge within {ticks} ticks ({pending} node(s) non-terminal)")]
    DidNotConverge { ticks: u32, pending: usize },
    /// A node was in a non-terminal phase when the report was built — a broken
    /// scheduler invariant surfaced typed, never silently mislabeled green.
    #[error("node `{name}` ended in a non-terminal phase after convergence")]
    NonTerminal { name: String },
}

/// The upper bound on scheduler ticks a run may take before [`run_in_process`]
/// gives up: a chain of `n` nodes advances at most one wave per tick, so
/// `n + 1` ticks is a generous ceiling (empirically the whole ready-cascade
/// clears in one tick). Any run exceeding it is a bug, surfaced as
/// [`RunError::DidNotConverge`] rather than an infinite loop.
fn tick_bound(node_count: usize) -> u32 {
    u32::try_from(node_count).unwrap_or(u32::MAX).saturating_add(2)
}

/// Map a terminal [`JobPhase`] to the typed [`NodeVerdict`]; a non-terminal
/// phase is the error case (a broken invariant, not a silent green).
fn verdict_for(name: &str, phase: Option<&JobPhase>) -> Result<NodeVerdict, RunError> {
    match phase {
        Some(JobPhase::Succeeded) => Ok(NodeVerdict::Succeeded),
        Some(JobPhase::Deadlettered) => Ok(NodeVerdict::Failed),
        Some(JobPhase::Skipped(reason)) => Ok(NodeVerdict::Skipped(reason.clone())),
        _ => Err(RunError::NonTerminal {
            name: name.to_string(),
        }),
    }
}

/// **The M1 wave-execution driver** (CANTEIRO §5) — decompose a [`CiRun`] into
/// its typed DAG, register every node as a shigoto [`RecordingJob`] in an
/// [`InProcessScheduler`], and tick the DAG to an all-terminal state in
/// dependency order, returning a typed [`RunReport`]. This IS the "shigoto
/// executes the CiNode DAG in-process" proof and the in-pod runner entrypoint
/// logic; a `[[bin]]`/subcommand over it is the remaining packaging step.
///
/// The drive loop is exactly the scheduler's own contract: `snapshot` the DAG,
/// break when every node is terminal (`Succeeded | Deadlettered | Skipped`),
/// else `tick` once and repeat — bounded by [`tick_bound`] so a pathological
/// graph errors rather than spins.
///
/// # Errors
/// - [`RunError::Decompose`] — the run's shape is illegal (cycle / bad dep / dup).
/// - [`RunError::Schedule`] — the scheduler raised an FSM/topology error.
/// - [`RunError::DidNotConverge`] — the DAG never went all-terminal in bound.
/// - [`RunError::NonTerminal`] — a node was non-terminal at report time.
pub async fn run_in_process(run: &CiRun) -> Result<RunReport, RunError> {
    let CanteiroDag { mut dag, nodes } = decompose(run)?;

    let scheduler = InProcessScheduler::new("canteiro");
    for node in nodes.values() {
        let job = CiNodeJob::for_run(run, node.clone());
        let erased: Arc<dyn ErasedJob> = Arc::new(job);
        scheduler.register_job(erased).await;
    }

    let bound = tick_bound(nodes.len());
    let mut ticks = 0u32;
    let final_phases = loop {
        let snap = scheduler.snapshot(&dag).await;
        let non_terminal = nodes
            .keys()
            .filter(|id| {
                !matches!(
                    snap.phases.get(id),
                    Some(JobPhase::Succeeded | JobPhase::Deadlettered | JobPhase::Skipped(_))
                )
            })
            .count();
        if non_terminal == 0 {
            break snap.phases;
        }
        if ticks >= bound {
            return Err(RunError::DidNotConverge {
                ticks,
                pending: non_terminal,
            });
        }
        scheduler.tick(&mut dag).await?;
        ticks += 1;
    };

    // Report in the run's declaration order (stable + operator-legible), not
    // the HashMap's arbitrary order.
    let mut report_nodes = Vec::with_capacity(run.nodes.len());
    for n in &run.nodes {
        let id = run.job_id(&n.name);
        let verdict = verdict_for(&n.name, final_phases.get(&id))?;
        report_nodes.push((n.name.clone(), verdict));
    }
    Ok(RunReport {
        nodes: report_nodes,
        ticks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // JobId is used by the decompose/affected tests below; it is no longer a
    // module-level import (the types + algebra moved to canteiro-types), so the
    // test module names it directly.
    use shigoto_types::JobId;

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
        let job = CiNodeJob::for_run(&run, node);
        assert!(job.execute().await.is_ok());
    }

    #[tokio::test]
    async fn cinode_job_surfaces_a_nonzero_exit() {
        let node = CiNode::new("fail", EnvClass::None, action("fail", "false"), vec![]);
        let job = CiNodeJob::new("w", "r", node);
        assert!(matches!(
            job.execute().await,
            Err(CiNodeError::NonZero { .. })
        ));
    }

    /// The load-bearing M1 invariant: the id shigoto derives for a
    /// [`CiNodeJob`] (via the `RecordingJob → Job` blanket) is byte-identical
    /// to [`CiRun::job_id`] for the same node, so a registered job keys against
    /// the matching DAG node. If this drifts, `run_in_process` would register
    /// jobs the scheduler can never find (they would deadletter as unregistered).
    #[test]
    fn cinode_job_derived_id_matches_cirun_job_id() {
        use shigoto_types::Job;
        let run = build_test_run();
        for n in &run.nodes {
            let job = CiNodeJob::for_run(&run, n.clone());
            assert_eq!(
                Job::id(&job),
                run.job_id(&n.name),
                "derived shigoto id must equal CiRun::job_id for `{}`",
                n.name
            );
        }
    }

    /// END-TO-END M1: decompose → register → in-process wave execution. A
    /// 2-node `build → test` run (both `env=None`, both `true`) drives through
    /// the [`InProcessScheduler`] and BOTH reach `Succeeded`, in dependency
    /// order (the DAG guarantees build before test).
    #[tokio::test]
    async fn run_in_process_two_node_build_test_both_succeed() {
        let run = build_test_run();
        let report = run_in_process(&run).await.expect("run");
        assert!(report.succeeded(), "both nodes must be green: {report:?}");
        assert_eq!(report.verdict("build"), Some(&NodeVerdict::Succeeded));
        assert_eq!(report.verdict("test"), Some(&NodeVerdict::Succeeded));
        // Report preserves declaration order (build then test).
        assert_eq!(
            report.nodes.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["build", "test"]
        );
    }

    /// END-TO-END M1: a failing node surfaces as a non-success terminal, and
    /// the whole run is not green.
    ///
    /// Tier-honest note (never rounded up): the shipped default runtime uses
    /// shigoto's `AllUpstreamsTerminal` gate, which by design *releases* a
    /// deadlettered node's descendants (SHIGOTO §VII.4 — "we never Skip from
    /// this gate: a Deadlettered ancestor is terminal, so it unblocks"). So
    /// `test` still RUNS after `build` deadletters — dependency *ordering* is
    /// honored (build reaches a terminal phase before test is released), but
    /// there is no fail-fast *skip*. Making a failed ancestor SKIP its
    /// descendants (`NodeVerdict::Skipped(BlockedByDeadletteredAncestor)`) is a
    /// registered fail-fast `Gate` — a named CANTEIRO follow-up, not M1; this
    /// test asserts exactly the behavior that ships, not the one we'd wish for.
    #[tokio::test]
    async fn run_in_process_failing_build_surfaces_as_failed_terminal() {
        let build = CiNode::new("build", EnvClass::None, action("build", "false"), vec![]);
        let test = CiNode::new(
            "test",
            EnvClass::None,
            action("test", "true"),
            vec!["build".to_string()],
        );
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "example".into(),
            nodes: vec![build, test],
        };
        let report = run_in_process(&run).await.expect("run");
        assert!(!report.succeeded(), "a failed build must not be green");
        assert_eq!(
            report.verdict("build"),
            Some(&NodeVerdict::Failed),
            "the failing node surfaces as a non-success (Failed) terminal"
        );
        // Default-runtime behavior: the deadlettered ancestor releases `test`,
        // which then runs green. Fail-fast skip is the follow-up gate.
        assert_eq!(report.verdict("test"), Some(&NodeVerdict::Succeeded));
    }

    /// A 2-node `build → test` run where BOTH nodes declare real input prefixes
    /// (so neither falls into the conservative always-affected class) — the
    /// fixture for the diff-scoped affected-set tests.
    fn scoped_build_test_run() -> CiRun {
        let build = CiNode::new("build", EnvClass::None, action("build", "true"), vec![])
            .with_inputs(vec!["src/build/".to_string()]);
        let test = CiNode::new(
            "test",
            EnvClass::None,
            action("test", "true"),
            vec!["build".to_string()],
        )
        .with_inputs(vec!["tests/".to_string()]);
        CiRun {
            workspace: "pleme-io".into(),
            repo: "example".into(),
            nodes: vec![build, test],
        }
    }

    /// (a) A change touching ONLY `test`'s inputs affects `{test}` — `build` is
    /// `test`'s ANCESTOR (edge `build → test`), not its descendant, so it must
    /// NOT be re-run.
    #[test]
    fn change_to_test_inputs_affects_only_test_not_its_ancestor_build() {
        let run = scoped_build_test_run();
        let affected = affected_set(&run, &["tests/foo.rs".to_string()]).expect("affected");
        assert!(affected.contains(&run.job_id("test")));
        assert!(
            !affected.contains(&run.job_id("build")),
            "build is test's ANCESTOR, not descendant — a test-input change must not re-run build"
        );
        assert_eq!(affected.len(), 1);
    }

    /// (b) A change touching `build`'s inputs affects `{build, test}` — `test`
    /// is `build`'s DESCENDANT (edge `build → test`), so a rebuilt `build`
    /// forces `test` to re-run.
    #[test]
    fn change_to_build_inputs_affects_build_and_its_descendant_test() {
        let run = scoped_build_test_run();
        let affected = affected_set(&run, &["src/build/main.rs".to_string()]).expect("affected");
        assert!(affected.contains(&run.job_id("build")));
        assert!(
            affected.contains(&run.job_id("test")),
            "test is build's DESCENDANT — a build change must re-run test"
        );
        assert_eq!(affected.len(), 2);
    }

    /// (c) A change touching NOTHING declared affects only the conservative
    /// always-affected (empty-inputs) nodes: here `lint` (no inputs) is
    /// affected, while the input-declaring `build`/`test` are not.
    #[test]
    fn change_touching_nothing_affects_only_conservative_always_affected_nodes() {
        let build = CiNode::new("build", EnvClass::None, action("build", "true"), vec![])
            .with_inputs(vec!["src/build/".to_string()]);
        let test = CiNode::new(
            "test",
            EnvClass::None,
            action("test", "true"),
            vec!["build".to_string()],
        )
        .with_inputs(vec!["tests/".to_string()]);
        // NO declared inputs → conservative always-affected class.
        let lint = CiNode::new("lint", EnvClass::None, action("lint", "true"), vec![]);
        let run = CiRun {
            workspace: "w".into(),
            repo: "r".into(),
            nodes: vec![build, test, lint],
        };
        let affected = affected_set(&run, &["docs/README.md".to_string()]).expect("affected");
        assert!(
            affected.contains(&run.job_id("lint")),
            "an empty-inputs node is ALWAYS affected"
        );
        assert!(!affected.contains(&run.job_id("build")));
        assert!(!affected.contains(&run.job_id("test")));
        assert_eq!(affected.len(), 1);
    }

    /// (d) A node with empty inputs is ALWAYS in the affected set, regardless of
    /// `changed_files` — including an empty diff and a wholly-unrelated one.
    #[test]
    fn empty_inputs_node_is_always_affected_regardless_of_changed_files() {
        // `build_test_run` nodes carry the default empty inputs.
        let run = build_test_run();
        for changed in [Vec::<String>::new(), vec!["totally/unrelated.txt".to_string()]] {
            let affected = affected_set(&run, &changed).expect("affected");
            assert!(
                affected.contains(&run.job_id("build")),
                "empty-inputs `build` always affected for changed={changed:?}"
            );
            assert!(
                affected.contains(&run.job_id("test")),
                "empty-inputs `test` always affected for changed={changed:?}"
            );
            assert_eq!(affected.len(), 2);
        }
    }

    /// (e) `affected_waves` yields waves containing ONLY the affected nodes, in
    /// dependency order — reusing the shipped `Dag::waves(Some(&affected))`.
    #[test]
    fn affected_waves_contains_only_affected_nodes_in_dependency_order() {
        let run = scoped_build_test_run();

        // A build-input change affects both; the waves order build before test.
        let waves = affected_waves(&run, &["src/build/main.rs".to_string()]).expect("waves");
        let flat: Vec<JobId> = waves.iter().flatten().cloned().collect();
        assert!(flat.contains(&run.job_id("build")));
        assert!(flat.contains(&run.job_id("test")));
        let wave_index = |name: &str| {
            waves
                .iter()
                .position(|w| w.contains(&run.job_id(name)))
                .expect("node present in some wave")
        };
        assert!(
            wave_index("build") < wave_index("test"),
            "build's wave must precede test's wave"
        );

        // A test-only change prunes `build` entirely: only `{test}` schedules.
        let waves2 = affected_waves(&run, &["tests/foo.rs".to_string()]).expect("waves");
        let flat2: Vec<JobId> = waves2.iter().flatten().cloned().collect();
        assert_eq!(
            flat2,
            vec![run.job_id("test")],
            "only the affected node is scheduled; the unaffected ancestor is pruned"
        );
        assert!(!flat2.contains(&run.job_id("build")));
    }
}
