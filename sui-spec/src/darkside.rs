//! Dark-side optimization — the typed catalog of byte-risky performance levers.
//!
//! A **dark-side optimization** is a perf change that is NOT observable-equivalent
//! by construction: it can perturb the answer under some demand order, resolution
//! path, partial-value shape, or finalization lifetime, so its correctness rests on
//! the external byte-oracle (cppnix) over a partial corpus — never a structural
//! guarantee. This module makes that discipline a *typed catalog*: each lever is a
//! row carrying its perturbation axis, byte-risk, gating method, and promotion
//! status, and the honesty gate ([`DarkSideLever::honesty_violation`]) makes the
//! two mistake-classes learned the hard way unrepresentable-as-honest:
//!
//! 1. a byte-RISKY lever claiming `ByteSafe` (a tier-overclaim), and
//! 2. a `Promoted` lever missing its evidence (a `Verified`-tier gate, a named
//!    runaway backstop, and a named residual ceiling).
//!
//! Tier-honest: this is the **parse-time-rejected** tier, not truly-unrepresentable.
//! A flat serde/tatara-lisp-authored struct *can* be constructed in a dishonest
//! state; `honesty_violation` REJECTS it (`apply` returns `SpecError::Interp`), the
//! same honest middle tier `perf.rs` uses. The aspirational typestate (a `Promoted`
//! constructor that demands its witnesses) is a named follow-up.
//!
//! Extends `perf.rs` (Operating Principle #1 — extend the near-miss, don't fork):
//! reuses [`Technique`], [`Ceiling`], and [`earned_tier`]; adds the byte-risk /
//! gating / promotion axes the perf ledger doesn't carry. Companion doctrine:
//! `docs/DARK-SIDE-DESIGN.md`; the reusable method: the `dark-side-optimization`
//! skill.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::perf::{earned_tier, Ceiling, ProofTier, Technique};
use crate::SpecError;

/// The single observable a lever CAN perturb. The first two axes are byte-SAFE
/// candidates (a representation/redundant-write change *may* be observable-
/// equivalent by construction); the last four are **always** byte-risky — a change
/// to force-order, name resolution, partial-value shape, or object lifetime can
/// change the answer, so declaring one of them `ByteSafe` is a tier-overclaim.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerturbationAxis {
    /// Storage layout only — the `Value` word, a HAMT node, an arena handle.
    Representation,
    /// Elide a provably-redundant store / collapse intermediate COWs.
    RedundantWrite,
    /// WHICH / WHEN thunks are forced (eager eval, elision).
    ForceOrder,
    /// HOW a name/scope resolves (positional frames, source-id caches).
    Resolution,
    /// The shape of a fixpoint-partial value (Blackhole-as-empty-attrs).
    PartialShape,
    /// Finalization timing / object lifetime (arena, custom drop).
    Lifetime,
}

impl PerturbationAxis {
    /// `true` iff a change on this axis is ALWAYS byte-risky (can change the
    /// answer). A `ByteSafe` claim on an always-risky axis is a tier-overclaim.
    #[must_use]
    pub fn is_always_risky(self) -> bool {
        matches!(
            self,
            PerturbationAxis::ForceOrder
                | PerturbationAxis::Resolution
                | PerturbationAxis::PartialShape
                | PerturbationAxis::Lifetime
        )
    }
}

/// The honest byte-risk tier. `ByteSafe` = observable-equivalent by construction
/// (only ever honest on `Representation`/`RedundantWrite` AND only when the
/// technique earns `ByteSufficient`). `ByteRisky` = conditionally correct;
/// correctness rests on the external oracle.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRisk {
    ByteSafe,
    ByteRisky,
}

