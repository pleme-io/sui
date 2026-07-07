//! `memory` — the **memory-first core behavior** of `/super-cache-ci`.
//!
//! super-cache-ci does not touch disk: every workload is RAMDISK / in-memory
//! (the [★★ MAGMA-NATIVE never-touch-disk law] applied to the Nix store
//! itself), so **memory is THE resource** and every decision below optimizes
//! memory + the in-memory cache. This module is the **typed, pure, deterministic
//! decision core** for the seven memory-first elements — the per-tick
//! *Classify* + *Decide* beats of the controller. It is testable without a
//! cluster because it is a **total function of typed inputs**: the side effects
//! (reading breathe/Redis/Postgres, driving the actuators) live behind the
//! coordinator loop, which is a named **DESIGN/LiveTODO** composing
//! [`autorevivy`] — this module never forks it.
//!
//! ## The seven elements → the typed surface
//!
//! | # | Element | Typed decision (pure) |
//! |---|---|---|
//! | 1 | Preemptive memory via breathe shadowing | [`plan_precarve`] over [`MemoryForecast`] → [`PreCarve`] (predict → shadow → confirm → allocate) |
//! | 2 | Memory-tuned 100%-spot auction | [`score_memory_bid`] / [`rank_memory_bids`] over [`SpotCandidate`] (MiB-per-dollar, not vCPU) |
//! | 3 | Sui Redis + Postgres tuning | [`derive_redis_tuning`] / [`derive_pg_tuning`] from the [`MemoryBand`](crate::MemoryBand) |
//! | 4 | Always-fresh cache | [`classify_freshness`] over [`EntryStat`] → [`FreshnessVerdict`] (warm / keep / evict / gc) |
//! | 5 | Guardrail-optimal in-memory presentation | [`present`] over [`PresentationInput`] → [`PresentationVerdict`] (tier + priority) |
//! | 6 | Continuously-more-efficient | [`evaluate_tune`] over [`TuneProposal`] → [`TuneOutcome`] (accept-if-improved-else-revert, shadow-first) |
//! | 7 | The controller | [`CONTROLLER_ELEMENTS`] — a typed self-describing readiness ledger; the *loop* composes [`autorevivy`], **not built here** |
//!
//! ## Tier-honest (never round up)
//!
//! - **Shipped (this module):** the pure decision functions + their typed
//!   borders + exhaustive unit tests. These ARE the controller's Classify/Decide
//!   beats and are correct-by-test without any backend.
//! - **DESIGN / LiveTODO:** the Observe/Act loop that reads the live signals and
//!   drives the actuators (breathe band carve, Redis eviction, PG GC, spot
//!   re-diversification). That loop is [`autorevivy`]'s coordinator controller
//!   (design-stage; runs manually today) — this module is the substrate it will
//!   drive, not a second controller.
//! - **LiveTODO(lisp):** each typed surface names its `(def…)` authoring keyword
//!   ([`AUTHORING_KEYWORDS`]); the `#[derive(DeriveTataraDomain)]` attach is
//!   gated on the same sui-workspace `tatara-lisp` pin skew that keeps the
//!   derive off [`SuperCacheCiConfig`](crate::SuperCacheCiConfig) — naming the
//!   keyword without a working derive is the honest tier, not a rounded-up one.
//!
//! [★★ MAGMA-NATIVE never-touch-disk law]: https://github.com/pleme-io/theory/blob/main/MAGMA-OPERATOR-BACKEND.md
//! [`autorevivy`]: https://github.com/pleme-io/autorevivy

use serde::{Deserialize, Serialize};

use crate::MemoryBand;

// ───────────────────────────────────────────────────────────────────────────
// Element 1 — Preemptive memory allocation via breathe SHADOWING
//             predict → shadow → confirm → allocate
// ───────────────────────────────────────────────────────────────────────────

/// How a [`MemoryForecast`] was produced. Never a silent guess — the basis is
/// carried so a wrong forecast is attributable and the cold-start conservatism
/// is explicit.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForecastBasis {
    /// A prior attested build of this *exact* content-key — content-addressed
    /// memory of what the derivation actually peaked at (the strongest signal).
    PriorReceipt,
    /// A sibling/class heuristic: same gen language domain, similar closure
    /// size — used when this exact key has no prior receipt.
    ClassHeuristic,
    /// No prior signal — the conservative cold-start default (over-reserve so
    /// the first build of a key never OOM-cliffs).
    ColdStart,
}

/// A typed estimate of the RAM a build/derivation will need, produced **before**
/// the build runs so the band can be pre-carved (RAMDISK ⇒ memory is the whole
/// game). Authored `(defmemoryforecast …)` at the destination.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MemoryForecast {
    /// BLAKE3 content-key of the derivation the forecast is for (stable
    /// identity; the same key across runs is the same forecast target).
    pub deriv_key: String,
    /// The predicted central (≈p50) peak resident working set, MiB.
    pub predicted_peak_mib: u32,
    /// The predicted upper-tail (≈p99) peak, MiB — what the band ceiling must
    /// cover so the build does not OOM-cliff.
    pub predicted_tail_mib: u32,
    /// How the estimate was produced.
    pub basis: ForecastBasis,
}

impl MemoryForecast {
    /// The conservative cold-start forecast for a key with no prior signal —
    /// reserve `peak_mib` centrally and `2×` at the tail. Over-reserving on the
    /// first build is the never-OOM-cliff posture; the second build refines it
    /// from a real receipt ([`ForecastBasis::PriorReceipt`]).
    #[must_use]
    pub fn cold_start(deriv_key: &str, peak_mib: u32) -> Self {
        Self {
            deriv_key: deriv_key.to_string(),
            predicted_peak_mib: peak_mib,
            predicted_tail_mib: peak_mib.saturating_mul(2),
            basis: ForecastBasis::ColdStart,
        }
    }
}

/// The preemptive-allocation FSM phase — the furthest state a pre-carve reaches.
///
/// The progression is total and monotone in `(over_ceiling, dry_run)`:
/// `Predict` (a forecast exists) → `Shadow` (band computed *would-apply*, or the
/// forecast overflows the ceiling and must escalate) → `Confirm` (would-apply is
/// proven safe but held because the band is in `dry_run`) → `Allocate` (carve
/// LIVE ahead of the build).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllocationPhase {
    /// A [`MemoryForecast`] exists; nothing carved yet.
    Predict,
    /// Shadow decision computed (or the forecast overflows the ceiling and the
    /// band must escalate — never-stuck: expand, do not wedge).
    Shadow,
    /// The shadow carve is proven safe but **not** applied because the band is
    /// shadow-first (`dry_run`).
    Confirm,
    /// The band is carved LIVE, ahead of the build.
    Allocate,
}

