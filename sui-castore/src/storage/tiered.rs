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
//!   │ miss OR ERROR
//! L2 (Postgres)    ── hit ─▶ warm L1, return
//!   │ miss OR ERROR
//! L3 (object)      ── hit ─▶ warm L2, warm L1, return
//!   │ miss OR ERROR
//! Ok(None) if every tier cleanly missed / Err if any tier was broken
//! ```
//!
//! **A broken tier is stepped over, not fatal.** A tier that errors is logged at
//! `ERROR` and the resolver continues down — so Postgres being unreachable can
//! never stop an object-store hit from being served. An error only surfaces when
//! *no* tier produced the content AND at least one tier failed, because then the
//! key genuinely cannot be ruled out (the broken tier might have held it). That
//! is deliberately conservative: the caller is told "I could not answer", not
//! "it is absent". Callers for whom a read is optional — the cache HTTP server —
//! turn that into a miss; callers for whom it is not (GC, which deletes) keep
//! treating it as an error.
//!
//! Promotion is **best-effort**: the read already succeeded, so a failed warm of
//! an upper tier is logged (`tracing::warn!`) and swallowed — it never turns a
//! successful read into an error. Because every key is content-derived, an L1
//! miss satisfied by L2/L3 returns *the same bytes* for the same key
//! (read-through transparency).
//!
//! # Write path (typed [`WritePolicy`])
//!
//! Every policy **attempts both durable tiers (L2 and L3) before returning** and
//! succeeds if **at least one** accepted the write — so a pod roll that loses the
//! ephemeral L1 loses nothing, and a later read (which falls through tiers) is
//! satisfied by whichever durable tier holds it. The policies differ only in how
//! they treat the hot L1 tier:
//!
//! - [`WritePolicy::WriteThrough`] (default) — durable tiers first, then warm L1.
//! - [`WritePolicy::WriteBack`] — warm L1 first (immediate hot availability for a
//!   racing read), then persist the durable tiers **before returning** (still
//!   crash-safe: it does *not* acknowledge before the durable flush). See the
//!   tier note on why fully-async deferred write-back is deliberately unshipped.
//! - [`WritePolicy::WriteAround`] — durable tiers only, skip L1 (avoids polluting
//!   the hot tier with write-once-read-never blobs; L1 fills lazily on read).
//!
//! A write fails only when **every** durable tier rejected it (nothing was
//! stored anywhere). One durable tier failing is logged at `WARN` as lost
//! redundancy, not an error — one broken durable tier must not zero out a
//! healthy one. An L1 warm failure is best-effort (logged), for the same reason
//! promotion is.
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

use super::nar_stream::{self, NarSource, NarStream};
use super::{NarResidency, StorageBackend};
use crate::StoreError;

/// A [`NarSource`] that re-reads one tier.
///
/// This is what makes a streamed promotion possible without a buffer: warming an
/// upper tier is `upper.put_nar_stream(path, &TierNarSource::new(lower, path))`,
/// and each `open()` is a fresh bounded read of the tier that already has the
/// bytes.
///
/// # The cost, stated plainly
///
/// Each warm is an **extra full pass over the lower tier**. An L2 hit reads L2
/// twice (once to warm L1, once to serve); an L3 hit reads L3 three times (warm
/// L2, warm L1, serve). The old code got those passes "free" because it was
/// already holding the whole NAR — which is precisely the thing that killed the
/// pod. Read amplification on the *promotion* path is the honest price of a
/// bounded peak, and promotion is the cold path by construction: it happens once
/// per key, after which L1 answers.
///
/// Two alternatives were considered and rejected. Sourcing the L1 warm from the
/// freshly-warmed L2 (2 passes instead of 3) makes the L1 warm silently depend
/// on L2's health, and a broken L2 must not stop L1 from being warmed from a
/// healthy L3 — the resolver's whole degrade-don't-fail posture. Backgrounding
/// the warm removes the pass from the request entirely but breaks the contract
/// that promotion has happened by the time `get` returns, which the resolver's
/// tests assert and callers rely on.
struct TierNarSource {
    tier: Arc<dyn StorageBackend>,
    path: String,
}

