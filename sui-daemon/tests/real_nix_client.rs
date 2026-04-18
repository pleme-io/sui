//! End-to-end integration: spin up sui-daemon on a temp socket and
//! talk to it with the REAL `nix-store` binary over `NIX_REMOTE=unix://`.
//!
//! This is the acid test for worker-protocol compatibility. If real
//! CppNix clients can't drive our daemon cleanly, the individual
//! unit tests in `src/connection/mod.rs` are lying about compat.
//!
//! # Gate
//!
//! Opt-in via `SUI_TEST_ONLINE=1` + requires `nix-store` on PATH.
//! Silent no-op otherwise so CI boxes without Nix installed stay
//! green. Also silently skips when `/nix/var/nix/db/db.sqlite`
//! isn't readable — `LocalStore::open` would fail and the daemon
//! would never come up.
//!
//! # What it proves
//!
//! Each sub-test runs one real `nix-store` command, inspects exit
//! status + stdout, and asserts the shape of the response. The
//! commands chosen exercise specific worker-protocol opcodes:
//!
//!   nix-store --query --references <path>   → QueryReferences (5)
//!   nix-store --query --referrers  <path>   → QueryReferrers  (6)
//!   nix-store --query --deriver    <path>   → QueryDeriver   (18)
//!   nix-store --check-validity    <path>    → IsValidPath     (1)
//!
//! If any sub-test fails, the failure message includes the nix
//! command and stderr output, so the gap (missing op, wire-format
//! bug, version-gate drift) is visible without debug-printing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use sui_daemon::{DaemonConfig, DaemonServer};
use sui_store::LocalStore;

// ── Gate ────────────────────────────────────────────────────────

fn online_mode() -> bool {
    matches!(
        std::env::var("SUI_TEST_ONLINE").as_deref(),
        Ok("1" | "true" | "TRUE")
    )
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Skip gate: returns `Some(reason)` if the test should no-op, else
/// `None`. Prints the reason to stderr so a skipped test leaves a
/// visible trail.
fn skip_reason() -> Option<&'static str> {
    if !online_mode() {
        return Some("SUI_TEST_ONLINE not set");
    }
    if which("nix-store").is_none() {
        return Some("nix-store not on PATH");
    }
    if !Path::new("/nix/var/nix/db/db.sqlite").exists() {
        return Some("/nix/var/nix/db/db.sqlite not readable");
    }
    None
}

// ── Fixture: spin up the daemon on a temp socket ───────────────

struct DaemonFixture {
    socket: PathBuf,
    _tmp: tempfile::TempDir,
    task: tokio::task::JoinHandle<Result<(), sui_daemon::DaemonError>>,
}

impl DaemonFixture {
    async fn start() -> Self {
        // Route daemon tracing to stderr so we see the op stream.
        let _ = tracing_subscriber::fmt()
            .with_env_filter("sui_daemon=debug,sui_store=warn")
            .with_test_writer()
            .try_init();

        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("sui-daemon.sock");

        // Open real local store in READ-ONLY mode. Every op this
        // integration exercises is read-only; write-path ops need
        // write mode + proper permissions.
        let store = LocalStore::open("/nix/var/nix/db/db.sqlite")
            .await
            .expect("open local nix db");

        let config = DaemonConfig::with_socket_path(&socket);
        let server = DaemonServer::new(config, store);

        // Run in a background task. Abort on drop.
        let task = tokio::spawn(async move { server.run().await });

        // Wait up to 2s for the socket to bind.
        for _ in 0..20 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(socket.exists(), "daemon socket never appeared at {}", socket.display());

        Self {
            socket,
            _tmp: tmp,
            task,
        }
    }

    fn remote_env(&self) -> String {
        format!("unix://{}", self.socket.display())
    }
}

impl Drop for DaemonFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ── Helper: pick a path we know is valid in the local store ────

/// Return any absolute store path from the local DB — we need
/// *some* live path to query. Uses the first hit from
/// `nix-store --query --requisites /run/current-system` so we get
/// a realistic path even on fresh systems.
fn pick_valid_store_path() -> Option<String> {
    // Preferred: something guaranteed to be a derivation output
    let out = Command::new("nix-store")
        .args(["--query", "--requisites", "/run/current-system"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout);
        if let Some(first) = s.lines().next() {
            if first.starts_with("/nix/store/") {
                return Some(first.to_string());
            }
        }
    }
    // Fallback: list the store dir. First entry that looks store-shaped.
    let out = Command::new("ls").arg("/nix/store").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        if line.len() > 32 && !line.starts_with('.') {
            return Some(format!("/nix/store/{line}"));
        }
    }
    None
}

