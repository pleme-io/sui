//! GC-root honesty, against the real daemon: what sui builds survives a
//! collection that runs while sui still needs it, and is released when it no
//! longer does.
//!
//! Each test is two processes: sui roots a path, then CppNix's
//! `nix-store --delete` (a targeted collection that honours every root) tries
//! to remove it. It must refuse while the root is held and succeed once it is
//! released, which also proves the refusal was caused by sui's root.
//!
//! Skipped, and says so, where no nix daemon runs.

use std::path::{Path, PathBuf};
use std::process::Command;

const SOCKET: &str = "/nix/var/nix/daemon-socket/socket";

fn daemon_available(test: &str) -> Option<PathBuf> {
    let nix_store = sui_compat::cppnix::locate("nix-store");
    if !Path::new(SOCKET).exists() || std::env::var_os("NIX_REMOTE").is_some() || nix_store.is_none() {
        eprintln!("skip {test}: no default nix daemon or no nix-store");
        return None;
    }
    nix_store
}

fn sui() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sui"))
}

/// A fresh, trivially buildable derivation for this host; returns its drvPath.
fn fresh_drv(tag: &str) -> String {
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    );
    let expr = format!(
        r#"(derivation {{ name = "sui-c8-{tag}"; nonce = "{nonce}"; system = builtins.currentSystem; builder = "/bin/sh"; args = [ "-c" "echo c8 > $out" ]; }}).drvPath"#
    );
    let out = sui().args(["eval", "--impure", "--raw", "--expr", &expr]).output().unwrap();
    assert!(out.status.success(), "sui eval: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// `nix-store --delete <path>`: Ok when deleted, Err(stderr) when refused.
fn delete(nix_store: &Path, path: &str) -> Result<(), String> {
    let out = Command::new(nix_store).args(["--delete", path]).output().unwrap();
    if out.status.success() && !Path::new(path).exists() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// `sui build` leaves `result` behind as an indirect root, the way `nix build`
/// does: the output outlives the sui process until the link is removed.
#[test]
fn build_out_link_is_an_indirect_gc_root() {
    let Some(nix_store) = daemon_available("build_out_link_is_an_indirect_gc_root") else {
        return;
    };
    let drv = fresh_drv("link");
    let work = tempfile::tempdir().unwrap();
    let link = work.path().join("result");
    let out = sui()
        .current_dir(work.path())
        .args(["build", "--out-link", &link.to_string_lossy(), &drv])
        .output()
        .unwrap();
    assert!(out.status.success(), "sui build: {}", String::from_utf8_lossy(&out.stderr));
    let built = String::from_utf8(out.stdout).unwrap().trim().to_string();

    assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from(&built));
    let auto = Path::new("/nix/var/nix/gcroots/auto")
        .join(sui_store::gc_roots::auto_root_name(&link.to_string_lossy()));
    assert_eq!(std::fs::read_link(&auto).unwrap(), link, "indirect root registered");

    let refused = delete(&nix_store, &built);
    assert!(refused.is_err(), "the linked output must survive a collection");
    assert!(refused.unwrap_err().contains("alive"), "refused for liveness");

    std::fs::remove_file(&link).unwrap();
    delete(&nix_store, &built).expect("with the link gone the output is collectable");
}

/// A realize roots its output for the life of the process, so a collection
/// between "built" and "linked" cannot take it.
#[test]
fn a_realized_output_stays_rooted_while_the_process_lives() {
    let Some(nix_store) = daemon_available("a_realized_output_stays_rooted_while_the_process_lives") else {
        return;
    };
    let drv = fresh_drv("temp");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let outputs = rt.block_on(sui_orchestrate::realize_drv(&drv)).unwrap();
    let built = outputs.first().expect("one output").clone();
    assert!(Path::new(&built).exists());

    let refused = delete(&nix_store, &built);
    assert!(refused.is_err(), "a live realize's output must survive a collection");

    sui_store::daemon_session::release(Path::new(SOCKET));
    delete(&nix_store, &built).expect("released, the output is collectable");
}
