//! canteiro plane (b) — CANTEIRO itself as the cross-worker RUNTIME scheduler.
//!
//! `theory/CANTEIRO.md` §7.1-A / §8-risk-1 (the named crux). Where
//! [`run_in_process`](crate::canteiro::run_in_process) drives the shipped
//! `InProcessScheduler` on ONE worker, and
//! [`emit_gha`](crate::canteiro_gha::emit_gha) projects the DAG onto a GitHub
//! Actions job graph (plane a — **GitHub** schedules the waves onto separate
//! runners), this module makes **canteiro** own the cross-worker orchestration:
//! it holds the DAG, publishes a node as a [`WorkItem`] only once its deps are
//! terminal (the ordering is canteiro's, not GitHub's), and N independent
//! [`Worker`]s claim, run, and report each item back.
//!
//! ## Tier-honest scope (never round up)
//!
//! **M0 (this increment): the dispatch / claim / report PROTOCOL, proven
//! in-process** with an [`InMemoryQueue`] + N worker tasks (each stamps its own
//! id, standing in for an ARC pod). The transport sits behind the [`WorkQueue`]
//! trait, so the **destination** — a Postgres `SELECT … FOR UPDATE SKIP LOCKED`
//! queue over super-cache-ci's `PgStore`, one worker per ARC pod (Agent A's
//! Root D) — is a trait-impl swap, **not** a rewrite. The report side is a typed
//! [`NodeResult`]; the shipped shigoto `Signal::ExecutionSucceeded/Failed`
//! mapping is the destination's report shape (reused, not reinvented).
//!
//! **NOT done, named as the live gate:** a real run on ≥2 separate camelot-eks
//! ARC pods (a `canteiro-worker` `[[bin]]` claiming from a live PG queue). This
//! module proves the protocol off any cluster. A **fail-fast gate** is also a
//! follow-up: a `Failed` node is treated as terminal here (its descendants are
//! still published — matching the shipped `AllUpstreamsTerminal` default the
//! canteiro module documents), never skipped.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shigoto_types::JobId;
use tokio::sync::{mpsc, Mutex, Notify};

use crate::canteiro::{CiNode, CiRun, DecomposeError};
use crate::canteiro_skip::{partition, CacheProbe};

/// A unit of work dispatched to a worker: one CI node keyed by its [`JobId`].
/// Serializable — the destination transport ships this across the process
/// boundary to an ARC-runner pod (`CiNode` + `JobId` are both serde).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub job_id: JobId,
    pub node: CiNode,
}

/// The outcome a worker reports for a claimed [`WorkItem`]. Maps 1:1 onto the
/// shipped shigoto `Signal::ExecutionSucceeded` / `ExecutionFailed` at the
/// destination transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeOutcome {
    Succeeded,
    Failed { message: String },
}

/// A worker's report back to the scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResult {
    pub job_id: JobId,
    /// Which worker ran it (the M0 stamps a worker index; the destination
    /// stamps the ARC pod name).
    pub worker_id: String,
    pub outcome: NodeOutcome,
}

/// The cross-worker transport seam. `publish`/`claim`/`report` are the whole
/// protocol; a claim must be **atomic** (each item to exactly one worker). M0 =
/// [`InMemoryQueue`]; the destination is a Postgres queue (`FOR UPDATE SKIP
/// LOCKED`) — same three methods, a different impl.
#[async_trait]
pub trait WorkQueue: Send + Sync {
    /// Publish a ready item for any worker to claim.
    async fn publish(&self, item: WorkItem);
    /// Atomically claim one pending item; `None` once the queue is drained AND
    /// shut down (the signal for a worker loop to exit).
    async fn claim(&self) -> Option<WorkItem>;
    /// Signal no more items will be published — pending drains, then `claim`
    /// returns `None` and workers exit.
    fn close(&self);

    /// Durable async close for a cross-process transport whose "closed" sentinel
    /// is a *remote* write (the PG `canteiro_queue_control` row). The default
    /// delegates to the synchronous [`close`](WorkQueue::close) — correct for an
    /// in-process transport like [`InMemoryQueue`] whose sentinel is a local
    /// [`AtomicBool`]. The PG transport OVERRIDES this to perform the actual DB
    /// write (its sync `close` cannot, being non-async), and the cross-process
    /// scheduler [`run_distributed_pg`] calls THIS, not the sync form.
    async fn close_durable(&self) {
        self.close();
    }
}

