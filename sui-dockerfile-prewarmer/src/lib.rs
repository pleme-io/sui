//! Phase 3 of `supa-charge-ci`: the standing pre-warming
//! service.
//!
//! Polls a watched list of public Dockerfiles (config: repo, ref, path)
//! via the GitHub REST API for new commits — zero cluster/k8s access,
//! purely watching public GitHub state. On detecting a change, fetches
//! the new content and invokes [`sui_dockerfile_wrapper::run_wrapper`]
//! in cache-miss mode: this IS the pre-warm, running the real build
//! once and back-filling the cache ahead of any real CI build.
//!
//! Every side effect (the GitHub API, the wrapper invocation) is behind
//! an injectable trait — [`github::CommitsApi`] and [`prewarm::PrewarmRunner`]
//! — so the poll-cycle logic in this module is fully unit-testable
//! without network, docker, or a cache backend.

pub mod config;
pub mod github;
pub mod outcome_chain;
pub mod prewarm;
pub mod viggy;

use config::WatchedDockerfile;
use github::{CommitsApi, GithubError};
use prewarm::PrewarmRunner;
use serde::{Deserialize, Serialize};

/// Identifies one watched Dockerfile for poll-state tracking. A struct
/// (not a bare tuple) so the `(owner, repo, git_ref, path)` shape has a
/// name and can't be silently reordered at a call site.
///
/// Stored as a `Vec<(EntryKey, String)>` rather than a `HashMap` in
/// [`PollState`] because `serde_json` requires string map keys — a
/// struct key would fail to serialize. The watched list is small
/// (single digits), so linear lookup is the right tradeoff over a
/// custom string-joining key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct EntryKey {
    owner: String,
    repo: String,
    git_ref: String,
    path: String,
}

impl EntryKey {
    fn of(entry: &WatchedDockerfile) -> Self {
        Self { owner: entry.owner.clone(), repo: entry.repo.clone(), git_ref: entry.git_ref.clone(), path: entry.path.clone() }
    }
}

/// The poll state this crate must carry across cycles: the last-seen
/// commit SHA per watched Dockerfile.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PollState {
    last_seen_sha: Vec<(EntryKey, String)>,
}

impl PollState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn last_seen(&self, entry: &WatchedDockerfile) -> Option<&str> {
        let key = EntryKey::of(entry);
        self.last_seen_sha.iter().find(|(k, _)| *k == key).map(|(_, sha)| sha.as_str())
    }

    pub fn record(&mut self, entry: &WatchedDockerfile, sha: String) {
        let key = EntryKey::of(entry);
        if let Some(slot) = self.last_seen_sha.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = sha;
        } else {
            self.last_seen_sha.push((key, sha));
        }
    }
}

/// The typed outcome of checking one watched Dockerfile for a change —
/// the "found change" vs "no change" distinction the poll loop's
/// structured log line reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// No prior SHA recorded, or the recorded SHA still matches HEAD.
    NoChange { sha: String },
    /// The latest commit SHA differs from the last-seen one (or this
    /// is the first time this entry has been checked).
    Changed { previous: Option<String>, new_sha: String },
}

/// The typed outcome of pre-warming one changed watched Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrewarmOutcome {
    Prewarmed,
    GithubError(String),
    WrapperError(String),
}

/// One watched entry's full per-cycle result — checked, found
/// change-or-not, and (if changed) pre-warmed-or-not. This is the typed
/// structured-log payload for one entry in one poll cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryCycleReport {
    pub entry: WatchedDockerfile,
    pub check: CheckOutcome,
    pub prewarm: Option<PrewarmOutcome>,
}

/// Check one watched Dockerfile for a new commit against `state`,
/// returning the typed outcome without mutating `state` — the caller
/// decides when to commit the new SHA (only after a successful
/// pre-warm, so a failed pre-warm is retried next cycle rather than
/// silently marked "seen").
async fn check_one<A: CommitsApi>(api: &A, state: &PollState, entry: &WatchedDockerfile) -> Result<CheckOutcome, GithubError> {
    let sha = api.latest_commit_sha(&entry.owner, &entry.repo, &entry.git_ref, &entry.path).await?;
    match state.last_seen(entry) {
        Some(previous) if previous == sha => Ok(CheckOutcome::NoChange { sha }),
        previous => Ok(CheckOutcome::Changed { previous: previous.map(str::to_string), new_sha: sha }),
    }
}