/// The typed pre-carve decision produced by [`plan_precarve`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreCarve {
    /// The furthest FSM phase reached.
    pub phase: AllocationPhase,
    /// The RAM to reserve ahead of the build, MiB — the tail estimate grossed
    /// up by the band headroom, clamped to the band ceiling.
    pub carve_mib: u32,
    /// The forecast tail exceeds the band ceiling — the band cannot cover this
    /// build and must escalate (bigger band / on-demand ladder). Never-stuck:
    /// this surfaces an escalation, it never silently under-carves and OOMs.
    pub needs_escalation: bool,
    /// The carve is shadow-only (the band is `dry_run`) — observe
    /// `ShadowWouldApply` before flipping LIVE.
    pub shadow_only: bool,
}

/// Plan the preemptive band carve for a forecast under a band posture —
/// **element 1**, the `predict → shadow → confirm → allocate` decision as a
/// total function. Pure: no I/O, deterministic for `(forecast, band)`.
///
/// The carve is the tail estimate grossed up by the band headroom so the build
/// runs with slack, clamped to the ceiling. If even the raw tail overflows the
/// ceiling, [`PreCarve::needs_escalation`] is set (the band escalates rather
/// than wedging). The band's `dry_run` gate is honored: a safe shadow carve
/// stops at [`AllocationPhase::Confirm`] and is marked
/// [`PreCarve::shadow_only`].
#[must_use]
pub fn plan_precarve(forecast: &MemoryForecast, band: &MemoryBand) -> PreCarve {
    let ceiling = band.max_mib;
    // Gross the tail up by headroom so the live band holds slack above the
    // predicted peak (integer math for determinism).
    let with_slack = forecast
        .predicted_tail_mib
        .saturating_mul(100u32.saturating_add(u32::from(band.headroom_pct)))
        / 100;

    if ceiling > 0 && forecast.predicted_tail_mib > ceiling {
        // The forecast overflows the hard ceiling — carve what we can and
        // escalate the rest. Never-stuck: expand, do not wedge.
        return PreCarve {
            phase: AllocationPhase::Shadow,
            carve_mib: ceiling,
            needs_escalation: true,
            shadow_only: band.dry_run,
        };
    }

    let carve_mib = if ceiling == 0 {
        with_slack
    } else {
        with_slack.min(ceiling)
    };

    if band.dry_run {
        // Proven safe in shadow, but held because the band is shadow-first.
        PreCarve {
            phase: AllocationPhase::Confirm,
            carve_mib,
            needs_escalation: false,
            shadow_only: true,
        }
    } else {
        PreCarve {
            phase: AllocationPhase::Allocate,
            carve_mib,
            needs_escalation: false,
            shadow_only: false,
        }
    }
}

/// The typed breathe carve **request** a [`PreCarve`] lifts into — the
/// composition border to breathe's `MemoryBand` + shadow gate. [`plan_precarve`]
/// decides *how much* to reserve; this names *which band* to carve and maps the
/// decision onto breathe's real actuator contract:
/// [`shadow_only`](PrecarveRequest::shadow_only) is breathe-core's
/// `ReconcileInput.dry_run` (observe `ShadowWouldApply` before mutating),
/// [`escalate`](PrecarveRequest::escalate) drives the on-demand ladder
/// (never-stuck), and [`carve_mib`](PrecarveRequest::carve_mib) is the
/// pre-build reservation (breathe-core `ReconcileInput.force` — a bounded,
/// through-the-gate carve). Authored `(defprecarverequest …)` at the
/// destination.
///
/// **Composes breathe, does not fork it:** this is the typed *input* the
/// [`autorevivy`] `breathe_bands` coordinator submits to `breathe-core`; the
/// actual submission is a named **LiveTODO** (the Observe/Act loop), never a
/// second controller.
///
/// [`autorevivy`]: https://github.com/pleme-io/autorevivy
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PrecarveRequest {
    /// The breathe band to pre-carve (composes `BreatheCfg.band_ref` /
    /// `CacheCfg.memory_band` from the config surface).
    pub band_ref: String,
    /// The RAM to reserve ahead of the build, MiB (from [`PreCarve::carve_mib`]).
    pub carve_mib: u32,
    /// Shadow-first: maps to breathe-core `ReconcileInput.dry_run` — observe
    /// `ShadowWouldApply` before flipping the band LIVE.
    pub shadow_only: bool,
    /// The forecast overflows the band ceiling — climb the on-demand ladder for
    /// the overflow rather than wedging (never-stuck).
    pub escalate: bool,
    /// How the forecast was produced (provenance — a wrong pre-carve is
    /// attributable, never a silent guess).
    pub basis: ForecastBasis,
}

/// Lift a forecast + band into the typed breathe carve request — **element 1**,
/// the composition border. Pure/deterministic: runs [`plan_precarve`] and maps
/// its decision onto breathe's `MemoryBand` + shadow-gate contract, tagging the
/// target band. This is `predict → shadow → confirm → allocate` expressed *over
/// breathe*, ready for the [`autorevivy`] `breathe_bands` coordinator to submit
/// (the submission is the LiveTODO loop, not built here).
///
/// [`autorevivy`]: https://github.com/pleme-io/autorevivy
#[must_use]
pub fn precarve_request(
    forecast: &MemoryForecast,
    band: &MemoryBand,
    band_ref: &str,
) -> PrecarveRequest {
    let pc = plan_precarve(forecast, band);
    PrecarveRequest {
        band_ref: band_ref.to_string(),
        carve_mib: pc.carve_mib,
        shadow_only: pc.shadow_only,
        escalate: pc.needs_escalation,
        basis: forecast.basis,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Element 2 — Memory-tuned 100%-spot AUCTION (breathe leilao, memory axis)
// ───────────────────────────────────────────────────────────────────────────

/// A candidate spot-instance offering the breathe auction (leilao) may bid on.
/// Integer fields keep scoring deterministic (no float non-determinism).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SpotCandidate {
    /// The instance family (e.g. `"r6i"` — memory-optimized).
    pub family: String,
    /// RAM the instance offers, MiB — **the axis super-cache-ci bids on**.
    pub mib: u32,
    /// vCPU count — carried for the record but **deliberately NOT scored**:
    /// super-cache-ci is memory-bound, so the auction optimizes MiB-per-dollar,
    /// not compute.
    pub vcpu: u16,
    /// Spot price, milli-USD per hour (integer for deterministic ranking).
    pub price_milli: u32,
    /// Reclaim-frequency bucket `0..=100` (lower is more stable) — the auction
    /// penalizes churny families so a mid-build reclaim is rarer.
    pub interrupt_rate: u8,
}

/// A scored bid — higher [`score`](MemoryBid::score) is a better memory buy.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MemoryBid {
    /// The candidate family this bid scores.
    pub family: String,
    /// MiB-per-dollar, discounted by the interrupt rate. Higher = better.
    pub score: u64,
}

