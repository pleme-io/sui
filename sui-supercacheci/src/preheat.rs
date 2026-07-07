//! `preheat` — the **perpetual cache-warming** decision core of
//! `/super-cache-ci`: the pure brain that keeps the sui super-cache **hot** so a
//! build **substitutes pre-built closures** (warm, ≈ seconds) instead of
//! cold-compiling (≈ minutes). It answers three questions, tick after tick,
//! forever:
//!
//! 1. **WHEN to re-warm** — [`classify_target`] maps each target's observed
//!    state to a [`WarmTrigger`] ([`ColdStart`](WarmTrigger::ColdStart) /
//!    [`InputChanged`](WarmTrigger::InputChanged) /
//!    [`Cadence`](WarmTrigger::Cadence)) or [`WarmAction::AlreadyWarm`].
//! 2. **WHICH closures** — one [`WarmTarget`] per tracked dep/service; a target
//!    is warm iff the cache holds the closure for its **current** tracked-input
//!    hash (flake.lock / Cargo.lock / go.mod / …).
//! 3. **HOW to spin the fleet** — [`plan_floor`] raises the 100%-spot
//!    scale-to-zero builder floor **only while warming**, then drops it back to
//!    the idle floor (0). Cost at rest is zero; the cache is kept current
//!    between real builds.
//!
//! It also carries the Viggy `(defpromessa)` **"the cache stays warm"** as a
//! typed value — [`WarmthPromessa`] — whose [`evaluate`](WarmthPromessa::evaluate)
//! turns the plan + observation into a [`WarmthVerdict`]
//! ([`Held`](WarmthVerdict::Held) / [`Breached`](WarmthVerdict::Breached)).
//!
//! ## Tier-honest (never round up)
//!
//! - **Shipped (this module):** the pure classify / plan / floor / promessa
//!   functions + their typed borders + exhaustive unit tests. These ARE the
//!   perpetual-warming controller's Classify/Decide beats and are correct-by-
//!   test with **no I/O** — a hand-built [`TargetObservation`] in, a
//!   [`PreheatPlan`] out.
//! - **SHADOW-first by construction:** [`PreheatCfg::dry_run`] ⇒
//!   [`PreheatPlan::shadow`] ⇒ the Act beat applies nothing. The plan is fully
//!   computed and *observed*; the fleet is not spun and the cache is not
//!   mutated until the operator flips the band LIVE (breathe's shadow gate).
//! - **LiveTODO(loop):** the coordinator that *ticks* this plan tick-by-tick is
//!   [`autorevivy`]'s CLEAN face (`superCacheCiRef`) — design-stage; the running
//!   interim actuator is the `akeyless-nix-images` `camelot-cache-warm`
//!   scheduled workflow (6 h cadence + on tracked-input change). This module
//!   ships the **brain both derive from**, never a second controller.
//! - **LiveTODO(observe-feed):** the real Observe beat reads L1/L2 presence
//!   (sui Redis + Postgres), the tracked-input hashes (git), and the last-warm
//!   timestamps. The shipped path takes a hand-built observation — so the tick
//!   never *invents* warmth it did not measure.
//! - **LiveTODO(lisp):** each surface names its `(def…)` keyword
//!   ([`PREHEAT_AUTHORING_KEYWORDS`]); the `#[derive(DeriveTataraDomain)]`
//!   attach is gated on the same sui-workspace `tatara-lisp` pin skew that keeps
//!   the derive off [`SuperCacheCiConfig`](crate::SuperCacheCiConfig).
//!
//! [`autorevivy`]: https://github.com/pleme-io/autorevivy

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::Arch;

/// The `(def…)` authoring keywords the perpetual-warming surface names itself
/// under — the vocabulary bridge (the Rust + Lisp primary pattern). The derive
/// attach stays gated on the workspace `tatara-lisp` pin skew; naming the
/// keyword without a compiling derive is the honest tier.
pub const PREHEAT_AUTHORING_KEYWORDS: [&str; 4] = [
    "defpreheatcfg",      // PreheatCfg / the perpetual-warming posture
    "defwarmtarget",      // WarmTarget / one dep-or-service warm unit
    "deffloorspin",       // FloorSpinCfg / spin-the-floor-only-while-warming
    "defwarmthpromessa",  // WarmthPromessa / the (defpromessa) "cache stays warm"
];

/// The tier this warming core ships at — a typed, testable self-description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreheatTier {
    /// The pure Classify/Decide brain ships + is tested; the Act beat is shadow
    /// (spins nothing, mutates nothing) until the LiveTODO loop binds it.
    ShadowCore,
    /// The plan is ticked + actuated by a live coordinator (LiveTODO).
    Live,
}

/// The shipped tier of the perpetual-warming core. Bumping this to
/// [`Live`](PreheatTier::Live) without a live coordinator is a build-failing
/// round-up (asserted by the honest gate).
pub const PREHEAT_TIER: PreheatTier = PreheatTier::ShadowCore;

// ───────────────────────────────────────────────────────────────────────────
// Tracked inputs — the content that determines which closure must be warm
// ───────────────────────────────────────────────────────────────────────────