impl TierNarSource {
    fn new(tier: &Arc<dyn StorageBackend>, path: &str) -> Self {
        Self { tier: Arc::clone(tier), path: path.to_string() }
    }
}

#[async_trait]
impl NarSource for TierNarSource {
    async fn open(&self) -> Result<NarStream, StoreError> {
        self.tier.get_nar_stream(&self.path).await?.ok_or_else(|| {
            // The content was there when the read resolved and is gone now —
            // an eviction or a concurrent delete racing the promotion. The warm
            // is best-effort, so the caller logs and moves on.
            StoreError::PathNotFound(format!(
                "{}: vanished from the source tier mid-promotion",
                self.path
            ))
        })
    }
}

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

    /// Best-effort warm of `tier` from a re-openable source. **O(chunk).**
    ///
    /// The result is logged and discarded — including the
    /// [`TooLarge`](StoreError::TooLarge) a capped hot tier returns for an
    /// oversized NAR. That is the contract, not laxity: a refused L1 warm must
    /// never fail a build.
    async fn warm_nar_from(tier: &Arc<dyn StorageBackend>, path: &str, src: &dyn NarSource) {
        if let Err(e) = tier.put_nar_stream(path, src).await {
            warn!(path = %path, error = %e, "tiered: best-effort NAR warm failed");
        }
    }

    /// Best-effort warm of `tier` by re-reading `from`. Used on the read path,
    /// where the bytes live in a lower tier rather than in a caller's buffer.
    async fn warm_nar_from_tier(
        tier: &Arc<dyn StorageBackend>,
        from: &Arc<dyn StorageBackend>,
        path: &str,
    ) {
        Self::warm_nar_from(tier, path, &TierNarSource::new(from, path)).await;
    }

    // ── read/write failure accounting ──────────────────────────────────────

    /// Log a tier's read failure loudly and hand the error back for the caller
    /// to hold as "we could not rule this key out".
    ///
    /// Degrading is NOT the same as going quiet: a broken tier is an operational
    /// fault that must be visible even though the request survives it. This is
    /// the one place that guarantee lives, so it cannot be forgotten at a call
    /// site.
    fn note_tier_read_failure(tier: &'static str, key: &str, e: StoreError) -> StoreError {
        tracing::error!(
            tier = tier,
            key = %key,
            error = %e,
            "tiered: READ FAILED on a tier — falling through to the next tier; \
             this tier is degraded and needs attention",
        );
        e
    }

    /// Collapse the two durable tiers' write results into one outcome.
    ///
    /// **Succeeds if at least one durable tier accepted the write.** A single
    /// broken durable tier must not zero out the other: with L2 (Postgres) down
    /// and L3 (object/disk) healthy, the old first-`?` behavior meant the L3
    /// write was never even attempted, so a push that could have half-landed
    /// landed nowhere. Since reads now fall through across tiers, content held
    /// by either durable tier is fully serveable.
    ///
    /// This deliberately weakens the old "both durable tiers always hold it"
    /// invariant to "at least one durable tier holds it". What operators
    /// actually depend on — *a read after a pod roll returns the bytes* — is
    /// preserved; what is lost is per-tier redundancy, which is logged as a
    /// partial write rather than hidden.
    fn durable_write_outcome(
        kind: &'static str,
        key: &str,
        l2: Result<(), StoreError>,
        l3: Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        match (l2, l3) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), Ok(())) => {
                warn!(
                    kind = kind, key = %key, tier = "l2", error = %e,
                    "tiered: durable write failed on ONE tier; the other durable tier \
                     accepted it, so the content is still serveable — redundancy lost",
                );
                Ok(())
            }
            (Ok(()), Err(e)) => {
                warn!(
                    kind = kind, key = %key, tier = "l3", error = %e,
                    "tiered: durable write failed on ONE tier; the other durable tier \
                     accepted it, so the content is still serveable — redundancy lost",
                );
                Ok(())
            }
            (Err(e2), Err(e3)) => {
                tracing::error!(
                    kind = kind, key = %key, l2_error = %e2, l3_error = %e3,
                    "tiered: durable write failed on EVERY durable tier — nothing was stored",
                );
                // Surface the L2 error; both are logged above.
                Err(e2)
            }
        }
    }
}

