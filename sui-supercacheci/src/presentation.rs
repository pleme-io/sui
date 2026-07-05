//! `presentation` — the **guardrail-like optimal in-memory presentation guard**
//! (element 5, whole-set) + the **lapidar continuous self-tune** (element 6,
//! whole-stream) of `/super-cache-ci`.
//!
//! [`memory`](crate::memory) ships the *per-entry* decision cores:
//! [`present`](crate::memory::present) (where should *this one* derivation sit)
//! and [`evaluate_tune`](crate::memory::evaluate_tune) (accept-or-revert *this
//! one* knob move). This module lifts both to the level the build layer actually
//! consumes — the **whole set** and the **whole stream** — without forking
//! either primitive:
//!
//! 1. **[`organize`]** takes the *whole set* of [`PresentationInput`]s + a finite
//!    RAMDISK L1 budget and produces the **optimal organized presentation**: the
//!    hottest (highest priority-per-MiB) derivations warmed into L1 within
//!    budget, overflow demoted to an L2 pointer, huge bytes kept out of RAM in
//!    L3. RAMDISK is finite, so *which* derivations occupy RAM IS the game.
//! 2. **[`PresentationGuard`]** mirrors `guardrail`'s `RuleEngine::check → Decision`
//!    severity fold exactly (Block ⇒ short-circuit, first Warn tracked, else
//!    Allow): it **validates** a served presentation against typed invariant
//!    rules and returns [`PresentationDecision`]
//!    (`Optimal` / `Suboptimal` / `Invalid` — the `Allow` / `Warn` / `Block`
//!    analog). This is the guard-like validation that always maintains + serves
//!    the *optimal* RAMDISK presentation: [`organize`]'s output is `Optimal` by
//!    construction, and the guard *catches* any hand-built or drifted
//!    presentation that is not.
//! 3. **[`run_self_tune`]** folds a *stream* of [`TuneProposal`]s through
//!    [`evaluate_tune`](crate::memory::evaluate_tune), re-baselining each step to
//!    the running state, keeping only measured improvements. The running
//!    objective is **monotone non-decreasing by construction** — the build layer
//!    gets *continuously more efficient* and can never regress
//!    ([`TuneRun::is_non_regressing`]).
//!
//! ## Tier-honest (never round up)
//!
//! - **Shipped (this module):** the pure whole-set organizer, the guardrail-like
//!   validation engine, and the continuous self-tune fold — all side-effect-free
//!   total functions, exhaustively unit-tested without a cluster.
//! - **Heuristic, not proven-optimal:** [`organize`]'s admission is
//!   *greedy-by-density with a single in-order fill pass* — it provably (a)
//!   never exceeds budget, (b) serves L1 density-descending, and (c) leaves **no
//!   deferred L1-want that would still fit the remaining headroom**. It is **not**
//!   a proven 0/1-knapsack optimum (that would be the R3 structural upgrade); the
//!   guard's [`PresentationSeverity::Suboptimal`] rules verify the utilization
//!   invariants the heuristic *does* guarantee, honestly not more.
//! - **DESIGN / LiveTODO:** feeding [`organize`] a real build-DAG frontier and
//!   driving the `target_tier` placement through the `TieredBackend` resolver,
//!   and driving [`run_self_tune`] from live telemetry, is
//!   [`autorevivy`](https://github.com/pleme-io/autorevivy)'s coordinator loop —
//!   composed, never a second controller.

use serde::{Deserialize, Serialize};

use crate::memory::{
    CacheTier, EfficiencyReading, HUGE_MIB, PresentationInput, TuneKnob, TuneOutcome, TuneProposal,
    evaluate_tune, present,
};

// ───────────────────────────────────────────────────────────────────────────
// The finite RAMDISK budget + the organized, served presentation
// ───────────────────────────────────────────────────────────────────────────

/// The finite RAMDISK L1 budget the organizer admits into — the whole point of
/// element 5: RAM is scarce, so *which* derivations occupy it is an
/// optimization, not a given.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentationBudget {
    /// The MiB of hot L1 (Redis / RAM) available to warm derivations into.
    pub l1_mib: u32,
}

impl PresentationBudget {
    /// A concrete budget.
    #[must_use]
    pub fn of(l1_mib: u32) -> Self {
        Self { l1_mib }
    }
}

