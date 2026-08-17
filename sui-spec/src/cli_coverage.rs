//! `cli_coverage` — typed self-description of sui's nix-replacement
//! progress.
//!
//! Every subcommand sui exposes is declared as a Lisp form:
//!
//!   (defsui-command
//!     :name             "store sign"
//!     :nix-equivalent   "nix store sign"
//!     :maturity         Working
//!     :substrate        ("store_layout" "hash")
//!     :notes            "Materializes ed25519-keyed signatures over NAR hashes")
//!
//! The substrate enforces a catalog ↔ source invariant: every
//! command pattern in `sui/src/main.rs` must have a catalog entry,
//! and every catalog entry must point at code that exists.  Adding
//! a stub command **requires** landing its catalog entry in the
//! same commit, so the operator-facing coverage matrix stays
//! truthful.
//!
//! Operators query "how close is sui to a full nix replacement?"
//! via `sui-spec-inventory --coverage`, which walks the catalog,
//! groups by maturity, and emits a Nord-styled coverage gauge.
//! The same data drives substrate-wide tickets — each `Missing`
//! and each `Stub` is a queued substrate task.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

/// One sui subcommand's coverage entry vs the equivalent nix
/// surface.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defsui-command")]
pub struct SuiCommand {
    /// Stable command path — `"store sign"`, `"flake show"`,
    /// `"rebuild-shadow"`.  Used as the catalog key + the
    /// inventory subject.
    pub name: String,
    /// Equivalent canonical nix invocation.  Sometimes empty
    /// when sui adds a primitive nix doesn't have
    /// (e.g. `sui rebuild-shadow`).
    #[serde(rename = "nixEquivalent")]
    pub nix_equivalent: String,
    /// Coverage maturity gate.
    ///
    /// **The grade is only a claim about [`Self::platforms`].**  See that
    /// field for why the two are inseparable.
    pub maturity: SuiCommandMaturity,
    /// The platforms this row's [`Self::maturity`] is a claim ABOUT.
    ///
    /// Empty means platform-independent — the overwhelming majority of
    /// rows (`hash file`, `store sign`, `flake show`) behave identically
    /// everywhere, and an empty list reads as "everywhere" so those rows
    /// need no annotation.
    ///
    /// **Why this field exists (2026-08-07).** `maturity` alone cannot
    /// express a command that is correct on one platform and broken on
    /// another, and `system rebuild` is exactly that: it was graded
    /// `Working` — defined above as "replace the nix invocation today
    /// **without behavior loss**" — while `sui-orchestrate`'s activation
    /// arm execs `${system_path}/activate`, which is nix-darwin's entry
    /// point.  NixOS activation is `${toplevel}/bin/switch-to-configuration`,
    /// which exports `INSTALL_BOOTLOADER` / `PRE_SWITCH_CHECK` / `SYSTEMD`
    /// and then reconciles units; the string `switch-to-configuration`
    /// appears nowhere in `sui-orchestrate`.  So the row was true on
    /// darwin and false on NixOS, and one grade had to be both.
    ///
    /// That is not a documentation defect.  `sui-nix-wrap`'s `route_for`
    /// **routes on this grade** — `Working | SuiNative` sends the real
    /// invocation to sui — so the misgrade pointed NixOS operators at an
    /// activation path that cannot activate.  Downgrading the row to
    /// `Partial` would have fixed the routing by discarding a capability
    /// that genuinely works on darwin; a round-DOWN discards true signal
    /// exactly as a round-up ships false.  Qualifying the claim keeps both
    /// halves honest.
    ///
    /// Values are `std::env::consts::OS` strings (`"macos"`, `"linux"`).
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Substrate primitives the command consumes.
    /// Cross-references `catalog::SubstrateDomain`.
    #[serde(default)]
    pub substrate: Vec<String>,
    /// One-line operator-facing description.
    pub notes: String,
}

impl SuiCommand {
    /// Does this row's [`Self::maturity`] claim cover `os`?
    ///
    /// `os` is an [`std::env::consts::OS`] string.  An empty
    /// [`Self::platforms`] means the claim is platform-independent and
    /// covers everything.
    ///
    /// This is the whole reason [`Self::platforms`] exists: a caller that
    /// acts on `maturity` without asking this question is acting on a
    /// claim that may not have been made about the machine it is running
    /// on.
    #[must_use]
    pub fn claims_platform(&self, os: &str) -> bool {
        self.platforms.is_empty() || self.platforms.iter().any(|p| p == os)
    }
}

/// Maturity gate for a sui subcommand — where it stands on the
/// path to full nix replacement.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SuiCommandMaturity {
    /// End-to-end working — operator can replace the nix
    /// invocation today without behavior loss.
    Working,
    /// Partial — accepts the args, produces correct output for
    /// the common path, but at least one known feature gap.
    Partial,
    /// Stub — argparser accepts the invocation but returns a
    /// `NotImplemented` typed error.
    Stub,
    /// Missing — no argparser binding yet.  Sui doesn't accept
    /// the command at all.
    Missing,
    /// Sui-native primitive — no nix equivalent.  Counted
    /// separately so it doesn't dilute the replacement metric.
    SuiNative,
}