/// A kind of tracked build input. A change to any tracked input's content hash
/// changes the target's build closure ⇒ the cache must re-warm for the new
/// closure.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TrackedInputKind {
    /// `flake.lock` — the Nix flake input pins (the fleet's primary closure key).
    FlakeLock,
    /// `Cargo.lock` — the Rust dependency graph.
    CargoLock,
    /// `go.mod` — the Go module requirements.
    GoMod,
    /// `go.sum` — the Go module checksums.
    GoSum,
    /// `package-lock.json` / `pnpm-lock.yaml` — the npm dependency graph.
    PackageLock,
    /// `requirements.txt` / `poetry.lock` — the Python dependency graph.
    Requirements,
    /// Any other tracked input a target declares.
    Other,
}

impl fmt::Display for TrackedInputKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Typed emission — the render surface for the input kind (no format!()).
        let s = match self {
            Self::FlakeLock => "flake.lock",
            Self::CargoLock => "Cargo.lock",
            Self::GoMod => "go.mod",
            Self::GoSum => "go.sum",
            Self::PackageLock => "package-lock.json",
            Self::Requirements => "requirements.txt",
            Self::Other => "other",
        };
        f.write_str(s)
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Config — the perpetual-warming posture
// ───────────────────────────────────────────────────────────────────────────

/// One warm target — a dep or service whose CURRENT build closure the cache
/// must hold warm. `inputs` is the set of tracked inputs whose hash drives the
/// closure; a change to any of them re-warms the target.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WarmTarget {
    /// The dep / service name (e.g. `"akeyless-auth"`).
    pub name: String,
    /// Which builder arch warms this target.
    pub arch: Arch,
    /// The tracked inputs whose content determines this target's closure. An
    /// **empty** set is honest: the target cannot be classified and is
    /// [`Skip`](WarmAction::Skip)ped rather than assumed warm.
    pub inputs: Vec<TrackedInputKind>,
}

impl WarmTarget {
    /// A target keyed on `flake.lock` (the fleet default closure key).
    #[must_use]
    pub fn flake(name: &str, arch: Arch) -> Self {
        Self {
            name: name.to_string(),
            arch,
            inputs: vec![TrackedInputKind::FlakeLock],
        }
    }
}

/// The floor-spin policy — the builder floor is raised **only while warming**,
/// then dropped to the idle floor. This is the "spin the 100%-spot
/// scale-to-zero builder floor ONLY while warming" rule made typed.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloorSpinCfg {
    /// Min runners to raise the floor to **while a warm is in flight** for that
    /// arch. Clamped to [`max_floor`](FloorSpinCfg::max_floor).
    pub warm_floor: u32,
    /// Min runners at rest — `0` = scale-to-zero (the camelot posture: cost at
    /// rest is zero).
    pub idle_floor: u32,
    /// The ceiling the floor never climbs past (the ARC `maxRunners` wall).
    pub max_floor: u32,
}

impl FloorSpinCfg {
    /// The camelot default: raise to 1 while warming, 0 at rest, never past 8.
    #[must_use]
    pub fn camelot() -> Self {
        Self {
            warm_floor: 1,
            idle_floor: 0,
            max_floor: 8,
        }
    }

    /// The zeroed floor (no opinion) — the honest [`bare`](crate::SuperCacheCiConfig::bare)
    /// value.
    #[must_use]
    pub fn unset() -> Self {
        Self {
            warm_floor: 0,
            idle_floor: 0,
            max_floor: 0,
        }
    }
}

/// The perpetual cache-warming config — the typed posture the warming
/// controller reconciles toward. Authored as `(defpreheatcfg …)`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PreheatCfg {
    /// Master toggle. `false` ⇒ no warming plan is produced (the honest floor).
    pub enabled: bool,
    /// Re-warm at least this often even absent an input change (seconds). A
    /// target staler than this triggers [`WarmTrigger::Cadence`].
    pub cadence_secs: u32,
    /// The warm-fraction setpoint (percent of targets that must be warm) — the
    /// [`WarmthPromessa`] objective.
    pub warm_fraction_target_pct: u8,
    /// The targets whose current closure the cache holds warm.
    pub targets: Vec<WarmTarget>,
    /// The spin-the-floor-only-while-warming policy.
    pub floor_spin: FloorSpinCfg,
    /// SHADOW-first gate: `true` computes the plan and applies nothing (observe
    /// `ShadowWouldApply` before flipping LIVE — breathe's shadow gate).
    pub dry_run: bool,
}

impl PreheatCfg {
    /// The honest OFF floor — warming disabled, no targets, shadow.
    #[must_use]
    pub fn off() -> Self {
        Self {
            enabled: false,
            cadence_secs: 0,
            warm_fraction_target_pct: 0,
            targets: Vec::new(),
            floor_spin: FloorSpinCfg::unset(),
            dry_run: true,
        }
    }

