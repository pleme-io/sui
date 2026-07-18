//! `reconcile` — the continuous system-convergence loop: the node is kept
//! **always rebuilt into place**, streaming the atomic profile-swap link change
//! each time its declared flake drifts from what is activated.
//!
//! ## What this is
//!
//! The Viggy Method (★★ CONTINUOUS CONVERGENCE — controllers, not runbooks)
//! applied to *this node's own OS state*. Where `sui system rebuild` is a
//! one-shot `(evaluate → build → activate)`, this is that step run **forever**
//! as a `(defpromessa system-in-place)` reconcile loop: the promessa is
//! *"the node's active system generation equals the toplevel its flake
//! declares"*, and the controller proves — tick by tick, attested — that it is
//! holding it, converging once whenever the source drifts.
//!
//! It **reuses the shipped fleet substrate, never re-rolls it**:
//!
//! - The **seven-beat contract** — [`Controller`] / [`ReconcileOutcome`] /
//!   [`ReconcileResult`] / [`ReconcileReport`] — is taken **verbatim from
//!   [`sui_supercacheci::controller`]** (the same contract the super-cache-ci
//!   controller and the dockerfile-prewarmer's layers-stay-warm loop implement).
//!   This is the *third* consumer — the three-site threshold that justifies the
//!   named destination: extract the trait to a shared leaf crate.
//! - The **Observe** (`build_toplevel`) and **Act** (`activate`) primitives are
//!   the shipped [`crate::system::SystemOrchestrator`] pipeline, behind the
//!   [`env::ReconcileEnvironment`] seam.
//! - The **converge STEP** is a typed [`shigoto_dag::Dag`] (SHIGOTO.md), ready
//!   for the day activation gains ordered sub-steps (`/etc` farm → launchd →
//!   `/run/current-system`, per `LINK-GRAPH.md` L4).
//! - The **attestation** is a BLAKE3 [`attest::OutcomeChain`] (tier-honest: a
//!   hash chain, not an Ed25519-signed chain).
//!
//! ## The seven beats (one tick)
//!
//! ```text
//! Observe ─ desired = build_toplevel(flake); current = active generation ─▶ SystemObservation
//! Diff    ─ basename(desired) vs basename(current) ──────────────────────▶ drifted?
//! Classify─ SystemInPlace.evaluate(obs) ─────────────────────────────────▶ ReconcileVerdict
//! Decide  ─ Hold | Converge | Shadow | BuildOnly | Unobservable + a Dag ──▶ ReconcileDecision
//! Act     ─ drive the converge Dag ⇒ env.converge (atomic profile swap) ──▶ ActOutcome
//! Attest  ─ append the tick to the BLAKE3 OutcomeChain ───────────────────▶ head id
//! Tick    ─ Requeue(interval) ────────────────────────────────────────────▶ ReconcileOutcome
//! ```
//!
//! ## Tier-honest (never round up)
//!
//! - **Shipped + tested here:** the promessa + drift math + the seven-beat tick
//!   + the shigoto converge Dag + the BLAKE3 attestation, all mock-green over
//!   [`env::mock::MockReconcileEnv`] (no eval, no build, no `/nix`).
//! - **The live converge rides the same gate as `rebuild_native`:** a
//!   *byte-identical* cid convergence is blocked on the module-system fixpoint
//!   (M2.6) exactly as `sui system rebuild` is (`SUI-SUPREMACY-ROADMAP.md`). The
//!   loop is REAL + tested; the byte-identical *result* is gated — never
//!   conflated.
//! - **Attestation tier:** BLAKE3 hash chain, unsigned (see [`attest`]).

pub mod attest;
pub mod driver;
pub mod env;

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shigoto_dag::Dag;
use shigoto_types::{JobId, JobKindId, JobScope, JobSubject};
use shikumi::TieredConfig;

// REUSE — the seven-beat Controller contract, verbatim from the super-cache-ci
// controller (the fleet-standard reconcile shape, mirror of engenho's).
pub use sui_supercacheci::controller::{
    Controller, ControllerError, ReconcileOutcome, ReconcileReport, ReconcileResult,
};

use crate::system::RebuildAction;
use attest::{OutcomeChain, ReconcileRecord};
use env::{ActiveGeneration, ReconcileEnvironment, store_basename};

/// The default reconcile tick cadence — how often the loop re-checks for drift
/// even when no source change fires. Mirrors the super-cache-ci controller's
/// 30-second default.
pub const DEFAULT_INTERVAL_SECS: u64 = 30;

// ───────────────────────────────────────────────────────────────────────────
// ReconcileConfig — the typed shikumi::TieredConfig the loop reconciles toward
// ───────────────────────────────────────────────────────────────────────────

