//! canteiro plane (b) — the **cross-process** transport: a Postgres-backed
//! [`WorkQueue`] + [`ResultTransport`] so a `canteiro-worker` pod can claim a
//! [`WorkItem`] and report a [`NodeResult`] across the process boundary that the
//! in-memory M0 (`canteiro_dist::InMemoryQueue` + its in-process `mpsc`) cannot
//! cross.
//!
//! `theory/CANTEIRO.md` §7.1-A: *"a `PgStore FOR UPDATE SKIP LOCKED` queue, one
//! worker per ARC pod."* This module realizes exactly that: the dispatch side is
//! `canteiro_work_items` with an atomic `SELECT … FOR UPDATE SKIP LOCKED` claim
//! (each ready node to exactly one pod — the faithful cross-process analog of
//! `InMemoryQueue`'s lock-guarded `pop_front`), and the RESULT side — which
//! cannot use the in-process `mpsc` — is `canteiro_results`, a second table the
//! scheduler polls (`ResultTransport::next_result`) and workers append to
//! (`ResultTransport::report`).
//!
//! # The connection seam (Environment / testability contract)
//!
//! Mirrors [`sui_castore`'s `PgCacheConn`]: [`PgWorkQueue`] is generic over
//! [`PgQueueConn`] — the minimal typed row-verb surface (`enqueue` / `claim_one`
//! / `set_closed` / `is_closed` / `report` / `next_result`). Unit tests inject an
//! in-memory mock; production injects [`SqlxPgQueueConn`] (a real `sqlx`
//! Postgres pool, behind the `postgres` feature). The whole claim/report state
//! machine — JSON (de)serialization, job-key derivation, the claimed/pending
//! transition, the closed-and-drained exit — is proven against the mock **with
//! no live Postgres required**.
//!
//! # Tier-honest scope (never round up)
//!
//! **Mock-proven, compiles behind the `postgres` feature.** The *atomicity* of
//! `FOR UPDATE SKIP LOCKED` under real concurrency — that two live pods hitting
//! the same pending row never both win it — is a property of Postgres itself,
//! NOT of the mock (the mock serializes through a `Mutex`, which proves the
//! state machine but is a single process). That, plus worker-death recovery (a
//! claimed-but-unreported node currently blocks its descendants; a claim-lease /
//! visibility-timeout is the named follow-up), is the **live gate**: a real
//! Postgres + ≥2 `canteiro-worker` pods on camelot-eks. This module does NOT
//! prove that; it proves the protocol + the SQL translation shape off any
//! cluster.

use std::time::Duration;

use async_trait::async_trait;

use crate::canteiro_dist::{NodeResult, ResultTransport, WorkItem, WorkQueue};

/// A typed error for the cross-process transport. `#[error(...)]` is the typed
/// error surface (★★ TYPED EMISSION); there is no free-form `format!()` of an
/// emitted string.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// A `WorkItem`/`NodeResult`/`JobId` failed to (de)serialize to/from the
    /// stored JSON.
    #[error("canteiro queue serde: {0}")]
    Serde(#[from] serde_json::Error),
    /// The underlying transport (Postgres) returned an error.
    #[error("canteiro queue transport: {0}")]
    Transport(String),
}

/// The minimal typed Postgres row-verb surface [`PgWorkQueue`] depends on — the
/// injectable **Environment seam**. A real implementation ([`SqlxPgQueueConn`],
/// `postgres` feature) talks to a live pool; tests substitute an in-memory mock.
///
/// `queue_id` namespaces one CI run's rows so concurrent runs share the tables
/// without colliding (the `InMemoryQueue` is one-instance-per-run; the durable
/// tables need the explicit key).
#[async_trait]
pub trait PgQueueConn: Send + Sync {
    /// Insert a ready item (idempotent by `(queue_id, job_key)` — a node is
    /// published at most once; a duplicate publish is a no-op).
    async fn enqueue(&self, queue_id: &str, job_key: &str, item_json: &str)
        -> Result<(), QueueError>;

    /// Atomically claim one pending item for `worker_id` (`FOR UPDATE SKIP
    /// LOCKED`): mark it claimed and return its serialized [`WorkItem`], or
    /// `None` if none is pending. Each pending row goes to exactly one caller.
    async fn claim_one(&self, queue_id: &str, worker_id: &str)
        -> Result<Option<String>, QueueError>;

