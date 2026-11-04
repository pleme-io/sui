//! GC-root honesty, against a real CppNix daemon that serves a private temporary
//! store: what sui builds survives a collection that runs while sui still needs
//! it, and is released when it no longer does. Nothing here reaches the machine's
//! own `/nix/store` or daemon.
//!
//! Each test is two processes: sui roots a path, then CppNix's
//! `nix-store --delete` (a targeted collection that honours every root) tries
//! to remove it. It must refuse while the root is held and succeed once it is
//! released, which also proves the refusal was caused by sui's root.
//!
//! Skipped, and says so, where CppNix's tools are absent. With `CI` set the
//! absence is a failure, so CI cannot skip this file.

#[path = "support/private_store.rs"]
mod private_store;

use std::path::{Path, PathBuf};
use std::process::Command;

use private_store::{assert_bound_to_the_private_store, Layout, PrivateStore, Shape};

fn sui(store: &PrivateStore) -> Command {
    store.layout.command(Path::new(env!("CARGO_BIN_EXE_sui")))
}

#[test]
fn every_command_of_the_fixture_is_bound_to_the_private_store() {
    for shape in [Shape::OwnStoreDir, Shape::RootedAtRealStoreDir] {
        assert_bound_to_the_private_store(&Layout { root: PathBuf::from("/tmp/sui-c8-layout"), shape });
    }
}

/// `sui build` leaves `result` behind as an indirect root, the way `nix build`
/// does: the output outlives the sui process until the link is removed.
#[test]
fn build_out_link_is_an_indirect_gc_root() {
    let Some(store) = PrivateStore::start("build_out_link_is_an_indirect_gc_root", Shape::OwnStoreDir) else {
        return;
    };
    let drv = store.fresh_drv("link");
    let work = tempfile::tempdir().unwrap();
    let link = work.path().join("result");
    let out = sui(&store)
        .current_dir(work.path())
        .args(["build", "--out-link", &link.to_string_lossy(), &drv])
        .output()
        .unwrap();
    assert!(out.status.success(), "sui build: {}", String::from_utf8_lossy(&out.stderr));
    let built = String::from_utf8(out.stdout).unwrap().trim().to_string();
    store.assert_private(&built);

    assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from(&built));
    let auto = store.layout.state().join("gcroots/auto").join(sui_store::gc_roots::auto_root_name(&link.to_string_lossy()));
    assert_eq!(std::fs::read_link(&auto).unwrap(), link, "indirect root registered");

    let refused = store.try_delete(&built);
    assert!(refused.is_err(), "the linked output must survive a collection");
    let why = refused.unwrap_err();
    assert!(
        why.contains("alive") || why.contains("referenced by the GC root"),
        "refused for liveness, got: {why}"
    );

    std::fs::remove_file(&link).unwrap();
    store.delete_once_released(&built).expect("with the link gone the output is collectable");
    store.assert_real_store_untouched(&store.nonce);
}

/// A realize roots its output for the life of the process, so a collection
/// between "built" and "linked" cannot take it.
#[test]
fn a_realized_output_stays_rooted_while_the_process_lives() {
    let Some(store) = PrivateStore::start("a_realized_output_stays_rooted_while_the_process_lives", Shape::OwnStoreDir) else {
        return;
    };
    let drv = store.fresh_drv("temp");
    sui_store::set_store_url(&store.layout.remote());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let outputs = rt.block_on(sui_orchestrate::realize_drv(&drv)).unwrap();
    let built = outputs.first().expect("one output").clone();
    store.assert_private(&built);
    assert!(Path::new(&built).exists());

    let refused = store.try_delete(&built);
    assert!(refused.is_err(), "a live realize's output must survive a collection");

    sui_store::daemon_session::release(&store.layout.socket());
    store.delete_once_released(&built).expect("released, the output is collectable");
    store.assert_real_store_untouched(&store.nonce);
}
