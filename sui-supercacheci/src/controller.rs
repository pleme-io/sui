//! `controller` — **element 7**, the tier-honest SKELETON: the one reconcile
//! loop that composes all six built decision cores into a single per-tick plan,
//! expressed against the **engenho `Controller` contract**.
//!
//! ## The verdict (tier-honest — SKELETON, not a live controller)
//!
//! The design's own assessment (encoded as [`crate::memory::CONTROLLER_ELEMENTS`])
//! holds: the six per-tick **pure decision cores** ship + are tested; every
//! element's **Observe/Act loop** is a `DesignLiveTodo` that composes an
//! already-shipped primitive. This module makes that split *executable*:
//!
//! - **BUILT (shipped + tested):** [`decide`] — the pure Classify/Decide brain
//!   that folds elements 1–6 ([`crate::memory`] + [`crate::freshness`] +
//!   [`crate::presentation`]) into one coherent [`TickPlan`], with **no I/O**;
//!   the [`Controller`] impl that drives it against an injected
//!   [`CacheEnvironment`]; and the shipped [`ShadowEnvironment`] that observes a
//!   supplied snapshot and **actuates nothing** (shadow-first, never-touch-disk,
//!   no cluster).
//! - **LiveTODO (named, never rounded up):**
//!   - *actuate* — the real [`CacheEnvironment`] whose `actuate` drives
//!     breathe carve / breathe-auction leilão / sui-cache warm·evict·GC /
//!     lapidar tune. Bound to [`autorevivy`]'s coordinator — **not a second
//!     controller** (autorevivy's closed coordinator is itself
//!     DESIGN/LiveTODO; forking it here is the cardinal anti-pattern).
//!   - *observe-feed* — the real Observe beat (breathe MCP band status, escuta
//!     CDC occupancy/hit-ratio, the build-DAG frontier, tameshi receipts for
//!     forecasts, the spot market, lapidar telemetry). The shipped path takes a
//!     hand-built [`CacheObservation`].
//!   - *engenho-bind* — the live `impl engenho_controllers::Controller for
//!     SuperCacheCiController` is a ~6-line adapter that must land **in
//!     engenho**: `sui-supercacheci` cannot depend on `engenho-controllers`
//!     because `engenho → sui` is the legal dependency edge (engenho-sui-typescape
//!     pulls `sui-eval`); the reverse is a **cycle** *and* a heavy K8s closure.
//!     So the [`Controller`] trait here is **shape-identical** to engenho's
//!     (`name` + async `tick` → [`ReconcileOutcome`] carrying a
//!     [`ReconcileResult`] requeue). **Destination (Op-Principle #1, solve
//!     once):** extract engenho's `Controller` trait to a shared leaf crate both
//!     `engenho` and `sui` implement — then this mirror is deleted and both bind
//!     the one trait. The mirror is the honest interim, named as such.
//!
//! ## The reconcile contract (one tick)
//!
//! ```text
//! Observe ─ env.observe() ───────────────▶ CacheObservation   (LiveTODO feed)
//!   │
//! Decide  ─ decide(cfg, obs) ─────────────▶ TickPlan          (BUILT — 6 cores)
//!   │        1 precarve · 2 auction · 3 tuning · 4 maintenance
//!   │        5 presentation(+guard) · 6 self-tune
//!   │
//! Act     ─ env.actuate(&plan) ───────────▶ Actuation         (shadow here)
//!   │        SHADOW-FIRST: a dry-run band ⇒ apply nothing, observe first
//!   │
//! Report  ─ ReconcileOutcome{ report, Requeue(interval) }     (never Done-forever)
//! ```
//!
//! Every band starts `dry_run` (breathe's shadow gate), so the *shipped* tick is
//! shadow by construction — it computes the full plan and mutates nothing. That
//! is why this is a legitimate SKELETON and not a competing live controller.
//!
//! [`autorevivy`]: https://github.com/pleme-io/autorevivy

use core::fmt;
use std::time::Duration;

use async_trait::async_trait;

use crate::freshness::{self, AlwaysWarmTuning, MaintenancePlan};
use crate::memory::{
    self, EfficiencyReading, EntryStat, MemoryAuctionPlan, MemoryForecast, PrecarveRequest,
    PresentationInput, SpotCandidate, TuneProposal,
};
use crate::presentation::{
    self, Presentation, PresentationBudget, PresentationDecision, TuneRun,
};
use crate::{preheat, Arch, SuperCacheCiConfig};

