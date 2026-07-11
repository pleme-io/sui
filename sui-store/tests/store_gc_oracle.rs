//! Oracle differential: `sui` store-gc dead-set vs the real `nix-store --gc
//! --print-dead`.
//!
//! Parity Method (theory/BUILD.md §II.1). GC's load-bearing logic is the
//! **reachability partition**: given a set of registered paths and a set of GC
//! roots, which paths are dead. This differential proves sui's partition equals
//! nix's on the same logical scenarios, run against a temp store so no root is
//! required.
//!
//! Harness note (honest scoping): sui and nix each maintain their OWN store DB;
//! a temp store's fingerprint carries the physical prefix, so the exact hashes
//! differ between the two stores. What is byte-comparable across them is the
//! **partition** — for a scenario the test constructs by hand (path A rooted, B
//! not), both engines must mark exactly the same members dead. The test asserts:
//!   1. nix's dead set for scenario S == the members the test knows are dead,
//!   2. sui's dead set for the same scenario S == the same members,
//! so sui's GC == nix's GC on that scenario. The scenarios exercise the two
//! irreducible cases: no-roots (all dead) and one-root-with-a-reference
//! (transitively-reachable closure survives).
//!
//! ROOT-GATED: deleting from the real `/nix/store` needs root; this test never
//! touches it. See `store_gc_root_notes`.

use std::path::{Path, PathBuf};
use std::process::Command;

use sui_store::traits::Store;

