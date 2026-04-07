//! Core builder trait and types.
//!
//! Defines the [`Builder`] trait, build lifecycle types ([`BuildState`],
//! [`BuildResult`]), the [`BuildLog`] accumulator, and the [`BuildError`]
//! error enum used throughout the crate.

use sui_compat::derivation::Derivation;
use sui_compat::store_path::StorePath;

use crate::sandbox::SandboxError;

// ── Build state machine ──────────────────────────────────────────

/// Tracks the lifecycle of a single build.
///
/// State transitions: `Pending → Building → Succeeded | Failed`.
/// Invalid transitions (e.g. `Succeeded → Building`) are prevented by
/// returning an error from [`BuildState::transition`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BuildState {
    /// Build is queued but has not started.
    Pending,
    /// Build is currently executing.
    Building,
    /// Build completed successfully.
    Succeeded,
    /// Build finished with an error.
    Failed(String),
}

impl BuildState {
    /// Attempt a state transition, returning an error on invalid moves.
    pub fn transition(&mut self, next: BuildState) -> Result<(), BuildError> {
        let valid = matches!(
            (&*self, &next),
            (Self::Pending, Self::Building)
                | (Self::Building, Self::Succeeded)
                | (Self::Building, Self::Failed(_))
        );
        if valid {
            *self = next;
            Ok(())
        } else {
            Err(BuildError::Failed(format!(
                "invalid state transition: {self:?} → {next:?}",
            )))
        }
    }

    /// Returns `true` if the build has finished (succeeded or failed).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed(_))
    }
}

impl std::fmt::Display for BuildState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Building => write!(f, "building"),
            Self::Succeeded => write!(f, "succeeded"),
            Self::Failed(reason) => write!(f, "failed: {reason}"),
        }
    }
}

impl std::str::FromStr for BuildState {
    type Err = BuildError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(reason) = s.strip_prefix("failed: ") {
            return Ok(Self::Failed(reason.to_owned()));
        }
        match s {
            "pending" => Ok(Self::Pending),
            "building" => Ok(Self::Building),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed(String::new())),
            _ => Err(BuildError::Failed(format!("unknown build state: {s}"))),
        }
    }
}

// ── Build log ────────────────────────────────────────────────────

/// Structured build log accumulator.
///
/// Collects timestamped log lines during a build. The final log text
/// is retrievable via [`BuildLog::finish`].
#[derive(Debug, Clone, Default)]
pub struct BuildLog {
    lines: Vec<String>,
}

impl BuildLog {
    /// Create an empty build log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a line to the log.
    pub fn push(&mut self, line: &str) {
        self.lines.push(line.to_owned());
    }

    /// Append multiple lines at once.
    pub fn extend(&mut self, lines: &[&str]) {
        self.lines.extend(lines.iter().map(|l| (*l).to_owned()));
    }

    /// Iterate over logged lines.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }

    /// Return the number of lines logged.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Return `true` if no lines have been logged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Consume the log and return the joined text.
    #[must_use]
    pub fn finish(self) -> String {
        self.lines.join("\n")
    }
}

// ── Build outcome ───────────────────────────────────────────────

/// Typed outcome of a build execution, replacing the boolean `success` field.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum BuildOutcome {
    /// The build finished successfully.
    Success,
    /// The build process exited with a non-zero code.
    Failure {
        /// Captured stderr from the build process.
        stderr: String,
        /// Process exit code (non-zero).
        exit_code: i32,
    },
    /// The build was cancelled before completion.
    Cancelled,
}

impl BuildOutcome {
    /// Returns `true` if the build succeeded.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// Returns `true` if the build failed (not cancelled).
    #[must_use]
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failure { .. })
    }
}

