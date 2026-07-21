//! Typed border for sui's **performance-lever claim ledger** — the
//! honesty surface over every optimization applied to the eval hot
//! path (the tree-walker + the bytecode VM).
//!
//! ## Why this domain exists
//!
//! A sui perf session produces *levers* — a NanBox repr swap, a lexical
//! pre-resolver, a positional-frame overlay, a redundant-store elision.
//! Each is a CLAIM: "this made eval faster and stayed byte-identical."
//! Historically those claims lived only in commit messages, a prose
//! arsenal doc, and a summary — and two claim-mistakes slipped through
//! by hand in one session:
//!
//!   * a lever **measured null** was almost recorded as a win
//!     (`m0-resolver`: `SUI_RESOLVE=1` measured −0.5 % ≈ null);
//!   * a lever **measured net-negative** was proposed as shippable
//!     (`positional-frames`: +7 %/+32–39 %), and its technique
//!     (a resolution change) was described with a proof-tier stronger
//!     than a partial byte corpus can honestly earn.
//!
//! This domain makes those two mistake-classes a property of the CLAIM,
//! not of the author's diligence: a null-as-win and a tier-over-claim
//! are **eval-caught** (the `is_honest` red-flag predicate, exactly the
//! shape of `laziness::ThunkDiscipline::is_correctly_classified`), and a
//! non-positive "improvement" is **truly-unrepresentable** on the sign
//! axis (the sealed [`Delta`], modeled on `sui_intern::memo::ContentKey`).
//!
//! ## What it is NOT (tier-honest — never round up)
//!
//! It types the claim SHAPE.  It does **not** generate optimizations and
//! it does **not** verify parity soundness — whether an edit is actually
//! force-order-neutral stays the `sui parity` byte oracle's job over a
//! PARTIAL corpus (C2, external observation, forever).  It also does NOT
//! catch two real session mistakes: attributing a number measured on one
//! engine to another (harness wiring, outside the claim's shape) and
//! re-claiming an already-landed lever (a cross-lever invariant — the
//! named M2 addition).  A technique *mislabeled* by the author (a
//! force-order change called `ReprSwap`) passes `is_honest` while being
//! byte-unsound — the type gates the technique CLASS, not the label's
//! truth.  `theory/BUILD.md` §II is the surrounding doctrine.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defperf-lever
//!   :name       "nanbox-upvalues"
//!   :attacks    ("vm-closure-upvalue-clone" "thunk-upvalue-clone")
//!   :technique  ReprSwap
//!   :proof-tier ByteSufficient
//!   :status     Landed
//!   :measured   Pending
//!   :speedup-bp 0
//!   :ceiling    NotApplicable)
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Delta — the sealed strictly-positive speedup (sign truly-unrep) ─

/// A **strictly-positive** speedup, in basis points (1 bp = 0.01 %; a
/// 1.86× speedup is 8 600 bp faster = `(1.86 − 1.0) × 10 000`).  The
/// sole constructors reject any non-positive measurement, so **no
/// `Delta` value names a null or a regression** — the never-ship-a-
/// regression rule made truly-unrepresentable on the *sign* axis
/// (modeled on `sui_intern::memo::ContentKey`'s sole-`of()` seal:
/// private field, fallible ctor).  Because a [`MeasuredKind::Improved`]
/// lever's delta is derived through this type, an "improved but actually
/// slower" lever is unconstructable.
///
/// The *provenance* of the number (right expression, same engine, real
/// run) is NOT sealed here — that stays only-CI-caught via the parity
/// harness.  Never conflate the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delta {
    /// Basis points *faster* — ≥ 1 by construction.
    speedup_bp: u32,
}

impl Delta {
    /// Derive a positive speedup from a before/after cost.  `None` when
    /// `after >= before` (no improvement or a regression) or `before`
    /// is zero — the sole numeric ctor, so a non-positive `Delta` has no
    /// inhabitant.
    #[must_use]
    pub fn measured(before: u64, after: u64) -> Option<Self> {
        if after == 0 || after >= before {
            return None;
        }
        // basis points *faster* = (before/after − 1) × 10 000, so a 2×
        // speedup is +100 % = 10 000 bp and a 3× is +200 % = 20 000 bp
        // (unbounded above, unlike a fraction-saved which caps at 100 %).
        let ratio_bp = (u128::from(before) * 10_000) / u128::from(after);
        let bp = u32::try_from(ratio_bp.saturating_sub(10_000)).unwrap_or(u32::MAX);
        Self::from_bp(bp)
    }

