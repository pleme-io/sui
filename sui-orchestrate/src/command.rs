//! Command execution abstraction.
//!
//! Defines the [`CommandRunner`] trait so orchestrators can be tested
//! without spawning real processes.

use std::process::Stdio;

/// Output from a command execution.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Whether the command exited successfully (exit code 0).
    pub success: bool,
    /// Combined stdout content.
    pub stdout: String,
    /// Combined stderr content.
    pub stderr: String,
    /// Process exit code (if available).
    pub exit_code: Option<i32>,
}

impl CommandOutput {
    /// Concatenate stdout and stderr into a single log string.
    #[must_use]
    pub fn combined_log(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Command execution errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CommandError {
    /// The command could not be found.
    #[error("command not found: {0}")]
    NotFound(String),
    /// An I/O error occurred during execution.
    #[error("io error: {0}")]
    Io(String),
    /// The command was killed or otherwise failed to complete.
    #[error("execution failed: {0}")]
    Failed(String),
}

/// Async command runner trait — abstracts over `tokio::process::Command`.
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    /// Execute a program with the given arguments and return the output.
    async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, CommandError>;
}

/// Default [`CommandRunner`] backed by `tokio::process::Command`.
pub struct TokioCommandRunner;

impl TokioCommandRunner {
    /// Create a new runner.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for TokioCommandRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl CommandRunner for TokioCommandRunner {
    async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, CommandError> {
        let output = tokio::process::Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    CommandError::NotFound(program.to_string())
                } else {
                    CommandError::Io(e.to_string())
                }
            })?;

        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_error_display() {
        let e = CommandError::NotFound("nix".to_string());
        assert!(e.to_string().contains("nix"));

        let e = CommandError::Io("broken pipe".to_string());
        assert!(e.to_string().contains("broken pipe"));
    }

    #[test]
    fn tokio_command_runner_default() {
        let _runner = TokioCommandRunner::default();
    }

    #[tokio::test]
    async fn run_echo() {
        let runner = TokioCommandRunner::new();
        let output = runner.run("echo", &["hello"]).await.unwrap();
        assert!(output.success);
        assert!(output.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn run_missing_command() {
        let runner = TokioCommandRunner::new();
        let result = runner
            .run("__nonexistent_command_12345__", &[])
            .await;
        assert!(result.is_err());
    }

    // ── CommandOutput construction ────────────────────────────

    #[test]
    fn command_output_success_construction() {
        let output = CommandOutput {
            success: true,
            stdout: "hello world\n".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
        };
        assert!(output.success);
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn command_output_failure_construction() {
        let output = CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: "permission denied\n".to_string(),
            exit_code: Some(1),
        };
        assert!(!output.success);
        assert_eq!(output.exit_code, Some(1));
        assert!(output.stderr.contains("permission denied"));
    }

    #[test]
    fn command_output_no_exit_code() {
        let output = CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: "killed by signal".to_string(),
            exit_code: None,
        };
        assert!(output.exit_code.is_none());
    }

    // ── CommandError display ─────────────────────────────────

    #[test]
    fn command_error_failed_display() {
        let e = CommandError::Failed("timed out".to_string());
        assert!(e.to_string().contains("timed out"));
        assert!(e.to_string().contains("execution failed"));
    }

    // ── CommandRunner trait: object safety ────────────────────

    #[test]
    fn command_runner_trait_is_object_safe() {
        fn assert_obj_safe(_: &dyn CommandRunner) {}
        assert_obj_safe(&TokioCommandRunner::new());
    }

    // ── CommandOutput clone ──────────────────────────────────

    #[test]
    fn command_output_clone() {
        let output = CommandOutput {
            success: true,
            stdout: "data".to_string(),
            stderr: "warn".to_string(),
            exit_code: Some(0),
        };
        let cloned = output.clone();
        assert_eq!(cloned.success, output.success);
        assert_eq!(cloned.stdout, output.stdout);
        assert_eq!(cloned.stderr, output.stderr);
        assert_eq!(cloned.exit_code, output.exit_code);
    }

    // ── TokioCommandRunner: exit code ─────────────────────────

    #[tokio::test]
    async fn run_false_returns_failure() {
        let runner = TokioCommandRunner::new();
        let output = runner.run("false", &[]).await.unwrap();
        assert!(!output.success);
        assert_eq!(output.exit_code, Some(1));
    }

    #[tokio::test]
    async fn run_true_returns_success() {
        let runner = TokioCommandRunner::new();
        let output = runner.run("true", &[]).await.unwrap();
        assert!(output.success);
        assert_eq!(output.exit_code, Some(0));
    }

    // ── TokioCommandRunner: captures stderr ───────────────────

    #[tokio::test]
    async fn run_captures_stderr() {
        let runner = TokioCommandRunner::new();
        let output = runner.run("sh", &["-c", "echo err >&2"]).await.unwrap();
        assert!(output.stderr.contains("err"));
    }

    // ── CommandError Debug ────────────────────────────────────

    #[test]
    fn command_error_debug() {
        let e = CommandError::NotFound("foo".to_string());
        let dbg = format!("{e:?}");
        assert!(dbg.contains("NotFound"));
        assert!(dbg.contains("foo"));
    }

    // ── CommandOutput Debug ───────────────────────────────────

    #[test]
    fn command_output_debug() {
        let output = CommandOutput {
            success: true,
            stdout: "hi".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
        };
        let dbg = format!("{output:?}");
        assert!(dbg.contains("success: true"));
    }

    // ── combined_log layout ───────────────────────────────────

    #[test]
    fn combined_log_concatenates_stdout_then_stderr() {
        let output = CommandOutput {
            success: true,
            stdout: "out-content".to_string(),
            stderr: "err-content".to_string(),
            exit_code: Some(0),
        };
        let combined = output.combined_log();
        assert_eq!(combined, "out-contenterr-content");
        let out_pos = combined.find("out-content").unwrap();
        let err_pos = combined.find("err-content").unwrap();
        assert!(out_pos < err_pos);
    }

    #[test]
    fn combined_log_empty_inputs() {
        let output = CommandOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        };
        assert_eq!(output.combined_log(), "");
    }

    #[test]
    fn combined_log_only_stderr() {
        let output = CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: "boom".to_string(),
            exit_code: Some(2),
        };
        assert_eq!(output.combined_log(), "boom");
    }

    // ── CommandError negative-test coverage for every variant ──

    #[test]
    fn command_error_io_variant_message() {
        let e = CommandError::Io("permission denied".to_string());
        let s = e.to_string();
        assert!(s.contains("io error"));
        assert!(s.contains("permission denied"));
    }

    #[test]
    fn command_error_failed_variant_message() {
        let e = CommandError::Failed("oom kill".to_string());
        let s = e.to_string();
        assert!(s.contains("execution failed"));
        assert!(s.contains("oom kill"));
    }

    #[test]
    fn command_error_not_found_variant_message() {
        let e = CommandError::NotFound("missing-tool".to_string());
        let s = e.to_string();
        assert!(s.contains("command not found"));
        assert!(s.contains("missing-tool"));
    }

    // ── TokioCommandRunner: arg passthrough ────────────────────

    #[tokio::test]
    async fn run_passes_multiple_args() {
        let runner = TokioCommandRunner::new();
        let output = runner
            .run("sh", &["-c", "echo arg-one arg-two"])
            .await
            .unwrap();
        assert!(output.success);
        assert!(output.stdout.contains("arg-one"));
        assert!(output.stdout.contains("arg-two"));
    }

    #[tokio::test]
    async fn run_propagates_nonzero_exit() {
        let runner = TokioCommandRunner::new();
        let output = runner
            .run("sh", &["-c", "exit 7"])
            .await
            .unwrap();
        assert!(!output.success);
        assert_eq!(output.exit_code, Some(7));
    }

    #[tokio::test]
    async fn run_missing_command_yields_not_found_variant() {
        let runner = TokioCommandRunner::new();
        let result = runner
            .run("__definitely_no_such_program__", &[])
            .await;
        match result.unwrap_err() {
            CommandError::NotFound(name) => assert!(name.contains("__definitely_no_such_program__")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