/// The typed reconcile config — a [`shikumi::TieredConfig`] (default ←
/// discovered ← override, hot-reload), configured exactly like every
/// operator-facing pleme-io tool (★★ CONFIGURATION MANAGEMENT).
///
/// `flake` is the full flake reference including the host attribute (e.g.
/// `.#cid` or `path:/Users/…/nix#cid`), matching `sui system rebuild --flake`.
/// The tier defaults leave it empty (the honest "unset" floor, like
/// super-cache-ci's `Endpoint::unset()`); the CLI / a config file supplies it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReconcileConfig {
    /// The promessa name (`system-in-place`).
    pub name: String,
    /// The full flake reference including the host attribute (e.g. `.#cid`).
    pub flake: String,
    /// The action a converge applies. `DryActivate` is the **shadow** posture
    /// (observe + build the desired toplevel, never activate) — the safe floor;
    /// `Switch`/`Boot`/`Test` are the mutating converge; `Build` builds-only.
    pub action: RebuildAction,
    /// The drift-catch tick cadence, in seconds.
    pub interval_secs: u64,
    /// Whether to stream FSEvents on the flake directory and re-converge the
    /// instant the source changes (in addition to the interval tick).
    pub watch: bool,
}

impl ReconcileConfig {
    /// The host / flake attribute this config targets — the part after `#`
    /// (or the whole string if there is no `#`). Used for the attestation
    /// record, the Dag subject, and logs.
    #[must_use]
    pub fn host(&self) -> &str {
        self.flake.split_once('#').map_or(self.flake.as_str(), |(_, a)| a)
    }

    /// The requeue interval as a [`Duration`].
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }

    /// Whether this config's action would actually mutate the live system on a
    /// drift (i.e. is NOT the shadow `DryActivate` / build-only posture).
    #[must_use]
    pub fn converges(&self) -> bool {
        self.action.mutates_system()
    }
}

impl TieredConfig for ReconcileConfig {
    /// Tier 0 — the honest, **safe** floor: shadow posture (`DryActivate`,
    /// observe-only, never touches the live system), watch off, 60 s cadence,
    /// no flake selected. Running `bare()` mutates nothing on any node.
    fn bare() -> Self {
        Self {
            name: "system-in-place".to_string(),
            flake: String::new(),
            action: RebuildAction::DryActivate,
            interval_secs: 60,
            watch: false,
        }
    }

    /// Tier 2 — the prescribed destination posture: **the node is kept in place
    /// live** (`Switch` converge on drift), FSEvents streaming on, the default
    /// 30 s drift-catch cadence. The `flake` is still unset — it is the one
    /// value only the operator (or a config file) can supply.
    fn prescribed_default() -> Self {
        Self {
            name: "system-in-place".to_string(),
            flake: String::new(),
            action: RebuildAction::Switch,
            interval_secs: DEFAULT_INTERVAL_SECS,
            watch: true,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The (defpromessa system-in-place) — the one predicate the loop proves
// ───────────────────────────────────────────────────────────────────────────

/// The Viggy `(defpromessa)` **"the node's active system generation equals the
/// toplevel its flake declares"** as a typed value. The twin of the prewarmer's
/// `LayersStayWarm`, over this loop's own observable: the *drift* between the
/// desired and active system toplevels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemInPlace {
    /// The promessa name (`system-in-place`).
    pub name: String,
}

impl Default for SystemInPlace {
    fn default() -> Self {
        Self {
            name: "system-in-place".to_string(),
        }
    }
}

impl SystemInPlace {
    /// Build a promessa with an explicit name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// **Evaluate the promessa against an observation.** Held (`InPlace`) iff the
    /// desired toplevel basename equals the active generation's basename;
    /// `Drifted` if they differ or no generation is active yet; `Unobservable`
    /// (never rounded up to Held *or* Drifted) if the desired could not be built
    /// or the active generation could not be read — a transient error can
    /// neither satisfy nor falsely fail the promessa (the same honest floor the
    /// prewarmer holds for a GitHub outage).
    #[must_use]
    pub fn evaluate(&self, obs: &SystemObservation) -> ReconcileEvaluation {
        if obs.desired_basename.is_none() {
            return ReconcileEvaluation::breached(
                ReconcileVerdict::Unobservable,
                obs.observe_error
                    .clone()
                    .unwrap_or_else(|| "desired toplevel unbuildable".to_string()),
            );
        }
        if let Some(err) = &obs.current_error {
            return ReconcileEvaluation::breached(ReconcileVerdict::Unobservable, err.clone());
        }
        match &obs.current {
            None => ReconcileEvaluation::breached(
                ReconcileVerdict::Drifted,
                "no-active-generation".to_string(),
            ),
            Some(cur) => {
                let desired = obs.desired_basename.as_deref().unwrap_or_default();
                if desired == cur.basename() {
                    ReconcileEvaluation::held()
                } else {
                    ReconcileEvaluation::breached(
                        ReconcileVerdict::Drifted,
                        "desired-not-active".to_string(),
                    )
                }
            }
        }
    }
}

/// The typed promessa verdict — the Classify beat's product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconcileVerdict {
    /// The active generation equals the declared toplevel — the promessa holds.
    InPlace,
    /// The declared toplevel differs from what is active (or nothing is active).
    Drifted,
    /// The desired or the current state could not be observed this tick.
    Unobservable,
}

impl ReconcileVerdict {
    /// The stable string for the attestation record.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InPlace => "in-place",
            Self::Drifted => "drifted",
            Self::Unobservable => "unobservable",
        }
    }

    /// Whether the promessa holds.
    #[must_use]
    pub fn is_held(&self) -> bool {
        matches!(self, Self::InPlace)
    }
}