    /// Set the run's "closed" sentinel — no more items will be published.
    async fn set_closed(&self, queue_id: &str) -> Result<(), QueueError>;

    /// Read the "closed" sentinel (default `false` if unset).
    async fn is_closed(&self, queue_id: &str) -> Result<bool, QueueError>;

    /// Append a worker's serialized [`NodeResult`] to the result table.
    async fn report(
        &self,
        queue_id: &str,
        job_key: &str,
        worker_id: &str,
        result_json: &str,
    ) -> Result<(), QueueError>;

    /// Atomically consume the next unconsumed result (`FOR UPDATE SKIP LOCKED`):
    /// mark it consumed and return its serialized [`NodeResult`], or `None`.
    /// Each result goes to exactly one caller (the scheduler).
    async fn next_result(&self, queue_id: &str) -> Result<Option<String>, QueueError>;
}

/// The default poll interval a worker's `claim` loop sleeps between empty
/// atomic-claim attempts (there is no cross-process `Notify`, so a worker polls).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// After this many consecutive transport errors, a `claim` loop DEGRADES
/// (returns `None`, the worker exits) rather than spinning forever — the
/// never-hang discipline applied to a persistently-broken connection.
const MAX_CONSECUTIVE_CLAIM_ERRORS: u32 = 10;

/// Postgres-backed [`WorkQueue`] + [`ResultTransport`]: the durable cross-process
/// transport, shared across pods, keyed by `queue_id`.
///
/// Generic over the [`PgQueueConn`] seam so the whole state machine is testable
/// against a mock. Each participant (each ARC worker pod, and the scheduler
/// process) constructs its OWN `PgWorkQueue` with its own `worker_id` and its own
/// connection to the SAME Postgres — coordination is via the DB, never a shared
/// in-process handle.
pub struct PgWorkQueue<C: PgQueueConn> {
    conn: C,
    queue_id: String,
    /// This participant's identity stamped into `claimed_by` on a claim (the pod
    /// name at the destination; the scheduler process uses a fixed marker since
    /// it only publishes + polls results, never claims work).
    worker_id: String,
    poll_interval: Duration,
}