/// Score one spot candidate on the **memory axis** — **element 2**. Pure,
/// integer, deterministic. `score = (MiB per milli-dollar) × (100 −
/// interrupt_rate) / 100`; vCPU is intentionally ignored (memory-bound
/// workload). This is the memory-tuned input the breathe leilao consumes to
/// diversify 100%-spot capacity — the auction is composed, not forked.
#[must_use]
pub fn score_memory_bid(c: &SpotCandidate) -> MemoryBid {
    let mib_per_dollar = (u64::from(c.mib) * 1000) / u64::from(c.price_milli.max(1));
    let interrupt = u64::from(c.interrupt_rate.min(100));
    let score = mib_per_dollar * (100 - interrupt) / 100;
    MemoryBid {
        family: c.family.clone(),
        score,
    }
}

/// Rank candidates best-memory-buy-first — **element 2**. Deterministic: sorted
/// by [`score`](MemoryBid::score) descending, ties broken by family name so the
/// ordering is stable across runs (a tenet of a reproducible auction).
#[must_use]
pub fn rank_memory_bids(candidates: &[SpotCandidate]) -> Vec<MemoryBid> {
    let mut bids: Vec<MemoryBid> = candidates.iter().map(score_memory_bid).collect();
    bids.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.family.cmp(&b.family)));
    bids
}

/// The **memory-tuned auction config** — the typed 100%-spot posture *on the
/// memory axis* (element 2's config half; the flat `spot_families: Vec<String>`
/// on `ArchAsg` carries none of it). breathe's leiloeiro (`BandLeiloeiro`)
/// diversifies 100%-spot capacity; this **constrains + biases** that
/// diversification toward memory, because super-cache-ci is memory-bound
/// (RAMDISK ⇒ MiB is the buy, never vCPU). Authored `(defmemoryauction …)` at
/// the destination.
///
/// Every field has a real behavioral effect in [`plan_memory_auction`] (no
/// vestigial knob): [`min_mib`](MemoryAuctionCfg::min_mib) is the memory floor,
/// [`max_interrupt_rate`](MemoryAuctionCfg::max_interrupt_rate) the reclaim
/// ceiling, [`diversify_top_n`](MemoryAuctionCfg::diversify_top_n) the
/// never-single-family cap. The *memory axis itself* is structural — the plan
/// ranks via [`rank_memory_bids`], which scores MiB-per-dollar and ignores vCPU
/// by construction — so it is not a togglable flag.
///
/// **Composes breathe-auction, does not fork it:** [`plan_memory_auction`]
/// produces the ranked, filtered candidate set the `BandLeiloeiro` consumes as
/// its memory-tuned input; the auction (diversification, grant allocation) stays
/// breathe's.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryAuctionCfg {
    /// The minimum RAM (MiB) a candidate must offer to be bid on — a
    /// memory-starved instance is never bid (memory is the resource). `0`
    /// disables the floor.
    pub min_mib: u32,
    /// The maximum reclaim-frequency bucket (`0..=100`) a family may carry — a
    /// churnier family is rejected, because a mid-build reclaim throws away the
    /// whole in-RAM build.
    pub max_interrupt_rate: u8,
    /// How many top-ranked families 100%-spot diversifies across — never a
    /// single family (a whole-fleet reclaim of one family must not stall the
    /// build). `0` disables the cap (rank all survivors).
    pub diversify_top_n: u8,
}

impl MemoryAuctionCfg {
    /// The memory-blind floor (no auction opinion) — the `bare()` tier. No
    /// floor, no reclaim ceiling, no diversification cap.
    #[must_use]
    pub fn unset() -> Self {
        Self {
            min_mib: 0,
            max_interrupt_rate: 100,
            diversify_top_n: 0,
        }
    }

    /// The prescribed camelot memory-tuned posture: reject a candidate below
    /// 8 GiB or churnier than 20%, and diversify 100%-spot across the top 3
    /// memory-ranked families.
    #[must_use]
    pub fn camelot() -> Self {
        Self {
            min_mib: 8192,
            max_interrupt_rate: 20,
            diversify_top_n: 3,
        }
    }
}

/// The memory-tuned auction plan — the diversified, memory-ranked spot set the
/// breathe leiloeiro consumes, plus the never-stuck escalation flag.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MemoryAuctionPlan {
    /// The ranked, constraint-filtered, top-N diversified memory bids — the
    /// memory-tuned input to breathe's `BandLeiloeiro`.
    pub bids: Vec<MemoryBid>,
    /// Every candidate was filtered out (all too small / too churny) — the
    /// leiloeiro has nothing to bid, so climb the on-demand ladder rather than
    /// wedging (never-stuck).
    pub needs_escalation: bool,
}

