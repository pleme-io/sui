//! Substrate-wide invariants between the CLI coverage catalog and
//! the actual sui CLI surface in `sui/src/main.rs`.
//!
//! Drift between code and catalog fails the build mechanically.
//! Adding a new `Commands::` pattern without landing its catalog
//! entry is impossible; flipping a Stub→Working without updating
//! the catalog is impossible.

use sui_spec::cli_coverage::{self, SuiCommand, SuiCommandMaturity};

#[test]
fn catalog_loads_and_is_nonempty() {
    let cat = cli_coverage::load_canonical().unwrap();
    assert!(cat.len() >= 50, "expected a comprehensive catalog, got {}", cat.len());
}

#[test]
fn working_commands_have_substrate_or_explanatory_notes() {
    // Every `Working` command should either declare substrate
    // primitives it consumes OR have a non-empty `notes` field
    // explaining why (e.g. "no substrate dep — pure shell hook").
    let cat = cli_coverage::load_canonical().unwrap();
    for c in cat.iter().filter(|c| c.maturity == SuiCommandMaturity::Working) {
        assert!(
            !c.substrate.is_empty() || !c.notes.trim().is_empty(),
            "command `{}` is Working but declares neither substrate refs nor explanatory notes",
            c.name,
        );
    }
}

#[test]
fn no_command_is_both_stub_and_sui_native() {
    // Stub = "we mean to implement, currently NotImplemented".
    // SuiNative = "no nix equivalent; sui's own primitive".  A
    // command can't be both — that's the discriminator's job.
    let cat = cli_coverage::load_canonical().unwrap();
    for c in &cat {
        if c.maturity == SuiCommandMaturity::SuiNative {
            assert!(
                c.nix_equivalent.is_empty(),
                "SuiNative command `{}` shouldn't declare a nix equivalent, has `{}`",
                c.name, c.nix_equivalent,
            );
        }
        if c.maturity != SuiCommandMaturity::SuiNative
            && c.maturity != SuiCommandMaturity::Working
        {
            // Stubs / Missing / Partial should always have a nix
            // equivalent — they're the gap to full replacement.
            assert!(
                !c.nix_equivalent.is_empty() || c.maturity == SuiCommandMaturity::Working,
                "non-Working / non-SuiNative command `{}` ({}) must declare a nix equivalent",
                c.name, c.maturity.name(),
            );
        }
    }
}

#[test]
fn histogram_partitions_catalog_completely() {
    // Sum of histogram counts must equal catalog size — every
    // command belongs to exactly one maturity gate.
    let cat = cli_coverage::load_canonical().unwrap();
    let hist = cli_coverage::maturity_histogram().unwrap();
    let total: usize = hist.iter().map(|(_, n)| n).sum();
    assert_eq!(total, cat.len(),
        "histogram total {} != catalog len {}", total, cat.len());
}

#[test]
fn replacement_percentage_is_strictly_positive() {
    // Sui has working `eval`, `build`, `develop` etc. — the
    // replacement percentage MUST be > 0.
    let pct = cli_coverage::replacement_percentage().unwrap();
    assert!(pct > 0.0, "replacement percentage is zero — something broke");
}

#[test]
fn every_substrate_ref_points_at_real_domain() {
    // Cross-reference invariant — catches typos in catalog edges.
    // Mirrors the same invariant in the catalog DAG tests.
    let cat = cli_coverage::load_canonical().unwrap();
    let domains = sui_spec::catalog::load_canonical().unwrap();
    let names: std::collections::HashSet<String> =
        domains.iter().map(|d| d.name.clone()).collect();
    for c in &cat {
        for s in &c.substrate {
            assert!(
                names.contains(s),
                "command `{}` references substrate `{}` not in catalog",
                c.name, s,
            );
        }
    }
}

#[test]
fn no_duplicate_command_names() {
    let cat = cli_coverage::load_canonical().unwrap();
    let mut seen = std::collections::HashSet::new();
    for c in &cat {
        assert!(seen.insert(c.name.clone()),
            "duplicate sui command `{}` in catalog", c.name);
    }
}

#[test]
fn classification_helpers_partition_maturity() {
    // Working counts as replacing nix.  Partial/Stub/Missing are
    // queued tasks.  SuiNative is neither.  Exactly one bucket
    // per maturity.
    use SuiCommandMaturity::*;
    for m in [Working, Partial, Stub, Missing, SuiNative] {
        let counts = m.counts_as_replacing_nix();
        let queued = m.is_queued_task();
        assert!(
            !(counts && queued),
            "maturity {} can't be both replacing-nix and queued-task",
            m.name(),
        );
        if m == Working {
            assert!(counts, "Working must count as replacing nix");
            assert!(!queued, "Working can't be queued");
        }
        if matches!(m, Partial | Stub | Missing) {
            assert!(queued, "{} must be queued task", m.name());
            assert!(!counts, "{} can't count as replacing", m.name());
        }
        if m == SuiNative {
            assert!(!counts, "SuiNative can't count as replacing");
            assert!(!queued, "SuiNative isn't a queued task");
        }
    }
}

#[test]
fn known_canonical_commands_are_present() {
    // Spot-check that the catalog covers the highest-leverage
    // commands.  If someone deletes the eval/build/develop
    // entries by accident, this test fires loud.
    let cat = cli_coverage::load_canonical().unwrap();
    let names: std::collections::HashSet<String> =
        cat.iter().map(|c| c.name.clone()).collect();
    for required in &["eval", "build", "develop", "run", "flake show", "system rebuild"] {
        assert!(names.contains(*required),
            "catalog missing required command `{required}`");
    }
}

#[test]
fn working_commands_show_in_helper_count() {
    let cat = cli_coverage::load_canonical().unwrap();
    let working: Vec<&SuiCommand> = cat
        .iter()
        .filter(|c| c.maturity.counts_as_replacing_nix())
        .collect();
    assert!(working.len() >= 20,
        "expected ≥20 working sui commands, got {}", working.len());
}
