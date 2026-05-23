//! Typed dual-subprocess runner — the shared substrate every ParityCheck
//! rides on.
//!
//! The original `sui-sweep` ran two subprocesses inline.  Reaching for the
//! same shape from a second site (the rebuild-probe sweep, the eventual
//! `sui rebuild-shadow` subcommand, the operator-facing `fleet rebuild
//! --shadow-sui` wrapper) makes this the canonical place to put it.
//!
//! Two construction guarantees this module pins down:
//!
//! 1. **NO SHELL.** Every subprocess is built with typed `Command`
//!    pieces.  There is no `bash -c` anywhere in the parity path.
//! 2. **Timeout is mandatory.** Every invocation goes through
//!    [`run_with_timeout`], which SIGKILLs the child after `timeout`.
//!    The parity harness must never hang on a runaway evaluator.

use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Captured output of one subprocess invocation.
///
/// Carries enough information for the sweep report to be self-contained
/// without re-running anything: both stdout and stderr are retained, the
/// duration is wall-clock between spawn and reap, and `timed_out` is true
/// iff the watchdog had to SIGKILL the child.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedOutput {
    /// Process exit code, or `None` if the OS didn't surface one
    /// (signal-killed, including our own SIGKILL on timeout).
    pub exit_code: Option<i32>,
    /// `true` iff `exit_code == Some(0)`.  Pre-computed so callers
    /// don't have to remember the unwrap.
    pub success: bool,
    /// Standard output, lossy-decoded as UTF-8 (Nix only emits ASCII
    /// or valid UTF-8 in practice; we never silently swallow bytes).
    pub stdout: String,
    /// Standard error, lossy-decoded as UTF-8.
    pub stderr: String,
    /// Wall-clock duration of the invocation.
    pub duration: Duration,
    /// `true` iff the watchdog had to kill the child.
    pub timed_out: bool,
}

impl CapturedOutput {
    /// Build a [`CapturedOutput`] from the raw parts the standard
    /// library produces.
    #[must_use]
    pub fn from_parts(
        status: ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        duration: Duration,
        timed_out: bool,
    ) -> Self {
        let exit_code = status.code();
        let success = exit_code == Some(0);
        Self {
            exit_code,
            success,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            duration,
            timed_out,
        }
    }

    /// Build an output representing a spawn failure (binary missing,
    /// `EACCES`, etc.).  The probe still gets recorded; the verdict
    /// will be a "fail-only" against whichever side blew up.
    #[must_use]
    pub fn spawn_failure(message: String, duration: Duration) -> Self {
        Self {
            exit_code: None,
            success: false,
            stdout: String::new(),
            stderr: format!("spawn failed: {message}"),
            duration,
            timed_out: false,
        }
    }
}

/// Render a [`Command`] back to a typed argv vector for the report.
///
/// `Command::get_args` returns `OsStr`s; we lossily convert because
/// the report is JSON and JSON can't represent non-UTF-8 anyway.
/// Operators reading the report on a screen never miss anything that
/// matters.
#[must_use]
pub fn command_argv(cmd: &Command) -> Vec<String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut argv = vec![program];
    for arg in cmd.get_args() {
        argv.push(arg.to_string_lossy().into_owned());
    }
    argv
}

/// Run a single command with a hard SIGKILL on timeout.
///
/// Watchdog is a one-shot thread parked on a `recv_timeout` against an
/// mpsc channel; on timeout it calls `libc::kill(pid, SIGKILL)`.  On
/// success the main thread sends a completion message which causes the
/// watchdog to exit harmlessly.
///
/// # Errors
///
/// Returns `Err` only if `spawn()` itself fails.  Subprocess non-zero
/// exits + timeouts are surfaced through the [`CapturedOutput`].
pub fn run_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
) -> std::io::Result<CapturedOutput> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let start = Instant::now();
    let child = cmd.spawn()?;
    let pid = child.id();

    let (tx, rx) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || -> bool {
        // Returns true iff the watchdog fired (i.e. timeout reached).
        match rx.recv_timeout(timeout) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => false,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                #[cfg(unix)]
                // SAFETY: `libc::kill` with SIGKILL and a freshly-spawned
                // child PID is sound; the worst case is a no-op if the
                // child already exited (kernel reuses PIDs only after
                // reap).
                unsafe { libc::kill(pid as i32, libc::SIGKILL); }
                true
            }
        }
    });

    let output = child.wait_with_output()?;
    // Tell the watchdog we're done; ignore send errors (it may already
    // have fired and dropped the rx).
    let _ = tx.send(());
    let timed_out = watchdog.join().unwrap_or(false);
    let duration = start.elapsed();

    Ok(CapturedOutput::from_parts(
        output.status,
        output.stdout,
        output.stderr,
        duration,
        timed_out,
    ))
}

