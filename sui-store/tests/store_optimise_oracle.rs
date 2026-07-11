//! Oracle differential: `sui` store-optimise vs the real `nix-store
//! --optimise`.
//!
//! Parity Method (theory/BUILD.md §II.1). `nix-store --optimise` deduplicates
//! identical-content files across the store by replacing them with hard links
//! to a single inode. This differential proves sui's `optimise_store` produces
//! the *same* inode-sharing outcome nix does — identical-content files share an
//! inode, distinct-content files do not — and that optimise NEVER mutates file
//! content (it only hard-links). Runs against a temp store, no root required.
//!
//! The test drives BOTH engines over the same content layout:
//!   - the nix arm materializes a temp nix store + runs `nix-store --optimise`,
//!   - the sui arm materializes an equivalent sui store + runs `optimise_store`,
//! and asserts the *outcome invariants* nix establishes hold identically for
//! sui: (dup files: same inode) ∧ (uniq files: distinct inodes) ∧ (bytes
//! unchanged). Because optimise is a pure filesystem-dedup operation, these
//! invariants are the complete behavioral contract — matching them is matching
//! nix.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use sui_store::traits::Store;

fn have_nix() -> bool {
    Command::new("nix")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn inode(p: &Path) -> u64 {
    std::fs::metadata(p).unwrap().ino()
}

/// The nix oracle: two packages, each with an identical `dup.txt` and a
/// distinct `uniq.txt`; run `nix-store --optimise`; assert the inode-sharing
/// outcome. Returns the two package dirs for the caller to double-check.
fn nix_optimise_outcome(base: &Path) {
    let store = base.join("nixstore");
    std::fs::create_dir_all(&store).unwrap();

    let mk = |name: &str, uniq: &str| -> String {
        let src = base.join(format!("src_{name}"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("dup.txt"), b"shared content xyz\n").unwrap();
        std::fs::write(src.join("uniq.txt"), uniq).unwrap();
        let out = Command::new("nix")
            .args([
                "--extra-experimental-features",
                "nix-command flakes",
                "store",
                "add-path",
                "--store",
            ])
            .arg(&store)
            .arg(&src)
            .args(["--name", name])
            .output()
            .unwrap();
        assert!(out.status.success(), "nix add-path {name}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let pa = mk("pkgA", "unique a\n");
    let pb = mk("pkgB", "unique b\n");
    let phys = |logical: &str| store.join("nix/store").join(logical.rsplit('/').next().unwrap());
    let da = phys(&pa);
    let db = phys(&pb);

    // Before: dup.txt inodes differ.
    assert_ne!(
        inode(&da.join("dup.txt")),
        inode(&db.join("dup.txt")),
        "nix pre-optimise: dup inodes differ"
    );

    let out = Command::new("nix-store")
        .args(["--store"])
        .arg(&store)
        .arg("--optimise")
        .output()
        .unwrap();
    assert!(out.status.success(), "nix-store --optimise");

    // After: dup.txt shares one inode; uniq stays distinct; content unchanged.
    assert_eq!(
        inode(&da.join("dup.txt")),
        inode(&db.join("dup.txt")),
        "nix post-optimise: dup files must share an inode"
    );
    assert_ne!(
        inode(&da.join("uniq.txt")),
        inode(&db.join("uniq.txt")),
        "nix post-optimise: distinct-content files must NOT share an inode"
    );
    assert_eq!(
        std::fs::read(da.join("dup.txt")).unwrap(),
        b"shared content xyz\n",
        "nix: content byte-unchanged"
    );
}

/// The sui arm: the same layout, run `optimise_store`, assert the same
/// invariants nix established.
async fn sui_optimise_outcome(base: &Path) {
    use sui_compat::nar::{NarEntry, NarNode, NarWriter};

    let store_dir = base.join("suistore/nix/store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let store = sui_store::LocalStore::open_in_memory_with_dir(store_dir.to_str().unwrap())
        .await
        .unwrap();

    let mk_nar = |uniq: &[u8]| {
        let node = NarNode::Directory {
            entries: vec![
                NarEntry {
                    name: "dup.txt".to_string(),
                    node: NarNode::Regular {
                        executable: false,
                        contents: b"shared content xyz\n".to_vec(),
                    },
                },
                NarEntry {
                    name: "uniq.txt".to_string(),
                    node: NarNode::Regular {
                        executable: false,
                        contents: uniq.to_vec(),
                    },
                },
            ],
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        buf
    };

    let ia = store.add_to_store("pkgA", &mk_nar(b"unique a\n"), &[]).await.unwrap();
    let ib = store.add_to_store("pkgB", &mk_nar(b"unique b\n"), &[]).await.unwrap();
    let da = Path::new(&ia.path);
    let db = Path::new(&ib.path);

    // Before optimise: dup inodes differ (fresh unpack).
    assert_ne!(
        inode(&da.join("dup.txt")),
        inode(&db.join("dup.txt")),
        "sui pre-optimise: dup inodes differ"
    );

    let res = store.optimise_store(false).await.unwrap();
    assert_eq!(res.files_linked, 1, "sui: exactly one dup file hard-linked");
    assert!(res.bytes_saved > 0, "sui: bytes saved by dedup");

    // After optimise: the invariants nix establishes must hold for sui too.
    assert_eq!(
        inode(&da.join("dup.txt")),
        inode(&db.join("dup.txt")),
        "sui post-optimise: dup files must share an inode (== nix)"
    );
    assert_ne!(
        inode(&da.join("uniq.txt")),
        inode(&db.join("uniq.txt")),
        "sui post-optimise: distinct-content files must NOT share an inode (== nix)"
    );
    // Content byte-unchanged — optimise only hard-links, never mutates bytes.
    assert_eq!(
        std::fs::read(da.join("dup.txt")).unwrap(),
        b"shared content xyz\n",
        "sui: dup content byte-unchanged"
    );
    assert_eq!(
        std::fs::read(da.join("uniq.txt")).unwrap(),
        b"unique a\n",
        "sui: uniq content byte-unchanged"
    );
    assert_eq!(
        std::fs::read(db.join("uniq.txt")).unwrap(),
        b"unique b\n",
        "sui: uniq content byte-unchanged"
    );
}

#[tokio::test]
async fn store_optimise_matches_nix() {
    if !have_nix() {
        eprintln!("skip store_optimise_matches_nix: no nix binary");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let base = td.path().canonicalize().unwrap();

    // Prove the nix oracle establishes the outcome (dup shares inode, uniq
    // does not, content unchanged) on this layout...
    nix_optimise_outcome(&base);
    // ...then prove sui establishes the identical outcome on the same layout.
    sui_optimise_outcome(&base).await;
}

/// Idempotency: a second optimise pass links nothing (nix behaves the same —
/// already-linked files are skipped, `nlink > 1`).
#[tokio::test]
async fn store_optimise_is_idempotent() {
    let td = tempfile::tempdir().unwrap();
    let base = td.path().canonicalize().unwrap();
    let store_dir = base.join("suistore/nix/store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let store = sui_store::LocalStore::open_in_memory_with_dir(store_dir.to_str().unwrap())
        .await
        .unwrap();

    use sui_compat::nar::{NarEntry, NarNode, NarWriter};
    let mk_nar = |uniq: &[u8]| {
        let node = NarNode::Directory {
            entries: vec![
                NarEntry {
                    name: "dup.txt".to_string(),
                    node: NarNode::Regular {
                        executable: false,
                        contents: b"shared content xyz\n".to_vec(),
                    },
                },
                NarEntry {
                    name: "uniq.txt".to_string(),
                    node: NarNode::Regular {
                        executable: false,
                        contents: uniq.to_vec(),
                    },
                },
            ],
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        buf
    };
    let _ = store.add_to_store("pkgA", &mk_nar(b"a\n"), &[]).await.unwrap();
    let _ = store.add_to_store("pkgB", &mk_nar(b"b\n"), &[]).await.unwrap();

    let first = store.optimise_store(false).await.unwrap();
    assert_eq!(first.files_linked, 1, "first pass links the one dup");
    let second = store.optimise_store(false).await.unwrap();
    assert_eq!(second.files_linked, 0, "second pass links nothing (idempotent)");
}