/// How a lever's byte-neutrality is CHECKED. Strength descends top→bottom; a
/// byte-RISKY lever promoted to default REQUIRES the strongest gate
/// (`DifferentialOracle`), and a `SingleByteCheck` is sufficient ONLY for a
/// byte-SAFE lever.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatingMethod {
    /// Run BOTH paths, byte-diff every observable over the corpus. The strongest.
    DifferentialOracle,
    /// Run both paths in-process; the fast path is authoritative ONLY on match.
    VerifyMode,
    /// Sampled production, auto-backoff on a divergence budget.
    ShadowCanary,
    /// Oracle-free relations — a BACKSTOP only, never the primary gate.
    Metamorphic,
    /// One byte-compare — sufficient ONLY for a byte-SAFE lever.
    SingleByteCheck,
}

/// The promotion rung, `dark → default`. Flat (authorable in tatara-lisp); the
/// evidence a `Promoted` rung requires is carried by the lever's `gate`,
/// `backstop`, and `ceiling` fields and enforced by [`DarkSideLever::honesty_violation`]
/// (parse-time-rejected, not a typestate the constructor demands).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionStatus {
    /// Behind a `SUI_*` flag, OFF by default, zero-cost when unset.
    DarkGated,
    /// A positive delta measured; not yet corpus-verified across the risky path.
    Measured,
    /// Corpus-clean across the risky path — the witness `Promoted` requires.
    Verified,
    /// Default-ON. Requires a `Verified`-grade gate + a named backstop + a ceiling.
    Promoted,
    /// Measured neutral/negative on the sacred path — never-ship-a-regression.
    Discarded,
    /// A `ForceOrder`/`Resolution` change over a partial corpus — no tractable
    /// proof; honest only as a discard, and must name a ceiling.
    Rejected,
}

/// One dark-side lever — a typed, honesty-gated CLAIM about one byte-risky (or
/// candidate byte-safe) optimization. Authored as `(defdarkside-lever …)`.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defdarkside-lever")]
pub struct DarkSideLever {
    pub name: String,
    /// The `SUI_*` flag that gates it (OFF by default). Empty only for a lever
    /// not yet prototyped.
    #[serde(default)]
    pub flag: String,
    /// The perf technique class (maps to `earned_tier` — the perf.rs seam).
    pub technique: Technique,
    /// The observable this lever CAN perturb.
    pub axis: PerturbationAxis,
    /// The declared byte-risk tier.
    #[serde(rename = "byteRisk")]
    pub byte_risk: ByteRisk,
    /// The cost site it attacks (a bare dhat/profile term at M0).
    pub attacks: String,
    /// MEASURED cost share, 0.0–1.0 (`None` = unmeasured, never rounded).
    #[serde(rename = "costShare", default)]
    pub cost_share: Option<f32>,
    /// How byte-neutrality is checked.
    pub gate: GatingMethod,
    /// The promotion rung.
    pub status: PromotionStatus,
    /// The runaway/kill-switch backstop that catches the un-sampled demand tail.
    /// REQUIRED (non-empty) for a `Promoted` lever.
    #[serde(default)]
    pub backstop: String,
    /// The residual ceiling a green gate does NOT round past. REQUIRED
    /// (`!= NotApplicable`) for a `Promoted` or `Rejected` lever.
    pub ceiling: Ceiling,
}

/// A specific way a lever's claim is internally dishonest.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DarkHonesty {
    /// Declared `ByteSafe` on an always-risky axis, or with a technique that
    /// cannot earn `ByteSufficient`.
    TierOverclaim,
    /// A byte-RISKY lever gated only by a single byte-check.
    RiskyGatedBySingleCheck,
    /// A byte-RISKY lever promoted without a differential-oracle gate.
    RiskyPromotedWithoutDifferential,
    /// A `Promoted` lever with no named runaway backstop.
    PromotedWithoutBackstop,
    /// A `Promoted` lever that names no residual ceiling.
    PromotedWithoutCeiling,
    /// A `Promoted` lever whose status is not backed by a `Verified`-grade gate.
    PromotedOnWeakGate,
    /// A `Rejected` lever that names no ceiling (mirrors perf.rs).
    RejectedWithoutCeiling,
}

impl DarkSideLever {
    /// The proof-tier the technique earns, read from the perf ledger.
    #[must_use]
    pub fn earned_tier(&self) -> ProofTier {
        earned_tier(self.technique)
    }

