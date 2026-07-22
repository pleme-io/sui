//! `(defeager-class …)` — the FIRST parity-typed runtime-fluidity knob.
//!
//! sui's headline advantage over cppnix is that eval *strategy* — which
//! shape-classes are forced eagerly instead of lazily — can be a live,
//! declarative conversation instead of a compiled-in constant. This module is
//! the beachhead: a typed `(defeager-class …)` tatara-lisp form + the
//! **enforcement border** that makes the *parity-typed knob-space law* real
//! rather than prose.
//!
//! ## The law (now a border, not a comment)
//!
//! Steering eval eager-vs-lazy reorders *when* work happens. Some reorderings
//! are byte-invisible (an unobserved traversal order, a representation swap);
//! others can change what the program observes (a force-order change) and so
//! could fork a derivation hash. A knob is therefore live-patchable **only if
//! the reordering it performs is byte-safe** — measured by the perf ledger's
//! [`crate::perf::earned_tier`]: exactly `ProofTier::ByteSufficient` earns it.
//! A `ForceOrderChange` / `HoistInvariant` / `ResolutionChange` knob is
//! **REFUSED at [`EagerClassRule::validate`]** until it is corpus-gated.
//!
//! Steering changes *how fast*; structurally, never *the bytes*. That
//! guarantee is what a force-order-shaped knob would break, so the border
//! refuses it by construction — no expressible live knob can change eval bytes.
//!
//! This is the M7 slice of `docs/STRATOSPHERE.md`: the vocabulary + the
//! enforcement border. Wiring the accepted knob through sui-daemon's shikumi
//! `ConfigStore` hot-reload and applying it to live eval are the named
//! follow-ups; the byte-safety border is the load-bearing part and lands here.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::perf::{earned_tier, ProofTier, Technique};
use crate::SpecError;

/// A declarative eager/lazy steering rule over a class of eval shapes.
///
/// Authored as `(defeager-class :name … :shape … :technique … :notes …)`.
/// The `technique` field is not decoration: it is the byte-safety class of the
/// reordering this rule performs, and it is what the enforcement border checks.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defeager-class")]
pub struct EagerClassRule {
    /// Unique rule name.
    pub name: String,
    /// The eval shape-class this steers eager (vs the default lazy). A bare
    /// descriptor at M0 (e.g. `"small-static-attrset-literal"`); binds to a
    /// typed shape border at M1.
    pub shape: String,
    /// The byte-safety class of the reordering the steering performs. The knob
    /// is live-patchable ONLY if this technique earns `ByteSufficient`; a
    /// force-order-shaped technique is REFUSED at the patch border.
    pub technique: Technique,
    /// Free-text rationale — why this shape is safe to force eagerly.
    #[serde(default)]
    pub notes: String,
}

impl EagerClassRule {
    /// The proof-tier this rule's reordering earns, read from the perf ledger.
    #[must_use]
    pub fn earned_tier(&self) -> ProofTier {
        earned_tier(self.technique)
    }

    /// `true` iff this rule may be applied as a LIVE knob. Only a
    /// `ByteSufficient` reordering is byte-safe to hot-patch; every weaker tier
    /// (`CouplingProof` / `ForceOrderProof` / `Rejected`) could change eval
    /// bytes and is not live-patchable until corpus-gated.
    #[must_use]
    pub fn is_byte_patchable(&self) -> bool {
        self.earned_tier() == ProofTier::ByteSufficient
    }

    /// The enforcement border. Accepts a byte-safe rule; **REFUSES** a
    /// force-order-shaped one with a typed error — the point at which the
    /// parity-typed knob law stops being prose and becomes a checked boundary.
    ///
    /// # Errors
    ///
    /// Returns [`SpecError::Interp`] if the rule's technique is not
    /// `ByteSufficient` (it could change eval bytes, so it cannot ship as a
    /// live knob).
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.is_byte_patchable() {
            Ok(())
        } else {
            Err(SpecError::Interp {
                phase: "eager_class::validate".to_string(),
                message: format!(
                    "eager-class rule `{}` REFUSED: technique {:?} earns {:?}, not ByteSufficient — a \
                     force-order-shaped knob can change eval bytes, so it is not live-patchable until \
                     corpus-gated",
                    self.name,
                    self.technique,
                    self.earned_tier()
                ),
            })
        }
    }
}

const CANONICAL_EAGER_CLASS_LISP: &str = include_str!("../specs/eager_class.lisp");

/// Load the canonical eager-class rules from the authored spec.
///
/// # Errors
///
/// Returns an error if the spec fails to parse.
pub fn load_canonical() -> Result<Vec<EagerClassRule>, SpecError> {
    crate::loader::load_all::<EagerClassRule>(CANONICAL_EAGER_CLASS_LISP)
}

#[cfg(test)]
mod tests {
    use super::{load_canonical, EagerClassRule};
    use crate::perf::{ProofTier, Technique};
    use crate::SpecError;

    fn rule(name: &str, technique: Technique) -> EagerClassRule {
        EagerClassRule {
            name: name.to_string(),
            shape: "test-shape".to_string(),
            technique,
            notes: String::new(),
        }
    }

    #[test]
    fn bytesufficient_techniques_are_patchable_and_validate() {
        for t in [
            Technique::ReprSwap,
            Technique::DropUnobservedOrder,
            Technique::SkipRedundantStore,
            Technique::MemoizeIdempotentQuery,
        ] {
            let r = rule("ok", t);
            assert_eq!(r.earned_tier(), ProofTier::ByteSufficient, "{t:?} earns ByteSufficient");
            assert!(r.is_byte_patchable(), "{t:?} must be byte-patchable");
            assert!(r.validate().is_ok(), "{t:?} must validate");
        }
    }

    #[test]
    fn force_order_and_weaker_knobs_are_refused_at_the_border() {
        // Every technique whose reordering is NOT byte-diff-sufficient must be
        // refused — the law made a border: no live knob can change eval bytes.
        for t in [
            Technique::ForceOrderChange, // ForceOrderProof
            Technique::HoistInvariant,   // CouplingProof
            Technique::ResolutionChange, // Rejected
        ] {
            let r = rule("bad", t);
            assert!(!r.is_byte_patchable(), "{t:?} must NOT be byte-patchable");
            let err = r.validate().expect_err("a non-ByteSufficient eager-class must be REFUSED");
            assert!(matches!(err, SpecError::Interp { .. }), "typed refusal, got {err:?}");
        }
    }

    #[test]
    fn canonical_specs_parse_and_every_authored_rule_is_byte_safe() {
        let rules = load_canonical().expect("eager_class canonical specs must compile");
        assert!(!rules.is_empty(), "at least one authored eager-class rule");
        for r in &rules {
            // The catalog itself cannot ship a force-order knob: every AUTHORED
            // rule passes the same border a live patch would.
            r.validate()
                .unwrap_or_else(|e| panic!("authored eager-class `{}` is not byte-safe: {e:?}", r.name));
        }
    }
}
