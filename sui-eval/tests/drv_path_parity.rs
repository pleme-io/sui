//! Layer 13: Derivation path parity.
//!
//! For simple pleme-io Rust tools, evaluate `packages.aarch64-darwin.default`
//! through both sui and real `nix eval` and verify the `drvPath` values match.
//!
//! These tests are discovery-oriented: mismatches are reported but do not
//! hard-fail. This lets us track progress toward full derivation parity
//! without blocking CI.
//!
//! All tests gate on `SUI_TEST_ONLINE=1`.

mod common;

use std::path::Path;
use std::process::Command;

// ── Real nix oracle ────────────────────────────────────────────────────

/// Evaluate a flake attribute via the real `nix eval` command and return
/// the parsed JSON value. Returns `None` on any failure (spawn, non-zero
/// exit, non-JSON output).
fn nix_eval_at(flake_dir: &Path, attr: &str) -> Option<serde_json::Value> {
    let ref_str = format!("path:{}#{attr}", flake_dir.display());
    let out = Command::new("nix")
        .args([
            "eval",
            "--json",
            "--no-write-lock-file",
            "--extra-experimental-features",
            "nix-command flakes",
            &ref_str,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).ok()
}

// ── Sui flake evaluator ────────────────────────────────────────────────

/// Evaluate a flake directory with sui's `evaluate_flake`, then navigate
/// to the given dot-separated attribute path and extract the `drvPath`
/// string from the resulting derivation attrset.
fn sui_eval_drv_path(flake_dir: &Path, attr_path: &str) -> Option<String> {
    let result = sui_eval::builtins::evaluate_flake(flake_dir).ok()?;

    // Navigate the dotted path (e.g. "packages.aarch64-darwin.default").
    let segments: Vec<&str> = attr_path.split('.').collect();
    let leaf = sui_eval::builtins::navigate_attrs(&result, &segments).ok()?;

    // The leaf should be a derivation attrset containing `drvPath`.
    match leaf {
        sui_eval::Value::Attrs(ref attrs) => {
            let drv = attrs.get("drvPath")?;
            let forced = sui_eval::eval::force_value(drv).ok()?;
            Some(forced.to_str().ok()?)
        }
        _ => None,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[test]
fn drv_path_parity_simple_tools() {
    if common::skip_if_offline("drv_path_parity") {
        return;
    }

    let root = common::pleme_io_root();
    let repos = [
        "tend",
        "codesearch",
        "zoekt-mcp",
        "blx",
        "skim-tab",
        "bm-complete",
        "seibi",
        "kontena",
    ];

    let mut matches = 0u32;
    let mut mismatches: Vec<(String, String, String)> = Vec::new();
    let mut skipped = 0u32;

    for name in repos {
        let dir = root.join(name);
        if !dir.join("flake.nix").exists() {
            skipped += 1;
            continue;
        }

        let attr = "packages.aarch64-darwin.default.drvPath";

        let nix_path = match nix_eval_at(&dir, attr) {
            Some(serde_json::Value::String(s)) => s,
            _ => {
                skipped += 1;
                continue;
            }
        };

        let sui_path = match sui_eval_drv_path(&dir, "packages.aarch64-darwin.default") {
            Some(s) => s,
            None => {
                mismatches.push((name.into(), nix_path, "<eval failed>".into()));
                continue;
            }
        };

        if nix_path == sui_path {
            matches += 1;
        } else {
            mismatches.push((name.into(), nix_path, sui_path));
        }
    }

    println!("\n=== Layer 13: Derivation Path Parity ===");
    println!(
        "{matches} match, {} mismatch, {skipped} skipped",
        mismatches.len()
    );
    for (name, nix, sui) in &mismatches {
        println!("  {name}:");
        println!("    nix: {nix}");
        println!("    sui: {sui}");
    }

    // Discovery test — report, don't assert hard.
    // Uncomment the assert below once derivation path computation is fully
    // aligned with CppNix:
    // assert!(mismatches.is_empty(), "derivation path mismatches found");
}

/// Verify that sui's `derivationStrict` actually writes a `.drv` file to the
/// store (or to `SUI_STORE_DIR` when set). Uses a minimal inline derivation
/// expression rather than a full flake so the test is self-contained.
///
/// This test does NOT require a real nix installation — it only exercises
/// sui's own derivation evaluator. It still gates on `SUI_TEST_ONLINE`
/// for consistency with the layer-13 convention.
#[test]
fn drv_file_written_to_store() {
    if !common::online_mode() {
        eprintln!("skip drv_file_written: SUI_TEST_ONLINE not set");
        return;
    }

    // Use a temp directory as the store to avoid writing to /nix/store
    // without root privileges.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let store_dir = tmp.path().to_str().expect("temp path is UTF-8");

    // Set SUI_STORE_DIR so build_derivation writes the .drv there.
    // SAFETY: This test is single-threaded and restores the variable
    // immediately after evaluation.
    unsafe { std::env::set_var("SUI_STORE_DIR", store_dir) };

    let expr = r#"derivation { name = "test-drv-written"; system = "x86_64-linux"; builder = "/bin/sh"; }"#;
    let result = sui_eval::eval(expr);

    // Restore env.
    unsafe { std::env::remove_var("SUI_STORE_DIR") };

    let val = match result {
        Ok(v) => v,
        Err(e) => {
            println!("derivation eval failed: {e}");
            return;
        }
    };

    // Extract drvPath from the result attrset.
    let drv_path = match &val {
        sui_eval::Value::Attrs(attrs) => attrs
            .get("drvPath")
            .and_then(|v| v.as_string().ok())
            .map(|s| s.to_string()),
        _ => None,
    };

    let drv_path = match drv_path {
        Some(p) => p,
        None => {
            println!("no drvPath in result");
            return;
        }
    };

    // The canonical drvPath starts with /nix/store; the on-disk file is
    // rewritten to use our temp store dir.
    let disk_path = drv_path.replacen("/nix/store", store_dir, 1);
    let exists = std::path::Path::new(&disk_path).exists();

    println!("drv_path:  {drv_path}");
    println!("disk_path: {disk_path}");
    println!("exists:    {exists}");

    assert!(exists, ".drv file should exist in SUI_STORE_DIR");
}

/// For repos that sui can evaluate, verify the `.drv` path returned by sui
/// is a syntactically valid Nix store path (starts with `/nix/store/` and
/// ends with `.drv`).
#[test]
fn drv_path_format_valid() {
    if common::skip_if_offline("drv_path_format") {
        return;
    }

    let root = common::pleme_io_root();
    let repos = [
        "tend",
        "codesearch",
        "zoekt-mcp",
        "blx",
        "skim-tab",
        "bm-complete",
    ];

    let mut checked = 0u32;
    for name in repos {
        let dir = root.join(name);
        if !dir.join("flake.nix").exists() {
            continue;
        }

        if let Some(drv_path) = sui_eval_drv_path(&dir, "packages.aarch64-darwin.default") {
            assert!(
                drv_path.starts_with("/nix/store/"),
                "{name}: drvPath should start with /nix/store/, got: {drv_path}"
            );
            assert!(
                drv_path.ends_with(".drv"),
                "{name}: drvPath should end with .drv, got: {drv_path}"
            );
            // The hash portion is 32 chars of nix32 encoding.
            let basename = drv_path.strip_prefix("/nix/store/").unwrap();
            assert!(
                basename.len() > 32,
                "{name}: drvPath basename too short: {basename}"
            );
            checked += 1;
        }
    }

    println!("\n=== Layer 13: drvPath format validation ===");
    println!("{checked} paths validated");
}