#[async_trait]
impl StorageBackend for TieredBackend {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, StoreError> {
        let mut broken: Option<StoreError> = None;

        // L1
        match self.l1.get_narinfo(hash).await {
            Ok(Some(v)) => return Ok(Some(v)),
            Ok(None) => {}
            Err(e) => broken = Some(Self::note_tier_read_failure("l1", hash, e)),
        }
        // L2 → promote to L1
        match self.l2.get_narinfo(hash).await {
            Ok(Some(v)) => {
                Self::warm_narinfo(&self.l1, hash, &v).await;
                return Ok(Some(v));
            }
            Ok(None) => {}
            Err(e) => broken = Some(Self::note_tier_read_failure("l2", hash, e)),
        }
        // L3 → promote to L2 then L1
        match self.l3.get_narinfo(hash).await {
            Ok(Some(v)) => {
                Self::warm_narinfo(&self.l2, hash, &v).await;
                Self::warm_narinfo(&self.l1, hash, &v).await;
                return Ok(Some(v));
            }
            Ok(None) => {}
            Err(e) => broken = Some(Self::note_tier_read_failure("l3", hash, e)),
        }

        match broken {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }

    async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        // ONE code path: the whole-value verb is the streaming verb drained, so
        // fall-through, promotion and error accounting cannot diverge between
        // the two shapes.
        match self.get_nar_stream(path).await? {
            Some(s) => Ok(Some(nar_stream::collect_nar(s, None).await?)),
            None => Ok(None),
        }
    }

    /// The composite's residency is its **weakest tier's** — a streaming
    /// resolver in front of a whole-value tier still materializes NARs.
    /// Reporting `Streaming` here because the resolver itself streams would be
    /// exactly the rounding-up this type exists to prevent.
    fn nar_residency(&self) -> NarResidency {
        self.l1
            .nar_residency()
            .weaker(self.l2.nar_residency())
            .weaker(self.l3.nar_residency())
    }

    /// Fall through the tiers and promote, **without ever holding the NAR**.
    ///
    /// Identical resolution order and identical promotion targets to
    /// `get_narinfo`; the only difference is that a promotion re-reads the tier
    /// that answered (see [`TierNarSource`]) instead of copying a buffer the
    /// caller is holding. The stream handed back is opened against the tier that
    /// actually had the content, so the caller's bytes never depend on whether a
    /// warm succeeded.
    async fn get_nar_stream(&self, path: &str) -> Result<Option<NarStream>, StoreError> {
        let mut broken: Option<StoreError> = None;

        match self.l1.get_nar_stream(path).await {
            Ok(Some(s)) => return Ok(Some(s)),
            Ok(None) => {}
            Err(e) => broken = Some(Self::note_tier_read_failure("l1", path, e)),
        }
        match self.l2.get_nar_stream(path).await {
            Ok(Some(s)) => {
                Self::warm_nar_from_tier(&self.l1, &self.l2, path).await;
                return Ok(Some(s));
            }
            Ok(None) => {}
            Err(e) => broken = Some(Self::note_tier_read_failure("l2", path, e)),
        }
        match self.l3.get_nar_stream(path).await {
            Ok(Some(s)) => {
                Self::warm_nar_from_tier(&self.l2, &self.l3, path).await;
                Self::warm_nar_from_tier(&self.l1, &self.l3, path).await;
                return Ok(Some(s));
            }
            Ok(None) => {}
            Err(e) => broken = Some(Self::note_tier_read_failure("l3", path, e)),
        }

        match broken {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }

    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), StoreError> {
        if self.write_policy == WritePolicy::WriteBack {
            Self::warm_narinfo(&self.l1, hash, content).await;
        }

        let l2 = self.l2.put_narinfo(hash, content).await;
        let l3 = self.l3.put_narinfo(hash, content).await;
        Self::durable_write_outcome("narinfo", hash, l2, l3)?;

        if self.write_policy == WritePolicy::WriteThrough {
            Self::warm_narinfo(&self.l1, hash, content).await;
        }
        Ok(())
    }