/// Run two commands sequentially under the same timeout each, returning
/// `(sui_output, nix_output)`.
///
/// Sequential (not parallel) on purpose: nix's eval cache lock, sui's
/// store-write lock, and disk-IO contention all behave better when the
/// engines don't race for the same FS resources.  Wall-clock loss is
/// small relative to a 30 s timeout budget.
///
/// # Errors
///
/// Returns the first spawn error encountered; both invocations are still
/// attempted independently — a sui spawn failure does not skip nix.
pub fn dual_run(
    sui: &mut Command,
    nix: &mut Command,
    timeout: Duration,
) -> (CapturedOutput, CapturedOutput) {
    let sui_out = match run_with_timeout(sui, timeout) {
        Ok(out) => out,
        Err(e) => CapturedOutput::spawn_failure(e.to_string(), Duration::ZERO),
    };
    let nix_out = match run_with_timeout(nix, timeout) {
        Ok(out) => out,
        Err(e) => CapturedOutput::spawn_failure(e.to_string(), Duration::ZERO),
    };
    (sui_out, nix_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_output_success_marks_success_flag() {
        let out = CapturedOutput::from_parts(
            fake_exit(0),
            b"hi".to_vec(),
            b"".to_vec(),
            Duration::from_millis(1),
            false,
        );
        assert!(out.success);
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.stdout, "hi");
    }

    #[test]
    fn captured_output_nonzero_is_not_success() {
        let out = CapturedOutput::from_parts(
            fake_exit(1),
            b"".to_vec(),
            b"oops".to_vec(),
            Duration::from_millis(1),
            false,
        );
        assert!(!out.success);
        assert_eq!(out.exit_code, Some(1));
    }

    #[test]
    fn spawn_failure_records_message() {
        let out = CapturedOutput::spawn_failure("no such file".into(), Duration::ZERO);
        assert!(!out.success);
        assert!(out.stderr.contains("no such file"));
        assert_eq!(out.exit_code, None);
    }

    #[test]
    fn command_argv_renders_program_and_args() {
        let mut cmd = Command::new("/bin/echo");
        cmd.args(["hello", "world"]);
        let argv = command_argv(&cmd);
        assert_eq!(argv, vec!["/bin/echo", "hello", "world"]);
    }

    #[test]
    fn timeout_kills_runaway_child() {
        // Skip the test on platforms without `/bin/sleep` (windows).
        if !std::path::Path::new("/bin/sleep").exists() {
            return;
        }
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30");
        let out = run_with_timeout(&mut cmd, Duration::from_millis(100))
            .expect("spawn should succeed");
        assert!(out.timed_out, "watchdog must have fired");
        // SIGKILL doesn't produce an exit code; the OS surfaces None.
        assert!(out.exit_code.is_none() || out.exit_code == Some(-1));
    }

    fn fake_exit(code: i32) -> ExitStatus {
        // Use `false` (exit 1) or `true` (exit 0) since rust's ExitStatus
        // has no public constructor on stable.  We invoke the binary that
        // matches the desired code; this is test-only.
        let bin = if code == 0 { "/usr/bin/true" } else { "/usr/bin/false" };
        let alt = if code == 0 { "/bin/true" } else { "/bin/false" };
        let path = if std::path::Path::new(bin).exists() { bin } else { alt };
        std::process::Command::new(path)
            .status()
            .expect("status must succeed")
    }
}
