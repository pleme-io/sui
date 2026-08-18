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
//! # Flag-gated on purpose
//!
//! `SUI_NORMALIZE=1` opts in. Off, every consume site takes today's exact
//! unchanged path, so landing this cannot move a single byte for anyone until
//! the flag is proven and the default flipped.

use std::cell::RefCell;
use std::sync::OnceLock;

use sui_normalize::{GroupPlan, NormalizeTable};

/// One-time read of `SUI_NORMALIZE`. `true` iff `SUI_NORMALIZE=1`.
static ENABLED: OnceLock<bool> = OnceLock::new();

/// Whether plan-driven attrset construction is enabled (`SUI_NORMALIZE=1`).
///
/// Read once and cached — matches `resolve_env::enabled()`'s one-way latch.
#[must_use]
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var("SUI_NORMALIZE").ok().as_deref() == Some("1"))
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