/// The default tick interval — how often the coordinator re-optimizes the whole
/// in-memory cache. `Requeue(this)` keeps the loop live; a controller never
/// terminates as `Done`-forever (the cache is a continuously-maintained thing).
pub const DEFAULT_TICK_SECS: u64 = 30;

// ───────────────────────────────────────────────────────────────────────────
// Tier marker — the honest self-description of what this controller IS
// ───────────────────────────────────────────────────────────────────────────

/// The controller's tier — a typed, testable statement of how far the loop is
/// realized. The shipped value is [`ShadowSkeleton`](ControllerTier::ShadowSkeleton):
/// the brain composes all six cores and the tick runs end-to-end, but the Act
/// beat is shadow (mutates nothing) until the LiveTODO actuator binds to
/// autorevivy's coordinator + the real backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerTier {
    /// The pure brain + the reconcile contract ship; Act is shadow (this crate).
    ShadowSkeleton,
    /// The Act beat drives real actuators via autorevivy's coordinator (LiveTODO).
    Live,
}

/// The shipped tier of [`SuperCacheCiController`]. Asserted by the honest gate;
/// bumping it to [`Live`](ControllerTier::Live) without a live actuator is a
/// build-failing round-up.
pub const CONTROLLER_TIER: ControllerTier = ControllerTier::ShadowSkeleton;

/// The `(def…)` authoring keywords the controller surface names itself under —
/// the vocabulary bridge (the Rust + Lisp primary pattern). The
/// `#[derive(DeriveTataraDomain)]` attach stays gated on the sui-workspace
/// `tatara-lisp` lib/derive pin skew (same one that keeps the derive off
/// [`SuperCacheCiConfig`]); naming the keyword without a compiling derive is the
/// honest tier.
pub const CONTROLLER_AUTHORING_KEYWORDS: [&str; 2] = [
    "defcachecontroller", // SuperCacheCiController / the reconcile contract
    "defsupercachetick",  // TickPlan / one Observe→Decide→Act tick
];

// ───────────────────────────────────────────────────────────────────────────
// engenho Controller contract — MIRRORED (shape-identical), never forked-in-spirit
// ───────────────────────────────────────────────────────────────────────────

/// A typed reconcile error at the Observe/Act boundary.
#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    /// The Observe beat failed to read live signals.
    #[error("observe failed: {0}")]
    Observe(String),
    /// The Act beat failed to actuate the decided plan.
    #[error("actuate failed: {0}")]
    Actuate(String),
}

/// One reconciliation attempt's requeue decision — **shape-identical to
/// `engenho_controllers::ReconcileResult`**. `Done` is the default; the cache
/// coordinator always returns [`Requeue`](ReconcileResult::Requeue) (it is a
/// continuously-maintained thing, never one-shot-done).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReconcileResult {
    /// Succeeded; no follow-up scheduled.
    #[default]
    Done,
    /// Re-tick after `delay`.
    Requeue(Duration),
    /// Like `Requeue`, but signalling progress-with-work-left.
    RequeueWithProgress(Duration),
}

impl ReconcileResult {
    /// The follow-up delay this result asks for, if any.
    #[must_use]
    pub fn requeue_after(&self) -> Option<Duration> {
        match self {
            Self::Done => None,
            Self::Requeue(d) | Self::RequeueWithProgress(d) => Some(*d),
        }
    }
}

/// One tick's report — **shape-identical to `engenho_controllers::ReconcileReport`**.
#[derive(Debug, Default, Clone)]
pub struct ReconcileReport {
    /// Decisions the tick considered (the plan's size).
    pub objects_examined: usize,
    /// Decisions actually actuated (0 in a shadow tick).
    pub objects_changed: usize,
    /// Decisions withheld (shadow-first: examined − changed).
    pub objects_skipped: usize,
    /// Typed one-line summary for logs.
    pub note: Option<String>,
}

/// Outcome of one reconcile tick — report + requeue decision. Shape-identical to
/// `engenho_controllers::ReconcileOutcome`.
#[derive(Debug, Default, Clone)]
pub struct ReconcileOutcome {
    /// The tick report.
    pub report: ReconcileReport,
    /// The requeue decision.
    pub result: ReconcileResult,
}

impl ReconcileOutcome {
    /// Build an outcome from a report + an explicit requeue decision.
    #[must_use]
    pub fn new(report: ReconcileReport, result: ReconcileResult) -> Self {
        Self { report, result }
    }
}