    /// The honesty check — the parity-typed boundary made a checked border.
    /// Returns `Some(violation)` iff the lever's claim is internally dishonest.
    #[must_use]
    pub fn honesty_violation(&self) -> Option<DarkHonesty> {
        // A ByteSafe claim is honest ONLY on a never-always-risky axis AND only
        // when the technique itself earns ByteSufficient.
        if self.byte_risk == ByteRisk::ByteSafe
            && (self.axis.is_always_risky() || self.earned_tier() != ProofTier::ByteSufficient)
        {
            return Some(DarkHonesty::TierOverclaim);
        }
        if self.byte_risk == ByteRisk::ByteRisky && self.gate == GatingMethod::SingleByteCheck {
            return Some(DarkHonesty::RiskyGatedBySingleCheck);
        }
        // The Promoted (default-on) evidence — a Verified-grade differential gate,
        // a named runaway backstop, a named residual ceiling — is required ONLY of a
        // byte-RISKY lever. A byte-SAFE promotion is observable-equivalent BY
        // CONSTRUCTION (proven once by a byte-check), so it carries no residual risk
        // and needs no backstop/ceiling.
        if self.status == PromotionStatus::Promoted && self.byte_risk == ByteRisk::ByteRisky {
            if self.gate == GatingMethod::Metamorphic {
                return Some(DarkHonesty::PromotedOnWeakGate);
            }
            if self.gate != GatingMethod::DifferentialOracle {
                return Some(DarkHonesty::RiskyPromotedWithoutDifferential);
            }
            if self.backstop.trim().is_empty() {
                return Some(DarkHonesty::PromotedWithoutBackstop);
            }
            if self.ceiling == Ceiling::NotApplicable {
                return Some(DarkHonesty::PromotedWithoutCeiling);
            }
        }
        if self.status == PromotionStatus::Rejected && self.ceiling == Ceiling::NotApplicable {
            return Some(DarkHonesty::RejectedWithoutCeiling);
        }
        None
    }

    /// `true` iff the lever's claim is internally honest.
    #[must_use]
    pub fn is_honest(&self) -> bool {
        self.honesty_violation().is_none()
    }
}

const CANONICAL_DARKSIDE_LISP: &str = include_str!("../specs/darkside.lisp");

/// Load the canonical dark-side lever catalog from the authored spec.
///
/// # Errors
///
/// Returns an error if the spec fails to parse OR if any authored lever's claim
/// is internally dishonest (the catalog cannot ship a dishonest row).
pub fn load_canonical() -> Result<Vec<DarkSideLever>, SpecError> {
    let levers = crate::loader::load_all::<DarkSideLever>(CANONICAL_DARKSIDE_LISP)?;
    for lever in &levers {
        if let Some(v) = lever.honesty_violation() {
            return Err(SpecError::Interp {
                phase: "darkside::honesty".to_string(),
                message: format!(
                    "dark-side lever `{}` REFUSED: {:?} (axis {:?}, byte-risk {:?}, gate {:?}, \
                     status {:?}, earned tier {:?})",
                    lever.name,
                    v,
                    lever.axis,
                    lever.byte_risk,
                    lever.gate,
                    lever.status,
                    lever.earned_tier()
                ),
            });
        }
    }
    Ok(levers)
}

#[cfg(test)]
mod tests {
    use super::{
        load_canonical, ByteRisk, DarkHonesty, DarkSideLever, GatingMethod, PerturbationAxis,
        PromotionStatus,
    };
    use crate::perf::{Ceiling, Technique};

    fn lever(axis: PerturbationAxis, byte_risk: ByteRisk, technique: Technique) -> DarkSideLever {
        DarkSideLever {
            name: "t".into(),
            flag: "SUI_T".into(),
            technique,
            axis,
            byte_risk,
            attacks: "x".into(),
            cost_share: None,
            gate: GatingMethod::DifferentialOracle,
            status: PromotionStatus::DarkGated,
            backstop: String::new(),
            ceiling: Ceiling::PartialCorpus,
        }
    }