    /// Build from an already-computed basis-point speedup.  `None` for
    /// `0` — a zero speedup is not a `Delta`.
    #[must_use]
    pub fn from_bp(speedup_bp: u32) -> Option<Self> {
        (speedup_bp != 0).then_some(Self { speedup_bp })
    }

    /// The basis-point speedup — always > 0.
    #[must_use]
    pub fn speedup_bp(self) -> u32 {
        self.speedup_bp
    }
}

// ── Typed border — the claim axes ──────────────────────────────────

/// The *class* of transformation a lever applies.  The class determines
/// the strongest proof-tier honest for it ([`earned_tier`]).  A specific
/// instance may prove more by hand-construction — that stronger claim the
/// type cannot verify, so soundness stays the parity oracle's (C2).  This
/// is the mislabel escape hatch, stated not hidden: a force-order change
/// *labeled* `ReprSwap` passes `is_honest` while being byte-unsound.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Technique {
    /// Swap a value's storage representation with no observable change.
    /// (#1 this session: NanBox upvalues — `Vec<VMValue>` → `Vec<NanBox>`.)
    ReprSwap,
    /// Drop a traversal/iteration order provably never observed.
    /// (`sui-resolve` M0: a lexical pre-resolution — same binding by
    /// construction, the unobserved with-chain walk dropped.)
    DropUnobservedOrder,
    /// Skip a store/write proven redundant where it would run.
    /// (`redundant-store-elision`, landed 57da0d79.)
    SkipRedundantStore,
    /// Hoist a computation proven invariant across the loop it leaves.
    /// Needs a coupling proof (the hoisted value stays coupled to its
    /// uses).  No lever this session — a future coupling-class technique.
    HoistInvariant,
    /// Change the ORDER thunks are forced.  Needs a force-order proof;
    /// a byte-diff alone is insufficient.
    ForceOrderChange,
    /// Change how name/scope RESOLUTION is computed.  Not byte-provable
    /// over a partial corpus → earns [`ProofTier::Rejected`].  (M1 this
    /// session: `positional-frames`; measured net-negative, discarded.)
    ResolutionChange,
    /// Memoize an idempotent EXTERNAL query (a syscall, an OS lookup)
    /// whose answer is stable under the evaluator's own frozen-inputs
    /// assumption — the same assumption CppNix makes when it copies a
    /// source tree to the store.  The observable value is identical by
    /// that assumption, so a byte-identical corpus fully establishes it
    /// (ByteSufficient) — but the ASSUMPTION must be named in the lever's
    /// record, because it is where the class can go wrong (a tree mutated
    /// mid-eval).  First instance: `canonicalize-memo` (2026-07-21) —
    /// registration-time canonicalization of input read_dirs + a
    /// success-only probe memo, after live-sampling showed 61% of the
    /// eval thread inside `__getattrlist` via `realpath`.
    MemoizeIdempotentQuery,
}

/// What proof a claim asserts is SUFFICIENT for parity — the session's
/// own four-tier parity-proof taxonomy, ordered by how much the claim
/// asserts is safe.  `Ord` follows declaration order (ascending): a
/// weaker sufficiency claim is "less".  `is_honest` requires
/// `claimed <= earned_tier(technique)` — you cannot claim a cheaper proof
/// than the technique's class earns.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProofTier {
    /// changes-resolution — no tractable parity proof; honest only as a
    /// discard, and must name a [`Ceiling`].  (The floor.)
    Rejected,
    /// needs-force-order-proof — a force-order argument is required.
    ForceOrderProof,
    /// needs-coupling-proof — a proof two sites stay coupled is required.
    CouplingProof,
    /// byte-diff-sufficient — a byte-identical corpus diff fully
    /// establishes it (nothing observable can change).  The strongest
    /// sufficiency claim.
    ByteSufficient,
}

/// The lifecycle state of a lever.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeverStatus {
    /// Designed, not merged.
    Proposed,
    /// Merged to main — code exists.  The perf delta may not be isolated
    /// yet; `measured` may be `Pending`.
    Landed,
    /// Merged AND measured a strictly-positive delta.  `is_honest`
    /// requires `measured == Improved` — no null-as-win.
    Proven,
    /// Measured net-negative or unsound — NOT merged (or reverted).  Must
    /// name a regression / no-improvement measurement OR a ceiling.
    Discarded,
    /// Built, but the measurement/decision is deferred (e.g. behind a
    /// flag, measured null → waiting for a real win).
    Deferred,
}

