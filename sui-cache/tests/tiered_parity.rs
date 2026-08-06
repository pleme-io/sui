//! Differential parity gate for [`TieredBackend`] — the tiered path is a **pure
//! performance projection** of the shipped on-disk backend and must never change
//! a returned byte (synthesis §5-P0-#1).
//!
//! The oracle is [`LocalStorage`] (the ground-truth on-disk backend). Every tier
//! here is *also* a real `LocalStorage` (on disk, in a tempdir), so this exercises
//! the resolver end-to-end against real StorageBackends with no in-crate mocks —
//! the honest complement to the unit tests' in-memory tiers.
//!
//! Tier-honest: this proves parity against the on-disk backend, NOT against a live
//! Redis/Postgres/S3 cluster (that is `TieredTier::LiveClusterProven`, unshipped —
//! see `TIERED_BACKEND_TIER`).

use std::sync::Arc;

use sui_cache::{LocalStorage, StorageBackend, TieredBackend, WritePolicy};

const NARINFO: &str = "StorePath: /nix/store/abc-hello\nURL: nar/abc.nar.xz\nCompression: xz\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n";
const NAR_PATH: &str = "nar/abc.nar.xz";
const NAR_BYTES: &[u8] = b"\x00\x01\x02\x03 fake compressed nar bytes \xff\xfe";

/// Three on-disk tiers + their inspection handles + a separate oracle.
struct Rig {
    _dirs: Vec<tempfile::TempDir>,
    l1_root: std::path::PathBuf,
    l1: Arc<LocalStorage>,
    l2: Arc<LocalStorage>,
    l3: Arc<LocalStorage>,
    oracle: LocalStorage,
    tiered: TieredBackend,
}

fn rig(policy: WritePolicy) -> Rig {
    let d1 = tempfile::tempdir().unwrap();
    let d2 = tempfile::tempdir().unwrap();
    let d3 = tempfile::tempdir().unwrap();
    let do_ = tempfile::tempdir().unwrap();
    let l1_root = d1.path().to_path_buf();
    let l1 = Arc::new(LocalStorage::new(d1.path()));
    let l2 = Arc::new(LocalStorage::new(d2.path()));
    let l3 = Arc::new(LocalStorage::new(d3.path()));
    let oracle = LocalStorage::new(do_.path());
    let tiered = TieredBackend::with_write_policy(l1.clone(), l2.clone(), l3.clone(), policy);
    Rig {
        _dirs: vec![d1, d2, d3, do_],
        l1_root,
        l1,
        l2,
        l3,
        oracle,
        tiered,
    }
}

/// Invariant 1 — write-through parity: a `put` populates every durable tier with
/// bytes byte-identical to the oracle.
#[tokio::test]
async fn write_through_parity_each_tier_matches_oracle() {
    let r = rig(WritePolicy::WriteThrough);

    r.oracle.put_narinfo("abc", NARINFO).await.unwrap();
    r.oracle.put_nar(NAR_PATH, NAR_BYTES).await.unwrap();
    r.tiered.put_narinfo("abc", NARINFO).await.unwrap();
    r.tiered.put_nar(NAR_PATH, NAR_BYTES).await.unwrap();

    let oracle_ni = r.oracle.get_narinfo("abc").await.unwrap().unwrap();
    let oracle_nar = r.oracle.get_nar(NAR_PATH).await.unwrap().unwrap();

    // Each tier ALONE returns exactly the oracle's bytes.
    for tier in [&r.l1, &r.l2, &r.l3] {
        assert_eq!(tier.get_narinfo("abc").await.unwrap().unwrap(), oracle_ni);
        assert_eq!(tier.get_nar(NAR_PATH).await.unwrap().unwrap(), oracle_nar);
    }
}

/// Invariant 2 — read-through parity: with L1 cold, the tiered read is
/// byte-identical to the oracle (an L1 miss satisfied by a durable tier is
/// transparent).
#[tokio::test]
async fn read_through_parity_with_cold_l1() {
    let r = rig(WritePolicy::WriteThrough);
    r.oracle.put_narinfo("abc", NARINFO).await.unwrap();
    r.tiered.put_narinfo("abc", NARINFO).await.unwrap();

    // Simulate a cold/rolled hot tier by wiping L1's directory.
    std::fs::remove_dir_all(&r.l1_root).unwrap();
    assert!(
        r.l1.get_narinfo("abc").await.unwrap().is_none(),
        "L1 is cold"
    );

    assert_eq!(
        r.tiered.get_narinfo("abc").await.unwrap().unwrap(),
        r.oracle.get_narinfo("abc").await.unwrap().unwrap(),
    );
}

/// Invariant 3 — miss parity: an absent key yields the identical `Ok(None)` from
/// both the oracle and the tiered resolver (no tier fabricates or errors).
#[tokio::test]
async fn miss_parity() {
    let r = rig(WritePolicy::WriteThrough);
    assert_eq!(
        r.tiered.get_narinfo("ghost").await.unwrap(),
        r.oracle.get_narinfo("ghost").await.unwrap(),
    );
    assert_eq!(
        r.tiered.get_nar("nar/ghost.nar.xz").await.unwrap(),
        r.oracle.get_nar("nar/ghost.nar.xz").await.unwrap(),
    );
}

/// Durability parity — the real proof of the never-touch-disk claim for the store
/// layer: drop the entire L1 tier between write and read; a subsequent read still
/// byte-matches the oracle, proving nothing durable lived only in the ephemeral
/// hot tier.
#[tokio::test]
async fn durability_parity_l1_loss_loses_nothing() {
    let r = rig(WritePolicy::WriteThrough);
    r.oracle.put_narinfo("abc", NARINFO).await.unwrap();
    r.oracle.put_nar(NAR_PATH, NAR_BYTES).await.unwrap();
    r.tiered.put_narinfo("abc", NARINFO).await.unwrap();
    r.tiered.put_nar(NAR_PATH, NAR_BYTES).await.unwrap();

    // Pod roll: the entire hot tier vanishes.
    std::fs::remove_dir_all(&r.l1_root).unwrap();

    assert_eq!(
        r.tiered.get_narinfo("abc").await.unwrap().unwrap(),
        r.oracle.get_narinfo("abc").await.unwrap().unwrap(),
    );
    assert_eq!(
        r.tiered.get_nar(NAR_PATH).await.unwrap().unwrap(),
        r.oracle.get_nar(NAR_PATH).await.unwrap().unwrap(),
    );
}

/// Write-around parity — durable tiers still match the oracle, and the read path
/// then lazily warms L1.
#[tokio::test]
async fn write_around_parity_durable_tiers_match_then_read_warms_l1() {
    let r = rig(WritePolicy::WriteAround);
    r.oracle.put_narinfo("abc", NARINFO).await.unwrap();
    r.tiered.put_narinfo("abc", NARINFO).await.unwrap();

    // L1 untouched by the write; L2/L3 match the oracle.
    assert!(r.l1.get_narinfo("abc").await.unwrap().is_none());
    let oracle_ni = r.oracle.get_narinfo("abc").await.unwrap().unwrap();
    assert_eq!(r.l2.get_narinfo("abc").await.unwrap().unwrap(), oracle_ni);
    assert_eq!(r.l3.get_narinfo("abc").await.unwrap().unwrap(), oracle_ni);

    // A tiered read is byte-identical AND promotes into L1.
    assert_eq!(
        r.tiered.get_narinfo("abc").await.unwrap().unwrap(),
        oracle_ni
    );
    assert_eq!(r.l1.get_narinfo("abc").await.unwrap().unwrap(), oracle_ni);
}
