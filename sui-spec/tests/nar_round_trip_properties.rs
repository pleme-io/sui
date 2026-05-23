//! Property tests for the NAR decoder + encode⇄decode round-trip.
//!
//! For every randomly-generated filesystem tree, the substrate's
//! NAR encode → decode → re-encode pipeline must be byte-equal.
//! This is the canonical invariant the materialize command relies
//! on, locked at substrate level for every future change.

use proptest::prelude::*;
use std::collections::BTreeMap;
use sui_spec::nar;

fn unique_tmpdir(label: &str) -> std::path::PathBuf {
    let id = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!("sui-nar-rt-{label}-{id}-{nanos}"));
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

proptest! {
    /// Decode of encode(file) produces a byte-identical file.
    #[test]
    fn file_encode_decode_roundtrip(
        content in proptest::collection::vec(any::<u8>(), 0..1024),
    ) {
        let src_dir = unique_tmpdir("file-src");
        let src = src_dir.join("f");
        std::fs::write(&src, &content).unwrap();
        let nar = nar::encode(&src).unwrap();

        let dst = unique_tmpdir("file-dst").join("f");
        nar::decode(&nar, &dst).unwrap();
        let got = std::fs::read(&dst).unwrap();

        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(dst.parent().unwrap());
        prop_assert_eq!(got, content);
    }

    /// Decode of encode(directory) produces byte-identical contents.
    #[test]
    fn directory_encode_decode_roundtrip(
        files in proptest::collection::btree_map(
            "[a-z]{1,5}",
            proptest::collection::vec(any::<u8>(), 0..64),
            0..6,
        ),
    ) {
        let src = unique_tmpdir("dir-src");
        write_tree(&src, &files);
        let nar = nar::encode(&src).unwrap();

        let dst = unique_tmpdir("dir-dst");
        let _ = std::fs::remove_dir_all(&dst);
        nar::decode(&nar, &dst).unwrap();

        for (rel, content) in &files {
            let got = std::fs::read(dst.join(rel)).unwrap();
            prop_assert_eq!(got, content.clone());
        }

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    /// Full round-trip: encode → decode → encode produces
    /// byte-identical NAR bytes (canonical encoding).
    #[test]
    fn nar_bytes_are_canonical(
        files in proptest::collection::btree_map(
            "[a-z]{1,5}",
            proptest::collection::vec(any::<u8>(), 0..64),
            1..5,
        ),
    ) {
        let src = unique_tmpdir("canon-src");
        write_tree(&src, &files);
        let nar1 = nar::encode(&src).unwrap();

        let dst = unique_tmpdir("canon-dst");
        let _ = std::fs::remove_dir_all(&dst);
        nar::decode(&nar1, &dst).unwrap();
        let nar2 = nar::encode(&dst).unwrap();

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
        prop_assert_eq!(nar1, nar2);
    }

    /// hash_path_nar is stable through round-trip.
    #[test]
    fn nar_hash_is_stable_through_roundtrip(
        content in proptest::collection::vec(any::<u8>(), 1..256),
    ) {
        let src_dir = unique_tmpdir("hash-src");
        let src = src_dir.join("f");
        std::fs::write(&src, &content).unwrap();
        let h1 = nar::hash_path_nar(&src).unwrap();

        let nar = nar::encode(&src).unwrap();
        let dst_dir = unique_tmpdir("hash-dst");
        let dst = dst_dir.join("f");
        nar::decode(&nar, &dst).unwrap();
        let h2 = nar::hash_path_nar(&dst).unwrap();

        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&dst_dir);
        prop_assert_eq!(h1, h2);
    }
}

// ── ParsedNar walk + at_path tests ──────────────────────────

proptest! {
    /// ParsedNar::parse(nar)::serialize() == nar (substrate-level
    /// round-trip; tests the typed walker + emitter symmetry).
    #[test]
    fn parsed_nar_serialize_roundtrips(
        files in proptest::collection::btree_map(
            "[a-z]{1,5}",
            proptest::collection::vec(any::<u8>(), 0..64),
            0..5,
        ),
    ) {
        let src = unique_tmpdir("parsed-rt");
        write_tree(&src, &files);
        let nar1 = nar::encode(&src).unwrap();
        let parsed = sui_spec::store_ops::ParsedNar::parse(&nar1).unwrap();
        let nar2 = parsed.serialize();
        let _ = std::fs::remove_dir_all(&src);
        prop_assert_eq!(nar1, nar2);
    }

    /// ParsedNar::file_count counts only files (not dirs/symlinks).
    #[test]
    fn parsed_nar_file_count_matches_input(
        n in 1usize..8,
    ) {
        let dir = unique_tmpdir("parsed-count");
        let mut expected_files = 0usize;
        for i in 0..n {
            std::fs::write(dir.join(format!("f{i}")), [i as u8]).unwrap();
            expected_files += 1;
        }
        let nar = nar::encode(&dir).unwrap();
        let parsed = sui_spec::store_ops::ParsedNar::parse(&nar).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        prop_assert_eq!(parsed.root.file_count(), expected_files);
    }
}