/// The measurement outcome — the AUTHORED kind; the strictly-positive
/// value it implies lives in [`Delta`] (sign truly-unrep).  `Improved` is
/// honest only with a non-zero `speedup_bp`; an `Improved` at 0 bp is the
/// null-measurement mistake, eval-caught.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasuredKind {
    /// Not yet isolated-measured.
    Pending,
    /// A strictly-positive speedup (magnitude in `speedup_bp`).
    Improved,
    /// Measured, but no improvement (null).  (`sui-resolve` M0.)
    NoImprovement,
    /// Measured a slowdown.  (`positional-frames`: +7 %/+32–39 %.)
    Regressed,
}

/// Why a lever cannot reach a stronger proof-tier — a first-class honest
/// ceiling, never rounded away.  Required when
/// `proof_tier == Rejected`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ceiling {
    /// No ceiling — the lever is not tier-capped.
    NotApplicable,
    /// C2 — only the external `sui parity` byte oracle over a PARTIAL
    /// corpus can establish content-soundness; the type gates shape,
    /// never content.
    PartialCorpus,
    /// The ~9× deep-recursion gap is inherent to sui's no-GC persistent-
    /// lazy design (the M2 verdict): a positional-frame/allocation lever
    /// pays more than the probe it removes.
    PersistentLazyDesign,
    /// C1 — safe Rust cannot express `PureFn`; the purity axis stays
    /// runtime-mitigated forever (the `memo.rs` module invariant).
    NoPureFn,
    /// C2 — the measurement is an external-world observation (did eval
    /// get faster AND stay byte-identical) — CI-caught, never a compile
    /// error.
    ExternalObservation,
}

/// One performance lever — a typed, honesty-gated CLAIM about one
/// optimization applied to sui's eval hot path.  Authored as
/// `(defperf-lever …)`.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defperf-lever")]
pub struct PerfLever {
    pub name: String,
    /// The cost sites this lever attacks (bare names at M0; binds to a
    /// `defcost-site` border over the `sui_eval::perf::Counter` registry
    /// at M1).
    pub attacks: Vec<String>,
    pub technique: Technique,
    #[serde(rename = "proofTier")]
    pub proof_tier: ProofTier,
    pub status: LeverStatus,
    pub measured: MeasuredKind,
    /// Basis points *faster* (1 bp = 0.01 %).  Must be > 0 iff
    /// `measured == Improved`, `0` otherwise.  The strictly-positive
    /// [`Delta`] is derived from this via [`PerfLever::delta`], so a
    /// non-positive speedup has no `Delta`.
    #[serde(rename = "speedupBp")]
    pub speedup_bp: u32,
    pub ceiling: Ceiling,
}

/// The strongest proof-tier honest for a technique CLASS.  A specific
/// instance may prove more (hand-construction) — that stronger claim the
/// type cannot verify; soundness stays the parity oracle's (C2).  Total
/// over [`Technique`].
#[must_use]
pub fn earned_tier(technique: Technique) -> ProofTier {
    match technique {
        Technique::ReprSwap
        | Technique::DropUnobservedOrder
        | Technique::SkipRedundantStore
        | Technique::MemoizeIdempotentQuery => ProofTier::ByteSufficient,
        Technique::HoistInvariant => ProofTier::CouplingProof,
        Technique::ForceOrderChange => ProofTier::ForceOrderProof,
        Technique::ResolutionChange => ProofTier::Rejected,
    }
}

/// A specific way a lever's claim is internally dishonest — a typed
/// render surface (its `Display` IS the message, per ★★ TYPED EMISSION;
/// no `format!`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HonestyViolation {
    /// Claimed a proof-tier stronger than the technique's class earns.
    TierOverclaim { claimed: ProofTier, earned: ProofTier },
    /// `Proven` status without an `Improved` measurement (null-as-win).
    ProvenWithoutImprovement,
    /// `Improved` measurement with a zero speedup (null delta).
    ImprovedButZeroDelta,
    /// A non-improvement carrying a non-zero speedup (measurement lie).
    NonImprovementCarriesDelta,
    /// A `Rejected` proof-tier with no ceiling named.
    RejectedWithoutCeiling,
    /// A `Discarded` lever with no stated cause (no regression/null, no
    /// ceiling).
    DiscardedWithoutCause,
}

