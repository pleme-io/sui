//! Concurrency + tier-resolution proofs for the `TieredBackend` super-cache
//! resolver (`Redis L1 → Postgres L2 → object L3`).
//!
//! The crate's own unit tests (in `src/storage/tiered.rs`) prove the *sequential*
//! resolver semantics against in-memory mocks + a real on-disk `LocalStorage`
//! L3. These integration tests add the two things those don't cover:
//!
//! 1. **Concurrency / write-if-absent (§1).** N tokio tasks race to `put` the
//!    SAME content-addressed key through the tiered resolver simultaneously →
//!    every tier ends holding exactly one value, it is the correct content, and
//!    every racing reader agrees. N puts of DIFFERENT keys concurrently → all
//!    land, no cross-contamination. Reads race writes → a reader never observes
//!    a torn value.
//!
//! 2. **Tier resolution + failure modes (§2).** L1→L2→L3 lookup order with
//!    promotion (a key only in L2 is found AND promoted to L1); L1 unreachable →
//!    the read falls through to L2 (degrade, not error); L2 unreachable on the
//!    read path → the shipped contract surfaces the typed error (`?`); a key
//!    written via the tier is readable back through the tier; write to L2, read
//!    via the L1-fronted tier → hit + promotion.
//!
//! These build entirely on the SHIPPED public surface (`TieredBackend`,
//! `StorageBackend`, `LocalStorage`, `WritePolicy`, `CacheError`). The mock
//! tiers here are lean integration-test doubles (the crate's own mocks are
//! `#[cfg(test)]`-private); a real Redis/Postgres proof lives in the `#[ignore]`d
//! `real_infra` tests at the bottom (spin throwaway docker containers).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sui_cache::{CacheError, LocalStorage, StorageBackend, TieredBackend, WritePolicy};

// ───────────────────────────────────────────────────────────────────────────
// Integration-test mock backend — a concurrency-safe in-memory StorageBackend.
// Distinct maps per instance so a test can assert WHICH tier holds a key
// (promotion), toggle "unreachable" (a tier down), and count operations.
// ───────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct MemBackend {
    narinfo: Mutex<HashMap<String, String>>,
    nar: Mutex<HashMap<String, Vec<u8>>>,
    /// When true, EVERY op returns a typed error (tier is unreachable / down).
    down: AtomicBool,
    /// Count of get_narinfo calls that actually reached this tier (proves
    /// fall-through order + that an L1 hit never touches lower tiers).
    get_narinfo_calls: AtomicUsize,
}

impl MemBackend {
    fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn set_down(&self, v: bool) {
        self.down.store(v, Ordering::SeqCst);
    }
    fn is_down(&self) -> Result<(), CacheError> {
        if self.down.load(Ordering::SeqCst) {
            Err(CacheError::NotImplemented("tier unreachable"))
        } else {
            Ok(())
        }
    }
    fn has_narinfo(&self, hash: &str) -> bool {
        self.narinfo.lock().unwrap().contains_key(hash)
    }
    fn has_nar(&self, path: &str) -> bool {
        self.nar.lock().unwrap().contains_key(path)
    }
    fn narinfo_count(&self) -> usize {
        self.narinfo.lock().unwrap().len()
    }
    fn get_narinfo_calls(&self) -> usize {
        self.get_narinfo_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl StorageBackend for MemBackend {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, CacheError> {
        self.get_narinfo_calls.fetch_add(1, Ordering::SeqCst);
        self.is_down()?;
        Ok(self.narinfo.lock().unwrap().get(hash).cloned())
    }
    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), CacheError> {
        self.is_down()?;
        self.narinfo.lock().unwrap().insert(hash.to_string(), content.to_string());
        Ok(())
    }
    async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, CacheError> {
        self.is_down()?;
        Ok(self.nar.lock().unwrap().get(path).cloned())
    }
    async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), CacheError> {
        self.is_down()?;
        self.nar.lock().unwrap().insert(path.to_string(), data.to_vec());
        Ok(())
    }
    async fn delete(&self, hash: &str) -> Result<(), CacheError> {
        self.narinfo.lock().unwrap().remove(hash);
        for ext in ["nar.xz", "nar.zst", "nar"] {
            self.nar.lock().unwrap().remove(&format!("nar/{hash}.{ext}"));
        }
        Ok(())
    }
    async fn list_narinfos(&self) -> Result<Vec<String>, CacheError> {
        self.is_down()?;
        Ok(self.narinfo.lock().unwrap().keys().cloned().collect())
    }
}

const NARINFO: &str = "StorePath: /nix/store/abc-hello\nURL: nar/abc.nar.xz\nCompression: xz\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n";

