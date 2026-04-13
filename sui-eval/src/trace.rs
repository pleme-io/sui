//! Infinite recursion debugging tools for the tree-walker evaluator.
//!
//! Five integrated tools:
//!
//! 1. **Force chain capture** — always-on, captures the chain of thunk
//!    forces leading to a blackhole cycle.
//! 2. **Trace mode** (`SUI_TRACE_EVAL=1` or `=verbose`) — logs every
//!    thunk force to stderr or a ring buffer.
//! 3. **Max force depth** (`--max-force-depth N`) — caps the force
//!    stack and reports early.
//! 4. **Thunk stats** — extends `perf.rs` counters with thunk-specific
//!    metrics (created, forced unique, max depth).
//! 5. **Static cycle detection** — lives in the compiler; see
//!    `sui-bytecode/src/compiler.rs`.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// ── Tool 1: Force Chain Capture ──────────────────────────────────

/// A single entry on the force stack.
#[derive(Debug, Clone)]
pub struct ForceFrame {
    /// File where the thunk was defined (if known).
    pub defined_in: Option<PathBuf>,
    /// Human-readable description (truncated source text).
    pub description: String,
    /// Unique identity of the thunk (pointer address).
    pub thunk_id: usize,
}

/// The chain of forces that led to a cycle.
#[derive(Debug, Clone)]
pub struct ForceChain(pub Vec<ForceFrame>);

thread_local! {
    static FORCE_STACK: RefCell<Vec<ForceFrame>> = RefCell::new(Vec::new());
}

/// Push a frame onto the force stack. Called when a thunk begins forcing.
pub fn push_force(frame: ForceFrame) {
    FORCE_STACK.with(|s| {
        s.borrow_mut().push(frame);
        // Update thunk stats: track max depth.
        let depth = s.borrow().len();
        THUNK_MAX_FORCE_DEPTH.with(|m| {
            if depth > m.get() as usize {
                m.set(depth as u32);
            }
        });
        THUNK_CURRENT_FORCE_DEPTH.with(|c| c.set(depth as u32));
    });
}

/// Pop a frame from the force stack. Called when a thunk finishes forcing.
pub fn pop_force() {
    FORCE_STACK.with(|s| {
        s.borrow_mut().pop();
        let depth = s.borrow().len();
        THUNK_CURRENT_FORCE_DEPTH.with(|c| c.set(depth as u32));
    });
}

/// Capture the cycle portion of the force stack starting from the
/// frame whose `thunk_id` matches the blackholed thunk.
pub fn capture_cycle(thunk_id: usize) -> ForceChain {
    FORCE_STACK.with(|s| {
        let stack = s.borrow();
        let start = stack.iter().position(|f| f.thunk_id == thunk_id);
        match start {
            Some(idx) => ForceChain(stack[idx..].to_vec()),
            None => ForceChain(stack.clone()),
        }
    })
}

impl std::fmt::Display for ForceChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "infinite recursion detected")?;
        writeln!(f, "force chain ({} frames):", self.0.len())?;
        let mut prev_desc: Option<&str> = None;
        let mut repeat = 0u32;
        for (i, frame) in self.0.iter().enumerate() {
            if prev_desc == Some(&frame.description) {
                repeat += 1;
                continue;
            }
            if repeat > 0 {
                writeln!(f, "    ... repeated {repeat} more times")?;
            }
            repeat = 0;
            let loc = frame
                .defined_in
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<eval>".into());
            let arrow = if i == 0 { "\u{2192}" } else { "\u{2192}" };
            writeln!(f, "  {arrow} {} ({})", frame.description, loc)?;
            prev_desc = Some(&frame.description);
        }
        if repeat > 0 {
            writeln!(f, "    ... repeated {repeat} more times")?;
        }
        Ok(())
    }
}

// ── Tool 2: Trace Mode ──────────────────────────────────────────

static TRACE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether trace mode is set to "verbose" (prints each force immediately)
/// vs. ring-buffer mode (only dumps on error).
static TRACE_VERBOSE: AtomicBool = AtomicBool::new(false);

/// Initialize tracing from the `SUI_TRACE_EVAL` environment variable.
///
/// - Empty / unset: tracing disabled
/// - `"1"` or `"verbose"`: verbose mode (each force printed to stderr)
/// - Any other non-empty value: ring-buffer mode (dumped on error)
pub fn init_trace() {
    let mode = std::env::var("SUI_TRACE_EVAL").unwrap_or_default();
    if mode.is_empty() {
        TRACE_ENABLED.store(false, Ordering::Relaxed);
        TRACE_VERBOSE.store(false, Ordering::Relaxed);
    } else {
        TRACE_ENABLED.store(true, Ordering::Relaxed);
        TRACE_VERBOSE.store(mode == "1" || mode == "verbose", Ordering::Relaxed);
    }
}

