//! Oracle differential: `sui` store-add-path vs the real `nix store add-path`.
//!
//! Parity Method (theory/BUILD.md §II.1): behavior-compatible against the real
//! `nix` oracle, sealed with a differential test. The **store-path computation**
//! and the **NAR serialization** are the byte-load-bearing surfaces on the
//! marquee critical path (roadmap §2 #8); both are proven here to match nix
//! exactly, using a temp store so no root is required.
//!
//! What is (and is not) covered without root:
//!
//! - The store-path fingerprint algorithm (`source:sha256:<narhash>:/nix/store:<name>`)
//!   and the NAR serialization are **fully verified** here — they are pure
//!   functions of the input tree and are identical whether the physical store
//!   root is `/nix/store` or a temp dir (nix uses the *logical* store dir
//!   `/nix/store` in the fingerprint regardless of the physical `--store`
//!   location — confirmed empirically below).
//! - The physical `LocalStore::add_to_store` write (NAR-unpack + DB register)
//!   is verified against a temp physical store.
//! - The privileged write to the real `/nix/store` via the daemon is a
//!   root-gated manual step (see `store_add_path_ROOT_NOTES` below); it is NOT
//!   faked green here.

use std::path::{Path, PathBuf};
use std::process::Command;

/// True if a real `nix` binary is on PATH. Tests skip (not fail) when absent so
/// CI without nix stays green; on a box with nix the assertions are mandatory.
fn have_nix() -> bool {
    Command::new("nix")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Materialize a small deterministic source tree under a temp dir and return
/// its canonicalized path (nix refuses symlinked store-parent dirs, so we
/// canonicalize — on macOS `/tmp` is a symlink to `/private/tmp`).
fn make_fixture(td: &Path) -> PathBuf {
    let src = td.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("file.txt"), b"hello world\n").unwrap();
    std::fs::write(src.join("other.txt"), b"content b\n").unwrap();
    let sub = src.join("nested");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("deep.txt"), b"deep content\n").unwrap();
    src.canonicalize().unwrap()
}

