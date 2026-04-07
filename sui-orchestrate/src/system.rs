//! System rebuild orchestration — darwin-rebuild/nixos-rebuild replacement.

use crate::command::{CommandRunner, TokioCommandRunner};

/// Rebuild action type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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

/// Result of a system rebuild.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Darwin,
    NixOS,
}

impl Platform {
    /// Detect the current platform.
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
    pub fn rebuild_command(&self) -> &'static str {
        match self {
            Self::Darwin => "darwin-rebuild",
            Self::NixOS => "nixos-rebuild",
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
    pub fn with_platform(platform: Platform) -> Self {
        Self {
            platform,
            runner: Box::new(TokioCommandRunner::new()),
        }
    }

    /// Create with an explicit platform and command runner.
    pub fn with_runner(platform: Platform, runner: Box<dyn CommandRunner>) -> Self {
        Self { platform, runner }
    }

    /// Returns the detected platform.
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
        let log = format!("{}{}", output.stdout, output.stderr);
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

        // Parse the last line to find current generation
        for line in output.stdout.lines().rev() {
            if line.contains("(current)") {
                if let Some(num_str) = line.split_whitespace().next() {
                    if let Ok(n) = num_str.parse::<i64>() {
                        return Ok(n);
                    }
                }
            }
        }
        Ok(0)
    }

    /// List all system generations.
    pub async fn list_generations(&self) -> Result<Vec<GenerationInfo>, SystemError> {
        let profile = match self.platform {
            Platform::Darwin => "/nix/var/nix/profiles/system",
            Platform::NixOS => "/nix/var/nix/profiles/system",
        };

        let output = self
            .runner
            .run("nix-env", &["--list-generations", "--profile", profile])
            .await?;

        let mut generations = Vec::new();

        for line in output.stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(number) = parts[0].parse::<i64>() {
                    let current = line.contains("(current)");
                    let date = parts.get(1).unwrap_or(&"").to_string();
                    generations.push(GenerationInfo {
                        number,
                        date,
                        current,
                    });
                }
            }
        }

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
        let log = format!("{}{}", output.stdout, output.stderr);

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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GenerationInfo {
    /// The generation number.
    pub number: i64,
    /// The date string from `nix-env --list-generations` output.
    pub date: String,
    /// Whether this is the currently active generation.
    pub current: bool,
}

/// Try to extract a generation number from rebuild output.
fn extract_generation(log: &str) -> Option<i64> {
    for line in log.lines().rev() {
        // Look for patterns like "generation 42" or "switched to generation 42"
        if let Some(idx) = line.find("generation") {
            let after = &line[idx + "generation".len()..];
            for word in after.split_whitespace() {
                if let Ok(n) = word.trim_end_matches('.').parse::<i64>() {
                    return Some(n);
                }
            }
        }
    }
    None
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
}