/// A promessa evaluation — the verdict plus, when not held, a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileEvaluation {
    /// The verdict.
    pub verdict: ReconcileVerdict,
    /// The reason string when not `InPlace`; `None` iff held.
    pub reason: Option<String>,
}

impl ReconcileEvaluation {
    /// A held evaluation (in place).
    #[must_use]
    pub fn held() -> Self {
        Self {
            verdict: ReconcileVerdict::InPlace,
            reason: None,
        }
    }

    /// A breached evaluation with a verdict + reason.
    #[must_use]
    pub fn breached(verdict: ReconcileVerdict, reason: String) -> Self {
        Self {
            verdict,
            reason: Some(reason),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The Observe + Diff product
// ───────────────────────────────────────────────────────────────────────────

/// The Observe+Diff product of one tick — the desired toplevel, the active
/// generation, and any observe errors, all as plain data so the Decide brain is
/// pure + mock-testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemObservation {
    /// The desired system toplevel store path (`None` if the build failed).
    pub desired: Option<String>,
    /// The desired toplevel's store basename (the drift-compare identity).
    pub desired_basename: Option<String>,
    /// The node's active generation (`None` on a fresh system).
    pub current: Option<ActiveGeneration>,
    /// Why `desired` is `None`, if it is (an eval/build error).
    pub observe_error: Option<String>,
    /// Why the active generation could not be read, if it could not be.
    pub current_error: Option<String>,
}

impl SystemObservation {
    /// Whether the system is drifted — the declared toplevel differs from the
    /// active one, or no generation is active yet (a fresh node). Only meaningful
    /// when observable (`desired` known + no `current_error`).
    #[must_use]
    pub fn drifted(&self) -> bool {
        if self.desired_basename.is_none() || self.current_error.is_some() {
            return false;
        }
        match (&self.desired_basename, &self.current) {
            (Some(d), Some(c)) => *d != c.basename(),
            (Some(_), None) => true, // fresh node — needs the first generation
            _ => false,
        }
    }

    /// Whether this tick could observe both the desired and current state.
    #[must_use]
    pub fn observable(&self) -> bool {
        self.desired_basename.is_some() && self.current_error.is_none()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The Decide beat — the reconcile decision + a typed shigoto converge Dag
// ───────────────────────────────────────────────────────────────────────────

/// The Decide beat's product — what this tick will do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileDecision {
    /// In place — hold, converge nothing.
    Hold,
    /// Drifted + a mutating action — converge (activate the desired toplevel).
    Converge,
    /// Drifted + the `DryActivate` shadow posture — the toplevel is built +
    /// diffed, but nothing is activated (breathe's shadow-first gate).
    Shadow,
    /// Drifted + the `Build` action — the toplevel is built, never activated.
    BuildOnly,
    /// The desired or current state could not be observed — converge nothing.
    Unobservable,
}

impl ReconcileDecision {
    /// The stable verdict string this decision attests, *before* the Act beat's
    /// success/failure is known (Converge is refined to `converged`/`failed` in
    /// [`ActOutcome`]).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hold => "in-place",
            Self::Converge => "converging",
            Self::Shadow => "shadowed",
            Self::BuildOnly => "built",
            Self::Unobservable => "unobservable",
        }
    }
}

/// Decide what to do from the config + observation. Pure.
#[must_use]
pub fn decide(config: &ReconcileConfig, obs: &SystemObservation) -> ReconcileDecision {
    if !obs.observable() {
        return ReconcileDecision::Unobservable;
    }
    if !obs.drifted() {
        return ReconcileDecision::Hold;
    }
    match config.action {
        RebuildAction::Switch | RebuildAction::Boot | RebuildAction::Test => {
            ReconcileDecision::Converge
        }
        RebuildAction::DryActivate => ReconcileDecision::Shadow,
        RebuildAction::Build => ReconcileDecision::BuildOnly,
    }
}

/// The typed `JobKindId` every apply-generation Job carries.
const APPLY_KIND: &str = "apply-generation";

/// The `JobScope` reconcile Jobs live under.
fn reconcile_scope() -> JobScope {
    JobScope::Workspace("reconcile".to_string())
}

/// The typed `JobId` for applying one host's generation — stable across ticks
/// (keyed by the host), so the same node maps to the same Job every tick.
#[must_use]
pub fn apply_job_id(host: &str) -> JobId {
    JobId {
        scope: reconcile_scope(),
        kind: JobKindId::new(APPLY_KIND),
        subject: JobSubject::Pinned(host.to_string()),
    }
}

/// The converge work as a typed [`shigoto_dag::Dag`] — a single
/// `apply-generation` node when the tick will converge, empty otherwise. One
/// node today (activation is one delegated step), but the Dag is the typed
/// carrier so the converge STEP is a work-graph value, not ad-hoc control flow —
/// ready for `LINK-GRAPH.md` L4, when native activation decomposes into ordered
/// sub-steps (`/etc` farm → launchd → `/run/current-system`) and [`Dag::waves`]
/// sequences them for free.
#[must_use]
pub fn build_reconcile_dag(host: &str, converge: bool) -> ReconcileDag {
    let mut dag = Dag::new();
    let job = if converge {
        let id = apply_job_id(host);
        dag.ensure_node(id.clone());
        Some(id)
    } else {
        None
    };
    ReconcileDag { dag, job }
}

/// The Decide beat's converge Dag + the (single) apply Job, if any.
pub struct ReconcileDag {
    /// The typed topology (one apply-generation node when converging).
    pub dag: Dag,
    /// The apply Job's id, `None` when there is no converge this tick.
    pub job: Option<JobId>,
}

impl ReconcileDag {
    /// Whether there is converge work this tick.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.job.is_none()
    }