    async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), StoreError> {
        self.put_nar_stream(path, &nar_stream::BytesNarSource::from(data)).await
    }

    /// Fan a NAR out to the tiers **in the same order as before, from a
    /// re-openable source**.
    ///
    /// Line for line the previous `put_nar`, with `data: &[u8]` replaced by
    /// `src: &dyn NarSource` and each tier opening its own bounded stream. That
    /// equivalence is the reason [`NarSource`] is re-openable rather than a
    /// one-shot `Stream`: a one-shot stream can be consumed once, so it would
    /// force either buffering the NAR to fan it out (the bug) or interleaving
    /// chunks across tiers (a different order). The load-bearing properties are
    /// unchanged:
    ///
    /// - L2 and L3 are **both attempted**, then gated by
    ///   [`durable_write_outcome`](Self::durable_write_outcome);
    /// - the L1 warm happens strictly **after** that gate under `WriteThrough`
    ///   (strictly before it under `WriteBack`), and its result is discarded, so
    ///   a refused L1 warm can never fail a build.
    async fn put_nar_stream(&self, path: &str, src: &dyn NarSource) -> Result<(), StoreError> {
        if self.write_policy == WritePolicy::WriteBack {
            Self::warm_nar_from(&self.l1, path, src).await;
        }

        let l2 = self.l2.put_nar_stream(path, src).await;
        let l3 = self.l3.put_nar_stream(path, src).await;
        Self::durable_write_outcome("nar", path, l2, l3)?;

        if self.write_policy == WritePolicy::WriteThrough {
            Self::warm_nar_from(&self.l1, path, src).await;
        }
        Ok(())
    }

    async fn delete(&self, hash: &str) -> Result<(), StoreError> {
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

    async fn list_narinfos(&self) -> Result<Vec<String>, StoreError> {
        // Authoritative tiers only (L2 ∪ L3); L1 is a partial hot subset. A
        // durable-tier list failure surfaces (`?`).
        let mut set = BTreeSet::new();
        set.extend(self.l2.list_narinfos().await?);
        set.extend(self.l3.list_narinfos().await?);
        Ok(set.into_iter().collect())
    }

    /// Fan the wipe out to EVERY tier (L1 hot + L2/L3 durable), so a cache-wipe
    /// clears all three at once. Best-effort per tier (mirrors `delete`): one
    /// tier's failure is logged, never aborting the others — the whole point is
    /// to return the store to cold. Reports the largest per-tier narinfo count
    /// removed (the authoritative tiers' full set).
    async fn wipe_all(&self) -> Result<usize, StoreError> {
        let mut cleared = 0usize;
        for (name, tier) in [("l1", &self.l1), ("l2", &self.l2), ("l3", &self.l3)] {
            match tier.wipe_all().await {
                Ok(n) => cleared = cleared.max(n),
                Err(e) => warn!(tier = name, error = %e, "tiered: best-effort wipe failed"),
            }
        }
        Ok(cleared)
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
        reads_fail: Mutex<bool>,
    }

    impl MemBackend {
        fn has_narinfo(&self, hash: &str) -> bool {
            self.narinfo.lock().unwrap().contains_key(hash)
        }
        fn has_nar(&self, path: &str) -> bool {
            self.nar.lock().unwrap().contains_key(path)
        }
        fn clear(&self) {
            self.narinfo.lock().unwrap().clear();
            self.nar.lock().unwrap().clear();
        }
        fn set_writes_fail(&self, v: bool) {
            *self.writes_fail.lock().unwrap() = v;
        }
        /// Simulate a tier that is up but broken — the shape of a Postgres whose
        /// tables vanished, which errors on every query rather than returning
        /// "not found".
        fn set_reads_fail(&self, v: bool) {
            *self.reads_fail.lock().unwrap() = v;
        }
        fn fail_if_configured(&self) -> Result<(), StoreError> {
            if *self.writes_fail.lock().unwrap() {
                Err(StoreError::NotImplemented("mock writes disabled"))
            } else {
                Ok(())
            }
        }
        fn fail_reads_if_configured(&self) -> Result<(), StoreError> {
            if *self.reads_fail.lock().unwrap() {
                Err(StoreError::SchemaMissing("mock: relation does not exist".to_string()))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl StorageBackend for MemBackend {
        async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, StoreError> {
            self.fail_reads_if_configured()?;
            Ok(self.narinfo.lock().unwrap().get(hash).cloned())
        }
        async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), StoreError> {
            self.fail_if_configured()?;
            self.narinfo.lock().unwrap().insert(hash.to_string(), content.to_string());
            Ok(())
        }
        async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
            self.fail_reads_if_configured()?;
            Ok(self.nar.lock().unwrap().get(path).cloned())
        }
        async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), StoreError> {
            self.fail_if_configured()?;
            self.nar.lock().unwrap().insert(path.to_string(), data.to_vec());
            Ok(())
        }
        /// An in-memory double holds whole values by construction; declaring it
        /// is what keeps a *production* backend from inheriting this path.
        fn nar_residency(&self) -> NarResidency {
            NarResidency::WholeValue
        }
        async fn delete(&self, hash: &str) -> Result<(), StoreError> {
            self.narinfo.lock().unwrap().remove(hash);
            for ext in ["nar.xz", "nar.zst", "nar"] {
                self.nar.lock().unwrap().remove(&format!("nar/{hash}.{ext}"));
            }
            Ok(())
        }
        async fn list_narinfos(&self) -> Result<Vec<String>, StoreError> {
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

    // ── a BROKEN tier is stepped over, never fatal (the incident) ──────────

    #[tokio::test]
    async fn l2_read_failure_falls_through_to_l3() {
        // THE regression under test. Postgres (L2) came back on a wiped
        // emptyDir, so every L2 query errored with `relation … does not exist`.
        // The old code `?`d that error straight out, so L3 — which was healthy
        // and held the content — was never even consulted.
        let (l1, l2, l3, tiered) = mocks();
        l3.put_narinfo("h", NARINFO).await.unwrap();
        l3.put_nar("nar/h.nar.xz", b"blob").await.unwrap();
        l2.set_reads_fail(true);

        assert_eq!(
            tiered.get_narinfo("h").await.unwrap().unwrap(),
            NARINFO,
            "a broken L2 must not hide a healthy L3",
        );
        assert_eq!(tiered.get_nar("nar/h.nar.xz").await.unwrap().unwrap(), b"blob");
        // The hit still promotes into the tiers that can take it.
        assert!(l1.has_narinfo("h"), "the L3 hit still warms the working hot tier");
    }

    #[tokio::test]
    async fn broken_l1_and_l2_still_serve_from_l3() {
        let (l1, l2, l3, tiered) = mocks();
        l3.put_narinfo("h", NARINFO).await.unwrap();
        // Both upper tiers up-but-broken.
        l1.set_reads_fail(true);
        l2.set_reads_fail(true);
        assert_eq!(tiered.get_narinfo("h").await.unwrap().unwrap(), NARINFO);
    }

    #[tokio::test]
    async fn every_tier_broken_and_no_hit_surfaces_an_error_not_a_false_absence() {
        // Conservative + honest: when a tier that might have held the key is
        // broken and nothing was found, the resolver must NOT claim the key is
        // absent. It says "I could not answer"; the HTTP layer is what decides
        // to serve that as a miss.
        let (l1, l2, l3, tiered) = mocks();
        for t in [&l1, &l2, &l3] {
            t.set_reads_fail(true);
        }
        assert!(matches!(
            tiered.get_narinfo("h").await.unwrap_err(),
            StoreError::SchemaMissing(_),
        ));
        assert!(matches!(
            tiered.get_nar("nar/h.nar.xz").await.unwrap_err(),
            StoreError::SchemaMissing(_),
        ));
    }

    #[tokio::test]
    async fn all_tiers_healthy_and_empty_is_a_clean_miss_not_an_error() {
        // The other side of the same coin: no tier failed, so `Ok(None)` is the
        // truthful answer and must not be polluted into an error.
        let (_l1, _l2, _l3, tiered) = mocks();
        assert!(tiered.get_narinfo("ghost").await.unwrap().is_none());
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
    async fn one_broken_durable_tier_still_lands_the_write_on_the_other() {
        // Regression: the old code `?`d on the FIRST durable tier, so an L2
        // failure meant L3 was never even attempted and the push landed
        // NOWHERE. With Postgres (L2) OOM-killed and a healthy L3, every push
        // failed and the cache stopped filling entirely.
        let l1 = Arc::new(MemBackend::default());
        let l2 = Arc::new(MemBackend::default());
        let l3 = Arc::new(MemBackend::default());
        l2.set_writes_fail(true);
        let tiered = TieredBackend::new(l1.clone(), l2.clone(), l3.clone());

        tiered.put_narinfo("h", NARINFO).await.expect("one healthy durable tier must accept");
        tiered.put_nar("nar/h.nar.xz", b"blob").await.expect("one healthy durable tier must accept");

        assert!(!l2.has_narinfo("h"), "the broken tier holds nothing");
        assert!(l3.has_narinfo("h"), "the healthy durable tier MUST have taken the write");
        assert!(l3.has_nar("nar/h.nar.xz"));
        // …and the content is fully serveable through the resolver.
        assert_eq!(tiered.get_narinfo("h").await.unwrap().unwrap(), NARINFO);
    }

    #[tokio::test]
    async fn write_fails_only_when_every_durable_tier_rejects() {
        // The honest floor: nothing was stored anywhere, so the caller must be
        // told. A 200 here would falsely acknowledge an upload that never landed.
        let l1 = Arc::new(MemBackend::default());
        let l2 = Arc::new(MemBackend::default());
        let l3 = Arc::new(MemBackend::default());
        l2.set_writes_fail(true);
        l3.set_writes_fail(true);
        let tiered = TieredBackend::new(l1, l2, l3);
        let err = tiered.put_narinfo("h", NARINFO).await.unwrap_err();
        assert!(matches!(err, StoreError::NotImplemented(_)));
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
    async fn wipe_all_clears_every_tier() {
        let (l1, l2, l3, tiered) = mocks();
        // write-through seeds all three tiers.
        tiered.put_narinfo("h", NARINFO).await.unwrap();
        tiered.put_nar("nar/h.nar.xz", b"blob").await.unwrap();
        // a durable-only key proves the list-driven clear, not just the hot one.
        l2.put_narinfo("only2", "x").await.unwrap();

        let removed = tiered.wipe_all().await.unwrap();
        assert!(removed >= 1, "wipe reported nothing cleared");

        for t in [&l1, &l2, &l3] {
            assert!(t.list_narinfos().await.unwrap().is_empty(), "a tier survived the wipe");
            assert!(!t.has_narinfo("h"));
            assert!(!t.has_nar("nar/h.nar.xz"));
        }
        assert!(tiered.list_narinfos().await.unwrap().is_empty(), "cache not cold after wipe");
        // and the cache is genuinely cold — a fresh get misses.
        assert!(tiered.get_narinfo("h").await.unwrap().is_none());
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

    // ── streamed NAR fan-out: the ordering is the contract ─────────────────

    /// A tier that records the order in which it was written, into a log shared
    /// with its siblings. This is what turns "L2, then L3, then L1" from a
    /// comment into an assertion.
    struct RecordingTier {
        name: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
        /// When set, every NAR write is refused — the capped-hot-tier shape.
        refuse: bool,
    }

    #[async_trait]
    impl StorageBackend for RecordingTier {
        async fn get_narinfo(&self, _h: &str) -> Result<Option<String>, StoreError> {
            Ok(None)
        }
        async fn put_narinfo(&self, _h: &str, _c: &str) -> Result<(), StoreError> {
            Ok(())
        }
        async fn get_nar(&self, _p: &str) -> Result<Option<Vec<u8>>, StoreError> {
            Ok(None)
        }
        async fn put_nar(&self, _p: &str, _d: &[u8]) -> Result<(), StoreError> {
            self.log.lock().unwrap().push(self.name);
            if self.refuse {
                return Err(StoreError::TooLarge { limit: 1, at_least: 2 });
            }
            Ok(())
        }
        fn nar_residency(&self) -> NarResidency {
            NarResidency::WholeValue
        }
        async fn delete(&self, _h: &str) -> Result<(), StoreError> {
            Ok(())
        }
        async fn list_narinfos(&self) -> Result<Vec<String>, StoreError> {
            Ok(vec![])
        }
    }

    fn recording_tiers(
        refuse_l1: bool,
    ) -> (Arc<Mutex<Vec<&'static str>>>, TieredBackend) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mk = |name, refuse| {
            Arc::new(RecordingTier { name, log: Arc::clone(&log), refuse })
                as Arc<dyn StorageBackend>
        };
        let tiered = TieredBackend::new(mk("l1", refuse_l1), mk("l2", false), mk("l3", false));
        (log, tiered)
    }

    #[tokio::test]
    async fn streamed_put_writes_l2_then_l3_then_warms_l1() {
        // The ordering the operator called load-bearing, asserted rather than
        // described. Moving to a streamed fan-out was only safe because a
        // `NarSource` re-opens: a one-shot stream would have forced chunk
        // interleaving and this order would have become l1/l2/l3 all at once.
        let (log, tiered) = recording_tiers(false);
        tiered.put_nar("nar/x.nar.xz", b"blob").await.unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["l2", "l3", "l1"]);
    }

    #[tokio::test]
    async fn a_refused_l1_warm_never_fails_the_write() {
        // The capped hot tier returns TooLarge for an oversized NAR. That is a
        // bound working as designed, and it must be swallowed: L1 is
        // best-effort, and a failed warm that 500s the push would fail a build
        // over a cache optimisation.
        let (log, tiered) = recording_tiers(true);
        tiered
            .put_nar("nar/x.nar.xz", b"blob")
            .await
            .expect("a refused L1 warm must not fail the write");
        assert_eq!(*log.lock().unwrap(), vec!["l2", "l3", "l1"], "L1 is still attempted, last");
    }

    #[tokio::test]
    async fn write_back_warms_l1_before_the_durable_gate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mk = |name| {
            Arc::new(RecordingTier { name, log: Arc::clone(&log), refuse: false })
                as Arc<dyn StorageBackend>
        };
        let tiered = TieredBackend::with_write_policy(
            mk("l1"), mk("l2"), mk("l3"), WritePolicy::WriteBack,
        );
        tiered.put_nar("nar/x.nar.xz", b"blob").await.unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["l1", "l2", "l3"]);
    }

    #[tokio::test]
    async fn a_multi_chunk_nar_round_trips_through_a_real_disk_tier() {
        // Over a chunk boundary, on real files, through the resolver — the
        // shape a NAR big enough to matter actually takes.
        let dir = tempfile::tempdir().unwrap();
        let l1 = Arc::new(MemBackend::default());
        let l2: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir.path().join("l2")));
        let l3: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir.path().join("l3")));
        let tiered = TieredBackend::new(l1.clone(), l2, l3);

        let nar: Vec<u8> = (0..crate::storage::NAR_CHUNK_BYTES + 777).map(|i| (i % 251) as u8).collect();
        tiered.put_nar("nar/big.nar.xz", &nar).await.unwrap();
        assert_eq!(tiered.get_nar("nar/big.nar.xz").await.unwrap().unwrap(), nar);
    }

    #[tokio::test]
    async fn residency_reports_the_weakest_tier_not_the_resolver() {
        // Two real streaming tiers behind one in-memory double: the composite
        // is NOT streaming, and saying otherwise would be exactly the round-up
        // NarResidency exists to prevent.
        let dir = tempfile::tempdir().unwrap();
        let disk1: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir.path().join("a")));
        let disk2: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir.path().join("b")));
        let disk3: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir.path().join("c")));
        let all_disk = TieredBackend::new(disk1.clone(), disk2.clone(), disk3);
        assert_eq!(all_disk.nar_residency(), NarResidency::Streaming);

        let with_double =
            TieredBackend::new(Arc::new(MemBackend::default()), disk1, disk2);
        assert_eq!(with_double.nar_residency(), NarResidency::WholeValue);
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