/// The cross-process RESULT transport — the companion to [`WorkQueue`] that
/// carries a worker's [`NodeResult`] back to the scheduler.
///
/// In the in-memory M0 the result path is an in-process `mpsc` living inside
/// [`run_dispatch`] — that channel does NOT cross a process boundary, so it
/// cannot carry a result from a `canteiro-worker` pod back to the scheduler.
/// This trait is the honest cross-process analog: `report` is the worker side
/// (push a result), `next_result` is the scheduler side (poll the *next*
/// unconsumed result, or `None` if none is ready yet). The destination impl is
/// the Postgres `canteiro_results` table (`crate::canteiro_pg::PgWorkQueue`,
/// `#[cfg(feature = "postgres")]`); the mock is [`InMemoryQueue`] here.
///
/// The shape deliberately MIRRORS [`WorkQueue`]: methods return no error (a
/// transport error is `tracing`-logged and surfaced as "nothing available yet",
/// which the scheduler [`run_distributed_pg`] already degrades on via a bounded
/// idle-poll cap). The concrete `PgWorkQueue` additionally exposes fallible
/// `try_*` inherent methods for the error-aware per-pod path.
#[async_trait]
pub trait ResultTransport: Send + Sync {
    /// Worker side: report the outcome of a claimed item back to the scheduler.
    async fn report(&self, result: NodeResult);
    /// Scheduler side: consume the next unreported-to-the-scheduler result, or
    /// `None` if none is available yet (the scheduler polls + sleeps). Each
    /// result is returned to exactly one caller (atomic consume).
    async fn next_result(&self) -> Option<NodeResult>;
}

/// In-memory [`WorkQueue`] + [`ResultTransport`] for the M0 protocol proof — a
/// shared FIFO whose `claim` is a lock-guarded `pop_front` (the faithful analog
/// of `FOR UPDATE SKIP LOCKED`: each item goes to exactly one claimer), plus a
/// second FIFO for the result path so the cross-process split (scheduler polls
/// results; workers report separately, NO in-process `mpsc`) is provable in a
/// single process.
pub struct InMemoryQueue {
    pending: Mutex<VecDeque<WorkItem>>,
    results: Mutex<VecDeque<NodeResult>>,
    notify: Notify,
    closed: AtomicBool,
}

impl Default for InMemoryQueue {
    fn default() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            results: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl ResultTransport for InMemoryQueue {
    async fn report(&self, result: NodeResult) {
        self.results.lock().await.push_back(result);
    }

    async fn next_result(&self) -> Option<NodeResult> {
        // Non-blocking pop: the scheduler ([`run_distributed_pg`]) owns the
        // sleep-between-polls, mirroring the real PG poll loop.
        self.results.lock().await.pop_front()
    }
}

#[async_trait]
impl WorkQueue for InMemoryQueue {
    async fn publish(&self, item: WorkItem) {
        self.pending.lock().await.push_back(item);
        self.notify.notify_one();
    }

