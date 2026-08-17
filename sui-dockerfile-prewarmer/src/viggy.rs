//! `viggy` — the **layers-stay-warm** closed loop: the prewarmer promoted
//! from a bare `tokio::interval` poll to a `(defpromessa layers-stay-warm)`
//! `PromessaController` running the Viggy **seven-beat** tick.
//!
//! ## What this is (and what it replaces)
//!
//! The shipped prewarmer ([`crate::run_poll_loop`], still present as the
//! interim) is a bare loop: tick → [`run_cycle`] → log. It *warms* but it
//! never *proves it is holding a promise* — there is no promessa, no
//! seven-beat structure, no attestation. This module is the destination:
//! the poll trigger stays (a `tokio::interval` still paces the loop), but
//! the tick body becomes the Viggy seven-beat over the same mockable
//! side-effect seams ([`CommitsApi`] + [`PrewarmRunner`]):
//!
//! ```text
//! Observe ─ read the seen/warm state of every watched graph ──▶ WatchObservation
//!   │        (each entry's HEAD sha via CommitsApi vs the recorded seen-sha)
//! Diff    ─ classify each graph warm | cold ──────────────────▶ WatchDiff
//! Classify─ compute the seen-ratio (warm / classifiable) ─────▶ SeenRatio
//! Decide  ─ LayersStayWarm.evaluate(ratio) + build the warm Dag▶ WarmthEvaluation + Dag
//!   │        (a shigoto::Dag of typed re-warm Jobs — NOT a bare loop)
//! Act     ─ drive the Dag's waves through PrewarmRunner ───────▶ WarmAct
//! Attest  ─ append the tick to a BLAKE3 OutcomeChain ──────────▶ OutcomeLink id
//! Tick    ─ Requeue(poll_interval) ───────────────────────────▶ ReconcileOutcome
//! ```
//!
//! ## REUSE, never re-roll (the instruction, honored)
//!
//! - The **seven-beat contract** — the [`Controller`] trait, the
//!   [`ReconcileOutcome`] / [`ReconcileResult`] / [`ReconcileReport`]
//!   requeue shape — is **taken verbatim from
//!   [`sui_supercacheci::controller`]** (the `SuperCacheCiController`
//!   pattern). This loop does NOT define a second PromessaController
//!   contract.
//! - The **promessa verdict vocabulary** — [`WarmthVerdict`] /
//!   [`WarmthBreach`] — is **taken verbatim from
//!   [`sui_supercacheci::preheat`]** (the `WarmthPromessa` pattern). Only
//!   the *predicate* differs: preheat's promessa observes a `PreheatPlan`
//!   over closure targets; this crate's observable is the **seen-ratio
//!   over watched Dockerfile graphs**, which a `PreheatPlan` cannot
//!   express — so [`LayersStayWarm`] mirrors `WarmthPromessa`'s exact
//!   shape (name + target + `evaluate → WarmthEvaluation`) over its own
//!   domain rather than misusing preheat's target-shaped one.
//! - The **warm STEP** is a [`shigoto_dag::Dag`] of typed
//!   [`shigoto_types::JobId`]s (`scope=Workspace("prewarm")`,
//!   `kind=rewarm-graph`, `subject=Pinned(image_tag)`), not an ad-hoc
//!   loop.
//! - Every side effect stays behind the existing mockable seams; the
//!   seven-beat brain is pure over an injected observation, so the whole
//!   loop is unit-tested with `MockCommitsApi` + `RecordingPrewarmRunner`
//!   and **no network, docker, or cache**.
//!
//! ## Tier-honest (never round up)
//!
//! - **Shipped (this module):** the promessa + seen-ratio math + the
//!   seven-beat tick + the shigoto warm Dag + the BLAKE3 attestation, all
//!   correct-by-test over the mock seams.
//! - **Attestation tier:** the OutcomeChain is a **content-addressed
//!   BLAKE3 hash chain, NOT an Ed25519-signed chain** — tamper-evident +
//!   append-only, but not externally verifiable. See
//!   [`crate::outcome_chain`] for the tier table + the named signing
//!   destination.
//! - **Engenho-bind:** the [`Controller`] trait is the shape-identical
//!   mirror `sui-supercacheci` already carries (sui cannot depend on the
//!   heavy `engenho-controllers` closure); binding the live
//!   `engenho_controllers::Controller` is the same named destination the
//!   supercacheci module documents, shared once when that trait is
//!   extracted to a leaf crate.

use std::collections::HashSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use shigoto_dag::Dag;
use shigoto_types::{JobId, JobKindId, JobScope, JobSubject};

// REUSE — the seven-beat contract, verbatim from the super-cache-ci controller.
pub use sui_supercacheci::controller::{
    Controller, ReconcileOutcome, ReconcileReport, ReconcileResult,
};
// REUSE — the promessa verdict vocabulary, verbatim from the preheat WarmthPromessa.
pub use sui_supercacheci::preheat::{WarmthBreach, WarmthEvaluation, WarmthVerdict};

use crate::config::WatchedDockerfile;
use crate::github::CommitsApi;
use crate::outcome_chain::{OutcomeChain, OutcomeRecord};
use crate::prewarm::PrewarmRunner;
use crate::PollState;

/// The default requeue interval when a config poll interval is not
/// supplied — mirrors the prescribed 15-minute prewarmer cadence.
pub const DEFAULT_REQUEUE_SECS: u64 = 15 * 60;

