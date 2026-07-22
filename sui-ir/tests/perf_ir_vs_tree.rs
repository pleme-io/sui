//! Measured perf: the L3 flat-IR evaluator (`eval_ir`) vs the tree-walker
//! (`sui_eval::eval`) on compute-heavy PURE workloads — the first MEASURED
//! evidence for the STRATOSPHERE M5 lever (the recon's "2.5–3.4× warm" was a
//! claim; this is data).
//!
//! `#[ignore]` — this is a perf REPORT, not a pass/fail gate (timing is
//! environment-dependent). Run it explicitly:
//!   `cargo test -p sui-ir --test perf_ir_vs_tree -- --ignored --nocapture`
//!
//! It reports the RATIO (tree_walltime / ir_walltime), which is load-robust:
//! both engines run back-to-back under the same machine load, so load inflates
//! both equally and cancels in the ratio — the honest metric under a busy box
//! (docs/STRATOSPHERE.md §0 perf-ratchet load-robustness). Workloads are pure
//! (no filesystem) + small-source/large-compute, so the one-time parse/lower cost
//! is negligible and the ratio is ~pure eval hot-path.
//!
//! CORRECTNESS GATE FIRST: each workload's two engines must render the SAME value
//! (via the shared `common::render` normalizer the differential suite uses) — we
//! refuse to time a divergence, and the `#[test]` FAILS if any workload diverges,
//! so even the ignored perf test doubles as a correctness check when run.

use std::rc::Rc;
use std::time::Instant;

use sui_ir::eval_ir::{eval_ir, IrEnv};
use sui_ir::lower_file;

mod common;
use common::render::{render_ir_value, render_tree};

/// Compute-heavy PURE workloads eval_ir + the tree-walker both handle.
// All PURE, small-source/large-compute, and builtin-driven (no deep USER recursion:
// a `go n acc` non-tail recursion overflows the test thread's default stack even at
// n=2000 — a separate call-depth finding, not this perf test's concern). These
// exercise the eval hot path — genList construction, foldl'/filter closures, list
// force — which is exactly where the rowan re-walk (tree-walker) vs flat-IR (eval_ir)
// difference lives.
const WORKLOADS: &[(&str, &str)] = &[
    ("fold-sum-20k", "builtins.foldl' (a: b: a + b) 0 (builtins.genList (i: i) 20000)"),
    ("genList-length-50k", "builtins.length (builtins.genList (i: i * i) 50000)"),
    ("filter-fold-15k", "builtins.foldl' (a: b: a + b) 0 (builtins.filter (x: x / 2 * 2 == x) (builtins.genList (i: i) 15000))"),
    ("map-genList-30k", "builtins.length (builtins.map (x: x + 1) (builtins.genList (i: i) 30000))"),
];

const WARMUP: u32 = 2;
const ITERS: u32 = 20;

#[test]
#[ignore = "perf report, not a gate — run with --ignored --nocapture"]
fn ir_vs_tree_walker_speedup() {
    println!("\nL3 eval_ir vs tree-walker — pure compute workloads (ratio is load-robust)\n");
    println!("{:<24} {:>12} {:>12} {:>11}  {}", "workload", "tree ms/it", "ir ms/it", "ir speedup", "correctness");

    let mut ratios = Vec::new();
    let mut diverged = Vec::new();

    for (name, src) in WORKLOADS {
        let prog = Rc::new(lower_file(src).unwrap_or_else(|e| panic!("{name}: lower failed: {e}")));
        let env = IrEnv::with_pure_builtins();

        // Correctness gate first — same normalized render both engines.
        let tree_r = match sui_eval::eval(src) {
            Ok(v) => render_tree(&v),
            Err(e) => Err(e.to_string()),
        };
        let ir_r = eval_ir(&prog, prog.root, &env)
            .map_err(|e| format!("{e:?}"))
            .and_then(|v| render_ir_value(&v).map_err(|e| format!("{e:?}")));
        let correct = tree_r == ir_r;
        if !correct {
            println!("{name:<24}  DIVERGE (not timed): tree={tree_r:?} ir={ir_r:?}");
            diverged.push(*name);
            continue;
        }

        // Tree-walker: parse+eval each iter (parse negligible vs the compute).
        for _ in 0..WARMUP {
            let _ = sui_eval::eval(src);
        }
        let t0 = Instant::now();
        for _ in 0..ITERS {
            let _ = sui_eval::eval(src);
        }
        let tree_ms = t0.elapsed().as_secs_f64() * 1000.0 / f64::from(ITERS);

        // eval_ir WARM — lower ONCE (the L3 thesis), eval_ir each iter.
        let env2 = IrEnv::with_pure_builtins();
        for _ in 0..WARMUP {
            let _ = eval_ir(&prog, prog.root, &env2);
        }
        let t1 = Instant::now();
        for _ in 0..ITERS {
            let _ = eval_ir(&prog, prog.root, &env2);
        }
        let ir_ms = t1.elapsed().as_secs_f64() * 1000.0 / f64::from(ITERS);

        let speedup = tree_ms / ir_ms;
        ratios.push(speedup);
        println!("{name:<24} {tree_ms:>12.3} {ir_ms:>12.3} {speedup:>10.2}x  == (byte-match)");
    }

    if !ratios.is_empty() {
        let geomean = (ratios.iter().map(|r| r.ln()).sum::<f64>() / ratios.len() as f64).exp();
        println!("\nGEOMEAN ir speedup over tree-walker: {geomean:.2}x  (n={})", ratios.len());
        println!("(eval_ir walks a flat Vec<Ir>; the tree-walker re-walks the rowan CST each");
        println!(" eval — the shared cost behind the 7x-vs-nix gap that M5 eliminates.)");
    }

    assert!(diverged.is_empty(), "workloads diverged between engines (correctness, not perf): {diverged:?}");
}