/// One derivation placed into the served presentation — carrying both what
/// [`present`](crate::memory::present) *wanted* and where it was actually
/// *placed*, so the guard can tell a genuine L2 pointer from an L1-want that was
/// demoted for budget.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PlacedEntry {
    /// The derivation's content-key.
    pub key: String,
    /// The tier [`present`](crate::memory::present) wanted this entry in.
    pub wanted_tier: CacheTier,
    /// The tier it was actually placed in (may be a budget demotion of
    /// `wanted_tier`).
    pub tier: CacheTier,
    /// Its resident size, MiB.
    pub size_mib: u32,
    /// Its staging priority (inverse of predicted time-to-need).
    pub priority: u64,
    /// The admission score: **priority per MiB** (×1000 for integer resolution).
    /// The hottest *and* smallest derivations pack RAM best — this is the value
    /// density the greedy admission sorts on.
    pub density: u64,
}

impl PlacedEntry {
    /// The priority-per-MiB admission density (×1000, integer, deterministic).
    #[must_use]
    fn density_of(priority: u64, size_mib: u32) -> u64 {
        priority.saturating_mul(1000) / u64::from(size_mib.max(1))
    }
}

/// The organized whole-set presentation served to the build layer — every input
/// placed into exactly one tier, L1 filled optimally within budget.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Presentation {
    /// The derivations warmed into hot L1 (RAM), density-descending.
    pub l1: Vec<PlacedEntry>,
    /// L2 (Postgres pointer / stream-on-demand) — native-L2 verdicts **and**
    /// L1-wants demoted for budget.
    pub l2: Vec<PlacedEntry>,
    /// L3 (object store) — huge, far-off bytes kept out of RAM.
    pub l3: Vec<PlacedEntry>,
    /// Left uncached (the `Cold` verdict — far + modest, not worth a tier).
    pub cold: Vec<PlacedEntry>,
    /// MiB of L1 the warm set occupies.
    pub l1_used_mib: u32,
    /// The L1 budget the warm set was admitted against.
    pub l1_budget_mib: u32,
}

impl Presentation {
    /// The unused L1 headroom, MiB — the room the guard checks was not wasted.
    #[must_use]
    pub fn l1_headroom_mib(&self) -> u32 {
        self.l1_budget_mib.saturating_sub(self.l1_used_mib)
    }
}

/// Build the **optimal organized presentation** of a whole set of derivations
/// under a finite RAMDISK budget — **element 5, whole-set**. Pure/deterministic.
///
/// The algorithm: run [`present`](crate::memory::present) per input; place the
/// L2/L3/Cold verdicts directly; then admit the L1-wants **greedily by density
/// (priority-per-MiB), best first, in a single in-order pass** — each admitted
/// if it fits the remaining headroom, else demoted to an L2 pointer. That single
/// pass guarantees the three utilization invariants the [`PresentationGuard`]
/// verifies: within budget, L1 served density-descending, and **no deferred
/// L1-want still fits the remaining headroom** (any entry that didn't fit at its
/// turn cannot fit the only-smaller final headroom). It is a heuristic, not a
/// proven 0/1-knapsack optimum — see the module tier note.
#[must_use]
pub fn organize(inputs: &[PresentationInput], budget: PresentationBudget) -> Presentation {
    let mut out = Presentation {
        l1_budget_mib: budget.l1_mib,
        ..Presentation::default()
    };

    // Partition by what present() wants. L1-wants become admission candidates.
    let mut candidates: Vec<PlacedEntry> = Vec::new();
    for i in inputs {
        let v = present(i);
        let entry = PlacedEntry {
            key: v.key.clone(),
            wanted_tier: v.target_tier,
            tier: v.target_tier,
            size_mib: i.size_mib,
            priority: v.priority,
            density: PlacedEntry::density_of(v.priority, i.size_mib),
        };
        match v.target_tier {
            CacheTier::L1Redis => candidates.push(entry),
            CacheTier::L2Pg => out.l2.push(entry),
            CacheTier::L3Object => out.l3.push(entry),
            CacheTier::Cold => out.cold.push(entry),
        }
    }

    // Deterministic best-first order: density descending, key ascending on ties.
    candidates.sort_by(|a, b| b.density.cmp(&a.density).then_with(|| a.key.cmp(&b.key)));

    // Single in-order admission pass — admit-if-fits-else-demote. This is what
    // makes both the density-order and the no-wasted-headroom invariants true by
    // construction.
    let mut remaining = budget.l1_mib;
    for mut c in candidates {
        if c.size_mib <= remaining {
            remaining -= c.size_mib;
            out.l1.push(c);
        } else {
            // Demote the L1-want to an L2 pointer; keep `wanted_tier == L1Redis`
            // so the guard can see it was a budget demotion, not a native L2.
            c.tier = CacheTier::L2Pg;
            out.l2.push(c);
        }
    }
    out.l1_used_mib = budget.l1_mib.saturating_sub(remaining);
    out
}

