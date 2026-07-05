//! `freshness` — the **always-fresh cache** + **Redis/Postgres always-warm
//! tuning** surface of `/super-cache-ci` (elements 3 + 4 of the memory-first
//! core), lifted from the pure per-entry verdict in [`crate::memory`] into a
//! typed **maintenance pass** that **composes the real sui crates** — it forks
//! nothing.
//!
//! ## What it composes (never forks)
//!
//! - **`sui-cache` root-set GC** — the shipped [`sui_cache::gc::collect_garbage`]
//!   over the shipped [`sui_cache::StorageBackend`] trait *is* the cache-level
//!   evict-the-unreferenced primitive; [`SuiCacheMaintenance`] binds the pass to
//!   it directly (a `roots` set → everything not in it is deleted).
//! - **`sui-store` durable GC contract** — [`derive_store_gc_options`] wires the
//!   freshness [`COLD_SECS`](crate::memory::COLD_SECS) threshold into the
//!   store-level [`sui_store::GcOptions`], and [`GcOutcome`] unifies both the
//!   cache-level ([`sui_cache::GcResult`]) and store-level
//!   ([`sui_store::GcResult`]) results into one typed border.
//! - **The pure freshness classifier** — [`crate::memory::classify_freshness`]
//!   decides warm / keep / evict / gc per entry; this module aggregates those
//!   verdicts into a [`MaintenancePlan`] and drives it against a mockable
//!   [`CacheMaintenance`] environment (the pleme-io **default delivery method**:
//!   pure core + one injectable side-effect trait).
//! - **The always-warm tuning** — [`derive_always_warm_tuning`] composes
//!   [`crate::memory::derive_redis_tuning`] + [`crate::memory::derive_pg_tuning`]
//!   into one [`AlwaysWarmTuning`] bundle (Redis L1 maxmemory/evict-setpoint +
//!   Postgres index-and-pointer pool) so the cache is always warm and the store
//!   always durable.
//!
//! ## Tier-honest (never round up)
//!
//! - **Shipped (this module):** [`plan_maintenance`] (pure), [`warm_set`]
//!   (pure), [`derive_always_warm_tuning`] / [`derive_store_gc_options`] (pure),
//!   the [`CacheMaintenance`] seam + [`run_maintenance_pass`], and
//!   [`SuiCacheMaintenance`] whose `collect` step drives the **real**
//!   `sui_cache::gc::collect_garbage`. All are correct-by-test (a mock env AND an
//!   in-memory real `StorageBackend`).
//! - **LiveTODO(tiered):** [`SuiCacheMaintenance::warm`]/`evict` are
//!   `NotSupported` — the flat `StorageBackend` has no L1 tier; warm/evict land
//!   when the `RedisBackend` + `TieredBackend` (L1→L2→L3) ship.
//! - **LiveTODO(recency):** [`SuiCacheMaintenance::snapshot`] reports each entry
//!   conservatively (`referenced = true`, no recency) because the flat backend
//!   tracks neither hit-recency nor the reference graph — so the classifier never
//!   GC's on incomplete data. Full [`EntryStat`] needs the recency tracker + the
//!   durable Store's reference closure.
//! - **LiveTODO(loop):** the coordinator that *schedules* this pass tick-by-tick
//!   is [`autorevivy`]'s active-cache-maintenance loop (hooks `super_cache_redis_l1`
//!   / `super_cache_pg_store` / CLEAN) — design-stage; this module ships the
//!   **pass it calls**, never a second controller.
//! - **LiveTODO(pg):** `impl Store for PgStore` (the durable never-touch-disk
//!   store the `sui-store` GC contract runs against) is unbuilt — today the
//!   durable path is the on-disk `redb` graph store.
//!
//! [`autorevivy`]: https://github.com/pleme-io/autorevivy

use serde::{Deserialize, Serialize};

use crate::memory::{
    classify_freshness, derive_pg_tuning, derive_redis_tuning, EntryStat, FreshnessVerdict,
    PgTuning, RedisTuning, COLD_SECS,
};
use crate::{MemoryBand, SuperCacheCiConfig};

