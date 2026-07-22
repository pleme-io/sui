//! `(defnix-surface …)` — the typed board of "every possible nix use case".
//!
//! "Test every nix use case" is not one flat corpus; it is the disjoint union of
//! five **surfaces** — the language (S1), whole-closure byte-parity (S2), the CLI
//! contract (S4), config/daemon/PATH (S5/S6/S7), and the twelve-class perf matrix
//! (U-perf). This module is the *surface axis* of the STRATOSPHERE use-case
//! vocabulary (`docs/STRATOSPHERE.md` §2, Altitude 1): a second instance of the
//! shipped `catalog.rs` meta-catalog pattern lifted one level — where
//! `SubstrateDomain` enumerates domains *inside* sui-spec, `NixSurface` enumerates
//! surfaces *of nix*.
//!
//! ## What it proves — and what it deliberately does NOT
//!
//! The `Surface` enum is **compile-exhaustive**: `surfaces_are_complete` matches on
//! every variant with no `_ =>` arm, so a new surface cannot be added to the enum
//! without a board row (truly-unrepresentable on the surface axis). Each row then
//! carries a **tier claim honesty-gated** against what its declared machinery earns
//! ([`earned_tier`]) — the `perf.rs` never-round-up seal, applied to coverage.
//!
//! It proves *sui-internal* surface enumeration + row honesty. It does **NOT** prove
//! sui-vs-nix behavioral soundness: [`SurfaceTier::ParityWired`] means "a live oracle
//! is bound to the covered rows", never "proven equal to nix" (that is a C2
//! external-oracle observation, forever runtime/CI, never a type — see
//! `coverage_at_100.rs`, where a `Working` command silently diverged from nix). The
//! type is named `ParityWired`, not `ParityProven`, so the distinction cannot be
//! rounded away.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

/// The five surfaces (seven sub-surfaces) whose union is "every possible nix use
/// case". A closed set — `surfaces_are_complete` refuses a variant with no board row.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// S1 — the Nix language: grammar × operators × ~120 builtins × error paths.
    S1Language,
    /// S2/S3 — whole-closure instantiation + realization (per-node ATerm/NAR byte-diff).
    S2Closure,
    /// S4 — the CLI contract: every subcommand × flag × JSON shape × exit code.
    S4Cli,
    /// S5 — config: `nix.conf` / `NIX_CONFIG` / `--option` / `NIX_PATH` parsing.
    S5Config,
    /// S6 — the daemon worker protocol, server side (nix clients connecting to sui).
    S6Daemon,
    /// S7 — drop-in PATH: `nix` + legacy `nix-*` entrypoints resolve to sui shims.
    S7Path,
    /// U — the twelve-class performance matrix (U01–U12), each at its gate tier.
    UPerf,
}

impl Surface {
    /// Every surface. Hand-written (sui-spec has no `allvariants` derive); the
    /// exhaustive match in `surfaces_are_complete` is the real compile-tier gate.
    pub const ALL: &'static [Surface] = &[
        Surface::S1Language,
        Surface::S2Closure,
        Surface::S4Cli,
        Surface::S5Config,
        Surface::S6Daemon,
        Surface::S7Path,
        Surface::UPerf,
    ];

    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Surface::S1Language => "S1Language",
            Surface::S2Closure => "S2Closure",
            Surface::S4Cli => "S4Cli",
            Surface::S5Config => "S5Config",
            Surface::S6Daemon => "S6Daemon",
            Surface::S7Path => "S7Path",
            Surface::UPerf => "UPerf",
        }
    }
}

/// What sui-side surface a row's coverage gate reflects. Most reflect **sui's own**
/// declared surface (not cppnix) — the honest limit baked into the type.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reflects {
    /// The `sui_eval` builtin registry keys (S1).
    BuiltinRegistry,
    /// The `Commands::` enum scanned from `main.rs` — **sui's own**, not nix's (S4).
    SuiCommandsEnum,
    /// The `worker_protocol.lisp` authored opcode set, name-bridged to the wire enum (S6).
    WireOpcodeCatalog,
    /// `nix show-config --json` keys (S5) — one of the two genuinely cppnix-side gates.
    NixShowConfigKeys,
    /// A pinned cppnix `bin/` listing (S7) — also genuinely cppnix-side.
    CppnixBinListing,
    /// The `BuildClosure::compute` node set (S2).
    ClosureWalk,
    /// The authored `(defuse-case)` set (U-perf) — self-referential, honestly labelled.
    UseCaseCatalog,
    /// No reflection source wired yet.
    None,
}

