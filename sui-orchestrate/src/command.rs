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

/// Command execution errors.
#[derive(Debug, thiserror::Error)]
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
}