/// Plan the memory-tuned 100%-spot auction — **element 2**, the config half.
/// Pure/deterministic: filter candidates by the config's memory floor + reclaim
/// ceiling, rank the survivors on the memory axis ([`rank_memory_bids`]), and
/// diversify across the top-N families. If the constraints empty the field,
/// [`MemoryAuctionPlan::needs_escalation`] is set (climb the on-demand ladder;
/// never wedge). This is the memory-tuned *input* breathe's `BandLeiloeiro`
/// diversifies — the auction is composed, not forked.
#[must_use]
pub fn plan_memory_auction(
    cfg: &MemoryAuctionCfg,
    candidates: &[SpotCandidate],
) -> MemoryAuctionPlan {
    let survivors: Vec<SpotCandidate> = candidates
        .iter()
        .filter(|c| c.mib >= cfg.min_mib && c.interrupt_rate <= cfg.max_interrupt_rate)
        .cloned()
        .collect();
    let mut bids = rank_memory_bids(&survivors);
    if cfg.diversify_top_n > 0 {
        bids.truncate(cfg.diversify_top_n as usize);
    }
    MemoryAuctionPlan {
        needs_escalation: bids.is_empty(),
        bids,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Element 3 — SUI REDIS + POSTGRES tuning derived from the MemoryBand
// ───────────────────────────────────────────────────────────────────────────

/// Concrete Redis hot-L1 knobs derived deterministically from a band —
/// **element 3**. `maxmemory` is the band ceiling; eviction fires at the band
/// setpoint (`target_pct` of the ceiling), leaving `headroom_pct` before the
/// hard OOM cliff; the policy is `allkeys-lru` (the cache is reconstructible, so
/// any key may be evicted). RDB/AOF persistence enables a warm restart.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedisTuning {
    /// Redis `maxmemory`, MiB — the band ceiling.
    pub maxmemory_mib: u32,
    /// The occupancy, MiB, at which LRU eviction begins (the band setpoint).
    pub evict_at_mib: u32,
    /// The eviction policy (a `&'static str`; the cache is fully reconstructible
    /// so all-keys LRU is safe).
    pub policy: &'static str,
    /// Persist the hot set (RDB/AOF) for a warm restart.
    pub rdb_persist: bool,
}

/// Derive Redis hot-L1 tuning from a band — **element 3**. Pure/deterministic.
#[must_use]
pub fn derive_redis_tuning(band: &MemoryBand, rdb_persist: bool) -> RedisTuning {
    let evict_at = band.max_mib.saturating_mul(u32::from(band.target_pct)) / 100;
    RedisTuning {
        maxmemory_mib: band.max_mib,
        evict_at_mib: evict_at,
        policy: "allkeys-lru",
        rdb_persist,
    }
}

/// Concrete Postgres durable-L2 knobs — **element 3**. The pool is the config's
/// `pg_pool`; the prepared-statement cache is on (the store's queries are a
/// small fixed set, so caching plans is pure win). Postgres holds the
/// content-addressed **index + BLAKE3 pointer**, never multi-GB NAR rows.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PgTuning {
    /// Connection-pool size.
    pub pool: u32,
    /// Cache prepared-statement plans (the store's query set is small + fixed).
    pub statement_cache: bool,
    /// The store holds index + pointer only; large blobs go to L3.
    pub index_and_pointer_only: bool,
}

/// Derive Postgres durable-L2 tuning from the pool size — **element 3**.
/// Pure/deterministic. A `0` pool (the honest `bare()` floor) yields a disabled
/// pool, not a silent default.
#[must_use]
pub fn derive_pg_tuning(pg_pool: u32) -> PgTuning {
    PgTuning {
        pool: pg_pool,
        statement_cache: pg_pool > 0,
        index_and_pointer_only: true,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Element 4 — ALWAYS-FRESH cache: evict / warm / GC verdict
// ───────────────────────────────────────────────────────────────────────────

/// Observed stats for one cache entry — the input to the freshness classifier.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct EntryStat {
    /// The entry's content-key.
    pub key: String,
    /// Its resident size, MiB.
    pub size_mib: u32,
    /// Lifetime hit count.
    pub hits: u32,
    /// Seconds since the entry was last used.
    pub secs_since_use: u32,
    /// Whether the entry is still a GC root (referenced by a live closure). An
    /// unreferenced entry is re-derivable, so it may be deleted, not just
    /// demoted.
    pub referenced: bool,
}

/// The freshness decision for one entry — **element 4**. `Evict` demotes to a
/// lower durable tier to reclaim RAM (never data-loss); `Gc` deletes an
/// unreferenced, cold, re-derivable entry.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessVerdict {
    /// Hot + referenced → promote/keep in Redis L1.
    Warm,
    /// Referenced but not hot → keep durable (L2/L3), do not spend L1 RAM.
    Keep,
    /// Cold under band pressure → demote out of L1 to reclaim RAM (recoverable).
    Evict,
    /// Unreferenced + cold → delete (reclaimable, re-derivable). Composes
    /// sui-cache's root-set GC with band-aware recency.
    Gc,
}

/// Coldness threshold (seconds) past which an unreferenced entry is GC-able.
pub const COLD_SECS: u32 = 3600;
/// Recency threshold (seconds) under which a referenced hot entry stays warm.
pub const WARM_SECS: u32 = 300;
/// Hit count at/above which a referenced, recent entry is considered hot.
pub const HOT_HITS: u32 = 3;

/// Classify one entry's freshness under a band + current occupancy —
/// **element 4**. Pure/deterministic. Composes sui-cache's root-set GC
/// (`referenced`) with memory-band-aware LRU (`occupancy_pct` vs the band
/// setpoint) so the cache is never stale and never cold-serves.
#[must_use]
pub fn classify_freshness(e: &EntryStat, band: &MemoryBand, occupancy_pct: u8) -> FreshnessVerdict {
    if !e.referenced && e.secs_since_use >= COLD_SECS {
        return FreshnessVerdict::Gc;
    }
    if e.referenced && e.hits >= HOT_HITS && e.secs_since_use < WARM_SECS {
        return FreshnessVerdict::Warm;
    }
    // Over the band setpoint and not recently used → demote to reclaim RAM.
    if band.target_pct > 0 && occupancy_pct > band.target_pct && e.secs_since_use >= WARM_SECS {
        return FreshnessVerdict::Evict;
    }
    FreshnessVerdict::Keep
}

// ───────────────────────────────────────────────────────────────────────────
// Element 5 — GUARDRAIL-like OPTIMAL in-memory (RAMDISK) presentation
// ───────────────────────────────────────────────────────────────────────────

/// Which tier an entry currently sits in (or should sit in) for optimal
/// presentation to the build layer.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheTier {
    /// Redis hot L1 (RAM) — the fastest, most-present tier.
    L1Redis,
    /// Postgres durable L2 (index + pointer).
    L2Pg,
    /// Object-store L3 (large content-addressed NAR bytes).
    L3Object,
    /// Not currently cached anywhere.
    Cold,
}

/// The input to the presentation guard — what the build DAG predicts about an
/// entry's near-future need.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PresentationInput {
    /// The entry's content-key.
    pub key: String,
    /// Seconds until the build graph is predicted to need this entry (from the
    /// dependency DAG's topological frontier).
    pub predicted_next_use_secs: u32,
    /// The entry's size, MiB.
    pub size_mib: u32,
    /// Where the entry currently sits.
    pub tier: CacheTier,
}

/// The presentation decision — where the entry *should* sit and how urgently to
/// stage it, for the most efficient RAMDISK presentation to the build layer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PresentationVerdict {
    /// The entry's content-key.
    pub key: String,
    /// The tier the entry should occupy for optimal presentation.
    pub target_tier: CacheTier,
    /// Staging priority — higher = present sooner/hotter. Inverse of the
    /// predicted time-to-need, so the build layer's next-needed derivations are
    /// warmed first.
    pub priority: u64,
}