// ───────────────────────────────────────────────────────────────────────────
// The guardrail-like validation engine (Rule → Decision, Block>Warn>Allow fold)
// ───────────────────────────────────────────────────────────────────────────

/// A presentation rule's severity — the `guardrail::Severity` analog. `Invalid`
/// is the `Block` peer (a served presentation that is *wrong* — over budget,
/// bytes in RAM); `Suboptimal` is the `Warn` peer (served, but not the most
/// efficient organization).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PresentationSeverity {
    /// The presentation violates a hard correctness invariant (⇒ `Invalid`).
    Invalid,
    /// The presentation is servable but not optimally organized (⇒ `Suboptimal`).
    Suboptimal,
}

impl core::fmt::Display for PresentationSeverity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            PresentationSeverity::Invalid => "invalid",
            PresentationSeverity::Suboptimal => "suboptimal",
        })
    }
}

/// A finding a rule produced — the rule name + a static explanation. Static
/// strings keep the finding a typed value (no `format!()` — the fleet
/// ★★ TYPED EMISSION ban); the [`Display`](core::fmt::Display) impl on
/// [`PresentationDecision`] is the one render surface.
///
/// `Serialize`-only (no `Deserialize`): a finding is an emitted verdict — the
/// `&'static str` fields serialize into a keyway JSON receipt but are never
/// parsed back (a `&'static str` cannot be produced by a deserializer).
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentationFinding {
    /// The rule that fired.
    pub rule: &'static str,
    /// The static, human-readable reason.
    pub detail: &'static str,
}

/// The guard's verdict on a served presentation — the
/// `guardrail::Decision` analog. `Optimal` = `Allow`, `Suboptimal` = `Warn`,
/// `Invalid` = `Block`.
///
/// `Serialize`-only (no `Deserialize`): the decision carries a
/// [`PresentationFinding`] whose `&'static str` fields cannot be deserialized —
/// a decision is an emitted verdict, never a parsed input.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PresentationDecision {
    /// The served presentation satisfies every invariant — the optimal RAMDISK
    /// organization. (`Allow` peer.)
    Optimal,
    /// Servable, but a utilization invariant is not tight. (`Warn` peer.)
    Suboptimal(PresentationFinding),
    /// A hard correctness invariant is violated — do not serve as-is.
    /// (`Block` peer.)
    Invalid(PresentationFinding),
}

impl PresentationDecision {
    /// Whether the presentation is servable at all (`Optimal` or `Suboptimal`) —
    /// only `Invalid` is a hard stop.
    #[must_use]
    pub fn is_servable(&self) -> bool {
        !matches!(self, PresentationDecision::Invalid(_))
    }

    /// Whether the presentation is the *optimal* organization.
    #[must_use]
    pub fn is_optimal(&self) -> bool {
        matches!(self, PresentationDecision::Optimal)
    }
}

impl core::fmt::Display for PresentationDecision {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PresentationDecision::Optimal => f.write_str("optimal"),
            PresentationDecision::Suboptimal(x) => {
                write!(f, "suboptimal [{}]: {}", x.rule, x.detail)
            }
            PresentationDecision::Invalid(x) => write!(f, "invalid [{}]: {}", x.rule, x.detail),
        }
    }
}

/// One typed presentation invariant — the `guardrail::Rule` analog. `eval`
/// returns `Some(detail)` when the invariant is violated, `None` when it holds.
#[derive(Debug, Clone, Copy)]
pub struct PresentationRule {
    /// The rule's stable name (appears in the [`PresentationFinding`]).
    pub name: &'static str,
    /// The severity a violation carries.
    pub severity: PresentationSeverity,
    /// The pure predicate — `Some(static reason)` iff violated.
    pub eval: fn(&Presentation) -> Option<&'static str>,
}

fn rule_l1_over_budget(p: &Presentation) -> Option<&'static str> {
    if p.l1_used_mib > p.l1_budget_mib {
        Some("L1 warm set exceeds the RAMDISK budget")
    } else {
        None
    }
}

fn rule_huge_bytes_in_l1(p: &Presentation) -> Option<&'static str> {
    if p.l1.iter().any(|e| e.size_mib > HUGE_MIB) {
        Some("a huge (> HUGE_MIB) blob is warmed into RAM; bytes belong in L3")
    } else {
        None
    }
}

