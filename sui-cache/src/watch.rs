//! L2 GC-survival: decide which store paths the warm cache must capture.
//!
//! ── WHAT THIS CLOSES ──────────────────────────────────────────────────────
//! `theory/ATATAME.md` calls L2 *"the single highest-value gap in this
//! doctrine"*. `attic watch-store` used to provide it; attic was retired
//! 2026-07-31, and until something captures a newly-realized store path
//! before `nix-collect-garbage` reaches it, the warm store is a
//! within-session memo rather than a durable win.
//!
//! ── A RECONCILER, NOT AN EVENT STREAM ─────────────────────────────────────
//! attic watched inotify events. This diffs *state* instead, and that is a
//! deliberate upgrade rather than an implementation shortcut:
//!
//!   * it converges after a restart, a missed event, or a crash — an event
//!     stream loses whatever happened while it was down;
//!   * it captures paths that arrived by SUBSTITUTION, which no build hook
//!     ever sees. rio's post-build hook only pushes what rio BUILDS, so a
//!     substituted path has never been covered by anything;
//!   * "what is missing" is computed from the cache itself, so a push that
//!     failed is simply still missing next pass — retry needs no bookkeeping.
//!
//! ── WHY A BASELINE, AND WHY IT IS THE DEFAULT ─────────────────────────────
//! A pure reconciler would mirror the ENTIRE store on first run. Measured on
//! rio 2026-08-08: 60,025 store paths against 6,929 cached, so the first pass
//! would try to capture ~53,000 paths and grow a 12 GiB cache toward the size
//! of the whole store. That is not what the doctrine asks for — L2 is
//! *survival of newly-realized paths*, not a full mirror.
//!
//! So the watcher records a BASELINE at startup and captures only what
//! appears after it. `--initial-reconcile` starts from an empty baseline for
//! operators who do want the backfill, and `max_per_pass` bounds either mode
//! so a large build cannot turn one tick into an unbounded upload.

use std::collections::HashSet;

/// What a single capture pass should do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapturePlan {
    /// Store-path hashes to push this pass, bounded by `max_per_pass`.
    pub to_capture: Vec<String>,
    /// Candidates that did not fit this pass. They are NOT lost — the next
    /// pass recomputes from live state and picks them up. Reported so a
    /// persistently non-zero value tells the operator the interval is too
    /// long or the bound too small for this machine's build rate.
    pub deferred: usize,
}

/// Decide what to capture, from state alone.
///
/// `valid` are the hashes currently valid in the nix store, `cached` the
/// hashes the cache already holds, `baseline` the hashes that existed when
/// the watcher started (empty for a full backfill).
///
/// A candidate is a path that is valid, not cached, and not in the baseline.
/// Order is deterministic (sorted) so a bounded pass is reproducible rather
/// than dependent on hash-map iteration order — an operator re-running a pass
/// against unchanged state gets the same answer.
#[must_use]
pub fn plan_capture(
    valid: &[String],
    cached: &HashSet<String>,
    baseline: &HashSet<String>,
    max_per_pass: usize,
) -> CapturePlan {
    let mut candidates: Vec<String> = valid
        .iter()
        .filter(|h| !cached.contains(*h) && !baseline.contains(*h))
        .cloned()
        .collect();
    candidates.sort_unstable();
    candidates.dedup();

    let deferred = candidates.len().saturating_sub(max_per_pass);
    candidates.truncate(max_per_pass);
    CapturePlan {
        to_capture: candidates,
        deferred,
    }
}

/// Outcome of a capture pass, for the operator-facing line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WatchReport {
    /// Valid store paths considered.
    pub scanned: usize,
    /// Paths successfully pushed into the warm cache.
    pub captured: usize,
    /// Paths that failed to push. Non-fatal — a single bad path must never
    /// stop the watcher, or one unreadable path disables GC-survival for the
    /// whole machine.
    pub failed: usize,
    /// Candidates that did not fit this pass.
    pub deferred: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }
    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// The default posture: a path present at startup is NOT captured. This
    /// is what stops the first pass on a real machine from trying to mirror
    /// 53,000 paths.
    #[test]
    fn baseline_paths_are_not_captured() {
        let plan = plan_capture(&v(&["a", "b"]), &set(&[]), &set(&["a", "b"]), 100);
        assert!(plan.to_capture.is_empty());
        assert_eq!(plan.deferred, 0);
    }

    /// The whole point: something realized after startup gets captured.
    #[test]
    fn a_newly_realized_path_is_captured() {
        let plan = plan_capture(&v(&["a", "b", "new"]), &set(&[]), &set(&["a", "b"]), 100);
        assert_eq!(plan.to_capture, v(&["new"]));
    }

    /// Already-cached paths are never re-pushed — this is what makes the
    /// watcher cheap to run on a short interval.
    #[test]
    fn cached_paths_are_skipped() {
        let plan = plan_capture(&v(&["a", "new"]), &set(&["new"]), &set(&["a"]), 100);
        assert!(plan.to_capture.is_empty());
    }

    /// A failed push leaves the path uncached, so the next pass retries it
    /// with no retry bookkeeping at all. Modelled here as: still valid, still
    /// not cached, still not in baseline => still a candidate.
    #[test]
    fn a_failed_capture_is_retried_next_pass() {
        let baseline = set(&["a"]);
        let first = plan_capture(&v(&["a", "new"]), &set(&[]), &baseline, 100);
        assert_eq!(first.to_capture, v(&["new"]));
        // push failed => cache still empty
        let second = plan_capture(&v(&["a", "new"]), &set(&[]), &baseline, 100);
        assert_eq!(second.to_capture, v(&["new"]), "must retry, not drop");
    }

    /// A big build must not turn one tick into an unbounded upload, and the
    /// overflow must be REPORTED rather than silently dropped.
    #[test]
    fn max_per_pass_bounds_the_work_and_reports_the_remainder() {
        let plan = plan_capture(&v(&["a", "b", "c", "d", "e"]), &set(&[]), &set(&[]), 2);
        assert_eq!(plan.to_capture.len(), 2);
        assert_eq!(plan.deferred, 3);
    }

    /// An empty baseline is the `--initial-reconcile` mode: everything
    /// missing becomes a candidate.
    #[test]
    fn empty_baseline_backfills() {
        let plan = plan_capture(&v(&["a", "b"]), &set(&["a"]), &set(&[]), 100);
        assert_eq!(plan.to_capture, v(&["b"]));
    }

    /// Deterministic order — a bounded pass over unchanged state must pick
    /// the same paths every time, not whatever the hash map yielded.
    #[test]
    fn selection_is_deterministic() {
        let valid = v(&["z", "m", "a", "q"]);
        let a = plan_capture(&valid, &set(&[]), &set(&[]), 2);
        let b = plan_capture(&valid, &set(&[]), &set(&[]), 2);
        assert_eq!(a, b);
        assert_eq!(a.to_capture, v(&["a", "m"]));
    }
}