    #[test]
    fn bytesafe_on_a_risky_axis_is_a_tier_overclaim() {
        let l = lever(PerturbationAxis::ForceOrder, ByteRisk::ByteSafe, Technique::ReprSwap);
        assert_eq!(l.honesty_violation(), Some(DarkHonesty::TierOverclaim));
    }

    #[test]
    fn bytesafe_with_a_non_bytesufficient_technique_is_a_tier_overclaim() {
        // ResolutionChange earns Rejected, so it can never be ByteSafe.
        let l = lever(
            PerturbationAxis::Representation,
            ByteRisk::ByteSafe,
            Technique::ResolutionChange,
        );
        assert_eq!(l.honesty_violation(), Some(DarkHonesty::TierOverclaim));
    }

    #[test]
    fn bytesafe_repr_swap_on_representation_is_honest() {
        let l = lever(
            PerturbationAxis::Representation,
            ByteRisk::ByteSafe,
            Technique::ReprSwap,
        );
        assert!(l.is_honest(), "{:?}", l.honesty_violation());
    }

    #[test]
    fn risky_gated_by_single_check_is_caught() {
        let mut l = lever(PerturbationAxis::Resolution, ByteRisk::ByteRisky, Technique::ResolutionChange);
        l.gate = GatingMethod::SingleByteCheck;
        assert_eq!(l.honesty_violation(), Some(DarkHonesty::RiskyGatedBySingleCheck));
    }

    #[test]
    fn promoted_without_backstop_is_caught() {
        let mut l = lever(PerturbationAxis::Representation, ByteRisk::ByteRisky, Technique::ReprSwap);
        l.status = PromotionStatus::Promoted;
        l.gate = GatingMethod::DifferentialOracle;
        l.backstop = String::new();
        assert_eq!(l.honesty_violation(), Some(DarkHonesty::PromotedWithoutBackstop));
    }

    #[test]
    fn promoted_risky_without_differential_is_caught() {
        let mut l = lever(PerturbationAxis::ForceOrder, ByteRisk::ByteRisky, Technique::ForceOrderChange);
        l.status = PromotionStatus::Promoted;
        l.gate = GatingMethod::VerifyMode;
        l.backstop = "runaway-force-depth".into();
        assert_eq!(
            l.honesty_violation(),
            Some(DarkHonesty::RiskyPromotedWithoutDifferential)
        );
    }

    #[test]
    fn a_fully_evidenced_promotion_is_honest() {
        let mut l = lever(PerturbationAxis::Representation, ByteRisk::ByteRisky, Technique::ReprSwap);
        l.status = PromotionStatus::Promoted;
        l.gate = GatingMethod::DifferentialOracle;
        l.backstop = "ir-fallback-to-walker".into();
        l.ceiling = Ceiling::PartialCorpus;
        assert!(l.is_honest(), "{:?}", l.honesty_violation());
    }

    #[test]
    fn a_bytesafe_promotion_needs_no_backstop_or_ceiling() {
        // A byte-SAFE change shipped as default is observable-equivalent by
        // construction — it carries no residual risk, so no backstop/ceiling.
        let mut l = lever(PerturbationAxis::RedundantWrite, ByteRisk::ByteSafe, Technique::SkipRedundantStore);
        l.status = PromotionStatus::Promoted;
        l.gate = GatingMethod::SingleByteCheck;
        l.backstop = String::new();
        l.ceiling = Ceiling::NotApplicable;
        assert!(l.is_honest(), "byte-safe promotion should be honest: {:?}", l.honesty_violation());
    }

    #[test]
    fn canonical_catalog_loads_and_every_row_is_honest() {
        let levers = load_canonical().expect("canonical dark-side catalog must load + be honest");
        assert!(levers.len() >= 3, "expected the catalog to carry the M0 levers");
        for l in &levers {
            assert!(l.is_honest(), "row `{}` dishonest: {:?}", l.name, l.honesty_violation());
        }
    }
}