fn rule_l1_density_ordered(p: &Presentation) -> Option<&'static str> {
    let ordered = p.l1.windows(2).all(|w| w[0].density >= w[1].density);
    if ordered {
        None
    } else {
        Some("L1 is not served density-descending; hottest derivations are not presented first")
    }
}

fn rule_wasted_l1_headroom(p: &Presentation) -> Option<&'static str> {
    let headroom = p.l1_headroom_mib();
    let wasted = p
        .l2
        .iter()
        .any(|e| e.wanted_tier == CacheTier::L1Redis && e.size_mib <= headroom);
    if wasted {
        Some("an L1-wanted derivation was deferred while RAMDISK headroom could still hold it")
    } else {
        None
    }
}

/// The default presentation invariants — two hard (`Invalid`) correctness rules
/// and two soft (`Suboptimal`) utilization rules. A const array so the catalog
/// is mechanically enumerable (CATALOG REFLECTION).
pub const DEFAULT_PRESENTATION_RULES: [PresentationRule; 4] = [
    PresentationRule {
        name: "l1-over-budget",
        severity: PresentationSeverity::Invalid,
        eval: rule_l1_over_budget,
    },
    PresentationRule {
        name: "huge-bytes-in-l1",
        severity: PresentationSeverity::Invalid,
        eval: rule_huge_bytes_in_l1,
    },
    PresentationRule {
        name: "l1-density-ordered",
        severity: PresentationSeverity::Suboptimal,
        eval: rule_l1_density_ordered,
    },
    PresentationRule {
        name: "wasted-l1-headroom",
        severity: PresentationSeverity::Suboptimal,
        eval: rule_wasted_l1_headroom,
    },
];

/// The `default_rules()` peer — the default invariant set as an owned `Vec`.
#[must_use]
pub fn default_presentation_rules() -> Vec<PresentationRule> {
    DEFAULT_PRESENTATION_RULES.to_vec()
}

/// The guardrail-like presentation guard — mirrors `guardrail::RuleEngine`. It
/// holds an ordered rule set and folds it into a single [`PresentationDecision`]
/// with the **same** severity priority as guardrail's engine: the first
/// `Invalid` short-circuits (like `Block`), otherwise the first `Suboptimal` is
/// carried (like `Warn`), otherwise `Optimal` (like `Allow`).
#[derive(Debug, Clone)]
pub struct PresentationGuard {
    rules: Vec<PresentationRule>,
}

impl Default for PresentationGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl PresentationGuard {
    /// A guard with the default invariant set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rules: default_presentation_rules(),
        }
    }

    /// A guard with a custom invariant set (for tests / bespoke postures).
    #[must_use]
    pub fn with_rules(rules: Vec<PresentationRule>) -> Self {
        Self { rules }
    }

    /// The rule set this guard folds.
    #[must_use]
    pub fn rules(&self) -> &[PresentationRule] {
        &self.rules
    }

    /// The number of rules.
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Validate a served presentation — **element 5, the guard**. Pure. The fold
    /// is byte-for-byte the `guardrail::RuleEngine::check` shape: `Invalid`
    /// short-circuits, first `Suboptimal` is tracked, else `Optimal`.
    #[must_use]
    pub fn check(&self, p: &Presentation) -> PresentationDecision {
        let mut first_suboptimal: Option<PresentationFinding> = None;
        for r in &self.rules {
            if let Some(detail) = (r.eval)(p) {
                let finding = PresentationFinding {
                    rule: r.name,
                    detail,
                };
                match r.severity {
                    PresentationSeverity::Invalid => return PresentationDecision::Invalid(finding),
                    PresentationSeverity::Suboptimal if first_suboptimal.is_none() => {
                        first_suboptimal = Some(finding);
                    }
                    PresentationSeverity::Suboptimal => {}
                }
            }
        }
        first_suboptimal.map_or(PresentationDecision::Optimal, PresentationDecision::Suboptimal)
    }
}

/// Organize a whole set **and** validate the result in one call — the composed
/// element-5 surface the build layer consumes: the served presentation plus the
/// guard's verdict on it (which, for [`organize`]'s own output, is
/// [`PresentationDecision::Optimal`] by construction).
#[must_use]
pub fn organize_and_validate(
    inputs: &[PresentationInput],
    budget: PresentationBudget,
) -> (Presentation, PresentationDecision) {
    let p = organize(inputs, budget);
    let decision = PresentationGuard::new().check(&p);
    (p, decision)
}

