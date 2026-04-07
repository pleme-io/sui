//! System rebuild orchestration — darwin-rebuild/nixos-rebuild replacement.

use crate::command::{CommandRunner, TokioCommandRunner};

/// Rebuild action type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum RebuildAction {
    /// Build and activate immediately.
    Switch,
    /// Build and set as boot default (activate on next boot).
    Boot,
    /// Build and activate without making it the boot default.
    Test,
    /// Build only (don't activate).
    Build,
}

impl RebuildAction {
    /// Returns the string representation used in CLI arguments.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Switch => "switch",
            Self::Boot => "boot",
            Self::Test => "test",
            Self::Build => "build",
        }
    }
}

impl std::fmt::Display for RebuildAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RebuildAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "switch" => Ok(Self::Switch),
            "boot" => Ok(Self::Boot),
            "test" => Ok(Self::Test),
            "build" => Ok(Self::Build),
            other => Err(format!("invalid rebuild action: {other}")),
        }
    }
}

/// Result of a system rebuild.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RebuildResult {
    /// Whether the rebuild completed successfully.
    pub success: bool,
    /// The new system generation number, if detected from output.
    pub generation: Option<i64>,
    /// The action that was performed (e.g. "switch", "boot", "rollback").
    pub action: String,
    /// Combined stdout and stderr log from the rebuild command.
    pub log: String,
    /// Wall-clock duration of the rebuild in seconds.
    pub duration_secs: f64,
}

/// Detected platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Platform {
    Darwin,
    NixOS,
}

impl Platform {
    /// Detect the current platform.
    #[must_use]
    pub fn detect() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Darwin)
        } else if std::path::Path::new("/etc/NIXOS").exists() {
            Some(Self::NixOS)
        } else {
            None
        }
    }

    /// Returns the platform-specific rebuild command name.
    #[must_use]
    pub fn rebuild_command(&self) -> &'static str {
        match self {
            Self::Darwin => "darwin-rebuild",
            Self::NixOS => "nixos-rebuild",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Darwin => f.write_str("darwin"),
            Self::NixOS => f.write_str("nixos"),
        }
    }
}

impl std::str::FromStr for Platform {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "darwin" => Ok(Self::Darwin),
            "nixos" => Ok(Self::NixOS),
            other => Err(format!("invalid platform: {other}")),
        }
    }
}

/// System orchestrator.
pub struct SystemOrchestrator {
    platform: Platform,
    runner: Box<dyn CommandRunner>,
}

/// Errors from system operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SystemError {
    #[error("unsupported platform")]
    UnsupportedPlatform,
    #[error("rebuild failed: {0}")]
    RebuildFailed(String),
    #[error("command not found: {0}")]
    CommandNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("command error: {0}")]
    Command(#[from] crate::command::CommandError),
}

impl SystemOrchestrator {
    /// Create a new orchestrator, auto-detecting the platform.
    pub fn new() -> Result<Self, SystemError> {
        let platform = Platform::detect().ok_or(SystemError::UnsupportedPlatform)?;
        Ok(Self {
            platform,
            runner: Box::new(TokioCommandRunner::new()),
        })
    }

    /// Create with an explicit platform.
    #[must_use]
    pub fn with_platform(platform: Platform) -> Self {
        Self {
            platform,
            runner: Box::new(TokioCommandRunner::new()),
        }
    }

    /// Create with an explicit platform and command runner.
    #[must_use]
    pub fn with_runner(platform: Platform, runner: Box<dyn CommandRunner>) -> Self {
        Self { platform, runner }
    }

    /// Returns the detected platform.
    #[must_use]
    pub fn platform(&self) -> Platform {
        self.platform
    }

    /// Execute a system rebuild.
    pub async fn rebuild(
        &self,
        action: RebuildAction,
        flake: Option<&str>,
    ) -> Result<RebuildResult, SystemError> {
        let start = std::time::Instant::now();
        let cmd_name = self.platform.rebuild_command();

        let mut args: Vec<&str> = vec![action.as_str()];
        // We need to own the combined string for the flake args
        let flake_flag;
        if let Some(flake_ref) = flake {
            flake_flag = flake_ref.to_string();
            args.push("--flake");
            args.push(&flake_flag);
        }

        tracing::info!("running: {cmd_name} {}", args.join(" "));

        let output = self.runner.run(cmd_name, &args).await.map_err(|e| match e {
            crate::command::CommandError::NotFound(cmd) => SystemError::CommandNotFound(cmd),
            other => SystemError::Command(other),
        })?;

        let duration = start.elapsed().as_secs_f64();
        let log = output.combined_log();
        let generation = extract_generation(&log);

        if output.success {
            tracing::info!("rebuild succeeded in {duration:.1}s");
            Ok(RebuildResult {
                success: true,
                generation,
                action: action.to_string(),
                log,
                duration_secs: duration,
            })
        } else {
            tracing::error!(
                "rebuild failed: {}",
                output.stderr.lines().last().unwrap_or("unknown error")
            );
            Err(SystemError::RebuildFailed(log))
        }
    }

