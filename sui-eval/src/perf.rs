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

/// Check the env var and, if set, enable counters. Called on every
/// top-level `eval()`. Deliberately one-way: if `SUI_EVAL_PERF` is
/// NOT set we leave the flag as-is rather than clearing it, so that
/// a prior `set_enabled(true)` (from a profiling tool or integration
/// test) survives the next `eval()` call.
pub fn init() {
    if std::env::var("SUI_EVAL_PERF").ok().as_deref() == Some("1") {
        ENABLED.store(true, Ordering::Relaxed);
    }
}

/// Enable or disable perf counters programmatically. Use this from
/// profiling tools, integration tests, or the `sui perf` subcommand
/// that wants counters even when `SUI_EVAL_PERF=1` isn't set in the
/// environment. Production code paths don't call this.
pub fn set_enabled(on: bool) {
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
    // Overlay (`//`) flatten instrumentation. `OverlayFlattenAttempt` counts
    // every `as_flat()` call on an Overlay node; `OverlayFlattenBuild` counts
    // the subset that actually ran the merge closure (a cache MISS — real
    // O(n) work). `OverlayFlattenEntries` accumulates total (left+right)
    // entries merged across all builds — the raw work volume. The redundancy
    // signal is builds / attempts: a high ratio means the same logical
    // overlay content is re-flattened because each fixpoint iteration mints a
    // fresh node with a cold cache.
    OverlayFlattenAttempt = 30,
    OverlayFlattenBuild = 31,
    OverlayFlattenEntries = 32,
    OverlayCreated = 33,
    // `sorted_entries()` — the sorted-name/value path used by `attrNames`,
    // `attrValues`, `keys`, `iter`. Each call resolves every Symbol → String
    // and string-sorts. `SortedEntriesCalls` counts invocations;
    // `SortedEntriesRows` accumulates entries sorted.
    SortedEntriesCalls = 34,
    SortedEntriesRows = 35,
    // List concatenation (`++` / `concatLists` / `concatMap`). `ListConcatCalls`
    // counts concat operations; `ListConcatElemsCopied` accumulates the total
    // number of list ELEMENTS physically copied (cloned) into a fresh Vec —
    // the O(n) storm signal. `ListConcatElemsReused` accumulates the elements
    // that were appended in place (Rc uniquely owned) instead of copied — the
    // work the structural-share fix elides. A left-associative `++` fold over a
    // growing accumulator makes elems_copied grow quadratically without the fix.
    ListConcatCalls = 36,
    ListConcatElemsCopied = 37,
    ListConcatElemsReused = 38,
    // Attrset structural equality (`Concrete::eq` Attrs arm, the non-ptr-eq /
    // non-derivation fallback that runs a full structural compare).
    // `AttrsEqStructuralCalls` counts how often that fallback is reached — each
    // is one site where the old `a.inner() == b.inner()` cloned BOTH backing
    // FxHashMaps purely to feed `HashMap::eq`. `AttrsEqEntriesCloneElided`
    // accumulates the combined entry count of both operands (the total map
    // entries no longer cloned by the borrow-compare fix). Purely a
    // measurement of eliminated clone/allocation work — the compare itself is
    // byte-identical (PERF-ARSENAL C-A).
    AttrsEqStructuralCalls = 39,
    AttrsEqEntriesCloneElided = 40,
    // M2 RISKY-tier waste measurement (C-with / C-slash / C-store).
    // `WithScopeCacheClone` counts every `(**attrs).clone()` a with-scope
    // lookup does to populate its `Rc<RefCell<Option<NixAttrs>>>` cache (C-with)
    // — an `im_rc` HAMT root clone (O(1) structural share), NOT an O(n) deep
    // copy. `SlashDeferredTailClone` counts the `(**la).clone()` in
    // `lazy_overlay_merge` (C-slash) — same O(1) HAMT clone, then COW-mutated.
    // `ThunkStoreWrites` counts the double `repr = Evaluated` + `cache.set`
    // stores per successful force (C-store). Pure measurement — confirms
    // whether the "waste" the arsenal names is real O(n) or already-O(1).
    WithScopeCacheClone = 41,
    SlashDeferredTailClone = 42,
    ThunkStoreWrites = 43,
    // C-store sub-probe: counts forces where the post-Store#1 thunk-chain
    // unwrap loop actually MUTATED `value` (so Store#2's content ≠ Store#1's).
    // When this is 0, Store#1 and Store#2 write byte-identical content and a
    // collapse to a single store is trivially content-neutral; when > 0, the
    // two stores hold different values and collapsing would change what an
    // interleaved observer sees.
    ThunkStoreLoopMutated = 44,
    // C-store sub-probe #2: forces where `value` was NOT a thunk at Store#1,
    // so the unwrap loop never ran and Store#2 is a pure redundant rewrite of
    // byte-identical content. Skipping Store#2 here is provably
    // content-and-order-neutral (Store#1 already established the final state).
    ThunkStoreRedundant = 45,
}

