//! End-to-end integration of the L1 substrate.
//!
//! Exercises: archive a typed graph via rkyv → write to GraphStore →
//! mmap on read → cast to `&Archived<T>` → traverse → verify hash.
//!
//! These tests use real filesystem operations under tempdir and are
//! the canonical "does the substrate actually work" check that gates
//! every commit.

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use sui_graph_store::{GraphHash, GraphKind, GraphStore};
use tempfile::tempdir;

#[derive(Archive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(derive(Debug))]
struct SampleGraph {
    version: u32,
    name: String,
    nodes: Vec<SampleNode>,
}

#[derive(Archive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(derive(Debug))]
struct SampleNode {
    id: u32,
    label: String,
    children: Vec<u32>,
}

fn sample() -> SampleGraph {
    SampleGraph {
        version: 7,
        name: "rio-flake-lock-fixture".into(),
        nodes: (0..16)
            .map(|i| SampleNode {
                id: i,
                label: format!("node-{i}"),
                children: (i + 1..(i + 5).min(16)).collect(),
            })
            .collect(),
    }
}

fn open_store() -> (tempfile::TempDir, GraphStore) {
    let dir = tempdir().unwrap();
    let store = GraphStore::open(dir.path().to_path_buf()).unwrap();
    (dir, store)
}

#[test]
fn rkyv_archive_roundtrips_through_store() {
    let (_dir, store) = open_store();
    let graph = sample();

    // Archive via rkyv (high API).
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&graph).unwrap();
    let hash = GraphHash::of(&bytes);

    store.put(GraphKind::Lockfile, hash, &bytes).unwrap();

    // Read back as mmap + zero-copy cast.
    let mapped = store.get(GraphKind::Lockfile, hash).unwrap();
    let archived = rkyv::access::<ArchivedSampleGraph, rkyv::rancor::Error>(&mapped).unwrap();
    assert_eq!(archived.version, 7);
    assert_eq!(archived.name.as_str(), "rio-flake-lock-fixture");
    assert_eq!(archived.nodes.len(), 16);
    assert_eq!(archived.nodes[5].id, 5);

    // Full deserialize to owned form for the round-trip check.
    let back: SampleGraph =
        rkyv::deserialize::<SampleGraph, rkyv::rancor::Error>(archived).unwrap();
    assert_eq!(back, graph);
}

#[test]
fn many_graphs_iterate_in_index() {
    let (_dir, store) = open_store();
    let mut hashes = Vec::new();
    for i in 0..50u32 {
        let mut g = sample();
        g.version = i;
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&g).unwrap();
        let hash = GraphHash::of(&bytes);
        store.put(GraphKind::Lockfile, hash, &bytes).unwrap();
        hashes.push(hash);
    }
    assert_eq!(store.len().unwrap(), 50);
    let keys = store.iter_keys().unwrap();
    assert_eq!(keys.len(), 50);
    for h in hashes {
        assert!(keys.contains(&(GraphKind::Lockfile, h)));
    }
}

#[test]
fn store_survives_reopen() {
    let dir = tempdir().unwrap();
    let graph = sample();
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&graph).unwrap();
    let hash = GraphHash::of(&bytes);

    {
        let store = GraphStore::open(dir.path().to_path_buf()).unwrap();
        store.put(GraphKind::Lockfile, hash, &bytes).unwrap();
    } // store dropped — redb file closed, blob fsynced

    {
        let store = GraphStore::open(dir.path().to_path_buf()).unwrap();
        assert!(store.contains(GraphKind::Lockfile, hash).unwrap());
        let mapped = store.get(GraphKind::Lockfile, hash).unwrap();
        assert_eq!(&*mapped, &bytes[..]);
    }
}

#[test]
fn get_validated_passes_for_locally_written_blob() {
    let (_dir, store) = open_store();
    let graph = sample();
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&graph).unwrap();
    let hash = GraphHash::of(&bytes);
    store.put(GraphKind::Module, hash, &bytes).unwrap();
    let _validated = store.get_validated(GraphKind::Module, hash).unwrap();
}
