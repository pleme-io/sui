//! Lightweight evaluation profiling counters.
//! Enabled via `SUI_EVAL_PERF=1` environment variable.
//!
//! Uses enum-indexed array dispatch instead of string matching —
//! a single `counts[variant as usize] += 1` per call, zero string
//! comparisons.

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

/// Performance counter identifiers.
///
/// Each variant maps to a fixed array index via `as usize`.
/// This avoids string matching in the hot path.
#[repr(u8)]
#[derive(Clone, Copy)]
pub enum Counter {
    EvalExpr = 0,
    ForceValue = 1,
    ThunkForce = 2,
    ThunkHit = 3,
    Import = 4,
    ImportHit = 5,
    Apply = 6,
    Select = 7,
    Attrset = 8,
    EnvClone = 9,
    EnvLookup = 10,
    EnvLookupDepth = 11,
}

const NUM_COUNTERS: usize = 12;

/// Display names for each counter, indexed by `Counter as usize`.
const COUNTER_NAMES: [&str; NUM_COUNTERS] = [
    "eval_expr",
    "force_value",
    "thunk_forces",
    "thunk_hits",
    "imports",
    "import_hits",
    "apply",
    "select",
    "attrsets",
    "env_clones",
    "env_lookups",
    "env_lookup_depth",
];

struct PerfCounters {
    counts: [u64; NUM_COUNTERS],
}

impl Default for PerfCounters {
    fn default() -> Self {
        Self {
            counts: [0; NUM_COUNTERS],
        }
    }
}

impl PerfCounters {
    #[inline(always)]
    fn inc(&mut self, counter: Counter) {
        self.counts[counter as usize] += 1;
    }

    #[inline(always)]
    fn add(&mut self, counter: Counter, n: u64) {
        self.counts[counter as usize] += n;
    }

    #[inline(always)]
    fn get(&self, counter: Counter) -> u64 {
        self.counts[counter as usize]
    }
}

thread_local! {
    static COUNTERS: RefCell<PerfCounters> = RefCell::new(PerfCounters::default());
    static START: RefCell<Option<Instant>> = RefCell::new(None);
}

pub fn start() {
    if enabled() {
        START.with(|s| *s.borrow_mut() = Some(Instant::now()));
    }
}

/// How often to print a progress snapshot (every N eval_expr calls).
const PROGRESS_INTERVAL: u64 = 1_000_000;

#[inline(always)]
pub fn inc(counter: Counter) {
    if !enabled() {
        return;
    }
    COUNTERS.with(|c| {
        let mut c = c.borrow_mut();
        c.inc(counter);
        // Progress reporting for EvalExpr
        if matches!(counter, Counter::EvalExpr)
            && c.get(Counter::EvalExpr) % PROGRESS_INTERVAL == 0
        {
            let elapsed = START.with(|s| {
                s.borrow()
                    .map(|s| s.elapsed().as_secs_f64())
                    .unwrap_or(0.0)
            });
            eprintln!(
                "[perf] {:.1}s | eval:{} force:{} thunk_f:{} thunk_h:{} import:{}({}) apply:{} select:{} attrset:{} env_c:{} env_l:{}",
                elapsed,
                c.get(Counter::EvalExpr),
                c.get(Counter::ForceValue),
                c.get(Counter::ThunkForce),
                c.get(Counter::ThunkHit),
                c.get(Counter::Import),
                c.get(Counter::ImportHit),
                c.get(Counter::Apply),
                c.get(Counter::Select),
                c.get(Counter::Attrset),
                c.get(Counter::EnvClone),
                c.get(Counter::EnvLookup),
            );
        }
    });
}

/// Increment the lookup depth accumulator by `depth`.
#[inline(always)]
pub fn add(counter: Counter, n: u64) {
    if !enabled() {
        return;
    }
    COUNTERS.with(|c| {
        c.borrow_mut().add(counter, n);
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
        let lookups = c.get(Counter::EnvLookup);
        let depth_total = c.get(Counter::EnvLookupDepth);
        let avg_lookup = if lookups > 0 {
            depth_total as f64 / lookups as f64
        } else {
            0.0
        };
        eprintln!("\n=== sui-eval performance ===");
        eprintln!("elapsed:        {elapsed:.2}s");
        eprintln!("eval_expr:      {}", c.get(Counter::EvalExpr));
        eprintln!("force_value:    {}", c.get(Counter::ForceValue));
        eprintln!("thunk_forces:   {}", c.get(Counter::ThunkForce));
        eprintln!("thunk_hits:     {}", c.get(Counter::ThunkHit));
        eprintln!(
            "imports:        {} ({} cached)",
            c.get(Counter::Import),
            c.get(Counter::ImportHit)
        );
        eprintln!("apply:          {}", c.get(Counter::Apply));
        eprintln!("select:         {}", c.get(Counter::Select));
        eprintln!("attrsets:       {}", c.get(Counter::Attrset));
        eprintln!("env_clones:     {}", c.get(Counter::EnvClone));
        eprintln!(
            "env_lookups:    {} (avg depth {avg_lookup:.1})",
            lookups
        );
        // Thunk stats from trace module.
        crate::trace::report_thunk_stats();
        eprintln!("===========================\n");
    });
}

/// Get the display name for a counter.
#[allow(dead_code)]
pub fn counter_name(counter: Counter) -> &'static str {
    COUNTER_NAMES[counter as usize]
}
