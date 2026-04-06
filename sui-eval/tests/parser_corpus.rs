//! Layer 1: parser corpus.
//!
//! Exercises sui's rnix wrapper against every real `flake.nix` file
//! sitting under `PLEME_IO_ROOT`. Offline — no oracle needed.
//!
//! The test is intentionally lenient about *which* files are in the
//! sample (they're discovered at runtime) but deterministic about
//! *ordering* (lex-sorted, first N), so reruns on the same machine
//! test the same inputs.
//!
//! Failure mode: we collect every broken file into a Vec and, at the
//! end, panic with a summary of the first few. Surfacing the first
//! failure alone would hide real parity regressions.

mod common;

use std::path::PathBuf;

/// How many flake.nix files to pull from the pleme-io workspace.
const FLAKE_SAMPLE_SIZE: usize = 200;

#[test]
fn parse_all_pleme_io_flake_nix_does_not_panic() {
    let corpus = common::pleme_io_flake_nix_sample(FLAKE_SAMPLE_SIZE);
    if corpus.is_empty() {
        eprintln!(
            "skip parse_all_pleme_io_flake_nix_does_not_panic: no flake.nix found under {}",
            common::pleme_io_root().display()
        );
        return;
    }

    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    for path in &corpus {
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                failures.push((path.clone(), format!("read: {e}")));
                continue;
            }
        };
        // Catch panics so one weird file doesn't kill the whole run.
        let parsed = std::panic::catch_unwind(|| rnix::Root::parse(&source));
        match parsed {
            Ok(p) => {
                if !p.errors().is_empty() {
                    let msgs: Vec<String> = p.errors().iter().map(|e| e.to_string()).collect();
                    failures.push((path.clone(), format!("parse errors: {}", msgs.join("; "))));
                }
            }
            Err(_) => failures.push((path.clone(), "parser panicked".to_string())),
        }
    }

    eprintln!(
        "parse_all_pleme_io_flake_nix: parsed {} files, {} failures",
        corpus.len(),
        failures.len()
    );
    if !failures.is_empty() {
        let summary: String = failures
            .iter()
            .take(10)
            .map(|(p, why)| format!("  {}\n    {why}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{} / {} flake.nix files failed to parse. First {} failures:\n{}",
            failures.len(),
            corpus.len(),
            failures.len().min(10),
            summary
        );
    }
}

#[test]
fn parse_roundtrip_preserves_source() {
    // For each flake.nix: parse → str(green_node) → reparse → assert
    // both string representations are equal. rnix is lossless, so this
    // should always hold; a regression here indicates trivia (whitespace,
    // comments) got dropped.
    let corpus = common::pleme_io_flake_nix_sample(FLAKE_SAMPLE_SIZE);
    if corpus.is_empty() {
        eprintln!("skip parse_roundtrip_preserves_source: no corpus");
        return;
    }

    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    for path in &corpus {
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        let first = rnix::Root::parse(&source);
        if !first.errors().is_empty() {
            continue; // handled by the previous test
        }
        // Round-trip: the green tree's Display impl yields the exact
        // source bytes (rnix is a lossless parser).
        let round_tripped = first.syntax().to_string();
        if round_tripped != source {
            failures.push((
                path.clone(),
                format!(
                    "round-trip mismatch (orig len {}, rt len {})",
                    source.len(),
                    round_tripped.len()
                ),
            ));
        }
    }

    if !failures.is_empty() {
        let summary: String = failures
            .iter()
            .take(5)
            .map(|(p, why)| format!("  {}\n    {why}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{} files failed round-trip. First {}:\n{}",
            failures.len(),
            failures.len().min(5),
            summary
        );
    }
}

#[test]
fn parse_known_bad_input_returns_error() {
    // Sanity: the parser must actually reject obviously broken input.
    // If this ever passes, parse_all_pleme_io_flake_nix is worthless.
    let garbage = "let x = ; in }";
    let parsed = rnix::Root::parse(garbage);
    assert!(
        !parsed.errors().is_empty(),
        "rnix accepted {garbage:?} with zero errors"
    );
}

#[test]
fn parse_empty_input_is_handled() {
    // Empty input is a common edge case. It should not panic; it may
    // or may not produce errors depending on the parser, but sui's
    // top-level `eval` should turn it into a clean error.
    let _ = rnix::Root::parse("");
    let v = common::sui_eval_json("");
    assert!(common::is_error_json(&v));
}