// ───────────────────────────────────────────────────────────────────────────
// Typed errors + the unified GC border
// ───────────────────────────────────────────────────────────────────────────

/// A failure in a maintenance step. Never a silent swallow — a `NotSupported`
/// step surfaces the tier gap mechanically instead of pretending success.
#[derive(Debug, thiserror::Error)]
pub enum MaintenanceError {
    /// A cache-backend (`sui-cache`) operation failed.
    #[error("cache backend error: {0}")]
    Cache(String),
    /// A durable-store (`sui-store`) operation failed.
    #[error("store backend error: {0}")]
    Store(String),
    /// The step is not supported by this backend yet (a named LiveTODO tier gap,
    /// not a silent no-op).
    #[error("maintenance step not supported: {0}")]
    NotSupported(&'static str),
}

impl From<sui_cache::CacheError> for MaintenanceError {
    fn from(e: sui_cache::CacheError) -> Self {
        Self::Cache(e.to_string())
    }
}

impl From<sui_store::StoreError> for MaintenanceError {
    fn from(e: sui_store::StoreError) -> Self {
        Self::Store(e.to_string())
    }
}

/// The unified result of a garbage-collection step — the typed border both the
/// cache-level ([`sui_cache::GcResult`]) and the store-level
/// ([`sui_store::GcResult`]) GC map into, so a maintenance report is
/// backend-agnostic.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GcOutcome {
    /// Store paths deleted (unreferenced + reclaimable).
    pub paths_deleted: usize,
    /// Bytes reclaimed.
    pub bytes_freed: u64,
}

impl From<sui_cache::GcResult> for GcOutcome {
    fn from(r: sui_cache::GcResult) -> Self {
        Self {
            paths_deleted: r.paths_deleted,
            bytes_freed: r.bytes_freed,
        }
    }
}