impl std::fmt::Display for HonestyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TierOverclaim { claimed, earned } => write!(
                f,
                "claimed proof-tier {claimed:?} exceeds the {earned:?} that a technique of this class earns"
            ),
            Self::ProvenWithoutImprovement => {
                write!(f, "status Proven requires a measured Improved (null-as-win)")
            }
            Self::ImprovedButZeroDelta => {
                write!(f, "measured Improved requires a non-zero speedup (null delta)")
            }
            Self::NonImprovementCarriesDelta => {
                write!(f, "a non-Improved measurement must carry a zero speedup")
            }
            Self::RejectedWithoutCeiling => {
                write!(f, "a Rejected proof-tier must name its ceiling")
            }
            Self::DiscardedWithoutCause => write!(
                f,
                "a Discarded lever must state a regression, a null measurement, or a ceiling"
            ),
        }
    }
}

impl PerfLever {
    /// The strictly-positive speedup this lever claims, if any.  `Some`
    /// only when `measured == Improved` AND `speedup_bp > 0` — the sealed
    /// [`Delta`] makes a non-positive "improvement" unconstructable.
    #[must_use]
    pub fn delta(&self) -> Option<Delta> {
        match self.measured {
            MeasuredKind::Improved => Delta::from_bp(self.speedup_bp),
            _ => None,
        }
    }

    /// The first way this lever's claim is internally dishonest, if any —
    /// the eval-caught gate.  The rules, each mapping to a real mistake
    /// class:
    ///   1. claimed proof-tier ≤ `earned_tier(technique)` — no tier
    ///      over-claim (`positional-frames` ResolutionChange claiming
    ///      ByteSufficient fires `TierOverclaim`).
    ///   2. `Proven` ⇒ `Improved` — no null-as-win (`m0-resolver` as
    ///      Proven+NoImprovement fires `ProvenWithoutImprovement`).
    ///   3. `Improved` ⇒ `speedup_bp > 0` — no null delta (the sealed
    ///      `Delta` makes a *negative* one unconstructable, so only the
    ///      null reaches here).
    ///   4. `Rejected` ⇒ a ceiling is named.
    ///   5. `Discarded` ⇒ a regression / null measurement OR a ceiling.
    ///
    /// It does NOT verify parity soundness (C2 oracle), does NOT catch
    /// mis-attribution (wrong engine), and does NOT catch already-landed
    /// drift (M2).
    #[must_use]
    pub fn honesty_violation(&self) -> Option<HonestyViolation> {
        let earned = earned_tier(self.technique);
        if self.proof_tier > earned {
            return Some(HonestyViolation::TierOverclaim {
                claimed: self.proof_tier,
                earned,
            });
        }
        if self.status == LeverStatus::Proven && self.measured != MeasuredKind::Improved {
            return Some(HonestyViolation::ProvenWithoutImprovement);
        }
        if self.measured == MeasuredKind::Improved && self.speedup_bp == 0 {
            return Some(HonestyViolation::ImprovedButZeroDelta);
        }
        if self.measured != MeasuredKind::Improved && self.speedup_bp != 0 {
            return Some(HonestyViolation::NonImprovementCarriesDelta);
        }
        if self.proof_tier == ProofTier::Rejected && self.ceiling == Ceiling::NotApplicable {
            return Some(HonestyViolation::RejectedWithoutCeiling);
        }
        if self.status == LeverStatus::Discarded
            && self.measured == MeasuredKind::Pending
            && self.ceiling == Ceiling::NotApplicable
        {
            return Some(HonestyViolation::DiscardedWithoutCause);
        }
        None
    }

    /// Whether this lever's claim is internally honest — `true` iff
    /// [`honesty_violation`](Self::honesty_violation) finds nothing.
    #[must_use]
    pub fn is_honest(&self) -> bool {
        self.honesty_violation().is_none()
    }
}

// ── Interpreter — the honesty audit over a PerfEnvironment ──────────

/// The side-effecting surface the audit needs: a coarse cost reading.
/// The real impl reads `sui_eval::perf::PerfSnapshot`; tests mock it (the
/// trait IS the testability contract — TYPED-SPEC triplet).  At M0 the
/// reading is provenance only — the per-lever measure-GATE (comparing a
/// reading against a per-counter budget) is M1 (the `perf_seal` `Budget`
/// generalization, not yet built).
pub trait PerfEnvironment {
    /// A coarse cost reading (e.g. the EvalExpr count) — recorded as the
    /// audit's provenance.
    fn cost_reading(&self) -> u64;
}