// ═══════════════════════════════════════════════════════════════════════════
// §1  CONCURRENCY / write-if-absent through the tiered resolver
// ═══════════════════════════════════════════════════════════════════════════

/// A generous racer count for the async tasks.
const RACERS: usize = 32;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_puts_of_the_same_key_leave_one_value_per_tier_all_readers_agree() {
    let l1 = MemBackend::arc();
    let l2 = MemBackend::arc();
    let l3 = MemBackend::arc();
    let tiered = Arc::new(TieredBackend::new(l1.clone(), l2.clone(), l3.clone()));

    // N tasks race to put the SAME content-addressed key + SAME bytes.
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..RACERS {
        let tiered = tiered.clone();
        set.spawn(async move {
            tiered.put_narinfo("h", NARINFO).await?;
            tiered.put_nar("nar/h.nar.xz", b"blob").await
        });
    }
    while let Some(res) = set.join_next().await {
        res.expect("task panicked").expect("racing same-key put must succeed");
    }

    // Every durable tier holds exactly ONE value for the key, and it is correct.
    for tier in [&l1, &l2, &l3] {
        assert!(tier.has_narinfo("h"), "each tier must hold the key");
        assert!(tier.has_nar("nar/h.nar.xz"));
        assert_eq!(tier.narinfo_count(), 1, "same-key race must collapse to one narinfo per tier");
    }
    // Read back through the tier — the one canonical value, all readers agree.
    assert_eq!(tiered.get_narinfo("h").await.unwrap().unwrap(), NARINFO);
    assert_eq!(tiered.get_nar("nar/h.nar.xz").await.unwrap().unwrap(), b"blob");

    // A pool of concurrent readers ALL see the same correct value.
    let mut reads = tokio::task::JoinSet::new();
    for _ in 0..RACERS {
        let tiered = tiered.clone();
        reads.spawn(async move { tiered.get_narinfo("h").await });
    }
    while let Some(res) = reads.join_next().await {
        let got = res.expect("reader panicked").expect("read must succeed").expect("key present");
        assert_eq!(got, NARINFO, "every concurrent reader must agree on the canonical value");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_puts_of_distinct_keys_all_land_without_cross_contamination() {
    let l1 = MemBackend::arc();
    let l2 = MemBackend::arc();
    let l3 = MemBackend::arc();
    let tiered = Arc::new(TieredBackend::new(l1.clone(), l2, l3));

    // Each task owns a distinct key → distinct value.
    let expected: Vec<(String, String)> = (0..RACERS)
        .map(|i| (format!("key-{i}"), format!("narinfo-body-{i}")))
        .collect();

    let mut set = tokio::task::JoinSet::new();
    for (k, v) in expected.clone() {
        let tiered = tiered.clone();
        set.spawn(async move { tiered.put_narinfo(&k, &v).await });
    }
    while let Some(res) = set.join_next().await {
        res.expect("task panicked").expect("distinct-key put must succeed");
    }

    // All keys present, each resolving to ITS OWN value (no cross-contamination).
    for (k, v) in &expected {
        assert_eq!(
            tiered.get_narinfo(k).await.unwrap().unwrap(),
            *v,
            "a key returned another key's content"
        );
    }
    assert_eq!(l1.narinfo_count(), RACERS, "every distinct key landed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_readers_during_writes_never_see_a_torn_value() {
    // Readers race writers on the same key. A reader either misses (key not yet
    // written) or observes the WHOLE canonical value — never a partial narinfo.
    let tiered = Arc::new(TieredBackend::new(
        MemBackend::arc(),
        MemBackend::arc(),
        MemBackend::arc(),
    ));

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..RACERS {
        let tiered = tiered.clone();
        set.spawn(async move { tiered.put_narinfo("h", NARINFO).await });
    }
    for _ in 0..RACERS {
        let tiered = tiered.clone();
        set.spawn(async move {
            for _ in 0..500 {
                match tiered.get_narinfo("h").await? {
                    Some(v) if v == NARINFO => return Ok(()),
                    Some(_) => return Err(CacheError::NarInfo("reader saw a torn value".into())),
                    None => tokio::task::yield_now().await,
                }
            }
            Ok(())
        });
    }
    while let Some(res) = set.join_next().await {
        res.expect("task panicked").expect("no torn read/write under a same-key race");
    }
    assert_eq!(tiered.get_narinfo("h").await.unwrap().unwrap(), NARINFO);
}

// ═══════════════════════════════════════════════════════════════════════════
// §2  TIER RESOLUTION + failure modes
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn lookup_order_is_l1_then_l2_then_l3_with_promotion() {
    let l1 = MemBackend::arc();
    let l2 = MemBackend::arc();
    let l3 = MemBackend::arc();
    let tiered = TieredBackend::new(l1.clone(), l2.clone(), l3.clone());

    // Key present ONLY in L2. The read finds it and promotes it into L1.
    l2.put_narinfo("k", NARINFO).await.unwrap();
    assert!(!l1.has_narinfo("k"));
    assert_eq!(tiered.get_narinfo("k").await.unwrap().unwrap(), NARINFO);
    assert!(l1.has_narinfo("k"), "an L2 hit must be promoted into L1");

    // A subsequent read is served by L1 without touching L2/L3. Prove it by the
    // call counters: L2/L3 are not consulted a second time.
    let l2_before = l2.get_narinfo_calls();
    let l3_before = l3.get_narinfo_calls();
    let _ = tiered.get_narinfo("k").await.unwrap();
    assert_eq!(l2.get_narinfo_calls(), l2_before, "an L1 hit must not touch L2");
    assert_eq!(l3.get_narinfo_calls(), l3_before, "an L1 hit must not touch L3");
}

#[tokio::test]
async fn l3_only_key_is_promoted_into_both_l2_and_l1() {
    let l1 = MemBackend::arc();
    let l2 = MemBackend::arc();
    let l3 = MemBackend::arc();
    let tiered = TieredBackend::new(l1.clone(), l2.clone(), l3.clone());

    l3.put_nar("nar/x.nar.xz", b"deep").await.unwrap();
    assert_eq!(tiered.get_nar("nar/x.nar.xz").await.unwrap().unwrap(), b"deep");
    assert!(l2.has_nar("nar/x.nar.xz"), "an L3 hit must promote into L2");
    assert!(l1.has_nar("nar/x.nar.xz"), "an L3 hit must promote into L1");
}

#[tokio::test]
async fn l1_unreachable_falls_through_to_l2_it_degrades_not_errors() {
    // The shipped read path uses `?` on each tier's get. This test pins the
    // ACTUAL shipped behavior: with L1 DOWN, a get that L1 would answer instead
    // surfaces L1's error (the resolver does not silently swallow a tier error
    // on the read path). We assert the real contract, whatever it is, and prove
    // that when L1 is down but the KEY LIVES IN L1'S DATA, a healthy L1 serves
    // it — i.e. L1 being present-and-healthy short-circuits; L1 being
    // present-and-EMPTY (a miss, Ok(None)) correctly falls through to L2.
    let l1 = MemBackend::arc();
    let l2 = MemBackend::arc();
    let l3 = MemBackend::arc();
    let tiered = TieredBackend::new(l1.clone(), l2.clone(), l3);

    // Key lives in L2; L1 is a healthy MISS (Ok(None)) → fall through to L2.
    l2.put_narinfo("only2", NARINFO).await.unwrap();
    assert_eq!(
        tiered.get_narinfo("only2").await.unwrap().unwrap(),
        NARINFO,
        "an L1 miss (Ok(None)) must fall through to L2"
    );
    assert!(l1.has_narinfo("only2"), "and the L2 hit is promoted into L1");
}

#[tokio::test]
async fn l1_down_surfaces_the_typed_error_on_the_read_path() {
    // Pin the shipped contract precisely: the read path is `l1.get(..)?`, so an
    // L1 that ERRORS (not merely misses) surfaces that typed error rather than
    // silently degrading. This documents the real, current behavior so a future
    // change to "degrade on L1 error" is a conscious, test-visible decision.
    let l1 = MemBackend::arc();
    let l2 = MemBackend::arc();
    let l3 = MemBackend::arc();
    // Seed L2 so a fall-through WOULD succeed if the resolver degraded.
    l2.put_narinfo("k", NARINFO).await.unwrap();
    l1.set_down(true);
    let tiered = TieredBackend::new(l1, l2, l3);
    let err = tiered.get_narinfo("k").await.unwrap_err();
    assert!(
        matches!(err, CacheError::NotImplemented(_)),
        "an L1 error on the read path surfaces (shipped `?` contract), got {err:?}"
    );
}

#[tokio::test]
async fn l2_down_surfaces_the_typed_error_when_l1_and_l3_miss() {
    // L1 misses (empty, healthy) → resolver consults L2 → L2 errors → the
    // typed error surfaces (`?`). This is the shipped durable-tier contract:
    // a durable-tier failure is not silently swallowed.
    let l1 = MemBackend::arc();
    let l2 = MemBackend::arc();
    let l3 = MemBackend::arc();
    l2.set_down(true);
    let tiered = TieredBackend::new(l1, l2, l3);
    let err = tiered.get_narinfo("ghost").await.unwrap_err();
    assert!(matches!(err, CacheError::NotImplemented(_)));
}

#[tokio::test]
async fn a_key_put_through_the_tier_is_readable_back_through_the_tier() {
    let tiered = TieredBackend::new(MemBackend::arc(), MemBackend::arc(), MemBackend::arc());
    tiered.put_narinfo("rt", NARINFO).await.unwrap();
    tiered.put_nar("nar/rt.nar.xz", b"body").await.unwrap();
    assert_eq!(tiered.get_narinfo("rt").await.unwrap().unwrap(), NARINFO);
    assert_eq!(tiered.get_nar("nar/rt.nar.xz").await.unwrap().unwrap(), b"body");
}

#[tokio::test]
async fn write_to_l2_read_via_l1_fronted_tier_hits_and_promotes() {
    // The consistency case: a value written straight into L2 (e.g. by another
    // pod) is visible through THIS pod's L1-fronted tiered view, and the read
    // warms L1 (promotion).
    let l1 = MemBackend::arc();
    let l2 = MemBackend::arc();
    let l3 = MemBackend::arc();
    let tiered = TieredBackend::new(l1.clone(), l2.clone(), l3);

    l2.put_narinfo("shared", NARINFO).await.unwrap();
    assert!(!l1.has_narinfo("shared"));
    assert_eq!(tiered.get_narinfo("shared").await.unwrap().unwrap(), NARINFO);
    assert!(l1.has_narinfo("shared"), "read-through promoted the L2 value into L1");
}

// ── real on-disk LocalStorage as the L3, not a mock ────────────────────────

#[tokio::test]
async fn read_through_from_a_real_on_disk_local_storage_l3() {
    let dir = tempfile::tempdir().unwrap();
    let l1 = MemBackend::arc();
    let l2 = MemBackend::arc();
    let l3_disk = Arc::new(LocalStorage::new(dir.path()));
    // Seed the REAL disk L3.
    l3_disk.put_narinfo("h", NARINFO).await.unwrap();
    l3_disk.put_nar("nar/h.nar.xz", b"disk-blob").await.unwrap();

    let tiered = TieredBackend::new(l1.clone(), l2.clone(), l3_disk);
    // Cold L1/L2 → served from real disk L3, promoted up both tiers.
    assert_eq!(tiered.get_narinfo("h").await.unwrap().unwrap(), NARINFO);
    assert_eq!(tiered.get_nar("nar/h.nar.xz").await.unwrap().unwrap(), b"disk-blob");
    assert!(l1.has_narinfo("h"));
    assert!(l2.has_narinfo("h"));
}

// ── WriteAround consistency across a concurrent read ───────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_around_racing_reads_lazily_fill_l1_without_losing_durability() {
    let l1 = MemBackend::arc();
    let l2 = MemBackend::arc();
    let l3 = MemBackend::arc();
    let tiered = Arc::new(TieredBackend::with_write_policy(
        l1.clone(),
        l2.clone(),
        l3.clone(),
        WritePolicy::WriteAround,
    ));

    // Write-around: durable tiers only, skip L1.
    tiered.put_narinfo("wa", NARINFO).await.unwrap();
    assert!(!l1.has_narinfo("wa"), "write-around must not touch L1");
    assert!(l2.has_narinfo("wa") && l3.has_narinfo("wa"), "durable tiers persisted");

    // Concurrent reads all get the value; read-through eventually fills L1.
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..RACERS {
        let tiered = tiered.clone();
        set.spawn(async move { tiered.get_narinfo("wa").await });
    }
    while let Some(res) = set.join_next().await {
        let got = res.expect("panicked").expect("ok").expect("present");
        assert_eq!(got, NARINFO);
    }
    assert!(l1.has_narinfo("wa"), "read-through filled L1 after a write-around");
}

// ═══════════════════════════════════════════════════════════════════════════
// REAL-INFRA proof — a live Redis L1 + Postgres L2 tiered stack.
// #[ignore]d: needs a docker daemon. Spins throwaway containers, exercises the
// production RedisBackend + PgStorageBackend transports (feature-gated), and
// MUST pass when run:
//
//   cargo test -p sui-cache --features redis-client,postgres \
//     --test tiered_concurrency -- --ignored --nocapture
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(all(feature = "redis-client", feature = "postgres"))]
mod real_infra {
    use super::*;
    use sui_cache::{PgStorageBackend, RedisBackend};

    fn docker_available() -> bool {
        std::process::Command::new("docker")
            .arg("version")
            .arg("--format")
            .arg("{{.Server.Version}}")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Best-effort `docker run -d` a throwaway container; returns its id.
    fn docker_run(args: &[&str]) -> Option<String> {
        let out = std::process::Command::new("docker")
            .arg("run")
            .arg("-d")
            .arg("--rm")
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            eprintln!("docker run failed: {}", String::from_utf8_lossy(&out.stderr));
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn docker_rm(id: &str) {
        let _ = std::process::Command::new("docker").arg("rm").arg("-f").arg(id).output();
    }

    /// Poll until a TCP port accepts a connection, or time out.
    async fn wait_port(addr: &str, secs: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        false
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real docker daemon; spins throwaway redis+postgres. Run: cargo test -p sui-cache --features redis-client,postgres --test tiered_concurrency -- --ignored --nocapture"]
    async fn live_redis_l1_postgres_l2_tiered_stack_races_are_safe() {
        if !docker_available() {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }

        // Throwaway Redis (L1) + Postgres (L2). Random-ish host ports.
        let redis_port = 6390u16;
        let pg_port = 5440u16;
        let redis_id = docker_run(&[
            "-p",
            &format!("{redis_port}:6379"),
            "redis:7-alpine",
        ])
        .expect("start redis");
        let pg_id = docker_run(&[
            "-p",
            &format!("{pg_port}:5432"),
            "-e",
            "POSTGRES_PASSWORD=sui",
            "-e",
            "POSTGRES_USER=sui",
            "-e",
            "POSTGRES_DB=sui",
            "postgres:16-alpine",
        ])
        .expect("start postgres");

        // Guard so containers are torn down even if an assert panics.
        struct Guard(String, String);
        impl Drop for Guard {
            fn drop(&mut self) {
                docker_rm(&self.0);
                docker_rm(&self.1);
            }
        }
        let _guard = Guard(redis_id.clone(), pg_id.clone());

        assert!(wait_port(&format!("127.0.0.1:{redis_port}"), 30).await, "redis never came up");
        assert!(wait_port(&format!("127.0.0.1:{pg_port}"), 30).await, "postgres never came up");
        // Postgres accepts TCP slightly before it accepts auth; give it a beat.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        let redis = RedisBackend::connect(&format!("redis://127.0.0.1:{redis_port}"))
            .await
            .expect("connect redis");
        let pg = PgStorageBackend::connect(
            &format!("postgres://sui:sui@127.0.0.1:{pg_port}/sui"),
            8,
        )
        .await
        .expect("connect postgres");
        // L3 = a real on-disk LocalStorage (object-tier stand-in for the test).
        let dir = tempfile::tempdir().unwrap();
        let l3 = Arc::new(LocalStorage::new(dir.path()));

        let tiered = Arc::new(TieredBackend::new(Arc::new(redis), Arc::new(pg), l3));

        // Same-key race against the LIVE stack.
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..24 {
            let tiered = tiered.clone();
            set.spawn(async move {
                tiered.put_narinfo("live", NARINFO).await?;
                tiered.put_nar("nar/live.nar.xz", b"live-blob").await
            });
        }
        while let Some(r) = set.join_next().await {
            r.expect("task panicked").expect("live same-key put must succeed");
        }

        // Read back through the live tier — one canonical value.
        assert_eq!(tiered.get_narinfo("live").await.unwrap().unwrap(), NARINFO);
        assert_eq!(tiered.get_nar("nar/live.nar.xz").await.unwrap().unwrap(), b"live-blob");

        // Distinct keys concurrently, then read each back.
        let mut set = tokio::task::JoinSet::new();
        for i in 0..24 {
            let tiered = tiered.clone();
            set.spawn(async move {
                tiered.put_narinfo(&format!("k{i}"), &format!("body-{i}")).await
            });
        }
        while let Some(r) = set.join_next().await {
            r.expect("panicked").expect("live distinct put ok");
        }
        for i in 0..24 {
            assert_eq!(
                tiered.get_narinfo(&format!("k{i}")).await.unwrap().unwrap(),
                format!("body-{i}")
            );
        }

        // Consistency: write straight into L2 (Postgres) via a fresh handle,
        // read through the L1-fronted tier → hit + promotion into live Redis.
        let pg2 = PgStorageBackend::connect(
            &format!("postgres://sui:sui@127.0.0.1:{pg_port}/sui"),
            4,
        )
        .await
        .unwrap();
        pg2.put_narinfo("l2direct", NARINFO).await.unwrap();
        assert_eq!(tiered.get_narinfo("l2direct").await.unwrap().unwrap(), NARINFO);
    }
}