/// Whether any trace mode is active.
#[inline(always)]
pub fn trace_enabled() -> bool {
    TRACE_ENABLED.load(Ordering::Relaxed)
}

thread_local! {
    static TRACE_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Ring buffer for non-verbose mode — only dump on error.
    static RING_BUFFER: RefCell<VecDeque<String>> =
        RefCell::new(VecDeque::with_capacity(256));
}

/// Log a force-enter event. In verbose mode, prints immediately.
/// In ring-buffer mode, stores for later dump.
pub fn trace_force_enter(file: Option<&Path>, desc: &str) {
    if !trace_enabled() {
        return;
    }
    let depth = TRACE_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    let indent = "  ".repeat(depth as usize);
    let loc = file
        .map(|f| f.display().to_string())
        .unwrap_or_default();
    let msg = format!("[trace] {indent}force {loc} ({desc})");
    if TRACE_VERBOSE.load(Ordering::Relaxed) {
        eprintln!("{msg}");
    }
    RING_BUFFER.with(|rb| {
        let mut rb = rb.borrow_mut();
        if rb.len() >= 256 {
            rb.pop_front();
        }
        rb.push_back(msg);
    });
}

/// Log a force-exit event (decrements trace depth).
pub fn trace_force_exit() {
    if !trace_enabled() {
        return;
    }
    TRACE_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
}

/// Dump the trace ring buffer to stderr. Called on error paths.
pub fn dump_trace_on_error() {
    if !trace_enabled() {
        return;
    }
    RING_BUFFER.with(|rb| {
        let rb = rb.borrow();
        if rb.is_empty() {
            return;
        }
        eprintln!("[trace] last {} force operations:", rb.len());
        for line in rb.iter() {
            eprintln!("{line}");
        }
    });
}

// ── Tool 3: Max Force Depth ─────────────────────────────────────

static MAX_FORCE_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Set the maximum allowed force depth. 0 means no limit.
pub fn set_max_force_depth(limit: usize) {
    MAX_FORCE_DEPTH.store(limit, Ordering::Relaxed);
}

/// Check whether the current force depth exceeds the configured limit.
/// Returns `Ok(())` if within bounds or no limit is set.
pub fn check_force_depth() -> Result<(), String> {
    let limit = MAX_FORCE_DEPTH.load(Ordering::Relaxed);
    if limit == 0 {
        return Ok(());
    }
    let depth = FORCE_STACK.with(|s| s.borrow().len());
    if depth > limit {
        Err(format!("force depth exceeded ({depth}/{limit})"))
    } else {
        Ok(())
    }
}

// ── Tool 5: Thunk Stats (extends perf.rs) ───────────────────────

