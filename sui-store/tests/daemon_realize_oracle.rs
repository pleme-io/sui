//! Live-daemon proof for the multi-user-store realize pivot
//! (`sui_store::daemon_realize`).
//!
//! # What this seals
//!
//! On a multi-user (daemon) Nix install the store is root-owned and read-only to
//! the invoking user, so sui must route privileged store writes through the
//! running nix daemon (worker protocol over the daemon socket). This test proves
//! [`realize_via_daemon`] against the **real** daemon:
//!
//! - **Realize path** — `AddTextToStore` the drv closure + `BuildPaths`
//!   (substitute-or-build) makes the output valid, and a [`Realized`] proof is
//!   minted (§1). (The pure substitute leg — a GC'd output re-fetched from
//!   `cache.nixos.org` — is proven live end-to-end in the roadmap's C0 note.)
//! - **Absent-drv honesty** — a realize whose `.drv` is nowhere on disk fails
//!   with a typed error rather than a silent pass (§2).
//!
//! Every test skips gracefully (eprintln + return) when the environment lacks a
//! daemon socket or a `nix` binary, so CI on a single-user store is green.
//!
//! ## Byte-parity note
//!
//! The pivot changes NO value the evaluator observes — it only makes the bytes at
//! an already-byte-correct output path present. The realized output is the same
//! bytes at the same content-addressed path a direct build would produce; the
//! daemon's own content-addressing is what guarantees it (the `Realized` proof's
//! C2 external-observation ceiling).

use std::path::{Path, PathBuf};
use std::process::Command;

use sui_store::daemon_realize::{
    realize_via_daemon, DaemonRealizeError, DaemonStore, StoreAccess,
};

const DAEMON_SOCKET: &str = "/nix/var/nix/daemon-socket/socket";

/// The substitutable IFD probe: `builtins.pathExists "${pkgs.hello}/bin/hello"`
/// forces IFD-realize of `hello`, which the daemon substitutes from
/// `cache.nixos.org` — no local build required.
const HELLO_FLAKE: &str = r#"{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in { helloDrv = pkgs.hello.drvPath; helloOut = pkgs.hello.outPath; };
}"#;

fn have_nix() -> bool {
    Command::new("nix").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn daemon() -> Option<DaemonStore> {
    DaemonStore::at(PathBuf::from(DAEMON_SOCKET))
}

/// Resolve `hello`'s drvPath + outPath via real nix from a throwaway flake.
fn hello_paths(dir: &Path) -> Option<(String, String)> {
    std::fs::write(dir.join("flake.nix"), HELLO_FLAKE).ok()?;
    let eval = |attr: &str| -> Option<String> {
        let out = Command::new("nix")
            .args(["eval", "--raw", &format!("{}#{attr}", dir.display())])
            .output()
            .ok()?;
        if !out.status.success() {
            eprintln!("nix eval {attr} failed: {}", String::from_utf8_lossy(&out.stderr));
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    Some((eval("helloDrv")?, eval("helloOut")?))
}

/// §1 — full realize path: `AddTextToStore` the drv closure + `BuildPaths`
/// (substitute-or-build) → the daemon makes the output valid.
///
/// This deliberately does NOT try to `nix-store --delete` the output first: a
/// delete needs the daemon's GC lock and can block a concurrent build/GC on a
/// busy store (a real hazard that wedged an earlier version of this test). The
/// realize contract is the same whether the output starts present (fast path) or
/// absent (substitute) — the assertion is the POST-condition: the output is
/// valid and a proof is minted. The substitute path itself is proven live in the
/// roadmap's C0 note (a GC'd hello re-substituted end-to-end via `sui eval`).
#[tokio::test]
async fn realize_full_path_makes_output_valid() {
    if !have_nix() {
        eprintln!("skip realize_full_path_makes_output_valid: no nix binary");
        return;
    }
    let Some(store) = daemon() else {
        eprintln!("skip realize_full_path_makes_output_valid: no daemon socket");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let Some((drv, out)) = hello_paths(tmp.path()) else {
        eprintln!("skip realize_full_path_makes_output_valid: could not resolve hello paths");
        return;
    };

    let realized = realize_via_daemon(&store, &drv, &out)
        .await
        .expect("hello must realize via the daemon");
    assert_eq!(realized.out_path(), out);
    assert!(Path::new(&out).exists(), "realized output present on disk");
}

/// §2 — a realize whose `.drv` is nowhere on disk fails with a typed error, not
/// a silent pass. Uses a syntactically-valid but non-existent drv path.
#[tokio::test]
async fn realize_missing_drv_is_typed_error() {
    let Some(store) = daemon() else {
        eprintln!("skip realize_missing_drv_is_typed_error: no daemon socket");
        return;
    };
    let bogus_drv = "/nix/store/0000000000000000000000000000000a-nonexistent.drv";
    let bogus_out = "/nix/store/0000000000000000000000000000000b-nonexistent";
    let err = realize_via_daemon(&store, bogus_drv, bogus_out).await.unwrap_err();
    // The output isn't valid and its drv is absent → a DrvMissing or OutputAbsent
    // error, never `Ok`.
    assert!(
        matches!(
            err,
            DaemonRealizeError::DrvMissing(_) | DaemonRealizeError::OutputAbsent { .. }
        ),
        "expected a typed missing-drv/absent-output error, got: {err:?}"
    );
}

/// The store-access detector picks the daemon on a multi-user store (this Mac).
#[test]
fn store_access_on_this_host() {
    // On the operator's daemon-store Mac, detection yields `Daemon`; on a
    // single-user CI store it yields `Direct`. Both are valid — the assertion is
    // only that SOME write path is detected when a daemon socket exists.
    if daemon().is_none() {
        eprintln!("skip store_access_on_this_host: no daemon socket");
        return;
    }
    let access = StoreAccess::detect();
    assert!(access.is_some(), "a write path must be detected when the daemon socket exists");
}