/// Soon-needed threshold (seconds) — under this, a small entry is warmed into
/// L1 ahead of need.
pub const SOON_SECS: u32 = 30;
/// The size (MiB) at/under which a soon-needed entry is warmed whole into L1;
/// larger soon-needed entries stay in L2 (pointer) and stream on demand.
pub const SMALL_MIB: u32 = 256;
/// The size (MiB) above which a not-soon entry is pushed to L3 object store.
pub const HUGE_MIB: u32 = 1024;

/// Compute the optimal in-memory presentation for one entry — **element 5**.
/// Pure/deterministic. This is the guardrail-*like* guard (mirroring
/// `guardrail`'s `Rule → Decision` engine, applied to cache presentation
/// instead of prompt safety): it constantly organizes which derivations sit
/// hottest so the build layer always sees the optimal RAMDISK presentation.
#[must_use]
pub fn present(i: &PresentationInput) -> PresentationVerdict {
    let soon = i.predicted_next_use_secs <= SOON_SECS;
    let target_tier = if soon {
        if i.size_mib <= SMALL_MIB {
            CacheTier::L1Redis // warm the whole small entry into RAM ahead of need
        } else {
            CacheTier::L2Pg // large + soon: keep the pointer hot, stream on demand
        }
    } else if i.size_mib > HUGE_MIB {
        CacheTier::L3Object // far + huge: bytes belong in L3, not RAM
    } else {
        i.tier // far + modest: leave where it is
    };
    // Inverse time-to-need → priority; +1 avoids divide-by-zero for imminent use.
    let priority = 1_000_000u64 / u64::from(i.predicted_next_use_secs.saturating_add(1));
    PresentationVerdict {
        key: i.key.clone(),
        target_tier,
        priority,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Element 6 — CONTINUOUSLY-MORE-EFFICIENT (lapidar accept-if-improved-else-revert)
// ───────────────────────────────────────────────────────────────────────────

/// A measured efficiency reading of the build service at a point in time.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EfficiencyReading {
    /// Cache hit rate, percent (higher is better).
    pub hit_rate_pct: u8,
    /// Build p50 latency, milliseconds (lower is better).
    pub build_p50_ms: u32,
    /// Seconds spent off spot / on on-demand (want ≈ 0; lower is better).
    pub ondemand_secs: u32,
    /// Over-carved RAM, MiB — waste (lower is better).
    pub ram_waste_mib: u32,
}

impl EfficiencyReading {
    /// A single scalar objective — higher is better. Combines the four axes with
    /// fixed integer weights (deterministic; no float drift). Used by
    /// [`evaluate_tune`] to compare a shadow reading against the baseline.
    #[must_use]
    pub fn objective(&self) -> i64 {
        i64::from(self.hit_rate_pct) * 100
            - i64::from(self.build_p50_ms) / 10
            - i64::from(self.ondemand_secs)
            - i64::from(self.ram_waste_mib) / 100
    }
}

/// The knob a tune proposal adjusts.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TuneKnob {
    /// Redis `maxmemory` / eviction setpoint.
    RedisMaxmemory,
    /// The tmpfs build-sandbox band.
    TmpfsBand,
    /// The spot instance family mix.
    SpotFamilies,
    /// The Postgres pool size.
    PgPool,
}

/// A shadow-measured tune proposal — a knob change with the baseline reading and
/// the reading measured **in shadow** after the change (shadow-first: the effect
/// is observed before any live commit).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TuneProposal {
    /// Which knob this proposal moves.
    pub knob: TuneKnob,
    /// The reading before the change.
    pub before: EfficiencyReading,
    /// The reading measured in shadow after the change.
    pub after_shadow: EfficiencyReading,
}

/// The outcome of evaluating a tune — accept it or revert it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TuneOutcome {
    /// The shadow reading strictly improves the objective past the margin —
    /// promote the knob change to LIVE.
    Accept,
    /// No improvement past the margin — revert; keep the prior posture.
    Revert,
}

/// The objective margin a tune must clear to be accepted — dampens churn so the
/// self-tuner does not thrash on noise.
pub const TUNE_MARGIN: i64 = 50;

/// Evaluate a shadow-measured tune — **element 6**, the lapidar-style
/// accept-if-improved-else-revert verdict. Pure/deterministic and **shadow-first
/// by construction**: the decision is a total function of the two readings, and
/// `after_shadow` is measured before any live mutation, so a regression is
/// reverted before it ever ships. This makes the build layer *continuously more
/// efficient* — it only ever keeps changes that measurably helped.
#[must_use]
pub fn evaluate_tune(p: &TuneProposal) -> TuneOutcome {
    if p.after_shadow.objective() > p.before.objective() + TUNE_MARGIN {
        TuneOutcome::Accept
    } else {
        TuneOutcome::Revert
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Element 7 — THE CONTROLLER (tier-honest assessment; the LOOP composes autorevivy)
// ───────────────────────────────────────────────────────────────────────────

/// The status of an element's Observe/Act *loop* (as opposed to its pure
/// decision core, which ships here).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoopTier {
    /// The pure per-tick decision function ships in this module + is tested.
    ShippedPureCore,
    /// The Observe/Act loop that drives it against live signals is design-stage
    /// (autorevivy's coordinator; runs manually today).
    DesignLiveTodo,
}

/// One row of the controller readiness ledger — a typed, self-describing record
/// of what is buildable **now** vs what is a **LiveTODO**, per element. This is
/// the honest answer to "is the controller space legit to code NOW?": the
/// per-tick *pure cores* are shipped + tested here; the *loop* that ticks them
/// is [`autorevivy`](https://github.com/pleme-io/autorevivy)'s coordinator —
/// composed, never forked. No second controller is invented here.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerElement {
    /// The element's short name.
    pub element: &'static str,
    /// The pure decision function that ships in this module.
    pub decision_fn: &'static str,
    /// Whether that pure core is shipped + tested (all `true` here).
    pub pure_core_shipped: bool,
    /// The Observe/Act loop status.
    pub loop_tier: LoopTier,
    /// The shipped primitive whose control surface the loop drives (never a
    /// fork).
    pub composes: &'static str,
}

