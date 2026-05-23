//! sui-vs-nix byte-equivalence parity harness.
//!
//! Locks the canonical byte-equivalent surfaces:
//! - hash to-{base16,base32,base64,sri}
//! - hash file
//! - store dump-path NAR sha256
//! - derivation show→add ATerm round-trip
//!
//! Each test is `#[ignore]` by default — these probes require
//! both `nix` and a built `sui` binary on PATH plus access to
//! `/nix/store`.  Run explicitly with:
//!
//! ```text
//! cargo test -p sui-spec --test sui_vs_nix_parity -- --ignored --nocapture
//! ```
//!
//! In CI / on the operator workstation, these provide the
//! mechanical proof that sui's nix-replacement coverage hasn't
//! diverged at the byte level.

use std::process::Command;

fn sui_bin() -> std::path::PathBuf {
    // Prefer the workspace's debug build.  If absent, fall back
    // to `sui` on PATH (release-installed).
    let here = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let workspace = std::path::Path::new(&here).parent().unwrap_or(std::path::Path::new("."));
    let debug = workspace.join("target/debug/sui");
    if debug.exists() {
        return debug;
    }
    let release = workspace.join("target/release/sui");
    if release.exists() {
        return release;
    }
    std::path::PathBuf::from("sui")
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn run_bytes(cmd: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in d { s.push_str(&format!("{b:02x}")); }
    s
}

fn first_store_path_matching(pattern: &str) -> Option<std::path::PathBuf> {
    let store = std::path::Path::new("/nix/store");
    if !store.exists() { return None; }
    std::fs::read_dir(store).ok()?
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().contains(pattern))
        .map(|e| e.path())
}

const SAMPLE_HASH: &str =
    "sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03";

// ── hash conversions — guaranteed byte-equivalent ─────────────────

#[test]
#[ignore]
fn hash_to_base16_byte_equivalent() {
    let sui = sui_bin();
    let nix_out = run("nix", &["hash", "to-base16", "--type", "sha256", SAMPLE_HASH])
        .expect("nix must be on PATH");
    let sui_out = run(sui.to_str().unwrap(), &["hash", "to-base16", SAMPLE_HASH])
        .expect("sui must be built");
    assert_eq!(sui_out, nix_out, "hash to-base16 diverged");
}

#[test]
#[ignore]
fn hash_to_base32_byte_equivalent() {
    let sui = sui_bin();
    let nix_out = run("nix", &["hash", "to-base32", "--type", "sha256", SAMPLE_HASH])
        .expect("nix must be on PATH");
    let sui_out = run(sui.to_str().unwrap(), &["hash", "to-base32", SAMPLE_HASH])
        .expect("sui must be built");
    assert_eq!(sui_out, nix_out, "hash to-base32 diverged");
}

#[test]
#[ignore]
fn hash_to_base64_byte_equivalent() {
    let sui = sui_bin();
    let nix_out = run("nix", &["hash", "to-base64", "--type", "sha256", SAMPLE_HASH])
        .expect("nix must be on PATH");
    let sui_out = run(sui.to_str().unwrap(), &["hash", "to-base64", SAMPLE_HASH])
        .expect("sui must be built");
    assert_eq!(sui_out, nix_out, "hash to-base64 diverged");
}

#[test]
#[ignore]
fn hash_to_sri_byte_equivalent() {
    let sui = sui_bin();
    let nix_out = run("nix", &["hash", "to-sri", "--type", "sha256", SAMPLE_HASH])
        .expect("nix must be on PATH");
    let sui_out = run(sui.to_str().unwrap(), &["hash", "to-sri", SAMPLE_HASH])
        .expect("sui must be built");
    assert_eq!(sui_out, nix_out, "hash to-sri diverged");
}