/// What a covered row on this surface is checked against.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Oracle {
    ByteParity,
    ExpFixture,
    ParityCheck,
    RealClient,
    PathResolve,
    PerfSeal,
    Absent,
}

/// The maturity a surface's coverage machinery has reached. `Ord` follows
/// declaration order (ascending) so `claimed <= earned` is a comparison — the
/// `ProofTier` twin from `perf.rs`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SurfaceTier {
    /// No rows, no gate.
    Absent,
    /// Designed; the `(def…)` form exists but no bijection gate against a real surface.
    Design,
    /// A bijection gate enumerates the surface's items (a missing row fails the build),
    /// but no live oracle is bound.
    Enumerated,
    /// Enumerated AND a live oracle is bound to the covered rows. **NOT** "proven equal
    /// to nix" — that is C2, forever external. The name is `ParityWired`, not `Proven`.
    ParityWired,
}

/// A first-class blocker that caps a surface's reachable tier — the `perf::Ceiling` twin.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceBlocker {
    None,
    /// U10 whole-closure eval swap-deaths at cid scale; S2 cannot reach `ParityWired` there.
    MemoryWall,
    /// No `nix.conf`/`NIX_PATH` parser exists yet (S5).
    NoParser,
    /// Legacy `nix-*` entrypoints unshimmed (S7).
    NoShims,
    /// A harness exists but no oracle is wired (self-consistency only).
    HarnessOnlyNoOracle,
}

/// One surface's board row. Authored as `(defnix-surface :id … :covers … …)`.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defnix-surface")]
pub struct NixSurface {
    /// The surface this row is for (1:1 onto [`Surface`]).
    pub id: Surface,
    /// One-line statement of the use-case space this surface covers.
    pub covers: String,
    /// The `(def…)` row form for this surface's items, or `""` if none exists yet.
    #[serde(default, rename = "rowForm")]
    pub row_form: String,
    /// What sui-side surface the coverage gate reads.
    pub reflects: Reflects,
    /// What a covered row is checked against.
    pub oracle: Oracle,
    /// The tier this row CLAIMS. Honesty-gated: must be `<= earned_tier(...)`.
    pub tier: SurfaceTier,
    /// The blocker (if any) capping this surface's reachable tier.
    pub blocker: SurfaceBlocker,
    /// The frontier — the honest current state, never hidden.
    #[serde(default)]
    pub notes: String,
}

/// The maximum tier the named machinery honestly earns — the `perf::earned_tier`
/// twin. A row with no row-form or no reflection can be at most `Design`; with a
/// reflection but no oracle, at most `Enumerated`; only both earns `ParityWired`.
#[must_use]
pub fn earned_tier(row_form: &str, reflects: Reflects, oracle: Oracle) -> SurfaceTier {
    if row_form.trim().is_empty() || reflects == Reflects::None {
        return SurfaceTier::Design;
    }
    if oracle == Oracle::Absent {
        return SurfaceTier::Enumerated;
    }
    SurfaceTier::ParityWired
}

impl NixSurface {
    /// The tier this row's declared machinery actually earns.
    #[must_use]
    pub fn earned_tier(&self) -> SurfaceTier {
        earned_tier(&self.row_form, self.reflects, self.oracle)
    }

    /// `true` iff the row does not claim more than its machinery earns.
    #[must_use]
    pub fn is_honest(&self) -> bool {
        self.tier <= self.earned_tier()
    }