    /// The prescribed camelot destination posture — enabled, 6 h cadence, a
    /// 99% warm-fraction objective, **shadow-first** (`dry_run = true`). Targets
    /// are left empty here rather than fabricate the microservice image names;
    /// the operator (or the chart values) supplies the real set — an empty
    /// target list yields an honest empty plan, never a claimed-warm cache.
    #[must_use]
    pub fn camelot() -> Self {
        Self {
            enabled: true,
            cadence_secs: 21_600, // 6 h — matches the camelot-cache-warm workflow cadence
            warm_fraction_target_pct: 99,
            targets: Vec::new(),
            floor_spin: FloorSpinCfg::camelot(),
            // breathe shadow-first: observe ShadowWouldApply before spinning the fleet.
            dry_run: true,
        }
    }

    /// The staleness ceiling this posture allows — the cadence plus a 1 h slack,
    /// the [`WarmthPromessa`] `max_staleness_secs`.
    #[must_use]
    pub fn max_staleness_secs(&self) -> u32 {
        self.cadence_secs.saturating_add(3_600)
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Observation — what the Observe beat measures per target (LiveTODO feed)
// ───────────────────────────────────────────────────────────────────────────

/// The measured state of one warm target at Observe time. Keeping it a plain
/// value makes [`classify_target`] testable with a hand-built observation and no
/// cluster. **The reference field is the hash comparison, not a guess:** a
/// target is warm iff the cache holds the closure for [`current_input_hash`] and
/// that equals the [`warmed_input_hash`] the cache was last warmed for.
///
/// [`current_input_hash`]: TargetObservation::current_input_hash
/// [`warmed_input_hash`]: TargetObservation::warmed_input_hash
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct TargetObservation {
    /// The target this observation is for (matches a [`WarmTarget::name`]).
    pub name: String,
    /// The closure is present in the L1 hot cache (sui Redis).
    pub present_l1: bool,
    /// The closure is present in the L2 durable store (sui Postgres).
    pub present_l2: bool,
    /// The content hash of the target's tracked inputs **now** (the closure key
    /// the build will resolve against). Empty ⇒ unknown ⇒ treated as cold.
    pub current_input_hash: String,
    /// The input hash the cache was **last warmed** for. Empty ⇒ never warmed.
    pub warmed_input_hash: String,
    /// Seconds since the target was last warmed. Drives [`WarmTrigger::Cadence`].
    pub secs_since_warm: u32,
}

// ───────────────────────────────────────────────────────────────────────────
// Verdicts — WHEN / WHETHER to warm
// ───────────────────────────────────────────────────────────────────────────

/// Why a target needs re-warming. Never a silent guess — the basis is typed so
/// a consumer (and a reviewer) sees exactly which condition fired.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WarmTrigger {
    /// The closure is absent from the cache (L1 or L2), or never warmed — the
    /// load-bearing case a fresh camelot floor starts in.
    ColdStart,
    /// A tracked input changed: `current_input_hash != warmed_input_hash`. The
    /// cache holds a *stale* closure; the new one must be built + warmed.
    InputChanged,
    /// No input change, but the warm is older than the cadence — a freshness
    /// re-warm so a long-quiet target never drifts cold.
    Cadence,
}

impl fmt::Display for WarmTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::ColdStart => "cold-start",
            Self::InputChanged => "input-changed",
            Self::Cadence => "cadence",
        };
        f.write_str(s)
    }
}

/// The action decided for one target this tick.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WarmAction {
    /// Warm the target — build + populate the cache for the current closure.
    Warm,
    /// Already warm — the cache holds the current closure and it is fresh.
    AlreadyWarm,
    /// Skipped — the target declares no tracked inputs, so warmth cannot be
    /// decided. Honest: never assume a cache-hit for an unclassifiable target.
    Skip,
}

/// One target's decision — action + (when warming) the trigger that fired.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WarmDecision {
    /// The target name.
    pub target: String,
    /// The decided action.
    pub action: WarmAction,
    /// The trigger — `Some` iff `action == Warm`.
    pub trigger: Option<WarmTrigger>,
}

/// **The classify beat.** Map a target's config + observation to a decision.
/// Pure + total: any observation yields a decision.
///
/// The order is load-bearing: **cold beats input-changed beats cadence**. A
/// cold cache is warmed regardless of hashes (there is nothing to compare); a
/// present-but-stale closure re-warms on the input change; a present-and-current
/// closure re-warms only when the cadence lapses.
#[must_use]
pub fn classify_target(target: &WarmTarget, obs: &TargetObservation, cadence_secs: u32) -> WarmDecision {
    // No tracked inputs ⇒ unclassifiable ⇒ Skip (never assume warm).
    if target.inputs.is_empty() {
        return WarmDecision {
            target: target.name.clone(),
            action: WarmAction::Skip,
            trigger: None,
        };
    }

    let warm = |trigger: WarmTrigger| WarmDecision {
        target: target.name.clone(),
        action: WarmAction::Warm,
        trigger: Some(trigger),
    };

    // Cold: absent from either tier, or never warmed.
    let cold = !obs.present_l1 || !obs.present_l2 || obs.warmed_input_hash.is_empty();
    if cold {
        return warm(WarmTrigger::ColdStart);
    }

    // Stale closure: a tracked input changed since the last warm.
    if obs.current_input_hash != obs.warmed_input_hash {
        return warm(WarmTrigger::InputChanged);
    }

    // Freshness: current closure is present, but the warm is older than cadence.
    if obs.secs_since_warm >= cadence_secs {
        return warm(WarmTrigger::Cadence);
    }

    WarmDecision {
        target: target.name.clone(),
        action: WarmAction::AlreadyWarm,
        trigger: None,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Floor plan — spin the floor ONLY while warming
// ───────────────────────────────────────────────────────────────────────────

/// Why a per-arch floor is set the way it is.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FloorReason {
    /// A warm is in flight for this arch ⇒ raise the floor so a spot builder
    /// pends and the node group scales up to run it.
    WarmingRaise,
    /// Nothing warming for this arch ⇒ drop to the idle floor (scale-to-zero).
    IdleDrop,
}

/// One arch's builder-floor decision for this tick.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ArchFloor {
    /// The runner arch.
    pub arch: Arch,
    /// The `minRunners` this tick asks for (already clamped to `max_floor`).
    pub desired_floor: u32,
    /// Why.
    pub reason: FloorReason,
    /// How many targets on this arch are being warmed this tick.
    pub warming_count: usize,
}