/// The controller readiness ledger — **element 7** as typed self-describing
/// data (CATALOG REFLECTION applied to the controller). Six rows, one per
/// decision core (elements 1–6); every pure core is shipped + tested, every
/// loop is a LiveTODO that composes an already-shipped primitive. The
/// verdict this encodes: **the pure decision core is legit to build now (done);
/// the reconciler loop is autorevivy's coordinator (design-stage) — do not
/// invent a competing controller.**
pub const CONTROLLER_ELEMENTS: [ControllerElement; 7] = [
    ControllerElement {
        element: "preemptive-memory",
        decision_fn: "plan_precarve",
        pure_core_shipped: true,
        loop_tier: LoopTier::DesignLiveTodo,
        composes: "breathe MemoryBand + ShadowGate (autorevivy hook: breathe_bands)",
    },
    ControllerElement {
        element: "memory-spot-auction",
        decision_fn: "score_memory_bid / rank_memory_bids",
        pure_core_shipped: true,
        loop_tier: LoopTier::DesignLiveTodo,
        composes: "breathe-auction (leilao) 100%-spot (autorevivy hook: spot_posture)",
    },
    ControllerElement {
        element: "sui-redis-pg-tuning",
        decision_fn: "derive_redis_tuning / derive_pg_tuning",
        pure_core_shipped: true,
        loop_tier: LoopTier::DesignLiveTodo,
        composes: "sui-cache RedisBackend + sui-store PgStore (autorevivy hooks: super_cache_redis_l1, super_cache_pg_store)",
    },
    ControllerElement {
        element: "always-fresh-cache",
        decision_fn: "classify_freshness",
        pure_core_shipped: true,
        loop_tier: LoopTier::DesignLiveTodo,
        composes: "sui-cache::gc root-set + autorevivy CLEAN (active cache maintenance)",
    },
    ControllerElement {
        element: "guardrail-presentation",
        decision_fn: "present",
        pure_core_shipped: true,
        loop_tier: LoopTier::DesignLiveTodo,
        composes: "guardrail Rule->Decision engine pattern + TieredBackend resolver",
    },
    ControllerElement {
        element: "continuous-efficiency",
        decision_fn: "evaluate_tune",
        pure_core_shipped: true,
        loop_tier: LoopTier::DesignLiveTodo,
        composes: "lapidar self-tune + autorevivy escalating-by-discoverability ladder",
    },
    ControllerElement {
        // Element 8 — the perpetual cache-warming core (crate::preheat): keep the
        // super-cache HOT so a build substitutes warm instead of cold-compiling.
        // The pure classify/plan/floor/promessa brain ships + is tested; the tick
        // LOOP is autorevivy's CLEAN face (superCacheCiRef), with the
        // camelot-cache-warm workflow as the running interim actuator.
        element: "perpetual-cache-warming",
        decision_fn: "plan_preheat / classify_target / plan_floor",
        pure_core_shipped: true,
        loop_tier: LoopTier::DesignLiveTodo,
        composes: "camelot-cache-warm workflow (interim) + autorevivy CLEAN (superCacheCiRef) + breathe floor spin",
    },
];

/// The `(def…)` authoring keywords for the memory-first typed surfaces —
/// **the vocabulary bridge**. Each names itself in the tatara-lisp vocabulary;
/// the `#[derive(DeriveTataraDomain)]` attach is a LiveTODO gated on the same
/// sui-workspace `tatara-lisp` pin skew that keeps the derive off
/// [`SuperCacheCiConfig`](crate::SuperCacheCiConfig). Naming the keyword without
/// a compiling derive is the honest tier — not a rounded-up "Lisp surface
/// works".
pub const AUTHORING_KEYWORDS: [&str; 8] = [
    "defmemoryforecast",   // element 1 — MemoryForecast / PreCarve
    "defprecarverequest",  // element 1 — PrecarveRequest (breathe carve border)
    "defmemorybid",        // element 2 — SpotCandidate / MemoryBid
    "defmemoryauction",    // element 2 — MemoryAuctionCfg / MemoryAuctionPlan
    "defcachetuning",      // element 3 — RedisTuning / PgTuning
    "deffreshnessverdict", // element 4 — EntryStat / FreshnessVerdict
    "defpresentationplan", // element 5 — PresentationInput / PresentationVerdict
    "defefficiencytune",   // element 6 — TuneProposal / TuneOutcome
];

#[cfg(test)]
mod tests {
    use super::*;

    fn band(target: u8, headroom: u8, max: u32, dry: bool) -> MemoryBand {
        MemoryBand {
            target_pct: target,
            headroom_pct: headroom,
            max_mib: max,
            dry_run: dry,
        }
    }

    // ── Element 1 ──────────────────────────────────────────────────────────

    #[test]
    fn precarve_shadow_first_stops_at_confirm_when_dry_run() {
        let f = MemoryForecast::cold_start("abc", 1000); // tail 2000
        let b = band(80, 20, 8192, true);
        let pc = plan_precarve(&f, &b);
        assert_eq!(pc.phase, AllocationPhase::Confirm);
        assert!(pc.shadow_only);
        assert!(!pc.needs_escalation);
        // tail 2000 grossed up by 20% headroom = 2400, under the 8192 ceiling.
        assert_eq!(pc.carve_mib, 2400);
    }

    #[test]
    fn precarve_allocates_live_when_band_is_not_dry_run() {
        let f = MemoryForecast::cold_start("abc", 1000);
        let b = band(80, 20, 8192, false);
        let pc = plan_precarve(&f, &b);
        assert_eq!(pc.phase, AllocationPhase::Allocate);
        assert!(!pc.shadow_only);
    }

    #[test]
    fn precarve_escalates_never_wedges_when_forecast_overflows_ceiling() {
        let f = MemoryForecast::cold_start("big", 5000); // tail 10000 > 8192
        let b = band(80, 20, 8192, false);
        let pc = plan_precarve(&f, &b);
        assert_eq!(pc.phase, AllocationPhase::Shadow);
        assert!(pc.needs_escalation, "over-ceiling must escalate, not wedge");
        assert_eq!(pc.carve_mib, 8192, "carve what the band can, escalate the rest");
    }

    #[test]
    fn cold_start_over_reserves_the_tail() {
        let f = MemoryForecast::cold_start("k", 512);
        assert_eq!(f.basis, ForecastBasis::ColdStart);
        assert_eq!(f.predicted_tail_mib, 1024, "cold-start tail is 2x peak");
    }