/// The default seen-ratio objective in basis points — `0.99` warm.
pub const DEFAULT_SEEN_RATIO_TARGET_BPS: u16 = 9_900;

// ───────────────────────────────────────────────────────────────────────────
// The (defpromessa layers-stay-warm) — mirrors WarmthPromessa's shape
// ───────────────────────────────────────────────────────────────────────────

/// The Viggy `(defpromessa)` **"the watched layers stay warm"** as a typed
/// outcome value — the twin of `sui-supercacheci`'s `WarmthPromessa`, over
/// the prewarmer's own observable. The one business predicate the loop
/// proves it is holding tick by tick: **at least `seen_ratio_target` of the
/// classifiable watched graphs are warm** (their current HEAD closure is
/// pre-built in the cache).
///
/// It reuses the `WarmthVerdict` / `WarmthBreach` vocabulary; only the
/// predicate is prewarmer-specific (`WarmthPromessa` evaluates a
/// `PreheatPlan` over closure targets, which cannot see Dockerfile
/// seen-state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayersStayWarm {
    /// The promessa name (`layers-stay-warm`).
    pub name: String,
    /// The seen-ratio objective in basis points (0..=10_000). At least
    /// this fraction of classifiable graphs must be warm for the
    /// promessa to hold.
    pub seen_ratio_target_bps: u16,
}

impl Default for LayersStayWarm {
    fn default() -> Self {
        Self {
            name: "layers-stay-warm".to_string(),
            seen_ratio_target_bps: DEFAULT_SEEN_RATIO_TARGET_BPS,
        }
    }
}

impl LayersStayWarm {
    /// The promessa with an explicit name + a target expressed as a
    /// fraction (clamped to `[0.0, 1.0]` and converted to basis points).
    #[must_use]
    pub fn new(name: impl Into<String>, target_ratio: f64) -> Self {
        let clamped = target_ratio.clamp(0.0, 1.0);
        // round-nearest into basis points; total + lossless-enough (the
        // attested value is always the exact integer bps, never a float).
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bps = (clamped * 10_000.0).round() as u16;
        Self {
            name: name.into(),
            seen_ratio_target_bps: bps,
        }
    }

    /// The objective as a fraction in `[0.0, 1.0]`, for display/logs.
    #[must_use]
    pub fn target_ratio(&self) -> f64 {
        f64::from(self.seen_ratio_target_bps) / 10_000.0
    }

    /// **Evaluate the promessa against an observed seen-ratio.** Held iff:
    ///
    /// 1. **classifiability** — at least one graph is classifiable
    ///    (`classifiable > 0`); nothing classifiable ⇒ warmth is vacuous
    ///    and *not* claimed — [`Breached`](WarmthVerdict::Breached) with
    ///    [`NothingClassifiable`](WarmthBreach::NothingClassifiable), never
    ///    a rounded-up Held (the same honest floor `WarmthPromessa` holds).
    /// 2. **seen-ratio** — `ratio.bps() >= seen_ratio_target_bps`;
    ///    otherwise [`WarmFractionLow`](WarmthBreach::WarmFractionLow).
    #[must_use]
    pub fn evaluate(&self, ratio: SeenRatio) -> WarmthEvaluation {
        if ratio.classifiable == 0 {
            return WarmthEvaluation {
                verdict: WarmthVerdict::Breached,
                breach: Some(WarmthBreach::NothingClassifiable),
            };
        }
        if ratio.bps() < self.seen_ratio_target_bps {
            return WarmthEvaluation {
                verdict: WarmthVerdict::Breached,
                breach: Some(WarmthBreach::WarmFractionLow),
            };
        }
        WarmthEvaluation {
            verdict: WarmthVerdict::Held,
            breach: None,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The seen-ratio — the Classify beat's pure product
// ───────────────────────────────────────────────────────────────────────────

/// The warm seen-ratio over the watched graphs: `warm / classifiable`,
/// carried as exact integer counts so the attested value is never a lossy
/// float. A graph is **warm** iff its current HEAD sha is the recorded
/// seen-sha (i.e. its current closure was pre-built into the cache);
/// **cold** otherwise; **unclassifiable** iff its HEAD could not be
/// observed this tick (a GitHub error — never counted as warm *or* cold,
/// so an API outage cannot silently satisfy or fail the promessa).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SeenRatio {
    /// Graphs observed warm (current HEAD == recorded seen-sha).
    pub warm: usize,
    /// Graphs observed cold (current HEAD != recorded seen-sha).
    pub cold: usize,
    /// Graphs classifiable this tick (`warm + cold`); excludes ones whose
    /// HEAD could not be observed.
    pub classifiable: usize,
}

impl SeenRatio {
    /// The ratio in basis points (0..=10_000). `0` when nothing is
    /// classifiable (never a divide-by-zero, never a NaN).
    #[must_use]
    pub fn bps(&self) -> u16 {
        if self.classifiable == 0 {
            return 0;
        }
        // warm <= classifiable so warm*10_000 fits in the arithmetic and
        // the quotient is <= 10_000; the cast is always in range.
        let bps = (self.warm * 10_000) / self.classifiable;
        u16::try_from(bps).unwrap_or(10_000)
    }

    /// The ratio as a fraction in `[0.0, 1.0]`, for display/logs.
    #[must_use]
    pub fn as_fraction(&self) -> f64 {
        f64::from(self.bps()) / 10_000.0
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The Observe/Diff beats — a per-graph warm/cold classification
// ───────────────────────────────────────────────────────────────────────────

/// One watched graph's observed warmth this tick — the Observe+Diff
/// product for a single entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphWarmth {
    /// The watched graph.
    pub entry: WatchedDockerfile,
    /// The classification: warm / cold(new HEAD to warm) / unobservable.
    pub state: WarmState,
}

/// The typed warmth classification for one watched graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarmState {
    /// Current HEAD == recorded seen-sha — the closure is pre-built.
    Warm { sha: String },
    /// Current HEAD != recorded seen-sha (or never seen) — needs re-warm.
    Cold { new_sha: String },
    /// HEAD could not be observed this tick (a GitHub error). Excluded
    /// from the ratio — never treated as warm or cold.
    Unobservable { error: String },
}

impl WarmState {
    /// Whether this graph is warm.
    #[must_use]
    pub fn is_warm(&self) -> bool {
        matches!(self, Self::Warm { .. })
    }
    /// Whether this graph is cold (needs a re-warm).
    #[must_use]
    pub fn is_cold(&self) -> bool {
        matches!(self, Self::Cold { .. })
    }
}

/// The Observe+Diff product over the whole watched list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchObservation {
    /// Per-graph warmth, one entry per watched graph.
    pub graphs: Vec<GraphWarmth>,
}

impl WatchObservation {
    /// The Classify beat — the seen-ratio over the observed graphs.
    #[must_use]
    pub fn seen_ratio(&self) -> SeenRatio {
        let mut r = SeenRatio::default();
        for g in &self.graphs {
            match &g.state {
                WarmState::Warm { .. } => {
                    r.warm += 1;
                    r.classifiable += 1;
                }
                WarmState::Cold { .. } => {
                    r.cold += 1;
                    r.classifiable += 1;
                }
                WarmState::Unobservable { .. } => {}
            }
        }
        r
    }