/// **The floor beat.** Given the per-target decisions + the spin policy, decide
/// each arch's `minRunners`: raise to `warm_floor` while any target on that arch
/// is warming, else the `idle_floor`. Pure + total.
///
/// Only arches that appear among the targets get a floor entry — an arch with no
/// declared target is not spun (honest: we never raise a floor we have no work
/// for).
#[must_use]
pub fn plan_floor(
    targets: &[WarmTarget],
    decisions: &[WarmDecision],
    spin: &FloorSpinCfg,
) -> Vec<ArchFloor> {
    // The arches in play, in first-seen order (deterministic, alloc-light).
    let mut arches: Vec<Arch> = Vec::new();
    for t in targets {
        if !arches.contains(&t.arch) {
            arches.push(t.arch);
        }
    }

    arches
        .into_iter()
        .map(|arch| {
            // Count targets on this arch decided to Warm this tick. Decisions are
            // matched to targets by name (order-independent + correct even if a
            // caller reorders), so a Warm on this arch raises the floor.
            let warming_count = targets
                .iter()
                .filter(|t| t.arch == arch)
                .filter(|t| {
                    decisions
                        .iter()
                        .any(|d| d.target == t.name && d.action == WarmAction::Warm)
                })
                .count();

            let (desired, reason) = if warming_count > 0 {
                (spin.warm_floor.min(spin.max_floor), FloorReason::WarmingRaise)
            } else {
                (spin.idle_floor.min(spin.max_floor), FloorReason::IdleDrop)
            };

            ArchFloor {
                arch,
                desired_floor: desired,
                reason,
                warming_count,
            }
        })
        .collect()
}

// ───────────────────────────────────────────────────────────────────────────
// The plan — the whole perpetual-warming decision for one tick
// ───────────────────────────────────────────────────────────────────────────

/// The full per-tick perpetual-warming plan — the warming controller's brain.
/// Pure + total (produced by [`plan_preheat`] from a config + observations with
/// no I/O).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PreheatPlan {
    /// One decision per configured target.
    pub decisions: Vec<WarmDecision>,
    /// The per-arch builder-floor plan (raise-while-warming).
    pub floors: Vec<ArchFloor>,
    /// The count of targets to warm this tick (`action == Warm`).
    pub to_warm: usize,
    /// The count of targets already warm.
    pub already_warm: usize,
    /// The count of targets skipped (unclassifiable).
    pub skipped: usize,
    /// The warm fraction, percent of **classifiable** targets that are warm
    /// (already-warm / (already-warm + to-warm)). `100` when nothing needs
    /// warming; `0` when all classifiable targets are cold. Skipped targets are
    /// excluded (they carry no warmth signal).
    pub warm_fraction_pct: u8,
    /// SHADOW-first: `true` ⇒ the Act beat applies nothing (observe first).
    pub shadow: bool,
}

impl PreheatPlan {
    /// The empty plan — warming disabled or no targets. Honest: no warmth
    /// claimed, nothing spun.
    #[must_use]
    pub fn empty(shadow: bool) -> Self {
        Self {
            decisions: Vec::new(),
            floors: Vec::new(),
            to_warm: 0,
            already_warm: 0,
            skipped: 0,
            // No classifiable targets ⇒ vacuously "warm" is a round-up; report 0.
            warm_fraction_pct: 0,
            shadow,
        }
    }

    /// The count of decisions this plan considered (decisions + floors) — feeds
    /// the controller report's `objects_examined`.
    #[must_use]
    pub fn decision_count(&self) -> usize {
        self.decisions.len() + self.floors.len()
    }
}