impl SuiCommandMaturity {
    /// Stable display name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::Partial => "Partial",
            Self::Stub => "Stub",
            Self::Missing => "Missing",
            Self::SuiNative => "SuiNative",
        }
    }

    /// `true` if the command counts toward the replacement metric
    /// (`Working` only — `Partial` doesn't count because of the
    /// known gap; `SuiNative` doesn't count because there's no
    /// nix equivalent).
    #[must_use]
    pub fn counts_as_replacing_nix(self) -> bool {
        matches!(self, Self::Working)
    }

    /// `true` if the command is a queued substrate task
    /// (`Partial` / `Stub` / `Missing`).
    #[must_use]
    pub fn is_queued_task(self) -> bool {
        matches!(self, Self::Partial | Self::Stub | Self::Missing)
    }
}

pub const CANONICAL_CLI_COVERAGE_LISP: &str =
    include_str!("../specs/cli_coverage.lisp");

/// Load the full canonical CLI coverage catalog.
///
/// # Errors
///
/// Fails if the Lisp source can't be parsed under the schema.
pub fn load_canonical() -> Result<Vec<SuiCommand>, SpecError> {
    crate::loader::load_all::<SuiCommand>(CANONICAL_CLI_COVERAGE_LISP)
}

/// Coverage histogram — how many commands sit in each maturity
/// gate.  Operators query this for the headline number.
///
/// # Errors
///
/// Returns the same errors as [`load_canonical`].
pub fn maturity_histogram()
    -> Result<Vec<(SuiCommandMaturity, usize)>, SpecError>
{
    use std::collections::BTreeMap;
    let cat = load_canonical()?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for c in &cat {
        *counts.entry(c.maturity.name().to_string()).or_default() += 1;
    }
    // Stable order: Working → Partial → Stub → Missing → SuiNative
    let order = [
        SuiCommandMaturity::Working,
        SuiCommandMaturity::Partial,
        SuiCommandMaturity::Stub,
        SuiCommandMaturity::Missing,
        SuiCommandMaturity::SuiNative,
    ];
    Ok(order
        .into_iter()
        .map(|m| (m, *counts.get(m.name()).unwrap_or(&0)))
        .collect())
}

/// The honest floor for [`replacement_percentage`] — ONE source, two gates.
///
/// This number lived in two places: `coverage_at_100.rs`'s
/// `MIN_REPLACEMENT_PCT` and an inline `0.84` in
/// `substrate_cross_domain.rs::cli_coverage_meets_the_honest_floor`, whose own
/// comment described itself as *"paired with coverage_at_100.rs's ratchet"*.
/// They were not paired — they were two literals, and the moment one moved
/// they disagreed: demoting three lying subcommands (`store repair`,
/// `store add-file`, `develop`) from `Working` to `Stub` lowered coverage to
/// 80.5%, the first floor was lowered to match, and the second one — which
/// nothing pointed at — went red.
///
/// A comment asserting that two constants track each other is not a mechanism.
/// This is the same shape as the `nixVersion` drift and the three hand-copied
/// global-scope lists, and it gets the same fix.
///
/// **Lowering this is allowed and sometimes correct** — the alternative is
/// leaving commands labelled `Working` that are not, which keeps CI green while
/// the catalog lies, and is exactly what this budget exists to prevent. Lower
/// it in the same commit as the demotion, and say why.
///
/// Currently 0.80: 62 Working of 77 non-`SuiNative` = 80.5%.
pub const MIN_REPLACEMENT_PCT: f64 = 0.80;

/// Headline coverage number: `Working / (everything that isn't SuiNative)`.
///
/// # Errors
///
/// Returns the same errors as [`load_canonical`].
pub fn replacement_percentage() -> Result<f64, SpecError> {
    let cat = load_canonical()?;
    let total_nix: usize = cat.iter()
        .filter(|c| c.maturity != SuiCommandMaturity::SuiNative)
        .count();
    let working: usize = cat.iter()
        .filter(|c| c.maturity == SuiCommandMaturity::Working)
        .count();
    if total_nix == 0 {
        return Ok(0.0);
    }
    Ok(working as f64 / total_nix as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_catalog_parses() {
        let cat = load_canonical().expect("catalog must parse");
        assert!(!cat.is_empty(), "catalog must have ≥1 entry");
    }

    #[test]
    fn every_command_has_unique_name() {
        let cat = load_canonical().unwrap();
        let mut seen = std::collections::HashSet::new();
        for c in &cat {
            assert!(seen.insert(c.name.clone()),
                "duplicate sui command name `{}`", c.name);
        }
    }

    #[test]
    fn histogram_sums_to_total() {
        let cat = load_canonical().unwrap();
        let hist = maturity_histogram().unwrap();
        let total: usize = hist.iter().map(|(_, n)| n).sum();
        assert_eq!(total, cat.len());
    }

    #[test]
    fn replacement_percentage_is_in_range() {
        let pct = replacement_percentage().unwrap();
        assert!((0.0..=1.0).contains(&pct));
    }

    #[test]
    fn every_substrate_ref_points_at_a_real_domain() {
        let cat = load_canonical().unwrap();
        let domains = crate::catalog::load_canonical().unwrap();
        let names: std::collections::HashSet<String> = domains
            .iter()
            .map(|d| d.name.clone())
            .collect();
        for c in &cat {
            for s in &c.substrate {
                assert!(
                    names.contains(s),
                    "command `{}` references substrate `{}` which has no catalog entry",
                    c.name, s,
                );
            }
        }
    }
}