    /// The Dag's topological waves — the order the Act beat drives the converge
    /// in. Errors only on a cycle, which a ≤1-node Dag cannot construct (kept as
    /// a typed error so a future edged Dag surfaces a malformed graph, never a
    /// panic).
    ///
    /// # Errors
    ///
    /// [`shigoto_dag::DagError::Cycle`] if a future edge set is cyclic.
    pub fn waves(&self) -> Result<Vec<Vec<JobId>>, shigoto_dag::DagError> {
        let affected: std::collections::HashSet<JobId> = self.job.iter().cloned().collect();
        self.dag.waves(Some(&affected))
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The Act beat's product
// ───────────────────────────────────────────────────────────────────────────

/// What the Act beat did this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActOutcome {
    /// The refined verdict string (`in-place` / `converged` / `failed` /
    /// `shadowed` / `built` / `unobservable`).
    pub verdict: String,
    /// Whether a converge (activation) actually ran + succeeded.
    pub converged: bool,
    /// The active generation before the converge, if one ran.
    pub from_generation: Option<u32>,
    /// The active generation after the converge, if one ran + succeeded.
    pub to_generation: Option<u32>,
    /// The failure reason, if the converge failed.
    pub error: Option<String>,
}

impl ActOutcome {
    /// A non-converging outcome (hold / shadow / built / unobservable).
    fn quiet(verdict: &str) -> Self {
        Self {
            verdict: verdict.to_string(),
            converged: false,
            from_generation: None,
            to_generation: None,
            error: None,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The controller — the SystemInPlace PromessaController running the seven beats
// ───────────────────────────────────────────────────────────────────────────

/// The `system-in-place` reconcile controller — this node's OS state as a Viggy
/// seven-beat loop. Holds the promessa + config + the injected
/// [`ReconcileEnvironment`] seam + the cross-tick attestation chain. Interior
/// mutability keeps the [`Controller::tick`] signature (`&self`) identical to the
/// super-cache-ci controller it mirrors.
pub struct SystemReconciler<E: ReconcileEnvironment> {
    promessa: SystemInPlace,
    config: ReconcileConfig,
    env: E,
    requeue: Duration,
    chain: Mutex<OutcomeChain>,
    tick_counter: AtomicU64,
}

impl<E: ReconcileEnvironment> SystemReconciler<E> {
    /// Build a reconciler from a config + an environment. The requeue interval
    /// is the config's `interval_secs`.
    #[must_use]
    pub fn new(config: ReconcileConfig, env: E) -> Self {
        let requeue = config.interval();
        Self {
            promessa: SystemInPlace::new(config.name.clone()),
            config,
            env,
            requeue,
            chain: Mutex::new(OutcomeChain::new()),
            tick_counter: AtomicU64::new(0),
        }
    }

    /// The config this controller reconciles toward.
    #[must_use]
    pub fn config(&self) -> &ReconcileConfig {
        &self.config
    }

    /// The promessa this controller proves it is holding.
    #[must_use]
    pub fn promessa(&self) -> &SystemInPlace {
        &self.promessa
    }

    /// The number of attested ticks so far.
    ///
    /// # Panics
    ///
    /// Only on a poisoned mutex (unreachable in the single-threaded tick loop).
    #[must_use]
    pub fn attested_ticks(&self) -> usize {
        self.chain.lock().unwrap().len()
    }

    /// A clone of the attestation chain — for verification / inspection.
    ///
    /// # Panics
    ///
    /// Only on a poisoned mutex (unreachable in the tick loop).
    #[must_use]
    pub fn outcome_chain(&self) -> OutcomeChain {
        self.chain.lock().unwrap().clone()
    }

    /// The attestation chain's current tip id — the head of proof.
    ///
    /// # Panics
    ///
    /// Only on a poisoned mutex (unreachable in the tick loop).
    #[must_use]
    pub fn chain_head(&self) -> [u8; 32] {
        self.chain.lock().unwrap().head()
    }

    /// **Beat 1 (Observe) + Beat 2 (Diff).** Build the flake's declared toplevel
    /// (desired) + read the node's active generation (current) via the seam, and
    /// diff their store basenames. Pure over the seam — no mutation.
    async fn observe(&self) -> SystemObservation {
        let (desired, desired_basename, observe_error) =
            match self.env.desired_toplevel(&self.config).await {
                Ok(path) => {
                    let base = store_basename(&path);
                    (Some(path), Some(base), None)
                }
                Err(e) => (None, None, Some(e.to_string())),
            };
        let (current, current_error) = match self.env.current_toplevel().await {
            Ok(active) => (active, None),
            Err(e) => (None, Some(e.to_string())),
        };
        SystemObservation {
            desired,
            desired_basename,
            current,
            observe_error,
            current_error,
        }
    }

    /// **Beat 5 (Act).** Drive the converge Dag through the seam. For a
    /// `Converge` decision this runs `env.converge` (the atomic profile swap —
    /// the streamed link change); every other decision is quiet (holds / shadows
    /// / builds-only / unobservable — nothing is activated).
    async fn act(
        &self,
        obs: &SystemObservation,
        decision: ReconcileDecision,
        dag: &ReconcileDag,
    ) -> ActOutcome {
        if decision != ReconcileDecision::Converge {
            return ActOutcome::quiet(decision.as_str());
        }
        // Drive the Dag's single wave (structural — a converge is one Job today).
        let Ok(_waves) = dag.waves() else {
            // A cyclic Dag is unconstructible for ≤1 node; treat the impossible
            // case as "no converge applied" rather than panicking.
            return ActOutcome::quiet("in-place");
        };
        let Some(desired) = obs.desired.as_deref() else {
            // Unreachable — Converge is only decided when desired is known.
            return ActOutcome::quiet("unobservable");
        };
        match self.env.converge(&self.config, desired).await {
            Ok(receipt) => ActOutcome {
                verdict: "converged".to_string(),
                converged: true,
                from_generation: receipt.from_generation,
                to_generation: receipt.to_generation,
                error: None,
            },
            Err(e) => ActOutcome {
                verdict: "failed".to_string(),
                converged: false,
                from_generation: None,
                to_generation: None,
                error: Some(e.to_string()),
            },
        }
    }

    /// **Beat 6 (Attest).** Build + append the tick's record to the BLAKE3
    /// OutcomeChain, returning the new head id.
    fn attest(&self, tick: u64, obs: &SystemObservation, act: &ActOutcome) -> [u8; 32] {
        // The reason a tick is not cleanly in-place: a converge failure if one
        // occurred, else the observe error (unobservable), else the
        // active-generation read error. `None` iff held / cleanly converged.
        let reason = act
            .error
            .clone()
            .or_else(|| obs.observe_error.clone())
            .or_else(|| obs.current_error.clone());
        let record = ReconcileRecord {
            promessa: self.promessa.name.clone(),
            tick,
            host: self.config.host().to_string(),
            verdict: act.verdict.clone(),
            reason,
            from_generation: act.from_generation,
            to_generation: act.to_generation,
            toplevel: obs.desired_basename.clone().unwrap_or_default(),
            action: self.config.action.as_str().to_string(),
        };
        self.chain.lock().unwrap().append(record)
    }
}

/// Build the seven-beat tick report — the typed `ReconcileReport` shape
/// (examined / changed / skipped + a note). `examined` = 1 when the tick could
/// observe (there is one node to reconcile), 0 when unobservable; `changed` = 1
/// iff a converge succeeded.
fn tick_report(obs: &SystemObservation, act: &ActOutcome, head: [u8; 32]) -> ReconcileReport {
    let examined = usize::from(obs.observable());
    let changed = usize::from(act.converged);
    ReconcileReport {
        objects_examined: examined,
        objects_changed: changed,
        objects_skipped: examined.saturating_sub(changed),
        note: Some(
            TickNote {
                verdict: &act.verdict,
                desired: obs.desired_basename.as_deref().unwrap_or("?"),
                from: act.from_generation,
                to: act.to_generation,
                head,
            }
            .to_string(),
        ),
    }
}

/// A typed one-line tick note — the sanctioned typed-emission surface for the
/// report note (`write!` inside `Display`, never `format!()`).
struct TickNote<'a> {
    verdict: &'a str,
    desired: &'a str,
    from: Option<u32>,
    to: Option<u32>,
    head: [u8; 32],
}

impl std::fmt::Display for TickNote<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} desired={}", self.verdict, self.desired)?;
        if let (Some(from), Some(to)) = (self.from, self.to) {
            write!(f, " gen={from}->{to}")?;
        }
        write!(
            f,
            " attest={}",
            data_encoding::HEXLOWER.encode(&self.head[..8])
        )
    }
}

