//! Prewarmer resilience: the standing poll service must survive EVERY
//! GitHub-API failure mode without ever panicking, and must never commit
//! poll-state on a failed check/fetch (so a transient failure is retried
//! next cycle, not silently marked "seen").
//!
//! Two layers, both against SHIPPED code (no rewrite of the poll loop):
//!
//!  1. **Poll-loop survival** — an injectable-outcome [`CommitsApi`] mock
//!     that yields any [`GithubError`] on demand, driving the real
//!     [`sui_dockerfile_prewarmer::run_cycle`]. Proves: a rate-limit
//!     (403/429), an API-status error (5xx / 404), and an empty response
//!     (`NoCommits`) each surface as a typed [`PrewarmOutcome::GithubError`],
//!     the loop keeps going to the next entry, the prewarm runner is never
//!     invoked on a failed check, and state is never committed.
//!
//!  2. **Real transport failure modes** — a throwaway in-process TCP HTTP
//!     server drives the REAL [`RealCommitsApi`] reqwest client against
//!     genuine wire responses: 403 + `Retry-After` (rate-limit), 429,
//!     malformed JSON, truncated JSON, a 404 (deleted/renamed branch), an
//!     empty `[]` array, and a dead port (connection-refused). Every one
//!     resolves to a typed [`GithubError`] — never an `unwrap`-panic.
//!
//! Not a single `unwrap`/`expect` on a fallible prewarmer surface here:
//! the whole point is that the poll loop degrades to a typed error and
//! keeps running.

use std::sync::Mutex;

use async_trait::async_trait;
use sui_dockerfile_prewarmer::config::WatchedDockerfile;
use sui_dockerfile_prewarmer::github::{CommitsApi, GithubError, RealCommitsApi};
use sui_dockerfile_prewarmer::prewarm::PrewarmRunner;
use sui_dockerfile_prewarmer::{run_cycle, CheckOutcome, PollState, PrewarmOutcome};
use sui_dockerfile_wrapper::{WrapperError, WrapperOutcome, WrapperReceipt};

// ── A local recording PrewarmRunner ──────────────────────────────────
//
// The crate's own `prewarm::mock::RecordingPrewarmRunner` is `#[cfg(test)]`
// so it isn't visible to an integration test (which links the lib built
// without cfg(test)). We define a minimal equivalent here — same shape,
// records every prewarm call, returns a synthetic success receipt — so
// this test drives the real `run_cycle` without a real cache or docker.

#[derive(Default)]
struct RecordingPrewarmRunner {
    calls: Mutex<usize>,
}

impl RecordingPrewarmRunner {
    fn new() -> Self {
        Self::default()
    }

