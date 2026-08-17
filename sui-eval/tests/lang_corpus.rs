//! Layer 6: nix-language-tests corpus.
//!
//! Walks every `eval-okay-*.nix` in `sui-eval/tests/fixtures/lang/`
//! and verifies:
//!
//! 1. sui evaluates the file to a value whose JSON matches the
//!    sibling `.exp` file. **Offline** — no oracle needed.
//! 2. (Online only) real `nix-instantiate --eval --json --strict`
//!    produces the same JSON as sui for the same file. This catches
//!    any drift between the vendored `.exp` and the nix version on
//!    the current machine.
//!
//! This harness is designed to scale: to add a new case, drop a
//! `.nix` / `.exp` pair into `tests/fixtures/lang/` and the next
//! test run picks it up. The full CppNix `tests/functional/lang/`
//! corpus should be vendored here — see `tests/fixtures/lang/REFRESH.md`.

mod common;

use std::path::PathBuf;

/// Return every lang fixture file (`eval-okay-*.nix`) sorted
/// lex-alphabetically so the run order is reproducible.
fn lang_fixtures() -> Vec<PathBuf> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("tests/fixtures/lang");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("nix") {
            continue;
        }
        if !p
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("eval-okay-"))
            .unwrap_or(false)
        {
            continue;
        }
        out.push(p);
    }
    out.sort();
    out
}

fn expected_path(nix_path: &PathBuf) -> PathBuf {
    nix_path.with_extension("exp")
}

fn read_expected(nix_path: &PathBuf) -> Option<serde_json::Value> {
    let exp_path = expected_path(nix_path);
    let text = std::fs::read_to_string(&exp_path).ok()?;
    serde_json::from_str(text.trim()).ok()
}

#[test]
fn lang_corpus_has_fixtures() {
    let fixtures = lang_fixtures();
    assert!(
        !fixtures.is_empty(),
        "no fixture files under tests/fixtures/lang/ — did the repo forget to vendor them?"
    );
    for path in &fixtures {
        assert!(
            expected_path(path).is_file(),
            "missing .exp sibling for {}",
            path.display()
        );
    }
}