/// Run one full poll cycle over every watched entry: check each for a
/// new commit; on a change, fetch the content and pre-warm; on
/// success, commit the new SHA into `state` so the next cycle sees
/// `NoChange`.
///
/// This is the single-iteration logic the poll loop's `tokio::time::interval`
/// ticks drive — tested directly here, never by sleeping in a test.
pub async fn run_cycle<A: CommitsApi, P: PrewarmRunner>(api: &A, prewarm_runner: &P, state: &mut PollState, watched: &[WatchedDockerfile]) -> Vec<EntryCycleReport> {
    let mut reports = Vec::with_capacity(watched.len());
    for entry in watched {
        let check = match check_one(api, state, entry).await {
            Ok(outcome) => outcome,
            Err(err) => {
                reports.push(EntryCycleReport {
                    entry: entry.clone(),
                    check: CheckOutcome::NoChange { sha: String::new() },
                    prewarm: Some(PrewarmOutcome::GithubError(err.to_string())),
                });
                continue;
            }
        };

        let prewarm = match &check {
            CheckOutcome::NoChange { .. } => None,
            CheckOutcome::Changed { new_sha, .. } => {
                Some(prewarm_changed_entry(api, prewarm_runner, state, entry, new_sha).await)
            }
        };

        reports.push(EntryCycleReport { entry: entry.clone(), check, prewarm });
    }
    reports
}