    /// Get the current system generation number.
    pub async fn current_generation(&self) -> Result<i64, SystemError> {
        let output = self
            .runner
            .run(
                "nix-env",
                &[
                    "--list-generations",
                    "--profile",
                    "/nix/var/nix/profiles/system",
                ],
            )
            .await?;

        let generation = output
            .stdout
            .lines()
            .rev()
            .filter(|line| line.contains("(current)"))
            .find_map(|line| line.split_whitespace().next()?.parse::<i64>().ok())
            .unwrap_or(0);

        Ok(generation)
    }

    /// List all system generations.
    pub async fn list_generations(&self) -> Result<Vec<GenerationInfo>, SystemError> {
        let profile = "/nix/var/nix/profiles/system";

        let output = self
            .runner
            .run("nix-env", &["--list-generations", "--profile", profile])
            .await?;

        let generations = output
            .stdout
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let number = parts.next()?.parse::<i64>().ok()?;
                let date = parts.next().unwrap_or_default().to_string();
                let current = line.contains("(current)");
                Some(GenerationInfo {
                    number,
                    date,
                    current,
                })
            })
            .collect();

        Ok(generations)
    }

    /// Rollback to the previous generation.
    pub async fn rollback(&self) -> Result<RebuildResult, SystemError> {
        let start = std::time::Instant::now();
        let cmd_name = self.platform.rebuild_command();

        let output = self
            .runner
            .run(cmd_name, &["switch", "--rollback"])
            .await?;

        let duration = start.elapsed().as_secs_f64();
        let log = output.combined_log();

        Ok(RebuildResult {
            success: output.success,
            generation: extract_generation(&log),
            action: "rollback".to_string(),
            log,
            duration_secs: duration,
        })
    }
}

/// Information about a single system generation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GenerationInfo {
    /// The generation number.
    pub number: i64,
    /// The date string from `nix-env --list-generations` output.
    pub date: String,
    /// Whether this is the currently active generation.
    pub current: bool,
}