    /// The enforcement border — refuses a row that CLAIMS a higher tier than its
    /// declared reflection + oracle earn (never round up). The `perf.rs`
    /// `every_authored_lever_is_honest` seal, applied per row.
    ///
    /// # Errors
    ///
    /// Returns [`SpecError::Interp`] if `tier > earned_tier(...)`.
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.is_honest() {
            Ok(())
        } else {
            Err(SpecError::Interp {
                phase: "nix_surface::validate".to_string(),
                message: format!(
                    "surface `{}` claims tier {:?} but its machinery (row_form={:?}, \
                     reflects={:?}, oracle={:?}) earns only {:?} — a coverage row must \
                     never round its tier up",
                    self.id.tag(),
                    self.tier,
                    if self.row_form.trim().is_empty() { "<none>" } else { self.row_form.as_str() },
                    self.reflects,
                    self.oracle,
                    self.earned_tier()
                ),
            })
        }
    }
}

const CANONICAL_NIX_SURFACE_LISP: &str = include_str!("../specs/nix_surface.lisp");

/// Load the canonical surface board.
///
/// # Errors
///
/// Returns an error if the spec fails to parse.
pub fn load_canonical() -> Result<Vec<NixSurface>, SpecError> {
    crate::loader::load_all::<NixSurface>(CANONICAL_NIX_SURFACE_LISP)
}

#[cfg(test)]
mod tests {
    use super::{load_canonical, earned_tier, NixSurface, Oracle, Reflects, Surface, SurfaceTier};
    use crate::SpecError;

    /// COMPILE-TIER gate: every `Surface` variant is handled here with no `_ =>`
    /// arm, so a new surface cannot be added to the enum without touching this
    /// exhaustive match — the surface axis is truly-unrepresentable-when-missing.
    #[test]
    fn surfaces_are_complete() {
        for s in Surface::ALL {
            let _covered: &str = match s {
                Surface::S1Language => "language",
                Surface::S2Closure => "whole-closure byte-parity",
                Surface::S4Cli => "CLI contract",
                Surface::S5Config => "config parsing",
                Surface::S6Daemon => "daemon worker protocol",
                Surface::S7Path => "drop-in PATH",
                Surface::UPerf => "performance matrix",
            };
        }
        assert_eq!(Surface::ALL.len(), 7, "the five/seven-surface set is closed");
    }

    #[test]
    fn earned_tier_never_rounds_up() {
        // No row form or no reflection => at most Design.
        assert_eq!(earned_tier("", Reflects::BuiltinRegistry, Oracle::ByteParity), SurfaceTier::Design);
        assert_eq!(earned_tier("defbuiltincase", Reflects::None, Oracle::ByteParity), SurfaceTier::Design);
        // Reflection but no oracle => Enumerated.
        assert_eq!(earned_tier("defbuiltincase", Reflects::BuiltinRegistry, Oracle::Absent), SurfaceTier::Enumerated);
        // Both => ParityWired (a live oracle bound — NOT "proven equal to nix").
        assert_eq!(earned_tier("defbuiltincase", Reflects::BuiltinRegistry, Oracle::ExpFixture), SurfaceTier::ParityWired);
    }

    #[test]
    fn a_row_claiming_more_than_it_earns_is_refused() {
        let dishonest = NixSurface {
            id: Surface::S5Config,
            covers: "config".into(),
            row_form: String::new(), // no form...
            reflects: Reflects::None, // ...no reflection...
            oracle: Oracle::Absent,
            tier: SurfaceTier::ParityWired, // ...but claims the top tier
            blocker: super::SurfaceBlocker::NoParser,
            notes: String::new(),
        };
        assert!(!dishonest.is_honest());
        let err = dishonest.validate().expect_err("must refuse a rounded-up tier");
        assert!(matches!(err, SpecError::Interp { .. }));
    }

    #[test]
    fn canonical_board_parses_covers_every_surface_and_is_all_honest() {
        let board = load_canonical().expect("nix_surface canonical board must compile");
        // Bijection: exactly one row per Surface variant, no dupes, no gaps.
        assert_eq!(board.len(), Surface::ALL.len(), "one board row per surface");
        for s in Surface::ALL {
            let n = board.iter().filter(|r| r.id == *s).count();
            assert_eq!(n, 1, "surface {:?} must have exactly one board row (found {n})", s);
        }
        // Every authored row passes the honesty border (the board cannot ship a
        // rounded-up tier).
        for r in &board {
            r.validate()
                .unwrap_or_else(|e| panic!("authored surface `{}` is dishonest: {e:?}", r.id.tag()));
        }
    }
}