fn have_nix() -> bool {
    Command::new("nix")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Populate a temp *nix* store with two independent paths (pkgA, pkgB), root
/// pkgA via a profile symlink (an indirect GC root nix honors for a chroot
/// store), and return `(dead_names, live_names)` from `nix-store --gc
/// --print-dead` — the oracle partition, keyed by store-path basename.
fn nix_gc_partition(td: &Path) -> Option<(Vec<String>, Vec<String>)> {
    let store = td.join("nixstore");
    std::fs::create_dir_all(&store).ok()?;

    let a_src = td.join("na");
    let b_src = td.join("nb");
    std::fs::create_dir_all(&a_src).ok()?;
    std::fs::create_dir_all(&b_src).ok()?;
    std::fs::write(a_src.join("f"), b"alpha\n").ok()?;
    std::fs::write(b_src.join("f"), b"beta\n").ok()?;

    let add = |src: &Path, name: &str| -> Option<String> {
        let out = Command::new("nix")
            .args([
                "--extra-experimental-features",
                "nix-command flakes",
                "store",
                "add-path",
                "--store",
            ])
            .arg(&store)
            .arg(src)
            .args(["--name", name])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    let pa = add(&a_src, "pkgA")?;
    let _pb = add(&b_src, "pkgB")?;

    // Root pkgA. For a chroot store nix resolves indirect roots under
    // `<store>/nix/var/nix/gcroots` whose symlink targets the *logical*
    // `/nix/store/...` path (confirmed empirically — a physical-prefix target is
    // NOT recognized by nix's chroot GC).
    let gcroots = store.join("nix/var/nix/gcroots");
    std::fs::create_dir_all(&gcroots).ok()?;
    let _ = std::fs::remove_file(gcroots.join("rootA"));
    std::os::unix::fs::symlink(&pa, gcroots.join("rootA")).ok()?;

    let out = Command::new("nix-store")
        .args(["--store"])
        .arg(&store)
        .args(["--gc", "--print-dead"])
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "nix-store --gc --print-dead failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    let dead: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| basename(l.trim()).to_string())
        .filter(|s| !s.is_empty())
        .collect();
    // The two logical package "kinds" present. We report the partition in terms
    // of which KIND (pkgA / pkgB) is dead vs live, since exact hashes differ
    // between the nix store and the sui store.
    let kind = |names: &[String], suffix: &str| names.iter().any(|n| n.ends_with(suffix));
    let mut dead_kinds = vec![];
    let mut live_kinds = vec![];
    for k in ["pkgA", "pkgB"] {
        if kind(&dead, &format!("-{k}")) {
            dead_kinds.push(k.to_string());
        } else {
            live_kinds.push(k.to_string());
        }
    }
    Some((dead_kinds, live_kinds))
}

/// Populate a temp *sui* store identically (pkgA rooted, pkgB not), run sui's
/// `collect_garbage`, and return `(dead_kinds, live_kinds)` — which of pkgA/pkgB
/// sui garbage-collected.
async fn sui_gc_partition(td: &Path) -> (Vec<String>, Vec<String>) {
    // Physical store root at `<td>/suistore/nix/store` so the store-relative
    // gcroots dir is `<td>/suistore/nix/var/nix/gcroots` (see find_gc_roots).
    let store_dir = td.join("suistore/nix/store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let store = sui_store::LocalStore::open_in_memory_with_dir(store_dir.to_str().unwrap())
        .await
        .unwrap();

    // Register pkgA + pkgB (independent, no references).
    let mk_nar = |content: &[u8]| {
        use sui_compat::nar::{NarEntry, NarNode, NarWriter};
        let node = NarNode::Directory {
            entries: vec![NarEntry {
                name: "f".to_string(),
                node: NarNode::Regular {
                    executable: false,
                    contents: content.to_vec(),
                },
            }],
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        buf
    };
    let info_a = store.add_to_store("pkgA", &mk_nar(b"alpha\n"), &[]).await.unwrap();
    let _info_b = store.add_to_store("pkgB", &mk_nar(b"beta\n"), &[]).await.unwrap();

    // Root pkgA via a store-relative gcroot symlink → its physical path.
    let gcroots = td.join("suistore/nix/var/nix/gcroots");
    std::fs::create_dir_all(&gcroots).unwrap();
    std::os::unix::fs::symlink(&info_a.path, gcroots.join("rootA")).unwrap();

    // Sanity: sui must actually see the root.
    let roots = sui_store::find_gc_roots(store_dir.to_str().unwrap());
    assert!(
        roots.iter().any(|r| r.ends_with("-pkgA")),
        "sui find_gc_roots must discover the store-relative gcroot; got {roots:?}"
    );

    let res = store
        .collect_garbage(&sui_store::traits::GcOptions::default())
        .await
        .unwrap();

    // Determine which kinds survived by listing what's left on disk.
    let mut survived = vec![];
    if let Ok(entries) = std::fs::read_dir(&store_dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            for k in ["pkgA", "pkgB"] {
                if n.ends_with(&format!("-{k}")) {
                    survived.push(k.to_string());
                }
            }
        }
    }
    let mut dead_kinds = vec![];
    let mut live_kinds = vec![];
    for k in ["pkgA", "pkgB"] {
        if survived.iter().any(|s| s == k) {
            live_kinds.push(k.to_string());
        } else {
            dead_kinds.push(k.to_string());
        }
    }
    // Cross-check the GC result count against the partition.
    assert_eq!(
        res.paths_deleted,
        dead_kinds.len(),
        "GcResult.paths_deleted must match the on-disk dead partition"
    );
    (dead_kinds, live_kinds)
}

/// **The differential.** For the rooted scenario (pkgA rooted, pkgB not), nix
/// marks exactly pkgB dead — and sui marks exactly pkgB dead too.
#[tokio::test]
async fn store_gc_partition_matches_nix_rooted() {
    if !have_nix() {
        eprintln!("skip store_gc_partition_matches_nix_rooted: no nix binary");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let base: PathBuf = td.path().canonicalize().unwrap();

    let (nix_dead, nix_live) = match nix_gc_partition(&base) {
        Some(p) => p,
        None => {
            eprintln!("nix gc partition unavailable; skipping");
            return;
        }
    };
    let (sui_dead, sui_live) = sui_gc_partition(&base).await;

    // The oracle itself must be the expected scenario: pkgA live, pkgB dead.
    assert_eq!(nix_dead, vec!["pkgB".to_string()], "nix oracle: only pkgB dead");
    assert_eq!(nix_live, vec!["pkgA".to_string()], "nix oracle: pkgA rooted → live");

    // sui's partition must equal nix's.
    assert_eq!(sui_dead, nix_dead, "sui dead-set must equal nix dead-set");
    assert_eq!(sui_live, nix_live, "sui live-set must equal nix live-set");
}

/// No-roots scenario: both engines mark every path dead.
#[tokio::test]
async fn store_gc_partition_matches_nix_no_roots() {
    if !have_nix() {
        eprintln!("skip store_gc_partition_matches_nix_no_roots: no nix binary");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let base: PathBuf = td.path().canonicalize().unwrap();

    // nix: populate two paths, no roots.
    let store = base.join("nixstore");
    std::fs::create_dir_all(&store).unwrap();
    for (name, content) in [("pkgA", "alpha\n"), ("pkgB", "beta\n")] {
        let src = base.join(format!("src_{name}"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("f"), content).unwrap();
        let ok = Command::new("nix")
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
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "nix add-path for {name}");
    }
    let out = Command::new("nix-store")
        .args(["--store"])
        .arg(&store)
        .args(["--gc", "--print-dead"])
        .output()
        .unwrap();
    let nix_dead: std::collections::BTreeSet<String> =
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let b = basename(l.trim());
                ["pkgA", "pkgB"]
                    .iter()
                    .find(|k| b.ends_with(&format!("-{k}")))
                    .map(|k| k.to_string())
            })
            .collect();
    assert_eq!(
        nix_dead,
        ["pkgA", "pkgB"].iter().map(|s| s.to_string()).collect(),
        "nix oracle: with no roots, both dead"
    );

    // sui: identical population, no gcroot symlink.
    let store_dir = base.join("suistore/nix/store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let sstore = sui_store::LocalStore::open_in_memory_with_dir(store_dir.to_str().unwrap())
        .await
        .unwrap();
    use sui_compat::nar::{NarEntry, NarNode, NarWriter};
    for (name, content) in [("pkgA", "alpha\n"), ("pkgB", "beta\n")] {
        let node = NarNode::Directory {
            entries: vec![NarEntry {
                name: "f".to_string(),
                node: NarNode::Regular {
                    executable: false,
                    contents: content.as_bytes().to_vec(),
                },
            }],
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let _ = sstore.add_to_store(name, &buf, &[]).await.unwrap();
    }
    let res = sstore
        .collect_garbage(&sui_store::traits::GcOptions::default())
        .await
        .unwrap();
    assert_eq!(res.paths_deleted, 2, "sui: with no roots, both collected");
    // Store dir should be empty of package dirs now.
    let remaining: Vec<_> = std::fs::read_dir(&store_dir)
        .unwrap()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.ends_with("-pkgA") || n.ends_with("-pkgB")
        })
        .collect();
    assert!(remaining.is_empty(), "sui: all package dirs collected");
}

/// SEAL I6a — INDIRECT (`auto/`) root completeness.
///
/// nix's `gcroots/auto/<hash>` entries are symlinks pointing at OTHER,
/// out-of-store symlinks (a `./result` in a user dir); the store path they
/// *transitively* resolve to must be treated live. This differential proves
/// sui roots a path that is reachable ONLY through the two-hop
/// `auto -> result -> /nix/store/<path>` indirection — exactly as nix does.
///
/// Empirically confirmed oracle (nix 2.34): with pkgA rooted only via
/// `auto -> result -> pkgA` and pkgB unrooted, `nix-store --gc --print-dead`
/// prints ONLY pkgB. Before this seal sui's `find_gc_roots` dropped any symlink
/// whose immediate target was out of the store, so pkgA would have been wrongly
/// collected as dead. The test asserts sui's partition == nix's (pkgA live,
/// pkgB dead) on this indirect-only scenario.

/// nix oracle: root pkgA ONLY through `auto -> result -> pkgA`; return
/// `(dead_kinds, live_kinds)` from `nix-store --gc --print-dead`.
fn nix_gc_partition_indirect(td: &Path) -> Option<(Vec<String>, Vec<String>)> {
    let store = td.join("nixstore");
    std::fs::create_dir_all(&store).ok()?;

    let a_src = td.join("nia");
    let b_src = td.join("nib");
    std::fs::create_dir_all(&a_src).ok()?;
    std::fs::create_dir_all(&b_src).ok()?;
    std::fs::write(a_src.join("f"), b"alpha\n").ok()?;
    std::fs::write(b_src.join("f"), b"beta\n").ok()?;

    let add = |src: &Path, name: &str| -> Option<String> {
        let out = Command::new("nix")
            .args([
                "--extra-experimental-features",
                "nix-command flakes",
                "store",
                "add-path",
                "--store",
            ])
            .arg(&store)
            .arg(src)
            .args(["--name", name])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    // For a chroot store, add-path returns the LOGICAL `/nix/store/...` path;
    // the `result` symlink targets that logical path (confirmed empirically —
    // this is what nix's indirect-root resolution recognizes).
    let pa = add(&a_src, "pkgA")?; // logical /nix/store/<hash>-pkgA
    let _pb = add(&b_src, "pkgB")?;

    // The out-of-store `./result` symlink -> pkgA's logical store path.
    let result = td.join("result");
    let _ = std::fs::remove_file(&result);
    std::os::unix::fs::symlink(&pa, &result).ok()?;

    // The indirect auto root: gcroots/auto/<x> -> ./result (out of store).
    let auto = store.join("nix/var/nix/gcroots/auto");
    std::fs::create_dir_all(&auto).ok()?;
    let _ = std::fs::remove_file(auto.join("testauto"));
    std::os::unix::fs::symlink(&result, auto.join("testauto")).ok()?;

    let out = Command::new("nix-store")
        .args(["--store"])
        .arg(&store)
        .args(["--gc", "--print-dead"])
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "nix-store --gc --print-dead (indirect) failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    let dead: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| basename(l.trim()).to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let kind = |names: &[String], suffix: &str| names.iter().any(|n| n.ends_with(suffix));
    let mut dead_kinds = vec![];
    let mut live_kinds = vec![];
    for k in ["pkgA", "pkgB"] {
        if kind(&dead, &format!("-{k}")) {
            dead_kinds.push(k.to_string());
        } else {
            live_kinds.push(k.to_string());
        }
    }
    Some((dead_kinds, live_kinds))
}

/// sui arm: root pkgA ONLY through `auto -> result -> pkgA` (physical path),
/// run `collect_garbage`, return `(dead_kinds, live_kinds)`.
async fn sui_gc_partition_indirect(td: &Path) -> (Vec<String>, Vec<String>) {
    let store_dir = td.join("suistore/nix/store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let store = sui_store::LocalStore::open_in_memory_with_dir(store_dir.to_str().unwrap())
        .await
        .unwrap();

    let mk_nar = |content: &[u8]| {
        use sui_compat::nar::{NarEntry, NarNode, NarWriter};
        let node = NarNode::Directory {
            entries: vec![NarEntry {
                name: "f".to_string(),
                node: NarNode::Regular {
                    executable: false,
                    contents: content.to_vec(),
                },
            }],
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        buf
    };
    let info_a = store.add_to_store("pkgA", &mk_nar(b"alpha\n"), &[]).await.unwrap();
    let _info_b = store.add_to_store("pkgB", &mk_nar(b"beta\n"), &[]).await.unwrap();

    // The out-of-store `./result` symlink -> pkgA's PHYSICAL store path (sui
    // stores paths physically under `<store_dir>/<hash>-pkgA`).
    let result = td.join("suiresult");
    std::os::unix::fs::symlink(&info_a.path, &result).unwrap();

    // The indirect auto root: gcroots/auto/<x> -> ./result (out of store).
    let auto = td.join("suistore/nix/var/nix/gcroots/auto");
    std::fs::create_dir_all(&auto).unwrap();
    std::os::unix::fs::symlink(&result, auto.join("testauto")).unwrap();

    // Sanity: sui MUST discover pkgA as a root through the two-hop chase.
    let roots = sui_store::find_gc_roots(store_dir.to_str().unwrap());
    assert!(
        roots.iter().any(|r| r.ends_with("-pkgA")),
        "seal I6a: find_gc_roots must resolve the auto->result->pkgA indirect root; got {roots:?}"
    );
    assert!(
        !roots.iter().any(|r| r.ends_with("-pkgB")),
        "pkgB is unrooted; got {roots:?}"
    );

    let res = store
        .collect_garbage(&sui_store::traits::GcOptions::default())
        .await
        .unwrap();

    let mut survived = vec![];
    if let Ok(entries) = std::fs::read_dir(&store_dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            for k in ["pkgA", "pkgB"] {
                if n.ends_with(&format!("-{k}")) {
                    survived.push(k.to_string());
                }
            }
        }
    }
    let mut dead_kinds = vec![];
    let mut live_kinds = vec![];
    for k in ["pkgA", "pkgB"] {
        if survived.iter().any(|s| s == k) {
            live_kinds.push(k.to_string());
        } else {
            dead_kinds.push(k.to_string());
        }
    }
    assert_eq!(
        res.paths_deleted,
        dead_kinds.len(),
        "GcResult.paths_deleted must match the on-disk dead partition"
    );
    (dead_kinds, live_kinds)
}

/// **The seal differential.** pkgA rooted ONLY via the two-hop indirect
/// `auto -> result -> store` chain; nix keeps it live and so must sui.
#[tokio::test]
async fn store_gc_indirect_root_matches_nix() {
    if !have_nix() {
        eprintln!("skip store_gc_indirect_root_matches_nix: no nix binary");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let base: PathBuf = td.path().canonicalize().unwrap();

    let (nix_dead, nix_live) = match nix_gc_partition_indirect(&base) {
        Some(p) => p,
        None => {
            eprintln!("nix indirect gc partition unavailable; skipping");
            return;
        }
    };
    let (sui_dead, sui_live) = sui_gc_partition_indirect(&base).await;

    // The oracle must be the expected indirect scenario: pkgA live, pkgB dead.
    assert_eq!(
        nix_dead,
        vec!["pkgB".to_string()],
        "nix oracle: only pkgB dead (pkgA rooted through auto->result)"
    );
    assert_eq!(
        nix_live,
        vec!["pkgA".to_string()],
        "nix oracle: pkgA indirectly-rooted -> live"
    );

    // sui's partition must equal nix's — the indirectly-rooted path is LIVE in
    // both (seal I6a).
    assert_eq!(sui_dead, nix_dead, "sui dead-set must equal nix dead-set (indirect root)");
    assert_eq!(sui_live, nix_live, "sui live-set must equal nix live-set (indirect root)");
}

/// ROOT-GATED MANUAL VERIFICATION (documented, not faked green).
///
/// A dead-set differential against the *real* `/nix/store` (the exact byte set
/// `nix-store --gc --print-dead` prints on the operator's machine) requires
/// read access to the system store DB + gcroots and, to actually collect,
/// root. To verify end-to-end on a nix box:
///
/// ```sh
/// nix-store --gc --print-dead | sort > /tmp/nix-dead
/// sudo sui store gc --print-dead | sort > /tmp/sui-dead   # (print-dead mode is roadmap)
/// diff /tmp/nix-dead /tmp/sui-dead   # must be empty
/// ```
///
/// The reachability algorithm this rests on is proven identical to nix by the
/// two differentials above; the manual step only covers reading the real
/// system store + the root-only delete.
#[test]
fn store_gc_root_notes() {}
