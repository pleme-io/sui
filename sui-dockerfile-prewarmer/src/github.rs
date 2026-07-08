//! GitHub-poll-for-changes: the read-only, public, zero-cluster-access
//! surface. All network I/O is behind [`CommitsApi`] so unit tests never
//! make a real HTTP call.

use async_trait::async_trait;
use serde::Deserialize;

/// Typed error surface for the GitHub polling client — never a panic.
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("github api returned no commits for the watched path")]
    NoCommits,
    #[error("github api error status {status}: {body}")]
    ApiStatus { status: u16, body: String },
}

/// The one injectable seam for all GitHub I/O this crate performs:
/// "what's the latest commit touching this path?" and "what's the file
/// content at that commit?".
#[async_trait]
pub trait CommitsApi: Send + Sync {
    /// Returns the SHA of the most recent commit touching `path` on
    /// `git_ref`, or [`GithubError::NoCommits`] if the API returns an
    /// empty list (e.g. a typo'd path).
    async fn latest_commit_sha(&self, owner: &str, repo: &str, git_ref: &str, path: &str) -> Result<String, GithubError>;

    /// Fetches the raw file content of `path` at `sha`.
    async fn fetch_raw_content(&self, owner: &str, repo: &str, sha: &str, path: &str) -> Result<String, GithubError>;
}

#[derive(Debug, Deserialize)]
struct CommitEntry {
    sha: String,
}

/// The production [`CommitsApi`] — a thin `reqwest` client against the
/// public GitHub REST API (`api.github.com`) and `raw.githubusercontent.com`.
/// No authentication: these are public read-only endpoints per the
/// Phase 3 spec (zero cluster/k8s access, zero credentials).
pub struct RealCommitsApi {
    client: reqwest::Client,
    api_base: String,
    raw_base: String,
}

impl RealCommitsApi {
    #[must_use]
    pub fn new(api_base: impl Into<String>, raw_base: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("sui-dockerfile-prewarmer")
                .build()
                .unwrap_or_default(),
            api_base: api_base.into(),
            raw_base: raw_base.into(),
        }
    }
}

#[async_trait]
impl CommitsApi for RealCommitsApi {
    async fn latest_commit_sha(&self, owner: &str, repo: &str, git_ref: &str, path: &str) -> Result<String, GithubError> {
        let url = format!("{}/repos/{owner}/{repo}/commits", self.api_base);
        let response = self
            .client
            .get(url)
            .query(&[("sha", git_ref), ("path", path), ("per_page", "1")])
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(GithubError::ApiStatus { status, body });
        }
        let entries: Vec<CommitEntry> = response.json().await?;
        entries.into_iter().next().map(|entry| entry.sha).ok_or(GithubError::NoCommits)
    }

    async fn fetch_raw_content(&self, owner: &str, repo: &str, sha: &str, path: &str) -> Result<String, GithubError> {
        let url = format!("{}/{owner}/{repo}/{sha}/{path}", self.raw_base);
        let response = self.client.get(url).send().await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(GithubError::ApiStatus { status, body });
        }
        Ok(response.text().await?)
    }
}

#[cfg(test)]
pub mod mock {
    //! A hand-rolled mock [`CommitsApi`] — no real network call, ever.
    //! Used by this crate's unit tests and re-exported for downstream
    //! crates that want to test against this seam.

    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::{CommitsApi, GithubError};
    use async_trait::async_trait;

    /// Keyed by `(owner, repo, git_ref, path)`.
    pub type ShaKey = (String, String, String, String);
    /// Keyed by `(owner, repo, sha, path)`.
    pub type ContentKey = (String, String, String, String);

    #[derive(Default)]
    pub struct MockCommitsApi {
        shas: Mutex<HashMap<ShaKey, String>>,
        contents: Mutex<HashMap<ContentKey, String>>,
        calls: Mutex<Vec<ShaKey>>,
    }

    impl MockCommitsApi {
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        #[must_use]
        pub fn with_sha(self, owner: &str, repo: &str, git_ref: &str, path: &str, sha: &str) -> Self {
            self.shas.lock().unwrap().insert(
                (owner.to_string(), repo.to_string(), git_ref.to_string(), path.to_string()),
                sha.to_string(),
            );
            self
        }

        #[must_use]
        pub fn with_content(self, owner: &str, repo: &str, sha: &str, path: &str, content: &str) -> Self {
            self.contents.lock().unwrap().insert(
                (owner.to_string(), repo.to_string(), sha.to_string(), path.to_string()),
                content.to_string(),
            );
            self
        }

        /// Every `latest_commit_sha` call recorded, in order — used to
        /// assert the poll loop queried exactly the watched entries.
        #[must_use]
        pub fn recorded_sha_lookups(&self) -> Vec<ShaKey> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommitsApi for MockCommitsApi {
        async fn latest_commit_sha(&self, owner: &str, repo: &str, git_ref: &str, path: &str) -> Result<String, GithubError> {
            let key = (owner.to_string(), repo.to_string(), git_ref.to_string(), path.to_string());
            self.calls.lock().unwrap().push(key.clone());
            self.shas.lock().unwrap().get(&key).cloned().ok_or(GithubError::NoCommits)
        }

        async fn fetch_raw_content(&self, owner: &str, repo: &str, sha: &str, path: &str) -> Result<String, GithubError> {
            let key = (owner.to_string(), repo.to_string(), sha.to_string(), path.to_string());
            self.contents.lock().unwrap().get(&key).cloned().ok_or(GithubError::NoCommits)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockCommitsApi;
    use super::CommitsApi;

    #[tokio::test]
    async fn latest_commit_sha_returns_the_configured_sha() {
        let api = MockCommitsApi::new().with_sha("akeylesslabs", "akeyless-main-repo", "master", "Dockerfile", "abc123");
        let sha = api.latest_commit_sha("akeylesslabs", "akeyless-main-repo", "master", "Dockerfile").await.unwrap();
        assert_eq!(sha, "abc123");
    }

    #[tokio::test]
    async fn unknown_path_is_a_typed_no_commits_error_not_a_panic() {
        let api = MockCommitsApi::new();
        let err = api.latest_commit_sha("o", "r", "main", "Dockerfile").await.unwrap_err();
        assert!(matches!(err, super::GithubError::NoCommits));
    }

    #[tokio::test]
    async fn fetch_raw_content_returns_the_configured_content() {
        let api = MockCommitsApi::new().with_content("o", "r", "abc123", "Dockerfile", "FROM debian\n");
        let content = api.fetch_raw_content("o", "r", "abc123", "Dockerfile").await.unwrap();
        assert_eq!(content, "FROM debian\n");
    }
}