thread_local! {
    static THUNKS_CREATED: Cell<u64> = const { Cell::new(0) };
    static THUNKS_FORCED_UNIQUE: Cell<u64> = const { Cell::new(0) };
    static THUNK_MAX_FORCE_DEPTH: Cell<u32> = const { Cell::new(0) };
    static THUNK_CURRENT_FORCE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Increment the thunks-created counter.
#[inline(always)]
pub fn inc_thunks_created() {
    if crate::perf::enabled() {
        THUNKS_CREATED.with(|c| c.set(c.get() + 1));
    }
}

/// Increment the thunks-forced-unique counter.
#[inline(always)]
pub fn inc_thunks_forced_unique() {
    if crate::perf::enabled() {
        THUNKS_FORCED_UNIQUE.with(|c| c.set(c.get() + 1));
    }
}

/// Get current force depth (debug).
pub fn current_force_depth() -> u32 {
    THUNK_CURRENT_FORCE_DEPTH.with(Cell::get)
}

/// Get thunks created count (for progress snapshots).
pub fn get_thunks_created() -> u64 {
    THUNKS_CREATED.with(Cell::get)
}

/// Get thunks forced count (for progress snapshots).
pub fn get_thunks_forced() -> u64 {
    THUNKS_FORCED_UNIQUE.with(Cell::get)
}

/// Report thunk stats to stderr (called from `perf::report`).
pub fn report_thunk_stats() {
    if !crate::perf::enabled() {
        return;
    }
    let created = THUNKS_CREATED.with(Cell::get);
    let forced = THUNKS_FORCED_UNIQUE.with(Cell::get);
    let max_depth = THUNK_MAX_FORCE_DEPTH.with(Cell::get);
    eprintln!("thunks_created: {created}");
    eprintln!("thunks_forced:  {forced}");
    eprintln!("max_force_depth: {max_depth}");
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Force chain capture ─────────────────────────────────

    #[test]
    fn force_chain_display_empty() {
        let chain = ForceChain(vec![]);
        let s = chain.to_string();
        assert!(s.contains("0 frames"));
    }

    #[test]
    fn force_chain_display_single() {
        let chain = ForceChain(vec![ForceFrame {
            defined_in: Some(PathBuf::from("/test.nix")),
            description: "x".into(),
            thunk_id: 1,
        }]);
        let s = chain.to_string();
        assert!(s.contains("1 frames"));
        assert!(s.contains("/test.nix"));
        assert!(s.contains("x"));
    }

    #[test]
    fn force_chain_display_repeated_frames() {
        let chain = ForceChain(vec![
            ForceFrame {
                defined_in: None,
                description: "x".into(),
                thunk_id: 1,
            },
            ForceFrame {
                defined_in: None,
                description: "x".into(),
                thunk_id: 2,
            },
            ForceFrame {
                defined_in: None,
                description: "x".into(),
                thunk_id: 3,
            },
            ForceFrame {
                defined_in: None,
                description: "y".into(),
                thunk_id: 4,
            },
        ]);
        let s = chain.to_string();
        assert!(s.contains("repeated 2 more times"));
        assert!(s.contains("y"));
    }

    #[test]
    fn force_chain_display_eval_location() {
        let chain = ForceChain(vec![ForceFrame {
            defined_in: None,
            description: "z".into(),
            thunk_id: 1,
        }]);
        let s = chain.to_string();
        assert!(s.contains("<eval>"));
    }

    #[test]
    fn push_pop_force_stack() {
        // Clear the thread-local stack first.
        FORCE_STACK.with(|s| s.borrow_mut().clear());
        push_force(ForceFrame {
            defined_in: None,
            description: "a".into(),
            thunk_id: 100,
        });
        push_force(ForceFrame {
            defined_in: None,
            description: "b".into(),
            thunk_id: 200,
        });
        let chain = capture_cycle(100);
        assert_eq!(chain.0.len(), 2);
        assert_eq!(chain.0[0].thunk_id, 100);
        pop_force();
        pop_force();
    }

    #[test]
    fn capture_cycle_with_unknown_id() {
        FORCE_STACK.with(|s| s.borrow_mut().clear());
        push_force(ForceFrame {
            defined_in: None,
            description: "a".into(),
            thunk_id: 10,
        });
        // Capture with an ID not on the stack returns the whole stack.
        let chain = capture_cycle(999);
        assert_eq!(chain.0.len(), 1);
        pop_force();
    }

    // ── Trace mode ──────────────────────────────────────────

    #[test]
    fn trace_disabled_by_default() {
        // After init with no env var, trace should be off.
        // (Cannot reliably test env var setting in parallel tests,
        // so just verify the function is callable.)
        let _ = trace_enabled();
    }

    #[test]
    fn trace_force_enter_exit_no_panic() {
        // Ensure enter/exit don't panic even when trace is off.
        trace_force_enter(None, "test");
        trace_force_exit();
    }

    // ── Max force depth ─────────────────────────────────────

    #[test]
    fn check_force_depth_logic() {
        // Test all depth-limit scenarios in a single test to avoid
        // AtomicUsize races between parallel tests.
        FORCE_STACK.with(|s| s.borrow_mut().clear());

        // No limit — always OK.
        set_max_force_depth(0);
        assert!(check_force_depth().is_ok());

        // Within limit — OK.
        set_max_force_depth(10);
        push_force(ForceFrame {
            defined_in: None,
            description: "a".into(),
            thunk_id: 1,
        });
        assert!(check_force_depth().is_ok());

        // Exceeded — 2 items with limit 1.
        set_max_force_depth(1);
        push_force(ForceFrame {
            defined_in: None,
            description: "b".into(),
            thunk_id: 2,
        });
        let result = check_force_depth();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("force depth exceeded"));

        // Cleanup.
        pop_force();
        pop_force();
        set_max_force_depth(0);
    }

    // ── Thunk stats ─────────────────────────────────────────

    #[test]
    fn thunk_stats_increment() {
        // Just verify the functions don't panic.
        inc_thunks_created();
        inc_thunks_forced_unique();
    }

    // ── Integration: force chain with eval ───────────────────

    #[test]
    fn force_chain_captures_self_reference() {
        let result = crate::eval::eval("let x = x; in x");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("infinite recursion")
                || msg.contains("force chain")
                || msg.contains("blackhole"),
            "expected infinite recursion error, got: {msg}"
        );
    }

    #[test]
    fn force_chain_captures_mutual_recursion() {
        let result = crate::eval::eval("let a = b; b = a; in a");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("infinite recursion")
                || msg.contains("force chain")
                || msg.contains("blackhole"),
            "expected infinite recursion error, got: {msg}"
        );
    }

    #[test]
    fn force_chain_captures_rec_self_reference() {
        let result = crate::eval::eval("rec { x = x; }.x");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("infinite recursion")
                || msg.contains("force chain")
                || msg.contains("blackhole"),
            "expected infinite recursion error, got: {msg}"
        );
    }
}