impl<C: PgQueueConn> PgWorkQueue<C> {
    /// Wrap a [`PgQueueConn`] for run `queue_id`, stamping claims with
    /// `worker_id`. Uses [`DEFAULT_POLL_INTERVAL`].
    pub fn new(conn: C, queue_id: impl Into<String>, worker_id: impl Into<String>) -> Self {
        Self {
            conn,
            queue_id: queue_id.into(),
            worker_id: worker_id.into(),
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    /// Override the claim poll interval (tests use a tiny value).
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Borrow the underlying connection (composition / diagnostics).
    pub fn conn(&self) -> &C {
        &self.conn
    }

    // -- Fallible inherent surface: the error-aware path a worker/scheduler uses
    //    when it needs to observe a transport failure (the `WorkQueue` /
    //    `ResultTransport` trait methods below wrap these, logging + erasing the
    //    error since those signatures return no `Result`). --

    /// Publish one item (fallible). `job_key` is the JSON of the item's `JobId`
    /// — the stable per-node key.
    ///
    /// # Errors
    /// [`QueueError`] on a serialize or transport failure.
    pub async fn try_publish(&self, item: &WorkItem) -> Result<(), QueueError> {
        let job_key = serde_json::to_string(&item.job_id)?;
        let item_json = serde_json::to_string(item)?;
        self.conn.enqueue(&self.queue_id, &job_key, &item_json).await
    }

    /// Attempt ONE atomic claim (no loop) for this queue's `worker_id`.
    ///
    /// # Errors
    /// [`QueueError`] on a transport or deserialize failure.
    pub async fn try_claim(&self) -> Result<Option<WorkItem>, QueueError> {
        match self.conn.claim_one(&self.queue_id, &self.worker_id).await? {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// Report a result (fallible) — the honest per-pod path.
    ///
    /// # Errors
    /// [`QueueError`] on a serialize or transport failure.
    pub async fn try_report(&self, result: &NodeResult) -> Result<(), QueueError> {
        let job_key = serde_json::to_string(&result.job_id)?;
        let result_json = serde_json::to_string(result)?;
        self.conn
            .report(&self.queue_id, &job_key, &result.worker_id, &result_json)
            .await
    }

    /// Consume the next result (fallible).
    ///
    /// # Errors
    /// [`QueueError`] on a transport or deserialize failure.
    pub async fn try_next_result(&self) -> Result<Option<NodeResult>, QueueError> {
        match self.conn.next_result(&self.queue_id).await? {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// Set the durable "closed" sentinel (fallible).
    ///
    /// # Errors
    /// [`QueueError`] on a transport failure.
    pub async fn try_close(&self) -> Result<(), QueueError> {
        self.conn.set_closed(&self.queue_id).await
    }

    /// Read the durable "closed" sentinel (fallible).
    ///
    /// # Errors
    /// [`QueueError`] on a transport failure.
    pub async fn closed(&self) -> Result<bool, QueueError> {
        self.conn.is_closed(&self.queue_id).await
    }
}

#[async_trait]
impl<C: PgQueueConn> WorkQueue for PgWorkQueue<C> {
    async fn publish(&self, item: WorkItem) {
        if let Err(e) = self.try_publish(&item).await {
            // Lossy by the trait's non-fallible shape; the scheduler degrades
            // via its idle-poll cap if a dropped publish strands descendants.
            tracing::error!(error = %e, "canteiro PgWorkQueue: publish failed, item not enqueued");
        }
    }

    async fn claim(&self) -> Option<WorkItem> {
        // The cross-process claim loop: an atomic SKIP-LOCKED claim; on empty,
        // exit iff the run is closed (drained + no more coming), else poll. A
        // persistently-broken connection degrades after a bounded error count.
        let mut errors: u32 = 0;
        loop {
            match self.try_claim().await {
                Ok(Some(item)) => return Some(item),
                Ok(None) => {
                    errors = 0;
                    match self.closed().await {
                        Ok(true) => return None,
                        Ok(false) => {}
                        Err(e) => {
                            tracing::error!(error = %e, "canteiro PgWorkQueue: is_closed failed");
                            return None; // degrade rather than spin blind
                        }
                    }
                    tokio::time::sleep(self.poll_interval).await;
                }
                Err(e) => {
                    errors += 1;
                    tracing::error!(error = %e, attempt = errors, "canteiro PgWorkQueue: claim failed");
                    if errors >= MAX_CONSECUTIVE_CLAIM_ERRORS {
                        return None; // never-hang: give up on a broken connection
                    }
                    tokio::time::sleep(self.poll_interval).await;
                }
            }
        }
    }

    fn close(&self) {
        // The synchronous form cannot perform the durable DB write; the durable
        // sentinel goes through `close_durable`, which the PG scheduler
        // (`run_distributed_pg`) calls. A sync `close()` here is a no-op with a
        // loud note so a mis-wired in-process caller is caught, not silently
        // ineffective.
        tracing::warn!(
            "canteiro PgWorkQueue::close() is a no-op — use close_durable() (async) to set the PG sentinel"
        );
    }

    async fn close_durable(&self) {
        if let Err(e) = self.try_close().await {
            tracing::error!(error = %e, "canteiro PgWorkQueue: close_durable failed");
        }
    }
}

#[async_trait]
impl<C: PgQueueConn> ResultTransport for PgWorkQueue<C> {
    async fn report(&self, result: NodeResult) {
        if let Err(e) = self.try_report(&result).await {
            tracing::error!(error = %e, "canteiro PgWorkQueue: report failed, result not recorded");
        }
    }

    async fn next_result(&self) -> Option<NodeResult> {
        match self.try_next_result().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "canteiro PgWorkQueue: next_result failed");
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Production transport — real sqlx Postgres pool, gated behind the `postgres`
// feature so the default build + unit tests pull zero driver surface. The
// state machine above is proven against the in-memory mock; this is the thin
// SQL layer (the `FOR UPDATE SKIP LOCKED` claim + result-consume are the only
// bits the mock cannot prove — that atomicity is the named live gate).
// ---------------------------------------------------------------------------

#[cfg(feature = "postgres")]
mod sqlx_conn {
    use super::{PgQueueConn, QueueError};
    use async_trait::async_trait;
    use sqlx::postgres::{PgPool, PgPoolOptions};
    use sqlx::Row;

    fn to_err(e: sqlx::Error) -> QueueError {
        let mut s = String::from("postgres: ");
        s.push_str(&e.to_string());
        QueueError::Transport(s)
    }

    // Full static SQL strings (typed emission — no runtime assembly of table
    // names or predicates).

    const DDL_WORK_ITEMS: &str = "CREATE TABLE IF NOT EXISTS canteiro_work_items (\
        queue_id TEXT NOT NULL, \
        job_key TEXT NOT NULL, \
        item_json TEXT NOT NULL, \
        claim_state TEXT NOT NULL DEFAULT 'pending', \
        claimed_by TEXT, \
        seq BIGSERIAL, \
        PRIMARY KEY (queue_id, job_key))";

    const DDL_CONTROL: &str = "CREATE TABLE IF NOT EXISTS canteiro_queue_control (\
        queue_id TEXT PRIMARY KEY, \
        closed BOOLEAN NOT NULL DEFAULT FALSE)";

    const DDL_RESULTS: &str = "CREATE TABLE IF NOT EXISTS canteiro_results (\
        seq BIGSERIAL PRIMARY KEY, \
        queue_id TEXT NOT NULL, \
        job_key TEXT NOT NULL, \
        worker_id TEXT NOT NULL, \
        result_json TEXT NOT NULL, \
        consumed BOOLEAN NOT NULL DEFAULT FALSE)";

    const ENQUEUE_SQL: &str = "INSERT INTO canteiro_work_items (queue_id, job_key, item_json) \
        VALUES ($1, $2, $3) ON CONFLICT (queue_id, job_key) DO NOTHING";

    // The canonical atomic job-claim: pick the oldest pending row for this
    // queue, lock it skipping any a sibling pod already holds, mark it claimed,
    // and return its payload — all in one statement.
    const CLAIM_SQL: &str = "WITH claimed AS (\
        SELECT queue_id, job_key FROM canteiro_work_items \
        WHERE queue_id = $1 AND claim_state = 'pending' \
        ORDER BY seq FOR UPDATE SKIP LOCKED LIMIT 1) \
        UPDATE canteiro_work_items w SET claim_state = 'claimed', claimed_by = $2 \
        FROM claimed WHERE w.queue_id = claimed.queue_id AND w.job_key = claimed.job_key \
        RETURNING w.item_json";

    const SET_CLOSED_SQL: &str = "INSERT INTO canteiro_queue_control (queue_id, closed) \
        VALUES ($1, TRUE) ON CONFLICT (queue_id) DO UPDATE SET closed = TRUE";

    const IS_CLOSED_SQL: &str = "SELECT closed FROM canteiro_queue_control WHERE queue_id = $1";

    const REPORT_SQL: &str =
        "INSERT INTO canteiro_results (queue_id, job_key, worker_id, result_json) \
         VALUES ($1, $2, $3, $4)";

    // The result-consume mirror of the claim: oldest unconsumed result for this
    // queue, locked skipping any the scheduler is already consuming, marked
    // consumed, payload returned.
    const NEXT_RESULT_SQL: &str = "WITH nxt AS (\
        SELECT seq FROM canteiro_results \
        WHERE queue_id = $1 AND consumed = FALSE \
        ORDER BY seq FOR UPDATE SKIP LOCKED LIMIT 1) \
        UPDATE canteiro_results r SET consumed = TRUE \
        FROM nxt WHERE r.seq = nxt.seq RETURNING r.result_json";

    /// Production [`PgQueueConn`] over a `sqlx` Postgres connection pool.
    pub struct SqlxPgQueueConn {
        pool: PgPool,
    }

    impl SqlxPgQueueConn {
        /// Connect to `url`, bound the pool at `max_conns`, and ensure the three
        /// canteiro transport tables exist.
        ///
        /// # Errors
        /// [`QueueError::Transport`] if the pool cannot be built or the DDL fails.
        pub async fn connect(url: &str, max_conns: u32) -> Result<Self, QueueError> {
            let pool = PgPoolOptions::new()
                .max_connections(max_conns)
                .connect(url)
                .await
                .map_err(to_err)?;
            let this = Self { pool };
            this.ensure_schema().await?;
            Ok(this)
        }

        async fn ensure_schema(&self) -> Result<(), QueueError> {
            for ddl in [DDL_WORK_ITEMS, DDL_CONTROL, DDL_RESULTS] {
                sqlx::query(ddl).execute(&self.pool).await.map_err(to_err)?;
            }
            Ok(())
        }
    }

    #[async_trait]
    impl PgQueueConn for SqlxPgQueueConn {
        async fn enqueue(
            &self,
            queue_id: &str,
            job_key: &str,
            item_json: &str,
        ) -> Result<(), QueueError> {
            sqlx::query(ENQUEUE_SQL)
                .bind(queue_id)
                .bind(job_key)
                .bind(item_json)
                .execute(&self.pool)
                .await
                .map_err(to_err)?;
            Ok(())
        }

        async fn claim_one(
            &self,
            queue_id: &str,
            worker_id: &str,
        ) -> Result<Option<String>, QueueError> {
            let row = sqlx::query(CLAIM_SQL)
                .bind(queue_id)
                .bind(worker_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(to_err)?;
            match row {
                Some(r) => Ok(Some(r.try_get::<String, _>("item_json").map_err(to_err)?)),
                None => Ok(None),
            }
        }

        async fn set_closed(&self, queue_id: &str) -> Result<(), QueueError> {
            sqlx::query(SET_CLOSED_SQL)
                .bind(queue_id)
                .execute(&self.pool)
                .await
                .map_err(to_err)?;
            Ok(())
        }

        async fn is_closed(&self, queue_id: &str) -> Result<bool, QueueError> {
            let row = sqlx::query(IS_CLOSED_SQL)
                .bind(queue_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(to_err)?;
            match row {
                Some(r) => Ok(r.try_get::<bool, _>("closed").map_err(to_err)?),
                None => Ok(false),
            }
        }

        async fn report(
            &self,
            queue_id: &str,
            job_key: &str,
            worker_id: &str,
            result_json: &str,
        ) -> Result<(), QueueError> {
            sqlx::query(REPORT_SQL)
                .bind(queue_id)
                .bind(job_key)
                .bind(worker_id)
                .bind(result_json)
                .execute(&self.pool)
                .await
                .map_err(to_err)?;
            Ok(())
        }

        async fn next_result(&self, queue_id: &str) -> Result<Option<String>, QueueError> {
            let row = sqlx::query(NEXT_RESULT_SQL)
                .bind(queue_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(to_err)?;
            match row {
                Some(r) => Ok(Some(r.try_get::<String, _>("result_json").map_err(to_err)?)),
                None => Ok(None),
            }
        }
    }

    impl super::PgWorkQueue<SqlxPgQueueConn> {
        /// Connect a `PgWorkQueue` to a Postgres `url` for run `queue_id`,
        /// stamping claims with `worker_id`, pool-bounded at `max_conns`.
        ///
        /// # Errors
        /// Propagates a connect/schema failure from [`SqlxPgQueueConn::connect`].
        pub async fn connect(
            url: &str,
            queue_id: impl Into<String>,
            worker_id: impl Into<String>,
            max_conns: u32,
        ) -> Result<Self, QueueError> {
            Ok(Self::new(
                SqlxPgQueueConn::connect(url, max_conns).await?,
                queue_id,
                worker_id,
            ))
        }
    }
}

#[cfg(feature = "postgres")]
pub use sqlx_conn::SqlxPgQueueConn;

// ---------------------------------------------------------------------------
// Unit tests — the transport state machine proven against an in-memory mock
// PgQueueConn. No live Postgres required. The mock serializes through a Mutex
// (proving the state machine, one process); real SKIP-LOCKED atomicity under
// concurrent pods is the named live gate.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canteiro::{ActionRef, CiNode, CiRun, EnvClass};
    use crate::canteiro_dist::{
        run_distributed_pg, worker_loop, NodeOutcome, NodeRunner,
    };
    use std::collections::{HashSet, VecDeque};
    use std::sync::{Arc, Mutex as StdMutex};

    /// One stored work-item row in the mock.
    struct MockItem {
        queue_id: String,
        job_key: String,
        item_json: String,
        claimed: bool,
    }

    /// One stored result row in the mock.
    struct MockResult {
        queue_id: String,
        json: String,
        consumed: bool,
    }

    #[derive(Default)]
    struct MockState {
        items: Vec<MockItem>,
        results: VecDeque<MockResult>,
        closed: HashSet<String>,
    }

    /// In-memory [`PgQueueConn`] mock. A cheap `Arc<Mutex<_>>` handle so many
    /// `PgWorkQueue`s (scheduler + N workers) share ONE state — the mock stand-in
    /// for "the same database". `claim_one` / `next_result` lock, so each row
    /// goes to exactly one caller (the state-machine analog of SKIP LOCKED).
    #[derive(Clone, Default)]
    struct MockPgQueueConn {
        state: Arc<StdMutex<MockState>>,
    }

    #[async_trait]
    impl PgQueueConn for MockPgQueueConn {
        async fn enqueue(
            &self,
            queue_id: &str,
            job_key: &str,
            item_json: &str,
        ) -> Result<(), QueueError> {
            let mut s = self.state.lock().unwrap();
            // ON CONFLICT (queue_id, job_key) DO NOTHING.
            if s.items
                .iter()
                .any(|i| i.queue_id == queue_id && i.job_key == job_key)
            {
                return Ok(());
            }
            s.items.push(MockItem {
                queue_id: queue_id.to_string(),
                job_key: job_key.to_string(),
                item_json: item_json.to_string(),
                claimed: false,
            });
            Ok(())
        }

        async fn claim_one(
            &self,
            queue_id: &str,
            _worker_id: &str,
        ) -> Result<Option<String>, QueueError> {
            let mut s = self.state.lock().unwrap();
            for i in &mut s.items {
                if i.queue_id == queue_id && !i.claimed {
                    i.claimed = true;
                    return Ok(Some(i.item_json.clone()));
                }
            }
            Ok(None)
        }

        async fn set_closed(&self, queue_id: &str) -> Result<(), QueueError> {
            self.state.lock().unwrap().closed.insert(queue_id.to_string());
            Ok(())
        }

        async fn is_closed(&self, queue_id: &str) -> Result<bool, QueueError> {
            Ok(self.state.lock().unwrap().closed.contains(queue_id))
        }

        async fn report(
            &self,
            queue_id: &str,
            _job_key: &str,
            _worker_id: &str,
            result_json: &str,
        ) -> Result<(), QueueError> {
            self.state.lock().unwrap().results.push_back(MockResult {
                queue_id: queue_id.to_string(),
                json: result_json.to_string(),
                consumed: false,
            });
            Ok(())
        }

        async fn next_result(&self, queue_id: &str) -> Result<Option<String>, QueueError> {
            let mut s = self.state.lock().unwrap();
            for r in &mut s.results {
                if r.queue_id == queue_id && !r.consumed {
                    r.consumed = true;
                    return Ok(Some(r.json.clone()));
                }
            }
            Ok(None)
        }
    }

    const QID: &str = "pleme-io/sui@abc123";

    fn ci_node(name: &str, deps: &[&str]) -> CiNode {
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

    fn run_of(nodes: Vec<CiNode>) -> CiRun {
        CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes,
        }
    }

    fn work_item(run: &CiRun, name: &str) -> WorkItem {
        WorkItem {
            job_id: run.job_id(name),
            node: ci_node(name, &[]),
        }
    }

    fn queue(conn: MockPgQueueConn, worker: &str) -> PgWorkQueue<MockPgQueueConn> {
        PgWorkQueue::new(conn, QID, worker).with_poll_interval(Duration::from_millis(1))
    }

    #[tokio::test]
    async fn publish_then_claim_roundtrips_the_work_item() {
        let run = run_of(vec![ci_node("build", &[])]);
        let q = queue(MockPgQueueConn::default(), "pod-0");
        q.try_publish(&work_item(&run, "build")).await.unwrap();

        let claimed = q.try_claim().await.unwrap().expect("one pending item");
        assert_eq!(claimed.node.name, "build");
        assert_eq!(claimed.job_id, run.job_id("build"), "the JobId survived the JSON roundtrip");
    }

    #[tokio::test]
    async fn a_claimed_item_is_not_claimed_again() {
        let run = run_of(vec![ci_node("build", &[])]);
        let q = queue(MockPgQueueConn::default(), "pod-0");
        q.try_publish(&work_item(&run, "build")).await.unwrap();

        assert!(q.try_claim().await.unwrap().is_some(), "first claim wins the item");
        assert!(
            q.try_claim().await.unwrap().is_none(),
            "the item is marked claimed — a second claim gets nothing (SKIP-LOCKED analog)"
        );
    }

    #[tokio::test]
    async fn two_items_are_each_claimed_exactly_once_across_shared_conn() {
        // Two PgWorkQueues (two pods) share ONE mock DB; two published items go
        // one-each, never both to one claimer.
        let conn = MockPgQueueConn::default();
        let run = run_of(vec![ci_node("a", &[]), ci_node("b", &[])]);
        let publisher = queue(conn.clone(), "scheduler");
        publisher.try_publish(&work_item(&run, "a")).await.unwrap();
        publisher.try_publish(&work_item(&run, "b")).await.unwrap();

        let p0 = queue(conn.clone(), "pod-0");
        let p1 = queue(conn.clone(), "pod-1");
        let c0 = p0.try_claim().await.unwrap().unwrap();
        let c1 = p1.try_claim().await.unwrap().unwrap();
        assert_ne!(c0.node.name, c1.node.name, "each item claimed exactly once");
        assert!(p0.try_claim().await.unwrap().is_none(), "queue drained");
    }

    #[tokio::test]
    async fn report_then_next_result_roundtrips_and_consumes() {
        let run = run_of(vec![ci_node("build", &[])]);
        let q = queue(MockPgQueueConn::default(), "pod-0");
        let result = NodeResult {
            job_id: run.job_id("build"),
            worker_id: "pod-0".to_string(),
            outcome: NodeOutcome::Succeeded,
        };
        q.try_report(&result).await.unwrap();

        let got = q.try_next_result().await.unwrap().expect("one result");
        assert_eq!(got.job_id, run.job_id("build"));
        assert_eq!(got.outcome, NodeOutcome::Succeeded);
        assert!(
            q.try_next_result().await.unwrap().is_none(),
            "the result was consumed — polled exactly once"
        );
    }

    #[tokio::test]
    async fn claim_loop_returns_none_only_after_close_when_drained() {
        // The WorkQueue::claim loop: with nothing pending and not-yet-closed it
        // would poll forever; closing lets it exit None (the worker-exit signal).
        let q = queue(MockPgQueueConn::default(), "pod-0");
        q.try_close().await.unwrap();
        assert!(q.closed().await.unwrap());
        assert!(
            <PgWorkQueue<MockPgQueueConn> as WorkQueue>::claim(&q).await.is_none(),
            "drained + closed → claim exits None"
        );
    }

    #[tokio::test]
    async fn close_durable_sets_the_sentinel() {
        let q = queue(MockPgQueueConn::default(), "scheduler");
        assert!(!q.closed().await.unwrap());
        q.close_durable().await;
        assert!(q.closed().await.unwrap(), "close_durable wrote the PG sentinel");
    }

    /// Records the order nodes ran in (proves the scheduler ordered by the DAG).
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
    async fn full_cross_process_run_over_pg_workqueue_type() {
        // The fullest mock proof: the ACTUAL production PgWorkQueue type driving a
        // build → test run across a scheduler + two external worker_loops, all
        // sharing one mock "database". Proves the whole split protocol (publish →
        // SKIP-LOCKED claim → report → poll) end to end on the real type; only
        // real-PG concurrency atomicity remains gated.
        let conn = MockPgQueueConn::default();
        let run = run_of(vec![ci_node("build", &[]), ci_node("test", &["build"])]);
        let rec = Arc::new(OrderRecorder {
            order: StdMutex::new(Vec::new()),
        });

        // Two worker pods, each its own PgWorkQueue over the shared conn.
        let workers: Vec<_> = (0..2)
            .map(|w| {
                let mut id = String::from("pod-");
                id.push_str(&w.to_string());
                let wq = Arc::new(queue(conn.clone(), &id));
                let r = rec.clone();
                tokio::spawn(async move { worker_loop(&id, wq, r).await })
            })
            .collect();

        // The scheduler's own PgWorkQueue over the same conn.
        let sched = Arc::new(queue(conn.clone(), "scheduler"));
        let results =
            run_distributed_pg(&run, sched, Duration::from_millis(1), 20_000).await;
        for h in workers {
            let _ = h.await;
        }

        assert_eq!(results.len(), 2, "both nodes reported through the PG-shaped result table");
        assert_eq!(
            *rec.order.lock().unwrap(),
            vec!["build".to_string(), "test".to_string()],
            "the DAG edge ordered execution across the cross-process transport"
        );
    }
}