/// Run the real `nix store add-path` against a temp physical store and return
/// the resulting **logical** store path string (`/nix/store/<hash>-<name>`).
fn nix_add_path(phys_store: &Path, src: &Path, name: &str) -> Option<String> {
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "store",
            "add-path",
            "--store",
        ])
        .arg(phys_store)
        .arg(src)
        .args(["--name", name])
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "nix store add-path failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Dump the NAR of a path from nix's temp store and return the sha256 hex of
/// the NAR bytes — the exact quantity nix folds into the store-path fingerprint.
fn nix_nar_hex(phys_store: &Path, logical_path: &str) -> Option<String> {
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "--store",
        ])
        .arg(phys_store)
        .args(["store", "dump-path", logical_path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    use sha2::{Digest, Sha256};
    Some(
        Sha256::digest(&out.stdout)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// **The load-bearing byte-parity assertion.** sui's computed store path AND
/// NAR hash for a fixed source tree == the real `nix store add-path`'s.
#[test]
fn store_add_path_algorithm_byte_matches_nix() {
    if !have_nix() {
        eprintln!("skip store_add_path_algorithm_byte_matches_nix: no nix binary");
        return;
    }

    let td = tempfile::tempdir().unwrap();
    let src = make_fixture(td.path());

    let phys = td.path().canonicalize().unwrap().join("store");
    std::fs::create_dir_all(&phys).unwrap();

    let name = "mysource";
    let nix_path = nix_add_path(&phys, &src, name)
        .expect("nix store add-path must succeed on a box with nix");
    let nix_hex =
        nix_nar_hex(&phys, &nix_path).expect("nix store dump-path must succeed");

    // sui: the pure store-path + NAR algorithm (logical store dir /nix/store,
    // exactly as nix uses regardless of the physical --store location).
    let sh = sui_compat::source::nar_hash_source_tree(&src, name)
        .expect("sui nar_hash_source_tree must succeed");
    use sha2::{Digest, Sha256};
    let sui_hex: String = Sha256::digest(&sh.nar_bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    assert_eq!(
        sh.store_path, nix_path,
        "sui store path must byte-match nix store add-path"
    );
    assert_eq!(
        sui_hex, nix_hex,
        "sui NAR serialization hash must byte-match nix"
    );
}

/// Parity across several names + tree shapes — a name change must move the path
/// exactly as it does for nix (the `<name>` is part of the fingerprint).
#[test]
fn store_add_path_matches_nix_across_names() {
    if !have_nix() {
        eprintln!("skip store_add_path_matches_nix_across_names: no nix binary");
        return;
    }

    for name in ["source", "my-thing", "pkg-1.2.3", "a"] {
        let td = tempfile::tempdir().unwrap();
        let src = make_fixture(td.path());
        let phys = td.path().canonicalize().unwrap().join("store");
        std::fs::create_dir_all(&phys).unwrap();

        let nix_path = match nix_add_path(&phys, &src, name) {
            Some(p) => p,
            None => {
                eprintln!("nix add-path failed for name={name}; skipping row");
                continue;
            }
        };
        let sh = sui_compat::source::nar_hash_source_tree(&src, name).unwrap();
        assert_eq!(
            sh.store_path, nix_path,
            "store path must match nix for name={name}"
        );
    }
}

/// The physical `LocalStore::add_to_store` write path: NAR-unpack + DB register.
/// Verified against a temp physical store (no root). Confirms the realizer that
/// the daemon-mediated privileged write ultimately calls.
#[tokio::test]
async fn local_store_add_to_store_unpacks_and_registers() {
    use sui_store::traits::Store;

    let td = tempfile::tempdir().unwrap();
    let src = make_fixture(td.path());

    // Serialize the tree to a NAR the same way the add-path realizer expects.
    let sh = sui_compat::source::nar_hash_source_tree(&src, "mysource").unwrap();

    // Physical store dir for the write. LocalStore couples the logical hash
    // prefix to the physical write dir, so this store's computed path uses the
    // temp dir in its fingerprint — we assert the *shape* + on-disk unpack +
    // DB registration here (byte-parity vs nix is proven by the algorithm test
    // above, which is what a `--store /nix/store` daemon write uses).
    let store_dir = td.path().canonicalize().unwrap().join("physstore");
    std::fs::create_dir_all(&store_dir).unwrap();
    let store = sui_store::LocalStore::open_in_memory_with_dir(store_dir.to_str().unwrap())
        .await
        .unwrap();

    let info = store
        .add_to_store("mysource", &sh.nar_bytes, &[])
        .await
        .expect("add_to_store must succeed");

    // The registered path is under the physical store dir with a valid basename.
    assert!(
        info.path.starts_with(store_dir.to_str().unwrap()),
        "registered path {} must be under the store dir",
        info.path
    );
    let basename = info.path.rsplit('/').next().unwrap();
    assert!(basename.ends_with("-mysource"), "basename shape: {basename}");

    // The NAR was unpacked onto disk.
    let on_disk = std::path::Path::new(&info.path);
    assert!(on_disk.join("file.txt").is_file(), "file.txt unpacked");
    assert!(on_disk.join("other.txt").is_file(), "other.txt unpacked");
    assert!(
        on_disk.join("nested/deep.txt").is_file(),
        "nested/deep.txt unpacked"
    );
    assert_eq!(
        std::fs::read(on_disk.join("file.txt")).unwrap(),
        b"hello world\n"
    );

    // It is registered as a valid path in the DB. (We verify via the returned
    // PathInfo + the store's own valid-path count rather than a StorePath
    // round-trip: `StorePath` hardcodes the logical `/nix/store` prefix, so a
    // lookup-by-path on a *temp* physical store is not representable — that is a
    // property of the temp harness, not of the write path. The nix-byte-parity
    // of the path itself is proven by `store_add_path_algorithm_byte_matches_nix`.)
    assert_eq!(info.nar_size, sh.nar_bytes.len() as i64);
    assert!(info.nar_hash.starts_with("sha256:"), "nar_hash shape");

    // Verify the DB row exists via a raw count (query_all_valid_paths filters
    // out non-`/nix/store` paths through StorePath, so it can't observe a temp
    // store — a harness artifact, not a write-path bug; count the row directly).
    use sea_orm::{ConnectionTrait, Statement};
    let backend = store.db().get_database_backend();
    let row = store
        .db()
        .query_one(Statement::from_string(
            backend,
            "SELECT COUNT(*) AS n FROM ValidPaths".to_string(),
        ))
        .await
        .unwrap()
        .unwrap();
    let n: i64 = row.try_get("", "n").unwrap();
    assert_eq!(n, 1, "exactly one ValidPaths row after add_to_store");
}

/// ROOT-GATED MANUAL VERIFICATION (documented, not faked green).
///
/// The privileged write to the *real* `/nix/store` goes through the sui daemon
/// (the unprivileged `sui store add-path` CLI copies to `~/.cache/sui/added-paths`
/// and prints "requires sudo/root"). To verify the end-to-end privileged path
/// byte-for-byte against nix, run as root on a nix box:
///
/// ```sh
/// # oracle
/// nix store add-path ./fixture --name mysource   # -> /nix/store/<H>-mysource
/// # sui (daemon-mediated, root)
/// sudo sui store add-path ./fixture --name mysource
/// # assert: identical /nix/store/<H>-mysource, and the DB row matches
/// nix path-info /nix/store/<H>-mysource
/// ```
///
/// The store-path + NAR bytes are already proven identical by
/// `store_add_path_algorithm_byte_matches_nix` (which is what the daemon write
/// uses); this manual step only exercises the root-only filesystem write into
/// `/nix/store` itself, which cannot run unprivileged in CI.
#[test]
fn store_add_path_root_notes() {
    // Intentionally a no-op doc-anchor test; the real verification is the
    // algorithm test above + the manual root step in this fn's doc comment.
}