impl From<ReconcileReport> for ReconcileOutcome {
    fn from(report: ReconcileReport) -> Self {
        Self {
            report,
            result: ReconcileResult::Done,
        }
    }
}

/// The reconcile-loop contract — **shape-identical to
/// `engenho_controllers::Controller`** (`name` + async `tick`). See the module
/// docs (`engenho-bind` LiveTODO) for why the trait is mirrored here rather than
/// depended-on, and the named destination (a shared leaf crate) that retires the
/// mirror.
#[async_trait]
pub trait Controller: Send + Sync {
    /// Stable name for telemetry + registration.
    fn name(&self) -> &'static str;
    /// One reconcile tick: Observe → Decide → Act → Report.
    async fn tick(&self) -> Result<ReconcileOutcome, ControllerError>;
}

// ───────────────────────────────────────────────────────────────────────────
// The Observe/Act seam — the pleme-io default delivery method
// ───────────────────────────────────────────────────────────────────────────

/// What the Observe beat produces — the pure snapshot the tick decides over.
/// Keeping it a plain value makes [`decide`] testable with a hand-built
/// observation and no cluster (pure brain; side effects behind
/// [`CacheEnvironment`]).
#[derive(Debug, Clone, Default)]
pub struct CacheObservation {
    /// Hot-cache occupancy, percent of the band ceiling (drives evict pressure).
    pub occupancy_pct: u8,
    /// The resident cache entries + their recency/reference stats (element 4).
    pub entries: Vec<EntryStat>,
    /// Memory forecasts for the derivations about to build (element 1).
    pub forecasts: Vec<MemoryForecast>,
    /// The live spot-market candidates the auction ranks (element 2).
    pub spot_candidates: Vec<SpotCandidate>,
    /// The build-DAG frontier's presentation inputs (element 5).
    pub presentation_inputs: Vec<PresentationInput>,
    /// The current efficiency reading (element 6 baseline); `None` ⇒ no telemetry
    /// yet ⇒ no tune this tick (honest: never invent a reading).
    pub efficiency: Option<EfficiencyReading>,
    /// Shadow-measured tune proposals to fold (element 6).
    pub tune_proposals: Vec<TuneProposal>,
    /// The measured warm-state of each perpetual-warming target (element 8).
    /// **LiveTODO(observe-feed):** L1/L2 presence (sui Redis + Postgres), the
    /// tracked-input hashes (git), the last-warm timestamps. Empty ⇒ every
    /// target is treated cold (never invents warmth it did not measure).
    pub preheat_targets: Vec<preheat::TargetObservation>,
}

/// What the Act beat applied. **Shadow-first:** a shadow tick returns
/// [`Actuation::shadow`] — nothing applied. The counts are non-zero only once
/// the live actuator (LiveTODO) drives the real backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Actuation {
    /// breathe carves applied (element 1).
    pub carves_applied: usize,
    /// cache warm/evict/GC ops applied (element 4).
    pub maintenance_applied: usize,
    /// lapidar tune knobs applied (element 6).
    pub tunes_applied: usize,
    /// Whether this was a shadow (observe-only) actuation.
    pub shadow: bool,
}

impl Actuation {
    /// The shadow actuation — nothing applied, observe-first.
    #[must_use]
    pub fn shadow() -> Self {
        Self {
            shadow: true,
            ..Default::default()
        }
    }

    /// Total decisions actually actuated.
    #[must_use]
    pub fn applied_total(&self) -> usize {
        self.carves_applied + self.maintenance_applied + self.tunes_applied
    }
}

/// The Observe/Act boundary — the pleme-io default delivery method (pure brain
/// in [`decide`]; every side effect behind ONE injectable trait, mocked in
/// tests). BOTH real implementations — the Observe feed AND the Act actuator —
/// are the LiveTODO; the only impl shipped here is [`ShadowEnvironment`].
#[async_trait]
pub trait CacheEnvironment: Send + Sync {
    /// The Observe beat — read live cache/market/telemetry signals.
    /// **LiveTODO(observe-feed):** breathe MCP + escuta CDC + build-DAG frontier
    /// + tameshi receipts + spot market + lapidar telemetry.
    async fn observe(&self) -> Result<CacheObservation, ControllerError>;

    /// The Act beat — apply the decided plan. **SHADOW-FIRST:** when the plan is
    /// shadow (any band `dry_run`), apply nothing and return
    /// [`Actuation::shadow`]. **LiveTODO(actuate):** the live impl drives breathe
    /// carve / breathe-auction / sui-cache warm·evict·GC / lapidar tune via
    /// autorevivy's coordinator — never a forked controller.
    async fn actuate(&self, plan: &TickPlan) -> Result<Actuation, ControllerError>;
}