#[test]
fn sui_matches_expected_output() {
    // Offline: compare sui's evaluation against the vendored .exp.
    let fixtures = lang_fixtures();
    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    for path in &fixtures {
        let Some(expected) = read_expected(path) else {
            failures.push((path.clone(), "missing or unparseable .exp".to_string()));
            continue;
        };
        let actual = common::sui_eval_json_file(path);
        if actual != expected {
            failures.push((
                path.clone(),
                format!(
                    "sui != expected\n       expected: {}\n       sui:      {}",
                    serde_json::to_string(&expected).unwrap_or_default(),
                    serde_json::to_string(&actual).unwrap_or_default(),
                ),
            ));
        }
    }
    eprintln!(
        "sui_matches_expected_output: {} / {} fixtures passing",
        fixtures.len() - failures.len(),
        fixtures.len()
    );
    // Diagnostic (env-gated, harmless): dump every failing fixture's stem to a
    // file so corpus-expansion work can bucket sui-pass vs sui-gap in one run
    // (the panic below only shows the first 5). Set SUI_LANG_DUMP=<path>.
    if let Some(dump) = std::env::var_os("SUI_LANG_DUMP") {
        let list: String = failures
            .iter()
            .map(|(p, _)| p.file_stem().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(std::path::Path::new(&dump), list).ok();
    }
    if !failures.is_empty() {
        let summary: String = failures
            .iter()
            .take(5)
            .map(|(p, why)| format!("  {}\n    {why}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{} / {} fixtures failed. First {}:\n{}",
            failures.len(),
            fixtures.len(),
            failures.len().min(5),
            summary
        );
    }
}

#[test]
fn nix_matches_expected_output() {
    // Online drift check: does real nix on THIS machine still agree
    // with the vendored .exp files? A drift here means either the
    // .exp needs regeneration or sui and nix both disagree.
    if common::skip_if_offline("nix_matches_expected_output") {
        return;
    }
    let fixtures = lang_fixtures();
    let mut drifts: Vec<(PathBuf, String)> = Vec::new();
    for path in &fixtures {
        let Some(expected) = read_expected(path) else {
            continue;
        };
        let oracle = common::nix_eval_json_file(path);
        if oracle != expected {
            drifts.push((
                path.clone(),
                format!(
                    "oracle != .exp\n       exp: {}\n       nix: {}",
                    serde_json::to_string(&expected).unwrap_or_default(),
                    serde_json::to_string(&oracle).unwrap_or_default(),
                ),
            ));
        }
    }
    if !drifts.is_empty() {
        let summary: String = drifts
            .iter()
            .take(5)
            .map(|(p, why)| format!("  {}\n    {why}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{} fixtures drift from real nix on this machine. First {}:\n{}",
            drifts.len(),
            drifts.len().min(5),
            summary
        );
    }
}

// ── The quarantine ledger ────────────────────────────────────────────────
//
// `lang_fixtures()` above uses a NON-recursive `read_dir`, so the 21 pairs
// under `known_broken/` are invisible to every test in this file. That is
// what makes quarantine cheap: a newly-failing fixture moved down there
// turns the suite green while REDUCING what it proves, and the printed
// "N / N fixtures passing" line still reads 100% against a smaller N.
//
// The two tests below close both halves of that leak:
//   - a quarantined fixture that starts PASSING is a silent graduation, so
//     it is red until somebody banks it by promoting the fixture;
//   - the denominator itself is a compared value, so shrinking it costs a
//     reviewed diff to CORPUS-DENOMINATOR.json.

/// Every `eval-okay-*.nix` under `known_broken/`, sorted for reproducible
/// run order (same contract as `lang_fixtures`, different directory).
fn known_broken_fixtures() -> Vec<PathBuf> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("tests/fixtures/lang/known_broken");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("nix") {
            continue;
        }
        if !p
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("eval-okay-"))
            .unwrap_or(false)
        {
            continue;
        }
        out.push(p);
    }
    out.sort();
    out
}

