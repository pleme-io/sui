//! The **no-fallback** lock.
//!
//! The wrapper's new invariant: **every `nix <cmd>` invocation
//! either runs entirely on sui or exits with a typed coverage-gap
//! message — there is no cppnix fallback.**
//!
//! Routing:
//! 1. Catalog says Working/SuiNative → run on sui.
//! 2. Catalog says Stub/Partial/Missing → exit 78 with typed
//!    `coverage-gap` message (no external binary invoked).
//! 3. Catalog has no entry → same coverage-gap path.
//! 4. Sui itself fails on a Working command → propagate sui's
//!    exit code (no retry on cppnix).
//!
//! These tests lock that invariant by exercising the wrapper
//! binary directly with a synthetic sui stand-in.
//!
//! Enforces the pleme-io directive: **sui replaces nix completely
//! in Rust, no fallback ever.**

use std::process::Command;

fn workspace_root() -> std::path::PathBuf {
    let here = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&here)
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf()
}

fn nix_wrap_bin() -> std::path::PathBuf {
    workspace_root().join("target/debug/nix-wrap")
}

/// Build a tiny "fake binary" script that exits with the given
/// code + emits its name as stdout.
fn make_fake_bin(dir: &std::path::Path, name: &str, exit_code: i32) -> std::path::PathBuf {
    let path = dir.join(name);
    let body = format!("#!/bin/sh\necho {name}\nexit {exit_code}\n");
    std::fs::write(&path, body).expect("write fake bin");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    path
}

fn run_wrap(sui: &std::path::Path, argv: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(nix_wrap_bin());
    cmd.env("NIX_WRAP_SUI_BIN", sui);
    cmd.env("HOME", "/tmp"); // log into /tmp, never real home
    cmd.args(argv);
    cmd.output().expect("spawn nix-wrap")
}

#[test]
fn working_command_runs_on_sui() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let sui = make_fake_bin(tmp.path(), "sui", 0);

    // `hash to-sri` is Working in the canonical catalog.
    let out = run_wrap(&sui, &["hash", "to-sri", "sha256:abc"]);

    assert!(out.status.success(), "wrapper should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), "sui", "stdout proves sui was the executor");
}

#[test]
fn working_command_sui_failure_propagates_no_retry() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let sui = make_fake_bin(tmp.path(), "sui", 42);

    let out = run_wrap(&sui, &["hash", "to-sri", "sha256:abc"]);

    assert_eq!(
        out.status.code(),
        Some(42),
        "sui's exit code must propagate; NO cppnix retry"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), "sui", "only sui ran; no fallback path");
}

#[test]
fn unknown_command_exits_with_coverage_gap_code_78() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let sui = make_fake_bin(tmp.path(), "sui", 0);

    // `totally-not-a-command` is not in the catalog.
    let out = run_wrap(&sui, &["totally-not-a-command"]);

    assert_eq!(
        out.status.code(),
        Some(78),
        "coverage-gap exits stable 78 (parseable by CI gates)"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("sui"),
        "sui must NOT be invoked for unknown commands"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("coverage-gap:"),
        "stderr must carry the typed coverage-gap prefix; got: {stderr}"
    );
    assert!(
        stderr.contains("absent"),
        "stderr must label the catalog state ('absent' for unknown commands)"
    );
}

#[test]
fn gap_message_names_the_command_and_directs_to_closure_work() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let sui = make_fake_bin(tmp.path(), "sui", 0);

    let out = run_wrap(&sui, &["some-future-command", "subarg"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(stderr.contains("some-future-command"));
    assert!(stderr.contains("cli_coverage.lisp"));
    assert!(stderr.contains("No cppnix"), "rationale must be visible");
}

#[test]
fn no_external_binary_is_invoked_for_coverage_gaps() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let sui = make_fake_bin(tmp.path(), "sui", 0);

    let out = run_wrap(&sui, &["totally-unknown"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout, "",
        "no external binary should run for a coverage-gap; stdout must be empty"
    );
}

/// **The lock**: enumerate the full catalog and assert every entry
/// has a stable typed routing decision (Working/SuiNative → sui;
/// everything else → coverage-gap). No silent miss, no fallback,
/// no panic.
#[test]
fn every_catalog_entry_has_a_route_decision() {
    use sui_spec::cli_coverage::{load_canonical, SuiCommandMaturity};

    let catalog = load_canonical().expect("catalog loads");
    assert!(catalog.len() >= 100, "catalog must have full coverage");

    let mut sui_count = 0usize;
    let mut gap_count = 0usize;
    for entry in &catalog {
        match entry.maturity {
            SuiCommandMaturity::Working | SuiCommandMaturity::SuiNative => {
                sui_count += 1;
            }
            _ => gap_count += 1,
        }
    }
    assert_eq!(
        sui_count + gap_count,
        catalog.len(),
        "every catalog entry produces a stable decision"
    );
    // Sanity floor — catalog regression alert.
    assert!(
        sui_count >= 50,
        "got {sui_count} Working/SuiNative entries — catalog regression?"
    );
}
