//! Concurrency proof for the L1 content-addressed store — the **write-if-absent
//! / no-shared-cell** invariant that the whole "content-addressing kills the
//! shared mutable cell" claim rests on.
//!
//! `GraphStore` is `Clone` (an `Arc<redb::Database>` + a `StoreLayout`) and is
//! documented as safe to share across threads ("redb is internally
//! concurrent … Construct one per process; pass clones into worker tasks").
//! These tests take that at its word and hammer it with **real OS threads**
//! against a **real on-disk redb + blob tree** (no mock) — the honest proof
//! that the content-addressed `put` is race-safe.
//!
//! The invariants exercised:
//!
//! 1. **Same-key race → exactly one stored value, all readers agree.** N threads
//!    race to `put` the *same* `(kind, hash, bytes)` triple. Because the key IS
//!    `GraphHash::of(bytes)`, every writer writes byte-identical content, so the
//!    "last writer wins" hazard is *degenerate* — there is only one legal value.
//!    The test asserts: exactly one index entry, one blob file, the bytes read
//!    back are the correct content, no torn/partial write, and every one of the
//!    N racing writers succeeded (idempotent re-put is not an error).
//!
//! 2. **Distinct-key race → all land, no cross-contamination.** N threads each
//!    `put` a *different* key concurrently. All N must be present afterward and
//!    each key must resolve to *its own* bytes (no key returns another key's
//!    content).
//!
//! 3. **Concurrent readers during writes see whole values, never a torn blob.**
//!    A reader thread pool races a writer pool; every successful read returns
//!    bytes that hash back to the key it asked for (`get_validated`), so a reader
//!    can never observe a half-written blob.
//!
//! These are deliberately built on the *shipped* `put`/`get`/`contains`/`len`
//! surface — no new production code, no rewrite. They ADD a race-safety proof
//! around logic that already exists.

use std::sync::{Arc, Barrier};
use std::thread;

use sui_graph_store::{GraphHash, GraphKind, GraphStore};
use tempfile::tempdir;

/// A generous racer count. Enough threads that, on a machine with real
/// parallelism, the same-key writers genuinely overlap inside `put`'s
/// `exists()`-check → tmp-write → rename → index-commit window.
const RACERS: usize = 32;

/// Open a fresh store on a real tempdir. Returns the dir guard (kept alive for
/// the test's lifetime) and the store.
fn fresh() -> (tempfile::TempDir, GraphStore) {
    let dir = tempdir().expect("tempdir");
    let store = GraphStore::open(dir.path().to_path_buf()).expect("open store");
    (dir, store)
}

// ───────────────────────────────────────────────────────────────────────────
// 1. Same-key race → exactly one stored value, every reader agrees.
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn n_threads_racing_to_put_the_same_key_yield_exactly_one_value() {
    let (_dir, store) = fresh();
    let payload: Arc<Vec<u8>> = Arc::new(b"the one true content-addressed blob".to_vec());
    let hash = GraphHash::of(&payload);
    let kind = GraphKind::Lockfile;

    // All writers start together so the puts genuinely overlap.
    let barrier = Arc::new(Barrier::new(RACERS));
    let mut handles = Vec::with_capacity(RACERS);
    for _ in 0..RACERS {
        let store = store.clone();
        let payload = Arc::clone(&payload);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            // Every racer writes the SAME key + SAME bytes. `put` is documented
            // idempotent on the same triple; a racer that loses the exists-check
            // race must still return Ok (idempotent re-put), never an error.
            store.put(kind, hash, &payload)
        }));
    }

    // Every single racing writer succeeded — no torn-write / lost-race error.
    let mut ok = 0usize;
    for h in handles {
        h.join().expect("writer thread panicked").expect("put must not error under same-key race");
        ok += 1;
    }
    assert_eq!(ok, RACERS, "every same-key racer must succeed idempotently");

    // Exactly ONE value is stored — the shared-cell hazard is unrepresentable
    // because the key IS the content hash.
    assert_eq!(store.len().unwrap(), 1, "same-key race must collapse to one index entry");
    assert!(store.contains(kind, hash).unwrap());

    // Exactly ONE blob file on disk for this key (no orphan tmp files, no
    // duplicate finals).
    assert_eq!(
        blob_files_for(&store, kind, hash),
        1,
        "exactly one blob file must exist for the raced key"
    );
    assert_eq!(tmp_files_under(&store), 0, "no leftover .tmp files after the race settled");

    // The single stored value is the CORRECT content, byte-for-byte, and passes
    // the content-address self-check — so it is neither torn nor another key's.
    let got = store.get_validated(kind, hash).unwrap();
    assert_eq!(&*got, &*payload, "the one stored value is the correct content");
}

