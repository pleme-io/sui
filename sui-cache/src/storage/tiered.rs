//! **Tiered** `StorageBackend` — the `Redis L1 → Postgres L2 → object L3`
//! read-through / write-through cache resolver.
//!
//! [`TieredBackend`] composes three [`StorageBackend`]s into one. It is itself a
//! `StorageBackend`, so it drops straight into `AppState.storage`
//! (`Arc<dyn StorageBackend>`) with **zero server change** — the daemon consumes
//! exactly one backend and does not care that it is three underneath.
//!
//! # Read path (read-through + promotion)
//!
//! `get_*` tries the tiers top-down and **promotes on a lower-tier hit**:
//!
//! ```text
//! L1 (Redis, hot)  ── hit ─▶ return
//!   │ miss
//! L2 (Postgres)    ── hit ─▶ warm L1, return
//!   │ miss
//! L3 (object)      ── hit ─▶ warm L2, warm L1, return
//!   │ miss
//! Ok(None)   (re-derive is the caller's job)
//! ```
//!
//! Promotion is **best-effort**: the read already succeeded, so a failed warm of
//! an upper tier is logged (`tracing::warn!`) and swallowed — it never turns a
//! successful read into an error. Because every key is content-derived, an L1
//! miss satisfied by L2/L3 returns *the same bytes* for the same key
//! (read-through transparency).
//!
//! # Write path (typed [`WritePolicy`])
//!
//! Every policy writes **both durable tiers (L2 and L3) before returning** — so a
//! pod roll that loses the ephemeral L1 loses nothing, and each tier alone can
//! satisfy a later read identically. The policies differ only in how they treat
//! the hot L1 tier:
//!
//! - [`WritePolicy::WriteThrough`] (default) — durable tiers first, then warm L1.
//! - [`WritePolicy::WriteBack`] — warm L1 first (immediate hot availability for a
//!   racing read), then persist the durable tiers **before returning** (still
//!   crash-safe: it does *not* acknowledge before the durable flush). See the
//!   tier note on why fully-async deferred write-back is deliberately unshipped.
//! - [`WritePolicy::WriteAround`] — durable tiers only, skip L1 (avoids polluting
//!   the hot tier with write-once-read-never blobs; L1 fills lazily on read).
//!
//! A durable-tier write failure propagates (`?`); an L1 warm failure is
//! best-effort (logged), for the same reason promotion is.
//!
//! # `delete` / `list_narinfos`
//!
//! `delete` fans out to all three tiers best-effort (content-addressed storage
//! makes delete a GC operation, not a correctness one — a key always resolves to
//! its content or to nothing; mirror [`S3Storage::delete`](super::S3Storage)).
//! `list_narinfos` unions the **authoritative** durable tiers (L2 ∪ L3), deduped;
//! L1 is skipped because it is only a partial hot subset.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::warn;

use super::StorageBackend;
use crate::CacheError;

/// How a `put` propagates across the tiers. See the module docs for the full
/// contract; every policy persists **both durable tiers before returning**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WritePolicy {
    /// Durable tiers (L2, L3) first, then warm L1. Crash-safe. The default.
    #[default]
    WriteThrough,
    /// Warm L1 first (immediate hot availability), then persist durable tiers
    /// before returning. Crash-safe (no ack before the durable flush).
    WriteBack,
    /// Durable tiers only; skip L1 (it fills lazily on read-through).
    WriteAround,
}

/// The honest self-description of what [`TieredBackend`] has been *proven*
/// against — asserted by the honest gate so a claim cannot be silently rounded
/// up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TieredTier {
    /// Resolver semantics proven against in-memory mock tiers + a real on-disk
    /// [`LocalStorage`](super::LocalStorage) L3 (the "real-shape L3"). **No live
    /// Redis/Postgres/S3 exercised.**
    MockParityProven,
    /// Additionally proven end-to-end against a live Redis + Postgres + object
    /// store in a cluster. (Not the shipped tier.)
    LiveClusterProven,
}

/// The shipped tier of [`TieredBackend`]. Asserted by the honest gate; bumping it
/// to [`LiveClusterProven`](TieredTier::LiveClusterProven) without a live
/// integration test is a build-failing round-up.
pub const TIERED_BACKEND_TIER: TieredTier = TieredTier::MockParityProven;