impl From<sui_store::GcResult> for GcOutcome {
    fn from(r: sui_store::GcResult) -> Self {
        Self {
            paths_deleted: r.paths_deleted,
            bytes_freed: r.bytes_freed,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Always-warm tuning — compose Redis (L1) + Postgres (L2) tuning
// ───────────────────────────────────────────────────────────────────────────

/// The always-warm tuning for the store+cache pair — **element 3**. Composes the
/// Redis hot-L1 knobs ([`RedisTuning`]) and the Postgres durable-L2 knobs
/// ([`PgTuning`]) into one bundle so a caller tunes both from one config: the
/// cache is always warm (Redis maxmemory = band ceiling, LRU eviction at the
/// setpoint, RDB warm-restart) and the store is always durable (Postgres holds
/// index + pointer, never NAR bytes).
///
/// *Derived* (computed from the config), never deserialized, so it does not
/// derive `Deserialize`: [`RedisTuning::policy`] is a `&'static str`, which serde
/// can only deserialize under a `'de: 'static` bound a wrapping struct cannot
/// propagate. `Serialize` (for a receipt/log) is unaffected.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlwaysWarmTuning {
    /// The Redis hot-L1 knobs derived from the cache band.
    pub redis: RedisTuning,
    /// The Postgres durable-L2 knobs derived from the store pool.
    pub pg: PgTuning,
}

/// Derive the always-warm tuning from the config — **element 3**.
/// Pure/deterministic. Composes [`derive_redis_tuning`] (from the cache
/// [`MemoryBand`] + `rdb_persist`) and [`derive_pg_tuning`] (from `pg_pool`).
#[must_use]
pub fn derive_always_warm_tuning(cfg: &SuperCacheCiConfig) -> AlwaysWarmTuning {
    AlwaysWarmTuning {
        redis: derive_redis_tuning(&cfg.cache.memory_band, cfg.cache.rdb_persist),
        pg: derive_pg_tuning(cfg.store.pg_pool),
    }
}

/// Derive the durable-store (Postgres) GC options from the freshness thresholds
/// — **element 4**, the `sui-store` half. Composes the shipped
/// [`sui_store::GcOptions`] builder: the store deletes paths cold past
/// [`COLD_SECS`](crate::memory::COLD_SECS), unbounded in bytes (the store is the
/// source of truth; the cache re-warms from it). `PgStore` implementing
/// [`sui_store::Store::collect_garbage`] is the LiveTODO this is authored for.
#[must_use]
pub fn derive_store_gc_options() -> sui_store::GcOptions {
    sui_store::GcOptions::default().with_delete_older_than(u64::from(COLD_SECS))
}

// ───────────────────────────────────────────────────────────────────────────
// The maintenance plan — aggregate per-entry verdicts into one pass
// ───────────────────────────────────────────────────────────────────────────

/// A typed maintenance plan for one freshness pass — the aggregation of
/// per-entry [`FreshnessVerdict`]s into the four action buckets. Deterministic:
/// buckets preserve the input entry order.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct MaintenancePlan {
    /// Keys to warm into Redis L1 (verdict [`FreshnessVerdict::Warm`]).
    pub promote: Vec<String>,
    /// Keys to demote out of L1 to reclaim RAM, kept durable (verdict
    /// [`FreshnessVerdict::Evict`]).
    pub demote: Vec<String>,
    /// The GC roots — every non-GC'd key (warm ∪ keep ∪ evict). Passed to the
    /// cache root-set GC; anything not here is reclaimed.
    pub roots: Vec<String>,
    /// The GC candidates — unreferenced + cold, re-derivable (verdict
    /// [`FreshnessVerdict::Gc`]). Deleted by the root-set GC (their complement of
    /// [`roots`](MaintenancePlan::roots)).
    pub collect: Vec<String>,
}

/// Aggregate a snapshot of entries into a [`MaintenancePlan`] — **element 4**.
/// Pure/deterministic. Composes [`crate::memory::classify_freshness`] per entry;
/// the resulting `roots` set is exactly the complement of the GC candidates, so
/// feeding it to [`sui_cache::gc::collect_garbage`] reclaims precisely the
/// cold+unreferenced entries and nothing else (never data-loss).
#[must_use]
pub fn plan_maintenance(
    entries: &[EntryStat],
    band: &MemoryBand,
    occupancy_pct: u8,
) -> MaintenancePlan {
    let mut plan = MaintenancePlan::default();
    for e in entries {
        match classify_freshness(e, band, occupancy_pct) {
            FreshnessVerdict::Warm => {
                plan.promote.push(e.key.clone());
                plan.roots.push(e.key.clone());
            }
            FreshnessVerdict::Keep => {
                plan.roots.push(e.key.clone());
            }
            FreshnessVerdict::Evict => {
                plan.demote.push(e.key.clone());
                plan.roots.push(e.key.clone());
            }
            FreshnessVerdict::Gc => {
                plan.collect.push(e.key.clone());
            }
        }
    }
    plan
}

/// The always-warm set — the keys that MUST stay resident in Redis L1 so the
/// cache never cold-serves (verdict [`FreshnessVerdict::Warm`]) — **element 4**.
/// Pure/deterministic; composes [`crate::memory::classify_freshness`]. This is
/// the "never a cold serve" guarantee surface: a warm-set entry is the
/// referenced-hot-recent working set that the coordinator keeps warm every tick.
#[must_use]
pub fn warm_set(entries: &[EntryStat], band: &MemoryBand, occupancy_pct: u8) -> Vec<String> {
    entries
        .iter()
        .filter(|e| classify_freshness(e, band, occupancy_pct) == FreshnessVerdict::Warm)
        .map(|e| e.key.clone())
        .collect()
}

// ───────────────────────────────────────────────────────────────────────────
// The maintenance pass — the Environment seam + the driver
// ───────────────────────────────────────────────────────────────────────────

/// The injectable side-effect seam for a freshness maintenance pass — the
/// pleme-io **default delivery method** (pure core + one mockable trait). Real
/// implementations bind to a `sui-cache` backend / a durable Store; tests mock
/// it. The pass ([`run_maintenance_pass`]) is a total function of this trait's
/// observations, so it is verifiable without any live Redis/Postgres.
#[async_trait::async_trait]
pub trait CacheMaintenance {
    /// Observe the current cache entries (the classifier input).
    async fn snapshot(&self) -> Result<Vec<EntryStat>, MaintenanceError>;
    /// Warm a key into the hot L1 tier ahead of / to keep it resident.
    async fn warm(&mut self, key: &str) -> Result<(), MaintenanceError>;
    /// Demote a key out of the hot L1 tier to reclaim RAM (kept durable).
    async fn evict(&mut self, key: &str) -> Result<(), MaintenanceError>;
    /// Run the root-set GC: delete everything NOT in `roots` (the complement is
    /// the cold+unreferenced set). Composes the cache's shipped root-set GC.
    async fn collect(&mut self, roots: &[String]) -> Result<GcOutcome, MaintenanceError>;
}

/// The typed receipt of one maintenance pass.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct MaintenanceReport {
    /// Entries warmed into L1.
    pub warmed: usize,
    /// Entries demoted out of L1.
    pub demoted: usize,
    /// Entries kept (the GC-root count).
    pub kept: usize,
    /// The garbage-collection outcome for the pass.
    pub gc: GcOutcome,
}

