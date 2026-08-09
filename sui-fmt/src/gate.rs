//! The canonical-source gate: one door from Nix TEXT to an evaluable tree.
//!
//! # What this is for
//!
//! The stated destination is "sui cannot evaluate non-canonical Nix that we
//! authored". Note the scope, because the shorter sentence people reach for —
//! *"sui can never run Nix that is not perfectly formatted"* — is **false and
//! must never be written down**: sui has to evaluate nixpkgs, 41k+ foreign
//! files it does not own and must never reformat. A gate without a scope
//! bricks every consumer.
//!
//! # Why enforcement is OFF today, and what turns it on
//!
//! Deciding canonicality means computing `format(src) == src`, which requires
//! byte parity with `nixfmt --strict` (RFC 166, the form the fleet adopted).
//! Measured parity is **31.0%** (93/300 corpus sample). Gating on that would
//! reject ~69% of *correctly formatted* files — worse than no gate, because it
//! teaches people to disable it.
//!
//! So [`Policy`] ships as [`Policy::Off`] and the flip is a one-line change
//! once parity clears the bar. This is deliberately a typed field rather than
//! absent code (MODULARIZE, DON'T DELETE): the mechanism is complete and
//! exercised by tests today, so turning it on is a decision rather than a
//! rebuild from memory.
//!
//! # Tier, stated honestly
//!
//! With `Policy::RejectAuthored` this is **parse-time-rejected** — a
//! `Result::Err` at a chokepoint — NOT unrepresentability. A caller inside
//! this crate can still reach `rnix::Root::parse` directly. Making that
//! unrepresentable means removing **both** `rnix` AND `rowan` from every
//! consumer crate's manifest so `Root::parse` is not nameable there (E0433);
//! `rowan` alone is enough to reach `Expr::cast` and get an evaluable tree, so
//! removing only `rnix` would be a tier round-up. That is a later, separate
//! change and is NOT claimed here.

use crate::is_canonical;

/// Where a piece of Nix source came from. **The gate is scoped on this**, and
/// getting it wrong in the permissive direction silently disables the gate,
/// while getting it wrong in the strict direction bricks evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceOrigin {
    /// Source in a tree we own and format. The ONLY origin the gate judges.
    Authored,
    /// Already-realized input under the store. Immutable by construction and
    /// frequently foreign; reformatting it would move NAR hashes.
    Store,
    /// Fetched from a flake input — someone else's repo, someone else's style.
    FlakeInput,
    /// sui's own built-in Nix (corepkgs). Ships inside the binary.
    Corepkgs,
}

impl SourceOrigin {
    /// Is this origin subject to the canonicality rule at all?
    ///
    /// Written as an explicit match rather than `== Authored` so a NEW origin
    /// variant fails to compile until someone decides which side it is on.
    /// A default arm here would silently admit every future origin.
    #[must_use]
    pub fn is_ours(self) -> bool {
        match self {
            SourceOrigin::Authored => true,
            SourceOrigin::Store | SourceOrigin::FlakeInput | SourceOrigin::Corepkgs => false,
        }
    }
}

/// What the gate does about non-canonical authored source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Policy {
    /// Parse only. **The shipping default** — see the module docs on parity.
    #[default]
    Off,
    /// Accept, but hand back the verdict so a caller can warn. Nothing in the
    /// evaluator is obliged to look, which is exactly why this is not a gate.
    WarnAuthored,
    /// Refuse non-canonical AUTHORED source. Foreign origins still pass.
    RejectAuthored,
}

/// A tree that has passed the door.
///
/// The field is private and there is no public constructor other than
/// [`parse_canonical`], so within any crate that consumes `sui-fmt` there is
/// no way to obtain one without going through the gate.
#[derive(Debug, Clone)]
pub struct CanonicalTree {
    root: rnix::Root,
    origin: SourceOrigin,
    /// `None` when the policy did not ask. Distinguishes "checked and
    /// canonical" from "never checked" — collapsing those into `false` is how
    /// an un-run gate reads as a passing one.
    canonical: Option<bool>,
}

impl CanonicalTree {
    #[must_use]
    pub fn tree(&self) -> &rnix::Root {
        &self.root
    }

    #[must_use]
    pub fn origin(&self) -> SourceOrigin {
        self.origin
    }

    /// `Some(true)`/`Some(false)` when the policy evaluated canonicality;
    /// `None` when it did not ask.
    #[must_use]
    pub fn canonical(&self) -> Option<bool> {
        self.canonical
    }
}

/// Why text did not become a tree.
#[derive(Debug)]
pub enum GateError {
    /// rnix reported parse errors. Same class sui already surfaces today.
    Parse(String),
    /// Authored source that is not in canonical form, under a policy that
    /// refuses it.
    NotCanonical { path: String },
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateError::Parse(m) => write!(f, "{m}"),
            GateError::NotCanonical { path } => write!(
                f,
                "{path} is not canonically formatted.\n\
                 This is source we author, and sui evaluates only canonical \
                 authored Nix.\n\
                 Fix it with:  sui fmt {path}\n\
                 (Canonical form is RFC 166 — `nixfmt --strict`. Files under \
                 /nix/store and flake inputs are never judged by this rule.)"
            ),
        }
    }
}

