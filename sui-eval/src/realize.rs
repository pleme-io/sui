//! Import-from-derivation (IFD): realize a derivation's output mid-eval.
//!
//! When eval coerces a **derivation** to a path for a filesystem read
//! (`import`, `readFile`, `readDir`, `pathExists`, `builtins.path`) and the
//! derivation's `outPath` is **not yet materialized on disk**, cppnix realizes
//! it — building or substituting the derivation during evaluation — so the read
//! can proceed. This is *import-from-derivation*; sui must do the same to
//! compute drvPaths whose graph reads a built output (e.g. the darwin toplevel
//! importing `ishou.stylix-fonts`, itself a `runCommand` derivation).
//!
//! ## Why a hook, not an inline builder
//!
//! `sui-eval` is a **pure, synchronous** library crate: it owns no store
//! handle, no tokio runtime, and no build sandbox. The realize pipeline
//! (`Substitutor` + `LocalBuilder` over an `open_rw` store) lives in the `sui`
//! binary, is async, and needs privileged store writes. Wiring that pipeline
//! *into* the evaluator would invert the dependency graph
//! (`sui-eval → sui-build → sandbox`) and force a tokio runtime + privileged
//! store into every pure eval.
//!
//! Instead the binary installs a **realize hook** — a thread-local callback the
//! evaluator invokes with `(drv_path, out_path)` at the exact moment a
//! derivation output is demanded on disk. The binary's hook opens the store,
//! substitutes-then-builds the closure, and returns once the output exists.
//! sui-eval stays pure; orchestration stays in the binary. This mirrors the
//! `INPUT_SOURCE_MAP` thread-local in `path.rs` (fetched-input redirect) — same
//! separation-of-concerns pattern, one layer up (a build, not a read-redirect).
//!
//! ## Byte-parity invariant
//!
//! The realize hook changes **no value** the evaluator observes: the `drvPath`
//! and `outPath` are computed by the module fixpoint *before* realize runs, and
//! are already byte-correct against nix (the marquee darwin roots proved this).
//! Realize only makes the bytes at that already-correct `outPath` *present on
//! disk*. Because the drvPath is byte-identical to nix, the realized output is
//! byte-identical to nix (same drv ⇒ same output path ⇒ same content). If no
//! hook is installed, IFD degrades to the pre-existing ENOENT — never a wrong
//! answer.

use std::cell::RefCell;
use std::rc::Rc;

/// The realize callback: given a derivation's `.drv` path and its expected
/// output store path, materialize that output on disk (substitute or build the
/// closure) and return `Ok(())` once the output path exists. On failure returns
/// a human-readable error string (surfaced as an eval `IoError`).
pub type RealizeFn = dyn Fn(&str, &str) -> Result<(), String>;

thread_local! {
    /// The installed realize hook for this eval thread, if any. `None` means
    /// IFD is unsupported on this thread — a demanded-but-absent derivation
    /// output falls through to the normal ENOENT read error (no wrong answer,
    /// no silent success). Held behind an `Rc` so a call can clone the handle
    /// out and invoke it while a **nested** realize on the same thread still
    /// observes the hook as installed (required for re-entrant dependency
    /// realizes and for cycle detection).
    static REALIZE_HOOK: RefCell<Option<Rc<RealizeFn>>> = const { RefCell::new(None) };

    /// Re-entrancy guard: a set of `out_path`s currently being realized on this
    /// thread. A derivation whose realize itself triggers eval that demands the
    /// SAME output must not recurse infinitely — it is a genuine cycle nix also
    /// rejects. Bounded: an entry is removed as soon as its realize returns.
    static IN_FLIGHT: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Install a realize hook for the current thread. Returns a guard that removes
/// the hook (restoring any previous one) when dropped — so a scoped install
/// (e.g. one CLI eval) does not leak into unrelated thread reuse.
///
/// The binary calls this once around the eval whose drvPath graph may read a
/// built output.
#[must_use = "the returned guard uninstalls the hook when dropped"]
pub fn install_realize_hook(hook: Box<RealizeFn>) -> RealizeHookGuard {
    let previous = REALIZE_HOOK.with(|h| h.borrow_mut().replace(Rc::from(hook)));
    RealizeHookGuard { previous: Some(previous) }
}

/// RAII guard restoring the prior realize hook when dropped.
pub struct RealizeHookGuard {
    previous: Option<Option<Rc<RealizeFn>>>,
}

impl Drop for RealizeHookGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.previous.take() {
            REALIZE_HOOK.with(|h| *h.borrow_mut() = prev);
        }
    }
}

/// Whether a realize hook is installed on this thread.
#[must_use]
pub fn has_realize_hook() -> bool {
    REALIZE_HOOK.with(|h| h.borrow().is_some())
}