impl std::fmt::Display for BuildOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success => write!(f, "success"),
            Self::Failure { exit_code, .. } => write!(f, "failure (exit code {exit_code})"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

// ── Build result ─────────────────────────────────────────────────

/// Build execution result.
#[derive(Debug, Clone)]
pub struct BuildResult {
    /// Output store paths produced by the build.
    pub outputs: Vec<StorePath>,
    /// Build log.
    pub log: String,
    /// Whether the build succeeded.
    pub success: bool,
    /// Typed build outcome with richer failure information.
    pub outcome: BuildOutcome,
    /// Wall-clock time in seconds.
    pub duration_secs: f64,
}

// ── Errors ───────────────────────────────────────────────────────

/// Build errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    /// The build command itself returned a failure.
    #[error("build failed: {0}")]
    Failed(String),
    /// The sandbox could not be configured or the sandboxed process failed.
    #[error("sandbox error: {0}")]
    Sandbox(#[from] SandboxError),
    /// The derivation is invalid or cannot be processed.
    #[error("derivation error: {0}")]
    Derivation(String),
    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Requested functionality is not yet available.
    #[error("not implemented: {0}")]
    NotImplemented(String),
}

// ── Builder trait ────────────────────────────────────────────────

/// The core builder interface.
///
/// Implementations run a derivation's builder command (optionally inside
/// a sandbox) and return a [`BuildResult`] on success.
#[allow(async_fn_in_trait)]
pub trait Builder: Send + Sync {
    /// Build a derivation and return the result.
    async fn build(&self, drv: &Derivation) -> Result<BuildResult, BuildError>;

    /// Check if an output path already exists (skip build).
    async fn output_exists(&self, path: &StorePath) -> Result<bool, BuildError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_error_failed_display() {
        let e = BuildError::Failed("compiler error".to_string());
        assert!(e.to_string().contains("compiler error"));
        assert!(e.to_string().contains("build failed"));
    }

    #[test]
    fn build_error_sandbox_display() {
        let inner = crate::sandbox::SandboxError::Setup("mount failed".to_string());
        let e = BuildError::Sandbox(inner);
        assert!(e.to_string().contains("mount failed"));
        assert!(e.to_string().contains("sandbox"));
    }

    #[test]
    fn build_error_derivation_display() {
        let e = BuildError::Derivation("parse error".to_string());
        assert!(e.to_string().contains("parse error"));
    }

    #[test]
    fn build_error_not_implemented_display() {
        let e = BuildError::NotImplemented("remote build".to_string());
        assert!(e.to_string().contains("remote build"));
    }

    #[test]
    fn build_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let build_err: BuildError = io_err.into();
        assert!(build_err.to_string().contains("no such file"));
    }

    #[test]
    fn build_result_construction_success() {
        let output = StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();

        let result = BuildResult {
            outputs: vec![output.clone()],
            log: "build succeeded\n".to_string(),
            success: true,
            outcome: BuildOutcome::Success,
            duration_secs: 42.5,
        };

        assert!(result.success);
        assert!(result.outcome.is_success());
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(
            result.outputs[0].to_absolute_path(),
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1"
        );
        assert!(result.log.contains("build succeeded"));
        assert!((result.duration_secs - 42.5).abs() < f64::EPSILON);
    }

    #[test]
    fn build_result_construction_failure() {
        let result = BuildResult {
            outputs: vec![],
            log: "error: builder for '/nix/store/abc.drv' failed\n".to_string(),
            success: false,
            outcome: BuildOutcome::Failure {
                stderr: "builder failed".to_string(),
                exit_code: 1,
            },
            duration_secs: 1.2,
        };

        assert!(!result.success);
        assert!(result.outcome.is_failure());
        assert!(result.outputs.is_empty());
        assert!(result.log.contains("failed"));
    }

    #[test]
    fn build_result_clone() {
        let result = BuildResult {
            outputs: vec![],
            log: "ok".to_string(),
            success: true,
            outcome: BuildOutcome::Success,
            duration_secs: 0.0,
        };
        let cloned = result.clone();
        assert_eq!(cloned.success, result.success);
        assert_eq!(cloned.log, result.log);
    }

    // ── BuildState tests ──────────────────────────────────────────

    #[test]
    fn build_state_pending_to_building() {
        let mut state = BuildState::Pending;
        assert!(state.transition(BuildState::Building).is_ok());
        assert_eq!(state, BuildState::Building);
    }

    #[test]
    fn build_state_building_to_succeeded() {
        let mut state = BuildState::Building;
        assert!(state.transition(BuildState::Succeeded).is_ok());
        assert_eq!(state, BuildState::Succeeded);
    }

    #[test]
    fn build_state_building_to_failed() {
        let mut state = BuildState::Building;
        assert!(state.transition(BuildState::Failed("oops".into())).is_ok());
        assert_eq!(state, BuildState::Failed("oops".into()));
    }

    #[test]
    fn build_state_pending_to_succeeded_is_invalid() {
        let mut state = BuildState::Pending;
        assert!(state.transition(BuildState::Succeeded).is_err());
        assert_eq!(state, BuildState::Pending);
    }

    #[test]
    fn build_state_succeeded_to_building_is_invalid() {
        let mut state = BuildState::Succeeded;
        assert!(state.transition(BuildState::Building).is_err());
        assert_eq!(state, BuildState::Succeeded);
    }

    #[test]
    fn build_state_failed_to_building_is_invalid() {
        let mut state = BuildState::Failed("x".into());
        assert!(state.transition(BuildState::Building).is_err());
    }

    #[test]
    fn build_state_pending_to_failed_is_invalid() {
        let mut state = BuildState::Pending;
        assert!(state.transition(BuildState::Failed("e".into())).is_err());
    }

    #[test]
    fn build_state_building_to_pending_is_invalid() {
        let mut state = BuildState::Building;
        assert!(state.transition(BuildState::Pending).is_err());
    }

    #[test]
    fn build_state_is_terminal() {
        assert!(!BuildState::Pending.is_terminal());
        assert!(!BuildState::Building.is_terminal());
        assert!(BuildState::Succeeded.is_terminal());
        assert!(BuildState::Failed("x".into()).is_terminal());
    }

    #[test]
    fn build_state_display() {
        assert_eq!(BuildState::Pending.to_string(), "pending");
        assert_eq!(BuildState::Building.to_string(), "building");
        assert_eq!(BuildState::Succeeded.to_string(), "succeeded");
        assert_eq!(BuildState::Failed("oom".into()).to_string(), "failed: oom");
    }

    #[test]
    fn build_state_clone_eq() {
        let a = BuildState::Failed("reason".into());
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn build_state_full_lifecycle() {
        let mut state = BuildState::Pending;
        assert!(!state.is_terminal());
        state.transition(BuildState::Building).unwrap();
        assert!(!state.is_terminal());
        state.transition(BuildState::Succeeded).unwrap();
        assert!(state.is_terminal());
    }

    #[test]
    fn build_state_full_lifecycle_failure() {
        let mut state = BuildState::Pending;
        state.transition(BuildState::Building).unwrap();
        state.transition(BuildState::Failed("segfault".into())).unwrap();
        assert!(state.is_terminal());
        assert_eq!(state.to_string(), "failed: segfault");
    }

    // ── BuildLog tests ──────────────────────────────────────────

    #[test]
    fn build_log_new_is_empty() {
        let log = BuildLog::new();
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn build_log_push_single_line() {
        let mut log = BuildLog::new();
        log.push("building hello-2.12.1");
        assert_eq!(log.len(), 1);
        assert!(!log.is_empty());
    }

    #[test]
    fn build_log_push_multiple_lines() {
        let mut log = BuildLog::new();
        log.push("configuring...");
        log.push("building...");
        log.push("installing...");
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn build_log_extend() {
        let mut log = BuildLog::new();
        log.extend(&["line1", "line2", "line3"]);
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn build_log_finish_joins_with_newline() {
        let mut log = BuildLog::new();
        log.push("hello");
        log.push("world");
        assert_eq!(log.finish(), "hello\nworld");
    }

    #[test]
    fn build_log_finish_empty() {
        let log = BuildLog::new();
        assert_eq!(log.finish(), "");
    }

    #[test]
    fn build_log_clone() {
        let mut log = BuildLog::new();
        log.push("test line");
        let cloned = log.clone();
        assert_eq!(cloned.len(), 1);
        assert_eq!(cloned.finish(), "test line");
    }

    #[test]
    fn build_log_default() {
        let log: BuildLog = Default::default();
        assert!(log.is_empty());
    }

    // ── BuildError conversion tests ─────────────────────────────

    #[test]
    fn build_error_from_sandbox_error() {
        let sandbox_err = crate::sandbox::SandboxError::Execution("timeout".into());
        let build_err: BuildError = sandbox_err.into();
        assert!(build_err.to_string().contains("timeout"));
    }

    // ── MockBuilder through Builder trait → BuildResult → API BuildStatus ──

    use crate::sandbox::{NoSandbox, Sandbox, SandboxConfig};

    /// A mock builder that uses NoSandbox internally and produces a BuildResult.
    struct MockBuilder;

    impl MockBuilder {
        async fn do_build(&self, drv: &Derivation) -> Result<BuildResult, BuildError> {
            let sandbox = NoSandbox;
            let config = SandboxConfig::from_derivation(drv, "/tmp");

            let result = sandbox.execute(&config)?;

            let outputs: Vec<StorePath> = drv
                .outputs
                .values()
                .filter_map(|o| StorePath::from_absolute_path(&o.path).ok())
                .collect();

            let success = result.exit_code == 0;
            let outcome = if success {
                BuildOutcome::Success
            } else {
                BuildOutcome::Failure {
                    stderr: String::from_utf8_lossy(&result.stderr).to_string(),
                    exit_code: result.exit_code,
                }
            };

            Ok(BuildResult {
                outputs,
                log: String::from_utf8_lossy(&result.stdout).to_string(),
                success,
                outcome,
                duration_secs: 0.1,
            })
        }
    }

    impl Builder for MockBuilder {
        async fn build(&self, drv: &Derivation) -> Result<BuildResult, BuildError> {
            self.do_build(drv).await
        }

        async fn output_exists(&self, _path: &StorePath) -> Result<bool, BuildError> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn mock_builder_through_trait_success() {
        use sui_compat::derivation::{Derivation, DerivationOutput};
        use std::collections::BTreeMap;

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );

        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "aarch64-darwin".to_string(),
            builder: "/bin/echo".to_string(),
            args: vec!["build-complete".to_string()],
            env: BTreeMap::new(),
        };

        let builder = MockBuilder;
        let result = builder.build(&drv).await.unwrap();
        assert!(result.success);
        assert_eq!(result.outputs.len(), 1);
        assert!(result.log.contains("build-complete"));
    }

    #[tokio::test]
    async fn mock_builder_result_to_api_build_status() {
        let output = StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();

        let result = BuildResult {
            outputs: vec![output],
            log: "building...\nfinished\n".to_string(),
            success: true,
            outcome: BuildOutcome::Success,
            duration_secs: 30.0,
        };

        assert!(result.success);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(
            result.outputs[0].to_absolute_path(),
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        );
        assert_eq!(result.log.lines().count(), 2);
    }

    #[tokio::test]
    async fn mock_builder_output_exists() {
        let builder = MockBuilder;
        let path = StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();
        assert!(!builder.output_exists(&path).await.unwrap());
    }

    #[tokio::test]
    async fn mock_builder_sandbox_failure_becomes_build_error() {
        use sui_compat::derivation::{Derivation, DerivationOutput};
        use std::collections::BTreeMap;

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );

        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "aarch64-darwin".to_string(),
            builder: "/nonexistent/builder/12345".to_string(),
            args: vec![],
            env: BTreeMap::new(),
        };

        let builder = MockBuilder;
        let result = builder.build(&drv).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            BuildError::Sandbox(inner) => assert!(!inner.to_string().is_empty()),
            other => panic!("expected Sandbox error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_builder_failing_command_produces_failure_result() {
        use sui_compat::derivation::{Derivation, DerivationOutput};
        use std::collections::BTreeMap;

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );

        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "aarch64-darwin".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "exit 1".to_string()],
            env: BTreeMap::new(),
        };

        let builder = MockBuilder;
        let result = builder.build(&drv).await.unwrap();
        assert!(!result.success);
        assert!(result.outcome.is_failure());
    }

    // ── BuildOutcome tests ──────────────────────────────────

    #[test]
    fn build_outcome_success_display() {
        assert_eq!(BuildOutcome::Success.to_string(), "success");
    }

    #[test]
    fn build_outcome_failure_display() {
        let outcome = BuildOutcome::Failure {
            stderr: "err".to_string(),
            exit_code: 42,
        };
        assert_eq!(outcome.to_string(), "failure (exit code 42)");
    }

    #[test]
    fn build_outcome_cancelled_display() {
        assert_eq!(BuildOutcome::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn build_outcome_is_success() {
        assert!(BuildOutcome::Success.is_success());
        assert!(!BuildOutcome::Cancelled.is_success());
        assert!(!BuildOutcome::Failure {
            stderr: String::new(),
            exit_code: 1
        }
        .is_success());
    }

    #[test]
    fn build_outcome_is_failure() {
        assert!(!BuildOutcome::Success.is_failure());
        assert!(!BuildOutcome::Cancelled.is_failure());
        assert!(BuildOutcome::Failure {
            stderr: String::new(),
            exit_code: 1
        }
        .is_failure());
    }

    #[test]
    fn build_outcome_clone_eq() {
        let a = BuildOutcome::Failure {
            stderr: "err".to_string(),
            exit_code: 2,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // ── BuildState FromStr round-trip tests ─────────────────

    #[test]
    fn build_state_fromstr_pending() {
        let state: BuildState = "pending".parse().unwrap();
        assert_eq!(state, BuildState::Pending);
        assert_eq!(state.to_string(), "pending");
    }

    #[test]
    fn build_state_fromstr_building() {
        let state: BuildState = "building".parse().unwrap();
        assert_eq!(state, BuildState::Building);
    }

    #[test]
    fn build_state_fromstr_succeeded() {
        let state: BuildState = "succeeded".parse().unwrap();
        assert_eq!(state, BuildState::Succeeded);
    }

    #[test]
    fn build_state_fromstr_failed_with_reason() {
        let state: BuildState = "failed: out of memory".parse().unwrap();
        assert_eq!(state, BuildState::Failed("out of memory".to_string()));
        assert_eq!(state.to_string(), "failed: out of memory");
    }

    #[test]
    fn build_state_fromstr_failed_bare() {
        let state: BuildState = "failed".parse().unwrap();
        assert_eq!(state, BuildState::Failed(String::new()));
    }

    #[test]
    fn build_state_fromstr_invalid() {
        let result: Result<BuildState, _> = "unknown".parse();
        assert!(result.is_err());
    }

    #[test]
    fn build_state_display_fromstr_roundtrip() {
        for state in [
            BuildState::Pending,
            BuildState::Building,
            BuildState::Succeeded,
            BuildState::Failed("oom".to_string()),
        ] {
            let s = state.to_string();
            let parsed: BuildState = s.parse().unwrap();
            assert_eq!(parsed, state);
        }
    }
}
