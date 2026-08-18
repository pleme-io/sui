//! The tree-walker's *consume* side of the `sui-normalize` attrset-binding
//! plan.
//!
//! Mirrors [`crate::resolve_env`]'s three parts — a one-way env-flag latch, a
//! thread-local table keyed by `(source_id, text_offset)`, and
//! populate/lookup/clear hooks wired into `eval_with_file` — because the
//! keying hazard is identical: a plan recorded for a binder at offset `o` in
//! one parse tree must never be read for a different (imported) tree that
//! happens to have a binder at the same offset.
//!
//! # ★ The failure discipline here is the INVERSE of `resolve_env`'s
//!
//! `sui-resolve` fails SAFE to `Dynamic` because its fallback is
//! *equivalent* — `lookup_fast` probes the same map with the same symbol, so
//! falling back costs only speed.
//!
//! **This table's fallback path is the divergence itself.** Falling back means
//! "walk `set.entries()` yourself", which is precisely the code that produces
//! the silent wrong answers `sui-normalize` exists to remove. So a miss must
//! never be treated as "nothing to do" in a group that NEEDED a plan.
//!
//! The shape that makes that safe: `sui-normalize` records a group **only**
//! when it has a duplicate static key or a dotted path. A miss therefore means
//! "this group has neither", which is exactly when the existing path is
//! already correct. The absence is a *positive* statement, not a fallback —
//! and that is what bounds this change's blast radius to the groups that are
//! wrong today.
//!
//! # Default ON since 2026-08-18
//!
//! `SUI_NORMALIZE=0` opts OUT, restoring the pre-plan construction path. The
//! latch survives the flip on purpose — a divergence suspected to come from
//! this pass is then one command away from being confirmed or cleared, which
//! is worth more than the tidiness of deleting it.

use std::cell::RefCell;
use std::sync::OnceLock;

use sui_normalize::{GroupPlan, NormalizeTable};

/// One-time read of `SUI_NORMALIZE`. Default ON; `SUI_NORMALIZE=0` opts out.
static ENABLED: OnceLock<bool> = OnceLock::new();

/// Whether plan-driven attrset construction is enabled. **Default: yes.**
///
/// Flipped from opt-in to opt-out on 2026-08-18, on this evidence:
///
/// * every wrong-answer shape in the class matches nix, including the
///   acceptance case `{ a = rec { b = c+1; d = 2; }; a.c = d+3; }.a.b` -> 6,
///   which needs mutual recursion ACROSS the merge boundary;
/// * a fleet scan of 4562 `.nix` files found ZERO false rejects — the one
///   rejection is a file `nix-instantiate --parse` also refuses;
/// * `sui perf-seal` moved DOWN or held on all three attr-merge rows
///   (`dotted full-set leaf deep-merge` 6 -> 5), which is what confirms the
///   splice happens at PARSE time rather than adding eval work;
/// * the suites are green both ways.
///
/// The latch is KEPT, deliberately, in the `SUI_SCOPE_NARROW` spirit:
/// `SUI_NORMALIZE=0` restores the pre-plan construction path, so a divergence
/// suspected to come from this pass can be bisected in one command instead of
/// a revert. That is also why the old entry loops are not deleted yet.
#[must_use]
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var("SUI_NORMALIZE").ok().as_deref() != Some("0"))
}

thread_local! {
    /// `(source_id << 32) | text_offset` -> the binder's plan. Same keying as
    /// `resolve_env::RESOLVE_TABLE` and `value::intern_cached`.
    static PLAN_TABLE: RefCell<rustc_hash::FxHashMap<u64, GroupPlan>> =
        RefCell::new(rustc_hash::FxHashMap::default());
}

#[inline]
fn key(source_id: u32, text_offset: u32) -> u64 {
    (u64::from(source_id) << 32) | u64::from(text_offset)
}

/// Merge a freshly-computed [`NormalizeTable`] into the thread-local table.
/// No-op when the flag is off.
pub fn populate(source_id: u32, table: &NormalizeTable) {
    if !enabled() {
        return;
    }
    PLAN_TABLE.with(|t| {
        let mut t = t.borrow_mut();
        for (offset, plan) in table.iter() {
            t.insert(key(source_id, offset), plan.clone());
        }
    });
}

/// The plan for the binder node at `text_offset` in `source_id`, if one was
/// recorded. `None` means the group needs no normalization — see the module
/// docs on why that is a positive statement rather than a fallback.
#[must_use]
pub fn plan_for(source_id: u32, text_offset: u32) -> Option<GroupPlan> {
    if !enabled() {
        return None;
    }
    PLAN_TABLE.with(|t| t.borrow().get(&key(source_id, text_offset)).cloned())
}

/// Drop every recorded plan. Wired into the same lifecycle point as
/// `resolve_env::clear`.
pub fn clear() {
    PLAN_TABLE.with(|t| t.borrow_mut().clear());
}
