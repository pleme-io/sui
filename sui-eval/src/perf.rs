//! Lightweight evaluation profiling counters.
//! Enabled via `SUI_EVAL_PERF=1` environment variable.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Cached flag — checked once at startup to avoid repeated `env::var` calls.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Call once at the start of evaluation to check the env var.
pub fn init() {
    let on = std::env::var("SUI_EVAL_PERF")
        .map(|v| v == "1")
        .unwrap_or(false);
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether profiling is enabled (fast atomic load).
#[inline(always)]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

thread_local! {
    static COUNTERS: RefCell<PerfCounters> = RefCell::new(PerfCounters::default());
    static START: RefCell<Option<Instant>> = RefCell::new(None);
}

#[derive(Default)]
pub struct PerfCounters {
    pub eval_expr_calls: u64,
    pub force_value_calls: u64,
    pub thunk_forces: u64,
    pub thunk_cache_hits: u64,
    pub import_calls: u64,
    pub import_cache_hits: u64,
    pub apply_calls: u64,
    pub select_calls: u64,
    pub attrset_constructions: u64,
    pub env_clones: u64,
    pub env_lookups: u64,
    pub env_lookup_depth_total: u64,
}

pub fn start() {
    if enabled() {
        START.with(|s| *s.borrow_mut() = Some(Instant::now()));
    }
}

/// How often to print a progress snapshot (every N eval_expr calls).
const PROGRESS_INTERVAL: u64 = 1_000_000;

#[inline(always)]
pub fn inc(field: &str) {
    if !enabled() {
        return;
    }
    COUNTERS.with(|c| {
        let mut c = c.borrow_mut();
        match field {
            "eval_expr" => {
                c.eval_expr_calls += 1;
                if c.eval_expr_calls % PROGRESS_INTERVAL == 0 {
                    let elapsed = START.with(|s| {
                        s.borrow()
                            .map(|s| s.elapsed().as_secs_f64())
                            .unwrap_or(0.0)
                    });
                    eprintln!(
                        "[perf] {:.1}s | eval:{} force:{} thunk_f:{} thunk_h:{} import:{}({}) apply:{} select:{} attrset:{} env_c:{} env_l:{}",
                        elapsed,
                        c.eval_expr_calls,
                        c.force_value_calls,
                        c.thunk_forces,
                        c.thunk_cache_hits,
                        c.import_calls,
                        c.import_cache_hits,
                        c.apply_calls,
                        c.select_calls,
                        c.attrset_constructions,
                        c.env_clones,
                        c.env_lookups,
                    );
                }
            }
            "force_value" => c.force_value_calls += 1,
            "thunk_force" => c.thunk_forces += 1,
            "thunk_hit" => c.thunk_cache_hits += 1,
            "import" => c.import_calls += 1,
            "import_hit" => c.import_cache_hits += 1,
            "apply" => c.apply_calls += 1,
            "select" => c.select_calls += 1,
            "attrset" => c.attrset_constructions += 1,
            "env_clone" => c.env_clones += 1,
            "env_lookup" => c.env_lookups += 1,
            _ => {}
        }
    });
}

/// Increment the lookup depth accumulator by `depth`.
#[inline(always)]
pub fn inc_lookup_depth(depth: u64) {
    if !enabled() {
        return;
    }
    COUNTERS.with(|c| {
        c.borrow_mut().env_lookup_depth_total += depth;
    });
}

pub fn report() {
    if !enabled() {
        return;
    }
    COUNTERS.with(|c| {
        let c = c.borrow();
        let elapsed = START.with(|s| {
            s.borrow()
                .map(|s| s.elapsed().as_secs_f64())
                .unwrap_or(0.0)
        });
        let avg_lookup = if c.env_lookups > 0 {
            c.env_lookup_depth_total as f64 / c.env_lookups as f64
        } else {
            0.0
        };
        eprintln!("\n=== sui-eval performance ===");
        eprintln!("elapsed:        {elapsed:.2}s");
        eprintln!("eval_expr:      {}", c.eval_expr_calls);
        eprintln!("force_value:    {}", c.force_value_calls);
        eprintln!("thunk_forces:   {}", c.thunk_forces);
        eprintln!("thunk_hits:     {}", c.thunk_cache_hits);
        eprintln!(
            "imports:        {} ({} cached)",
            c.import_calls, c.import_cache_hits
        );
        eprintln!("apply:          {}", c.apply_calls);
        eprintln!("select:         {}", c.select_calls);
        eprintln!("attrsets:       {}", c.attrset_constructions);
        eprintln!("env_clones:     {}", c.env_clones);
        eprintln!(
            "env_lookups:    {} (avg depth {avg_lookup:.1})",
            c.env_lookups
        );
        eprintln!("===========================\n");
    });
}