    fn recorded(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl PrewarmRunner for RecordingPrewarmRunner {
    async fn prewarm(&self, _path: &str, _content: &str, _tag: &str) -> Result<WrapperReceipt, WrapperError> {
        *self.calls.lock().unwrap() += 1;
        Ok(WrapperReceipt {
            outcome: WrapperOutcome::CacheMiss { docker_build_duration_ms: 0, nodes_cached: 1 },
            nodes: Vec::new(),
            total_wall_clock_ms: 0,
            docker_ran: true,
            fell_through_reason: None,
        })
    }
}

// ── Layer 1: injectable-outcome CommitsApi mock ──────────────────────
//
// The crate's own `MockCommitsApi` can only ever return `NoCommits`.
// To exercise the poll loop against the *full* GithubError surface we
// need a mock that returns an arbitrary scripted outcome per call — a
// rate-limit, an API-status error, a network error, or a success.

/// One scripted response for a `latest_commit_sha` call.
enum ShaResponse {
    Ok(String),
    RateLimited,
    ApiStatus { status: u16, body: String },
    Empty,
}

/// A `CommitsApi` whose every call returns the next scripted response,
/// so a test can drive the real `run_cycle` through each failure mode
/// deterministically. `content` is scripted separately.
struct ScriptedApi {
    sha_script: Mutex<std::collections::VecDeque<ShaResponse>>,
    content: Mutex<Option<Result<String, ()>>>,
    /// Every `latest_commit_sha` invocation, in order — proves the loop
    /// kept polling past a failure rather than aborting the whole cycle.
    sha_calls: Mutex<usize>,
    content_calls: Mutex<usize>,
}

impl ScriptedApi {
    fn new(script: Vec<ShaResponse>) -> Self {
        Self {
            sha_script: Mutex::new(script.into_iter().collect()),
            content: Mutex::new(None),
            sha_calls: Mutex::new(0),
            content_calls: Mutex::new(0),
        }
    }

    fn with_content(self, content: Result<String, ()>) -> Self {
        *self.content.lock().unwrap() = Some(content);
        self
    }

    fn sha_call_count(&self) -> usize {
        *self.sha_calls.lock().unwrap()
    }

    fn content_call_count(&self) -> usize {
        *self.content_calls.lock().unwrap()
    }
}

#[async_trait]
impl CommitsApi for ScriptedApi {
    async fn latest_commit_sha(&self, _o: &str, _r: &str, _g: &str, _p: &str) -> Result<String, GithubError> {
        *self.sha_calls.lock().unwrap() += 1;
        let next = self.sha_script.lock().unwrap().pop_front();
        match next {
            Some(ShaResponse::Ok(sha)) => Ok(sha),
            // GitHub rate-limits with 403 (secondary limit) or 429; the
            // typed surface both take is `ApiStatus`.
            Some(ShaResponse::RateLimited) => Err(GithubError::ApiStatus {
                status: 403,
                body: "API rate limit exceeded".to_string(),
            }),
            Some(ShaResponse::ApiStatus { status, body }) => Err(GithubError::ApiStatus { status, body }),
            // Empty commit list for the watched path — the crate maps this
            // to NoCommits (e.g. a typo'd / deleted path).
            Some(ShaResponse::Empty) | None => Err(GithubError::NoCommits),
        }
    }

    async fn fetch_raw_content(&self, _o: &str, _r: &str, _s: &str, _p: &str) -> Result<String, GithubError> {
        *self.content_calls.lock().unwrap() += 1;
        match self.content.lock().unwrap().clone() {
            Some(Ok(c)) => Ok(c),
            Some(Err(())) | None => Err(GithubError::NoCommits),
        }
    }
}

fn watched(path: &str) -> WatchedDockerfile {
    WatchedDockerfile {
        owner: "example-org".to_string(),
        repo: "example-images".to_string(),
        git_ref: "master".to_string(),
        path: path.to_string(),
        image_tag: "example/base:prewarm".to_string(),
    }
}

#[tokio::test]
async fn a_rate_limit_on_check_is_a_typed_outcome_and_the_loop_survives() {
    let entry = watched("Dockerfile");
    let api = ScriptedApi::new(vec![ShaResponse::RateLimited]);
    let runner = RecordingPrewarmRunner::new();
    let mut state = PollState::new();

    let reports = run_cycle(&api, &runner, &mut state, std::slice::from_ref(&entry)).await;

    assert_eq!(reports.len(), 1);
    assert!(matches!(reports[0].prewarm, Some(PrewarmOutcome::GithubError(_))));
    assert_eq!(runner.recorded(), 0, "a rate-limited check never pre-warms");
    assert_eq!(state.last_seen(&entry), None, "a rate-limited check never commits state");
    assert_eq!(api.sha_call_count(), 1);
}

#[tokio::test]
async fn a_5xx_api_status_on_check_is_a_typed_outcome_and_the_loop_survives() {
    let entry = watched("Dockerfile");
    let api = ScriptedApi::new(vec![ShaResponse::ApiStatus { status: 502, body: "Bad Gateway".into() }]);
    let runner = RecordingPrewarmRunner::new();
    let mut state = PollState::new();

    let reports = run_cycle(&api, &runner, &mut state, std::slice::from_ref(&entry)).await;

    assert!(matches!(reports[0].prewarm, Some(PrewarmOutcome::GithubError(_))));
    assert_eq!(state.last_seen(&entry), None);
}

#[tokio::test]
async fn an_empty_commit_list_is_a_typed_outcome_and_the_loop_survives() {
    let entry = watched("Dockerfile");
    let api = ScriptedApi::new(vec![ShaResponse::Empty]);
    let runner = RecordingPrewarmRunner::new();
    let mut state = PollState::new();

    let reports = run_cycle(&api, &runner, &mut state, std::slice::from_ref(&entry)).await;

    assert!(matches!(reports[0].prewarm, Some(PrewarmOutcome::GithubError(_))));
    assert_eq!(state.last_seen(&entry), None);
}

#[tokio::test]
async fn one_failing_entry_does_not_abort_the_rest_of_the_cycle() {
    // The loop must keep going: entry A fails its check (404), entry B
    // succeeds and pre-warms. Proves a per-entry failure is isolated and
    // never aborts the whole poll cycle.
    let entry_a = watched("docker/base/Dockerfile");
    let entry_b = watched("Dockerfile.nonroot_gateway");

    let api = ScriptedApi::new(vec![
        ShaResponse::ApiStatus { status: 404, body: "Not Found".into() },
        ShaResponse::Ok("sha-b".to_string()),
    ])
    .with_content(Ok("FROM debian:bookworm-slim\n".to_string()));
    let runner = RecordingPrewarmRunner::new();
    let mut state = PollState::new();

    let reports = run_cycle(&api, &runner, &mut state, &[entry_a.clone(), entry_b.clone()]).await;

    assert_eq!(reports.len(), 2, "both entries were checked despite A failing");
    assert!(matches!(reports[0].prewarm, Some(PrewarmOutcome::GithubError(_))));
    assert_eq!(reports[1].prewarm, Some(PrewarmOutcome::Prewarmed));
    assert_eq!(state.last_seen(&entry_a), None, "the failed entry committed no state");
    assert_eq!(state.last_seen(&entry_b), Some("sha-b"), "the good entry advanced");
    assert_eq!(api.sha_call_count(), 2, "both entries polled");
}

#[tokio::test]
async fn a_failed_content_fetch_after_a_good_check_never_commits_state() {
    // The check succeeds (new SHA), but the raw-content fetch fails (e.g.
    // a 429 on raw.githubusercontent.com). The entry must NOT be marked
    // seen, so the fetch is retried next cycle.
    let entry = watched("Dockerfile");
    let api = ScriptedApi::new(vec![ShaResponse::Ok("sha1".to_string())]).with_content(Err(()));
    let runner = RecordingPrewarmRunner::new();
    let mut state = PollState::new();

    let reports = run_cycle(&api, &runner, &mut state, std::slice::from_ref(&entry)).await;

    assert!(matches!(reports[0].check, CheckOutcome::Changed { .. }));
    assert!(matches!(reports[0].prewarm, Some(PrewarmOutcome::GithubError(_))));
    assert_eq!(runner.recorded(), 0, "no pre-warm without content");
    assert_eq!(state.last_seen(&entry), None);
    assert_eq!(api.content_call_count(), 1, "the content fetch was attempted");
}

#[tokio::test]
async fn a_recovered_failure_prewarms_on_the_next_cycle() {
    // Cycle 1: the check rate-limits, nothing committed. Cycle 2: the
    // check recovers, content fetches, and the entry finally pre-warms.
    // Proves the "retry next cycle" contract end-to-end across cycles.
    let entry = watched("Dockerfile");
    let api = ScriptedApi::new(vec![ShaResponse::RateLimited, ShaResponse::Ok("sha1".to_string())])
        .with_content(Ok("FROM debian\n".to_string()));
    let runner = RecordingPrewarmRunner::new();
    let mut state = PollState::new();

    let cycle1 = run_cycle(&api, &runner, &mut state, std::slice::from_ref(&entry)).await;
    assert!(matches!(cycle1[0].prewarm, Some(PrewarmOutcome::GithubError(_))));
    assert_eq!(state.last_seen(&entry), None);

    let cycle2 = run_cycle(&api, &runner, &mut state, std::slice::from_ref(&entry)).await;
    assert_eq!(cycle2[0].prewarm, Some(PrewarmOutcome::Prewarmed));
    assert_eq!(state.last_seen(&entry), Some("sha1"));
    assert_eq!(runner.recorded(), 1, "exactly one pre-warm across both cycles");
}

// ── Layer 2: real RealCommitsApi transport against a throwaway server ─
//
// A minimal in-process HTTP/1.1 server on an ephemeral loopback port
// that serves ONE scripted raw response, then closes. Drives the REAL
// reqwest client through genuine wire responses — the code path a real
// deployment runs — so each failure mode is proven against the shipped
// transport, not just the trait mock.

mod tiny_server {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Bind an ephemeral port, serve `raw_response` verbatim to the first
    /// connection (reading + discarding the request), then close.
    /// Returns the bound `http://127.0.0.1:<port>` base URL.
    pub async fn serve_once(raw_response: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                // Drain the request headers (best-effort — we don't parse it).
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(raw_response).await;
                let _ = socket.flush().await;
                // Give the client time to read before the socket drops.
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    /// Build a full HTTP/1.1 response with a correct Content-Length.
    pub fn response(status_line: &str, extra_headers: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status_line}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
}

#[tokio::test]
async fn real_client_maps_403_rate_limit_to_typed_api_status_not_a_panic() {
    let raw = tiny_server::response(
        "403 Forbidden",
        "Retry-After: 60\r\nx-ratelimit-remaining: 0\r\n",
        r#"{"message":"API rate limit exceeded"}"#,
    );
    // Leak to get a 'static slice for the spawned server — one tiny,
    // bounded allocation per test, freed at process exit.
    let raw: &'static [u8] = Box::leak(raw.into_boxed_slice());
    let base = tiny_server::serve_once(raw).await;

    let api = RealCommitsApi::new(base, "http://unused");
    let err = api
        .latest_commit_sha("o", "r", "master", "Dockerfile")
        .await
        .expect_err("a 403 must surface as an error, not Ok");
    match err {
        GithubError::ApiStatus { status, .. } => assert_eq!(status, 403),
        other => panic!("expected ApiStatus(403), got {other:?}"),
    }
}

#[tokio::test]
async fn real_client_maps_429_too_many_requests_to_typed_api_status() {
    let raw = tiny_server::response("429 Too Many Requests", "Retry-After: 120\r\n", "rate limited");
    let raw: &'static [u8] = Box::leak(raw.into_boxed_slice());
    let base = tiny_server::serve_once(raw).await;

    let api = RealCommitsApi::new(base, "http://unused");
    let err = api.latest_commit_sha("o", "r", "master", "Dockerfile").await.expect_err("429 must error");
    match err {
        GithubError::ApiStatus { status, .. } => assert_eq!(status, 429),
        other => panic!("expected ApiStatus(429), got {other:?}"),
    }
}

#[tokio::test]
async fn real_client_maps_404_deleted_or_renamed_path_to_typed_api_status() {
    let raw = tiny_server::response("404 Not Found", "", r#"{"message":"Not Found"}"#);
    let raw: &'static [u8] = Box::leak(raw.into_boxed_slice());
    let base = tiny_server::serve_once(raw).await;

    let api = RealCommitsApi::new(base, "http://unused");
    let err = api.latest_commit_sha("o", "r", "deleted-branch", "Dockerfile").await.expect_err("404 must error");
    match err {
        GithubError::ApiStatus { status, .. } => assert_eq!(status, 404),
        other => panic!("expected ApiStatus(404), got {other:?}"),
    }
}

#[tokio::test]
async fn real_client_on_malformed_json_body_is_a_typed_http_error_not_a_panic() {
    // 200 OK but the body is NOT the expected `[{"sha":...}]` array —
    // reqwest's `.json()` fails, which the crate maps to GithubError::Http.
    let raw = tiny_server::response("200 OK", "Content-Type: application/json\r\n", "{ this is not valid json ][");
    let raw: &'static [u8] = Box::leak(raw.into_boxed_slice());
    let base = tiny_server::serve_once(raw).await;

    let api = RealCommitsApi::new(base, "http://unused");
    let err = api.latest_commit_sha("o", "r", "master", "Dockerfile").await.expect_err("malformed json must error");
    assert!(matches!(err, GithubError::Http(_)), "malformed json -> typed Http error, got {err:?}");
}

#[tokio::test]
async fn real_client_on_truncated_json_body_is_a_typed_error_not_a_panic() {
    // A Content-Length that promises more bytes than are sent would hang
    // the reader; instead send a well-framed but truncated/partial JSON
    // document (valid framing, invalid JSON) — reqwest parses the framed
    // body then fails to deserialize.
    let raw = tiny_server::response("200 OK", "Content-Type: application/json\r\n", r#"[{"sha":"abc"#);
    let raw: &'static [u8] = Box::leak(raw.into_boxed_slice());
    let base = tiny_server::serve_once(raw).await;

    let api = RealCommitsApi::new(base, "http://unused");
    let err = api.latest_commit_sha("o", "r", "master", "Dockerfile").await.expect_err("truncated json must error");
    assert!(matches!(err, GithubError::Http(_)), "truncated json -> typed Http error, got {err:?}");
}

#[tokio::test]
async fn real_client_on_empty_commit_array_is_typed_no_commits() {
    // 200 OK with a well-formed but EMPTY array — the watched path exists
    // in the repo but has no commit history matching the query. The crate
    // maps the empty list to the typed NoCommits, never a panic on
    // `.into_iter().next()`.
    let raw = tiny_server::response("200 OK", "Content-Type: application/json\r\n", "[]");
    let raw: &'static [u8] = Box::leak(raw.into_boxed_slice());
    let base = tiny_server::serve_once(raw).await;

    let api = RealCommitsApi::new(base, "http://unused");
    let err = api.latest_commit_sha("o", "r", "master", "Dockerfile").await.expect_err("empty array must error");
    assert!(matches!(err, GithubError::NoCommits), "empty array -> NoCommits, got {err:?}");
}

#[tokio::test]
async fn real_client_on_connection_refused_is_a_typed_http_error_not_a_panic() {
    // Point at a port nothing is listening on. reqwest's connect fails;
    // the crate surfaces GithubError::Http — never an unwrap-panic.
    // Port 1 on loopback is reliably unbound/unroutable for a client.
    let api = RealCommitsApi::new("http://127.0.0.1:1", "http://unused");
    let err = api.latest_commit_sha("o", "r", "master", "Dockerfile").await.expect_err("connection refused must error");
    assert!(matches!(err, GithubError::Http(_)), "connection refused -> typed Http error, got {err:?}");
}

#[tokio::test]
async fn real_client_fetch_raw_content_on_404_is_a_typed_api_status() {
    // The raw-content endpoint (raw.githubusercontent.com) 404s when a
    // path was deleted at the fetched SHA — must be a typed error, since
    // the poll loop turns it into PrewarmOutcome::GithubError.
    let raw = tiny_server::response("404 Not Found", "", "404: Not Found");
    let raw: &'static [u8] = Box::leak(raw.into_boxed_slice());
    let base = tiny_server::serve_once(raw).await;

    let api = RealCommitsApi::new("http://unused", base);
    let err = api.fetch_raw_content("o", "r", "deadbeef", "Dockerfile").await.expect_err("raw 404 must error");
    match err {
        GithubError::ApiStatus { status, .. } => assert_eq!(status, 404),
        other => panic!("expected ApiStatus(404), got {other:?}"),
    }
}
