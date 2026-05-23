//! Substrate-wide invariant: the CLI coverage catalog reports
//! 100% nix-replacement coverage.  This test fails the build the
//! moment a regression downgrades any Working command back to
//! Partial / Stub / Missing.
//!
//! Pair with `cli_coverage_invariants.rs` (shape invariants) +
//! `cli_coverage` unit tests (load + histogram).

use sui_spec::cli_coverage::{self, SuiCommandMaturity};

#[test]
fn replacement_percentage_is_one_hundred() {
    let pct = cli_coverage::replacement_percentage().expect("catalog must load");
    assert!(
        (pct - 1.0).abs() < f64::EPSILON,
        "nix-replacement coverage regressed from 100% — now {:.1}%",
        pct * 100.0,
    );
}

#[test]
fn zero_stubs() {
    let cat = cli_coverage::load_canonical().unwrap();
    let stubs: Vec<&str> = cat.iter()
        .filter(|c| c.maturity == SuiCommandMaturity::Stub)
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        stubs.is_empty(),
        "stubs reintroduced: {stubs:?}",
    );
}

#[test]
fn zero_partials() {
    let cat = cli_coverage::load_canonical().unwrap();
    let partials: Vec<&str> = cat.iter()
        .filter(|c| c.maturity == SuiCommandMaturity::Partial)
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        partials.is_empty(),
        "partials reintroduced (each names a substrate gap): {partials:?}",
    );
}

#[test]
fn zero_missing() {
    let cat = cli_coverage::load_canonical().unwrap();
    let missing: Vec<&str> = cat.iter()
        .filter(|c| c.maturity == SuiCommandMaturity::Missing)
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        missing.is_empty(),
        "Missing entries appeared: {missing:?}",
    );
}

#[test]
fn working_command_count_is_stable_or_growing() {
    // Lock the floor at 75 commands.  Adding a new Stub / Partial
    // / Missing won't trip this test, but the
    // `replacement_percentage_is_one_hundred` test will catch any
    // dilution of the gauge.
    let cat = cli_coverage::load_canonical().unwrap();
    let working = cat.iter()
        .filter(|c| c.maturity == SuiCommandMaturity::Working)
        .count();
    assert!(
        working >= 75,
        "working command count regressed: now {working} (floor 75)",
    );
}
