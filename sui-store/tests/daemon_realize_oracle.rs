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
//!   minted (§1), against a private daemon and temp store. (The pure substitute
//!   leg — a GC'd output re-fetched from `cache.nixos.org` — is proven live
//!   end-to-end in the roadmap's C0 note.)
//! - **Absent-drv honesty** — a realize whose `.drv` is nowhere on disk fails
//!   with a typed error rather than a silent pass (§2).
//!
//! §1 skips loudly where CppNix's tools are absent and fails under `CI`. The
//! read-only tests skip where no daemon socket exists.
//!
//! ## Byte-parity note
//!
//! The pivot changes NO value the evaluator observes — it only makes the bytes at
//! an already-byte-correct output path present. The realized output is the same
//! bytes at the same content-addressed path a direct build would produce; the
//! daemon's own content-addressing is what guarantees it (the `Realized` proof's
//! C2 external-observation ceiling).

use std::path::{Path, PathBuf};

use sui_store::daemon_realize::{
    realize_via_daemon, DaemonRealizeError, DaemonStore, StoreAccess,
};

#[path = "../../tests/support/private_store.rs"]
mod private_store;

const DAEMON_SOCKET: &str = "/nix/var/nix/daemon-socket/socket";

fn daemon() -> Option<DaemonStore> {
    DaemonStore::at(PathBuf::from(DAEMON_SOCKET))
}

/// §1 — full realize path: `AddTextToStore` the drv closure + `BuildPaths`
/// (substitute-or-build) → the daemon makes the output valid. A private daemon
/// builds a fresh derivation here, so the output starts absent and the real
/// store is never written.
#[tokio::test]
async fn realize_full_path_makes_output_valid() {
    let Some(private) = private_store::PrivateStore::start("realize_full_path_makes_output_valid", private_store::Shape::OwnStoreDir) else {
        return;
    };
    let store = DaemonStore::at(private.layout.socket()).expect("the private daemon's socket");
    let drv = private.fresh_drv("realize");
    let out = private.output_of(&drv);
    private.assert_private(&out);
    assert!(!Path::new(&out).exists(), "the output starts absent");

    let realized = realize_via_daemon(&store, &drv, &out)
        .await
        .expect("the derivation must realize via the daemon");
    assert_eq!(realized.out_path(), out);
    assert!(Path::new(&out).exists(), "realized output present on disk");
    private.assert_real_store_untouched(&private.nonce);
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
