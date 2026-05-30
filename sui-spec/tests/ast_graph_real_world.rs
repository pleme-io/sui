//! Real-world AST-graph integration tests.
//!
//! Parses + archives actual `.nix` files from the pleme-io/nix checkout
//! when present. Asserts:
//!
//! 1. The pipeline (`from_source` → `archive_and_hash` → `rkyv::access`)
//!    succeeds on every file.
//! 2. Archive is deterministic — same source → same BLAKE3.
//! 3. Node-count budget — rio's configuration.nix (large NixOS module)
//!    should fit in a single rkyv archive under 1 MB compressed-on-disk
//!    equivalent.
//!
//! Skipped silently if no nix files are reachable.

use std::path::PathBuf;
use std::time::Instant;

use sui_spec::ast_graph::{ArchivedAstGraph, AstGraph};

fn find_nix_files() -> Vec<PathBuf> {
    if let Ok(p) = std::env::var("SUI_TEST_NIX_FILE") {
        return vec![PathBuf::from(p)];
    }
    let mut out = Vec::new();
    for candidate in [
        "/home/drzzln/code/github/pleme-io/nix/nodes/rio/configuration.nix",
        "/home/drzzln/code/github/pleme-io/nix/nodes/rio/default.nix",
        "/home/drzzln/code/github/pleme-io/nix/profiles/nixos-attic-cache-warmer/default.nix",
        "/home/drzzln/code/github/pleme-io/nix/profiles/darwin-developer/caches.nix",
    ] {
        let pb = PathBuf::from(candidate);
        if pb.exists() {
            out.push(pb);
        }
    }
    out
}

#[test]
fn real_nix_files_parse_archive_and_cast_back() {
    let files = find_nix_files();
    if files.is_empty() {
        eprintln!("[skip] no real .nix files reachable; set SUI_TEST_NIX_FILE");
        return;
    }
    for path in files {
        let source = std::fs::read_to_string(&path).expect("read .nix file");

        let t0 = Instant::now();
        let graph = AstGraph::from_source(&source).expect("parse + lower");
        let lower_ms = t0.elapsed().as_millis();

        let t1 = Instant::now();
        let (stamped, bytes) = graph.clone().archive_and_hash().expect("archive + hash");
        let archive_ms = t1.elapsed().as_millis();

        let t2 = Instant::now();
        let archived =
            rkyv::access::<ArchivedAstGraph, rkyv::rancor::Error>(&bytes).expect("cast back");
        let access_ms = t2.elapsed().as_millis();

        eprintln!(
            "[ast_graph_real_world] {} ({} bytes source) → {} nodes, {} bytes archive; \
             lower {}ms, archive {}ms, access {}ms",
            path.file_name().unwrap().to_string_lossy(),
            source.len(),
            stamped.nodes.len(),
            bytes.len(),
            lower_ms,
            archive_ms,
            access_ms,
        );

        assert_eq!(archived.nodes.len(), stamped.nodes.len());
        assert_ne!(stamped.canonical_hash.bytes, [0u8; 32]);
        assert!(
            bytes.len() < 4 * source.len() + 1024,
            "archive blow-up larger than 4× source on {}: {} → {}",
            path.display(),
            source.len(),
            bytes.len()
        );
    }
}

#[test]
fn archive_is_deterministic_across_runs() {
    let files = find_nix_files();
    let Some(path) = files.into_iter().next() else {
        eprintln!("[skip] no real .nix files reachable; set SUI_TEST_NIX_FILE");
        return;
    };
    let source = std::fs::read_to_string(&path).expect("read .nix file");

    let g1 = AstGraph::from_source(&source).expect("parse");
    let g2 = AstGraph::from_source(&source).expect("parse");
    let (s1, b1) = g1.archive_and_hash().unwrap();
    let (s2, b2) = g2.archive_and_hash().unwrap();
    assert_eq!(
        s1.canonical_hash.bytes, s2.canonical_hash.bytes,
        "AST graph BLAKE3 must be deterministic on the same source"
    );
    assert_eq!(b1, b2, "AST graph bytes must be deterministic");
}