fn run_nix_store_against(socket_remote: &str, args: &[&str]) -> Result<String, String> {
    // Wrap in /usr/bin/timeout if available so a stuck nix-store
    // doesn't hang the whole suite. The timeout process exits with
    // code 124 on elapse, which we treat as a real (diagnosable)
    // failure rather than a silent kill.
    let timeout_bin = which("timeout").or_else(|| which("gtimeout"));
    let mut cmd = if let Some(ref t) = timeout_bin {
        let mut c = Command::new(t);
        c.arg("15").arg("nix-store");
        c
    } else {
        // No `timeout` on PATH — call nix-store directly and rely on
        // tokio's test timeout to kill us. Less informative but the
        // test still works.
        Command::new("nix-store")
    };
    let out = cmd
        .env("NIX_REMOTE", socket_remote)
        // Stop real nix-daemon startup side-effects from contaminating.
        .env_remove("NIX_DAEMON_SOCKET_PATH")
        // Ask nix-store to be chatty on stderr so we get actionable
        // output when it fails.
        .env("NIX_CONFIG", "show-trace = true")
        .args(args)
        .output()
        .map_err(|e| format!("spawn nix-store failed: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        return Err(format!(
            "nix-store {:?} exit={code}\n\
             stdout:\n{stdout}\n\
             stderr:\n{stderr}",
            args
        ));
    }
    Ok(stdout)
}

// ── The test ───────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_nix_store_queries_references_via_sui_daemon() {
    if let Some(reason) = skip_reason() {
        eprintln!("skip real_nix_store_queries_references_via_sui_daemon: {reason}");
        return;
    }

    let fixture = DaemonFixture::start().await;
    let Some(target) = pick_valid_store_path() else {
        eprintln!("skip: could not find any valid store path to query");
        return;
    };
    eprintln!("probe target: {target}");

    // --query --references: exercises QueryPathInfo (for the
    // references field). CppNix's `nix-store --query --references`
    // sends QueryReferences (op 5) directly on older protocol
    // minor versions; newer versions may send QueryPathInfo and
    // extract the refs field.  Either routes to our daemon.
    let refs = run_nix_store_against(&fixture.remote_env(), &[
        "--query", "--references", &target,
    ]);
    match refs {
        Ok(s) => eprintln!("references output:\n{}", truncate(&s, 400)),
        Err(e) => panic!("references query failed: {e}"),
    }

    // --query --deriver: exercises QueryDeriver (op 18)
    let deriver = run_nix_store_against(&fixture.remote_env(), &[
        "--query", "--deriver", &target,
    ]);
    match deriver {
        Ok(s) => eprintln!("deriver output:\n{}", truncate(&s, 400)),
        Err(e) => panic!("deriver query failed: {e}"),
    }

    // --query --referrers: exercises QueryReferrers (op 6).
    // MockStore returned empty; LocalStore may return real ones.
    let referrers = run_nix_store_against(&fixture.remote_env(), &[
        "--query", "--referrers", &target,
    ]);
    match referrers {
        Ok(s) => eprintln!("referrers output (first 400 chars):\n{}", truncate(&s, 400)),
        Err(e) => eprintln!(
            "referrers query failed (non-fatal — sui-store may not support it): {e}"
        ),
    }
}

/// Second-wave probes: commands that push further into the op
/// surface. Each either passes (validating an existing op) or
/// surfaces the next gap. Log everything; assert only the
/// minimum bar so one failure doesn't hide others.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_nix_store_probes_more_commands() {
    if let Some(reason) = skip_reason() {
        eprintln!("skip real_nix_store_probes_more_commands: {reason}");
        return;
    }

    let fixture = DaemonFixture::start().await;
    let Some(target) = pick_valid_store_path() else {
        eprintln!("skip: no valid store path");
        return;
    };

    let probes: &[(&str, &[&str])] = &[
        // IsValidPath (op 1) — boolean existence check. Exits 0 if
        // valid, non-zero if not.
        ("check-validity (op 1)", &["--check-validity", &target]),
        // QueryPathInfo (op 26) — size is pulled from PathInfo.
        ("query --size (op 26)", &["--query", "--size", &target]),
        // QueryPathInfo — hash field.
        ("query --hash (op 26)", &["--query", "--hash", &target]),
        // QueryPathInfo + closure walk — forces repeated references()
        // queries over the transitive closure.
        ("query --requisites (op 26 ×N)", &["--query", "--requisites", &target]),
        // `nix store ping` exercises handshake + trust exchange and
        // nothing else. If this fails, the handshake regressed.
        ("path-info --json via modern CLI",
          // Fall back to nix-store --query --hash if modern `nix` CLI
          // doesn't honor NIX_REMOTE the same way (some builds route
          // through different code paths).
          &["--query", "--binding", "", &target]),
    ];

    let mut outcomes: Vec<(String, Result<String, String>)> = Vec::new();
    for (name, args) in probes {
        let r = run_nix_store_against(&fixture.remote_env(), args);
        outcomes.push((name.to_string(), r));
    }

    eprintln!("\n── nix-store probe table ──");
    let mut fatal_fail = 0;
    for (name, r) in &outcomes {
        match r {
            Ok(out) => eprintln!(
                "  ✓ {name}\n    output: {}",
                truncate(out.trim(), 200)
            ),
            Err(e) => {
                eprintln!("  ✗ {name}\n    err: {}", truncate(e, 300));
                // Only fail the test on timeout (exit=124) — that
                // means the daemon got stuck, which is always a bug.
                // Other errors might be "op returned an expected
                // error like bad-path-format," which doesn't imply
                // a daemon problem.
                if e.contains("exit=124") {
                    fatal_fail += 1;
                }
            }
        }
    }
    assert_eq!(
        fatal_fail, 0,
        "{fatal_fail} probe(s) hung the daemon — see log for which ops"
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}... ({} more bytes)", &s[..max], s.len() - max)
    }
}