const NUM_COUNTERS: usize = 46;

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
    "overlay_flatten_attempt",
    "overlay_flatten_build",
    "overlay_flatten_entries",
    "overlay_created",
    "sorted_entries_calls",
    "sorted_entries_rows",
    "list_concat_calls",
    "list_concat_elems_copied",
    "list_concat_elems_reused",
    "attrs_eq_structural_calls",
    "attrs_eq_entries_clone_elided",
    "with_scope_cache_clone",
    "slash_deferred_tail_clone",
    "thunk_store_writes",
    "thunk_store_loop_mutated",
    "thunk_store_redundant",
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
        // Overlay (`//`) flatten stats — the re-flatten storm signal.
        let ov_created = c.get(Counter::OverlayCreated);
        let ov_attempt = c.get(Counter::OverlayFlattenAttempt);
        let ov_build = c.get(Counter::OverlayFlattenBuild);
        let ov_entries = c.get(Counter::OverlayFlattenEntries);
        if ov_created > 0 || ov_attempt > 0 {
            let hit = ov_attempt.saturating_sub(ov_build);
            let hit_rate = if ov_attempt > 0 {
                (hit as f64 / ov_attempt as f64) * 100.0
            } else {
                0.0
            };
            eprintln!("--- overlay (`//`) flatten ---");
            eprintln!("  overlays_created:  {ov_created}");
            eprintln!("  flatten_attempts:  {ov_attempt}");
            eprintln!("  flatten_builds:    {ov_build}  (cache-miss = real O(n) merge)");
            eprintln!("  cache_hit_rate:    {hit_rate:.1}%");
            eprintln!("  entries_merged:    {ov_entries}  (sum of left+right over all builds)");
            if ov_created > 0 {
                let builds_per_overlay = ov_build as f64 / ov_created as f64;
                eprintln!("  builds_per_overlay:{builds_per_overlay:.2}");
            }
            let flatten_ms = crate::trace::get_overlay_flatten_nanos() as f64 / 1_000_000.0;
            let pct = if elapsed > 0.0 {
                (flatten_ms / 1000.0 / elapsed) * 100.0
            } else {
                0.0
            };
            eprintln!("  flatten_walltime:  {flatten_ms:.1}ms  ({pct:.1}% of eval, incl. nested)");
        }
        let se_calls = c.get(Counter::SortedEntriesCalls);
        let se_rows = c.get(Counter::SortedEntriesRows);
        if se_calls > 0 {
            let se_ms = crate::trace::get_sorted_entries_nanos() as f64 / 1_000_000.0;
            let se_pct = if elapsed > 0.0 {
                (se_ms / 1000.0 / elapsed) * 100.0
            } else {
                0.0
            };
            eprintln!("--- sorted_entries (attrNames/iter) ---");
            eprintln!("  calls:             {se_calls}");
            eprintln!("  rows_sorted:       {se_rows}");
            eprintln!("  walltime:          {se_ms:.1}ms  ({se_pct:.1}% of eval)");
        }
        // List concatenation (`++` / concatLists / concatMap) — the O(n) copy
        // storm. `elems_copied` is the raw work volume; `elems_reused` is what
        // the in-place structural-share fix elides. A copied/reused ratio near
        // 0 means most concats hit the shared fast path.
        let lc_calls = c.get(Counter::ListConcatCalls);
        let lc_copied = c.get(Counter::ListConcatElemsCopied);
        let lc_reused = c.get(Counter::ListConcatElemsReused);
        if lc_calls > 0 {
            let total = lc_copied + lc_reused;
            let reuse_pct = if total > 0 {
                (lc_reused as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            eprintln!("--- list concat (`++` / concatLists) ---");
            eprintln!("  calls:             {lc_calls}");
            eprintln!("  elems_copied:      {lc_copied}");
            eprintln!("  elems_reused:      {lc_reused}  (in-place, Rc uniquely owned)");
            eprintln!("  reuse_rate:        {reuse_pct:.1}%");
        }
        // Attrset structural `==` — the clones the borrow-compare fix elides.
        let eq_calls = c.get(Counter::AttrsEqStructuralCalls);
        let eq_elided = c.get(Counter::AttrsEqEntriesCloneElided);
        if eq_calls > 0 {
            eprintln!("--- attrs structural eq (`==` fallback) ---");
            eprintln!("  structural_calls:  {eq_calls}");
            eprintln!(
                "  map_clones_elided: {}  ({eq_elided} entries not cloned — was 2 FxHashMap clones/call)",
                eq_calls * 2
            );
        }
        // M2 RISKY-tier waste measurement (C-with / C-slash / C-store).
        // Each `im_rc` HAMT clone is O(1) structural sharing, so these counts
        // are the NUMBER of O(1) clones, not an O(n) copy volume.
        let wc = c.get(Counter::WithScopeCacheClone);
        let sc = c.get(Counter::SlashDeferredTailClone);
        let ts = c.get(Counter::ThunkStoreWrites);
        if wc + sc + ts > 0 {
            eprintln!("--- M2 RISKY-tier waste probes ---");
            eprintln!("  with_scope_cache_clone:   {wc}  (C-with; O(1) HAMT clone each)");
            eprintln!("  slash_deferred_tail_clone:{sc}  (C-slash; O(1) HAMT clone, then COW-merged)");
            eprintln!("  thunk_store_writes:       {ts}  (C-store; repr+cache double-store per force)");
            let tm = c.get(Counter::ThunkStoreLoopMutated);
            eprintln!("  thunk_store_loop_mutated: {tm}  (C-store; Store#2 content ≠ Store#1 — collapse NOT content-neutral if >0)");
            let tr = c.get(Counter::ThunkStoreRedundant);
            eprintln!("  thunk_store_redundant:    {tr}  (C-store; Store#2 = pure redundant rewrite — provably skippable)");
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

// ──────────────────────────────────────────────────────────────────
// Programmatic snapshot API — the foundation for perf-analysis tools.
//
// `report()` dumps to stderr; great for humans, useless for tools that
// want structured data, want to sort, diff, or capture counter deltas
// per-eval. `PerfSnapshot` gives you a plain struct you can move
// around, serialize, subtract, and inspect.
// ──────────────────────────────────────────────────────────────────

/// An immutable snapshot of counter + timer state at a single point.
///
/// Cheap to clone (just a fixed-size array + a few u64). Subtract two
/// snapshots (`b.delta_from(&a)`) to get the work done between them —
/// the foundation of `with_scope` and per-program profiling.
#[derive(Clone, Debug)]
pub struct PerfSnapshot {
    /// Wall-clock elapsed since [`start`] was called. `None` if `start`
    /// wasn't called in this thread.
    pub elapsed: Option<std::time::Duration>,
    /// Raw counter values, indexed by `Counter as usize`.
    pub counters: [u64; NUM_COUNTERS],
    /// Thunks allocated in this thread to date (from `trace` module).
    pub thunks_created: u64,
    /// Thunks forced (from suspended → evaluated) to date.
    pub thunks_forced: u64,
}

impl PerfSnapshot {
    /// All-zero snapshot. Useful as the identity element for folds.
    #[must_use]
    pub fn zero() -> Self {
        Self {
            elapsed: None,
            counters: [0; NUM_COUNTERS],
            thunks_created: 0,
            thunks_forced: 0,
        }
    }

    /// Read a counter by enum variant.
    #[must_use]
    pub fn get(&self, counter: Counter) -> u64 {
        self.counters[counter as usize]
    }

    /// `self - other`, treating both as counter totals. Used to get
    /// the delta across a scoped evaluation (snapshot before, snapshot
    /// after, subtract). Saturates to zero on underflow — shouldn't
    /// happen in normal use but guards against races / manual resets.
    #[must_use]
    pub fn delta_from(&self, other: &PerfSnapshot) -> PerfSnapshot {
        let mut counters = [0u64; NUM_COUNTERS];
        for i in 0..NUM_COUNTERS {
            counters[i] = self.counters[i].saturating_sub(other.counters[i]);
        }
        let elapsed = match (self.elapsed, other.elapsed) {
            (Some(a), Some(b)) => Some(a.saturating_sub(b)),
            _ => self.elapsed,
        };
        PerfSnapshot {
            elapsed,
            counters,
            thunks_created: self.thunks_created.saturating_sub(other.thunks_created),
            thunks_forced: self.thunks_forced.saturating_sub(other.thunks_forced),
        }
    }

    /// Thunk memoization hit rate — 1.0 means every suspended thunk
    /// produced useful work; 0.0 means everything we built was wasted.
    /// Returns `None` when no thunks were created in this window.
    #[must_use]
    pub fn thunk_hit_rate(&self) -> Option<f64> {
        let created = self.thunks_created;
        if created == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let r = self.thunks_forced as f64 / created as f64;
        Some(r)
    }

    /// `(top_variant, count)` — the expression kind with the highest
    /// count in this snapshot. For ranking "what is this eval mostly
    /// doing?" at a glance.
    #[must_use]
    pub fn dominant_expr_kind(&self) -> Option<(Counter, u64)> {
        let kinds = [
            Counter::ExprIdent,
            Counter::ExprLiteral,
            Counter::ExprStr,
            Counter::ExprList,
            Counter::ExprAttrs,
            Counter::ExprSelect,
            Counter::ExprApply,
            Counter::ExprLetIn,
            Counter::ExprIfElse,
            Counter::ExprWith,
            Counter::ExprLambda,
            Counter::ExprBinOp,
            Counter::ExprHasAttr,
            Counter::ExprUnaryOp,
            Counter::ExprAssert,
            Counter::ExprPath,
            Counter::ExprOther,
        ];
        kinds
            .iter()
            .map(|&k| (k, self.get(k)))
            .filter(|&(_, n)| n > 0)
            .max_by_key(|&(_, n)| n)
    }
}

/// Capture the current counter state as a [`PerfSnapshot`].
///
/// Does NOT reset anything — the next `snapshot()` / `inc()` / `add()`
/// call sees the same state. Use [`reset`] if you want a fresh window.
#[must_use]
pub fn snapshot() -> PerfSnapshot {
    let mut out = [0u64; NUM_COUNTERS];
    COUNTERS.with(|c| {
        let c = c.borrow();
        out.copy_from_slice(&c.counts);
    });
    let elapsed = START.with(|s| s.borrow().map(|s| s.elapsed()));
    PerfSnapshot {
        elapsed,
        counters: out,
        thunks_created: crate::trace::get_thunks_created(),
        thunks_forced: crate::trace::get_thunks_forced(),
    }
}

/// Zero every counter and restart the timer. The thread-local thunk
/// trace counters are reset too.
pub fn reset() {
    COUNTERS.with(|c| {
        let mut c = c.borrow_mut();
        *c = PerfCounters::default();
    });
    START.with(|s| *s.borrow_mut() = Some(Instant::now()));
    crate::trace::reset_thunk_stats();
}

/// Scoped profiling: enable counters if they aren't already, reset
/// them, run `f`, and return `(result, delta_snapshot)`.
///
/// The counters are left enabled after this returns — callers that
/// want strict zero-overhead production eval should `set_enabled(false)`
/// afterwards.
pub fn with_scope<F, R>(f: F) -> (R, PerfSnapshot)
where
    F: FnOnce() -> R,
{
    let prev_enabled = enabled();
    set_enabled(true);
    reset();
    let before = snapshot();
    let result = f();
    let after = snapshot();
    let delta = after.delta_from(&before);
    set_enabled(prev_enabled);
    (result, delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_enum_has_30_variants() {
        // Each variant maps to a fixed index; NUM_COUNTERS is the array size.
        // Extended by the M2 RISKY-tier waste probes (41..=45): C-with /
        // C-slash / C-store measurement + the C-store redundant-store skip.
        assert_eq!(NUM_COUNTERS, 46);
        assert_eq!(COUNTER_NAMES.len(), NUM_COUNTERS);
        assert_eq!(Counter::OverlayFlattenAttempt as usize, 30);
        assert_eq!(Counter::OverlayCreated as usize, 33);
        assert_eq!(Counter::SortedEntriesCalls as usize, 34);
        assert_eq!(Counter::SortedEntriesRows as usize, 35);
        assert_eq!(Counter::ListConcatCalls as usize, 36);
        assert_eq!(Counter::ListConcatElemsCopied as usize, 37);
        assert_eq!(Counter::ListConcatElemsReused as usize, 38);
        assert_eq!(Counter::AttrsEqStructuralCalls as usize, 39);
        assert_eq!(Counter::AttrsEqEntriesCloneElided as usize, 40);
        assert_eq!(Counter::WithScopeCacheClone as usize, 41);
        assert_eq!(Counter::SlashDeferredTailClone as usize, 42);
        assert_eq!(Counter::ThunkStoreWrites as usize, 43);
        assert_eq!(Counter::ThunkStoreLoopMutated as usize, 44);
        assert_eq!(Counter::ThunkStoreRedundant as usize, 45);
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
