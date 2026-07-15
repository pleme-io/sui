//! Regression tests for the cross-run eval-cache wiring into `sui eval`.
//!
//! The content-addressed eval-cache (`sui-eval/src/eval_cache.rs`) memoizes
//! eval OUTPUT across runs keyed on `(source+mode, flake.lock)` — the win nix
//! structurally cannot offer. These tests pin the load-bearing invariants of
//! the CLI wiring so it can't silently regress:
//!
//!   1. a second identical installable eval is served byte-identically,
//!   2. the served bytes equal a fresh eval (the `SUI_EVAL_CACHE_VERIFY` gate),
//!   3. `--no-eval-cache` bypasses the cache (never populates it),
//!   4. `--raw` prints a drvPath WITHOUT surrounding quotes (nix parity),
//!   5. a different render mode never collides on one cache entry.
//!
//! Hermetic: `SUI_EVAL_CACHE_PATH` redirects the store to a temp file so a run
//! never touches the operator's real `~/Library/Caches/sui/eval-cache.json`.

use assert_cmd::Command;
use std::path::Path;

/// A no-input flake with one cheap `derivation` output. No inputs ⇒ a minimal
/// but valid `flake.lock` (required: the cache only keys lock-pinned flakes).
fn write_fixture(dir: &Path) {
    std::fs::write(
        dir.join("flake.nix"),
        r#"{
  description = "eval-cache wiring test fixture";
  outputs = { self }: {
    drv = derivation {
      name = "cache-wiring-test";
      system = "aarch64-darwin";
      builder = "/bin/sh";
      args = [ "-c" "echo hi > $out" ];
    };
  };
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("flake.lock"),
        "{\n  \"nodes\": { \"root\": {} },\n  \"root\": \"root\",\n  \"version\": 7\n}\n",
    )
    .unwrap();
}

/// `sui --no-vm eval [--raw] <installable>` with a hermetic cache path.
fn run(cache: &Path, installable: &str, raw: bool) -> String {
    let mut cmd = Command::cargo_bin("sui").expect("cargo_bin sui");
    cmd.env("SUI_EVAL_CACHE_PATH", cache).arg("--no-vm").arg("eval");
    if raw {
        cmd.arg("--raw");
    }
    let assert = cmd.arg(installable).assert().success();
    String::from_utf8_lossy(&assert.get_output().stdout)
        .trim_end()
        .to_string()
}

#[test]
fn eval_cache_serves_identical_installable_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture(tmp.path());
    let cache = tmp.path().join("eval-cache.json");
    let installable = format!("{}#drv.drvPath", tmp.path().display());

    // Cold: populates the cache.
    let cold = run(&cache, &installable, true);
    assert!(cold.contains("-cache-wiring-test.drv"), "cold eval drvPath: {cold}");
    assert!(cache.exists(), "cold eval must populate the hermetic cache file");

    // Warm: served from the cache, byte-identical.
    let warm = run(&cache, &installable, true);
    assert_eq!(cold, warm, "warm (cached) output must be byte-identical to cold");
}

#[test]
fn eval_cache_raw_has_no_surrounding_quotes() {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture(tmp.path());
    let cache = tmp.path().join("eval-cache.json");
    let installable = format!("{}#drv.drvPath", tmp.path().display());

    let out = run(&cache, &installable, true);
    // `nix eval --raw` prints the bare string; sui must match (no quotes).
    assert!(!out.starts_with('"'), "--raw drvPath must not be quote-wrapped: {out}");
    assert!(out.starts_with("/nix/store/"), "--raw drvPath is the bare store path: {out}");
}

#[test]
fn eval_cache_verify_gate_passes_on_a_hit() {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture(tmp.path());
    let cache = tmp.path().join("eval-cache.json");
    let installable = format!("{}#drv.drvPath", tmp.path().display());

    // Populate.
    run(&cache, &installable, true);
    // On the hit, SUI_EVAL_CACHE_VERIFY re-evals fresh and asserts equality;
    // a mismatch would panic the process and fail `.success()`.
    Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .env("SUI_EVAL_CACHE_PATH", &cache)
        .env("SUI_EVAL_CACHE_VERIFY", "1")
        .args(["--no-vm", "eval", "--raw", &installable])
        .assert()
        .success();
}

#[test]
fn no_eval_cache_flag_bypasses_the_cache() {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture(tmp.path());
    let cache = tmp.path().join("eval-cache.json");
    let installable = format!("{}#drv.drvPath", tmp.path().display());

    Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .env("SUI_EVAL_CACHE_PATH", &cache)
        .args(["--no-vm", "eval", "--raw", "--no-eval-cache", &installable])
        .assert()
        .success();
    assert!(
        !cache.exists(),
        "--no-eval-cache must never populate the cache file",
    );
}

#[test]
fn render_modes_do_not_collide_on_one_entry() {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture(tmp.path());
    let cache = tmp.path().join("eval-cache.json");
    let installable = format!("{}#drv.drvPath", tmp.path().display());

    // Seed the RAW entry, then request JSON: a mode collision would serve the
    // raw bytes for the json request. `--json` of a string is a quoted JSON
    // string, distinct from the bare `--raw` bytes.
    let raw = run(&cache, &installable, true);
    let json = {
        let assert = Command::cargo_bin("sui")
            .expect("cargo_bin sui")
            .env("SUI_EVAL_CACHE_PATH", &cache)
            .args(["--no-vm", "eval", "--json", &installable])
            .assert()
            .success();
        String::from_utf8_lossy(&assert.get_output().stdout).trim_end().to_string()
    };
    assert_ne!(raw, json, "raw and json outputs must not collide on one cache entry");
    assert_eq!(json, format!("\"{raw}\""), "json output is the quoted form of the raw drvPath");
}
