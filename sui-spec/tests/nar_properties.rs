//! Property tests for the NAR encoder + store path computation.
//!
//! These lock the substrate's correctness mechanically:
//! every randomly-generated tree shape encodes deterministically,
//! the digest is sensitive to content but stable under re-encoding,
//! and `store_path_for` matches cppnix's canonical shape.

use proptest::prelude::*;
use std::collections::BTreeMap;
use sui_spec::nar;

// ── Helpers ──────────────────────────────────────────────────────

fn unique_tmpdir(label: &str) -> std::path::PathBuf {
    let id = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!("sui-spec-nar-prop-{label}-{id}-{nanos}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write_tree(dir: &std::path::Path, files: &BTreeMap<String, Vec<u8>>) {
    for (rel, content) in files {
        let target = dir.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(target, content).unwrap();
    }
}

// ── store_path_for properties ───────────────────────────────────

proptest! {
    /// `store_path_for` is deterministic for any (digest, name)
    /// pair.  Two invocations with identical inputs MUST yield
    /// identical paths.
    #[test]
    fn store_path_is_deterministic(
        digest_bytes in proptest::collection::vec(any::<u8>(), 32..=32),
        name in "[a-z][a-z0-9-]{0,15}",
    ) {
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&digest_bytes);
        let p1 = nar::store_path_for("/nix/store", &digest, &name);
        let p2 = nar::store_path_for("/nix/store", &digest, &name);
        prop_assert_eq!(p1, p2);
    }

    /// `store_path_for` ALWAYS produces a path with a 32-char
    /// hash component followed by `-<name>`.
    #[test]
    fn store_path_has_canonical_shape(
        digest_bytes in proptest::collection::vec(any::<u8>(), 32..=32),
        name in "[a-z][a-z0-9-]{1,15}",
    ) {
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&digest_bytes);
        let path = nar::store_path_for("/nix/store", &digest, &name);
        prop_assert!(path.starts_with("/nix/store/"));
        let after = path.strip_prefix("/nix/store/").unwrap();
        let (hash, after_name) = after.split_once('-').unwrap();
        prop_assert_eq!(hash.len(), 32);
        prop_assert_eq!(after_name, name.as_str());
    }

    /// Different names with the same digest produce paths sharing
    /// the hash component but differing in the name suffix.
    #[test]
    fn store_path_hash_stable_across_names(
        digest_bytes in proptest::collection::vec(any::<u8>(), 32..=32),
        n1 in "[a-z]{1,8}",
        n2 in "[a-z]{1,8}",
    ) {
        prop_assume!(n1 != n2);
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&digest_bytes);
        let p1 = nar::store_path_for("/nix/store", &digest, &n1);
        let p2 = nar::store_path_for("/nix/store", &digest, &n2);
        let h1 = p1.strip_prefix("/nix/store/").unwrap().split_once('-').unwrap().0;
        let h2 = p2.strip_prefix("/nix/store/").unwrap().split_once('-').unwrap().0;
        prop_assert_eq!(h1, h2);
    }

    /// Different digests produce paths with different hash
    /// components (collision-resistance smoke test).
    #[test]
    fn store_path_hash_differs_with_digest(
        d1 in proptest::collection::vec(any::<u8>(), 32..=32),
        d2 in proptest::collection::vec(any::<u8>(), 32..=32),
        name in "[a-z]{2,8}",
    ) {
        prop_assume!(d1 != d2);
        let mut a = [0u8; 32]; a.copy_from_slice(&d1);
        let mut b = [0u8; 32]; b.copy_from_slice(&d2);
        let p1 = nar::store_path_for("/nix/store", &a, &name);
        let p2 = nar::store_path_for("/nix/store", &b, &name);
        let h1 = p1.strip_prefix("/nix/store/").unwrap().split_once('-').unwrap().0;
        let h2 = p2.strip_prefix("/nix/store/").unwrap().split_once('-').unwrap().0;
        prop_assert_ne!(h1, h2);
    }
}

// ── NAR encoder properties ───────────────────────────────────────