/// The shipped [`CacheEnvironment`] — **observe a supplied snapshot, actuate
/// nothing**. Never-touch-disk, no cluster, deterministic. This is the honest
/// skeleton's Act beat: the tick runs end-to-end and the plan is fully computed,
/// but the world is not mutated. Real actuation is `LiveTODO(actuate)`.
#[derive(Debug, Clone, Default)]
pub struct ShadowEnvironment {
    /// The observation the tick will decide over.
    pub observation: CacheObservation,
}

impl ShadowEnvironment {
    /// Wrap a hand-built (or test) observation.
    #[must_use]
    pub fn new(observation: CacheObservation) -> Self {
        Self { observation }
    }
}

#[async_trait]
impl CacheEnvironment for ShadowEnvironment {
    async fn observe(&self) -> Result<CacheObservation, ControllerError> {
        Ok(self.observation.clone())
    }

    async fn actuate(&self, _plan: &TickPlan) -> Result<Actuation, ControllerError> {
        // Shadow-first by construction: the skeleton applies nothing.
        Ok(Actuation::shadow())
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The plan — the six cores composed into one coherent per-tick decision
// ───────────────────────────────────────────────────────────────────────────

/// One architecture's memory-tuned auction plan (element 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchAuction {
    /// The runner architecture this plan is for.
    pub arch: Arch,
    /// The memory-ranked 100%-spot bid plan breathe's leilão consumes.
    pub plan: MemoryAuctionPlan,
}

/// The full per-tick plan — **the controller's brain**: all six built decision
/// cores folded into one coherent decision. Pure + total (produced by [`decide`]
/// from a config + observation with no I/O).
#[derive(Debug, Clone)]
pub struct TickPlan {
    /// Element 1 — pre-carve requests (predict → shadow → confirm → allocate).
    pub precarves: Vec<PrecarveRequest>,
    /// Element 2 — memory-tuned 100%-spot auction plans, per arch.
    pub auctions: Vec<ArchAuction>,
    /// Element 3 — always-warm Redis/Postgres tuning.
    pub tuning: AlwaysWarmTuning,
    /// Element 4 — the evict/warm/GC maintenance plan.
    pub maintenance: MaintenancePlan,
    /// Element 5 — the optimal in-memory (RAMDISK) presentation.
    pub presentation: Presentation,
    /// Element 5 — the guard's verdict on that presentation (must be servable).
    pub presentation_ok: PresentationDecision,
    /// Element 6 — the continuous self-tune over this tick's proposals.
    pub self_tune: TuneRun,
    /// Element 8 — the perpetual cache-warming plan (WHEN/WHICH to warm + the
    /// spin-the-floor-only-while-warming builder-floor plan). Keeps the cache
    /// HOT so a build substitutes warm instead of cold-compiling.
    pub preheat: preheat::PreheatPlan,
    /// Shadow-first: any band `dry_run` ⇒ the tick is shadow (Act applies nothing).
    pub shadow: bool,
    /// Never-stuck signals — pre-carve/auction escalations off spot/over-ceiling.
    pub escalations: u32,
}

impl TickPlan {
    /// The count of decisions this tick considered — the report's
    /// `objects_examined`. Sums the six cores' produced items.
    #[must_use]
    pub fn decision_count(&self) -> usize {
        self.precarves.len()
            + self.auctions.len()
            + self.maintenance.promote.len()
            + self.maintenance.demote.len()
            + self.maintenance.collect.len()
            + self.presentation.l1.len()
            + self.presentation.l2.len()
            + self.presentation.l3.len()
            + self.presentation.cold.len()
            + self.self_tune.considered as usize
            + self.preheat.decision_count()
    }
}

/// A typed one-line tick summary — the sanctioned typed-emission surface for the
/// report note (`write!` inside `Display`, never `format!()`).
struct TickSummary<'a> {
    plan: &'a TickPlan,
    act: &'a Actuation,
}

impl fmt::Display for TickSummary<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} precarve={} auction={} promote={} evict={} gc={} l1={} tune_accepted={} preheat_warm={} preheat_pct={} escalations={} applied={}",
            if self.plan.shadow { "shadow" } else { "live" },
            self.plan.precarves.len(),
            self.plan.auctions.len(),
            self.plan.maintenance.promote.len(),
            self.plan.maintenance.demote.len(),
            self.plan.maintenance.collect.len(),
            self.plan.presentation.l1.len(),
            self.plan.self_tune.accepted.len(),
            self.plan.preheat.to_warm,
            self.plan.preheat.warm_fraction_pct,
            self.plan.escalations,
            self.act.applied_total(),
        )
    }
}

