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

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shigoto_types::JobId;
use tokio::sync::{mpsc, Mutex, Notify};

use crate::canteiro::{CiNode, CiRun};

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
}

/// In-memory [`WorkQueue`] for the M0 protocol proof — a shared FIFO whose
/// `claim` is a lock-guarded `pop_front` (the faithful analog of `FOR UPDATE
/// SKIP LOCKED`: each item goes to exactly one claimer).
pub struct InMemoryQueue {
    pending: Mutex<VecDeque<WorkItem>>,
    notify: Notify,
    closed: AtomicBool,
}

impl Default for InMemoryQueue {
    fn default() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        }
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

    let mut done: HashSet<String> = HashSet::new();
    let mut published: HashSet<String> = HashSet::new();
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
}
