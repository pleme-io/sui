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