/// Run one always-fresh maintenance pass — **element 4**, the loop's per-tick
/// body. Observes the environment, aggregates a [`MaintenancePlan`], warms the
/// hot set, demotes the cold-under-pressure set, and runs the root-set GC over
/// the kept roots — returning a typed [`MaintenanceReport`]. Errors propagate (a
/// `NotSupported` warm/evict surfaces the tier gap rather than silently
/// swallowing it). The *scheduling* of this pass is autorevivy's coordinator
/// (LiveTODO(loop)); this is the body it calls.
pub async fn run_maintenance_pass(
    band: &MemoryBand,
    occupancy_pct: u8,
    env: &mut impl CacheMaintenance,
) -> Result<MaintenanceReport, MaintenanceError> {
    let entries = env.snapshot().await?;
    let plan = plan_maintenance(&entries, band, occupancy_pct);
    for key in &plan.promote {
        env.warm(key).await?;
    }
    for key in &plan.demote {
        env.evict(key).await?;
    }
    let gc = env.collect(&plan.roots).await?;
    Ok(MaintenanceReport {
        warmed: plan.promote.len(),
        demoted: plan.demote.len(),
        kept: plan.roots.len(),
        gc,
    })
}

// ───────────────────────────────────────────────────────────────────────────
// The real adapter — bind the pass to a shipped sui_cache::StorageBackend
// ───────────────────────────────────────────────────────────────────────────

/// The real [`CacheMaintenance`] adapter over a shipped
/// [`sui_cache::StorageBackend`] — composes the **real** root-set GC
/// ([`sui_cache::gc::collect_garbage`]), never a fork.
///
/// Tier-honest: `collect` is fully wired; `snapshot` is a conservative partial
/// observation (LiveTODO(recency) — the flat backend tracks neither hit-recency
/// nor the reference graph, so every entry is reported `referenced = true` and
/// the classifier never GC's on incomplete data); `warm`/`evict` are
/// `NotSupported` until the tiered `RedisBackend`/`TieredBackend` L1 ships
/// (LiveTODO(tiered)).
pub struct SuiCacheMaintenance<'a> {
    backend: &'a dyn sui_cache::StorageBackend,
}

impl<'a> SuiCacheMaintenance<'a> {
    /// Bind a maintenance adapter to a shipped storage backend.
    #[must_use]
    pub fn new(backend: &'a dyn sui_cache::StorageBackend) -> Self {
        Self { backend }
    }
}