async fn prewarm_changed_entry<A: CommitsApi, P: PrewarmRunner>(
    api: &A,
    prewarm_runner: &P,
    state: &mut PollState,
    entry: &WatchedDockerfile,
    new_sha: &str,
) -> PrewarmOutcome {
    let content = match api.fetch_raw_content(&entry.owner, &entry.repo, new_sha, &entry.path).await {
        Ok(content) => content,
        Err(err) => return PrewarmOutcome::GithubError(err.to_string()),
    };
    match prewarm_runner.prewarm(&entry.path, &content, &entry.image_tag).await {
        Ok(_receipt) => {
            state.record(entry, new_sha.to_string());
            PrewarmOutcome::Prewarmed
        }
        Err(err) => PrewarmOutcome::WrapperError(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::config::WatchedDockerfile;
    use super::github::mock::MockCommitsApi;
    use super::prewarm::mock::RecordingPrewarmRunner;
    use super::*;

    fn watched() -> WatchedDockerfile {
        WatchedDockerfile {
            owner: "example-org".to_string(),
            repo: "example-images".to_string(),
            git_ref: "master".to_string(),
            path: "docker/base/Dockerfile".to_string(),
            image_tag: "example/base:prewarm".to_string(),
        }
    }

    #[tokio::test]
    async fn first_check_with_no_prior_state_is_a_change() {
        let entry = watched();
        let api = MockCommitsApi::new().with_sha(&entry.owner, &entry.repo, &entry.git_ref, &entry.path, "sha1");
        let state = PollState::new();

        let outcome = check_one(&api, &state, &entry).await.unwrap();
        assert_eq!(outcome, CheckOutcome::Changed { previous: None, new_sha: "sha1".to_string() });
    }

    #[tokio::test]
    async fn same_sha_as_last_seen_is_no_change() {
        let entry = watched();
        let api = MockCommitsApi::new().with_sha(&entry.owner, &entry.repo, &entry.git_ref, &entry.path, "sha1");
        let mut state = PollState::new();
        state.record(&entry, "sha1".to_string());

        let outcome = check_one(&api, &state, &entry).await.unwrap();
        assert_eq!(outcome, CheckOutcome::NoChange { sha: "sha1".to_string() });
    }

    #[tokio::test]
    async fn different_sha_from_last_seen_is_a_change() {
        let entry = watched();
        let api = MockCommitsApi::new().with_sha(&entry.owner, &entry.repo, &entry.git_ref, &entry.path, "sha2");
        let mut state = PollState::new();
        state.record(&entry, "sha1".to_string());

        let outcome = check_one(&api, &state, &entry).await.unwrap();
        assert_eq!(outcome, CheckOutcome::Changed { previous: Some("sha1".to_string()), new_sha: "sha2".to_string() });
    }

    #[tokio::test]
    async fn run_cycle_on_change_invokes_prewarm_runner_with_fetched_content() {
        let entry = watched();
        let api = MockCommitsApi::new()
            .with_sha(&entry.owner, &entry.repo, &entry.git_ref, &entry.path, "sha1")
            .with_content(&entry.owner, &entry.repo, "sha1", &entry.path, "FROM debian:bookworm-slim\n");
        let prewarm_runner = RecordingPrewarmRunner::new();
        let mut state = PollState::new();

        let reports = run_cycle(&api, &prewarm_runner, &mut state, std::slice::from_ref(&entry)).await;

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].prewarm, Some(PrewarmOutcome::Prewarmed));
        let recorded = prewarm_runner.recorded();
        assert_eq!(recorded.len(), 1, "exactly one prewarm invocation for the one changed entry");
        assert_eq!(recorded[0].dockerfile_content, "FROM debian:bookworm-slim\n");
        assert_eq!(recorded[0].dockerfile_path, entry.path);
        assert_eq!(recorded[0].image_tag, entry.image_tag);
        // State was committed so a second cycle against the same sha is a no-op.
        assert_eq!(state.last_seen(&entry), Some("sha1"));
    }

    #[tokio::test]
    async fn run_cycle_with_no_change_never_invokes_prewarm_runner() {
        let entry = watched();
        let api = MockCommitsApi::new().with_sha(&entry.owner, &entry.repo, &entry.git_ref, &entry.path, "sha1");
        let prewarm_runner = RecordingPrewarmRunner::new();
        let mut state = PollState::new();
        state.record(&entry, "sha1".to_string());

        let reports = run_cycle(&api, &prewarm_runner, &mut state, std::slice::from_ref(&entry)).await;

        assert_eq!(reports[0].prewarm, None);
        assert!(prewarm_runner.recorded().is_empty());
    }

    #[tokio::test]
    async fn run_cycle_over_multiple_entries_checks_each_independently() {
        let entry_a = watched();
        let mut entry_b = watched();
        entry_b.path = "Dockerfile.nonroot_gateway".to_string();

        let api = MockCommitsApi::new()
            .with_sha(&entry_a.owner, &entry_a.repo, &entry_a.git_ref, &entry_a.path, "sha1")
            .with_sha(&entry_b.owner, &entry_b.repo, &entry_b.git_ref, &entry_b.path, "sha2")
            .with_content(&entry_a.owner, &entry_a.repo, "sha1", &entry_a.path, "FROM a\n")
            .with_content(&entry_b.owner, &entry_b.repo, "sha2", &entry_b.path, "FROM b\n");
        let prewarm_runner = RecordingPrewarmRunner::new();
        let mut state = PollState::new();

        let reports = run_cycle(&api, &prewarm_runner, &mut state, &[entry_a.clone(), entry_b.clone()]).await;

        assert_eq!(reports.len(), 2);
        assert_eq!(prewarm_runner.recorded().len(), 2);
        assert_eq!(state.last_seen(&entry_a), Some("sha1"));
        assert_eq!(state.last_seen(&entry_b), Some("sha2"));
    }

    #[tokio::test]
    async fn a_github_error_on_check_is_a_typed_outcome_not_a_panic() {
        let entry = watched();
        let api = MockCommitsApi::new(); // no sha configured -> NoCommits
        let prewarm_runner = RecordingPrewarmRunner::new();
        let mut state = PollState::new();

        let reports = run_cycle(&api, &prewarm_runner, &mut state, std::slice::from_ref(&entry)).await;

        assert!(matches!(reports[0].prewarm, Some(PrewarmOutcome::GithubError(_))));
        assert!(prewarm_runner.recorded().is_empty());
        assert_eq!(state.last_seen(&entry), None, "a failed check never commits state");
    }

    #[tokio::test]
    async fn a_failed_content_fetch_never_commits_state_so_it_retries_next_cycle() {
        let entry = watched();
        // sha configured, but no content -> fetch_raw_content errors.
        let api = MockCommitsApi::new().with_sha(&entry.owner, &entry.repo, &entry.git_ref, &entry.path, "sha1");
        let prewarm_runner = RecordingPrewarmRunner::new();
        let mut state = PollState::new();

        let reports = run_cycle(&api, &prewarm_runner, &mut state, std::slice::from_ref(&entry)).await;

        assert!(matches!(reports[0].prewarm, Some(PrewarmOutcome::GithubError(_))));
        assert_eq!(state.last_seen(&entry), None);
    }

    #[test]
    fn poll_state_round_trips_through_json() {
        let entry = watched();
        let mut state = PollState::new();
        state.record(&entry, "sha1".to_string());

        let json = serde_json::to_string(&state).unwrap();
        let parsed: PollState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.last_seen(&entry), Some("sha1"));
    }
}