// ───────────────────────────────────────────────────────────────────────────
// The lapidar CONTINUOUS self-tune (element 6, whole-stream)
// ───────────────────────────────────────────────────────────────────────────

/// The result of running the continuous self-tune over a stream of proposals —
/// which knobs were accepted, the final reading, and the baseline it started
/// from. The invariant [`is_non_regressing`](TuneRun::is_non_regressing) holds
/// by construction.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TuneRun {
    /// The reading the run started from.
    pub baseline: EfficiencyReading,
    /// The reading the run ended on (== the last accepted `after_shadow`, or the
    /// baseline if nothing was accepted).
    pub final_reading: EfficiencyReading,
    /// The knobs whose shadow-measured change was accepted, in order.
    pub accepted: Vec<TuneKnob>,
    /// How many proposals were considered.
    pub considered: u32,
}

impl TuneRun {
    /// The net objective improvement over the run — always `>= 0`.
    #[must_use]
    pub fn improvement(&self) -> i64 {
        self.final_reading.objective() - self.baseline.objective()
    }

    /// The continuous-efficiency invariant: the run never regressed the
    /// objective. True by construction (an accept requires a strict improvement
    /// past the margin; a revert leaves the running state untouched).
    #[must_use]
    pub fn is_non_regressing(&self) -> bool {
        self.final_reading.objective() >= self.baseline.objective()
    }
}

