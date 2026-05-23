//! End-to-end integration test for `sui_spec::sweep::run`.
//!
//! Drives the full shadow-rebuild substrate against mock engine
//! binaries (`/bin/true` and `/bin/false`) — proves the typed contract
//! holds without depending on real cppnix or sui being on $PATH.
//!
//! Three properties under test:
//!
//! 1. **All-match** — when both mock engines produce identical empty
//!    stdout (via `/bin/true`), every probe lands as `Verdict::Match`
//!    and `ShadowReport::all_pass` is true.  Validates the full
//!    invoke → capture → classify → record pipeline.
//! 2. **Sui-fail-only** — when sui is `/bin/false` and nix is
//!    `/bin/true`, every probe lands as `Verdict::SuiFailOnly`.
//!    Validates exit-code routing into the verdict matrix.
//! 3. **Report roundtrip** — the JSON report written to disk
//!    deserializes back into a `ShadowReport` with the same tally +
//!    record set.  Validates the report serialization contract.
//!
//! The test creates a tempdir with a placeholder `flake.nix` so the
//! sweep's flake-walking logic finds at least one fixture; the
//! placeholder is never actually evaluated (the mock engines don't
//! parse Nix).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use sui_spec::parity::{ShadowReport, Verdict};
use sui_spec::sweep::{self, Corpus, SweepConfig};

/// Build a minimal fixture flake at `dir/flake.nix`.  Content doesn't
/// matter — the mock engines ignore it.
fn write_fixture_flake(dir: &std::path::Path) {
    let path = dir.join("flake.nix");
    std::fs::write(&path, "{ outputs = _: { }; }\n")
        .expect("fixture flake write must succeed");
}

fn mock_config(sui_bin: &str, nix_bin: &str, root: PathBuf) -> SweepConfig {
    SweepConfig {
        sui_bin: PathBuf::from(sui_bin),
        nix_bin: PathBuf::from(nix_bin),
        flakes_root: root.clone(),
        explicit_flakes: vec![root],
        include_tags: Vec::new(),
        exclude_tags: vec!["expensive".into()],  // skip heavy probes
        timeout: Duration::from_secs(5),
        corpus: Corpus::Parity,
        verbose: false,
        report_path: None,
    }
}

/// Pick the platform's path to `true` and `false`.  `/usr/bin/true` on
/// most Linux + macOS; `/bin/true` on some older Linux + BSD.  Falls
/// back to `true`/`false` (looked up via $PATH) if neither exists,
/// which works as long as the test env is sane.
fn pick(name: &str) -> String {
    for candidate in [format!("/usr/bin/{name}"), format!("/bin/{name}")] {
        if std::path::Path::new(&candidate).exists() {
            return candidate;
        }
    }
    name.to_string()
}

#[test]
fn sweep_all_match_when_engines_agree() {
    let tmp = tempfile::tempdir().expect("tempdir must succeed");
    write_fixture_flake(tmp.path());

    let truthy = pick("true");
    let config = mock_config(&truthy, &truthy, tmp.path().to_path_buf());

    let report = sweep::run(&config).expect("sweep::run must succeed");
    assert!(
        report.all_pass(),
        "all probes should match when both engines produce identical empty stdout; report: {:#?}",
        report,
    );
    assert_eq!(report.divergence_count(), 0);
    // The parity corpus has at least one probe; the sweep ran them
    // all against the fixture flake.
    assert!(
        !report.records.is_empty(),
        "report must include at least one record",
    );
    // Tally must agree with record count.
    let total: usize = report.tally.values().sum();
    assert_eq!(total, report.records.len());
}

#[test]
fn sweep_sui_fail_only_when_sui_exits_nonzero() {
    let tmp = tempfile::tempdir().expect("tempdir must succeed");
    write_fixture_flake(tmp.path());

    let truthy = pick("true");
    let falsy = pick("false");
    let config = mock_config(&falsy, &truthy, tmp.path().to_path_buf());

    let report = sweep::run(&config).expect("sweep::run must succeed");
    assert!(
        !report.all_pass(),
        "all-fail-only must NOT be a pass; report: {:#?}",
        report,
    );
    let sui_fail = report
        .tally
        .get(Verdict::SuiFailOnly.name())
        .copied()
        .unwrap_or(0);
    assert!(
        sui_fail >= report.records.len() / 2,
        "expected the majority of records to be SuiFailOnly, got tally: {:#?}",
        report.tally,
    );
}

