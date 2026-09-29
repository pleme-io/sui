//! `sui flip-probe` end to end.
//!
//! * Against real CppNix on a fixture flake: the verdict is `match` and the
//!   receipt carries both engines' measurements.
//! * Against a stand-in `nix` that reports a drv graph differing from sui's in
//!   one leaf: the verdict is `diverge`, the exit is non-zero, and the receipt
//!   names that leaf and its field, not the top drv. This is the probe proving
//!   it can say "no".
//!
//! Both need a nix store to instantiate into; skipped, and say so, without one.

use std::path::Path;
use std::process::Command;

use sui::flip_probe::{read_receipt, Verdict};

fn sui() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sui"))
}

fn store_available(test: &str) -> bool {
    let ok = Path::new("/nix/var/nix/daemon-socket/socket").exists()
        && sui_compat::cppnix::locate("nix").is_some();
    if !ok {
        eprintln!("skip {test}: no nix daemon or no nix binary");
    }
    ok
}

/// A flake whose `probe` is top → leaf, the leaf carrying `flags`.
fn graph(flags: &str, nonce: &str) -> String {
    format!(
        r#"let leaf = derivation {{ name = "fp-leaf"; nonce = "{nonce}"; flags = "{flags}"; system = "x86_64-linux"; builder = "/bin/sh"; }};
in derivation {{ name = "fp-top"; nonce = "{nonce}"; dep = leaf; system = "x86_64-linux"; builder = "/bin/sh"; }}"#
    )
}

fn nonce() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    )
}

fn fixture_flake(dir: &Path, body: &str) -> String {
    std::fs::write(dir.join("flake.nix"), format!("{{ outputs = {{ self }}: {{ probe = {body}; }}; }}\n")).unwrap();
    format!("path:{}#probe", dir.display())
}

#[test]
fn the_same_attribute_matches_against_cppnix() {
    if !store_available("the_same_attribute_matches_against_cppnix") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let installable = fixture_flake(dir.path(), &graph("-O2", &nonce()));
    let receipt = dir.path().join("receipt.json");
    let out = sui()
        .args(["flip-probe", &installable, "--receipt"])
        .arg(&receipt)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let r = read_receipt(&receipt).unwrap();
    assert_eq!(r.verdict, Verdict::Match);
    assert_eq!(r.sui.drv_path, r.cppnix.drv_path);
    assert!(r.sui.drv_path.as_deref().is_some_and(|p| p.ends_with("-fp-top.drv")));
    assert!(r.sui.max_rss_bytes > 0 && r.cppnix.max_rss_bytes > 0);
    assert!(r.cppnix.version.contains("nix"), "{}", r.cppnix.version);
    assert!(r.divergence.is_none());
}

#[test]
fn a_divergence_is_traced_to_the_leaf_that_caused_it() {
    if !store_available("a_divergence_is_traced_to_the_leaf_that_caused_it") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let n = nonce();
    let installable = fixture_flake(dir.path(), &graph("-O2", &n));

    // The graph the stand-in "CppNix" reports: identical but for the leaf flag.
    let other = sui()
        .args(["eval", "--raw", "--expr", &format!("({}).drvPath", graph("-O3", &n))])
        .output()
        .unwrap();
    assert!(other.status.success(), "{}", String::from_utf8_lossy(&other.stderr));
    let other_top = String::from_utf8(other.stdout).unwrap().trim().to_string();
    let fake_nix = dir.path().join("nix");
    std::fs::write(
        &fake_nix,
        format!("#!/bin/sh\ncase \"$1\" in --version) echo stand-in;; eval) echo {other_top};; *) exit 1;; esac\n"),
    )
    .unwrap();
    std::fs::set_permissions(&fake_nix, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755)).unwrap();

    let receipt = dir.path().join("receipt.json");
    let out = sui()
        .args(["flip-probe", &installable, "--nix"])
        .arg(&fake_nix)
        .arg("--receipt")
        .arg(&receipt)
        .output()
        .unwrap();
    assert!(!out.status.success(), "a divergence must exit non-zero");
    let r = read_receipt(&receipt).unwrap();
    assert_eq!(r.verdict, Verdict::Diverge);
    let div = r.divergence.expect("divergence recorded");
    let first = div.first.expect("a frontier drv");
    assert_eq!(first.name, "fp-leaf.drv", "named {:?}, not the leaf", first.name);
    assert_eq!(first.trail, vec!["fp-top.drv".to_string(), "fp-leaf.drv".to_string()]);
    let flags = first.fields.iter().find(|f| f.field == "env.flags").expect("env.flags differs");
    assert_eq!((flags.sui.as_deref(), flags.cppnix.as_deref()), (Some("-O2"), Some("-O3")));
    assert!(!flags.hash_cascade_only);
}