#[async_trait::async_trait]
impl CacheMaintenance for SuiCacheMaintenance<'_> {
    async fn snapshot(&self) -> Result<Vec<EntryStat>, MaintenanceError> {
        // Enumerate the backend's keys. LiveTODO(recency): the flat backend has
        // no hit-recency or reference graph, so report conservatively — never GC
        // on incomplete metadata. Size stays 0 here (the durable Store's
        // metadata surfaces it once the recency tracker ships).
        let keys = self.backend.list_narinfos().await?;
        Ok(keys
            .into_iter()
            .map(|key| EntryStat {
                key,
                size_mib: 0,
                hits: 0,
                secs_since_use: 0,
                referenced: true,
            })
            .collect())
    }

    async fn warm(&mut self, _key: &str) -> Result<(), MaintenanceError> {
        Err(MaintenanceError::NotSupported(
            "warm: tiered L1 (RedisBackend/TieredBackend) not yet shipped",
        ))
    }

    async fn evict(&mut self, _key: &str) -> Result<(), MaintenanceError> {
        Err(MaintenanceError::NotSupported(
            "evict: tiered L1 (RedisBackend/TieredBackend) not yet shipped",
        ))
    }

    async fn collect(&mut self, roots: &[String]) -> Result<GcOutcome, MaintenanceError> {
        // Compose the REAL shipped root-set GC — never a fork.
        let res = sui_cache::gc::collect_garbage(self.backend, roots).await?;
        Ok(res.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    // The real StorageBackend trait must be in scope for the in-memory double's
    // methods to be callable directly in assertions.
    use sui_cache::StorageBackend;

    fn band(target: u8, headroom: u8, max: u32, dry: bool) -> MemoryBand {
        MemoryBand {
            target_pct: target,
            headroom_pct: headroom,
            max_mib: max,
            dry_run: dry,
        }
    }

    fn stat(key: &str, referenced: bool, hits: u32, secs: u32) -> EntryStat {
        EntryStat {
            key: key.to_string(),
            size_mib: 10,
            hits,
            secs_since_use: secs,
            referenced,
        }
    }

    // ── plan_maintenance — the freshness aggregation ───────────────────────

    #[test]
    fn plan_buckets_every_verdict_and_excludes_gc_from_roots() {
        // warm (hot+recent), evict (cold under pressure), gc (unref+cold),
        // keep (recent, not hot). occupancy 90 > band setpoint 80.
        let entries = vec![
            stat("warm", true, 5, 10),
            stat("evict", true, 1, 400),
            stat("gc", false, 0, 4000),
            stat("keep", true, 1, 100),
        ];
        let plan = plan_maintenance(&entries, &band(80, 20, 8192, false), 90);
        assert_eq!(plan.promote, vec!["warm"]);
        assert_eq!(plan.demote, vec!["evict"]);
        assert_eq!(plan.collect, vec!["gc"]);
        // roots = every non-GC key, in input order.
        assert_eq!(plan.roots, vec!["warm", "evict", "keep"]);
        // The GC candidate is precisely the complement of roots.
        assert!(!plan.roots.contains(&"gc".to_string()));
    }

    #[test]
    fn plan_is_deterministic_in_input_order() {
        let entries = vec![stat("z", true, 5, 1), stat("a", true, 5, 1)];
        let plan = plan_maintenance(&entries, &band(80, 20, 8192, false), 50);
        assert_eq!(plan.promote, vec!["z", "a"], "input order preserved");
    }

    #[test]
    fn plan_no_pressure_keeps_all_as_roots_and_collects_nothing() {
        let entries = vec![stat("a", true, 1, 100), stat("b", true, 1, 100)];
        let plan = plan_maintenance(&entries, &band(80, 20, 8192, false), 50);
        assert!(plan.collect.is_empty(), "nothing cold+unref → nothing GC'd");
        assert_eq!(plan.roots.len(), 2, "everything kept");
        assert!(plan.demote.is_empty());
    }

    // ── warm_set — the never-cold-serve guarantee ──────────────────────────

    #[test]
    fn warm_set_is_only_the_hot_recent_working_set() {
        let entries = vec![
            stat("hot", true, 5, 10),
            stat("cold", true, 0, 5000),
            stat("hot2", true, 9, 1),
        ];
        let ws = warm_set(&entries, &band(80, 20, 8192, false), 50);
        assert_eq!(ws, vec!["hot", "hot2"]);
    }

    // ── always-warm tuning — compose Redis + PG ────────────────────────────

    #[test]
    fn always_warm_tuning_composes_redis_and_pg_from_config() {
        let cfg = <SuperCacheCiConfig as shikumi::TieredConfig>::prescribed_default();
        let t = derive_always_warm_tuning(&cfg);
        // Redis maxmemory = band ceiling; eviction at the 80% setpoint.
        assert_eq!(t.redis.maxmemory_mib, 8192);
        assert_eq!(t.redis.evict_at_mib, 6553);
        assert_eq!(t.redis.policy, "allkeys-lru");
        assert!(t.redis.rdb_persist, "prescribed cache persists the hot set");
        // Postgres holds index + pointer over the prescribed 16-conn pool.
        assert_eq!(t.pg.pool, 16);
        assert!(t.pg.statement_cache);
        assert!(t.pg.index_and_pointer_only);
    }

    #[test]
    fn store_gc_options_wire_the_freshness_threshold() {
        let opts = derive_store_gc_options();
        assert_eq!(opts.delete_older_than, Some(u64::from(COLD_SECS)));
        assert_eq!(opts.max_freed, 0, "unbounded — the store is source of truth");
    }

    // ── GcOutcome — unify the two GC result types ──────────────────────────

    #[test]
    fn gc_outcome_from_cache_and_store_results_agree() {
        let from_cache: GcOutcome = sui_cache::GcResult {
            paths_deleted: 3,
            bytes_freed: 999,
        }
        .into();
        let from_store: GcOutcome = sui_store::GcResult {
            paths_deleted: 3,
            bytes_freed: 999,
        }
        .into();
        assert_eq!(from_cache, from_store);
        assert_eq!(from_cache.paths_deleted, 3);
    }

    // ── run_maintenance_pass — drive a mock environment ────────────────────

    #[derive(Default)]
    struct MockMaintenance {
        entries: Vec<EntryStat>,
        warmed: Vec<String>,
        evicted: Vec<String>,
        gc_roots: Vec<String>,
    }

    #[async_trait::async_trait]
    impl CacheMaintenance for MockMaintenance {
        async fn snapshot(&self) -> Result<Vec<EntryStat>, MaintenanceError> {
            Ok(self.entries.clone())
        }
        async fn warm(&mut self, key: &str) -> Result<(), MaintenanceError> {
            self.warmed.push(key.to_string());
            Ok(())
        }
        async fn evict(&mut self, key: &str) -> Result<(), MaintenanceError> {
            self.evicted.push(key.to_string());
            Ok(())
        }
        async fn collect(&mut self, roots: &[String]) -> Result<GcOutcome, MaintenanceError> {
            self.gc_roots = roots.to_vec();
            Ok(GcOutcome {
                paths_deleted: self.entries.len().saturating_sub(roots.len()),
                bytes_freed: 0,
            })
        }
    }

    #[tokio::test]
    async fn pass_warms_demotes_and_collects_per_verdict() {
        let mut env = MockMaintenance {
            entries: vec![
                stat("warm", true, 5, 10),
                stat("evict", true, 1, 400),
                stat("gc", false, 0, 4000),
                stat("keep", true, 1, 100),
            ],
            ..Default::default()
        };
        let report = run_maintenance_pass(&band(80, 20, 8192, false), 90, &mut env)
            .await
            .unwrap();
        assert_eq!(report.warmed, 1);
        assert_eq!(report.demoted, 1);
        assert_eq!(report.kept, 3);
        assert_eq!(report.gc.paths_deleted, 1, "only the cold+unref entry is reaped");
        assert_eq!(env.warmed, vec!["warm"]);
        assert_eq!(env.evicted, vec!["evict"]);
        assert!(!env.gc_roots.contains(&"gc".to_string()));
    }

    // ── SuiCacheMaintenance — compose the REAL sui_cache root-set GC ────────

    /// An in-memory `sui_cache::StorageBackend` — proves composition of the real
    /// `collect_garbage` with ZERO disk (honoring never-touch-disk even in test).
    struct MemBackend {
        narinfos: Mutex<HashMap<String, String>>,
        nars: Mutex<HashMap<String, Vec<u8>>>,
    }

    fn nar_path(hash: &str) -> String {
        let mut s = String::from("nar/");
        s.push_str(hash);
        s.push_str(".nar.xz");
        s
    }

    fn mk_narinfo(hash: &str) -> String {
        // Built by concatenation (no format!()), valid enough for NarInfo::parse
        // so collect_garbage can account bytes_freed.
        let mut s = String::from("StorePath: /nix/store/");
        s.push_str(hash);
        s.push_str("-pkg\nURL: ");
        s.push_str(&nar_path(hash));
        s.push_str(
            "\nCompression: xz\nFileHash: sha256:aaaa\nFileSize: 100\n\
             NarHash: sha256:bbbb\nNarSize: 5000\nReferences: \n",
        );
        s
    }

    impl MemBackend {
        fn with(hashes: &[&str]) -> Self {
            let me = Self {
                narinfos: Mutex::new(HashMap::new()),
                nars: Mutex::new(HashMap::new()),
            };
            {
                let mut ni = me.narinfos.lock().unwrap();
                let mut na = me.nars.lock().unwrap();
                for h in hashes {
                    ni.insert((*h).to_string(), mk_narinfo(h));
                    na.insert(nar_path(h), b"nar-bytes".to_vec());
                }
            }
            me
        }
    }

    #[async_trait::async_trait]
    impl sui_cache::StorageBackend for MemBackend {
        async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, sui_cache::CacheError> {
            Ok(self.narinfos.lock().unwrap().get(hash).cloned())
        }
        async fn put_narinfo(
            &self,
            hash: &str,
            content: &str,
        ) -> Result<(), sui_cache::CacheError> {
            self.narinfos
                .lock()
                .unwrap()
                .insert(hash.to_string(), content.to_string());
            Ok(())
        }
        async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, sui_cache::CacheError> {
            Ok(self.nars.lock().unwrap().get(path).cloned())
        }
        async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), sui_cache::CacheError> {
            self.nars
                .lock()
                .unwrap()
                .insert(path.to_string(), data.to_vec());
            Ok(())
        }
        async fn delete(&self, hash: &str) -> Result<(), sui_cache::CacheError> {
            self.narinfos.lock().unwrap().remove(hash);
            self.nars.lock().unwrap().remove(&nar_path(hash));
            Ok(())
        }
        async fn list_narinfos(&self) -> Result<Vec<String>, sui_cache::CacheError> {
            let mut v: Vec<String> = self.narinfos.lock().unwrap().keys().cloned().collect();
            v.sort();
            Ok(v)
        }
    }

    #[tokio::test]
    async fn adapter_collect_composes_real_gc_deleting_non_roots() {
        let backend = MemBackend::with(&["keep", "drop"]);
        let mut env = SuiCacheMaintenance::new(&backend);
        // roots = ["keep"] → the real collect_garbage reaps "drop".
        let out = env.collect(&["keep".to_string()]).await.unwrap();
        assert_eq!(out.paths_deleted, 1);
        assert!(out.bytes_freed > 0, "real GC accounts FileSize + narinfo text");
        assert!(backend.get_narinfo("keep").await.unwrap().is_some());
        assert!(backend.get_narinfo("drop").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn adapter_full_pass_is_a_safe_noop_until_recency_ships() {
        // The conservative snapshot marks every entry referenced with no recency,
        // so all become GC roots → nothing warmed, nothing demoted, nothing
        // reaped. Honest: end-to-end composition with the real backend, no
        // data-loss on incomplete metadata.
        let backend = MemBackend::with(&["a", "b"]);
        let mut env = SuiCacheMaintenance::new(&backend);
        let report = run_maintenance_pass(&band(80, 20, 8192, false), 90, &mut env)
            .await
            .unwrap();
        assert_eq!(report.warmed, 0);
        assert_eq!(report.demoted, 0);
        assert_eq!(report.kept, 2);
        assert_eq!(report.gc.paths_deleted, 0, "safe no-op on incomplete metadata");
        assert!(backend.get_narinfo("a").await.unwrap().is_some());
        assert!(backend.get_narinfo("b").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn adapter_warm_and_evict_surface_the_tier_gap_not_a_silent_noop() {
        let backend = MemBackend::with(&["x"]);
        let mut env = SuiCacheMaintenance::new(&backend);
        assert!(matches!(
            env.warm("x").await,
            Err(MaintenanceError::NotSupported(_))
        ));
        assert!(matches!(
            env.evict("x").await,
            Err(MaintenanceError::NotSupported(_))
        ));
    }
}
