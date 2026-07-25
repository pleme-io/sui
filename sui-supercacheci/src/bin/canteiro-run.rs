//! `canteiro-run` — the in-pod live-proof binary for canteiro M1.
//!
//! It constructs the canonical 2-node `build → test` [`CiRun`] (both
//! `env=None`), drives it through [`run_in_process`] (decompose → register each
//! node as a shigoto `RecordingJob` in an `InProcessScheduler` → tick the DAG to
//! terminal in dependency order), prints each node's terminal verdict, and exits
//! `0` iff **both** nodes reached the green terminal, else `1`.
//!
//! ## What this proves — and what it deliberately does NOT
//!
//! This is the **leanest live proof** that the canteiro machinery runs green on
//! a real camelot ARC worker: *decompose → in-process wave execution → both
//! nodes Succeeded*. The node actions are intentionally trivial (`true`) — this
//! proves the **orchestration substrate on a live worker**, NOT "a real build +
//! test with real compilation work". Replacing `true` with a real `cargo build`
//! / `cargo test` action is a later increment (the L1/L2/L8 legs + the caixa
//! `:kind Acao` authoring vocabulary, CANTEIRO §7); this binary is honest that
//! today it exercises the plumbing, not the payload.
//!
//! Run locally with `cargo run -p sui-supercacheci --bin canteiro-run`; the GHA
//! workflow `.github/workflows/canteiro-m1.yml` (workflow_dispatch-only) is what
//! runs it on the `camelot-builder-pleme-eks` ARC pool.

use std::process::ExitCode;

use sui_supercacheci::canteiro::{
    ActionRef, CiNode, CiRun, EnvClass, NodeVerdict, RunReport, run_in_process,
};

/// Build the canonical 2-node run: `build` (no deps) then `test` (depends on
/// `build`), both `env=None`, both running the fast, always-green `true`.
fn build_test_run() -> CiRun {
    let build = CiNode::new(
        "build",
        EnvClass::None,
        ActionRef {
            name: "build".to_string(),
            command: "true".to_string(),
            args: vec![],
        },
        vec![],
    );
    let test = CiNode::new(
        "test",
        EnvClass::None,
        ActionRef {
            name: "test".to_string(),
            command: "true".to_string(),
            args: vec![],
        },
        vec!["build".to_string()],
    );
    CiRun {
        workspace: "pleme-io".to_string(),
        repo: "sui".to_string(),
        nodes: vec![build, test],
    }
}

/// The stable operator label for a terminal verdict (no `format!()` of the
/// emitted string; the `Debug` payload of a `Skipped` reason is rendered inline
/// at the print site).
fn verdict_label(v: &NodeVerdict) -> &'static str {
    match v {
        NodeVerdict::Succeeded => "SUCCEEDED",
        NodeVerdict::Failed => "FAILED",
        NodeVerdict::Skipped(_) => "SKIPPED",
    }
}

/// Print the per-node verdicts of a completed run to stdout.
fn print_report(report: &RunReport) {
    println!("canteiro-run — in-process wave execution report");
    println!("  ticks to converge: {}", report.ticks);
    for (name, verdict) in &report.nodes {
        match verdict {
            NodeVerdict::Skipped(reason) => {
                println!("  node {name:<8} -> {} ({reason:?})", verdict_label(verdict));
            }
            _ => println!("  node {name:<8} -> {}", verdict_label(verdict)),
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let run = build_test_run();
    println!(
        "canteiro-run: decomposing {} nodes for {}/{} (env=None, trivial actions — proving the machinery on a live worker, NOT a real build+test)",
        run.nodes.len(),
        run.workspace,
        run.repo
    );

    let report = match run_in_process(&run).await {
        Ok(report) => report,
        Err(e) => {
            eprintln!("canteiro-run: run failed before producing a report: {e}");
            return ExitCode::FAILURE;
        }
    };

    print_report(&report);

    // Green iff BOTH nodes reached the success terminal.
    let build_ok = matches!(report.verdict("build"), Some(NodeVerdict::Succeeded));
    let test_ok = matches!(report.verdict("test"), Some(NodeVerdict::Succeeded));
    if report.succeeded() && build_ok && test_ok {
        println!("canteiro-run: PASS — both nodes reached the green terminal in dependency order");
        ExitCode::SUCCESS
    } else {
        eprintln!("canteiro-run: FAIL — not every node reached the green terminal");
        ExitCode::FAILURE
    }
}
