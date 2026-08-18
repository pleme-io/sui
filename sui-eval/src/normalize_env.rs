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

use std::rc::Rc;

use sui_normalize::{GroupPlan, NormalizeError};

/// One-time read of `SUI_NORMALIZE`. Default ON; `SUI_NORMALIZE=0` opts out.
static ENABLED: OnceLock<bool> = OnceLock::new();

/// Whether plan-driven attrset construction is enabled. **Default: yes.**
///
/// Flipped from opt-in to opt-out on 2026-08-18, on this evidence:
///
/// * every wrong-answer shape in the class matches nix, including the
///   acceptance case `{ a = rec { b = c+1; d = 2; }; a.c = d+3; }.a.b` -> 6,
///   which needs mutual recursion ACROSS the merge boundary;
/// * a fleet scan found ZERO false rejects — every rejection is a file
///   `nix-instantiate --parse` also refuses. **Corrected 2026-08-18: the
///   count was FOUR, not "the one".** Re-scanned at 4570 files:
///   `blackmatter-services/…/jitsi`, `…/keycloak`,
///   `kindling-profiles/…/macos-developer`, and
///   `blackmatter/…/enhanced/composed.nix`, each verified individually
///   against `nix-instantiate --parse`, which refuses all four and names the
///   same attribute path sui does. The zero-false-rejects claim is unchanged
///   and is the load-bearing half; the "one" was a coverage figure that
///   rotted UPWARD — understated, so it read as modest and nothing ever
///   flagged it. Re-run the scanner rather than trusting this number:
///   `cargo run --release -p sui-normalize --example scan -- <dir>`;
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
    /// `Rc` because a lookup happens on EVERY evaluation of a planned attrset.
    /// Storing the plan by value made `plan_for` a deep copy of the whole plan
    /// subtree per evaluation; refcounted, it is a pointer bump.
    static PLAN_TABLE: RefCell<rustc_hash::FxHashMap<u64, Memo>> =
        RefCell::new(rustc_hash::FxHashMap::default());
}

#[inline]
fn key(source_id: u32, text_offset: u32) -> u64 {
    (u64::from(source_id) << 32) | u64::from(text_offset)
}

/// The plan for one binder node, computed ON DEMAND and memoized.
///
/// ★ Replaces a parse-door walk that planned every binder in every parsed
/// file. Laziness means most of those are never evaluated, so that work was
/// mostly discarded — measured as a ~4% wall-clock tax on a real nixpkgs eval.
/// Planning at first evaluation is identical in result (a group's plan depends
/// only on its own entries) and pays only for groups that are reached.
///
/// The memo is keyed exactly as before, so a plan computed for a node at
/// offset `o` in one parse tree is never read for a different (imported) tree
/// with a binder at the same offset.
///
/// A `None` return is a POSITIVE statement — the group has no duplicate static
/// key and no dotted path, so the caller's existing path is already correct.
/// See the module docs on why that is not a fallback.
///
/// # Errors
///
/// [`NormalizeError`] for a group nix itself rejects — a duplicate attribute
/// (`{ a = 1; a = 2; }`) or a duplicate formal.
///
/// ★ This used to swallow that error with `.ok().flatten()`, which silently
/// turned a rejection into "no plan" and sent the group down the entry loop.
/// The result was accepting what nix refuses, and — worse — accepting it
/// DIFFERENTLY from the bytecode VM: measured 2026-08-18, `{ a = 1; a = 2; }`
/// is `{ a = 2; }` on the walker and `{ a = 1; }` on the VM, both at exit 0
/// where nix exits 1. Neither answer is right, so no choice of winner
/// reconciles the engines; only refusing does.
pub fn plan_for_node<N>(
    node: &N,
    recursive: bool,
    source_id: u32,
    offset: u32,
) -> Result<Option<Rc<GroupPlan>>, NormalizeError>
where
    N: rnix::ast::HasEntry,
{
    if !enabled() {
        return Ok(None);
    }
    let k = key(source_id, offset);
    if let Some(hit) = PLAN_TABLE.with(|t| t.borrow().get(&k).cloned()) {
        return hit.0;
    }
    let computed = sui_normalize::plan_for_group(node, recursive).map(|p| p.map(Rc::new));
    PLAN_TABLE.with(|t| t.borrow_mut().insert(k, Memo(computed.clone())));
    computed
}

/// A memo entry.
///
/// `Memo(Ok(None))` records "this group needs no plan", which must be
/// remembered too — otherwise every evaluation of an ordinary attrset re-runs
/// `needs_plan`, which is the cost the memo exists to remove. A rejection is
/// memoized for the same reason: a group nix refuses is refused on every
/// evaluation, and re-deriving that is pure waste.
#[derive(Clone)]
struct Memo(Result<Option<Rc<GroupPlan>>, NormalizeError>);


/// Drop every recorded plan. Wired into the same lifecycle point as
/// `resolve_env::clear`.
pub fn clear() {
    PLAN_TABLE.with(|t| t.borrow_mut().clear());
}