/// **The plan beat.** Fold the per-target classify + the per-arch floor into one
/// coherent [`PreheatPlan`]. Pure + total.
///
/// - Warming disabled (`!cfg.enabled`) ⇒ [`PreheatPlan::empty`] (honest OFF).
/// - Observations are matched to targets by name; a target with no observation
///   is treated as **cold** (never observed ⇒ never warmed ⇒ warm it).
/// - `cfg.dry_run` ⇒ the plan is `shadow` ⇒ the Act beat applies nothing.
#[must_use]
pub fn plan_preheat(cfg: &PreheatCfg, observations: &[TargetObservation]) -> PreheatPlan {
    if !cfg.enabled || cfg.targets.is_empty() {
        return PreheatPlan::empty(cfg.dry_run);
    }

    let decisions: Vec<WarmDecision> = cfg
        .targets
        .iter()
        .map(|t| {
            // Match the observation by name; absent ⇒ a cold default observation.
            let obs = observations
                .iter()
                .find(|o| o.name == t.name)
                .cloned()
                .unwrap_or_else(|| TargetObservation {
                    name: t.name.clone(),
                    ..Default::default()
                });
            classify_target(t, &obs, cfg.cadence_secs)
        })
        .collect();

    let to_warm = decisions.iter().filter(|d| d.action == WarmAction::Warm).count();
    let already_warm = decisions
        .iter()
        .filter(|d| d.action == WarmAction::AlreadyWarm)
        .count();
    let skipped = decisions.iter().filter(|d| d.action == WarmAction::Skip).count();

    // Warm fraction over CLASSIFIABLE targets only (skipped carry no signal).
    let classifiable = to_warm + already_warm;
    let warm_fraction_pct = if classifiable == 0 {
        0
    } else {
        // (already_warm / classifiable) * 100, integer, saturating into u8.
        let pct = (already_warm * 100) / classifiable;
        u8::try_from(pct).unwrap_or(100)
    };

    let floors = plan_floor(&cfg.targets, &decisions, &cfg.floor_spin);

    PreheatPlan {
        decisions,
        floors,
        to_warm,
        already_warm,
        skipped,
        warm_fraction_pct,
        shadow: cfg.dry_run,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The Viggy (defpromessa) "cache stays warm"
// ───────────────────────────────────────────────────────────────────────────

/// The Viggy `(defpromessa)` **"the sui super-cache stays warm"** as a typed
/// outcome value. This is the typed twin of the `camelot-cache-warm` Promessa
/// CR: the three business predicates the cluster proves it is holding tick by
/// tick.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WarmthPromessa {
    /// The promessa name (`camelot-cache-warm`).
    pub name: String,
    /// The warm-fraction objective — at least this percent of classifiable
    /// targets must be warm.
    pub warm_fraction_target_pct: u8,
    /// No target may be staler than this (seconds) — the cadence + slack.
    pub max_staleness_secs: u32,
    /// The cost invariant: when nothing is warming, every builder floor must be
    /// zero (100%-spot scale-to-zero; cost at rest is zero).
    pub floor_zero_when_idle: bool,
}

impl WarmthPromessa {
    /// The camelot promessa derived from a [`PreheatCfg`] — the objective + the
    /// staleness ceiling + the floor-zero-when-idle cost invariant.
    #[must_use]
    pub fn from_cfg(name: &str, cfg: &PreheatCfg) -> Self {
        Self {
            name: name.to_string(),
            warm_fraction_target_pct: cfg.warm_fraction_target_pct,
            max_staleness_secs: cfg.max_staleness_secs(),
            floor_zero_when_idle: cfg.floor_spin.idle_floor == 0,
        }
    }

    /// **Evaluate the promessa against a plan + the observed staleness.** Held
    /// iff all three predicates hold:
    ///
    /// 1. **warm-fraction** — `plan.warm_fraction_pct >= target` (or nothing is
    ///    classifiable, in which case warmth is vacuous and *not* claimed —
    ///    returns [`Breached`](WarmthVerdict::Breached) with a
    ///    [`WarmthBreach::NothingClassifiable`], never a rounded-up Held).
    /// 2. **freshness** — `max_observed_staleness_secs <= max_staleness_secs`.
    /// 3. **cost** — if [`floor_zero_when_idle`](WarmthPromessa::floor_zero_when_idle),
    ///    every arch whose floor is [`IdleDrop`](FloorReason::IdleDrop) has
    ///    `desired_floor == 0`.
    #[must_use]
    pub fn evaluate(&self, plan: &PreheatPlan, max_observed_staleness_secs: u32) -> WarmthEvaluation {
        // Predicate 1 — warm fraction. Nothing classifiable ⇒ never claim warm.
        let classifiable = plan.to_warm + plan.already_warm;
        if classifiable == 0 {
            return WarmthEvaluation {
                verdict: WarmthVerdict::Breached,
                breach: Some(WarmthBreach::NothingClassifiable),
            };
        }
        if plan.warm_fraction_pct < self.warm_fraction_target_pct {
            return WarmthEvaluation {
                verdict: WarmthVerdict::Breached,
                breach: Some(WarmthBreach::WarmFractionLow),
            };
        }

        // Predicate 2 — freshness.
        if max_observed_staleness_secs > self.max_staleness_secs {
            return WarmthEvaluation {
                verdict: WarmthVerdict::Breached,
                breach: Some(WarmthBreach::Stale),
            };
        }

        // Predicate 3 — cost invariant (floor zero when idle).
        if self.floor_zero_when_idle {
            let leaks = plan
                .floors
                .iter()
                .any(|f| f.reason == FloorReason::IdleDrop && f.desired_floor != 0);
            if leaks {
                return WarmthEvaluation {
                    verdict: WarmthVerdict::Breached,
                    breach: Some(WarmthBreach::FloorNotZeroAtRest),
                };
            }
        }

        WarmthEvaluation {
            verdict: WarmthVerdict::Held,
            breach: None,
        }
    }
}

/// The promessa verdict — is the "cache stays warm" outcome currently held?
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WarmthVerdict {
    /// All three predicates hold — the cache is warm, fresh, and cost-zero at rest.
    Held,
    /// At least one predicate is violated (see [`WarmthBreach`]).
    Breached,
}

/// Which predicate a breach violated — typed so a consumer sees exactly why.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WarmthBreach {
    /// No classifiable target ⇒ warmth is vacuous ⇒ not claimed (honest floor).
    NothingClassifiable,
    /// The warm fraction is below the objective.
    WarmFractionLow,
    /// A target is staler than the ceiling.
    Stale,
    /// A builder floor is non-zero while idle (a cost leak).
    FloorNotZeroAtRest,
}