#[test]
fn same_key_race_readers_and_writers_always_see_a_whole_value() {
    // A reader pool races the writer pool. A reader either misses (not yet
    // written) or reads a WHOLE, correct value — never a torn blob. `get`
    // enforces the index-len vs file-len check; `get_validated` additionally
    // re-hashes, so a torn read is impossible to observe as success.
    let (_dir, store) = fresh();
    let payload: Arc<Vec<u8>> = Arc::new(vec![0xABu8; 64 * 1024]); // 64 KiB — big enough that a torn write would be observable
    let hash = GraphHash::of(&payload);
    let kind = GraphKind::Derivation;

    let barrier = Arc::new(Barrier::new(RACERS * 2));
    let mut handles = Vec::with_capacity(RACERS * 2);

    // Writers.
    for _ in 0..RACERS {
        let store = store.clone();
        let payload = Arc::clone(&payload);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || -> Result<(), String> {
            barrier.wait();
            store.put(kind, hash, &payload).map_err(|e| e.to_string())
        }));
    }
    // Readers — spin until the key appears, then validate every read.
    for _ in 0..RACERS {
        let store = store.clone();
        let payload = Arc::clone(&payload);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || -> Result<(), String> {
            barrier.wait();
            // Try a bounded number of reads; each success MUST be whole+correct.
            for _ in 0..1000 {
                if store.contains(kind, hash).map_err(|e| e.to_string())? {
                    // Validated read: rejects torn/partial or wrong bytes.
                    let blob = store.get_validated(kind, hash).map_err(|e| e.to_string())?;
                    if &*blob != &*payload {
                        return Err("reader observed non-canonical bytes".to_string());
                    }
                    return Ok(());
                }
                std::thread::yield_now();
            }
            // Never observed the write — acceptable (the reader lost the race
            // entirely); the point is that no read ever saw a *torn* value.
            Ok(())
        }));
    }

    for h in handles {
        h.join().expect("thread panicked").expect("no torn read / write under same-key race");
    }
    assert_eq!(store.len().unwrap(), 1);
    assert_eq!(&*store.get_validated(kind, hash).unwrap(), &*payload);
}

// ───────────────────────────────────────────────────────────────────────────
// 2. Distinct-key race → all land, no cross-contamination.
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn n_threads_racing_distinct_keys_all_land_without_cross_contamination() {
    let (_dir, store) = fresh();
    let kind = GraphKind::Ast;

    // Each thread owns a unique payload → a unique content hash.
    let payloads: Vec<Vec<u8>> = (0..RACERS)
        .map(|i| format!("distinct-key-payload-#{i}-{}", "x".repeat(i)).into_bytes())
        .collect();
    let expected: Vec<(GraphHash, Vec<u8>)> =
        payloads.iter().map(|p| (GraphHash::of(p), p.clone())).collect();

    let barrier = Arc::new(Barrier::new(RACERS));
    let mut handles = Vec::with_capacity(RACERS);
    for i in 0..RACERS {
        let store = store.clone();
        let payload = payloads[i].clone();
        let hash = expected[i].0;
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            store.put(kind, hash, &payload)
        }));
    }
    for h in handles {
        h.join().expect("thread panicked").expect("distinct-key put must succeed");
    }

    // All N distinct keys present.
    assert_eq!(store.len().unwrap(), RACERS as u64, "every distinct key must land");

    // No cross-contamination: each key resolves to ITS OWN bytes.
    for (hash, want) in &expected {
        assert!(store.contains(kind, *hash).unwrap());
        let got = store.get_validated(kind, *hash).unwrap();
        assert_eq!(&*got, want.as_slice(), "a key returned another key's content");
    }

    // And the index's key set is exactly the set we inserted.
    let mut got_keys: Vec<GraphHash> = store
        .iter_keys()
        .unwrap()
        .into_iter()
        .filter(|(k, _)| *k == kind)
        .map(|(_, h)| h)
        .collect();
    got_keys.sort();
    let mut want_keys: Vec<GraphHash> = expected.iter().map(|(h, _)| *h).collect();
    want_keys.sort();
    assert_eq!(got_keys, want_keys);
}

#[test]
fn interleaved_same_and_distinct_keys_converge_correctly() {
    // A harder mix: several DISTINCT keys, each with SEVERAL racers, all firing
    // at once. Proves the two invariants hold simultaneously — distinct keys
    // don't collide, and duplicate racers on one key collapse to one value.
    let (_dir, store) = fresh();
    let kind = GraphKind::Module;
    let distinct = 8usize;
    let dupes = 6usize;

    let payloads: Vec<Arc<Vec<u8>>> = (0..distinct)
        .map(|i| Arc::new(format!("mixed-payload-{i}").into_bytes()))
        .collect();
    let hashes: Vec<GraphHash> = payloads.iter().map(|p| GraphHash::of(p)).collect();

    let total = distinct * dupes;
    let barrier = Arc::new(Barrier::new(total));
    let mut handles = Vec::with_capacity(total);
    for i in 0..distinct {
        for _ in 0..dupes {
            let store = store.clone();
            let payload = Arc::clone(&payloads[i]);
            let hash = hashes[i];
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                store.put(kind, hash, &payload)
            }));
        }
    }
    for h in handles {
        h.join().expect("thread panicked").expect("mixed put must succeed");
    }

    // Exactly `distinct` entries — the `dupes` racers per key collapsed.
    assert_eq!(store.len().unwrap(), distinct as u64);
    for (i, hash) in hashes.iter().enumerate() {
        assert_eq!(&*store.get_validated(kind, *hash).unwrap(), &**payloads[i]);
    }
}

// ───────────────────────────────────────────────────────────────────────────
// on-disk inspection helpers — count the REAL blob / tmp files so a torn or
// duplicated write would be caught structurally, not just via the index.
// ───────────────────────────────────────────────────────────────────────────

/// Count blob files that back a specific `(kind, hash)` — should always be 0 or 1.
fn blob_files_for(store: &GraphStore, kind: GraphKind, hash: GraphHash) -> usize {
    let path = store.layout().blob_path(kind, hash);
    usize::from(path.exists())
}

/// Count `.tmp` sidecar files anywhere under the blob tree — a nonzero count
/// after a race settles means a rename was lost (torn write).
fn tmp_files_under(store: &GraphStore) -> usize {
    let blobs = store.layout().root().join("blobs");
    let mut count = 0usize;
    let mut stack = vec![blobs];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("tmp") {
                count += 1;
            }
        }
    }
    count
}