// ───────────────────────────────────────────────────────────────────────────
// decide — the pure Classify/Decide brain (BUILT, tested, no I/O)
// ───────────────────────────────────────────────────────────────────────────

/// **The pure brain.** Fold all six built decision cores over a config +
/// observation into one coherent [`TickPlan`]. Side-effect-free + total: any
/// observation yields a plan. This is the shipped heart of element 7 — the
/// controller's Classify/Decide beats; the Observe/Act loop is
/// [`Controller::tick`] over a [`CacheEnvironment`].
#[must_use]
pub fn decide(cfg: &SuperCacheCiConfig, obs: &CacheObservation) -> TickPlan {
    // Element 1 — pre-carve each forecast onto the cache band ahead of the build.
    let mut escalations = 0u32;
    let precarves: Vec<PrecarveRequest> = obs
        .forecasts
        .iter()
        .map(|fc| {
            let req =
                memory::precarve_request(fc, &cfg.cache.memory_band, &cfg.breathe.band_ref);
            if req.escalate {
                escalations += 1;
            }
            req
        })
        .collect();

    // Element 2 — the memory-tuned 100%-spot auction, per configured arch.
    let auctions: Vec<ArchAuction> = cfg
        .builders
        .arches
        .iter()
        .map(|a| {
            let plan = memory::plan_memory_auction(&a.memory_auction, &obs.spot_candidates);
            if plan.needs_escalation {
                escalations += 1;
            }
            ArchAuction {
                arch: a.arch,
                plan,
            }
        })
        .collect();

    // Element 3 — always-warm Redis/Postgres tuning derived from the config.
    let tuning = freshness::derive_always_warm_tuning(cfg);

    // Element 4 — the evict/warm/GC maintenance plan over the resident entries.
    let maintenance =
        freshness::plan_maintenance(&obs.entries, &cfg.cache.memory_band, obs.occupancy_pct);

    // Element 5 — the optimal RAMDISK presentation within the L1 budget + guard.
    let budget = PresentationBudget::of(cfg.cache.memory_band.max_mib);
    let (presentation, presentation_ok) =
        presentation::organize_and_validate(&obs.presentation_inputs, budget);

    // Element 6 — the continuous self-tune. No telemetry ⇒ neutral baseline +
    // no proposals ⇒ an empty, non-regressing run (honest: never invent a gain).
    let baseline = obs.efficiency.unwrap_or(EfficiencyReading {
        hit_rate_pct: 0,
        build_p50_ms: 0,
        ondemand_secs: 0,
        ram_waste_mib: 0,
    });
    let self_tune = presentation::run_self_tune(baseline, &obs.tune_proposals);

    // Element 8 — the perpetual cache-warming plan: WHEN/WHICH to re-warm + the
    // spin-the-floor-only-while-warming builder-floor plan (its own dry_run gate).
    let preheat = preheat::plan_preheat(&cfg.preheat, &obs.preheat_targets);
    if preheat.floors.iter().any(|f| f.reason == preheat::FloorReason::WarmingRaise) {
        // A warm that must raise the 100%-spot floor off scale-to-zero is a
        // never-stuck escalation signal (the fleet must wake to warm).
        escalations += u32::try_from(
            preheat
                .floors
                .iter()
                .filter(|f| f.reason == preheat::FloorReason::WarmingRaise)
                .count(),
        )
        .unwrap_or(u32::MAX);
    }

    // Shadow-first: any band in dry-run — including the preheat band — makes the
    // whole tick shadow.
    let shadow =
        cfg.cache.memory_band.dry_run || cfg.sandbox.tmpfs_band.dry_run || cfg.preheat.dry_run;

    TickPlan {
        precarves,
        auctions,
        tuning,
        maintenance,
        presentation,
        presentation_ok,
        self_tune,
        preheat,
        shadow,
        escalations,
    }
}

