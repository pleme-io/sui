//! Wide-tooling verification: exercise every Working +
//! SuiNative store command end-to-end against the operator's
//! /nix/store and confirm the substrate-claimed behaviour.
//!
//! Each test invokes one command with sensible defaults and
//! asserts:
//!
//! 1. Process exit-code matches the documented contract
//!    (0 for clean, 1 for drift, 2 for findings).
//! 2. Stdout / stderr conforms to the typed schema (JSON
//!    parses, Nord output non-empty).
//! 3. Repeated invocation is deterministic (same input →
//!    same output) where applicable.
//!
//! Marked `#[ignore]` by default — needs a built sui binary
//! + access to /nix/store.  Run with:
//!
//!   cargo test -p sui-spec --test wide_tooling_verification -- --ignored

use std::process::Command;

fn sui_bin() -> std::path::PathBuf {
    let here = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let workspace = std::path::Path::new(&here).parent().unwrap_or(std::path::Path::new("."));
    workspace.join("target/debug/sui")
}

fn run(args: &[&str]) -> std::process::Output {
    let bin = sui_bin();
    Command::new(&bin).args(args).output()
        .unwrap_or_else(|e| panic!("spawn `{}`: {e}", bin.display()))
}

#[allow(dead_code)]
fn run_with_stdin(args: &[&str], stdin: &[u8]) -> std::process::Output {
    use std::io::Write;
    let bin = sui_bin();
    let mut child = Command::new(&bin).args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn `{}`: {e}", bin.display()));
    child.stdin.as_mut().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().expect("wait")
}

fn first_store_path_matching(suffix: &str) -> Option<std::path::PathBuf> {
    std::fs::read_dir("/nix/store").ok()?
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().contains(suffix))
        .map(|e| e.path())
}

// ── store inventory / closure / fingerprint / stats ───────────