/// The audit of one lever's claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeverAudit {
    pub name: String,
    /// The coarse cost the environment reported at audit time
    /// (provenance; the per-counter measure-gate is M1).
    pub observed_cost: u64,
}

/// Audit a lever's claim against the environment.  Returns the audit, or
/// a typed `Interp` error naming the dishonest claim — never a silent
/// `Ok` for a dishonest lever (the TYPED-SPEC-TRIPLET rule: no stub-Ok).
///
/// # Errors
/// `SpecError::Interp { phase: "honesty" }` when the lever's claim is
/// internally dishonest (tier over-claim, null-as-win, unnamed ceiling…).
pub fn apply<E: PerfEnvironment>(lever: &PerfLever, env: &E) -> Result<LeverAudit, SpecError> {
    if let Some(violation) = lever.honesty_violation() {
        return Err(SpecError::Interp {
            phase: "honesty".into(),
            message: violation.to_string(),
        });
    }
    Ok(LeverAudit {
        name: lever.name.clone(),
        observed_cost: env.cost_reading(),
    })
}

// ── Canonical Lisp + loader ────────────────────────────────────────

/// The embedded canonical perf-lever ledger.
pub const CANONICAL_PERF_LISP: &str = include_str!("../specs/perf.lisp");

/// Load every authored `(defperf-lever …)`.
///
/// # Errors
/// Fails if the canonical Lisp doesn't parse under the schema.
pub fn load_canonical_levers() -> Result<Vec<PerfLever>, SpecError> {
    crate::loader::load_all::<PerfLever>(CANONICAL_PERF_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock cost source — a fixed coarse reading.
    struct MockPerf {
        cost: u64,
    }
    impl PerfEnvironment for MockPerf {
        fn cost_reading(&self) -> u64 {
            self.cost
        }
    }

    fn lever(name: &str) -> PerfLever {
        load_canonical_levers()
            .unwrap()
            .into_iter()
            .find(|l| l.name == name)
            .unwrap_or_else(|| panic!("canonical lever {name}"))
    }

    // ── The M0 gate ────────────────────────────────────────────────

    #[test]
    fn canonical_lisp_loads_this_sessions_levers() {
        let levers = load_canonical_levers().unwrap();
        // Rounds 1-3: 4 original + overlay-base-move + round-2 (ident-intern
        // Proven, apply-trace-clone + force-roundtrip Discarded) + round-3
        // (select-ident-token + string-concat Proven, attrset-symcache Deferred,
        // overlay-merge-structural Discarded).
        // +1 (2026-07-21): canonicalize-memo — the first MemoizeIdempotentQuery
        // lever (the 61%-in-getattrlist live-sample root; Landed/Pending).
        // +1 (2026-07-21): sym-keyed-attrs — lever 1 of the 20s campaign
        // (interner cluster 27-39% -> <=6.7% measured; Landed/Pending).
        // +1 (2026-07-21): path-memo — L2 tried + honestly Discarded
        // (NoImprovement on two harnesses; residual noted for post-S6).
        assert_eq!(levers.len(), 17, "authored levers");
    }

    #[test]
    fn every_authored_lever_is_honest() {
        // The M0 gate: the corrected forms of this session's four levers
        // are all internally honest.  Green here IS the proof that the
        // null (m0-resolver) and the tier-contradiction (positional-
        // frames) were driven to their honest records, not rounded up.
        for l in load_canonical_levers().unwrap() {
            assert!(
                l.is_honest(),
                "authored lever `{}` is dishonest: {:?}",
                l.name,
                l.honesty_violation()
            );
        }
    }

    // ── Mistake (c): the null measurement, eval-caught ─────────────

    #[test]
    fn null_measurement_recorded_as_proven_is_caught() {
        // m0-resolver measured null.  Authoring it as a Proven win is the
        // mistake — the ledger fires ProvenWithoutImprovement.
        let mut bug = lever("m0-resolver");
        bug.status = LeverStatus::Proven; // claim a win…
        // measured stays NoImprovement → the null-as-win.
        assert_eq!(
            bug.honesty_violation(),
            Some(HonestyViolation::ProvenWithoutImprovement)
        );
    }

    #[test]
    fn improved_with_zero_speedup_is_caught() {
        let mut bug = lever("m0-resolver");
        bug.measured = MeasuredKind::Improved; // claim an improvement…
        bug.speedup_bp = 0; // …with no measured speedup.
        assert_eq!(
            bug.honesty_violation(),
            Some(HonestyViolation::ImprovedButZeroDelta)
        );
    }

    // ── Mistake (d-negative): the regression, truly-unrep on sign ──

    #[test]
    fn a_regression_has_no_delta_inhabitant() {
        // after >= before ⇒ no Delta exists.  Never-ship-a-regression
        // made unrepresentable on the sign axis.
        assert!(Delta::measured(1000, 1300).is_none(), "a slowdown is not a Delta");
        assert!(Delta::measured(1000, 1000).is_none(), "a null is not a Delta");
        assert!(Delta::measured(0, 0).is_none(), "a zero baseline is not a Delta");
        assert!(Delta::from_bp(0).is_none(), "a zero speedup is not a Delta");
        // A real speedup is a Delta: 1000→500 is a 2× speedup = +100 %.
        let d = Delta::measured(1000, 500).expect("2x speedup is a Delta");
        assert_eq!(d.speedup_bp(), 10_000, "2x (1000→500) is +100% = 10_000 bp faster");
        // And a 3× speedup exceeds 100 % (the fraction-saved formula could not).
        assert_eq!(
            Delta::measured(3000, 1000).unwrap().speedup_bp(),
            20_000,
            "3x is +200% = 20_000 bp"
        );
    }

    // ── Mistake (d-contradiction): the tier over-claim, eval-caught ─

    #[test]
    fn a_resolution_change_claiming_byte_sufficiency_is_caught() {
        // positional-frames is a ResolutionChange; that class earns only
        // Rejected over a partial byte corpus.  Claiming ByteSufficient
        // fires TierOverclaim — the tier-contradiction, eval-caught.
        let mut bug = lever("positional-frames");
        bug.proof_tier = ProofTier::ByteSufficient;
        // (also clear the now-redundant ceiling so this fails on the tier,
        //  not the discard rule)
        assert_eq!(
            bug.honesty_violation(),
            Some(HonestyViolation::TierOverclaim {
                claimed: ProofTier::ByteSufficient,
                earned: ProofTier::Rejected,
            })
        );
    }

    #[test]
    fn a_repr_swap_may_honestly_claim_byte_sufficiency() {
        // The dual: a genuine ReprSwap earns ByteSufficient, so the same
        // claim that is dishonest for a ResolutionChange is honest here.
        let nanbox = lever("nanbox-upvalues");
        assert_eq!(nanbox.technique, Technique::ReprSwap);
        assert_eq!(nanbox.proof_tier, ProofTier::ByteSufficient);
        assert!(nanbox.is_honest());
    }

    // ── The interpreter drives a real (mock) environment ───────────

    #[test]
    fn apply_errors_on_a_dishonest_lever_never_silent_ok() {
        let mut bug = lever("positional-frames");
        bug.proof_tier = ProofTier::ByteSufficient; // over-claim
        let env = MockPerf { cost: 100 };
        let err = apply(&bug, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "honesty"),
            other => panic!("expected an Interp honesty error, got {other:?}"),
        }
    }

    #[test]
    fn apply_records_provenance_for_an_honest_lever() {
        let honest = lever("nanbox-upvalues");
        let env = MockPerf { cost: 4_242 };
        let audit = apply(&honest, &env).unwrap();
        assert_eq!(audit.name, "nanbox-upvalues");
        assert_eq!(audit.observed_cost, 4_242, "the env reading is recorded as provenance");
    }

    // ── The honest boundary: what the ledger does NOT reach ────────

    #[test]
    fn mis_attribution_is_out_of_scope_by_construction() {
        // The (a) wrong-engine mistake lives in harness wiring, outside
        // the claim's typed shape.  A lever with an otherwise-honest claim
        // stays honest even if its number was measured on the wrong
        // engine — the ledger cannot see that, and does not pretend to.
        // This test PINS that boundary so the ledger is never over-sold.
        let honest = lever("nanbox-upvalues");
        assert!(honest.is_honest());
        // Nothing in the type records WHICH engine produced a reading; the
        // narrowing `:engine` field is the named M2 addition, not M0.
    }
}
