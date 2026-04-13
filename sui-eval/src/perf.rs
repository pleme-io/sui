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
    // Expression type breakdown
    ExprIdent = 12,
    ExprLiteral = 13,
    ExprStr = 14,
    ExprList = 15,
    ExprAttrs = 16,
    ExprSelect = 17,
    ExprApply = 18,
    ExprLetIn = 19,
    ExprIfElse = 20,
    ExprWith = 21,
    ExprLambda = 22,
    ExprOther = 23,
    // Dead binding elimination
    DeadBindingsSkipped = 24,
    // Finer "other" breakdown
    ExprBinOp = 25,
    ExprHasAttr = 26,
    ExprUnaryOp = 27,
    ExprAssert = 28,
    ExprPath = 29,
}

const NUM_COUNTERS: usize = 30;

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
    "expr_ident",
    "expr_literal",
    "expr_str",
    "expr_list",
    "expr_attrs",
    "expr_select",
    "expr_apply",
    "expr_letin",
    "expr_ifelse",
    "expr_with",
    "expr_lambda",
    "expr_other",
    "dead_bindings_skipped",
    "expr_binop",
    "expr_hasattr",
    "expr_unaryop",
    "expr_assert",
    "expr_path",
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
            // Expression type breakdown
            eprintln!(
                "  [id:{} ap:{} if:{} let:{} sel:{} at:{} w:{} lam:{} lit:{} str:{} list:{} ot:{}]",
                c.get(Counter::ExprIdent),
                c.get(Counter::ExprApply),
                c.get(Counter::ExprIfElse),
                c.get(Counter::ExprLetIn),
                c.get(Counter::ExprSelect),
                c.get(Counter::ExprAttrs),
                c.get(Counter::ExprWith),
                c.get(Counter::ExprLambda),
                c.get(Counter::ExprLiteral),
                c.get(Counter::ExprStr),
                c.get(Counter::ExprList),
                c.get(Counter::ExprOther),
            );
            // Finer "other" breakdown
            let binop = c.get(Counter::ExprBinOp);
            let hasattr = c.get(Counter::ExprHasAttr);
            let unary = c.get(Counter::ExprUnaryOp);
            let assert = c.get(Counter::ExprAssert);
            let path = c.get(Counter::ExprPath);
            if binop + hasattr + unary + assert + path > 0 {
                eprintln!(
                    "  [binop:{binop} hasattr:{hasattr} unary:{unary} assert:{assert} path:{path}]",
                );
            }
            // Dead binding elimination stats
            let dead = c.get(Counter::DeadBindingsSkipped);
            // Thunk creation stats
            let created = crate::trace::get_thunks_created();
            let forced = crate::trace::get_thunks_forced();
            if created > 0 {
                let waste = (1.0 - forced as f64 / created as f64) * 100.0;
                eprintln!("  [thunks created:{created} forced:{forced} waste:{waste:.0}% dead_skipped:{dead}]");
            }
            // Force-site breakdown
            crate::eval::dump_force_sites();
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
        // Expression type breakdown
        let total = c.get(Counter::EvalExpr);
        if total > 0 {
            eprintln!("--- expression breakdown ---");
            for (counter, name) in [
                (Counter::ExprIdent, "ident"),
                (Counter::ExprApply, "apply"),
                (Counter::ExprLetIn, "let-in"),
                (Counter::ExprIfElse, "if-else"),
                (Counter::ExprSelect, "select"),
                (Counter::ExprAttrs, "attrset"),
                (Counter::ExprWith, "with"),
                (Counter::ExprLambda, "lambda"),
                (Counter::ExprLiteral, "literal"),
                (Counter::ExprStr, "string"),
                (Counter::ExprList, "list"),
                (Counter::ExprBinOp, "binop"),
                (Counter::ExprHasAttr, "hasattr"),
                (Counter::ExprUnaryOp, "unaryop"),
                (Counter::ExprAssert, "assert"),
                (Counter::ExprPath, "path"),
                (Counter::ExprOther, "other"),
            ] {
                let n = c.get(counter);
                if n > 0 {
                    let pct = (n as f64 / total as f64) * 100.0;
                    eprintln!("  {name:<12} {n:>12} ({pct:.1}%)");
                }
            }
        }
        // Dead binding elimination
        let dead = c.get(Counter::DeadBindingsSkipped);
        if dead > 0 {
            eprintln!("dead_skipped:   {dead}");
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_enum_has_30_variants() {
        // Each variant maps to an index 0..29, and NUM_COUNTERS == 30.
        assert_eq!(NUM_COUNTERS, 30);
        assert_eq!(Counter::EvalExpr as usize, 0);
        assert_eq!(Counter::ForceValue as usize, 1);
        assert_eq!(Counter::ThunkForce as usize, 2);
        assert_eq!(Counter::ThunkHit as usize, 3);
        assert_eq!(Counter::Import as usize, 4);
        assert_eq!(Counter::ImportHit as usize, 5);
        assert_eq!(Counter::Apply as usize, 6);
        assert_eq!(Counter::Select as usize, 7);
        assert_eq!(Counter::Attrset as usize, 8);
        assert_eq!(Counter::EnvClone as usize, 9);
        assert_eq!(Counter::EnvLookup as usize, 10);
        assert_eq!(Counter::EnvLookupDepth as usize, 11);
        assert_eq!(Counter::DeadBindingsSkipped as usize, 24);
        assert_eq!(Counter::ExprBinOp as usize, 25);
        assert_eq!(Counter::ExprPath as usize, 29);
    }

    #[test]
    fn inc_does_not_panic_when_disabled() {
        // Ensure ENABLED is false (default for tests).
        ENABLED.store(false, Ordering::Relaxed);
        // Should be a no-op, not panic.
        inc(Counter::EvalExpr);
        inc(Counter::ForceValue);
        inc(Counter::ThunkForce);
    }

    #[test]
    fn counter_variant_maps_to_correct_index() {
        assert_eq!(counter_name(Counter::EvalExpr), "eval_expr");
        assert_eq!(counter_name(Counter::ForceValue), "force_value");
        assert_eq!(counter_name(Counter::ThunkForce), "thunk_forces");
        assert_eq!(counter_name(Counter::ThunkHit), "thunk_hits");
        assert_eq!(counter_name(Counter::Import), "imports");
        assert_eq!(counter_name(Counter::ImportHit), "import_hits");
        assert_eq!(counter_name(Counter::Apply), "apply");
        assert_eq!(counter_name(Counter::Select), "select");
        assert_eq!(counter_name(Counter::Attrset), "attrsets");
        assert_eq!(counter_name(Counter::EnvClone), "env_clones");
        assert_eq!(counter_name(Counter::EnvLookup), "env_lookups");
        assert_eq!(counter_name(Counter::EnvLookupDepth), "env_lookup_depth");
    }

    #[test]
    fn add_increments_by_given_amount() {
        let mut counters = PerfCounters::default();
        assert_eq!(counters.get(Counter::EvalExpr), 0);
        counters.add(Counter::EvalExpr, 5);
        assert_eq!(counters.get(Counter::EvalExpr), 5);
        counters.add(Counter::EvalExpr, 3);
        assert_eq!(counters.get(Counter::EvalExpr), 8);
    }
}