/// A promessa evaluation — verdict + the breach (if any).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmthEvaluation {
    /// Held or breached.
    pub verdict: WarmthVerdict,
    /// The specific breach — `None` iff `verdict == Held`.
    pub breach: Option<WarmthBreach>,
}

/// A typed one-line preheat-plan summary — the sanctioned typed-emission surface
/// for a log/report note (`write!` inside `Display`, never `format!()`).
pub struct PreheatSummary<'a> {
    /// The plan to summarize.
    pub plan: &'a PreheatPlan,
}

impl fmt::Display for PreheatSummary<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let raised = self
            .plan
            .floors
            .iter()
            .filter(|fl| fl.reason == FloorReason::WarmingRaise)
            .count();
        write!(
            f,
            "{} warm={} already={} skip={} warm_pct={} floors_raised={}",
            if self.plan.shadow { "shadow" } else { "live" },
            self.plan.to_warm,
            self.plan.already_warm,
            self.plan.skipped,
            self.plan.warm_fraction_pct,
            raised,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(name: &str, l1: bool, l2: bool, cur: &str, warmed: &str, age: u32) -> TargetObservation {
        TargetObservation {
            name: name.to_string(),
            present_l1: l1,
            present_l2: l2,
            current_input_hash: cur.to_string(),
            warmed_input_hash: warmed.to_string(),
            secs_since_warm: age,
        }
    }

    #[test]
    fn cold_target_warms_on_cold_start() {
        let t = WarmTarget::flake("auth", Arch::Amd64);
        // Absent from L1 → cold regardless of hashes.
        let d = classify_target(&t, &obs("auth", false, true, "h1", "h1", 0), 3600);
        assert_eq!(d.action, WarmAction::Warm);
        assert_eq!(d.trigger, Some(WarmTrigger::ColdStart));
    }

    #[test]
    fn never_warmed_is_cold_even_if_present() {
        let t = WarmTarget::flake("auth", Arch::Amd64);
        // Present in both tiers but warmed_input_hash empty ⇒ never warmed ⇒ cold.
        let d = classify_target(&t, &obs("auth", true, true, "h1", "", 0), 3600);
        assert_eq!(d.trigger, Some(WarmTrigger::ColdStart));
    }

    #[test]
    fn input_change_beats_cadence() {
        let t = WarmTarget::flake("auth", Arch::Amd64);
        // Present + warmed, but the input hash changed AND it is old — input-changed wins.
        let d = classify_target(&t, &obs("auth", true, true, "h2", "h1", 999_999), 3600);
        assert_eq!(d.trigger, Some(WarmTrigger::InputChanged));
    }

    #[test]
    fn cadence_fires_when_current_but_stale() {
        let t = WarmTarget::flake("auth", Arch::Amd64);
        let d = classify_target(&t, &obs("auth", true, true, "h1", "h1", 7200), 3600);
        assert_eq!(d.trigger, Some(WarmTrigger::Cadence));
    }

    #[test]
    fn fresh_current_closure_is_already_warm() {
        let t = WarmTarget::flake("auth", Arch::Amd64);
        let d = classify_target(&t, &obs("auth", true, true, "h1", "h1", 60), 3600);
        assert_eq!(d.action, WarmAction::AlreadyWarm);
        assert_eq!(d.trigger, None);
    }

    #[test]
    fn no_tracked_inputs_is_skip_never_assumed_warm() {
        let t = WarmTarget {
            name: "mystery".to_string(),
            arch: Arch::Amd64,
            inputs: Vec::new(),
        };
        let d = classify_target(&t, &obs("mystery", true, true, "h1", "h1", 0), 3600);
        assert_eq!(d.action, WarmAction::Skip);
        assert_eq!(d.trigger, None);
    }

    #[test]
    fn floor_raised_only_while_warming_then_dropped() {
        let cfg = PreheatCfg {
            enabled: true,
            cadence_secs: 3600,
            warm_fraction_target_pct: 99,
            targets: vec![
                WarmTarget::flake("auth", Arch::Amd64),
                WarmTarget::flake("gw", Arch::Arm64),
            ],
            floor_spin: FloorSpinCfg::camelot(),
            dry_run: true,
        };
        // auth is cold (warms) → amd64 floor raised; gw is fresh → arm64 idle.
        let observations = vec![
            obs("auth", false, false, "h1", "", 0),
            obs("gw", true, true, "h1", "h1", 60),
        ];
        let plan = plan_preheat(&cfg, &observations);
        let amd = plan.floors.iter().find(|f| f.arch == Arch::Amd64).unwrap();
        let arm = plan.floors.iter().find(|f| f.arch == Arch::Arm64).unwrap();
        assert_eq!(amd.reason, FloorReason::WarmingRaise);
        assert_eq!(amd.desired_floor, 1);
        assert_eq!(arm.reason, FloorReason::IdleDrop);
        assert_eq!(arm.desired_floor, 0, "cost at rest is zero on the idle arch");
    }

    #[test]
    fn floor_clamped_to_max() {
        let spin = FloorSpinCfg {
            warm_floor: 50,
            idle_floor: 0,
            max_floor: 8,
        };
        let targets = vec![WarmTarget::flake("a", Arch::Amd64)];
        let decisions = vec![WarmDecision {
            target: "a".to_string(),
            action: WarmAction::Warm,
            trigger: Some(WarmTrigger::ColdStart),
        }];
        let floors = plan_floor(&targets, &decisions, &spin);
        assert_eq!(floors[0].desired_floor, 8, "never climb past max_floor");
    }

    #[test]
    fn disabled_yields_empty_honest_plan() {
        let mut cfg = PreheatCfg::camelot();
        cfg.enabled = false;
        cfg.targets = vec![WarmTarget::flake("auth", Arch::Amd64)];
        let plan = plan_preheat(&cfg, &[]);
        assert!(plan.decisions.is_empty());
        assert!(plan.floors.is_empty());
        assert_eq!(plan.warm_fraction_pct, 0, "off ⇒ no warmth claimed");
    }

    #[test]
    fn missing_observation_is_treated_cold() {
        let cfg = PreheatCfg {
            enabled: true,
            cadence_secs: 3600,
            warm_fraction_target_pct: 99,
            targets: vec![WarmTarget::flake("auth", Arch::Amd64)],
            floor_spin: FloorSpinCfg::camelot(),
            dry_run: true,
        };
        // No observation supplied ⇒ default (cold) ⇒ warm on cold-start.
        let plan = plan_preheat(&cfg, &[]);
        assert_eq!(plan.to_warm, 1);
        assert_eq!(plan.decisions[0].trigger, Some(WarmTrigger::ColdStart));
    }

    #[test]
    fn warm_fraction_excludes_skips() {
        let cfg = PreheatCfg {
            enabled: true,
            cadence_secs: 3600,
            warm_fraction_target_pct: 50,
            targets: vec![
                WarmTarget::flake("a", Arch::Amd64), // already warm
                WarmTarget::flake("b", Arch::Amd64), // cold → warm
                WarmTarget {                          // skip (no inputs)
                    name: "c".to_string(),
                    arch: Arch::Amd64,
                    inputs: Vec::new(),
                },
            ],
            floor_spin: FloorSpinCfg::camelot(),
            dry_run: true,
        };
        let observations = vec![
            obs("a", true, true, "h1", "h1", 60),
            obs("b", false, false, "h1", "", 0),
        ];
        let plan = plan_preheat(&cfg, &observations);
        assert_eq!(plan.already_warm, 1);
        assert_eq!(plan.to_warm, 1);
        assert_eq!(plan.skipped, 1);
        // 1 warm / 2 classifiable = 50%, skip excluded.
        assert_eq!(plan.warm_fraction_pct, 50);
    }

    #[test]
    fn promessa_held_when_warm_fresh_and_cost_zero() {
        let cfg = PreheatCfg::camelot();
        let promessa = WarmthPromessa::from_cfg("camelot-cache-warm", &cfg);
        // A plan where everything is already warm, idle floors zero.
        let plan = PreheatPlan {
            decisions: vec![WarmDecision {
                target: "auth".to_string(),
                action: WarmAction::AlreadyWarm,
                trigger: None,
            }],
            floors: vec![ArchFloor {
                arch: Arch::Amd64,
                desired_floor: 0,
                reason: FloorReason::IdleDrop,
                warming_count: 0,
            }],
            to_warm: 0,
            already_warm: 1,
            skipped: 0,
            warm_fraction_pct: 100,
            shadow: true,
        };
        let e = promessa.evaluate(&plan, 60);
        assert_eq!(e.verdict, WarmthVerdict::Held);
        assert_eq!(e.breach, None);
    }

    #[test]
    fn promessa_breached_when_cold() {
        let cfg = PreheatCfg::camelot();
        let promessa = WarmthPromessa::from_cfg("camelot-cache-warm", &cfg);
        // Everything cold ⇒ warm_fraction 0 < 99 ⇒ breach.
        let plan = PreheatPlan {
            decisions: vec![WarmDecision {
                target: "auth".to_string(),
                action: WarmAction::Warm,
                trigger: Some(WarmTrigger::ColdStart),
            }],
            floors: vec![ArchFloor {
                arch: Arch::Amd64,
                desired_floor: 1,
                reason: FloorReason::WarmingRaise,
                warming_count: 1,
            }],
            to_warm: 1,
            already_warm: 0,
            skipped: 0,
            warm_fraction_pct: 0,
            shadow: true,
        };
        let e = promessa.evaluate(&plan, 60);
        assert_eq!(e.verdict, WarmthVerdict::Breached);
        assert_eq!(e.breach, Some(WarmthBreach::WarmFractionLow));
    }

    #[test]
    fn promessa_breached_on_staleness() {
        let cfg = PreheatCfg::camelot();
        let promessa = WarmthPromessa::from_cfg("camelot-cache-warm", &cfg);
        let plan = PreheatPlan {
            decisions: vec![],
            floors: vec![],
            to_warm: 0,
            already_warm: 1,
            skipped: 0,
            warm_fraction_pct: 100,
            shadow: true,
        };
        // Observed staleness beyond cadence+slack (6h+1h = 25200s).
        let e = promessa.evaluate(&plan, 30_000);
        assert_eq!(e.breach, Some(WarmthBreach::Stale));
    }

    #[test]
    fn promessa_breached_on_cost_leak() {
        let cfg = PreheatCfg::camelot();
        let promessa = WarmthPromessa::from_cfg("camelot-cache-warm", &cfg);
        // Warm + fresh, but an idle arch has a non-zero floor ⇒ cost leak.
        let plan = PreheatPlan {
            decisions: vec![],
            floors: vec![ArchFloor {
                arch: Arch::Arm64,
                desired_floor: 2,
                reason: FloorReason::IdleDrop,
                warming_count: 0,
            }],
            to_warm: 0,
            already_warm: 1,
            skipped: 0,
            warm_fraction_pct: 100,
            shadow: true,
        };
        let e = promessa.evaluate(&plan, 60);
        assert_eq!(e.breach, Some(WarmthBreach::FloorNotZeroAtRest));
    }

    #[test]
    fn promessa_breached_when_nothing_classifiable() {
        let cfg = PreheatCfg::camelot();
        let promessa = WarmthPromessa::from_cfg("camelot-cache-warm", &cfg);
        let plan = PreheatPlan::empty(true);
        // Vacuous warmth is NOT rounded up to Held.
        let e = promessa.evaluate(&plan, 0);
        assert_eq!(e.verdict, WarmthVerdict::Breached);
        assert_eq!(e.breach, Some(WarmthBreach::NothingClassifiable));
    }

    #[test]
    fn shadow_first_flows_from_dry_run() {
        let cfg = PreheatCfg::camelot(); // dry_run = true
        let mut cfg = cfg;
        cfg.targets = vec![WarmTarget::flake("auth", Arch::Amd64)];
        let plan = plan_preheat(&cfg, &[]);
        assert!(plan.shadow, "dry_run ⇒ shadow ⇒ Act applies nothing");
    }

    #[test]
    fn camelot_cfg_is_shadow_first_six_hour_cadence() {
        let c = PreheatCfg::camelot();
        assert!(c.enabled);
        assert!(c.dry_run, "camelot warming is shadow-first");
        assert_eq!(c.cadence_secs, 21_600);
        assert_eq!(c.floor_spin.idle_floor, 0, "scale-to-zero at rest");
        assert_eq!(c.max_staleness_secs(), 25_200); // 6h + 1h slack
    }

    #[test]
    fn authoring_keywords_are_unique_and_prefixed() {
        for k in PREHEAT_AUTHORING_KEYWORDS {
            assert!(k.starts_with("def"), "{k} is a def-keyword");
        }
        for (i, a) in PREHEAT_AUTHORING_KEYWORDS.iter().enumerate() {
            for b in &PREHEAT_AUTHORING_KEYWORDS[i + 1..] {
                assert_ne!(a, b, "keywords are unique");
            }
        }
    }

    #[test]
    fn honest_gate_tier_is_shadow_core_not_live() {
        // Bumping PREHEAT_TIER to Live without a live coordinator loop is a
        // build-failing round-up.
        assert_eq!(PREHEAT_TIER, PreheatTier::ShadowCore);
    }

    #[test]
    fn summary_renders_without_format_macro() {
        let plan = plan_preheat(
            &PreheatCfg {
                enabled: true,
                cadence_secs: 3600,
                warm_fraction_target_pct: 99,
                targets: vec![WarmTarget::flake("auth", Arch::Amd64)],
                floor_spin: FloorSpinCfg::camelot(),
                dry_run: true,
            },
            &[],
        );
        let s = PreheatSummary { plan: &plan }.to_string();
        assert!(s.contains("shadow"));
        assert!(s.contains("warm=1"));
    }
}
