//! Ident/apply/force-heavy workload for interleaved-A/B of the ident-intern
//! lever (foldl' over a 1M list — 2M ident evals + 1M lambda applications).
const T: &str = "builtins.foldl' (acc: n: acc + n) 0 (builtins.genList (i: i) 1000000)";
fn main() {
    let s = std::time::Instant::now();
    let mut n = 0u64;
    while s.elapsed().as_secs_f64() < 12.0 {
        let _ = sui_eval::eval(T);
        n += 1;
    }
    eprintln!("done: {n} iters");
}
