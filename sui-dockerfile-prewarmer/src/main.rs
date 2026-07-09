//! `sui-dockerfile-prewarmer` — the standing pre-warming service
//! binary.
//!
//! ## The layers-stay-warm Viggy loop (the destination — default)
//!
//! The binary runs the `(defpromessa layers-stay-warm)` PromessaController
//! ([`sui_dockerfile_prewarmer::viggy::LayersWarmController`]) on a
//! `tokio::time::interval`: the poll trigger stays, but each interval tick
//! runs the Viggy **seven-beat** (Observe → Diff → Classify → Decide → Act
//! via a shigoto Dag → Attest to a BLAKE3 OutcomeChain → Tick), proving —
//! tick by tick, with an attested seen-ratio — that the watched layers stay
//! warm. This replaces the bare poll loop that merely warmed without
//! proving a promise.
//!
//! The interim [`run_poll_loop`] (bare cycle + logs, no promessa, no
//! attestation) is retained below only as the honest interim reference; the
//! binary no longer runs it.

use std::time::Duration;

use sui_dockerfile_prewarmer::config::PrewarmerConfig;
use sui_dockerfile_prewarmer::github::RealCommitsApi;
use sui_dockerfile_prewarmer::prewarm::RealPrewarmRunner;
use sui_dockerfile_prewarmer::viggy::{Controller, LayersStayWarm, LayersWarmController};
use sui_dockerfile_prewarmer::{run_cycle, CheckOutcome, PollState, PrewarmOutcome};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();

    let config = match PrewarmerConfig::discover_and_load() {
        Some(Ok(config)) => config,
        Some(Err(err)) => {
            tracing::error!(error = %err, "failed to load sui-dockerfile-prewarmer config");
            return Err(err.into());
        }
        None => {
            tracing::warn!("no sui-dockerfile-prewarmer config found; running with zero watched dockerfiles");
            sui_dockerfile_prewarmer::config::RawPrewarmerConfig::default()
                .validate()
                .expect("the default config always validates")
        }
    };

    let promessa = LayersStayWarm::default();
    tracing::info!(
        watched_count = config.watched.len(),
        poll_interval_secs = config.poll_interval_secs,
        promessa = %promessa.name,
        seen_ratio_target = promessa.target_ratio(),
        "starting sui-dockerfile-prewarmer (layers-stay-warm Viggy loop)"
    );

    let commits_api = RealCommitsApi::new(config.github_api_base.clone(), config.github_raw_base.clone());
    let cache = sui_cache::storage::build_backend(&config.cache_backend).await?;
    let prewarm_runner = RealPrewarmRunner::new(cache, std::env::current_dir()?);

    let controller = LayersWarmController::new(
        promessa,
        config.watched.clone(),
        commits_api,
        prewarm_runner,
        Duration::from_secs(config.poll_interval_secs),
    );

    run_viggy_loop(&controller, config.poll_interval_secs).await;
    Ok(())
}

/// The standing seven-beat loop: tick every `poll_interval_secs`, run one
/// Viggy reconcile tick, emit the typed tick report + the attestation head.
/// Runs forever — the process is the deploy unit's whole lifecycle.
async fn run_viggy_loop<A, P>(controller: &LayersWarmController<A, P>, poll_interval_secs: u64)
where
    A: sui_dockerfile_prewarmer::github::CommitsApi,
    P: sui_dockerfile_prewarmer::prewarm::PrewarmRunner,
{
    let mut interval = tokio::time::interval(Duration::from_secs(poll_interval_secs));
    loop {
        interval.tick().await;
        match controller.tick().await {
            Ok(outcome) => {
                tracing::info!(
                    examined = outcome.report.objects_examined,
                    rewarmed = outcome.report.objects_changed,
                    skipped = outcome.report.objects_skipped,
                    attested_ticks = controller.attested_ticks(),
                    note = outcome.report.note.as_deref().unwrap_or(""),
                    "layers-stay-warm tick"
                );
            }
            Err(err) => {
                tracing::error!(error = %err, "layers-stay-warm tick failed; retrying next interval");
            }
        }
    }
}

/// **Interim reference (no longer run by the binary).** The bare poll loop
/// the Viggy controller replaces: tick every `poll_interval_secs`, run one
/// cycle, emit one structured log line per watched entry per cycle — but no
/// promessa, no seen-ratio, no attestation. Kept for parity/reference; the
/// destination is [`run_viggy_loop`].
#[allow(dead_code)]
async fn run_poll_loop<A, P>(commits_api: &A, prewarm_runner: &P, config: PrewarmerConfig)
where
    A: sui_dockerfile_prewarmer::github::CommitsApi,
    P: sui_dockerfile_prewarmer::prewarm::PrewarmRunner,
{
    let mut state = PollState::new();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(config.poll_interval_secs));
    loop {
        interval.tick().await;
        let cycle_started = std::time::Instant::now();
        let reports = run_cycle(commits_api, prewarm_runner, &mut state, &config.watched).await;
        let duration_ms = cycle_started.elapsed().as_millis();

        for report in &reports {
            match (&report.check, &report.prewarm) {
                (CheckOutcome::NoChange { sha }, _) => {
                    tracing::info!(owner = %report.entry.owner, repo = %report.entry.repo, path = %report.entry.path, sha = %sha, "checked: no change");
                }
                (CheckOutcome::Changed { new_sha, .. }, Some(PrewarmOutcome::Prewarmed)) => {
                    tracing::info!(owner = %report.entry.owner, repo = %report.entry.repo, path = %report.entry.path, sha = %new_sha, "pre-warmed cache for new commit");
                }
                (CheckOutcome::Changed { new_sha, .. }, Some(PrewarmOutcome::GithubError(err))) => {
                    tracing::warn!(owner = %report.entry.owner, repo = %report.entry.repo, path = %report.entry.path, sha = %new_sha, error = %err, "github fetch failed; will retry next cycle");
                }
                (CheckOutcome::Changed { new_sha, .. }, Some(PrewarmOutcome::WrapperError(err))) => {
                    tracing::warn!(owner = %report.entry.owner, repo = %report.entry.repo, path = %report.entry.path, sha = %new_sha, error = %err, "pre-warm wrapper failed; will retry next cycle");
                }
                (CheckOutcome::Changed { .. }, None) => {
                    tracing::error!(owner = %report.entry.owner, repo = %report.entry.repo, path = %report.entry.path, "changed entry produced no prewarm outcome (unreachable)");
                }
            }
        }
        tracing::info!(watched_count = config.watched.len(), duration_ms, "poll cycle complete");
    }
}