/// Run the **continuous** lapidar self-tune over a stream of shadow-measured
/// proposals — **element 6, whole-stream**. Pure/deterministic.
///
/// Each proposal is **re-baselined to the running state** and evaluated with the
/// *same* [`evaluate_tune`](crate::memory::evaluate_tune) primitive (extend, do
/// not fork): a proposal is accepted only if its shadow reading beats the
/// **current** running objective past [`TUNE_MARGIN`](crate::memory::TUNE_MARGIN),
/// which makes the running objective monotone non-decreasing — the build layer
/// gets continuously more efficient and can never regress. A regression, or a
/// gain that is real against the original baseline but not against the improved
/// running state, is reverted.
#[must_use]
pub fn run_self_tune(baseline: EfficiencyReading, proposals: &[TuneProposal]) -> TuneRun {
    let mut running = baseline;
    let mut accepted: Vec<TuneKnob> = Vec::new();
    for p in proposals {
        // Re-baseline this step to the running state, then reuse evaluate_tune.
        let step = TuneProposal {
            knob: p.knob,
            before: running,
            after_shadow: p.after_shadow,
        };
        if evaluate_tune(&step) == TuneOutcome::Accept {
            accepted.push(p.knob);
            running = p.after_shadow;
        }
    }
    TuneRun {
        baseline,
        final_reading: running,
        accepted,
        considered: u32::try_from(proposals.len()).unwrap_or(u32::MAX),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Vocabulary bridge
// ───────────────────────────────────────────────────────────────────────────

/// The `(def…)` authoring keywords for the whole-set surfaces added here — the
/// vocabulary bridge (distinct from [`memory::AUTHORING_KEYWORDS`](crate::memory::AUTHORING_KEYWORDS)).
/// The `#[derive(DeriveTataraDomain)]` attach is the same LiveTODO gated on the
/// sui-workspace `tatara-lisp` pin skew — naming the keyword without a compiling
/// derive is the honest tier.
pub const PRESENTATION_KEYWORDS: [&str; 2] = [
    "defpresentationguard", // the whole-set guard (organize + PresentationGuard)
    "defselftune",          // the continuous self-tune (run_self_tune / TuneRun)
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{AUTHORING_KEYWORDS, HUGE_MIB};

    fn input(key: &str, next_use_secs: u32, size_mib: u32, tier: CacheTier) -> PresentationInput {
        PresentationInput {
            key: key.to_string(),
            predicted_next_use_secs: next_use_secs,
            size_mib,
            tier,
        }
    }

    fn reading(hit: u8, p50: u32, ondemand: u32, waste: u32) -> EfficiencyReading {
        EfficiencyReading {
            hit_rate_pct: hit,
            build_p50_ms: p50,
            ondemand_secs: ondemand,
            ram_waste_mib: waste,
        }
    }

    // ── organize: optimality ────────────────────────────────────────────────

    #[test]
    fn organize_warms_small_soon_entries_into_l1_within_budget() {
        // Two small soon entries, ample budget → both warm, density-ordered.
        let inputs = vec![
            input("a", 10, 64, CacheTier::Cold),
            input("b", 2, 32, CacheTier::Cold), // sooner + smaller → higher density
        ];
        let p = organize(&inputs, PresentationBudget::of(1024));
        assert_eq!(p.l1.len(), 2);
        assert!(p.l1_used_mib <= p.l1_budget_mib);
        // b (sooner + smaller) is denser → served first.
        assert_eq!(p.l1[0].key, "b");
        assert!(p.l1[0].density >= p.l1[1].density);
    }

    #[test]
    fn organize_demotes_overflow_to_l2_pointer_under_tight_budget() {
        // Budget only fits one of two small-soon entries → the denser is warmed,
        // the other is demoted to L2 with wanted_tier still L1.
        let inputs = vec![
            input("big", 10, 200, CacheTier::Cold),
            input("small", 2, 100, CacheTier::Cold),
        ];
        let p = organize(&inputs, PresentationBudget::of(150));
        assert_eq!(p.l1.len(), 1);
        assert_eq!(p.l1[0].key, "small", "the denser (sooner+smaller) entry wins RAM");
        assert_eq!(p.l2.len(), 1);
        assert_eq!(p.l2[0].key, "big");
        assert_eq!(p.l2[0].wanted_tier, CacheTier::L1Redis, "a demoted L1-want");
        assert_eq!(p.l2[0].tier, CacheTier::L2Pg);
    }

    #[test]
    fn organize_routes_far_huge_bytes_to_l3_never_ram() {
        let inputs = vec![input("huge", 600, 4096, CacheTier::L1Redis)];
        let p = organize(&inputs, PresentationBudget::of(65536));
        assert!(p.l1.is_empty(), "huge far bytes never enter RAM");
        assert_eq!(p.l3.len(), 1);
        assert_eq!(p.l3[0].tier, CacheTier::L3Object);
    }

    #[test]
    fn organize_output_is_guard_optimal_end_to_end() {
        // A mixed set that exceeds budget — organize must still produce a
        // presentation the guard rates Optimal (within budget, density-ordered,
        // no wasted headroom, no huge-in-RAM).
        let inputs = vec![
            input("s1", 1, 64, CacheTier::Cold),
            input("s2", 3, 128, CacheTier::Cold),
            input("s3", 5, 200, CacheTier::Cold),
            input("s4", 2, 90, CacheTier::Cold),
            input("l1", 4, 4096, CacheTier::L1Redis), // large+soon → L2 pointer
            input("h1", 900, 8192, CacheTier::L1Redis), // far+huge → L3
        ];
        let (p, decision) = organize_and_validate(&inputs, PresentationBudget::of(300));
        assert_eq!(decision, PresentationDecision::Optimal, "got {decision}");
        assert!(p.l1_used_mib <= 300);
    }

    #[test]
    fn organize_fill_pass_leaves_no_deferred_want_that_fits_headroom() {
        // A dense-but-big entry blocks first; a smaller one must still be
        // admitted into the leftover headroom (single in-order pass) so no
        // deferred L1-want fits the remaining budget.
        let inputs = vec![
            input("blocker", 1, 250, CacheTier::Cold), // densest, admitted first
            input("filler", 2, 40, CacheTier::Cold),   // must fill leftover
        ];
        let p = organize(&inputs, PresentationBudget::of(300));
        // 250 + 40 = 290 <= 300 → both fit.
        assert_eq!(p.l1.len(), 2);
        let guard = PresentationGuard::new();
        assert_eq!(guard.check(&p), PresentationDecision::Optimal);
    }

    #[test]
    fn organize_empty_is_optimal() {
        let p = organize(&[], PresentationBudget::of(1024));
        assert!(p.l1.is_empty() && p.l2.is_empty() && p.l3.is_empty() && p.cold.is_empty());
        assert_eq!(PresentationGuard::new().check(&p), PresentationDecision::Optimal);
    }

    // ── guard: the guardrail-like validation engine ──────────────────────────

    #[test]
    fn guard_flags_over_budget_as_invalid() {
        let p = Presentation {
            l1: vec![],
            l1_used_mib: 200,
            l1_budget_mib: 100,
            ..Presentation::default()
        };
        match PresentationGuard::new().check(&p) {
            PresentationDecision::Invalid(f) => assert_eq!(f.rule, "l1-over-budget"),
            other => panic!("expected Invalid over-budget, got {other}"),
        }
    }

    #[test]
    fn guard_flags_huge_in_l1_as_invalid() {
        let huge = PlacedEntry {
            key: "h".into(),
            wanted_tier: CacheTier::L1Redis,
            tier: CacheTier::L1Redis,
            size_mib: HUGE_MIB + 1,
            priority: 10,
            density: 10,
        };
        let p = Presentation {
            l1: vec![huge.clone()],
            l1_used_mib: huge.size_mib,
            l1_budget_mib: HUGE_MIB * 4,
            ..Presentation::default()
        };
        match PresentationGuard::new().check(&p) {
            PresentationDecision::Invalid(f) => assert_eq!(f.rule, "huge-bytes-in-l1"),
            other => panic!("expected Invalid huge-in-l1, got {other}"),
        }
    }

    #[test]
    fn guard_flags_unordered_density_as_suboptimal() {
        // L1 served low-density-first → suboptimal presentation order.
        let lo = PlacedEntry { key: "lo".into(), wanted_tier: CacheTier::L1Redis, tier: CacheTier::L1Redis, size_mib: 10, priority: 1, density: 100 };
        let hi = PlacedEntry { key: "hi".into(), wanted_tier: CacheTier::L1Redis, tier: CacheTier::L1Redis, size_mib: 10, priority: 9, density: 900 };
        let p = Presentation {
            l1: vec![lo, hi], // WRONG order (ascending density)
            l1_used_mib: 20,
            l1_budget_mib: 1024,
            ..Presentation::default()
        };
        match PresentationGuard::new().check(&p) {
            PresentationDecision::Suboptimal(f) => assert_eq!(f.rule, "l1-density-ordered"),
            other => panic!("expected Suboptimal density-order, got {other}"),
        }
    }

    #[test]
    fn guard_flags_wasted_headroom_as_suboptimal() {
        // An L1-want deferred to L2 that would fit the free headroom → waste.
        let deferred = PlacedEntry { key: "d".into(), wanted_tier: CacheTier::L1Redis, tier: CacheTier::L2Pg, size_mib: 50, priority: 5, density: 100 };
        let p = Presentation {
            l1: vec![],
            l2: vec![deferred],
            l1_used_mib: 100,
            l1_budget_mib: 300, // 200 free — the deferred 50 would fit
            ..Presentation::default()
        };
        match PresentationGuard::new().check(&p) {
            PresentationDecision::Suboptimal(f) => assert_eq!(f.rule, "wasted-l1-headroom"),
            other => panic!("expected Suboptimal wasted-headroom, got {other}"),
        }
    }

    #[test]
    fn guard_invalid_takes_priority_over_suboptimal() {
        // A presentation that is BOTH over budget (Invalid) AND density-unordered
        // (Suboptimal) must return Invalid — mirrors guardrail Block>Warn.
        let lo = PlacedEntry { key: "lo".into(), wanted_tier: CacheTier::L1Redis, tier: CacheTier::L1Redis, size_mib: 10, priority: 1, density: 100 };
        let hi = PlacedEntry { key: "hi".into(), wanted_tier: CacheTier::L1Redis, tier: CacheTier::L1Redis, size_mib: 10, priority: 9, density: 900 };
        let p = Presentation {
            l1: vec![lo, hi], // unordered → Suboptimal
            l1_used_mib: 500, // over budget → Invalid
            l1_budget_mib: 100,
            ..Presentation::default()
        };
        assert!(matches!(
            PresentationGuard::new().check(&p),
            PresentationDecision::Invalid(_)
        ));
    }

    #[test]
    fn guard_has_two_invalid_and_two_suboptimal_default_rules() {
        let invalid = DEFAULT_PRESENTATION_RULES
            .iter()
            .filter(|r| r.severity == PresentationSeverity::Invalid)
            .count();
        let suboptimal = DEFAULT_PRESENTATION_RULES
            .iter()
            .filter(|r| r.severity == PresentationSeverity::Suboptimal)
            .count();
        assert_eq!(invalid, 2);
        assert_eq!(suboptimal, 2);
        assert_eq!(PresentationGuard::new().rule_count(), 4);
    }

    #[test]
    fn decision_display_is_typed_render_surface() {
        assert_eq!(PresentationDecision::Optimal.to_string(), "optimal");
        let d = PresentationDecision::Invalid(PresentationFinding { rule: "r", detail: "d" });
        assert_eq!(d.to_string(), "invalid [r]: d");
        let s = PresentationDecision::Suboptimal(PresentationFinding { rule: "r", detail: "d" });
        assert_eq!(s.to_string(), "suboptimal [r]: d");
        assert_eq!(PresentationSeverity::Invalid.to_string(), "invalid");
        assert!(PresentationDecision::Optimal.is_optimal());
        assert!(PresentationDecision::Optimal.is_servable());
        assert!(!d.is_servable());
    }

    // ── continuous self-tune ──────────────────────────────────────────────────

    #[test]
    fn self_tune_keeps_only_improvements() {
        let baseline = reading(70, 2000, 100, 500); // objective 6790
        let improve = reading(85, 1500, 0, 200);    // better
        let regress = reading(50, 4000, 400, 900);  // worse
        let improve2 = reading(95, 800, 0, 0);       // best
        let run = run_self_tune(
            baseline,
            &[
                TuneProposal { knob: TuneKnob::RedisMaxmemory, before: baseline, after_shadow: improve },
                TuneProposal { knob: TuneKnob::TmpfsBand, before: baseline, after_shadow: regress },
                TuneProposal { knob: TuneKnob::PgPool, before: baseline, after_shadow: improve2 },
            ],
        );
        assert_eq!(run.accepted, vec![TuneKnob::RedisMaxmemory, TuneKnob::PgPool]);
        assert_eq!(run.final_reading, improve2);
        assert_eq!(run.considered, 3);
        assert!(run.improvement() > 0);
    }

    #[test]
    fn self_tune_is_monotone_non_regressing_across_regressions() {
        let baseline = reading(80, 1000, 0, 0);
        let worse1 = reading(10, 9000, 900, 9000);
        let worse2 = reading(20, 8000, 800, 8000);
        let run = run_self_tune(
            baseline,
            &[
                TuneProposal { knob: TuneKnob::SpotFamilies, before: baseline, after_shadow: worse1 },
                TuneProposal { knob: TuneKnob::TmpfsBand, before: baseline, after_shadow: worse2 },
            ],
        );
        assert!(run.accepted.is_empty(), "no regression is ever accepted");
        assert_eq!(run.final_reading, baseline);
        assert!(run.is_non_regressing());
        assert_eq!(run.improvement(), 0);
    }

    #[test]
    fn self_tune_rebaselines_each_step_not_the_original() {
        // A big jump lands first (running advances); a second proposal that is
        // better than the ORIGINAL baseline but worse than the RUNNING state must
        // be reverted — proving re-baselining.
        let baseline = reading(50, 3000, 0, 0);  // objective 4700
        let big = reading(95, 500, 0, 0);         // objective 9450 (accept)
        let mid = reading(70, 2000, 0, 0);        // objective 6800 (> baseline, < running)
        let run = run_self_tune(
            baseline,
            &[
                TuneProposal { knob: TuneKnob::RedisMaxmemory, before: baseline, after_shadow: big },
                TuneProposal { knob: TuneKnob::PgPool, before: baseline, after_shadow: mid },
            ],
        );
        assert_eq!(run.accepted, vec![TuneKnob::RedisMaxmemory]);
        assert_eq!(run.final_reading, big, "the mid proposal regresses the RUNNING state → reverted");
        assert!(run.is_non_regressing());
    }

    #[test]
    fn self_tune_empty_stream_is_identity() {
        let baseline = reading(80, 1000, 0, 0);
        let run = run_self_tune(baseline, &[]);
        assert_eq!(run.final_reading, baseline);
        assert!(run.accepted.is_empty());
        assert_eq!(run.considered, 0);
        assert!(run.is_non_regressing());
    }

    #[test]
    fn self_tune_reverts_marginal_gain_no_churn() {
        // A gain under TUNE_MARGIN must not be accepted (continuous no-churn).
        let baseline = reading(80, 1000, 0, 0);
        let marginal = reading(80, 990, 0, 0); // +1 objective, under the margin
        let run = run_self_tune(
            baseline,
            &[TuneProposal { knob: TuneKnob::PgPool, before: baseline, after_shadow: marginal }],
        );
        assert!(run.accepted.is_empty());
        assert_eq!(run.final_reading, baseline);
    }

    // ── vocabulary bridge ─────────────────────────────────────────────────────

    #[test]
    fn presentation_keywords_are_def_prefixed_unique_and_disjoint_from_memory() {
        let mut seen = std::collections::BTreeSet::new();
        for k in PRESENTATION_KEYWORDS {
            assert!(k.starts_with("def"), "{k} must be a def-form keyword");
            assert!(seen.insert(k), "{k} collision within the presentation surface");
        }
        // Cross-module honesty gate: no keyword collides with the memory surface.
        for k in PRESENTATION_KEYWORDS {
            assert!(
                !AUTHORING_KEYWORDS.contains(&k),
                "{k} collides with a memory-surface keyword"
            );
        }
    }
}
