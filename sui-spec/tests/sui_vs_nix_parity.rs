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


// ── key generate-secret / convert-secret-to-public ────────────────
//
// These lock the SECRET-KEY wire format against cppnix. The class they
// exist to prevent, measured 2026-08-27: `sui key generate-secret` emitted
// a bare 32-byte ed25519 seed while cppnix emits 64 bytes (seed || public).
// Every consumer disagreed -- nix refused sui's key ("secret key is not
// valid"), sui's own cache signer refused it ("expected 64 bytes, got 32"),
// and so the pleme-io-native binary could not mint a key for the
// pleme-io-native cache. The operator had to keep cppnix installed for it.
//
// Nothing caught it because the only test that touched the command
// (sui-spec/tests/wide_tooling_verification.rs) asserted the SHAPE of the
// output -- a `name:` prefix on stdout, a pubkey on stderr -- and then
// round-tripped the key through `sui store sign-manifest`, which was on the
// same wrong side. Self-consistency proved nothing; only a cross-binary
// oracle can. That is what these are.

/// A key minted by sui must be readable by nix, and both must agree on the
/// public half. This is the direction that was broken.
#[test]
#[ignore]
fn sui_minted_key_is_readable_by_nix() {
    let sui = sui_bin();
    let dir = std::env::temp_dir().join("sui-key-parity-a");
    std::fs::create_dir_all(&dir).unwrap();
    let key_path = dir.join("sui.key");

    let minted = run(sui.to_str().unwrap(), &["key", "generate-secret", "--key-name", "parity-a"])
        .expect("sui key generate-secret failed");
    std::fs::write(&key_path, &minted).unwrap();

    let sui_pub = pipe_stdin(sui.to_str().unwrap(), &["key", "convert-secret-to-public"], &minted)
        .expect("sui convert failed");
    let nix_pub = pipe_stdin("nix", &["key", "convert-secret-to-public"], &minted)
        .expect("nix REFUSED a sui-minted key -- the format has diverged again");

    assert_eq!(sui_pub, nix_pub, "sui and nix disagree on the public half of a sui-minted key");
    assert!(sui_pub.starts_with("parity-a:"), "key name lost: {sui_pub}");
}

/// A key minted by nix must be readable by sui, and both must agree.
#[test]
#[ignore]
fn nix_minted_key_is_readable_by_sui() {
    let sui = sui_bin();
    let minted = run("nix", &["key", "generate-secret", "--key-name", "parity-b"])
        .expect("nix key generate-secret failed");

    let nix_pub = pipe_stdin("nix", &["key", "convert-secret-to-public"], &minted).unwrap();
    let sui_pub = pipe_stdin(sui.to_str().unwrap(), &["key", "convert-secret-to-public"], &minted)
        .expect("sui REFUSED a nix-minted key -- the format has diverged again");

    assert_eq!(sui_pub, nix_pub, "sui and nix disagree on the public half of a nix-minted key");
}

/// The payload is 64 bytes on BOTH sides. Guards the specific regression:
/// a 32-byte seed round-trips perfectly against itself, so only an explicit
/// length assertion catches the split before the cross-binary check does.
#[test]
#[ignore]
fn both_binaries_emit_64_byte_payloads() {
    let sui = sui_bin();
    for (who, secret) in [
        ("sui", run(sui.to_str().unwrap(), &["key", "generate-secret", "--key-name", "len-s"]).unwrap()),
        ("nix", run("nix", &["key", "generate-secret", "--key-name", "len-n"]).unwrap()),
    ] {
        let b64 = secret.split_once(':').expect("no colon").1;
        let raw = base64_decode_std(b64).expect("bad base64");
        assert_eq!(raw.len(), 64, "{who} emitted a {}-byte payload, cppnix uses 64", raw.len());
    }
}

/// The end the whole exercise is for: a key minted by sui must be usable by
/// sui's OWN cache signer. This is what failed before, and it is the reason
/// the operator could not drop the nix binary.
#[test]
#[ignore]
fn sui_minted_key_is_accepted_by_sui_cache() {
    let sui = sui_bin();
    let dir = std::env::temp_dir().join("sui-key-parity-c");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let key_path = dir.join("sui.key");

    let minted = run(sui.to_str().unwrap(), &["key", "generate-secret", "--key-name", "parity-c"]).unwrap();
    std::fs::write(&key_path, &minted).unwrap();

    let out = std::process::Command::new(sui.to_str().unwrap())
        .args(["cache", "watch", "--once",
               "--store-path", store.to_str().unwrap(),
               "--signing-key", key_path.to_str().unwrap()])
        .output()
        .expect("spawn sui cache watch");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && !stderr.contains("signing error"),
        "sui cache REJECTED a sui-minted key: {stderr}"
    );
}

/// stdout must be byte-identical, trailing newline included. `run()` calls
/// `trim_end()`, so every other test here is blind to this: cppnix emits NO
/// trailing newline and sui emitted one, which silently made
/// `sui key generate-secret > keyfile` produce a file differing from nix's by
/// one byte. sui's own readers trim, so nothing inside sui could see it --
/// the same shape as suminuri's `--extract` missing-newline bug.
#[test]
#[ignore]
fn key_output_is_byte_identical_including_trailing_newline() {
    let sui = sui_bin();
    let minted = run("nix", &["key", "generate-secret", "--key-name", "nl"]).unwrap();

    let nix_raw = pipe_stdin_bytes("nix", &["key", "convert-secret-to-public"], &minted).unwrap();
    let sui_raw = pipe_stdin_bytes(sui.to_str().unwrap(), &["key", "convert-secret-to-public"], &minted).unwrap();
    assert_eq!(nix_raw, sui_raw, "convert-secret-to-public stdout differs byte-wise");

    // generate-secret cannot be compared directly (the key is random), so
    // compare the SHAPE that the newline bug lived in: the final byte.
    let nix_gen = run_bytes("nix", &["key", "generate-secret", "--key-name", "nl2"]).unwrap();
    let sui_gen = run_bytes(sui.to_str().unwrap(), &["key", "generate-secret", "--key-name", "nl2"]).unwrap();
    assert_eq!(
        nix_gen.last().map(|b| *b == b'\n'),
        sui_gen.last().map(|b| *b == b'\n'),
        "generate-secret trailing-newline behaviour differs from cppnix"
    );
}

fn pipe_stdin_bytes(cmd: &str, args: &[&str], input: &str) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() { return None; }
    Some(out.stdout)
}

fn pipe_stdin(cmd: &str, args: &[&str], input: &str) -> Option<String> {
    use std::io::Write;
    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn base64_decode_std(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
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