/// Three-tier read-through / write-through cache resolver.
///
/// Holds `Arc<dyn StorageBackend>` per tier, so any backend composes — the real
/// deployment injects `RedisBackend` (L1), `PgStorageBackend` (L2), `S3Storage`
/// (L3); tests inject in-memory mocks + a `LocalStorage`.
pub struct TieredBackend {
    l1: Arc<dyn StorageBackend>,
    l2: Arc<dyn StorageBackend>,
    l3: Arc<dyn StorageBackend>,
    write_policy: WritePolicy,
}

impl TieredBackend {
    /// Compose three tiers with the default [`WritePolicy::WriteThrough`].
    #[must_use]
    pub fn new(
        l1: Arc<dyn StorageBackend>,
        l2: Arc<dyn StorageBackend>,
        l3: Arc<dyn StorageBackend>,
    ) -> Self {
        Self::with_write_policy(l1, l2, l3, WritePolicy::default())
    }

    /// Compose three tiers with an explicit [`WritePolicy`].
    #[must_use]
    pub fn with_write_policy(
        l1: Arc<dyn StorageBackend>,
        l2: Arc<dyn StorageBackend>,
        l3: Arc<dyn StorageBackend>,
        write_policy: WritePolicy,
    ) -> Self {
        Self { l1, l2, l3, write_policy }
    }

    /// The active write policy.
    #[must_use]
    pub fn write_policy(&self) -> WritePolicy {
        self.write_policy
    }

    // ── best-effort warmers (promotion + hot write) ────────────────────────

    async fn warm_narinfo(tier: &Arc<dyn StorageBackend>, hash: &str, content: &str) {
        if let Err(e) = tier.put_narinfo(hash, content).await {
            warn!(hash = %hash, error = %e, "tiered: best-effort narinfo warm failed");
        }
    }

    async fn warm_nar(tier: &Arc<dyn StorageBackend>, path: &str, data: &[u8]) {
        if let Err(e) = tier.put_nar(path, data).await {
            warn!(path = %path, error = %e, "tiered: best-effort NAR warm failed");
        }
    }
}

#[async_trait]
impl StorageBackend for TieredBackend {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, CacheError> {
        // L1
        if let Some(v) = self.l1.get_narinfo(hash).await? {
            return Ok(Some(v));
        }
        // L2 → promote to L1
        if let Some(v) = self.l2.get_narinfo(hash).await? {
            Self::warm_narinfo(&self.l1, hash, &v).await;
            return Ok(Some(v));
        }
        // L3 → promote to L2 then L1
        if let Some(v) = self.l3.get_narinfo(hash).await? {
            Self::warm_narinfo(&self.l2, hash, &v).await;
            Self::warm_narinfo(&self.l1, hash, &v).await;
            return Ok(Some(v));
        }
        Ok(None)
    }

    async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, CacheError> {
        if let Some(v) = self.l1.get_nar(path).await? {
            return Ok(Some(v));
        }
        if let Some(v) = self.l2.get_nar(path).await? {
            Self::warm_nar(&self.l1, path, &v).await;
            return Ok(Some(v));
        }
        if let Some(v) = self.l3.get_nar(path).await? {
            Self::warm_nar(&self.l2, path, &v).await;
            Self::warm_nar(&self.l1, path, &v).await;
            return Ok(Some(v));
        }
        Ok(None)
    }

    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), CacheError> {
        match self.write_policy {
            WritePolicy::WriteThrough => {
                self.l2.put_narinfo(hash, content).await?;
                self.l3.put_narinfo(hash, content).await?;
                Self::warm_narinfo(&self.l1, hash, content).await;
            }
            WritePolicy::WriteBack => {
                Self::warm_narinfo(&self.l1, hash, content).await;
                self.l2.put_narinfo(hash, content).await?;
                self.l3.put_narinfo(hash, content).await?;
            }
            WritePolicy::WriteAround => {
                self.l2.put_narinfo(hash, content).await?;
                self.l3.put_narinfo(hash, content).await?;
            }
        }
        Ok(())
    }

    async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), CacheError> {
        match self.write_policy {
            WritePolicy::WriteThrough => {
                self.l2.put_nar(path, data).await?;
                self.l3.put_nar(path, data).await?;
                Self::warm_nar(&self.l1, path, data).await;
            }
            WritePolicy::WriteBack => {
                Self::warm_nar(&self.l1, path, data).await;
                self.l2.put_nar(path, data).await?;
                self.l3.put_nar(path, data).await?;
            }
            WritePolicy::WriteAround => {
                self.l2.put_nar(path, data).await?;
                self.l3.put_nar(path, data).await?;
            }
        }
        Ok(())
    }

    async fn delete(&self, hash: &str) -> Result<(), CacheError> {
        // Best-effort across all tiers: content-addressed delete is a GC op, not a
        // correctness one (a key resolves to its content or to nothing). Mirror
        // `S3Storage::delete` — warn, never hard-fail on one tier.
        for (name, tier) in [("l1", &self.l1), ("l2", &self.l2), ("l3", &self.l3)] {
            if let Err(e) = tier.delete(hash).await {
                warn!(hash = %hash, tier = name, error = %e, "tiered: best-effort delete failed");
            }
        }
        Ok(())
    }

    async fn list_narinfos(&self) -> Result<Vec<String>, CacheError> {
        // Authoritative tiers only (L2 ∪ L3); L1 is a partial hot subset. A
        // durable-tier list failure surfaces (`?`).
        let mut set = BTreeSet::new();
        set.extend(self.l2.list_narinfos().await?);
        set.extend(self.l3.list_narinfos().await?);
        Ok(set.into_iter().collect())
    }
}