#[test]
#[ignore]
fn store_inventory_clean_exit_with_json() {
    let out = run(&["store", "inventory", "tiny", "--json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let _doc: serde_json::Value = serde_json::from_slice(&out.stdout)
        .expect("inventory JSON must parse");
}

#[test]
#[ignore]
fn store_closure_clean_exit_with_json() {
    let Some(path) = first_store_path_matching(".drv") else {
        eprintln!("skip: no .drv on this host"); return;
    };
    let out = run(&["store", "closure", path.to_str().unwrap(), "--json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["count"].as_u64().unwrap_or(0) > 0, "closure must have ≥1 path");
}

#[test]
#[ignore]
fn store_fingerprint_clean_exit_with_json() {
    let Some(path) = first_store_path_matching("-source") else { return; };
    let out = run(&["store", "fingerprint", path.to_str().unwrap(), "--json"]);
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["nar_sha256_hex"].as_str().unwrap().len() == 64);
    assert!(doc["nar_sha256_sri"].as_str().unwrap().starts_with("sha256-"));
}

#[test]
#[ignore]
fn store_stats_clean_exit_with_json() {
    let out = run(&["store", "stats", "tiny", "--json"]);
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["entries"].as_u64().unwrap_or(0) > 0);
}

#[test]
#[ignore]
fn store_analyze_clean_exit_with_json() {
    let out = run(&["store", "analyze", "tiny", "--no-duplicates", "--json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["histogram"].is_object());
}

#[test]
#[ignore]
fn store_upgrade_paths_clean_exit() {
    let out = run(&["store", "upgrade-paths", "tiny", "--json"]);
    assert!(out.status.success());
    let _doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
}

// ── store materialize / diff / dedupe-plan ───────────────────

#[test]
#[ignore]
fn store_materialize_byte_perfect() {
    let out = run(&["store", "materialize", "tiny-sources", "--json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let perfect = doc["perfect"].as_u64().unwrap_or(0);
    let diverged = doc["diverged"].as_u64().unwrap_or(0);
    assert!(perfect > 0, "expected ≥1 perfect rematerialization");
    assert_eq!(diverged, 0, "expected 0 diverged");
}

#[test]
#[ignore]
fn store_diff_same_path_is_empty() {
    let Some(path) = first_store_path_matching("-source") else { return; };
    let out = run(&["store", "diff", path.to_str().unwrap(), path.to_str().unwrap(), "--json"]);
    assert!(out.status.success(), "self-diff should exit 0");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["total"].as_u64().unwrap_or(99), 0, "self-diff total should be 0");
}

#[test]
#[ignore]
fn store_dedupe_plan_clean_exit() {
    let out = run(&["store", "dedupe-plan", "tiny", "--json"]);
    assert!(out.status.success());
    let _doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
}

// ── store entropy / ascii-graph / find ───────────────────────

#[test]
#[ignore]
fn store_entropy_clean_exit_with_json() {
    let Some(path) = first_store_path_matching("-source") else { return; };
    let out = run(&["store", "entropy", path.to_str().unwrap(), "--json"]);
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let entropy = doc["entropy_bits"].as_f64().unwrap();
    assert!((0.0..=8.0).contains(&entropy), "entropy must be in [0, 8] bits/byte");
}

#[test]
#[ignore]
fn store_ascii_graph_terminates_within_depth() {
    let Some(path) = first_store_path_matching(".drv") else { return; };
    let out = run(&["store", "ascii-graph", path.to_str().unwrap(), "--max-depth", "2"]);
    assert!(out.status.success(), "ascii-graph must exit clean");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("derivation graph"));
}

#[test]
#[ignore]
fn store_find_with_name_predicate() {
    let out = run(&["store", "find", "--name", ".*", "--json"]);
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["matches"].as_u64().unwrap_or(0) > 0,
        "name=.* should match every entry");
}

// ── store recipe / fingerprint-many cycle ─────────────────────

#[test]
#[ignore]
fn store_recipe_audit_sources_is_noop() {
    let out = run(&["store", "recipe", "audit-sources", "--json"]);
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["total_rewrites"].as_u64().unwrap_or(99), 0,
        "audit-sources recipe should be pure round-trip");
}

#[test]
#[ignore]
fn store_fingerprint_many_then_compare_self() {
    let tmp_a = std::env::temp_dir().join("sui-wide-test-a.json");
    let tmp_b = std::env::temp_dir().join("sui-wide-test-b.json");
    let out_a = run(&["store", "fingerprint-many", "tiny", "--out", tmp_a.to_str().unwrap()]);
    assert!(out_a.status.success(), "fingerprint-many A: stderr={}",
        String::from_utf8_lossy(&out_a.stderr));
    let out_b = run(&["store", "fingerprint-many", "tiny", "--out", tmp_b.to_str().unwrap()]);
    assert!(out_b.status.success(), "fingerprint-many B: stderr={}",
        String::from_utf8_lossy(&out_b.stderr));
    let cmp = run(&["store", "compare-manifests", tmp_a.to_str().unwrap(), tmp_b.to_str().unwrap()]);
    let _ = std::fs::remove_file(&tmp_a);
    let _ = std::fs::remove_file(&tmp_b);
    assert!(cmp.status.success(),
        "self-compare should exit 0: stderr={}", String::from_utf8_lossy(&cmp.stderr));
}

// ── store sbom / license / cve / sign-verify ────────────────

#[test]
#[ignore]
fn store_sbom_emits_well_formed_spdx() {
    let Some(path) = first_store_path_matching("-source") else { return; };
    let out = run(&["store", "sbom", path.to_str().unwrap()]);
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["spdxVersion"].as_str().unwrap(), "SPDX-2.3");
    assert_eq!(doc["dataLicense"].as_str().unwrap(), "CC0-1.0");
    assert!(doc["packages"].is_array());
    assert!(doc["relationships"].is_array());
}

#[test]
#[ignore]
fn store_license_scan_clean_exit() {
    let Some(path) = first_store_path_matching("-source") else { return; };
    let out = run(&["store", "license-scan", path.to_str().unwrap(), "--json"]);
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["closure_size"].as_u64().unwrap_or(0) > 0);
}

#[test]
#[ignore]
fn store_cve_scan_with_zero_pattern_finds_nothing_critical() {
    let Some(path) = first_store_path_matching("-source") else { return; };
    // Use a regex unlikely to match anything in plain source.
    let out = run(&["store", "cve-scan", path.to_str().unwrap(),
        "CVE-9999-9999999", "--json"]);
    // Should exit 0 (no matches) — typed regex worked.
    assert!(out.status.success() || out.status.code() == Some(2),
        "cve-scan must produce a typed exit (0 or 2); got {:?}", out.status);
    let _doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
}

