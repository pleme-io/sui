//! `canteiro-worker` — the per-pod cross-process CI worker (CANTEIRO §7.1-A).
//!
//! This is what ONE camelot-eks ARC pod runs. It connects to the shared
//! Postgres transport, then loops: **claim** a [`WorkItem`] from the
//! `canteiro_work_items` queue (atomic `FOR UPDATE SKIP LOCKED`), **run** the
//! node's action via the shipped [`SubprocessRunner`], **report** the
//! [`NodeResult`] back through the `canteiro_results` table, and **repeat** until
//! the run is drained + closed (the scheduler's `run_distributed_pg` set the
//! sentinel), at which point `claim` returns `None` and the loop — and the pod —
//! exits `0`.
//!
//! The scheduler runs `run_distributed_pg` in a SEPARATE process; it owns the
//! DAG ordering and only publishes a node once its deps are terminal. N of these
//! workers run in parallel, one per pod, coordinating solely through Postgres.
//!
//! ## Configuration (env)
//!
//! | var | meaning | required |
//! |-----|---------|----------|
//! | `CANTEIRO_PG_URL` | Postgres connection URL | yes |
//! | `CANTEIRO_QUEUE_ID` | the run's queue namespace (matches the scheduler) | yes |
//! | `CANTEIRO_WORKER_ID` → `POD_NAME` → `HOSTNAME` | this worker's identity | falls back |
//! | `CANTEIRO_PG_MAX_CONNS` | pool size (default 4) | no |
//!
//! ## Tier-honest (never round up)
//!
//! This binary compiles behind the `postgres` feature and its logic — connect →
//! `worker_loop` over the real `PgWorkQueue<SqlxPgQueueConn>` — is exercised at
//! the mock level in `canteiro_pg`'s tests. What is **NOT** proven here: a live
//! run against a real Postgres with ≥2 of these pods (the SKIP-LOCKED atomicity
//! + a full green DAG across pods). That is the named live gate.

// Without the `postgres` feature there is no `SqlxPgQueueConn`, so the real body
// cannot compile. Provide a stub `main` so the default `cargo build` /
// `cargo test` (which always builds every `[[bin]]`) still succeeds; the real
// worker is `cargo build --features postgres --bin canteiro-worker`.
#[cfg(not(feature = "postgres"))]
fn main() -> std::process::ExitCode {
    eprintln!(
        "canteiro-worker requires the `postgres` feature: \
         cargo build -p sui-supercacheci --features postgres --bin canteiro-worker"
    );
    std::process::ExitCode::FAILURE
}

#[cfg(feature = "postgres")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    use std::sync::Arc;

    use sui_supercacheci::canteiro_dist::{worker_loop, SubprocessRunner};
    use sui_supercacheci::canteiro_pg::PgWorkQueue;

    // --- required config ---
    let Ok(pg_url) = std::env::var("CANTEIRO_PG_URL") else {
        eprintln!("canteiro-worker: CANTEIRO_PG_URL is required");
        return ExitCode::FAILURE;
    };
    let Ok(queue_id) = std::env::var("CANTEIRO_QUEUE_ID") else {
        eprintln!("canteiro-worker: CANTEIRO_QUEUE_ID is required");
        return ExitCode::FAILURE;
    };

    // --- worker identity: explicit → pod name → hostname → generic ---
    let worker_id = std::env::var("CANTEIRO_WORKER_ID")
        .or_else(|_| std::env::var("POD_NAME"))
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "canteiro-worker".to_string());

    let max_conns: u32 = std::env::var("CANTEIRO_PG_MAX_CONNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);

    println!(
        "canteiro-worker: worker={worker_id} queue={queue_id} connecting to Postgres (max_conns={max_conns})"
    );

    let queue = match PgWorkQueue::connect(&pg_url, &queue_id, &worker_id, max_conns).await {
        Ok(q) => Arc::new(q),
        Err(e) => {
            eprintln!("canteiro-worker: failed to connect/init the PG transport: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("canteiro-worker: connected; entering claim → run → report loop");
    worker_loop(&worker_id, queue, Arc::new(SubprocessRunner)).await;
    println!("canteiro-worker: queue drained + closed; exiting cleanly");
    ExitCode::SUCCESS
}