    /// The cold graphs — the Decide beat's re-warm work set.
    #[must_use]
    pub fn cold_graphs(&self) -> Vec<&GraphWarmth> {
        self.graphs.iter().filter(|g| g.state.is_cold()).collect()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The Decide beat — the warm work as a typed shigoto Dag (not a bare loop)
// ───────────────────────────────────────────────────────────────────────────

/// The typed `JobKindId` every re-warm Job carries.
const REWARM_KIND: &str = "rewarm-graph";

/// The `JobScope` the prewarmer's re-warm Jobs live under.
fn warm_scope() -> JobScope {
    JobScope::Workspace("prewarm".to_string())
}

/// The typed `JobId` for re-warming one watched graph — stable across
/// ticks (keyed by the graph's image tag), so the same graph maps to the
/// same Job every tick.
#[must_use]
pub fn rewarm_job_id(entry: &WatchedDockerfile) -> JobId {
    JobId {
        scope: warm_scope(),
        kind: JobKindId::new(REWARM_KIND),
        subject: JobSubject::Pinned(entry.image_tag.clone()),
    }
}

/// Build the re-warm work as a [`shigoto_dag::Dag`] of typed Jobs — one
/// node per cold graph. Prewarm Jobs are independent (each graph is
/// self-contained, no cross-graph ordering), so the Dag is a single wave;
/// the Dag is still the typed carrier so the warm STEP is a work-graph
/// value, not ad-hoc control flow — and the shape is ready for the day a
/// base→derived graph edge is introduced (then the wave count grows and
/// [`Dag::waves`] sequences them for free).
///
/// Returns the Dag plus the parallel `Vec` of (JobId, cold-entry) so the
/// Act beat can look each wave's Job up to its watched entry + new sha.
#[must_use]
pub fn build_warm_dag(observation: &WatchObservation) -> WarmDag {
    let mut dag = Dag::new();
    let mut jobs = Vec::new();
    for g in observation.cold_graphs() {
        let WarmState::Cold { new_sha } = &g.state else {
            continue;
        };
        let id = rewarm_job_id(&g.entry);
        dag.ensure_node(id.clone());
        jobs.push(WarmJob {
            id,
            entry: g.entry.clone(),
            new_sha: new_sha.clone(),
        });
    }
    WarmDag { dag, jobs }
}

/// One typed re-warm Job — the JobId plus the payload the Act beat needs
/// to run it.
#[derive(Debug, Clone)]
pub struct WarmJob {
    /// The typed identity in the Dag.
    pub id: JobId,
    /// The watched graph to re-warm.
    pub entry: WatchedDockerfile,
    /// The new HEAD sha whose closure to build + commit on success.
    pub new_sha: String,
}

/// The Decide beat's product — the warm Dag + its Jobs' payloads.
pub struct WarmDag {
    /// The typed topology (nodes = cold graphs; single wave today).
    pub dag: Dag,
    /// The per-node payloads, indexed by JobId equality.
    pub jobs: Vec<WarmJob>,
}

impl WarmDag {
    /// The re-warm work set size (== cold-graph count).
    #[must_use]
    pub fn len(&self) -> usize {
        self.jobs.len()
    }
    /// Whether there is any re-warm work this tick.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// The Dag's topological waves over the re-warm nodes — the order the
    /// Act beat drives them in. Errors only on a cycle, which the
    /// prewarmer's edge-free Dag cannot construct (kept as a typed error
    /// so a future edged Dag surfaces a malformed graph, never a panic).
    ///
    /// # Errors
    ///
    /// [`shigoto_dag::DagError::Cycle`] if a future edge set is cyclic.
    pub fn waves(&self) -> Result<Vec<Vec<JobId>>, shigoto_dag::DagError> {
        let affected: HashSet<JobId> = self.jobs.iter().map(|j| j.id.clone()).collect();
        self.dag.waves(Some(&affected))
    }

    /// Look a JobId up to its payload.
    #[must_use]
    pub fn job(&self, id: &JobId) -> Option<&WarmJob> {
        self.jobs.iter().find(|j| &j.id == id)
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The Act beat's product
// ───────────────────────────────────────────────────────────────────────────

/// What the Act beat applied this tick — how many cold graphs re-warmed
/// and how many re-warm attempts failed (typed, never a silent drop).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WarmAct {
    /// Cold graphs successfully re-warmed (cache back-filled, sha committed).
    pub rewarmed: usize,
    /// Re-warm attempts that failed (github/wrapper error) — retried next tick.
    pub failed: usize,
    /// Whether any re-warm STEP actually ran this tick.
    pub acted: bool,
}

// ───────────────────────────────────────────────────────────────────────────
// The controller — the PromessaController running the seven-beat tick
// ───────────────────────────────────────────────────────────────────────────

/// The `layers-stay-warm` PromessaController — the prewarmer as a Viggy
/// seven-beat loop. Holds the promessa + the injected side-effect seams
/// ([`CommitsApi`] + [`PrewarmRunner`]) + the cross-tick state
/// ([`PollState`] + the [`OutcomeChain`]). Interior mutability keeps the
/// [`Controller::tick`] signature (`&self`) identical to the super-cache-ci
/// controller it mirrors.
pub struct LayersWarmController<A: CommitsApi, P: PrewarmRunner> {
    promessa: LayersStayWarm,
    watched: Vec<WatchedDockerfile>,
    api: A,
    runner: P,
    requeue: Duration,
    state: std::sync::Mutex<PollState>,
    chain: std::sync::Mutex<OutcomeChain>,
    tick_counter: std::sync::atomic::AtomicU64,
}

impl<A: CommitsApi, P: PrewarmRunner> LayersWarmController<A, P> {
    /// Build the controller from the promessa, the watched list, the two
    /// side-effect seams, and the requeue interval.
    #[must_use]
    pub fn new(
        promessa: LayersStayWarm,
        watched: Vec<WatchedDockerfile>,
        api: A,
        runner: P,
        requeue: Duration,
    ) -> Self {
        Self {
            promessa,
            watched,
            api,
            runner,
            requeue,
            state: std::sync::Mutex::new(PollState::new()),
            chain: std::sync::Mutex::new(OutcomeChain::new()),
            tick_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The promessa this controller proves it is holding.
    #[must_use]
    pub fn promessa(&self) -> &LayersStayWarm {
        &self.promessa
    }

    /// The attestation chain's current tip id — the head of proof.
    ///
    /// # Panics
    ///
    /// Only if the internal mutex is poisoned by a prior panic in another
    /// thread (unreachable in the single-threaded tick loop).
    #[must_use]
    pub fn chain_head(&self) -> [u8; 32] {
        self.chain.lock().unwrap().head()
    }

    /// The number of attested ticks so far.
    ///
    /// # Panics
    ///
    /// Only on a poisoned mutex (unreachable in the tick loop).
    #[must_use]
    pub fn attested_ticks(&self) -> usize {
        self.chain.lock().unwrap().len()
    }

    /// A clone of the attestation chain — for verification/inspection.
    ///
    /// # Panics
    ///
    /// Only on a poisoned mutex (unreachable in the tick loop).
    #[must_use]
    pub fn outcome_chain(&self) -> OutcomeChain {
        self.chain.lock().unwrap().clone()
    }

    /// **Beat 1 (Observe) + Beat 2 (Diff).** Read every watched graph's
    /// current HEAD sha via the [`CommitsApi`] seam and diff it against the
    /// recorded seen-sha to classify warm / cold / unobservable. Pure over
    /// the seam + a `PollState` snapshot — no mutation, no re-warm.
    async fn observe(&self, state: &PollState) -> WatchObservation {
        let mut graphs = Vec::with_capacity(self.watched.len());
        for entry in &self.watched {
            let state_v = match self
                .api
                .latest_commit_sha(&entry.owner, &entry.repo, &entry.git_ref, &entry.path)
                .await
            {
                Ok(sha) => match state.last_seen(entry) {
                    Some(seen) if seen == sha => WarmState::Warm { sha },
                    _ => WarmState::Cold { new_sha: sha },
                },
                Err(err) => WarmState::Unobservable {
                    error: err.to_string(),
                },
            };
            graphs.push(GraphWarmth {
                entry: entry.clone(),
                state: state_v,
            });
        }
        WatchObservation { graphs }
    }

    /// **Beat 5 (Act).** Drive the warm Dag's waves through the
    /// [`PrewarmRunner`] seam: for each cold graph, fetch content + run the
    /// wrapper (the pre-warm), committing the new sha into `state` only on
    /// success (so a failed re-warm retries next tick — the same
    /// never-mark-seen-on-failure contract [`crate::run_cycle`] holds).
    async fn act(&self, warm: &WarmDag, state: &mut PollState) -> WarmAct {
        let mut result = WarmAct::default();
        // A cache coordinator with no cold work does nothing — shadow-quiet.
        let Ok(waves) = warm.waves() else {
            // A cyclic Dag is unconstructible today; treat the impossible
            // case as "no work applied" rather than panicking.
            return result;
        };
        for wave in waves {
            for job_id in wave {
                let Some(job) = warm.job(&job_id) else {
                    continue;
                };
                result.acted = true;
                match self
                    .api
                    .fetch_raw_content(&job.entry.owner, &job.entry.repo, &job.new_sha, &job.entry.path)
                    .await
                {
                    Ok(content) => {
                        match self
                            .runner
                            .prewarm(&job.entry.path, &content, &job.entry.image_tag)
                            .await
                        {
                            Ok(_receipt) => {
                                state.record(&job.entry, job.new_sha.clone());
                                result.rewarmed += 1;
                            }
                            Err(_err) => result.failed += 1,
                        }
                    }
                    Err(_err) => result.failed += 1,
                }
            }
        }
        result
    }

    /// **Beat 6 (Attest).** Append the tick's evaluation to the BLAKE3
    /// OutcomeChain and return the new link id (the head of proof).
    fn attest(
        &self,
        chain: &mut OutcomeChain,
        ratio: SeenRatio,
        eval: WarmthEvaluation,
        act: &WarmAct,
    ) -> [u8; 32] {
        let tick = self
            .tick_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let record = OutcomeRecord {
            promessa: self.promessa.name.clone(),
            tick,
            seen_ratio_bps: ratio.bps(),
            verdict: verdict_str(eval.verdict).to_string(),
            breach: eval.breach.map(|b| breach_str(b).to_string()),
            rewarmed: act.rewarmed,
        };
        chain.append(record)
    }
}

/// Stable string for a verdict — the sanctioned typed surface for the
/// attested value (no `format!()`; a fixed enum → `&'static str`).
fn verdict_str(v: WarmthVerdict) -> &'static str {
    match v {
        WarmthVerdict::Held => "held",
        WarmthVerdict::Breached => "breached",
    }
}

/// Stable string for a breach reason.
fn breach_str(b: WarmthBreach) -> &'static str {
    match b {
        WarmthBreach::NothingClassifiable => "nothing_classifiable",
        WarmthBreach::WarmFractionLow => "seen_ratio_low",
        WarmthBreach::Stale => "stale",
        WarmthBreach::FloorNotZeroAtRest => "floor_not_zero_at_rest",
    }
}

#[async_trait::async_trait]
impl<A: CommitsApi, P: PrewarmRunner> Controller for LayersWarmController<A, P> {
    fn name(&self) -> &'static str {
        "layers-stay-warm"
    }

    async fn tick(&self) -> Result<ReconcileOutcome, sui_supercacheci::controller::ControllerError> {
        // Snapshot the cross-tick state under the lock, then release it
        // across the awaits (the mutex guards the value, not the I/O).
        let mut state = { self.state.lock().unwrap().clone() };

        // Beat 1+2 — Observe + Diff.
        let observation = self.observe(&state).await;
        // Beat 3 — Classify: the seen-ratio.
        let ratio = observation.seen_ratio();
        // Beat 4 — Decide: evaluate the promessa (its state BEFORE this
        // tick's work) + build the warm Dag of re-warm Jobs.
        let pre_eval = self.promessa.evaluate(ratio);
        tracing::debug!(
            promessa = %self.promessa.name,
            pre_verdict = verdict_str(pre_eval.verdict),
            pre_seen_bps = ratio.bps(),
            "layers-stay-warm: decide beat"
        );
        let warm = build_warm_dag(&observation);
        // Beat 5 — Act: drive the Dag's waves (re-warm the cold graphs).
        let act = self.act(&warm, &mut state).await;
        // Beat 6 — Attest: append to the OutcomeChain. Re-evaluate the
        // *post-act* ratio so the attested verdict reflects this tick's
        // work (a cold graph re-warmed this tick is now warm).
        let post_ratio = post_act_ratio(ratio, &act);
        let post_eval = self.promessa.evaluate(post_ratio);
        let head = {
            let mut chain = self.chain.lock().unwrap();
            self.attest(&mut chain, post_ratio, post_eval, &act)
        };

        // Commit the advanced state back for the next tick.
        {
            *self.state.lock().unwrap() = state;
        }

        let report = tick_report(&observation, ratio, post_eval, &warm, &act, head);

        // Beat 7 — Tick: a warmth promessa is never one-shot Done.
        Ok(ReconcileOutcome::new(
            report,
            ReconcileResult::Requeue(self.requeue),
        ))
    }
}

/// The post-Act seen-ratio: every graph the Act beat re-warmed moves from
/// cold to warm (its current closure is now pre-built). Pure — recomputed
/// from the pre-Act ratio + the applied count so the attested value
/// reflects this tick's work without a second Observe round-trip.
fn post_act_ratio(pre: SeenRatio, act: &WarmAct) -> SeenRatio {
    // Never claim more warm than was cold; classifiable is unchanged.
    let moved = act.rewarmed.min(pre.cold);
    SeenRatio {
        warm: pre.warm + moved,
        cold: pre.cold - moved,
        classifiable: pre.classifiable,
    }
}

/// Build the seven-beat tick report — the typed `ReconcileReport` shape
/// (examined / changed / skipped + a note) the super-cache-ci controller
/// uses. `examined` = classifiable graphs; `changed` = re-warmed;
/// `skipped` = examined − changed.
fn tick_report(
    observation: &WatchObservation,
    pre_ratio: SeenRatio,
    eval: WarmthEvaluation,
    warm: &WarmDag,
    act: &WarmAct,
    head: [u8; 32],
) -> ReconcileReport {
    let examined = pre_ratio.classifiable;
    let changed = act.rewarmed;
    ReconcileReport {
        objects_examined: examined,
        objects_changed: changed,
        objects_skipped: examined.saturating_sub(changed),
        note: Some(
            TickNote {
                verdict: eval.verdict,
                seen_bps: pre_ratio.bps(),
                cold: warm.len(),
                rewarmed: act.rewarmed,
                failed: act.failed,
                unobservable: observation
                    .graphs
                    .iter()
                    .filter(|g| matches!(g.state, WarmState::Unobservable { .. }))
                    .count(),
                head,
            }
            .to_string(),
        ),
    }
}

/// A typed one-line tick note — the sanctioned typed-emission surface for
/// the report note (`write!` inside `Display`, never `format!()`).
struct TickNote {
    verdict: WarmthVerdict,
    seen_bps: u16,
    cold: usize,
    rewarmed: usize,
    failed: usize,
    unobservable: usize,
    head: [u8; 32],
}

impl std::fmt::Display for TickNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} seen_bps={} cold={} rewarmed={} failed={} unobservable={} attest={}",
            verdict_str(self.verdict),
            self.seen_bps,
            self.cold,
            self.rewarmed,
            self.failed,
            self.unobservable,
            data_encoding::HEXLOWER.encode(&self.head[..8]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::mock::MockCommitsApi;
    use crate::prewarm::mock::RecordingPrewarmRunner;

    fn watched(path: &str, tag: &str) -> WatchedDockerfile {
        WatchedDockerfile {
            owner: "example-org".to_string(),
            repo: "example-images".to_string(),
            git_ref: "master".to_string(),
            path: path.to_string(),
            image_tag: tag.to_string(),
        }
    }

    // ── the promessa (defpromessa layers-stay-warm) — seen-ratio math ──────

    #[test]
    fn seen_ratio_bps_is_exact_integer_never_a_float() {
        let r = SeenRatio { warm: 99, cold: 1, classifiable: 100 };
        assert_eq!(r.bps(), 9_900);
        assert!((r.as_fraction() - 0.99).abs() < 1e-9);
    }

    #[test]
    fn seen_ratio_all_warm_is_ten_thousand_bps() {
        let r = SeenRatio { warm: 3, cold: 0, classifiable: 3 };
        assert_eq!(r.bps(), 10_000);
    }

    #[test]
    fn seen_ratio_nothing_classifiable_is_zero_bps_never_divides_by_zero() {
        let r = SeenRatio { warm: 0, cold: 0, classifiable: 0 };
        assert_eq!(r.bps(), 0);
    }

    #[test]
    fn promessa_held_when_ratio_meets_the_target() {
        let p = LayersStayWarm::default(); // 0.99
        let held = p.evaluate(SeenRatio { warm: 99, cold: 1, classifiable: 100 });
        assert_eq!(held.verdict, WarmthVerdict::Held);
        assert!(held.breach.is_none());
    }

    #[test]
    fn promessa_breached_when_ratio_below_target() {
        let p = LayersStayWarm::default(); // 0.99
        let e = p.evaluate(SeenRatio { warm: 98, cold: 2, classifiable: 100 });
        assert_eq!(e.verdict, WarmthVerdict::Breached);
        assert_eq!(e.breach, Some(WarmthBreach::WarmFractionLow));
    }

    #[test]
    fn promessa_breached_and_not_rounded_up_when_nothing_classifiable() {
        // The honest floor (identical to WarmthPromessa): nothing to warm
        // ⇒ warmth is vacuous ⇒ Breached(NothingClassifiable), never Held.
        let p = LayersStayWarm::default();
        let e = p.evaluate(SeenRatio::default());
        assert_eq!(e.verdict, WarmthVerdict::Breached);
        assert_eq!(e.breach, Some(WarmthBreach::NothingClassifiable));
    }

    #[test]
    fn promessa_target_ratio_clamps_and_round_trips() {
        assert_eq!(LayersStayWarm::new("p", 0.99).seen_ratio_target_bps, 9_900);
        assert_eq!(LayersStayWarm::new("p", 1.5).seen_ratio_target_bps, 10_000);
        assert_eq!(LayersStayWarm::new("p", -0.2).seen_ratio_target_bps, 0);
        assert!((LayersStayWarm::default().target_ratio() - 0.99).abs() < 1e-9);
    }

    // ── the Observe/Diff/Classify beats over a WatchObservation ─────────────

    fn obs(states: Vec<(&str, WarmState)>) -> WatchObservation {
        WatchObservation {
            graphs: states
                .into_iter()
                .map(|(tag, state)| GraphWarmth { entry: watched("Dockerfile", tag), state })
                .collect(),
        }
    }

    #[test]
    fn observation_classifies_seen_ratio_excluding_unobservable() {
        let o = obs(vec![
            ("a", WarmState::Warm { sha: "s".into() }),
            ("b", WarmState::Warm { sha: "s".into() }),
            ("c", WarmState::Cold { new_sha: "s2".into() }),
            ("d", WarmState::Unobservable { error: "boom".into() }),
        ]);
        let r = o.seen_ratio();
        // d is excluded — an API outage cannot satisfy or fail the promessa.
        assert_eq!(r, SeenRatio { warm: 2, cold: 1, classifiable: 3 });
        assert_eq!(o.cold_graphs().len(), 1);
    }

    // ── the Decide beat — the shigoto Dag warm STEP ─────────────────────────

    #[test]
    fn warm_dag_has_one_node_per_cold_graph() {
        let o = obs(vec![
            ("a", WarmState::Warm { sha: "s".into() }),
            ("b", WarmState::Cold { new_sha: "s2".into() }),
            ("c", WarmState::Cold { new_sha: "s3".into() }),
        ]);
        let warm = build_warm_dag(&o);
        assert_eq!(warm.len(), 2, "two cold graphs ⇒ two Jobs");
        assert_eq!(warm.dag.node_count(), 2);
        assert_eq!(warm.dag.edge_count(), 0, "independent graphs ⇒ no edges");
    }

    #[test]
    fn warm_dag_waves_are_a_single_wave_of_all_cold_jobs() {
        let o = obs(vec![
            ("b", WarmState::Cold { new_sha: "s2".into() }),
            ("c", WarmState::Cold { new_sha: "s3".into() }),
        ]);
        let warm = build_warm_dag(&o);
        let waves = warm.waves().expect("edge-free dag never cycles");
        assert_eq!(waves.len(), 1, "no edges ⇒ exactly one wave");
        assert_eq!(waves[0].len(), 2, "both cold Jobs in wave 0");
        // Each JobId resolves back to its payload.
        for id in &waves[0] {
            assert!(warm.job(id).is_some());
        }
    }

    #[test]
    fn rewarm_job_id_is_stable_and_typed() {
        let id = rewarm_job_id(&watched("Dockerfile", "img:tag"));
        assert!(matches!(id.scope, JobScope::Workspace(ref w) if w == "prewarm"));
        assert_eq!(id.kind, JobKindId::new("rewarm-graph"));
        assert_eq!(id.subject, JobSubject::Pinned("img:tag".into()));
        // Same entry ⇒ same id across ticks.
        assert_eq!(id, rewarm_job_id(&watched("Dockerfile", "img:tag")));
    }

    #[test]
    fn empty_observation_yields_an_empty_warm_dag() {
        let warm = build_warm_dag(&WatchObservation::default());
        assert!(warm.is_empty());
        assert_eq!(warm.waves().unwrap().len(), 1); // one empty wave
        assert!(warm.waves().unwrap()[0].is_empty());
    }

    // ── post-Act ratio math ─────────────────────────────────────────────────

    #[test]
    fn post_act_ratio_moves_rewarmed_graphs_from_cold_to_warm() {
        let pre = SeenRatio { warm: 8, cold: 2, classifiable: 10 };
        let post = post_act_ratio(pre, &WarmAct { rewarmed: 2, failed: 0, acted: true });
        assert_eq!(post, SeenRatio { warm: 10, cold: 0, classifiable: 10 });
        assert_eq!(post.bps(), 10_000);
    }

    #[test]
    fn post_act_ratio_never_claims_more_warm_than_was_cold() {
        let pre = SeenRatio { warm: 8, cold: 1, classifiable: 10 };
        // A (bug-shaped) over-count is clamped — never > classifiable.
        let post = post_act_ratio(pre, &WarmAct { rewarmed: 5, failed: 0, acted: true });
        assert_eq!(post, SeenRatio { warm: 9, cold: 0, classifiable: 10 });
    }

    // ── the full seven-beat tick, over the mock seams ───────────────────────

    #[tokio::test]
    async fn seven_beat_tick_warms_cold_graphs_and_holds_the_promessa() {
        let a = watched("Dockerfile.a", "img:a");
        let b = watched("Dockerfile.b", "img:b");
        let api = MockCommitsApi::new()
            .with_sha(&a.owner, &a.repo, &a.git_ref, &a.path, "sha-a")
            .with_sha(&b.owner, &b.repo, &b.git_ref, &b.path, "sha-b")
            .with_content(&a.owner, &a.repo, "sha-a", &a.path, "FROM a\n")
            .with_content(&b.owner, &b.repo, "sha-b", &b.path, "FROM b\n");
        let runner = RecordingPrewarmRunner::new();
        let ctl = LayersWarmController::new(
            LayersStayWarm::default(),
            vec![a.clone(), b.clone()],
            api,
            runner,
            Duration::from_secs(60),
        );

        // First tick: both graphs are cold ⇒ both re-warmed.
        let out = ctl.tick().await.expect("tick");
        assert_eq!(out.report.objects_examined, 2, "both classifiable");
        assert_eq!(out.report.objects_changed, 2, "both re-warmed");
        assert_eq!(out.report.objects_skipped, 0);
        // A warmth promessa never terminates Done — it requeues.
        assert_eq!(out.result, ReconcileResult::Requeue(Duration::from_secs(60)));
        // Attested exactly one tick; the chain verifies.
        assert_eq!(ctl.attested_ticks(), 1);
        let chain = ctl.outcome_chain();
        chain.verify().expect("chain verifies");
        // The attested (post-act) verdict is Held at 1.0 seen-ratio.
        let tip = chain.tip().unwrap();
        assert_eq!(tip.record.verdict, "held");
        assert_eq!(tip.record.seen_ratio_bps, 10_000);
        assert_eq!(tip.record.rewarmed, 2);
        assert!(!tip.is_signed(), "tier-honest: the chain is unsigned (hash chain)");
    }

    #[tokio::test]
    async fn second_tick_is_quiet_when_nothing_changed() {
        let a = watched("Dockerfile.a", "img:a");
        let api = MockCommitsApi::new()
            .with_sha(&a.owner, &a.repo, &a.git_ref, &a.path, "sha-a")
            .with_content(&a.owner, &a.repo, "sha-a", &a.path, "FROM a\n");
        let runner = RecordingPrewarmRunner::new();
        let ctl = LayersWarmController::new(
            LayersStayWarm::default(),
            vec![a.clone()],
            api,
            runner,
            Duration::from_secs(60),
        );

        // Tick 1 — cold → warm.
        let t1 = ctl.tick().await.expect("tick 1");
        assert_eq!(t1.report.objects_changed, 1);
        // Tick 2 — HEAD unchanged, already seen ⇒ warm ⇒ no re-warm work.
        let t2 = ctl.tick().await.expect("tick 2");
        assert_eq!(t2.report.objects_examined, 1);
        assert_eq!(t2.report.objects_changed, 0, "already warm ⇒ quiet");
        assert_eq!(t2.report.objects_skipped, 1);
        // Two attested ticks; the chain still verifies + is prev-linked.
        assert_eq!(ctl.attested_ticks(), 2);
        let chain = ctl.outcome_chain();
        chain.verify().expect("chain verifies across ticks");
        assert_eq!(chain.links()[1].prev, chain.links()[0].id);
        // The 2nd tick's post-act ratio is still Held at 1.0.
        assert_eq!(chain.tip().unwrap().record.verdict, "held");
    }

    #[tokio::test]
    async fn a_github_outage_is_unobservable_not_a_false_breach() {
        // No sha configured for the entry ⇒ latest_commit_sha errors ⇒
        // the graph is Unobservable, excluded from the ratio. With zero
        // classifiable, the promessa is honestly Breached(NothingClassifiable)
        // — never a rounded-up Held, never a silent warm.
        let a = watched("Dockerfile.a", "img:a");
        let api = MockCommitsApi::new(); // no sha ⇒ NoCommits error
        let runner = RecordingPrewarmRunner::new();
        let ctl = LayersWarmController::new(
            LayersStayWarm::default(),
            vec![a],
            api,
            runner,
            Duration::from_secs(60),
        );
        let out = ctl.tick().await.expect("tick");
        assert_eq!(out.report.objects_examined, 0, "unobservable ⇒ not classifiable");
        assert_eq!(out.report.objects_changed, 0);
        let tip_note = out.report.note.unwrap();
        assert!(tip_note.contains("unobservable=1"), "note: {tip_note}");
        // The attested verdict is Breached(nothing_classifiable).
        let chain = ctl.outcome_chain();
        assert_eq!(chain.tip().unwrap().record.verdict, "breached");
        assert_eq!(
            chain.tip().unwrap().record.breach.as_deref(),
            Some("nothing_classifiable")
        );
    }

    #[tokio::test]
    async fn a_failed_prewarm_never_commits_seen_state_so_it_retries() {
        // HEAD observable (cold), but content fetch fails ⇒ the re-warm
        // fails ⇒ state is NOT committed ⇒ the graph stays cold next tick.
        let a = watched("Dockerfile.a", "img:a");
        let api = MockCommitsApi::new().with_sha(&a.owner, &a.repo, &a.git_ref, &a.path, "sha-a");
        // no with_content ⇒ fetch_raw_content errors in the Act beat.
        let runner = RecordingPrewarmRunner::new();
        let ctl = LayersWarmController::new(
            LayersStayWarm::default(),
            vec![a],
            api,
            runner,
            Duration::from_secs(60),
        );
        let out = ctl.tick().await.expect("tick");
        assert_eq!(out.report.objects_examined, 1, "cold ⇒ classifiable");
        assert_eq!(out.report.objects_changed, 0, "the re-warm failed");
        let note = out.report.note.unwrap();
        assert!(note.contains("failed=1"), "note: {note}");
        // Attested with the pre-act (still-cold) verdict: Breached(seen_ratio_low).
        let chain = ctl.outcome_chain();
        assert_eq!(chain.tip().unwrap().record.verdict, "breached");
        assert_eq!(chain.tip().unwrap().record.seen_ratio_bps, 0);
    }

    #[tokio::test]
    async fn controller_name_is_the_promessa_name() {
        let ctl = LayersWarmController::new(
            LayersStayWarm::default(),
            Vec::new(),
            MockCommitsApi::new(),
            RecordingPrewarmRunner::new(),
            Duration::from_secs(60),
        );
        assert_eq!(ctl.name(), "layers-stay-warm");
        assert_eq!(ctl.promessa().name, "layers-stay-warm");
    }

    #[test]
    fn breach_and_verdict_strings_are_stable() {
        assert_eq!(verdict_str(WarmthVerdict::Held), "held");
        assert_eq!(verdict_str(WarmthVerdict::Breached), "breached");
        assert_eq!(breach_str(WarmthBreach::WarmFractionLow), "seen_ratio_low");
        assert_eq!(breach_str(WarmthBreach::NothingClassifiable), "nothing_classifiable");
    }
}