    async fn claim(&self) -> Option<WorkItem> {
        loop {
            if let Some(item) = self.pending.lock().await.pop_front() {
                return Some(item);
            }
            if self.closed.load(Ordering::Acquire) {
                // Drained + closed → tell this worker to exit. Wake any siblings
                // still parked so they observe the same closed+empty state.
                self.notify.notify_waiters();
                return None;
            }
            self.notify.notified().await;
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

/// The side-effecting seam a worker runs a node through — the TYPED-SPEC-triplet
/// `Environment`: real impls spawn the node's action as a subprocess; tests
/// inject a recording mock, so the dispatch protocol is proven with zero real
/// process/cluster linkage.
#[async_trait]
pub trait NodeRunner: Send + Sync {
    async fn run(&self, worker_id: &str, node: &CiNode) -> NodeOutcome;
}

/// The real [`NodeRunner`]: runs a node's action as a subprocess — what a live
/// canteiro-worker pod runs. Mirrors
/// [`CiNodeJob::execute`](crate::canteiro::CiNodeJob::execute)'s spawn+status
/// logic, adapted to the [`NodeOutcome`] contract (no `format!` — messages are
/// built with `push_str`, ★★ TYPED EMISSION).
pub struct SubprocessRunner;

#[async_trait]
impl NodeRunner for SubprocessRunner {
    async fn run(&self, _worker_id: &str, node: &CiNode) -> NodeOutcome {
        let a = &node.action;
        match std::process::Command::new(&a.command).args(&a.args).status() {
            Ok(s) if s.success() => NodeOutcome::Succeeded,
            Ok(s) => {
                let mut message = String::from("node exited non-zero (status ");
                message.push_str(&s.code().unwrap_or(-1).to_string());
                message.push(')');
                NodeOutcome::Failed { message }
            }
            Err(e) => {
                let mut message = String::from("spawn failed: ");
                message.push_str(&e.to_string());
                NodeOutcome::Failed { message }
            }
        }
    }
}

/// The result of an affected-aware distributed run: which nodes actually ran
/// (with their [`NodeResult`]s), which were soundly skipped (served from cache).
#[derive(Debug, Clone)]
pub struct AffectedRun {
    pub results: Vec<NodeResult>,
    pub ran: Vec<String>,
    pub skipped: Vec<String>,
}

/// Run `run`'s DAG across `num_workers` workers, canteiro owning the ordering.
///
/// The scheduler publishes a node only once every dep is terminal, so the DAG
/// edges — not the workers, not GitHub — decide order. Returns every
/// [`NodeResult`] (one per node). A `Failed` node counts as terminal (its
/// descendants are still published — the shipped `AllUpstreamsTerminal`
/// behavior; fail-fast is a named follow-up).
pub async fn run_distributed<Q, R>(
    run: &CiRun,
    num_workers: usize,
    queue: Arc<Q>,
    runner: Arc<R>,
) -> Vec<NodeResult>
where
    Q: WorkQueue + 'static,
    R: NodeRunner + 'static,
{
    run_dispatch(run, num_workers, queue, runner, HashSet::new()).await
}

/// Sound affected-aware distributed run (CANTEIRO ROOT-4): [`partition`] the run
/// by the diff + a [`CacheProbe`], then dispatch ONLY the run-set — skipped
/// nodes are pre-marked terminal so their descendants still proceed (their
/// output is served from cache). SOUND BY CONSTRUCTION: with the shipped
/// `UnrealizedProbe` the skip-set is empty, so this is identical to
/// [`run_distributed`] (nothing skips until B-Root2's realize + a real probe).
///
/// # Errors
/// [`DecomposeError`] if the run is not a valid DAG (propagated from
/// [`partition`]/`affected_set`, never swallowed).
pub async fn run_distributed_affected<Q, R, P>(
    run: &CiRun,
    changed_files: &[String],
    num_workers: usize,
    queue: Arc<Q>,
    runner: Arc<R>,
    probe: &P,
) -> Result<AffectedRun, DecomposeError>
where
    Q: WorkQueue + 'static,
    R: NodeRunner + 'static,
    P: CacheProbe,
{
    let part = partition(run, changed_files, probe).await?;
    let skip: HashSet<String> = part.skip.iter().cloned().collect();
    let results = run_dispatch(run, num_workers, queue, runner, skip).await;
    Ok(AffectedRun {
        results,
        ran: part.run,
        skipped: part.skip,
    })
}

/// The shared dispatch loop. `pre_done` nodes are treated as already-terminal
/// (never published, their descendants proceed) — empty for a full run, the
/// skip-set for an affected run.
async fn run_dispatch<Q, R>(
    run: &CiRun,
    num_workers: usize,
    queue: Arc<Q>,
    runner: Arc<R>,
    pre_done: HashSet<String>,
) -> Vec<NodeResult>
where
    Q: WorkQueue + 'static,
    R: NodeRunner + 'static,
{
    // Static DAG facts, by node name.
    let deps: HashMap<String, Vec<String>> = run
        .nodes
        .iter()
        .map(|n| (n.name.clone(), n.deps.clone()))
        .collect();
    let by_name: HashMap<String, CiNode> =
        run.nodes.iter().map(|n| (n.name.clone(), n.clone())).collect();
    let total = run.nodes.len();

    // Spawn workers: each loops claim → run → report until the queue closes.
    let (res_tx, mut res_rx) = mpsc::unbounded_channel::<NodeResult>();
    let mut handles = Vec::with_capacity(num_workers);
    for w in 0..num_workers {
        let mut worker_id = String::from("worker-");
        worker_id.push_str(&w.to_string());
        let q = queue.clone();
        let r = runner.clone();
        let tx = res_tx.clone();
        handles.push(tokio::spawn(async move {
            while let Some(item) = q.claim().await {
                let outcome = r.run(&worker_id, &item.node).await;
                // Send can only fail if the scheduler dropped the receiver,
                // which it does only after every node is terminal — so a lost
                // send here carries no un-recorded work.
                let _ = tx.send(NodeResult {
                    job_id: item.job_id,
                    worker_id: worker_id.clone(),
                    outcome,
                });
            }
        }));
    }
    drop(res_tx); // only the workers hold senders now → res_rx closes when they exit

    // Skipped nodes (pre_done) start terminal + already-"published": never
    // dispatched, but their descendants see them satisfied and proceed.
    let mut done: HashSet<String> = pre_done.clone();
    let mut published: HashSet<String> = pre_done;
    let mut results: Vec<NodeResult> = Vec::with_capacity(total);

    // Publish every node whose deps are all terminal and which isn't yet out.
    let publish_ready = |published: &mut HashSet<String>, done: &HashSet<String>| {
        let mut ready = Vec::new();
        for node in &run.nodes {
            if published.contains(&node.name) {
                continue;
            }
            if deps[&node.name].iter().all(|d| done.contains(d)) {
                published.insert(node.name.clone());
                ready.push(node.name.clone());
            }
        }
        ready
    };

    for name in publish_ready(&mut published, &done) {
        let node = by_name[&name].clone();
        queue
            .publish(WorkItem {
                job_id: run.job_id(&name),
                node,
            })
            .await;
    }

    while done.len() < total {
        let Some(result) = res_rx.recv().await else {
            break; // all workers exited before every node completed — degrade, don't hang
        };
        let name = match &result.job_id.subject {
            shigoto_types::JobSubject::Pinned(n) => n.clone(),
            _ => continue,
        };
        done.insert(name);
        results.push(result);
        for ready in publish_ready(&mut published, &done) {
            let node = by_name[&ready].clone();
            queue
                .publish(WorkItem {
                    job_id: run.job_id(&ready),
                    node,
                })
                .await;
        }
    }

    queue.close();
    for h in handles {
        let _ = h.await;
    }
    results
}

/// The per-pod worker loop — exactly what one `canteiro-worker` `[[bin]]` (one
/// ARC pod) runs: claim a [`WorkItem`], run it through the [`NodeRunner`],
/// report the [`NodeResult`] back through the [`ResultTransport`], repeat until
/// the queue is drained AND closed (`claim` returns `None`).
///
/// This is the cross-process factoring of [`run_dispatch`]'s inner worker task:
/// there the reported result went out an in-process `mpsc` (`tx.send`); here it
/// goes through [`ResultTransport::report`], which crosses the process boundary
/// (the PG `canteiro_results` table). The scheduler runs [`run_distributed_pg`]
/// in a *separate* process and never spawns these workers.
pub async fn worker_loop<Q, R>(worker_id: &str, queue: Arc<Q>, runner: Arc<R>)
where
    Q: WorkQueue + ResultTransport + 'static,
    R: NodeRunner + 'static,
{
    while let Some(item) = queue.claim().await {
        let outcome = runner.run(worker_id, &item.node).await;
        queue
            .report(NodeResult {
                job_id: item.job_id,
                worker_id: worker_id.to_string(),
                outcome,
            })
            .await;
    }
}

/// Why [`run_dispatch`] cannot be reused unchanged for the cross-process path,
/// and the minimal driver that replaces it.
///
/// [`run_dispatch`] does TWO things in one process: (1) it `tokio::spawn`s the
/// workers itself and (2) it receives their results over an in-process
/// `mpsc::unbounded_channel`. Both assumptions break across processes: a real
/// worker is a *separate ARC pod* running [`worker_loop`] (the scheduler must
/// NOT spawn it), and an `mpsc` channel has no cross-process endpoint — a pod
/// cannot `tx.send` into the scheduler's receiver. The minimal change is to
/// split the two halves: workers become external [`worker_loop`] processes, and
/// the scheduler keeps ONLY the DAG-ordering publish loop, swapping
/// `res_rx.recv().await` for a poll of [`ResultTransport::next_result`] (the PG
/// `canteiro_results` table). That poll-driven scheduler is this function.
///
/// It publishes a node only once every dep is terminal (canteiro owns the
/// ordering), polls the result transport for completions, and publishes newly
/// unblocked nodes as results land — until every node is terminal, then
/// [`WorkQueue::close`]s so the external workers exit. `idle_poll_interval` is
/// the sleep between empty polls; `max_idle_polls` bounds a stall so the
/// scheduler DEGRADES (returns what it has) rather than hanging forever if a
/// worker vanished mid-item (a claim-lease / visibility-timeout is the named
/// live-gate follow-up — this driver does not yet re-dispatch a lost claim).
pub async fn run_distributed_pg<Q>(
    run: &CiRun,
    queue: Arc<Q>,
    idle_poll_interval: Duration,
    max_idle_polls: u64,
) -> Vec<NodeResult>
where
    Q: WorkQueue + ResultTransport + 'static,
{
    run_distributed_pg_partitioned(run, queue, idle_poll_interval, max_idle_polls, HashSet::new())
        .await
}

/// Shared scheduler loop for [`run_distributed_pg`]. `pre_done` nodes start
/// terminal + never published (the affected-run skip-set); empty for a full run.
async fn run_distributed_pg_partitioned<Q>(
    run: &CiRun,
    queue: Arc<Q>,
    idle_poll_interval: Duration,
    max_idle_polls: u64,
    pre_done: HashSet<String>,
) -> Vec<NodeResult>
where
    Q: WorkQueue + ResultTransport + 'static,
{
    let deps: HashMap<String, Vec<String>> = run
        .nodes
        .iter()
        .map(|n| (n.name.clone(), n.deps.clone()))
        .collect();
    let by_name: HashMap<String, CiNode> =
        run.nodes.iter().map(|n| (n.name.clone(), n.clone())).collect();
    let total = run.nodes.len();

    let mut done: HashSet<String> = pre_done.clone();
    let mut published: HashSet<String> = pre_done;
    let mut results: Vec<NodeResult> = Vec::with_capacity(total);

    let publish_ready = |published: &mut HashSet<String>, done: &HashSet<String>| {
        let mut ready = Vec::new();
        for node in &run.nodes {
            if published.contains(&node.name) {
                continue;
            }
            if deps[&node.name].iter().all(|d| done.contains(d)) {
                published.insert(node.name.clone());
                ready.push(node.name.clone());
            }
        }
        ready
    };

    for name in publish_ready(&mut published, &done) {
        let node = by_name[&name].clone();
        queue
            .publish(WorkItem {
                job_id: run.job_id(&name),
                node,
            })
            .await;
    }

    let mut idle: u64 = 0;
    while done.len() < total {
        match queue.next_result().await {
            Some(result) => {
                idle = 0;
                let name = match &result.job_id.subject {
                    shigoto_types::JobSubject::Pinned(n) => n.clone(),
                    _ => continue,
                };
                done.insert(name);
                results.push(result);
                for ready in publish_ready(&mut published, &done) {
                    let node = by_name[&ready].clone();
                    queue
                        .publish(WorkItem {
                            job_id: run.job_id(&ready),
                            node,
                        })
                        .await;
                }
            }
            None => {
                idle += 1;
                if idle >= max_idle_polls {
                    // Bounded degrade — never hang. A vanished worker leaves a
                    // claimed-but-unreported node; the claim-lease follow-up
                    // will re-dispatch it. Until then, stop rather than spin.
                    break;
                }
                tokio::time::sleep(idle_poll_interval).await;
            }
        }
    }

    // Durable close so external worker pods observe drained+closed and exit.
    queue.close_durable().await;
    results
}

/// Sound affected-aware cross-process run (the PG sibling of
/// [`run_distributed_affected`]): [`partition`] by the diff + a [`CacheProbe`],
/// then drive ONLY the run-set through [`run_distributed_pg`]; skipped nodes are
/// pre-marked terminal so their descendants proceed (served from cache). SOUND
/// BY CONSTRUCTION: with the shipped `UnrealizedProbe` the skip-set is empty, so
/// this is identical to a full [`run_distributed_pg`].
///
/// # Errors
/// [`DecomposeError`] if the run is not a valid DAG (propagated from
/// [`partition`], never swallowed).
pub async fn run_distributed_pg_affected<Q, P>(
    run: &CiRun,
    changed_files: &[String],
    queue: Arc<Q>,
    idle_poll_interval: Duration,
    max_idle_polls: u64,
    probe: &P,
) -> Result<AffectedRun, DecomposeError>
where
    Q: WorkQueue + ResultTransport + 'static,
    P: CacheProbe,
{
    let part = partition(run, changed_files, probe).await?;
    let skip: HashSet<String> = part.skip.iter().cloned().collect();
    let results =
        run_distributed_pg_partitioned(run, queue, idle_poll_interval, max_idle_polls, skip).await;
    Ok(AffectedRun {
        results,
        ran: part.run,
        skipped: part.skip,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canteiro::{ActionRef, EnvClass};
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Barrier;

    fn node(name: &str, deps: &[&str]) -> CiNode {
        CiNode::new(
            name,
            EnvClass::None,
            ActionRef {
                name: name.to_string(),
                command: "true".to_string(),
                args: vec![],
            },
            deps.iter().map(|d| (*d).to_string()).collect(),
        )
    }

    /// Records the order nodes actually ran in — proves the SCHEDULER ordered by
    /// the DAG edges (not the workers).
    struct OrderRecorder {
        order: StdMutex<Vec<String>>,
    }
    #[async_trait]
    impl NodeRunner for OrderRecorder {
        async fn run(&self, _worker_id: &str, node: &CiNode) -> NodeOutcome {
            self.order.lock().unwrap().push(node.name.clone());
            NodeOutcome::Succeeded
        }
    }

    #[tokio::test]
    async fn scheduler_owns_dag_ordering_build_before_test() {
        // build → test: the scheduler must publish `test` only after `build`'s
        // result lands, so `build` runs strictly before `test`.
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes: vec![node("build", &[]), node("test", &["build"])],
        };
        let rec = Arc::new(OrderRecorder {
            order: StdMutex::new(Vec::new()),
        });
        let results =
            run_distributed(&run, 2, Arc::new(InMemoryQueue::default()), rec.clone()).await;

        assert_eq!(results.len(), 2, "both nodes report exactly once");
        assert_eq!(
            *rec.order.lock().unwrap(),
            vec!["build".to_string(), "test".to_string()],
            "the DAG edge, not the workers, ordered execution"
        );
    }

    /// Runs `a` and `b` behind a 2-party barrier — each blocks until BOTH are
    /// claimed. If only one worker existed the barrier would deadlock (the test
    /// would hang → fail), so passing PROVES two distinct workers ran them
    /// concurrently. `c` (deps a,b) never touches the barrier.
    struct ConcurrentProver {
        barrier: Barrier,
        who: StdMutex<HashMap<String, String>>, // node name → worker id
    }
    #[async_trait]
    impl NodeRunner for ConcurrentProver {
        async fn run(&self, worker_id: &str, node: &CiNode) -> NodeOutcome {
            if node.name == "a" || node.name == "b" {
                self.barrier.wait().await; // requires a second worker on the OTHER node
            }
            self.who
                .lock()
                .unwrap()
                .insert(node.name.clone(), worker_id.to_string());
            NodeOutcome::Succeeded
        }
    }

    #[tokio::test]
    async fn dispatches_parallel_nodes_across_distinct_workers() {
        // a ∥ b (no deps) → c (deps a,b). Two workers.
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes: vec![node("a", &[]), node("b", &[]), node("c", &["a", "b"])],
        };
        let prover = Arc::new(ConcurrentProver {
            barrier: Barrier::new(2),
            who: StdMutex::new(HashMap::new()),
        });
        let results =
            run_distributed(&run, 2, Arc::new(InMemoryQueue::default()), prover.clone()).await;

        assert_eq!(results.len(), 3);
        let who = prover.who.lock().unwrap();
        // The barrier already proved 2 workers ran a+b concurrently; confirm
        // they were DISTINCT workers (the cross-worker dispatch claim).
        assert_ne!(
            who.get("a").unwrap(),
            who.get("b").unwrap(),
            "the two ready nodes were claimed by different workers"
        );
        assert!(who.contains_key("c"), "c ran after both a and b were terminal");
    }

    #[tokio::test]
    async fn work_item_roundtrips_through_json() {
        // The dispatch payload must survive the process boundary the destination
        // transport crosses (the whole point of the serde foundation).
        let item = WorkItem {
            job_id: CiRun {
                workspace: "pleme-io".into(),
                repo: "sui".into(),
                nodes: vec![],
            }
            .job_id("build"),
            node: node("build", &[]),
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: WorkItem = serde_json::from_str(&json).unwrap();
        assert_eq!(back.node.name, "build");
    }

    fn node_in(name: &str, deps: &[&str], inputs: &[&str]) -> CiNode {
        node(name, deps).with_inputs(inputs.iter().map(|i| (*i).to_string()).collect())
    }

    #[tokio::test]
    async fn subprocess_runner_maps_exit_status_to_outcome() {
        // `true` → Succeeded; `false` → Failed (the real per-pod runner logic).
        assert_eq!(
            SubprocessRunner.run("w0", &node("ok", &[])).await,
            NodeOutcome::Succeeded
        );
        let mut bad = node("bad", &[]);
        bad.action.command = "false".to_string();
        assert!(matches!(
            SubprocessRunner.run("w0", &bad).await,
            NodeOutcome::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn affected_run_with_unrealized_probe_runs_everything() {
        // Executor-level safety property: UnrealizedProbe skips nothing, so an
        // affected run is identical to a full run — no unsound skip ever ships.
        use crate::canteiro_skip::UnrealizedProbe;
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes: vec![node_in("build", &[], &["src/"]), node_in("test", &["build"], &["tests/"])],
        };
        let rec = Arc::new(OrderRecorder {
            order: StdMutex::new(Vec::new()),
        });
        let out = run_distributed_affected(
            &run,
            &["docs/README.md".into()],
            2,
            Arc::new(InMemoryQueue::default()),
            rec.clone(),
            &UnrealizedProbe,
        )
        .await
        .unwrap();
        assert!(out.skipped.is_empty(), "UnrealizedProbe skips nothing");
        assert_eq!(out.results.len(), 2);
        assert_eq!(rec.order.lock().unwrap().len(), 2, "both nodes actually ran");
    }

    /// A probe reporting a fixed set of node names as cached.
    struct CachedNames(HashSet<String>);
    #[async_trait]
    impl CacheProbe for CachedNames {
        async fn is_output_cached(&self, node: &CiNode) -> bool {
            self.0.contains(&node.name)
        }
    }

    #[tokio::test]
    async fn affected_run_skips_the_unaffected_cached_ancestor() {
        // Diff touches only tests/ → `build` unaffected; mark build cached. build
        // is soundly skipped (its output serves test from cache); test runs. The
        // skipped node is NEVER dispatched (no result, not in the run order).
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes: vec![node_in("build", &[], &["src/"]), node_in("test", &["build"], &["tests/"])],
        };
        let rec = Arc::new(OrderRecorder {
            order: StdMutex::new(Vec::new()),
        });
        let cached: HashSet<String> = ["build".to_string()].into_iter().collect();
        let out = run_distributed_affected(
            &run,
            &["tests/it.rs".into()],
            2,
            Arc::new(InMemoryQueue::default()),
            rec.clone(),
            &CachedNames(cached),
        )
        .await
        .unwrap();
        assert_eq!(out.skipped, vec!["build".to_string()]);
        assert_eq!(out.ran, vec!["test".to_string()]);
        assert_eq!(out.results.len(), 1, "only test was dispatched");
        assert_eq!(
            *rec.order.lock().unwrap(),
            vec!["test".to_string()],
            "the skipped ancestor never ran; its dependent still did"
        );
    }

    // ---- cross-process split protocol (run_distributed_pg + worker_loop) ----
    // These prove the SPLIT: the scheduler polls results via ResultTransport
    // (NOT an in-process mpsc), and workers report through the same transport —
    // the exact shape the PG transport crosses a process boundary with.

    const FAST_POLL: Duration = Duration::from_millis(1);
    const AMPLE_POLLS: u64 = 20_000;

    #[tokio::test]
    async fn pg_split_scheduler_owns_dag_ordering_build_before_test() {
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes: vec![node("build", &[]), node("test", &["build"])],
        };
        let queue = Arc::new(InMemoryQueue::default());
        let rec = Arc::new(OrderRecorder {
            order: StdMutex::new(Vec::new()),
        });
        // Two EXTERNAL workers (stand-ins for two ARC pods), spawned separately
        // from the scheduler — the scheduler never spawns them.
        let workers: Vec<_> = (0..2)
            .map(|w| {
                let mut id = String::from("pod-");
                id.push_str(&w.to_string());
                let q = queue.clone();
                let r = rec.clone();
                tokio::spawn(async move { worker_loop(&id, q, r).await })
            })
            .collect();

        let results = run_distributed_pg(&run, queue.clone(), FAST_POLL, AMPLE_POLLS).await;
        for h in workers {
            let _ = h.await;
        }

        assert_eq!(results.len(), 2, "both nodes report exactly once via PG-shaped transport");
        assert_eq!(
            *rec.order.lock().unwrap(),
            vec!["build".to_string(), "test".to_string()],
            "the DAG edge ordered execution; the scheduler polled results, no mpsc"
        );
    }

    #[tokio::test]
    async fn pg_split_dispatches_parallel_nodes_across_distinct_workers() {
        // a ∥ b (no deps) → c. The 2-party barrier deadlocks (test hangs → fails)
        // unless TWO distinct external workers claim a and b concurrently.
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes: vec![node("a", &[]), node("b", &[]), node("c", &["a", "b"])],
        };
        let queue = Arc::new(InMemoryQueue::default());
        let prover = Arc::new(ConcurrentProver {
            barrier: Barrier::new(2),
            who: StdMutex::new(HashMap::new()),
        });
        let workers: Vec<_> = (0..2)
            .map(|w| {
                let mut id = String::from("pod-");
                id.push_str(&w.to_string());
                let q = queue.clone();
                let p = prover.clone();
                tokio::spawn(async move { worker_loop(&id, q, p).await })
            })
            .collect();

        let results = run_distributed_pg(&run, queue.clone(), FAST_POLL, AMPLE_POLLS).await;
        for h in workers {
            let _ = h.await;
        }

        assert_eq!(results.len(), 3);
        let who = prover.who.lock().unwrap();
        assert_ne!(
            who.get("a").unwrap(),
            who.get("b").unwrap(),
            "a and b were claimed by DISTINCT external workers"
        );
        assert!(who.contains_key("c"), "c ran after a and b were both terminal");
    }

    #[tokio::test]
    async fn pg_split_degrades_when_no_worker_ever_reports() {
        // No workers spawned → no result ever lands. The scheduler must DEGRADE
        // (return what it has) after max_idle_polls, never hang.
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes: vec![node("build", &[])],
        };
        let queue = Arc::new(InMemoryQueue::default());
        let results = run_distributed_pg(&run, queue, Duration::from_millis(1), 5).await;
        assert!(results.is_empty(), "no worker reported, so no results — but no hang");
    }

    #[tokio::test]
    async fn pg_split_affected_skips_unaffected_ancestor() {
        // The cross-process affected run: diff touches only tests/ → build is
        // cached + soundly skipped; only test is dispatched to a worker.
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes: vec![node_in("build", &[], &["src/"]), node_in("test", &["build"], &["tests/"])],
        };
        let queue = Arc::new(InMemoryQueue::default());
        let rec = Arc::new(OrderRecorder {
            order: StdMutex::new(Vec::new()),
        });
        let worker = {
            let q = queue.clone();
            let r = rec.clone();
            tokio::spawn(async move { worker_loop("pod-0", q, r).await })
        };
        let cached: HashSet<String> = ["build".to_string()].into_iter().collect();
        let out = run_distributed_pg_affected(
            &run,
            &["tests/it.rs".into()],
            queue.clone(),
            FAST_POLL,
            AMPLE_POLLS,
            &CachedNames(cached),
        )
        .await
        .unwrap();
        let _ = worker.await;

        assert_eq!(out.skipped, vec!["build".to_string()]);
        assert_eq!(out.ran, vec!["test".to_string()]);
        assert_eq!(out.results.len(), 1, "only test crossed the transport");
        assert_eq!(*rec.order.lock().unwrap(), vec!["test".to_string()]);
    }
}