    #[test]
    fn precarve_request_maps_shadow_to_dry_run_and_tags_the_band() {
        let f = MemoryForecast::cold_start("abc", 1000); // tail 2000
        let b = band(80, 20, 8192, true); // dry_run band = shadow gate
        let req = precarve_request(&f, &b, "super-cache-ci");
        assert_eq!(req.band_ref, "super-cache-ci", "the carve request tags the band");
        // tail 2000 grossed up by 20% headroom = 2400, under the 8192 ceiling.
        assert_eq!(req.carve_mib, 2400);
        assert!(
            req.shadow_only,
            "a dry-run band => shadow_only (breathe ReconcileInput.dry_run) — observe before mutating"
        );
        assert!(!req.escalate);
        assert_eq!(req.basis, ForecastBasis::ColdStart, "provenance is carried, never a silent guess");
    }

    #[test]
    fn precarve_request_escalates_over_ceiling_never_wedges() {
        let f = MemoryForecast::cold_start("big", 5000); // tail 10000 > 8192
        let b = band(80, 20, 8192, false);
        let req = precarve_request(&f, &b, "super-cache-ci");
        assert!(req.escalate, "over-ceiling climbs the on-demand ladder, never wedges");
        assert_eq!(req.carve_mib, 8192, "carve what the band can, escalate the rest");
        assert!(!req.shadow_only, "non-dry-run band carves LIVE");
    }

    // ── Element 2 ──────────────────────────────────────────────────────────

    #[test]
    fn memory_bid_favors_mib_per_dollar_ignoring_vcpu() {
        // Same price; the memory-heavy family wins even with fewer vCPU.
        let mem = SpotCandidate { family: "r6i".into(), mib: 32768, vcpu: 8, price_milli: 500, interrupt_rate: 0 };
        let cpu = SpotCandidate { family: "c7i".into(), mib: 8192, vcpu: 16, price_milli: 500, interrupt_rate: 0 };
        assert!(score_memory_bid(&mem).score > score_memory_bid(&cpu).score);
    }

    #[test]
    fn memory_bid_penalizes_churny_families() {
        let stable = SpotCandidate { family: "a".into(), mib: 16384, vcpu: 4, price_milli: 400, interrupt_rate: 0 };
        let churny = SpotCandidate { family: "b".into(), mib: 16384, vcpu: 4, price_milli: 400, interrupt_rate: 50 };
        assert!(score_memory_bid(&stable).score > score_memory_bid(&churny).score);
    }

    #[test]
    fn rank_is_deterministic_desc_with_stable_tiebreak() {
        let cands = vec![
            SpotCandidate { family: "z".into(), mib: 8192, vcpu: 4, price_milli: 500, interrupt_rate: 0 },
            SpotCandidate { family: "a".into(), mib: 8192, vcpu: 4, price_milli: 500, interrupt_rate: 0 },
            SpotCandidate { family: "top".into(), mib: 32768, vcpu: 4, price_milli: 500, interrupt_rate: 0 },
        ];
        let ranked = rank_memory_bids(&cands);
        assert_eq!(ranked[0].family, "top");
        // equal-score tie broken by family name ascending.
        assert_eq!(ranked[1].family, "a");
        assert_eq!(ranked[2].family, "z");
    }

    #[test]
    fn auction_filters_below_the_memory_floor() {
        let cfg = MemoryAuctionCfg::camelot(); // min 8192 MiB
        let cands = vec![
            SpotCandidate { family: "small".into(), mib: 4096, vcpu: 4, price_milli: 200, interrupt_rate: 0 },
            SpotCandidate { family: "big".into(), mib: 32768, vcpu: 4, price_milli: 500, interrupt_rate: 0 },
        ];
        let plan = plan_memory_auction(&cfg, &cands);
        assert_eq!(plan.bids.len(), 1, "the sub-floor family is filtered out (memory is the resource)");
        assert_eq!(plan.bids[0].family, "big");
        assert!(!plan.needs_escalation);
    }

    #[test]
    fn auction_rejects_churny_families_and_escalates_when_empty() {
        let cfg = MemoryAuctionCfg::camelot(); // max_interrupt 20
        let cands = vec![SpotCandidate {
            family: "churny".into(),
            mib: 16384,
            vcpu: 4,
            price_milli: 400,
            interrupt_rate: 50, // over the 20 ceiling — a mid-build reclaim wastes the whole in-RAM build
        }];
        let plan = plan_memory_auction(&cfg, &cands);
        assert!(plan.bids.is_empty(), "a family churnier than the ceiling is rejected");
        assert!(
            plan.needs_escalation,
            "all candidates filtered => escalate to on-demand, never wedge (never-stuck)"
        );
    }

    #[test]
    fn auction_diversifies_across_top_n_never_single_family() {
        let cfg = MemoryAuctionCfg::camelot(); // top 3
        let cands = vec![
            SpotCandidate { family: "a".into(), mib: 32768, vcpu: 4, price_milli: 500, interrupt_rate: 0 },
            SpotCandidate { family: "b".into(), mib: 16384, vcpu: 4, price_milli: 500, interrupt_rate: 0 },
            SpotCandidate { family: "c".into(), mib: 12288, vcpu: 4, price_milli: 500, interrupt_rate: 0 },
            SpotCandidate { family: "d".into(), mib: 10240, vcpu: 4, price_milli: 500, interrupt_rate: 0 },
        ];
        let plan = plan_memory_auction(&cfg, &cands);
        assert_eq!(plan.bids.len(), 3, "diversify across the top 3, not all 4 (100%-spot resilience)");
        assert_eq!(plan.bids[0].family, "a", "top family is the biggest MiB-per-dollar");
    }

    #[test]
    fn auction_unset_floor_keeps_all_survivors() {
        let cfg = MemoryAuctionCfg::unset();
        let cands = vec![SpotCandidate {
            family: "tiny".into(),
            mib: 512,
            vcpu: 1,
            price_milli: 100,
            interrupt_rate: 90,
        }];
        let plan = plan_memory_auction(&cfg, &cands);
        // unset floor (0) + max_interrupt 100 + no top-N cap keeps even a tiny churny candidate.
        assert_eq!(plan.bids.len(), 1);
        assert!(!plan.needs_escalation);
    }

    // ── Element 3 ──────────────────────────────────────────────────────────

    #[test]
    fn redis_tuning_evicts_at_the_band_setpoint() {
        let t = derive_redis_tuning(&band(80, 20, 8192, true), true);
        assert_eq!(t.maxmemory_mib, 8192);
        assert_eq!(t.evict_at_mib, 6553); // 80% of 8192
        assert_eq!(t.policy, "allkeys-lru");
        assert!(t.rdb_persist);
    }