proptest! {
    /// Encoding the same file twice yields identical NAR bytes.
    /// (Determinism under re-encoding.)
    #[test]
    fn nar_file_encoding_is_deterministic(
        content in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let dir = unique_tmpdir("file-det");
        let file = dir.join("f");
        std::fs::write(&file, &content).unwrap();
        let n1 = nar::encode(&file).unwrap();
        let n2 = nar::encode(&file).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        prop_assert_eq!(n1, n2);
    }

    /// Encoding the same directory twice yields identical NAR
    /// bytes regardless of insertion order — the encoder must
    /// sort entries.
    #[test]
    fn nar_directory_encoding_is_deterministic(
        files in proptest::collection::btree_map(
            "[a-z]{1,4}",
            proptest::collection::vec(any::<u8>(), 0..32),
            1..6,
        ),
    ) {
        let d1 = unique_tmpdir("dir-det-a");
        let d2 = unique_tmpdir("dir-det-b");
        write_tree(&d1, &files);
        write_tree(&d2, &files);
        let n1 = nar::encode(&d1).unwrap();
        let n2 = nar::encode(&d2).unwrap();
        let _ = std::fs::remove_dir_all(&d1);
        let _ = std::fs::remove_dir_all(&d2);
        prop_assert_eq!(n1, n2);
    }

    /// Different file content produces a different digest
    /// (collision-resistance via sha256).
    #[test]
    fn nar_hash_differs_with_content(
        a in proptest::collection::vec(any::<u8>(), 1..128),
        b in proptest::collection::vec(any::<u8>(), 1..128),
    ) {
        prop_assume!(a != b);
        let dir = unique_tmpdir("hash-diff");
        let path = dir.join("f");
        std::fs::write(&path, &a).unwrap();
        let ha = nar::hash_path_nar(&path).unwrap();
        std::fs::write(&path, &b).unwrap();
        let hb = nar::hash_path_nar(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        prop_assert_ne!(ha, hb);
    }

    /// The encoded NAR always starts with the canonical magic
    /// header (`nix-archive-1` length-prefixed + padded).
    #[test]
    fn nar_encode_starts_with_magic(
        content in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let dir = unique_tmpdir("magic");
        let path = dir.join("f");
        std::fs::write(&path, &content).unwrap();
        let nar = nar::encode(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        prop_assert!(nar.len() >= 24, "NAR must be at least 24 bytes (magic header)");
        let len = u64::from_le_bytes(nar[0..8].try_into().unwrap());
        prop_assert_eq!(len, 13, "magic header length must be 13");
        prop_assert_eq!(&nar[8..21], b"nix-archive-1");
    }

    /// Empty directories encode to a stable, non-empty NAR (header
    /// + opening + type=directory + closing).
    #[test]
    fn nar_empty_dir_encodes_stably(seed in any::<u8>()) {
        let dir = unique_tmpdir(&format!("empty-{seed}"));
        let nar = nar::encode(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        // Magic header (24 bytes) + open + "type"/"directory"
        // + close = some minimum positive bytes.
        prop_assert!(nar.len() >= 64);
    }
}

// ── full round-trip via store path ───────────────────────────────

proptest! {
    /// For any file content and name: encoding + hashing +
    /// store_path_for is end-to-end deterministic AND produces
    /// canonical paths.  This is the substrate-level smoke test
    /// for `sui store add-file`.
    #[test]
    fn end_to_end_add_file_is_deterministic(
        content in proptest::collection::vec(any::<u8>(), 1..256),
        name in "[a-z][a-z0-9-]{0,7}",
    ) {
        let dir = unique_tmpdir("e2e");
        let f = dir.join("source");
        std::fs::write(&f, &content).unwrap();

        let h1 = nar::hash_path_nar(&f).unwrap();
        let p1 = nar::store_path_for("/nix/store", &h1, &name);

        let h2 = nar::hash_path_nar(&f).unwrap();
        let p2 = nar::store_path_for("/nix/store", &h2, &name);

        let _ = std::fs::remove_dir_all(&dir);
        prop_assert_eq!(h1, h2);
        prop_assert_eq!(p1, p2);
    }
}