// ---------------------------------------------------------------------------
// Unit tests — the resolver semantics (fallthrough, promotion, write policies,
// delete fan-out, list dedup, durability) proven against in-memory mock tiers
// plus a real-shape on-disk L3. No live Redis/PG/S3.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::LocalStorage;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory [`StorageBackend`] mock. Per-instance maps so a test can assert
    /// *which tier* holds a key (proving promotion), `clear` a tier (simulate a
    /// pod roll), and optionally fail all writes (prove best-effort warm).
    #[derive(Default)]
    struct MemBackend {
        narinfo: Mutex<HashMap<String, String>>,
        nar: Mutex<HashMap<String, Vec<u8>>>,
        writes_fail: Mutex<bool>,
    }

    impl MemBackend {
        fn has_narinfo(&self, hash: &str) -> bool {
            self.narinfo.lock().unwrap().contains_key(hash)
        }
        fn has_nar(&self, path: &str) -> bool {
            self.nar.lock().unwrap().contains_key(path)
        }
        fn narinfo_count(&self) -> usize {
            self.narinfo.lock().unwrap().len()
        }
        fn clear(&self) {
            self.narinfo.lock().unwrap().clear();
            self.nar.lock().unwrap().clear();
        }
        fn set_writes_fail(&self, v: bool) {
            *self.writes_fail.lock().unwrap() = v;
        }
        fn fail_if_configured(&self) -> Result<(), CacheError> {
            if *self.writes_fail.lock().unwrap() {
                Err(CacheError::NotImplemented("mock writes disabled"))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl StorageBackend for MemBackend {
        async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, CacheError> {
            Ok(self.narinfo.lock().unwrap().get(hash).cloned())
        }
        async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), CacheError> {
            self.fail_if_configured()?;
            self.narinfo.lock().unwrap().insert(hash.to_string(), content.to_string());
            Ok(())
        }
        async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, CacheError> {
            Ok(self.nar.lock().unwrap().get(path).cloned())
        }
        async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), CacheError> {
            self.fail_if_configured()?;
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
            Ok(self.narinfo.lock().unwrap().keys().cloned().collect())
        }
    }

    const NARINFO: &str = "StorePath: /nix/store/abc-hello\nURL: nar/abc.nar.xz\nCompression: xz\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n";

    /// Build a tiered backend over three fresh mocks, returning the concrete
    /// handles for inspection alongside the composed resolver.
    fn mocks() -> (Arc<MemBackend>, Arc<MemBackend>, Arc<MemBackend>, TieredBackend) {
        let l1 = Arc::new(MemBackend::default());
        let l2 = Arc::new(MemBackend::default());
        let l3 = Arc::new(MemBackend::default());
        let tiered = TieredBackend::new(l1.clone(), l2.clone(), l3.clone());
        (l1, l2, l3, tiered)
    }

    // ── read fallthrough + promotion ───────────────────────────────────────

    #[tokio::test]
    async fn l1_hit_returns_without_touching_lower_tiers() {
        let (l1, l2, l3, tiered) = mocks();
        l1.put_narinfo("h", "hot").await.unwrap();
        assert_eq!(tiered.get_narinfo("h").await.unwrap().unwrap(), "hot");
        // Lower tiers never populated.
        assert!(!l2.has_narinfo("h"));
        assert!(!l3.has_narinfo("h"));
    }

    #[tokio::test]
    async fn l2_hit_promotes_into_l1() {
        let (l1, l2, _l3, tiered) = mocks();
        l2.put_narinfo("h", NARINFO).await.unwrap();
        assert!(!l1.has_narinfo("h"));
        let got = tiered.get_narinfo("h").await.unwrap().unwrap();
        assert_eq!(got, NARINFO);
        // Promotion warmed L1.
        assert!(l1.has_narinfo("h"), "L2 hit must promote into L1");
    }

    #[tokio::test]
    async fn l3_hit_promotes_into_l2_and_l1() {
        let (l1, l2, l3, tiered) = mocks();
        l3.put_narinfo("h", NARINFO).await.unwrap();
        let got = tiered.get_narinfo("h").await.unwrap().unwrap();
        assert_eq!(got, NARINFO);
        assert!(l2.has_narinfo("h"), "L3 hit must promote into L2");
        assert!(l1.has_narinfo("h"), "L3 hit must promote into L1");
    }

    #[tokio::test]
    async fn nar_l3_hit_promotes_into_l2_and_l1() {
        let (l1, l2, l3, tiered) = mocks();
        l3.put_nar("nar/x.nar.xz", b"blob").await.unwrap();
        let got = tiered.get_nar("nar/x.nar.xz").await.unwrap().unwrap();
        assert_eq!(got, b"blob");
        assert!(l2.has_nar("nar/x.nar.xz"));
        assert!(l1.has_nar("nar/x.nar.xz"));
    }

    #[tokio::test]
    async fn miss_at_all_tiers_is_none() {
        let (_l1, _l2, _l3, tiered) = mocks();
        assert!(tiered.get_narinfo("ghost").await.unwrap().is_none());
        assert!(tiered.get_nar("nar/ghost.nar.xz").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn promotion_failure_does_not_break_a_read() {
        // A best-effort warm that fails must NOT turn a successful read into an
        // error — the bytes were found.
        let (l1, l2, _l3, tiered) = mocks();
        l2.put_narinfo("h", NARINFO).await.unwrap();
        l1.set_writes_fail(true); // L1 warm will error
        let got = tiered.get_narinfo("h").await.unwrap();
        assert_eq!(got.unwrap(), NARINFO);
        assert!(!l1.has_narinfo("h"), "warm failed, so L1 stays empty — but the read still succeeded");
    }

    // ── write policies ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn write_through_populates_all_tiers() {
        let (l1, l2, l3, tiered) = mocks();
        tiered.put_narinfo("h", NARINFO).await.unwrap();
        assert!(l1.has_narinfo("h"), "write-through warms L1");
        assert!(l2.has_narinfo("h"), "write-through persists L2");
        assert!(l3.has_narinfo("h"), "write-through persists L3");
    }

    #[tokio::test]
    async fn write_around_skips_l1_but_persists_durable() {
        let l1 = Arc::new(MemBackend::default());
        let l2 = Arc::new(MemBackend::default());
        let l3 = Arc::new(MemBackend::default());
        let tiered = TieredBackend::with_write_policy(
            l1.clone(), l2.clone(), l3.clone(), WritePolicy::WriteAround,
        );
        tiered.put_narinfo("h", NARINFO).await.unwrap();
        assert!(!l1.has_narinfo("h"), "write-around must NOT touch L1");
        assert!(l2.has_narinfo("h"));
        assert!(l3.has_narinfo("h"));
        // …and a subsequent read lazily fills L1 (read-through).
        let _ = tiered.get_narinfo("h").await.unwrap();
        assert!(l1.has_narinfo("h"), "read-through fills L1 after a write-around");
    }

    #[tokio::test]
    async fn write_back_populates_all_tiers_and_is_durable() {
        let l1 = Arc::new(MemBackend::default());
        let l2 = Arc::new(MemBackend::default());
        let l3 = Arc::new(MemBackend::default());
        let tiered = TieredBackend::with_write_policy(
            l1.clone(), l2.clone(), l3.clone(), WritePolicy::WriteBack,
        );
        assert_eq!(tiered.write_policy(), WritePolicy::WriteBack);
        tiered.put_nar("nar/x.nar.xz", b"blob").await.unwrap();
        // Even write-back persists durable tiers before returning.
        assert!(l1.has_nar("nar/x.nar.xz"));
        assert!(l2.has_nar("nar/x.nar.xz"));
        assert!(l3.has_nar("nar/x.nar.xz"));
    }

    #[tokio::test]
    async fn durable_write_failure_propagates() {
        // If a durable tier rejects the write, the put must fail (not silently
        // succeed L1-only).
        let l1 = Arc::new(MemBackend::default());
        let l2 = Arc::new(MemBackend::default());
        let l3 = Arc::new(MemBackend::default());
        l2.set_writes_fail(true);
        let tiered = TieredBackend::new(l1.clone(), l2, l3);
        let err = tiered.put_narinfo("h", NARINFO).await.unwrap_err();
        assert!(matches!(err, CacheError::NotImplemented(_)));
    }

    // ── durability (the never-touch-disk claim's real proof) ───────────────

    #[tokio::test]
    async fn pod_roll_losing_l1_loses_nothing() {
        let (l1, _l2, _l3, tiered) = mocks();
        tiered.put_narinfo("h", NARINFO).await.unwrap();
        tiered.put_nar("nar/h.nar.xz", b"blob").await.unwrap();
        // Simulate a pod roll wiping the entire hot tier.
        l1.clear();
        // A read still returns byte-identical content, satisfied by a durable tier.
        assert_eq!(tiered.get_narinfo("h").await.unwrap().unwrap(), NARINFO);
        assert_eq!(tiered.get_nar("nar/h.nar.xz").await.unwrap().unwrap(), b"blob");
    }

    // ── delete + list ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_fans_out_to_all_tiers() {
        let (l1, l2, l3, tiered) = mocks();
        tiered.put_narinfo("h", NARINFO).await.unwrap();
        tiered.put_nar("nar/h.nar.xz", b"blob").await.unwrap();
        tiered.delete("h").await.unwrap();
        for t in [&l1, &l2, &l3] {
            assert!(!t.has_narinfo("h"));
            assert!(!t.has_nar("nar/h.nar.xz"));
        }
    }

    #[tokio::test]
    async fn list_narinfos_unions_durable_tiers_deduped() {
        let (l1, l2, l3, tiered) = mocks();
        // A key present in BOTH durable tiers must appear once.
        l2.put_narinfo("shared", "x").await.unwrap();
        l3.put_narinfo("shared", "x").await.unwrap();
        l2.put_narinfo("only2", "y").await.unwrap();
        l3.put_narinfo("only3", "z").await.unwrap();
        // An L1-only key must NOT appear (L1 is a partial hot subset).
        l1.put_narinfo("hot-only", "w").await.unwrap();
        let listed = tiered.list_narinfos().await.unwrap();
        assert_eq!(listed, vec!["only2".to_string(), "only3".to_string(), "shared".to_string()]);
    }

    // ── real-shape L3 (LocalStorage on disk), not a mock ───────────────────

    #[tokio::test]
    async fn read_through_from_a_real_local_storage_l3() {
        let dir = tempfile::tempdir().unwrap();
        let l1 = Arc::new(MemBackend::default());
        let l2 = Arc::new(MemBackend::default());
        let l3_disk = Arc::new(LocalStorage::new(dir.path()));
        // Seed the real on-disk L3 directly.
        l3_disk.put_narinfo("h", NARINFO).await.unwrap();
        l3_disk.put_nar("nar/h.nar.xz", b"disk-blob").await.unwrap();

        let tiered = TieredBackend::new(l1.clone(), l2.clone(), l3_disk);
        // Cold L1/L2 → served from the real disk L3, and promoted up.
        assert_eq!(tiered.get_narinfo("h").await.unwrap().unwrap(), NARINFO);
        assert_eq!(tiered.get_nar("nar/h.nar.xz").await.unwrap().unwrap(), b"disk-blob");
        assert!(l1.has_narinfo("h"));
        assert!(l2.has_narinfo("h"));
    }

    // ── the honest gate ────────────────────────────────────────────────────

    #[test]
    fn honest_gate_tier_is_mock_parity_proven_not_live_cluster() {
        // The shipped tier is MockParityProven (in-mem tiers + real-shape disk
        // L3). Bumping TIERED_BACKEND_TIER to LiveClusterProven without an actual
        // live Redis/PG/S3 integration test fails HERE — the claim is not rounded
        // up.
        assert_eq!(TIERED_BACKEND_TIER, TieredTier::MockParityProven);
    }
}