/// A quarantined fixture must still fail. One that has started passing is a
/// fix nobody banked — promote it back into the active corpus.
///
/// TIER: eval-caught (a red `cargo test`), and the vacuity refusal below is a
/// hand-written check rather than the `Examined`/`NonZeroUsize` witness type
/// that `sui_spec::parity` uses for exactly this property. Reaching that
/// witness would mean a `sui-eval` -> `sui-spec` dev-dependency edge; the
/// destination is to move this test into `sui-spec` and inherit the witness
/// (its private ingress makes a zero-denominator pass unconstructible rather
/// than merely asserted). Named, not scheduled.
#[test]
fn quarantined_fixtures_still_fail() {
    let quarantined = known_broken_fixtures();

    // ANTI-VACUITY, AND IT MUST COME FIRST. With this check placed after the
    // loop, `rm -rf known_broken/` iterates zero fixtures, collects zero
    // graduations, and reports green — the empty set satisfies "all still
    // fail" vacuously. Refusing up front is what makes deleting the
    // quarantine a red test instead of a clean one.
    assert!(
        !quarantined.is_empty(),
        "known_broken/ holds no eval-okay-* fixtures. Either the quarantine \
         was emptied without promoting its contents into the active corpus, \
         or the directory moved. An empty quarantine is NOT evidence that \
         every fixture passes — it is the absence of evidence, and this test \
         refuses to report it as a pass."
    );

    let mut graduated: Vec<PathBuf> = Vec::new();
    let mut unpaired: Vec<PathBuf> = Vec::new();
    for path in &quarantined {
        // A quarantined fixture with no .exp cannot be compared at all, so
        // it could never graduate — quarantine would be a dumping ground
        // rather than a ledger. Surface it as its own failure mode.
        let Some(expected) = read_expected(path) else {
            unpaired.push(path.clone());
            continue;
        };
        if common::sui_eval_json_file(path) == expected {
            graduated.push(path.clone());
        }
    }

    eprintln!(
        "quarantined_fixtures_still_fail: {} / {} still failing (as expected)",
        quarantined.len() - graduated.len() - unpaired.len(),
        quarantined.len()
    );

    assert!(
        unpaired.is_empty(),
        "{} quarantined fixture(s) have no .exp sibling, so they can never be \
         compared and can never graduate:\n{}",
        unpaired.len(),
        unpaired
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );

    assert!(
        graduated.is_empty(),
        "{} quarantined fixture(s) now PASS. This is a fix that was never \
         banked: the corpus is stronger than its ledger says, and the slack \
         is where the next regression hides.\n{}\n\nPromote each one per \
         tests/fixtures/lang/known_broken/README.md (move the .nix/.exp pair \
         up into tests/fixtures/lang/), then decrement `quarantined` and \
         increment `active` in tests/fixtures/lang/CORPUS-DENOMINATOR.json.",
        graduated.len(),
        graduated
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The corpus denominator is a compared value, not a printed one.
///
/// Both directions are red — see CORPUS-DENOMINATOR.json for why an
/// improvement is also a re-pin rather than a warning.
#[test]
fn corpus_denominator_is_pinned() {
    let mut lang = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    lang.push("tests/fixtures/lang");

    let pin_path = lang.join("CORPUS-DENOMINATOR.json");
    let pin: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&pin_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", pin_path.display())),
    )
    .unwrap_or_else(|e| panic!("parse {}: {e}", pin_path.display()));

    // A MISSING or defaulted field is a hard error, never a zero. A pinned
    // denominator that silently defaults to 0 is the vacuity bug wearing a
    // config file: every count would then be >= its floor forever.
    let field = |k: &str| -> usize {
        pin.get(k)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| {
                panic!(
                    "{} is missing the `{k}` field (or it is not a number). \
                     Refusing to default it — a defaulted denominator passes \
                     unconditionally.",
                    pin_path.display()
                )
            })
            .try_into()
            .expect("count fits in usize")
    };
    let (want_active, want_quarantined, want_helpers) =
        (field("active"), field("quarantined"), field("helpers"));

    let got_active = lang_fixtures().len();
    let got_quarantined = known_broken_fixtures().len();

    // Helpers: top-level .nix that are not fixtures (imported.nix, lib.nix …).
    let got_helpers = std::fs::read_dir(&lang)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    let p = e.path();
                    p.extension().and_then(|s| s.to_str()) == Some("nix")
                        && !p
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with("eval-okay-"))
                            .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0);

    assert_eq!(
        (got_active, got_quarantined, got_helpers),
        (want_active, want_quarantined, want_helpers),
        "\nlang corpus denominator moved.\n  \
         active:      {got_active} (pinned {want_active})\n  \
         quarantined: {got_quarantined} (pinned {want_quarantined})\n  \
         helpers:     {got_helpers} (pinned {want_helpers})\n\n\
         If coverage GREW, bank it: update {} and keep the ratchet tight.\n\
         If coverage SHRANK, that is the thing this gate exists to make you \
         say out loud in a diff.",
        pin_path.display()
    );

    // The three buckets must exhaust every .nix under lang/, so a fixture
    // parked in some new subdirectory cannot shrink `active` and
    // `quarantined` together while the pin still reconciles.
    let mut total = 0usize;
    let mut stack = vec![lang.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|s| s.to_str()) == Some("nix") {
                total += 1;
            }
        }
    }
    assert_eq!(
        got_active + got_quarantined + got_helpers,
        total,
        "the three buckets ({got_active} active + {got_quarantined} \
         quarantined + {got_helpers} helpers) do not account for all {total} \
         .nix files under {}. A fixture is parked somewhere neither bucket \
         reads, which is quarantine without a ledger.",
        lang.display()
    );

    eprintln!(
        "corpus_denominator_is_pinned: {{\"active\":{got_active},\
         \"quarantined\":{got_quarantined},\"helpers\":{got_helpers}}}"
    );
}