/// Try to extract a generation number from rebuild output.
///
/// Scans lines in reverse looking for "generation <N>" patterns such as
/// "switched to generation 42" or "activating generation 15.".
pub(crate) fn extract_generation(log: &str) -> Option<i64> {
    log.lines()
        .rev()
        .filter_map(|line| {
            let suffix = line.split_once("generation")?.1;
            suffix
                .split_whitespace()
                .find_map(|word| word.trim_end_matches('.').parse::<i64>().ok())
        })
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CommandOutput, CommandError};

    /// A mock command runner for testing.
    struct MockCommandRunner {
        responses: std::collections::BTreeMap<String, CommandOutput>,
    }

    impl MockCommandRunner {
        fn new() -> Self {
            Self {
                responses: std::collections::BTreeMap::new(),
            }
        }

        fn with_response(mut self, program: &str, output: CommandOutput) -> Self {
            self.responses.insert(program.to_string(), output);
            self
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for MockCommandRunner {
        async fn run(&self, program: &str, _args: &[&str]) -> Result<CommandOutput, CommandError> {
            self.responses
                .get(program)
                .cloned()
                .ok_or_else(|| CommandError::NotFound(program.to_string()))
        }
    }

    #[test]
    fn rebuild_action_display() {
        assert_eq!(RebuildAction::Switch.as_str(), "switch");
        assert_eq!(RebuildAction::Boot.as_str(), "boot");
        assert_eq!(RebuildAction::Test.as_str(), "test");
        assert_eq!(RebuildAction::Build.as_str(), "build");
    }

    #[test]
    fn platform_detection() {
        let platform = Platform::detect();
        if cfg!(target_os = "macos") {
            assert_eq!(platform, Some(Platform::Darwin));
        }
    }

    #[test]
    fn extract_generation_from_log() {
        assert_eq!(
            extract_generation("switched to generation 42"),
            Some(42)
        );
        assert_eq!(
            extract_generation("activating generation 15."),
            Some(15)
        );
        assert_eq!(extract_generation("no generation info"), None);
    }

    #[test]
    fn rebuild_result_serialization() {
        let result = RebuildResult {
            success: true,
            generation: Some(42),
            action: "switch".to_string(),
            log: "ok".to_string(),
            duration_secs: 1.5,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"generation\":42"));
    }

    #[tokio::test]
    async fn mock_rebuild_success() {
        let runner = MockCommandRunner::new().with_response(
            "darwin-rebuild",
            CommandOutput {
                success: true,
                stdout: "switched to generation 99\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys.rebuild(RebuildAction::Switch, None).await.unwrap();
        assert!(result.success);
        assert_eq!(result.generation, Some(99));
    }

    #[tokio::test]
    async fn mock_rebuild_failure() {
        let runner = MockCommandRunner::new().with_response(
            "darwin-rebuild",
            CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: "build error\n".to_string(),
                exit_code: Some(1),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys.rebuild(RebuildAction::Switch, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mock_rollback() {
        let runner = MockCommandRunner::new().with_response(
            "darwin-rebuild",
            CommandOutput {
                success: true,
                stdout: "rolled back to generation 41\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys.rollback().await.unwrap();
        assert!(result.success);
        assert_eq!(result.generation, Some(41));
    }

    // ── rebuild() with flake ─────────────────────────────────

    #[tokio::test]
    async fn mock_rebuild_with_flake_ref() {
        let runner = MockCommandRunner::new().with_response(
            "darwin-rebuild",
            CommandOutput {
                success: true,
                stdout: "switched to generation 55\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys
            .rebuild(RebuildAction::Switch, Some(".#cid"))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.generation, Some(55));
        assert_eq!(result.action, "switch");
    }

    // ── rebuild() different actions ──────────────────────────

    #[tokio::test]
    async fn mock_rebuild_boot_action() {
        let runner = MockCommandRunner::new().with_response(
            "darwin-rebuild",
            CommandOutput {
                success: true,
                stdout: "generation 10\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys.rebuild(RebuildAction::Boot, None).await.unwrap();
        assert!(result.success);
        assert_eq!(result.action, "boot");
        assert_eq!(result.generation, Some(10));
    }

    // ── rebuild() NixOS platform ────────────────────────────

    #[tokio::test]
    async fn mock_rebuild_nixos_success() {
        let runner = MockCommandRunner::new().with_response(
            "nixos-rebuild",
            CommandOutput {
                success: true,
                stdout: "activating generation 77\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::NixOS, Box::new(runner));
        let result = sys.rebuild(RebuildAction::Switch, None).await.unwrap();
        assert!(result.success);
        assert_eq!(result.generation, Some(77));
    }

    // ── rebuild() command not found ─────────────────────────

    #[tokio::test]
    async fn mock_rebuild_command_not_found() {
        let runner = MockCommandRunner::new(); // no responses
        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys.rebuild(RebuildAction::Switch, None).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SystemError::CommandNotFound(cmd) => assert_eq!(cmd, "darwin-rebuild"),
            other => panic!("expected CommandNotFound, got {other:?}"),
        }
    }

    // ── current_generation() parsing ────────────────────────

    #[tokio::test]
    async fn mock_current_generation_parses_output() {
        let runner = MockCommandRunner::new().with_response(
            "nix-env",
            CommandOutput {
                success: true,
                stdout: "  41   2024-01-01\n  42   2024-01-02 (current)\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let current = sys.current_generation().await.unwrap();
        assert_eq!(current, 42);
    }

    #[tokio::test]
    async fn mock_current_generation_no_current_marker() {
        let runner = MockCommandRunner::new().with_response(
            "nix-env",
            CommandOutput {
                success: true,
                stdout: "  41   2024-01-01\n  42   2024-01-02\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let current = sys.current_generation().await.unwrap();
        assert_eq!(current, 0); // no (current) marker found
    }

    // ── list_generations() parsing ──────────────────────────

    #[tokio::test]
    async fn mock_list_generations_multi_line() {
        let runner = MockCommandRunner::new().with_response(
            "nix-env",
            CommandOutput {
                success: true,
                stdout: "  1  2024-01-01\n  2  2024-01-15 (current)\n  3  2024-02-01\n"
                    .to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let gens = sys.list_generations().await.unwrap();
        assert_eq!(gens.len(), 3);
        assert_eq!(gens[0].number, 1);
        assert!(!gens[0].current);
        assert_eq!(gens[1].number, 2);
        assert!(gens[1].current);
        assert_eq!(gens[2].number, 3);
        assert!(!gens[2].current);
    }

    #[tokio::test]
    async fn mock_list_generations_empty_output() {
        let runner = MockCommandRunner::new().with_response(
            "nix-env",
            CommandOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let gens = sys.list_generations().await.unwrap();
        assert!(gens.is_empty());
    }

    // ── rollback() success with NixOS ───────────────────────

    #[tokio::test]
    async fn mock_rollback_nixos() {
        let runner = MockCommandRunner::new().with_response(
            "nixos-rebuild",
            CommandOutput {
                success: true,
                stdout: "rolled back to generation 20\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::NixOS, Box::new(runner));
        let result = sys.rollback().await.unwrap();
        assert!(result.success);
        assert_eq!(result.generation, Some(20));
        assert_eq!(result.action, "rollback");
    }

    // ── rollback() failure ──────────────────────────────────

    #[tokio::test]
    async fn mock_rollback_failure() {
        let runner = MockCommandRunner::new().with_response(
            "darwin-rebuild",
            CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: "rollback failed\n".to_string(),
                exit_code: Some(1),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys.rollback().await.unwrap();
        assert!(!result.success);
    }

    // ── Platform helper tests ───────────────────────────────

    #[test]
    fn platform_rebuild_command_darwin() {
        assert_eq!(Platform::Darwin.rebuild_command(), "darwin-rebuild");
    }

    #[test]
    fn platform_rebuild_command_nixos() {
        assert_eq!(Platform::NixOS.rebuild_command(), "nixos-rebuild");
    }

    // ── extract_generation edge cases ─────────────────────────

    #[test]
    fn extract_generation_multiple_lines() {
        let log = "building system\nactivating generation 10\nswitched to generation 42\n";
        assert_eq!(extract_generation(log), Some(42));
    }

    #[test]
    fn extract_generation_trailing_period() {
        assert_eq!(extract_generation("generation 7."), Some(7));
    }

    #[test]
    fn extract_generation_empty_string() {
        assert_eq!(extract_generation(""), None);
    }

    #[test]
    fn extract_generation_number_only_no_keyword() {
        assert_eq!(extract_generation("42"), None);
    }

    #[test]
    fn extract_generation_large_number() {
        assert_eq!(
            extract_generation("switched to generation 999999"),
            Some(999999)
        );
    }

    #[test]
    fn extract_generation_negative_parsed() {
        // Negative numbers are technically parseable by i64; the function
        // doesn't filter them — callers should validate if needed.
        assert_eq!(extract_generation("generation -1"), Some(-1));
    }

    #[test]
    fn extract_generation_zero() {
        assert_eq!(extract_generation("generation 0"), Some(0));
    }

    // ── RebuildAction serde roundtrip ─────────────────────────

    #[test]
    fn rebuild_action_serde_roundtrip() {
        for action in [
            RebuildAction::Switch,
            RebuildAction::Boot,
            RebuildAction::Test,
            RebuildAction::Build,
        ] {
            let json = serde_json::to_string(&action).unwrap();
            let parsed: RebuildAction = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, action);
        }
    }

    #[test]
    fn rebuild_action_display_matches_as_str() {
        for action in [
            RebuildAction::Switch,
            RebuildAction::Boot,
            RebuildAction::Test,
            RebuildAction::Build,
        ] {
            assert_eq!(action.to_string(), action.as_str());
        }
    }

    // ── SystemError display ───────────────────────────────────

    #[test]
    fn system_error_unsupported_platform_display() {
        let e = SystemError::UnsupportedPlatform;
        assert_eq!(e.to_string(), "unsupported platform");
    }

    #[test]
    fn system_error_rebuild_failed_display() {
        let e = SystemError::RebuildFailed("build error".to_string());
        assert!(e.to_string().contains("build error"));
    }

    #[test]
    fn system_error_command_not_found_display() {
        let e = SystemError::CommandNotFound("nix".to_string());
        assert!(e.to_string().contains("nix"));
    }

    // ── GenerationInfo serde roundtrip ────────────────────────

    #[test]
    fn generation_info_serde_roundtrip() {
        let info = GenerationInfo {
            number: 42,
            date: "2024-06-01".to_string(),
            current: true,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: GenerationInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.number, 42);
        assert_eq!(parsed.date, "2024-06-01");
        assert!(parsed.current);
    }

    // ── RebuildResult deserialization ──────────────────────────

    #[test]
    fn rebuild_result_deserialization() {
        let json = r#"{"success":true,"generation":10,"action":"switch","log":"ok","duration_secs":2.0}"#;
        let result: RebuildResult = serde_json::from_str(json).unwrap();
        assert!(result.success);
        assert_eq!(result.generation, Some(10));
        assert_eq!(result.action, "switch");
    }

    #[test]
    fn rebuild_result_null_generation() {
        let json = r#"{"success":false,"generation":null,"action":"build","log":"err","duration_secs":0.5}"#;
        let result: RebuildResult = serde_json::from_str(json).unwrap();
        assert!(!result.success);
        assert_eq!(result.generation, None);
    }

    // ── rebuild() Test and Build actions ──────────────────────

    #[tokio::test]
    async fn mock_rebuild_test_action() {
        let runner = MockCommandRunner::new().with_response(
            "nixos-rebuild",
            CommandOutput {
                success: true,
                stdout: "generation 5\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::NixOS, Box::new(runner));
        let result = sys.rebuild(RebuildAction::Test, None).await.unwrap();
        assert!(result.success);
        assert_eq!(result.action, "test");
        assert_eq!(result.generation, Some(5));
    }

    #[tokio::test]
    async fn mock_rebuild_build_action() {
        let runner = MockCommandRunner::new().with_response(
            "nixos-rebuild",
            CommandOutput {
                success: true,
                stdout: "built successfully\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::NixOS, Box::new(runner));
        let result = sys.rebuild(RebuildAction::Build, None).await.unwrap();
        assert!(result.success);
        assert_eq!(result.action, "build");
        assert_eq!(result.generation, None);
    }

    // ── rebuild() stderr captured in log ──────────────────────

    #[tokio::test]
    async fn mock_rebuild_captures_stderr_in_log() {
        let runner = MockCommandRunner::new().with_response(
            "darwin-rebuild",
            CommandOutput {
                success: true,
                stdout: "generation 1\n".to_string(),
                stderr: "warning: something\n".to_string(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys.rebuild(RebuildAction::Switch, None).await.unwrap();
        assert!(result.log.contains("warning: something"));
        assert!(result.log.contains("generation 1"));
    }

    // ── current_generation() command not found ────────────────

    #[tokio::test]
    async fn mock_current_generation_command_not_found() {
        let runner = MockCommandRunner::new();
        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys.current_generation().await;
        assert!(result.is_err());
    }

    // ── list_generations() with unparseable lines ─────────────

    #[tokio::test]
    async fn mock_list_generations_skips_unparseable_lines() {
        let runner = MockCommandRunner::new().with_response(
            "nix-env",
            CommandOutput {
                success: true,
                stdout: "  garbage line\n  5 2024-03-01\n  not-a-number date\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let gens = sys.list_generations().await.unwrap();
        assert_eq!(gens.len(), 1);
        assert_eq!(gens[0].number, 5);
    }

    // ── with_platform constructor ─────────────────────────────

    #[test]
    fn with_platform_constructor() {
        let sys = SystemOrchestrator::with_platform(Platform::NixOS);
        assert_eq!(sys.platform(), Platform::NixOS);
    }

    #[test]
    fn with_platform_darwin_constructor() {
        let sys = SystemOrchestrator::with_platform(Platform::Darwin);
        assert_eq!(sys.platform(), Platform::Darwin);
    }

    // ── proptest: extract_generation never panics ─────────────

    proptest::proptest! {
        #[test]
        fn extract_generation_never_panics(s in ".*") {
            let _ = extract_generation(&s);
        }

        #[test]
        fn extract_generation_finds_injected_number(n in 0i64..100_000) {
            let log = format!("switched to generation {n}");
            assert_eq!(extract_generation(&log), Some(n));
        }
    }

    // ── Platform FromStr ──────────────────────────────────────

    #[test]
    fn platform_from_str_valid() {
        use std::str::FromStr;
        assert_eq!(Platform::from_str("darwin").unwrap(), Platform::Darwin);
        assert_eq!(Platform::from_str("nixos").unwrap(), Platform::NixOS);
    }

    #[test]
    fn platform_from_str_rejects_garbage() {
        use std::str::FromStr;
        let err = Platform::from_str("windows").unwrap_err();
        assert!(err.contains("invalid platform"));
        assert!(err.contains("windows"));
    }

    #[test]
    fn platform_from_str_case_sensitive() {
        use std::str::FromStr;
        assert!(Platform::from_str("Darwin").is_err());
        assert!(Platform::from_str("NIXOS").is_err());
        assert!(Platform::from_str("").is_err());
    }

    #[test]
    fn platform_display_strings() {
        assert_eq!(Platform::Darwin.to_string(), "darwin");
        assert_eq!(Platform::NixOS.to_string(), "nixos");
    }

    #[test]
    fn platform_serde_roundtrip() {
        for p in [Platform::Darwin, Platform::NixOS] {
            let json = serde_json::to_string(&p).unwrap();
            let parsed: Platform = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, p);
        }
    }

    // ── RebuildAction FromStr ─────────────────────────────────

    #[test]
    fn rebuild_action_from_str_all_valid() {
        use std::str::FromStr;
        assert_eq!(RebuildAction::from_str("switch").unwrap(), RebuildAction::Switch);
        assert_eq!(RebuildAction::from_str("boot").unwrap(), RebuildAction::Boot);
        assert_eq!(RebuildAction::from_str("test").unwrap(), RebuildAction::Test);
        assert_eq!(RebuildAction::from_str("build").unwrap(), RebuildAction::Build);
    }

    #[test]
    fn rebuild_action_from_str_rejects_garbage() {
        use std::str::FromStr;
        let err = RebuildAction::from_str("rollback").unwrap_err();
        assert!(err.contains("invalid rebuild action"));
        assert!(err.contains("rollback"));
    }

    #[test]
    fn rebuild_action_from_str_case_sensitive() {
        use std::str::FromStr;
        assert!(RebuildAction::from_str("Switch").is_err());
        assert!(RebuildAction::from_str("BOOT").is_err());
        assert!(RebuildAction::from_str("").is_err());
    }

    // ── Arg-capturing mock runner: verify rebuild builds the right CLI ─

    use std::sync::Mutex;

    struct CapturingRunner {
        captured: Mutex<Vec<(String, Vec<String>)>>,
        response: CommandOutput,
    }

    impl CapturingRunner {
        fn new(response: CommandOutput) -> Self {
            Self {
                captured: Mutex::new(Vec::new()),
                response,
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.captured.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for CapturingRunner {
        async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, CommandError> {
            self.captured.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| (*s).to_string()).collect(),
            ));
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn rebuild_invokes_correct_program_and_args_no_flake() {
        let runner = CapturingRunner::new(CommandOutput {
            success: true,
            stdout: "generation 1\n".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
        });

        // We need a Box<dyn CommandRunner> but we also need to inspect the mock
        // afterward. Wrap in Arc and clone for the inspection.
        let captor = Arc::new(runner);
        struct Forwarder(Arc<CapturingRunner>);
        #[async_trait::async_trait]
        impl CommandRunner for Forwarder {
            async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, CommandError> {
                self.0.run(program, args).await
            }
        }
        let sys = SystemOrchestrator::with_runner(
            Platform::Darwin,
            Box::new(Forwarder(Arc::clone(&captor))),
        );
        sys.rebuild(RebuildAction::Switch, None).await.unwrap();
        let calls = captor.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "darwin-rebuild");
        assert_eq!(calls[0].1, vec!["switch"]);
    }

    #[tokio::test]
    async fn rebuild_invokes_correct_program_and_args_with_flake() {
        let captor = Arc::new(CapturingRunner::new(CommandOutput {
            success: true,
            stdout: "generation 1\n".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
        }));
        struct Forwarder(Arc<CapturingRunner>);
        #[async_trait::async_trait]
        impl CommandRunner for Forwarder {
            async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, CommandError> {
                self.0.run(program, args).await
            }
        }
        let sys = SystemOrchestrator::with_runner(
            Platform::NixOS,
            Box::new(Forwarder(Arc::clone(&captor))),
        );
        sys.rebuild(RebuildAction::Boot, Some(".#node"))
            .await
            .unwrap();
        let calls = captor.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "nixos-rebuild");
        assert_eq!(calls[0].1, vec!["boot", "--flake", ".#node"]);
    }

    use std::sync::Arc;

    // ── rebuild_failed includes log ───────────────────────────

    #[tokio::test]
    async fn rebuild_failure_log_propagated_in_error() {
        let runner = MockCommandRunner::new().with_response(
            "darwin-rebuild",
            CommandOutput {
                success: false,
                stdout: "starting build\n".to_string(),
                stderr: "ERROR: dirty git tree\n".to_string(),
                exit_code: Some(1),
            },
        );
        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let err = sys.rebuild(RebuildAction::Switch, None).await.unwrap_err();
        match err {
            SystemError::RebuildFailed(log) => {
                assert!(log.contains("starting build"));
                assert!(log.contains("ERROR: dirty git tree"));
            }
            other => panic!("expected RebuildFailed, got {other:?}"),
        }
    }

    // ── SystemError::Io display ───────────────────────────────

    #[test]
    fn system_error_io_display() {
        let inner = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no");
        let e: SystemError = inner.into();
        let s = e.to_string();
        assert!(s.contains("io error"));
    }

    // ── SystemError::Command display from CommandError ────────

    #[test]
    fn system_error_from_command_error() {
        let cmd_err = crate::command::CommandError::Failed("oops".to_string());
        let e: SystemError = cmd_err.into();
        let s = e.to_string();
        assert!(s.contains("command error"));
        assert!(s.contains("oops"));
    }

    // ── current_generation: empty stdout ──────────────────────

    #[tokio::test]
    async fn mock_current_generation_empty_stdout() {
        let runner = MockCommandRunner::new().with_response(
            "nix-env",
            CommandOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );
        let sys = SystemOrchestrator::with_runner(Platform::NixOS, Box::new(runner));
        let current = sys.current_generation().await.unwrap();
        assert_eq!(current, 0);
    }

    // ── list_generations: lines with no date ──────────────────

    #[tokio::test]
    async fn mock_list_generations_line_with_only_number() {
        let runner = MockCommandRunner::new().with_response(
            "nix-env",
            CommandOutput {
                success: true,
                stdout: "  9\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );
        let sys = SystemOrchestrator::with_runner(Platform::NixOS, Box::new(runner));
        let gens = sys.list_generations().await.unwrap();
        assert_eq!(gens.len(), 1);
        assert_eq!(gens[0].number, 9);
        assert_eq!(gens[0].date, "");
        assert!(!gens[0].current);
    }

    // ── new() unsupported platform error message ──────────────

    #[test]
    fn system_error_unsupported_platform_message() {
        let e = SystemError::UnsupportedPlatform;
        let s = e.to_string();
        assert_eq!(s, "unsupported platform");
        let dbg = format!("{e:?}");
        assert!(dbg.contains("UnsupportedPlatform"));
    }

    // ── platform() accessor for orchestrator ──────────────────

    #[test]
    fn orchestrator_platform_accessor_returns_set_value() {
        let s1 = SystemOrchestrator::with_platform(Platform::Darwin);
        assert_eq!(s1.platform(), Platform::Darwin);
        let s2 = SystemOrchestrator::with_platform(Platform::NixOS);
        assert_eq!(s2.platform(), Platform::NixOS);
    }

    // ── extract_generation: only "generation" word, no number ─

    #[test]
    fn extract_generation_keyword_no_number() {
        assert_eq!(extract_generation("generation"), None);
        assert_eq!(extract_generation("the generation system"), None);
    }

    // ── extract_generation: number before generation keyword ──

    #[test]
    fn extract_generation_number_before_keyword_ignored() {
        // The function only looks at words after "generation"
        assert_eq!(extract_generation("42 generation"), None);
    }

    // ── GenerationInfo equality ───────────────────────────────

    #[test]
    fn generation_info_equality() {
        let a = GenerationInfo {
            number: 1,
            date: "2024-01-01".to_string(),
            current: true,
        };
        let b = GenerationInfo {
            number: 1,
            date: "2024-01-01".to_string(),
            current: true,
        };
        assert_eq!(a, b);
        let c = GenerationInfo {
            number: 2,
            date: "2024-01-01".to_string(),
            current: true,
        };
        assert_ne!(a, c);
    }
}