/// **The one door.** Nix text in, an evaluable tree out — or a refusal.
///
/// `path` is used only for the error message; pass the canonical path so a
/// refusal names something the operator can act on.
///
/// # Errors
/// [`GateError::Parse`] when rnix reports errors; [`GateError::NotCanonical`]
/// when an authored file fails the canonicality rule under a refusing policy.
pub fn parse_canonical(
    src: &str,
    path: &str,
    origin: SourceOrigin,
    policy: Policy,
) -> Result<CanonicalTree, GateError> {
    let parsed = rnix::Root::parse(src);
    let errors = parsed.errors();
    if !errors.is_empty() {
        return Err(GateError::Parse(
            errors
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    // Canonicality is computed ONLY when a policy asks and the origin is ours.
    // `is_canonical` re-renders the whole file, so doing it unconditionally
    // would put a formatter in the hot path of every import in a nixpkgs-scale
    // closure — for a verdict nobody reads.
    let canonical = if matches!(policy, Policy::Off) || !origin.is_ours() {
        None
    } else {
        Some(is_canonical(src))
    };

    if matches!(policy, Policy::RejectAuthored) && canonical == Some(false) {
        return Err(GateError::NotCanonical {
            path: path.to_string(),
        });
    }

    Ok(CanonicalTree {
        root: parsed.tree(),
        origin,
        canonical,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Canonical per OUR formatter — and it agrees with `nixfmt --strict`
    // here: a single binding stays flat. The first draft used the multi-line
    // spelling, which our own `is_canonical` rejects; the test caught it,
    // which is the parity gap showing up as a failing assertion rather than
    // as a silently-wrong gate.
    const CANON: &str = "{ a = 1; }\n";
    const MESSY: &str = "{   a=1;   }";

    fn assert_messy_is_actually_messy() {
        assert!(!is_canonical(MESSY), "fixture must be non-canonical");
    }

    /// The shipping default changes nothing. If this ever fails, the gate was
    /// turned on without the parity work — and ~69% of correct files break.
    #[test]
    fn default_policy_is_off() {
        assert_eq!(Policy::default(), Policy::Off);
    }

    #[test]
    fn off_admits_non_canonical_authored_source() {
        let t = parse_canonical(MESSY, "f.nix", SourceOrigin::Authored, Policy::Off)
            .expect("Off must not refuse");
        assert_eq!(
            t.canonical(),
            None,
            "Off must not even COMPUTE canonicality — that is the hot path cost"
        );
    }

    /// The gate's whole reason to exist.
    #[test]
    fn reject_refuses_non_canonical_authored_source() {
        assert_messy_is_actually_messy();
        let e = parse_canonical(MESSY, "f.nix", SourceOrigin::Authored, Policy::RejectAuthored)
            .expect_err("must refuse");
        assert!(matches!(e, GateError::NotCanonical { .. }));
        assert!(e.to_string().contains("sui fmt f.nix"), "must name the fix");
    }

    #[test]
    fn reject_admits_canonical_authored_source() {
        let t = parse_canonical(CANON, "f.nix", SourceOrigin::Authored, Policy::RejectAuthored)
            .expect("canonical source must pass");
        assert_eq!(t.canonical(), Some(true));
    }

    /// **The nixpkgs guarantee.** Every foreign origin passes under the
    /// strictest policy, with the same input the authored arm refuses. If this
    /// ever fails, sui cannot evaluate nixpkgs and nothing works.
    #[test]
    fn foreign_origins_are_never_judged_even_under_reject() {
        assert_messy_is_actually_messy();
        for origin in [
            SourceOrigin::Store,
            SourceOrigin::FlakeInput,
            SourceOrigin::Corepkgs,
        ] {
            let t = parse_canonical(MESSY, "f.nix", origin, Policy::RejectAuthored)
                .unwrap_or_else(|e| panic!("{origin:?} must never be refused: {e}"));
            assert_eq!(t.canonical(), None, "{origin:?} must not be judged at all");
        }
    }

    /// Anti-vacuity: the gate must be able to FAIL. A rule that admits its own
    /// counter-example is not a rule.
    #[test]
    fn the_gate_is_falsifiable() {
        let refused =
            parse_canonical(MESSY, "f.nix", SourceOrigin::Authored, Policy::RejectAuthored).is_err();
        let admitted =
            parse_canonical(CANON, "f.nix", SourceOrigin::Authored, Policy::RejectAuthored).is_ok();
        assert!(refused && admitted, "gate must separate the two cases");
    }

    /// Warn reports the verdict without refusing — so a caller that ignores it
    /// is not accidentally protected.
    #[test]
    fn warn_reports_but_does_not_refuse() {
        let t = parse_canonical(MESSY, "f.nix", SourceOrigin::Authored, Policy::WarnAuthored)
            .expect("Warn must not refuse");
        assert_eq!(t.canonical(), Some(false));
    }

    /// A parse error is a parse error under EVERY policy — the canonicality
    /// rule must never mask or replace it.
    #[test]
    fn parse_errors_survive_every_policy() {
        for policy in [Policy::Off, Policy::WarnAuthored, Policy::RejectAuthored] {
            let e = parse_canonical("let x = ; in x", "f.nix", SourceOrigin::Authored, policy)
                .expect_err("must not parse");
            assert!(matches!(e, GateError::Parse(_)), "{policy:?}");
        }
    }

    /// `is_ours` is exhaustive by construction; this pins the split so a new
    /// variant cannot be quietly added to the permissive side.
    #[test]
    fn only_authored_is_ours() {
        assert!(SourceOrigin::Authored.is_ours());
        assert!(!SourceOrigin::Store.is_ours());
        assert!(!SourceOrigin::FlakeInput.is_ours());
        assert!(!SourceOrigin::Corepkgs.is_ours());
    }
}
