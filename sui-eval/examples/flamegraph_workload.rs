//! MS2 — a release workload for **wall-clock** profiling (the time lens, to
//! cross-check the counter lens).  Loops a `//`-merge-heavy eval-core workload
//! (the module-fixpoint shape the marquee is dominated by), NO perf counters,
//! so `/usr/bin/sample <pid> <secs>` attributes real wall-clock across the
//! eval — answering "where does the OTHER ~93% (everything the counters don't
//! instrument) actually go?".
//!
//! ```text
//! cargo build --release --example flamegraph_workload -p sui-eval
//! ./target/release/examples/flamegraph_workload &   # prints its own pid
//! sample <pid> 12 -f /tmp/sui-flame.txt
//! ```

const TARGET: &str = r#"
let
  range = builtins.genList (i: i) 300;
  build = n: builtins.foldl'
    (acc: k: acc // { "k${toString k}" = { depth = n; val = k * n; }; })
    {} range;
  tower = builtins.foldl'
    (acc: n: acc // { "level${toString n}" = build n; })
    {} (builtins.genList (i: i) 120);
in builtins.deepSeq tower tower
"#;

fn main() {
    eprintln!("flamegraph_workload pid={} — looping the //-merge eval for ~15s", std::process::id());
    let start = std::time::Instant::now();
    let mut iters = 0u64;
    while start.elapsed().as_secs_f64() < 15.0 {
        // deepSeq inside TARGET forces the whole structure — real deep eval.
        let _ = sui_eval::eval(TARGET);
        iters += 1;
    }
    eprintln!("done: {iters} iters in {:.1}s", start.elapsed().as_secs_f64());
}
