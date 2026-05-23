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
    pub maturity: SuiCommandMaturity,
    /// Substrate primitives the command consumes.
    /// Cross-references `catalog::SubstrateDomain`.
    #[serde(default)]
    pub substrate: Vec<String>,
    /// One-line operator-facing description.
    pub notes: String,
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