    #[test]
    fn pg_tuning_disables_cache_on_zero_pool_floor() {
        assert!(!derive_pg_tuning(0).statement_cache);
        let t = derive_pg_tuning(16);
        assert_eq!(t.pool, 16);
        assert!(t.statement_cache);
        assert!(t.index_and_pointer_only, "PG holds index+pointer, never NAR bytes");
    }

    // ── Element 4 ──────────────────────────────────────────────────────────

    fn stat(referenced: bool, hits: u32, secs: u32) -> EntryStat {
        EntryStat { key: "k".into(), size_mib: 10, hits, secs_since_use: secs, referenced }
    }

    #[test]
    fn freshness_gcs_unreferenced_cold_entries() {
        let v = classify_freshness(&stat(false, 0, COLD_SECS), &band(80, 20, 8192, false), 50);
        assert_eq!(v, FreshnessVerdict::Gc);
    }

    #[test]
    fn freshness_warms_referenced_hot_recent_entries() {
        let v = classify_freshness(&stat(true, HOT_HITS, 10), &band(80, 20, 8192, false), 50);
        assert_eq!(v, FreshnessVerdict::Warm);
    }

    #[test]
    fn freshness_evicts_cold_under_band_pressure() {
        // Over the 80% setpoint (occupancy 90) + not recently used → demote.
        let v = classify_freshness(&stat(true, 1, WARM_SECS), &band(80, 20, 8192, false), 90);
        assert_eq!(v, FreshnessVerdict::Evict);
    }

    #[test]
    fn freshness_keeps_when_no_pressure_and_not_gcable() {
        let v = classify_freshness(&stat(true, 1, WARM_SECS), &band(80, 20, 8192, false), 50);
        assert_eq!(v, FreshnessVerdict::Keep);
    }

    // ── Element 5 ──────────────────────────────────────────────────────────

    #[test]
    fn presentation_warms_small_soon_entries_into_l1() {
        let v = present(&PresentationInput { key: "k".into(), predicted_next_use_secs: 5, size_mib: 64, tier: CacheTier::L2Pg });
        assert_eq!(v.target_tier, CacheTier::L1Redis);
    }

    #[test]
    fn presentation_keeps_large_soon_entries_in_l2_pointer() {
        let v = present(&PresentationInput { key: "k".into(), predicted_next_use_secs: 5, size_mib: 4096, tier: CacheTier::L3Object });
        assert_eq!(v.target_tier, CacheTier::L2Pg);
    }

    #[test]
    fn presentation_pushes_far_huge_entries_to_l3() {
        let v = present(&PresentationInput { key: "k".into(), predicted_next_use_secs: 600, size_mib: 4096, tier: CacheTier::L1Redis });
        assert_eq!(v.target_tier, CacheTier::L3Object);
    }

    #[test]
    fn presentation_priority_is_inverse_of_time_to_need() {
        let soon = present(&PresentationInput { key: "a".into(), predicted_next_use_secs: 1, size_mib: 10, tier: CacheTier::Cold });
        let later = present(&PresentationInput { key: "b".into(), predicted_next_use_secs: 100, size_mib: 10, tier: CacheTier::Cold });
        assert!(soon.priority > later.priority);
    }

    // ── Element 6 ──────────────────────────────────────────────────────────

    #[test]
    fn tune_accepts_a_measured_improvement() {
        let before = EfficiencyReading { hit_rate_pct: 70, build_p50_ms: 2000, ondemand_secs: 100, ram_waste_mib: 500 };
        let after = EfficiencyReading { hit_rate_pct: 85, build_p50_ms: 1500, ondemand_secs: 0, ram_waste_mib: 200 };
        assert_eq!(evaluate_tune(&TuneProposal { knob: TuneKnob::RedisMaxmemory, before, after_shadow: after }), TuneOutcome::Accept);
    }

    #[test]
    fn tune_reverts_a_regression() {
        let before = EfficiencyReading { hit_rate_pct: 85, build_p50_ms: 1500, ondemand_secs: 0, ram_waste_mib: 200 };
        let worse = EfficiencyReading { hit_rate_pct: 60, build_p50_ms: 3000, ondemand_secs: 300, ram_waste_mib: 900 };
        assert_eq!(evaluate_tune(&TuneProposal { knob: TuneKnob::TmpfsBand, before, after_shadow: worse }), TuneOutcome::Revert);
    }

    #[test]
    fn tune_reverts_within_the_margin_no_churn() {
        // A tiny improvement under the margin must NOT churn the posture.
        let before = EfficiencyReading { hit_rate_pct: 80, build_p50_ms: 1000, ondemand_secs: 0, ram_waste_mib: 0 };
        let marginal = EfficiencyReading { hit_rate_pct: 80, build_p50_ms: 990, ondemand_secs: 0, ram_waste_mib: 0 };
        assert_eq!(evaluate_tune(&TuneProposal { knob: TuneKnob::PgPool, before, after_shadow: marginal }), TuneOutcome::Revert);
    }

    // ── Element 7 — the honesty gate ───────────────────────────────────────

    #[test]
    fn controller_ledger_is_tier_honest() {
        // Every pure core ships + is tested here; every LOOP is a LiveTODO that
        // composes an already-shipped primitive (never a forked controller).
        for e in &CONTROLLER_ELEMENTS {
            assert!(e.pure_core_shipped, "{}: pure core must be shipped", e.element);
            assert_eq!(
                e.loop_tier,
                LoopTier::DesignLiveTodo,
                "{}: the loop is autorevivy's coordinator, NOT built here — never round it up",
                e.element
            );
            assert!(!e.composes.is_empty(), "{}: must name what it composes", e.element);
        }
    }

    #[test]
    fn controller_ledger_composes_autorevivy_not_a_fork() {
        // At least the fresh-cache + efficiency loops must route through
        // autorevivy — the composition-index controller, never a second one.
        let mentions = CONTROLLER_ELEMENTS
            .iter()
            .filter(|e| e.composes.contains("autorevivy"))
            .count();
        assert!(mentions >= 3, "the loop must compose autorevivy, not fork a controller");
    }

    #[test]
    fn authoring_keywords_are_unique_and_named() {
        // Vocabulary bridge: every memory-first surface names its (def…) keyword.
        for k in AUTHORING_KEYWORDS {
            assert!(k.starts_with("def"), "{k} must be a def-form keyword");
        }
        let mut seen = std::collections::BTreeSet::new();
        for k in AUTHORING_KEYWORDS {
            assert!(seen.insert(k), "{k} keyword collision — must be globally unique");
        }
    }
}