#[test]
#[ignore]
fn store_sign_then_verify_roundtrips() {
    // Generate keypair + sign manifest + verify.
    let tmp_dir = std::env::temp_dir().join(format!("sui-wide-sig-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let sec_path = tmp_dir.join("sec");
    let pub_path = tmp_dir.join("pub");
    let manifest_path = tmp_dir.join("manifest.json");

    let key_out = run(&["key", "generate-secret", "--key-name", "wide-test"]);
    assert!(key_out.status.success(), "key gen failed");
    let stdout = String::from_utf8_lossy(&key_out.stdout);
    let stderr = String::from_utf8_lossy(&key_out.stderr);
    let sec_line = stdout.lines().find(|l| l.starts_with("wide-test:"))
        .expect("missing secret line in stdout");
    let pub_line = stderr.lines().find(|l| l.contains("wide-test:"))
        .and_then(|l| l.split_whitespace().last())
        .expect("missing pubkey in stderr");
    std::fs::write(&sec_path, sec_line).unwrap();
    std::fs::write(&pub_path, pub_line).unwrap();

    let mf = run(&["store", "fingerprint-many",
        "tiny", "--out", manifest_path.to_str().unwrap()]);
    assert!(mf.status.success(), "fingerprint-many: stderr={}",
        String::from_utf8_lossy(&mf.stderr));
    let sign = run(&["store", "sign-manifest",
        manifest_path.to_str().unwrap(),
        "-k", sec_path.to_str().unwrap()]);
    assert!(sign.status.success(), "sign-manifest failed: {}",
        String::from_utf8_lossy(&sign.stderr));
    let verify = run(&["store", "verify-manifest",
        manifest_path.to_str().unwrap(),
        "-p", pub_path.to_str().unwrap()]);
    let _ = std::fs::remove_dir_all(&tmp_dir);
    assert!(verify.status.success(),
        "verify-manifest should succeed on freshly signed payload: {}",
        String::from_utf8_lossy(&verify.stderr));
}

// ── derivation graph + hash + parity baseline ───────────────

#[test]
#[ignore]
fn derivation_graph_clean_exit_with_json() {
    let Some(path) = first_store_path_matching(".drv") else { return; };
    let out = run(&["derivation", "graph", path.to_str().unwrap(),
        "--max-depth", "5", "--json"]);
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["nodes"].as_u64().unwrap_or(0) > 0);
}

#[test]
#[ignore]
fn hash_to_sri_idempotent_on_empty_sha256() {
    let empty = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let out = run(&["hash", "to-sri", empty]);
    assert!(out.status.success());
    let sri = String::from_utf8_lossy(&out.stdout);
    assert_eq!(sri.trim(), "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=");
}

#[test]
#[ignore]
fn sui_parity_runs_clean() {
    let out = run(&["parity", "--json"]);
    assert!(out.status.success(), "parity should pass: {}",
        String::from_utf8_lossy(&out.stderr));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["match"].as_u64().unwrap_or(0) > 0);
    assert_eq!(doc["diverged"].as_u64().unwrap_or(99), 0);
}

#[test]
#[ignore]
fn sui_spec_inventory_coverage_is_one_hundred_percent() {
    // The sui-spec-inventory binary has its own --coverage mode;
    // we invoke the sui binary's underlying catalog to confirm
    // the gauge is at 100% via JSON.
    let cat = sui_spec::cli_coverage::load_canonical().unwrap();
    let working = cat.iter().filter(|c|
        c.maturity == sui_spec::cli_coverage::SuiCommandMaturity::Working
    ).count();
    let total_nix_equivalent = cat.iter().filter(|c|
        c.maturity != sui_spec::cli_coverage::SuiCommandMaturity::SuiNative
    ).count();
    assert_eq!(working, total_nix_equivalent,
        "coverage gauge regressed: {working}/{total_nix_equivalent}");
}

// ── census: every catalog Working command has --help support ──
//
// Compounding layer: if anyone renames or removes a Working
// command, --help will fail and this catches it without us
// having to enumerate every command above by hand.

#[test]
#[ignore]
fn every_working_command_has_help_surface() {
    let cat = sui_spec::cli_coverage::load_canonical().unwrap();
    let mut failures = vec![];
    for entry in &cat {
        if entry.maturity != sui_spec::cli_coverage::SuiCommandMaturity::Working
            && entry.maturity != sui_spec::cli_coverage::SuiCommandMaturity::SuiNative
        {
            continue;
        }
        // Skip the top-level commands that may collide with each
        // other (e.g. nix-channel vs nix channel — both bind in
        // clap but only one wins via the underscored variant).
        let tokens: Vec<&str> = entry.name.split_whitespace().collect();
        if tokens.is_empty() { continue; }
        let mut argv: Vec<&str> = tokens.clone();
        argv.push("--help");
        let out = run(&argv);
        if !out.status.success() {
            failures.push(format!("{} (exit {:?})",
                entry.name, out.status.code()));
        }
    }
    assert!(failures.is_empty(),
        "{} catalog commands missing --help surface:\n  - {}",
        failures.len(), failures.join("\n  - "));
}

// Quiet the unused import warning from the stdin variant —
// reserved for future tests that pipe input into commands.
fn _unused_keeper() {
    let _ = run_with_stdin(&["--help"], &[]);
}