#[test]
#[ignore]
fn hash_file_sri_byte_equivalent() {
    let sui = sui_bin();
    let tmp = std::env::temp_dir().join("sui-parity-hash-file");
    std::fs::write(&tmp, b"hello\n").unwrap();
    let nix_out = run("nix", &["hash", "file", tmp.to_str().unwrap()])
        .expect("nix must be on PATH");
    let sui_out = run(sui.to_str().unwrap(),
        &["hash", "file", tmp.to_str().unwrap(), "--base", "sri"])
        .expect("sui must be built");
    let _ = std::fs::remove_file(&tmp);
    assert_eq!(sui_out, nix_out, "hash file --base sri diverged");
}

// ── NAR dump-path byte-equivalent ─────────────────────────────────

#[test]
#[ignore]
fn store_dump_path_nar_byte_equivalent() {
    let sui = sui_bin();
    let Some(target) = first_store_path_matching("-source") else {
        eprintln!("skip: no `-source` store path available on this host");
        return;
    };
    let nix_bytes = run_bytes("nix",
        &["--extra-experimental-features", "nix-command",
          "store", "dump-path", target.to_str().unwrap()])
        .expect("nix must be on PATH");
    let sui_bytes = run_bytes(sui.to_str().unwrap(),
        &["store", "dump-path", target.to_str().unwrap()])
        .expect("sui must be built");
    let nix_hash = sha256_hex(&nix_bytes);
    let sui_hash = sha256_hex(&sui_bytes);
    assert_eq!(sui_hash, nix_hash,
        "NAR sha256 diverged for {}\n  nix: {nix_hash}\n  sui: {sui_hash}",
        target.display());
}

// ── ATerm round-trip ──────────────────────────────────────────────

#[test]
#[ignore]
fn derivation_show_add_aterm_roundtrips() {
    let sui = sui_bin();
    let Some(target) = first_store_path_matching(".drv") else {
        eprintln!("skip: no `.drv` in /nix/store on this host");
        return;
    };
    let original = std::fs::read_to_string(&target).expect("read .drv");

    // sui derivation show → JSON
    let json = run(sui.to_str().unwrap(),
        &["derivation", "show", target.to_str().unwrap()])
        .expect("sui derivation show must succeed");
    let tmp = std::env::temp_dir().join("sui-parity-drv.json");
    std::fs::write(&tmp, &json).unwrap();

    // sui derivation add JSON → ATerm (on stderr)
    let out = Command::new(sui.to_str().unwrap())
        .args(["derivation", "add", tmp.to_str().unwrap()])
        .output().expect("sui derivation add must spawn");
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Strip the `# ...` info line we emit.
    let aterm: String = stderr.lines()
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let aterm = aterm.trim_end_matches('\n');

    let _ = std::fs::remove_file(&tmp);
    assert_eq!(aterm, original.trim_end_matches('\n'),
        "ATerm round-trip diverged for {}", target.display());
}

// ── catalog-driven smoke probes ───────────────────────────────────

#[test]
#[ignore]
fn every_working_command_at_least_runs_argparse() {
    // For every Working catalog entry, invoke `sui <command>
    // --help` and assert argparse exits cleanly.  Catches the
    // "catalog claims Working but the subcommand is missing
    // entirely" regression mode.
    let sui = sui_bin();
    let cat = sui_spec::cli_coverage::load_canonical().unwrap();
    let mut failures = Vec::new();
    for entry in cat.iter() {
        if entry.maturity != sui_spec::cli_coverage::SuiCommandMaturity::Working {
            continue;
        }
        // Skip top-level aggregate commands that only have
        // subcommands (no --help on the bare name).
        let parts: Vec<&str> = entry.name.split_whitespace().collect();
        // `sui store ls --help` etc.
        let mut args: Vec<&str> = parts.clone();
        args.push("--help");
        let status = Command::new(sui.to_str().unwrap())
            .args(&args)
            .output();
        match status {
            Ok(o) if o.status.success() => {}
            Ok(o) => failures.push(format!(
                "{}: exit={:?} stderr={}",
                entry.name,
                o.status.code(),
                String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or(""),
            )),
            Err(e) => failures.push(format!("{}: spawn error: {e}", entry.name)),
        }
    }
    assert!(failures.is_empty(),
        "argparse failures across {} Working commands:\n  {}",
        failures.len(),
        failures.join("\n  "));
}
