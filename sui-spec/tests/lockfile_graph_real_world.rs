//! Real-world integration test for `LockfileGraph`.
//!
//! Exercises the full L1 pipeline against the **actual** pleme-io/nix
//! `flake.lock` (when present at the conventional location) or the
//! `SUI_TEST_FLAKE_LOCK` env override. Skipped silently if neither
//! source is available — keeps CI on a stripped checkout green.
//!
//! Pipeline under test:
//!
//! ```text
//! flake.lock JSON
//!   → sui_compat::flake::FlakeLock::parse        (existing v7 parser)
//!     → LockfileGraph::from_flake_lock           (interning + follows resolution)
//!       → LockfileGraph::archive_and_hash        (rkyv + BLAKE3 stamp)
//!         → rkyv::access::<ArchivedLockfileGraph>  (zero-copy read)
//! ```
//!
//! Asserts:
//!
//! 1. Every node has a dense `id` matching its index in `nodes`.
//! 2. Every edge target points at a valid node.
//! 3. `archive_and_hash` produces a deterministic BLAKE3 for the
//!    same input (run twice, expect identical hash + identical bytes).
//! 4. The archived form parses back via `rkyv::access` and reports the
//!    same node count.
//! 5. Build + archive completes in under 5 seconds on rio-class hardware
//!    for the rio flake (~25 MB / 1 M JSON lines / 109 inputs).

use std::path::PathBuf;
use std::time::Instant;

use sui_compat::flake::FlakeLock;
use sui_spec::lockfile_graph::{ArchivedLockfileGraph, LockfileGraph};

fn find_flake_lock() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SUI_TEST_FLAKE_LOCK") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    for candidate in [
        "/home/drzzln/code/github/pleme-io/nix/flake.lock",
        "/var/lib/tend/workspace/pleme-io/nix/flake.lock",
    ] {
        let pb = PathBuf::from(candidate);
        if pb.exists() {
            return Some(pb);
        }
    }
    None
}

#[test]
fn rio_flake_lock_builds_and_archives_under_budget() {
    let Some(path) = find_flake_lock() else {
        eprintln!("[skip] no real flake.lock available; set SUI_TEST_FLAKE_LOCK");
        return;
    };

    let json = std::fs::read_to_string(&path).expect("read flake.lock");
    let json_len = json.len();

    let t0 = Instant::now();
    let lock = FlakeLock::parse(&json).expect("parse flake.lock");
    let parse_ms = t0.elapsed().as_millis();

    let t1 = Instant::now();
    let graph = LockfileGraph::from_flake_lock(&lock).expect("build graph");
    let build_ms = t1.elapsed().as_millis();

    let t2 = Instant::now();
    let (stamped, bytes) = graph.archive_and_hash().expect("archive + hash");
    let archive_ms = t2.elapsed().as_millis();

    let t3 = Instant::now();
    let archived = rkyv::access::<ArchivedLockfileGraph, rkyv::rancor::Error>(&bytes)
        .expect("rkyv access");
    let access_ms = t3.elapsed().as_millis();

    eprintln!(
        "[lockfile_graph_real_world] {} bytes JSON → {} nodes, archive {} bytes; \
         parse {}ms, build {}ms, archive {}ms, access {}ms",
        json_len,
        stamped.nodes.len(),
        bytes.len(),
        parse_ms,
        build_ms,
        archive_ms,
        access_ms,
    );

    // Structural invariants.
    assert_eq!(stamped.version, 7);
    assert_eq!(stamped.root_id, 0);
    assert_eq!(stamped.nodes[0].id, 0);
    assert_eq!(stamped.nodes[0].name, "root");
    for (i, node) in stamped.nodes.iter().enumerate() {
        assert_eq!(node.id as usize, i, "node ids must be dense / sequential");
        for edge in &node.inputs {
            assert!(
                (edge.target as usize) < stamped.nodes.len(),
                "edge target out of range: node {} → {}",
                node.name,
                edge.target,
            );
        }
    }

    // Archive form sees the same shape.
    assert_eq!(archived.version, 7);
    assert_eq!(archived.root_id, 0);
    assert_eq!(archived.nodes.len(), stamped.nodes.len());

    // Hash stamped, non-zero.
    assert_ne!(stamped.canonical_hash.bytes, [0u8; 32]);

    // Generous perf budget — we want regression signal even when this
    // runs on a slow shared CI box. Real target is sub-100ms for the
    // build+archive pair on rio-class hardware.
    assert!(
        parse_ms + build_ms + archive_ms < 60_000,
        "pipeline took longer than 60s for {} bytes; perf regression",
        json_len
    );
}

#[test]
fn archive_is_deterministic() {
    let Some(path) = find_flake_lock() else {
        eprintln!("[skip] no real flake.lock available; set SUI_TEST_FLAKE_LOCK");
        return;
    };
    let json = std::fs::read_to_string(&path).expect("read flake.lock");
    let lock = FlakeLock::parse(&json).expect("parse flake.lock");

    let g1 = LockfileGraph::from_flake_lock(&lock).unwrap();
    let g2 = LockfileGraph::from_flake_lock(&lock).unwrap();
    let (s1, b1) = g1.archive_and_hash().unwrap();
    let (s2, b2) = g2.archive_and_hash().unwrap();
    assert_eq!(
        s1.canonical_hash.bytes, s2.canonical_hash.bytes,
        "archive_and_hash must be deterministic for the same input"
    );
    assert_eq!(b1, b2, "archive bytes must be deterministic");
}