/// Build the tick report from the decided plan + what the Act beat applied.
fn report_of(plan: &TickPlan, act: &Actuation) -> ReconcileReport {
    let examined = plan.decision_count();
    let changed = act.applied_total();
    ReconcileReport {
        objects_examined: examined,
        objects_changed: changed,
        objects_skipped: examined.saturating_sub(changed),
        note: Some(TickSummary { plan, act }.to_string()),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The controller — Observe → Decide → Act, over the injected environment
// ───────────────────────────────────────────────────────────────────────────

/// The `/super-cache-ci` reconcile controller — **element 7**. Holds the typed
/// config + an injected [`CacheEnvironment`]; each [`tick`](Controller::tick)
/// observes, decides (the six cores), acts (shadow in the skeleton), and
/// requeues. The pure brain is [`decide`]; the actuator is the environment.
#[derive(Debug, Clone)]
pub struct SuperCacheCiController<E: CacheEnvironment> {
    cfg: SuperCacheCiConfig,
    env: E,
    requeue: Duration,
}

impl<E: CacheEnvironment> SuperCacheCiController<E> {
    /// Build a controller from a config + an environment (the default tick
    /// interval).
    #[must_use]
    pub fn new(cfg: SuperCacheCiConfig, env: E) -> Self {
        Self {
            cfg,
            env,
            requeue: Duration::from_secs(DEFAULT_TICK_SECS),
        }
    }

    /// Override the requeue interval.
    #[must_use]
    pub fn with_requeue(mut self, interval: Duration) -> Self {
        self.requeue = interval;
        self
    }

    /// The typed config this controller reconciles toward.
    #[must_use]
    pub fn config(&self) -> &SuperCacheCiConfig {
        &self.cfg
    }

    /// The tier this controller ships at — always
    /// [`ControllerTier::ShadowSkeleton`] here.
    #[must_use]
    pub fn tier(&self) -> ControllerTier {
        CONTROLLER_TIER
    }

    /// Run the pure brain directly against an observation (no I/O) — the
    /// testable Classify/Decide beat.
    #[must_use]
    pub fn decide(&self, obs: &CacheObservation) -> TickPlan {
        decide(&self.cfg, obs)
    }
}

#[async_trait]
impl<E: CacheEnvironment> Controller for SuperCacheCiController<E> {
    fn name(&self) -> &'static str {
        "super-cache-ci"
    }

    async fn tick(&self) -> Result<ReconcileOutcome, ControllerError> {
        // Observe — LiveTODO(observe-feed); shipped path reads the supplied snapshot.
        let obs = self.env.observe().await?;
        // Decide — the pure brain, all six cores (BUILT).
        let plan = decide(&self.cfg, &obs);
        // Act — shadow in the skeleton; LiveTODO(actuate) drives real backends.
        let act = self.env.actuate(&plan).await?;
        // Report + requeue — a cache coordinator is never one-shot-Done.
        Ok(ReconcileOutcome::new(
            report_of(&plan, &act),
            ReconcileResult::Requeue(self.requeue),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{CacheTier as CT, ForecastBasis};
    use shikumi::TieredConfig;

    fn obs_with_work() -> CacheObservation {
        CacheObservation {
            occupancy_pct: 95, // over the 80% setpoint ⇒ evict pressure
            entries: vec![
                // referenced + hot + recent ⇒ Warm
                EntryStat {
                    key: "hot".into(),
                    size_mib: 64,
                    hits: 9,
                    secs_since_use: 5,
                    referenced: true,
                },
                // unreferenced + cold ⇒ Gc
                EntryStat {
                    key: "dead".into(),
                    size_mib: 128,
                    hits: 0,
                    secs_since_use: 99_999,
                    referenced: false,
                },
            ],
            forecasts: vec![MemoryForecast::cold_start("drv-a", 2048)],
            spot_candidates: vec![
                SpotCandidate {
                    family: "r6i".into(),
                    mib: 32_768,
                    vcpu: 8,
                    price_milli: 900,
                    interrupt_rate: 5,
                },
                SpotCandidate {
                    family: "c7i".into(),
                    mib: 8_192,
                    vcpu: 8,
                    price_milli: 800,
                    interrupt_rate: 5,
                },
            ],
            presentation_inputs: vec![PresentationInput {
                key: "soon-small".into(),
                predicted_next_use_secs: 5,
                size_mib: 64,
                tier: CT::L2Pg,
            }],
            efficiency: Some(EfficiencyReading {
                hit_rate_pct: 60,
                build_p50_ms: 5000,
                ondemand_secs: 10,
                ram_waste_mib: 200,
            }),
            tune_proposals: Vec::new(),
            // Perpetual-warming observe-feed (empty here — the prescribed config
            // has no targets; a dedicated test exercises a populated set).
            preheat_targets: Vec::new(),
        }
    }

    #[test]
    fn decide_composes_all_six_cores() {
        let cfg = SuperCacheCiConfig::prescribed_default();
        let plan = decide(&cfg, &obs_with_work());
        // 1 — a forecast produced a precarve request.
        assert_eq!(plan.precarves.len(), 1);
        // 2 — one auction plan per configured arch (amd64 + arm64).
        assert_eq!(plan.auctions.len(), cfg.builders.arches.len());
        // 3 — tuning derived (Redis maxmemory == the band ceiling).
        assert_eq!(plan.tuning.redis.maxmemory_mib, cfg.cache.memory_band.max_mib);
        // 4 — the cold+unreferenced entry is a GC candidate; the hot one a root.
        assert!(plan.maintenance.collect.contains(&"dead".to_string()));
        assert!(plan.maintenance.roots.contains(&"hot".to_string()));
        // 5 — the soon+small input is warmed into L1 and the guard is servable.
        assert!(plan.presentation_ok.is_servable());
        // 6 — a self-tune ran (0 proposals ⇒ empty but present + non-regressing).
        assert!(plan.self_tune.is_non_regressing());
    }

    #[test]
    fn prescribed_tick_is_shadow_first() {
        // Every prescribed band starts dry_run ⇒ the whole tick is shadow.
        let cfg = SuperCacheCiConfig::prescribed_default();
        let plan = decide(&cfg, &obs_with_work());
        assert!(plan.shadow, "prescribed bands are dry_run ⇒ shadow-first");
    }

    #[test]
    fn decide_is_total_on_the_empty_observation() {
        // A total function: even a blank observation yields a coherent plan.
        let cfg = SuperCacheCiConfig::prescribed_default();
        let plan = decide(&cfg, &CacheObservation::default());
        assert!(plan.precarves.is_empty());
        assert_eq!(plan.auctions.len(), cfg.builders.arches.len());
        assert!(plan.maintenance.collect.is_empty());
        assert!(plan.self_tune.is_non_regressing());
    }

    #[tokio::test]
    async fn tick_over_shadow_env_actuates_nothing_but_examines_the_plan() {
        let cfg = SuperCacheCiConfig::prescribed_default();
        let ctl = SuperCacheCiController::new(cfg, ShadowEnvironment::new(obs_with_work()));
        let out = ctl.tick().await.expect("tick");
        // The skeleton's Act beat is shadow — nothing changed.
        assert_eq!(out.report.objects_changed, 0, "shadow skeleton mutates nothing");
        // But the plan was fully computed — decisions were examined.
        assert!(out.report.objects_examined > 0, "the tick considered the plan");
        assert_eq!(
            out.report.objects_skipped, out.report.objects_examined,
            "shadow-first: every considered decision is withheld"
        );
        // A cache coordinator never terminates Done — it requeues.
        assert_eq!(
            out.result,
            ReconcileResult::Requeue(Duration::from_secs(DEFAULT_TICK_SECS))
        );
        assert!(out.result.requeue_after().is_some());
        // The note is the typed summary (shadow, applied=0).
        let note = out.report.note.unwrap();
        assert!(note.starts_with("shadow "), "note: {note}");
        assert!(note.ends_with("applied=0"), "note: {note}");
    }

    #[tokio::test]
    async fn controller_name_is_stable() {
        let ctl = SuperCacheCiController::new(
            SuperCacheCiConfig::prescribed_default(),
            ShadowEnvironment::default(),
        );
        assert_eq!(ctl.name(), "super-cache-ci");
        assert_eq!(ctl.tier(), ControllerTier::ShadowSkeleton);
    }

    #[test]
    fn requeue_result_semantics_match_engenho() {
        assert_eq!(ReconcileResult::default(), ReconcileResult::Done);
        assert_eq!(ReconcileResult::Done.requeue_after(), None);
        let d = Duration::from_secs(5);
        assert_eq!(ReconcileResult::Requeue(d).requeue_after(), Some(d));
        assert_eq!(
            ReconcileResult::RequeueWithProgress(d).requeue_after(),
            Some(d)
        );
        // From<report> defaults to Done (the engenho legacy-site shape).
        let out: ReconcileOutcome = ReconcileReport::default().into();
        assert_eq!(out.result, ReconcileResult::Done);
    }

    #[test]
    fn escalation_never_wedges_it_signals() {
        // A forecast that overflows the band ceiling ⇒ a never-stuck escalation,
        // surfaced in the plan (not a wedge, not a silent drop).
        let cfg = SuperCacheCiConfig::prescribed_default();
        let mut o = CacheObservation::default();
        o.forecasts = vec![MemoryForecast {
            deriv_key: "huge".into(),
            predicted_peak_mib: 999_999,
            predicted_tail_mib: 999_999, // >> the 8192 cache-band ceiling
            basis: ForecastBasis::ColdStart,
        }];
        let plan = decide(&cfg, &o);
        assert!(plan.escalations >= 1, "over-ceiling ⇒ escalate, never wedge");
        assert!(plan.precarves[0].escalate);
    }

    // ── the honest gate ─────────────────────────────────────────────────────

    #[test]
    fn decide_folds_perpetual_warming_plan() {
        // A config with two warm targets: one cold (warms → floor raised) + one
        // fresh (already warm → arch idle). decide() must fold the preheat plan.
        let mut cfg = SuperCacheCiConfig::prescribed_default();
        cfg.preheat.targets = vec![
            preheat::WarmTarget::flake("auth", Arch::Amd64),
            preheat::WarmTarget::flake("gw", Arch::Arm64),
        ];
        let mut obs = obs_with_work();
        obs.preheat_targets = vec![
            preheat::TargetObservation {
                name: "auth".into(),
                present_l1: false,
                present_l2: false,
                current_input_hash: "h1".into(),
                warmed_input_hash: String::new(),
                secs_since_warm: 0,
            },
            preheat::TargetObservation {
                name: "gw".into(),
                present_l1: true,
                present_l2: true,
                current_input_hash: "h1".into(),
                warmed_input_hash: "h1".into(),
                secs_since_warm: 60,
            },
        ];
        let plan = decide(&cfg, &obs);
        assert_eq!(plan.preheat.to_warm, 1, "the cold target warms");
        assert_eq!(plan.preheat.already_warm, 1, "the fresh target stays warm");
        // The amd64 floor is raised (auth warming); the arm64 floor stays 0.
        let amd = plan
            .preheat
            .floors
            .iter()
            .find(|f| f.arch == Arch::Amd64)
            .unwrap();
        assert_eq!(amd.reason, preheat::FloorReason::WarmingRaise);
        // Shadow-first: the prescribed preheat band is dry-run ⇒ the tick is shadow.
        assert!(plan.shadow);
        // The preheat decisions are counted in the tick's examined set.
        assert!(plan.decision_count() >= plan.preheat.decision_count());
        assert!(plan.preheat.decision_count() > 0);
    }

    #[test]
    fn honest_gate_controller_is_a_shadow_skeleton_not_a_live_controller() {
        // The shipped tier is ShadowSkeleton. Bumping CONTROLLER_TIER to Live
        // without a live actuator fails HERE — the loop is not rounded up.
        assert_eq!(CONTROLLER_TIER, ControllerTier::ShadowSkeleton);
    }

    #[test]
    fn honest_gate_every_element_loop_is_still_a_livetodo() {
        // element 7's ledger must still say every Observe/Act loop is a LiveTODO
        // that composes a shipped primitive — this SKELETON does not flip any of
        // them to shipped-loop. If a real loop lands, its ledger row flips AND
        // this assertion is updated in the SAME commit (never silently).
        for e in &memory::CONTROLLER_ELEMENTS {
            assert!(e.pure_core_shipped, "{}: pure core ships", e.element);
            assert_eq!(
                e.loop_tier,
                memory::LoopTier::DesignLiveTodo,
                "{}: the Observe/Act loop is a LiveTODO (autorevivy's coordinator)",
                e.element
            );
        }
    }

    #[tokio::test]
    async fn honest_gate_shadow_env_applies_nothing() {
        // The shipped CacheEnvironment MUST actuate nothing — the never-touch-disk,
        // no-cluster, shadow-first guarantee. A real actuator is LiveTODO(actuate).
        let env = ShadowEnvironment::new(obs_with_work());
        let cfg = SuperCacheCiConfig::prescribed_default();
        let plan = decide(&cfg, &env.observation);
        let act = env.actuate(&plan).await.expect("actuate");
        assert!(act.shadow);
        assert_eq!(act.applied_total(), 0, "shadow env mutates nothing");
    }

    #[test]
    fn controller_authoring_keywords_are_unique_and_prefixed() {
        for k in CONTROLLER_AUTHORING_KEYWORDS {
            assert!(k.starts_with("def"), "{k} must be a (def…) form");
        }
        assert_ne!(
            CONTROLLER_AUTHORING_KEYWORDS[0],
            CONTROLLER_AUTHORING_KEYWORDS[1]
        );
    }
}
