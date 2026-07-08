//! `sui-dockerfile-prewarmer` — the standing pre-warming service
//! binary. Loads typed config, then runs the poll loop forever on a
//! `tokio::time::interval`, emitting structured `tracing` events on
//! every cycle (checked, found change, pre-warmed, duration).

use sui_dockerfile_prewarmer::config::PrewarmerConfig;
use sui_dockerfile_prewarmer::github::RealCommitsApi;
use sui_dockerfile_prewarmer::prewarm::RealPrewarmRunner;
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

    tracing::info!(watched_count = config.watched.len(), poll_interval_secs = config.poll_interval_secs, "starting sui-dockerfile-prewarmer");

    let commits_api = RealCommitsApi::new(config.github_api_base.clone(), config.github_raw_base.clone());
    let cache = sui_cache::storage::build_backend(&config.cache_backend).await?;
    let prewarm_runner = RealPrewarmRunner::new(cache, std::env::current_dir()?);

    run_poll_loop(&commits_api, &prewarm_runner, config).await;
    Ok(())
}

/// The standing loop: tick every `poll_interval_secs`, run one cycle,
/// emit one structured log line per watched entry per cycle. Runs
/// forever — the process is the deploy unit's whole lifecycle.
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