/// Realize a derivation output mid-eval so a subsequent filesystem read of
/// `out_path` succeeds.
///
/// - If no hook is installed, returns `Ok(false)` (caller falls through to the
///   normal read, which will ENOENT — never a wrong value).
/// - If the same `out_path` is already being realized on this thread (an
///   IFD-realizes-itself cycle — a real nix error too), returns an `Err` rather
///   than recursing.
/// - Otherwise invokes the hook and returns `Ok(true)` once it succeeds.
///
/// # Errors
///
/// Returns `Err(message)` when the hook itself fails, or when a realize cycle
/// is detected.
pub fn realize_output(drv_path: &str, out_path: &str) -> Result<bool, String> {
    let hook_present = REALIZE_HOOK.with(|h| h.borrow().is_some());
    if !hook_present {
        return Ok(false);
    }

    // Bounded re-entrancy guard: refuse to realize an output whose realize is
    // already in flight on this thread.
    let already_in_flight = IN_FLIGHT.with(|f| f.borrow().iter().any(|p| p == out_path));
    if already_in_flight {
        return Err(format!(
            "import-from-derivation cycle: realizing '{out_path}' requires evaluating its own realize"
        ));
    }

    IN_FLIGHT.with(|f| f.borrow_mut().push(out_path.to_string()));
    // Clone the `Rc` handle OUT of the RefCell so the borrow is released before
    // the (possibly re-entrant) hook runs — the hook's own eval may query the
    // hook again (nested dependency realize), which must observe it still
    // installed. Dropping the clone after the call is cheap.
    let hook = REALIZE_HOOK.with(|h| h.borrow().clone());
    let result = match hook {
        Some(f) => f(drv_path, out_path),
        None => Ok(()),
    };
    IN_FLIGHT.with(|f| {
        let mut f = f.borrow_mut();
        if let Some(pos) = f.iter().position(|p| p == out_path) {
            f.remove(pos);
        }
    });

    result.map(|()| true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn no_hook_returns_false() {
        // A fresh thread has no hook: realize is a no-op that reports "not
        // realized" so the caller falls through to the normal read.
        assert!(!has_realize_hook());
        assert_eq!(realize_output("/nix/store/x.drv", "/nix/store/x-out").unwrap(), false);
    }

    #[test]
    fn hook_is_invoked_with_drv_and_out() {
        let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let _guard = install_realize_hook(Box::new(move |drv, out| {
            seen2.lock().unwrap().push((drv.to_string(), out.to_string()));
            Ok(())
        }));
        assert!(has_realize_hook());
        assert_eq!(realize_output("/nix/store/a.drv", "/nix/store/a-out").unwrap(), true);
        let s = seen.lock().unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, "/nix/store/a.drv");
        assert_eq!(s[0].1, "/nix/store/a-out");
    }

    #[test]
    fn guard_uninstalls_on_drop() {
        assert!(!has_realize_hook());
        {
            let _guard = install_realize_hook(Box::new(|_, _| Ok(())));
            assert!(has_realize_hook());
        }
        assert!(!has_realize_hook());
    }

    #[test]
    fn hook_error_propagates() {
        let _guard = install_realize_hook(Box::new(|_, _| Err("boom".to_string())));
        let e = realize_output("/nix/store/b.drv", "/nix/store/b-out").unwrap_err();
        assert!(e.contains("boom"));
    }

    #[test]
    fn reentrancy_cycle_is_refused() {
        // A hook that tries to realize the SAME out_path it is already realizing
        // must be refused, not recurse forever.
        let depth = Arc::new(AtomicUsize::new(0));
        let depth2 = depth.clone();
        let _guard = install_realize_hook(Box::new(move |drv, out| {
            depth2.fetch_add(1, Ordering::SeqCst);
            // Re-enter with the same out_path — must return an Err (cycle),
            // NOT recurse into the hook again.
            realize_output(drv, out)?;
            Ok(())
        }));
        let e = realize_output("/nix/store/c.drv", "/nix/store/c-out").unwrap_err();
        assert!(e.contains("cycle"), "expected a cycle error, got: {e}");
        // The outer hook ran exactly once; the inner re-entry was refused before
        // invoking the hook a second time.
        assert_eq!(depth.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn distinct_outputs_do_not_false_cycle() {
        // Realizing one output whose hook realizes a DIFFERENT output is fine
        // (a real dependency chain), not a cycle.
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();
        let _guard = install_realize_hook(Box::new(move |_drv, out| {
            let n = count2.fetch_add(1, Ordering::SeqCst);
            if n == 0 && out == "/nix/store/outer" {
                // Nested realize of a distinct output succeeds.
                realize_output("/nix/store/inner.drv", "/nix/store/inner")?;
            }
            Ok(())
        }));
        assert_eq!(realize_output("/nix/store/outer.drv", "/nix/store/outer").unwrap(), true);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