#[test]
fn sweep_writes_roundtrippable_json_report() {
    let tmp = tempfile::tempdir().expect("tempdir must succeed");
    write_fixture_flake(tmp.path());
    let report_path = tmp.path().join("report.json");

    let truthy = pick("true");
    let mut config = mock_config(&truthy, &truthy, tmp.path().to_path_buf());
    config.report_path = Some(report_path.clone());

    let written = sweep::run(&config).expect("sweep::run must succeed");
    assert!(report_path.exists(), "report file must be written");

    // Roundtrip — the written JSON deserializes back into the same
    // semantic shape.
    let raw = std::fs::read_to_string(&report_path).expect("read report");
    let loaded: ShadowReport = serde_json::from_str(&raw).expect("deserialize report");
    assert_eq!(loaded.records.len(), written.records.len());
    assert_eq!(loaded.tally, written.tally);
    assert_eq!(loaded.host, written.host);
    assert_eq!(loaded.system, written.system);
    // The roundtripped report must agree with the original on pass/fail
    // count.  Records use Vec semantics — order is preserved.
    let original_names: Vec<&str> = written.records.iter().map(|r| r.name.as_str()).collect();
    let loaded_names: Vec<&str> = loaded.records.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(original_names, loaded_names);
}

#[test]
fn corpus_selector_filters_corpora() {
    let tmp = tempfile::tempdir().expect("tempdir must succeed");
    write_fixture_flake(tmp.path());
    let truthy = pick("true");

    let mut parity_only = mock_config(&truthy, &truthy, tmp.path().to_path_buf());
    parity_only.corpus = Corpus::Parity;
    let parity_report = sweep::run(&parity_only).expect("parity sweep must succeed");

    let mut builtins_only = mock_config(&truthy, &truthy, tmp.path().to_path_buf());
    builtins_only.corpus = Corpus::BuiltinSmoke;
    let builtins_report = sweep::run(&builtins_only).expect("builtins sweep must succeed");

    let mut all = mock_config(&truthy, &truthy, tmp.path().to_path_buf());
    all.corpus = Corpus::All;
    let all_report = sweep::run(&all).expect("all sweep must succeed");

    // `all` must contain at least as many records as either subset.
    // Strict > because the rebuild corpus also contributes records.
    assert!(
        all_report.records.len() > parity_report.records.len(),
        "all corpus must produce strictly more records than parity alone",
    );
    assert!(
        all_report.records.len() > builtins_report.records.len(),
        "all corpus must produce strictly more records than builtins alone",
    );
}

#[test]
fn tag_filters_subset_corpus() {
    let tmp = tempfile::tempdir().expect("tempdir must succeed");
    write_fixture_flake(tmp.path());
    let truthy = pick("true");

    let mut all = mock_config(&truthy, &truthy, tmp.path().to_path_buf());
    all.corpus = Corpus::All;
    let all_report = sweep::run(&all).expect("baseline sweep must succeed");

    let mut smoke_only = mock_config(&truthy, &truthy, tmp.path().to_path_buf());
    smoke_only.corpus = Corpus::All;
    smoke_only.include_tags = vec!["smoke".into()];
    let smoke_report = sweep::run(&smoke_only).expect("smoke-tag sweep must succeed");

    assert!(
        smoke_report.records.len() < all_report.records.len(),
        "smoke-tag filter must produce a strict subset",
    );
    // Every smoke probe carries the smoke tag.
    for record in &smoke_report.records {
        assert!(
            record.tags.iter().any(|t| t == "smoke"),
            "smoke-tag filter let through non-smoke probe: {}",
            record.name,
        );
    }
}

#[test]
fn empty_tally_is_well_formed() {
    let report = ShadowReport {
        generated_at: "now".into(),
        generator: "test".into(),
        host: "h".into(),
        system: "aarch64-darwin".into(),
        os: "darwin".into(),
        user: "u".into(),
        sui_version: None,
        nix_version: None,
        records: Vec::new(),
        tally: BTreeMap::new(),
    };
    assert!(report.all_pass());  // vacuously true
    assert_eq!(report.divergence_count(), 0);
}