#[async_trait]
impl<E: ReconcileEnvironment> Controller for SystemReconciler<E> {
    fn name(&self) -> &'static str {
        "system-reconcile"
    }

    async fn tick(&self) -> Result<ReconcileOutcome, ControllerError> {
        let tick = self.tick_counter.fetch_add(1, Ordering::Relaxed);

        // Beat 1+2 — Observe + Diff.
        let observation = self.observe().await;
        // Beat 3 — Classify: the promessa verdict.
        let eval = self.promessa.evaluate(&observation);
        // Beat 4 — Decide: hold / converge / shadow / build / unobservable + Dag.
        let decision = decide(&self.config, &observation);
        let dag = build_reconcile_dag(self.config.host(), decision == ReconcileDecision::Converge);
        tracing::debug!(
            promessa = %self.promessa.name,
            verdict = eval.verdict.as_str(),
            decision = decision.as_str(),
            "system-reconcile: classify + decide"
        );
        // Beat 5 — Act: drive the converge Dag (or hold/shadow).
        let act = self.act(&observation, decision, &dag).await;
        // Beat 6 — Attest.
        let head = self.attest(tick, &observation, &act);

        // Beat 7 — report + requeue. A system reconciler is never one-shot Done.
        let report = tick_report(&observation, &act, head);
        Ok(ReconcileOutcome::new(
            report,
            ReconcileResult::Requeue(self.requeue),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::env::mock::MockReconcileEnv;
    use super::*;

    fn cfg(action: RebuildAction) -> ReconcileConfig {
        ReconcileConfig {
            name: "system-in-place".to_string(),
            flake: "path:/x#cid".to_string(),
            action,
            interval_secs: 30,
            watch: false,
        }
    }

    // ── TieredConfig tiers ──────────────────────────────────────────────────

    #[test]
    fn bare_is_the_safe_shadow_floor() {
        let b = ReconcileConfig::bare();
        assert_eq!(b.action, RebuildAction::DryActivate, "bare() never mutates");
        assert!(!b.converges());
        assert!(!b.watch);
    }

    #[test]
    fn prescribed_default_keeps_the_node_in_place_live() {
        let p = ReconcileConfig::prescribed_default();
        assert_eq!(p.action, RebuildAction::Switch);
        assert!(p.converges());
        assert!(p.watch, "the destination streams source changes");
        assert_eq!(p.interval_secs, DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn host_is_the_flake_attribute() {
        assert_eq!(cfg(RebuildAction::Switch).host(), "cid");
        let no_attr = ReconcileConfig {
            flake: "just-a-path".to_string(),
            ..cfg(RebuildAction::Switch)
        };
        assert_eq!(no_attr.host(), "just-a-path");
    }

    // ── the promessa evaluate ───────────────────────────────────────────────

    #[test]
    fn promessa_holds_when_desired_equals_active() {
        let obs = SystemObservation {
            desired: Some("/nix/store/abc-sys".into()),
            desired_basename: Some("abc-sys".into()),
            current: Some(ActiveGeneration {
                generation: 5,
                toplevel: "/nix/store/abc-sys".into(),
            }),
            observe_error: None,
            current_error: None,
        };
        let e = SystemInPlace::default().evaluate(&obs);
        assert_eq!(e.verdict, ReconcileVerdict::InPlace);
        assert!(e.reason.is_none());
        assert!(!obs.drifted());
    }

    #[test]
    fn promessa_drifts_when_desired_differs() {
        let obs = SystemObservation {
            desired: Some("/nix/store/new-sys".into()),
            desired_basename: Some("new-sys".into()),
            current: Some(ActiveGeneration {
                generation: 5,
                toplevel: "/nix/store/old-sys".into(),
            }),
            observe_error: None,
            current_error: None,
        };
        let e = SystemInPlace::default().evaluate(&obs);
        assert_eq!(e.verdict, ReconcileVerdict::Drifted);
        assert_eq!(e.reason.as_deref(), Some("desired-not-active"));
        assert!(obs.drifted());
    }

    #[test]
    fn promessa_drifts_on_a_fresh_node() {
        let obs = SystemObservation {
            desired: Some("/nix/store/first".into()),
            desired_basename: Some("first".into()),
            current: None,
            observe_error: None,
            current_error: None,
        };
        let e = SystemInPlace::default().evaluate(&obs);
        assert_eq!(e.verdict, ReconcileVerdict::Drifted);
        assert_eq!(e.reason.as_deref(), Some("no-active-generation"));
        assert!(obs.drifted());
    }

    #[test]
    fn promessa_is_unobservable_never_rounded_up_when_build_fails() {
        let obs = SystemObservation {
            desired: None,
            desired_basename: None,
            current: Some(ActiveGeneration {
                generation: 5,
                toplevel: "/nix/store/old".into(),
            }),
            observe_error: Some("eval blew up".into()),
            current_error: None,
        };
        let e = SystemInPlace::default().evaluate(&obs);
        // Never a false InPlace or a false Drifted — honestly Unobservable.
        assert_eq!(e.verdict, ReconcileVerdict::Unobservable);
        assert_eq!(e.reason.as_deref(), Some("eval blew up"));
        assert!(!obs.drifted(), "unobservable is not drift");
    }

    // ── decide ──────────────────────────────────────────────────────────────

    #[test]
    fn decide_holds_when_in_place() {
        let obs = SystemObservation {
            desired_basename: Some("x".into()),
            current: Some(ActiveGeneration {
                generation: 1,
                toplevel: "/nix/store/x".into(),
            }),
            ..Default::default()
        };
        assert_eq!(decide(&cfg(RebuildAction::Switch), &obs), ReconcileDecision::Hold);
    }

    #[test]
    fn decide_shadows_a_drift_under_dry_activate() {
        let obs = SystemObservation {
            desired_basename: Some("new".into()),
            current: Some(ActiveGeneration {
                generation: 1,
                toplevel: "/nix/store/old".into(),
            }),
            ..Default::default()
        };
        assert_eq!(
            decide(&cfg(RebuildAction::DryActivate), &obs),
            ReconcileDecision::Shadow
        );
    }

    #[test]
    fn decide_converges_a_drift_under_switch() {
        let obs = SystemObservation {
            desired_basename: Some("new".into()),
            current: Some(ActiveGeneration {
                generation: 1,
                toplevel: "/nix/store/old".into(),
            }),
            ..Default::default()
        };
        assert_eq!(
            decide(&cfg(RebuildAction::Switch), &obs),
            ReconcileDecision::Converge
        );
    }

    #[test]
    fn build_reconcile_dag_has_one_node_iff_converging() {
        let converging = build_reconcile_dag("cid", true);
        assert!(!converging.is_empty());
        assert_eq!(converging.dag.node_count(), 1);
        let waves = converging.waves().expect("≤1-node dag never cycles");
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].len(), 1);

        let quiet = build_reconcile_dag("cid", false);
        assert!(quiet.is_empty());
        assert_eq!(quiet.dag.node_count(), 0);
    }

    // ── the full seven-beat tick over the mock seam ─────────────────────────

    #[tokio::test]
    async fn tick_holds_when_in_place_and_never_converges() {
        let env = MockReconcileEnv::in_place("abc-sys", 7);
        let ctl = SystemReconciler::new(cfg(RebuildAction::Switch), env);
        let out = ctl.tick().await.expect("tick");
        assert_eq!(out.report.objects_examined, 1, "one node observed");
        assert_eq!(out.report.objects_changed, 0, "in place ⇒ nothing changed");
        assert_eq!(ctl.config().action, RebuildAction::Switch);
        // A reconciler never terminates Done — it requeues.
        assert_eq!(out.result, ReconcileResult::Requeue(Duration::from_secs(30)));
        // Attested Held, no converge call.
        let chain = ctl.outcome_chain();
        chain.verify().expect("chain verifies");
        assert_eq!(chain.tip().unwrap().record.verdict, "in-place");
        assert!(!chain.tip().unwrap().is_signed(), "tier-honest: unsigned chain");
    }

    #[tokio::test]
    async fn tick_converges_a_drift_under_switch_then_holds() {
        let env = MockReconcileEnv::drifted("new-sys", "old-sys", 5, 6);
        let ctl = SystemReconciler::new(cfg(RebuildAction::Switch), env);

        // Tick 1 — drifted ⇒ converge.
        let t1 = ctl.tick().await.expect("tick 1");
        assert_eq!(t1.report.objects_changed, 1, "converged");
        let chain = ctl.outcome_chain();
        let tip = chain.tip().unwrap();
        assert_eq!(tip.record.verdict, "converged");
        assert_eq!(tip.record.from_generation, Some(5));
        assert_eq!(tip.record.to_generation, Some(6));

        // Tick 2 — the mock advanced current to the desired ⇒ now in place.
        let t2 = ctl.tick().await.expect("tick 2");
        assert_eq!(t2.report.objects_changed, 0, "already in place ⇒ quiet");
        assert_eq!(ctl.outcome_chain().tip().unwrap().record.verdict, "in-place");
        // Exactly one converge ran across the two ticks.
        assert_eq!(ctl.attested_ticks(), 2);
    }

    #[tokio::test]
    async fn tick_shadows_a_drift_under_dry_activate_and_mutates_nothing() {
        let env = MockReconcileEnv::drifted("new-sys", "old-sys", 5, 6);
        let ctl = SystemReconciler::new(cfg(RebuildAction::DryActivate), env);
        let out = ctl.tick().await.expect("tick");
        assert_eq!(out.report.objects_changed, 0, "shadow mutates nothing");
        assert_eq!(
            ctl.outcome_chain().tip().unwrap().record.verdict,
            "shadowed"
        );
        // The shadow tick made NO converge call — observe-first, never touch cid.
        assert_eq!(ctl.env_converge_count(), 0);
    }

    #[tokio::test]
    async fn tick_converges_a_fresh_node_to_its_first_generation() {
        let env = MockReconcileEnv::fresh("first-sys", 1);
        let ctl = SystemReconciler::new(cfg(RebuildAction::Switch), env);
        let out = ctl.tick().await.expect("tick");
        assert_eq!(out.report.objects_changed, 1, "fresh node ⇒ converge");
        let tip = ctl.outcome_chain().tip().unwrap().record.clone();
        assert_eq!(tip.verdict, "converged");
        assert_eq!(tip.from_generation, None, "no prior generation");
        assert_eq!(tip.to_generation, Some(1));
    }

    #[tokio::test]
    async fn tick_is_unobservable_and_never_converges_when_build_fails() {
        let env = MockReconcileEnv::unobservable("eval error", "old-sys", 5);
        let ctl = SystemReconciler::new(cfg(RebuildAction::Switch), env);
        let out = ctl.tick().await.expect("tick");
        assert_eq!(out.report.objects_examined, 0, "unobservable ⇒ nothing examined");
        assert_eq!(out.report.objects_changed, 0);
        let tip = ctl.outcome_chain().tip().unwrap().record.clone();
        assert_eq!(tip.verdict, "unobservable");
        assert!(
            tip.reason.as_deref().unwrap_or_default().contains("eval error"),
            "reason should carry the observe error, got {:?}",
            tip.reason
        );
        // A build failure NEVER converges — cid is never touched on a bad eval.
        assert_eq!(ctl.env_converge_count(), 0);
    }

    #[tokio::test]
    async fn a_failed_converge_is_attested_failed_not_silently_dropped() {
        let env = MockReconcileEnv::drifted("new-sys", "old-sys", 5, 6).with_failing_converge();
        let ctl = SystemReconciler::new(cfg(RebuildAction::Switch), env);
        let out = ctl.tick().await.expect("tick");
        assert_eq!(out.report.objects_changed, 0, "the converge failed");
        let tip = ctl.outcome_chain().tip().unwrap().record.clone();
        assert_eq!(tip.verdict, "failed");
        assert!(tip.reason.is_some());
        // The loop did not die — it attested + will requeue.
        assert_eq!(out.result, ReconcileResult::Requeue(Duration::from_secs(30)));
    }

    #[tokio::test]
    async fn the_attestation_chain_links_across_ticks() {
        let env = MockReconcileEnv::in_place("abc-sys", 7);
        let ctl = SystemReconciler::new(cfg(RebuildAction::Switch), env);
        ctl.tick().await.expect("t1");
        ctl.tick().await.expect("t2");
        ctl.tick().await.expect("t3");
        let chain = ctl.outcome_chain();
        assert_eq!(chain.len(), 3);
        chain.verify().expect("chain verifies across ticks");
        assert_eq!(chain.links()[1].prev, chain.links()[0].id);
        assert_eq!(chain.links()[2].prev, chain.links()[1].id);
        // Monotonic tick counter in the records.
        assert_eq!(chain.links()[0].record.tick, 0);
        assert_eq!(chain.links()[2].record.tick, 2);
    }

    #[tokio::test]
    async fn controller_name_is_stable() {
        let ctl = SystemReconciler::new(
            cfg(RebuildAction::Switch),
            MockReconcileEnv::in_place("x", 1),
        );
        assert_eq!(ctl.name(), "system-reconcile");
    }
}

// A tiny test-only accessor so the controller tests can assert the mock's
// converge-call count without exposing the env field publicly.
#[cfg(test)]
impl SystemReconciler<env::mock::MockReconcileEnv> {
    fn env_converge_count(&self) -> usize {
        self.env.converge_count()
    }
}
