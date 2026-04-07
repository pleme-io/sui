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

    /// Native rebuild: evaluate flake, build closure, activate system.
    ///
    /// Uses `nix eval` for evaluation and `nix build` for the actual build
    /// (the full native build pipeline will be wired once the substitution
    /// layer is production-ready). The key win is that `sui system rebuild`
    /// becomes the user's single entry point.
    pub async fn rebuild_native(
        &self,
        flake_ref_str: &str,
        action: RebuildAction,
    ) -> Result<RebuildResult, SystemError> {
        let start = std::time::Instant::now();
        let flake_ref = sui_compat::flake_ref::FlakeRef::parse(flake_ref_str)
            .map_err(|e| SystemError::RebuildFailed(e.to_string()))?;

        // 1. Determine the attribute path based on platform
        let platform_prefix = match self.platform {
            Platform::Darwin => "darwinConfigurations",
            Platform::NixOS => "nixosConfigurations",
        };
        let attr_path = format!(
            "{platform_prefix}.{}.config.system.build.toplevel",
            flake_ref.attribute
        );

        // 2. Evaluate the flake natively to get the derivation path.
        let flake_result = sui_eval::builtins::evaluate_flake(&flake_ref.flake_dir)
            .map_err(|e| SystemError::RebuildFailed(format!("eval: {e}")))?;

        // Navigate the outputs attrset to the system derivation.
        let attr_segments: Vec<&str> = attr_path.split('.').collect();
        let drv_value = sui_eval::builtins::navigate_attrs(&flake_result, &attr_segments)
            .map_err(|e| SystemError::RebuildFailed(format!("navigate attrs: {e}")))?;

        // Extract drvPath from the derivation attrset.
        let _drv_path = match drv_value {
            sui_eval::Value::Attrs(ref attrs) => {
                attrs.get("drvPath")
                    .and_then(|v| v.as_string().ok())
                    .map(|s| s.to_string())
                    .ok_or_else(|| SystemError::RebuildFailed(
                        "derivation attrset missing drvPath".into(),
                    ))?
            }
            _ => {
                return Ok(RebuildResult {
                    success: false,
                    generation: None,
                    action: action.to_string(),
                    log: format!("eval failed: expected derivation attrset, got {}", drv_value.type_name()),
                    duration_secs: start.elapsed().as_secs_f64(),
                });
            }
        };

        // 3. Build the system derivation (still delegates to nix build for the
        //    full closure — the native single-derivation builder doesn't handle
        //    recursive dependency resolution yet).
        let build_ref = format!(
            "{}#{attr_path}",
            flake_ref.flake_dir.display()
        );
        let build_output = self
            .runner
            .run(
                "nix",
                &["build", "--no-link", "--print-out-paths", &build_ref],
            )
            .await
            .map_err(|e| SystemError::CommandNotFound(format!("nix: {e}")))?;

        if !build_output.success {
            return Ok(RebuildResult {
                success: false,
                generation: None,
                action: action.to_string(),
                log: build_output.stderr.clone(),
                duration_secs: start.elapsed().as_secs_f64(),
            });
        }

        let system_path = build_output.stdout.trim().to_string();

        // 4. Activate the system profile
        self.activate_system(&system_path, action).await?;

        // 5. Get the new generation
        let current_gen = self.current_generation().await.ok();

        Ok(RebuildResult {
            success: true,
            generation: current_gen,
            action: action.to_string(),
            log: format!("native rebuild completed: {system_path}"),
            duration_secs: start.elapsed().as_secs_f64(),
        })
    }

    /// Activate a built system profile.
    ///
    /// Sets the system profile and runs activation scripts as appropriate
    /// for the given [`RebuildAction`].
    async fn activate_system(
        &self,
        system_path: &str,
        action: RebuildAction,
    ) -> Result<(), SystemError> {
        match action {
            RebuildAction::Switch | RebuildAction::Test => {
                // Set the system profile
                self.runner
                    .run(
                        "nix-env",
                        &[
                            "--profile",
                            "/nix/var/nix/profiles/system",
                            "--set",
                            system_path,
                        ],
                    )
                    .await
                    .map_err(|e| SystemError::RebuildFailed(e.to_string()))?;

                // Run the activate script
                let activate = format!("{system_path}/activate");
                self.runner
                    .run(&activate, &[])
                    .await
                    .map_err(|e| SystemError::RebuildFailed(format!("activate: {e}")))?;

                if action == RebuildAction::Switch && self.platform == Platform::Darwin {
                    let activate_user = format!("{system_path}/activate-user");
                    if std::path::Path::new(&activate_user).exists() {
                        self.runner
                            .run(&activate_user, &[])
                            .await
                            .map_err(|e| {
                                SystemError::RebuildFailed(format!("activate-user: {e}"))
                            })?;
                    }
                }
            }
            RebuildAction::Boot => {
                // Set profile but don't activate — takes effect on next boot
                self.runner
                    .run(
                        "nix-env",
                        &[
                            "--profile",
                            "/nix/var/nix/profiles/system",
                            "--set",
                            system_path,
                        ],
                    )
                    .await
                    .map_err(|e| SystemError::RebuildFailed(e.to_string()))?;
            }
            RebuildAction::Build => {
                // Build only — nothing to activate
            }
        }
        Ok(())
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

    /// Rollback to a specific numbered generation.
    ///
    /// Invokes `darwin-rebuild switch --switch-generation <n>` on Darwin or
    /// `nixos-rebuild switch --switch-generation <n>` on NixOS. Both rebuild
    /// commands accept the standard Nix `--switch-generation` flag.
    pub async fn rollback_to(&self, generation: u32) -> Result<RebuildResult, SystemError> {
        let start = std::time::Instant::now();
        let cmd_name = self.platform.rebuild_command();
        let gen_str = generation.to_string();

        tracing::info!("rolling back to generation {generation}");

        let output = self
            .runner
            .run(cmd_name, &["switch", "--switch-generation", &gen_str])
            .await
            .map_err(|e| match e {
                crate::command::CommandError::NotFound(cmd) => SystemError::CommandNotFound(cmd),
                other => SystemError::Command(other),
            })?;

        let duration = start.elapsed().as_secs_f64();
        let log = output.combined_log();

        // Prefer the explicitly requested generation if the command succeeded,
        // otherwise try to parse it from the log (mirrors `rollback()` behavior).
        let parsed_gen = extract_generation(&log);
        let generation_field = if output.success {
            Some(i64::from(generation))
        } else {
            parsed_gen
        };

        Ok(RebuildResult {
            success: output.success,
            generation: generation_field,
            action: format!("rollback-to-{generation}"),
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

    // ── current_generation invokes the right argv ─────────────

    #[tokio::test]
    async fn current_generation_invokes_nix_env_with_profile_arg() {
        let captor = Arc::new(CapturingRunner::new(CommandOutput {
            success: true,
            stdout: "  1   2024-01-01 (current)\n".to_string(),
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
        let n = sys.current_generation().await.unwrap();
        assert_eq!(n, 1);
        let calls = captor.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "nix-env");
        assert_eq!(
            calls[0].1,
            vec![
                "--list-generations",
                "--profile",
                "/nix/var/nix/profiles/system",
            ]
        );
    }

    // ── list_generations invokes the right argv ───────────────

    #[tokio::test]
    async fn list_generations_invokes_nix_env_with_profile_arg() {
        let captor = Arc::new(CapturingRunner::new(CommandOutput {
            success: true,
            stdout: "  1   2024-01-01\n  2   2024-01-15 (current)\n".to_string(),
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
            Platform::Darwin,
            Box::new(Forwarder(Arc::clone(&captor))),
        );
        let gens = sys.list_generations().await.unwrap();
        assert_eq!(gens.len(), 2);
        let calls = captor.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "nix-env");
        assert_eq!(
            calls[0].1,
            vec![
                "--list-generations",
                "--profile",
                "/nix/var/nix/profiles/system",
            ]
        );
    }

    // ── rollback invokes correct argv ─────────────────────────

    #[tokio::test]
    async fn rollback_invokes_switch_rollback() {
        let captor = Arc::new(CapturingRunner::new(CommandOutput {
            success: true,
            stdout: "rolled back\n".to_string(),
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
        sys.rollback().await.unwrap();
        let calls = captor.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "nixos-rebuild");
        assert_eq!(calls[0].1, vec!["switch", "--rollback"]);
    }

    // ── current_generation: numerically sorted lines, latest current
    //    line wins for the rev() scan ────────────────────────────

    #[tokio::test]
    async fn current_generation_picks_last_current_line() {
        let runner = MockCommandRunner::new().with_response(
            "nix-env",
            CommandOutput {
                success: true,
                stdout: "  41  2024-01-01 (current)\n  42  2024-01-02 (current)\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );
        let sys = SystemOrchestrator::with_runner(Platform::NixOS, Box::new(runner));
        let n = sys.current_generation().await.unwrap();
        // Function scans in rev() order so the bottom (current) wins
        assert_eq!(n, 42);
    }

    // ── rebuild_action serde lowercase ────────────────────────

    #[test]
    fn rebuild_action_serde_lowercase_strings() {
        for (action, expected) in [
            (RebuildAction::Switch, "\"switch\""),
            (RebuildAction::Boot, "\"boot\""),
            (RebuildAction::Test, "\"test\""),
            (RebuildAction::Build, "\"build\""),
        ] {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(json, expected);
        }
    }

    // ── Platform detection: macOS host invariant ──────────────

    #[cfg(target_os = "macos")]
    #[test]
    fn platform_detect_returns_darwin_on_macos() {
        assert_eq!(Platform::detect(), Some(Platform::Darwin));
    }

    // ── rollback_to(generation) ───────────────────────────────

    /// Mock runner that records the args passed to the command on the last invocation.
    /// Wraps state in `Arc` so callers can keep a handle to inspect after the orchestrator
    /// has taken ownership of the boxed runner.
    struct RecordingRunner {
        response: CommandOutput,
        last_args: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl RecordingRunner {
        fn new(response: CommandOutput) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
            let last_args = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Self {
                    response,
                    last_args: std::sync::Arc::clone(&last_args),
                },
                last_args,
            )
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for RecordingRunner {
        async fn run(&self, _program: &str, args: &[&str]) -> Result<CommandOutput, CommandError> {
            *self.last_args.lock().unwrap() = args.iter().map(|s| s.to_string()).collect();
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn rollback_to_happy_path_darwin() {
        let (runner, args_handle) = RecordingRunner::new(CommandOutput {
            success: true,
            stdout: "switched to generation 5\n".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
        });

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));

        let result = sys.rollback_to(5).await.unwrap();
        assert!(result.success);
        // On success, the requested generation is preserved verbatim.
        assert_eq!(result.generation, Some(5));
        assert_eq!(result.action, "rollback-to-5");
        // Verify the right CLI flags were used.
        let args = args_handle.lock().unwrap().clone();
        assert_eq!(args, vec!["switch", "--switch-generation", "5"]);
    }

    #[tokio::test]
    async fn rollback_to_failure_returns_unsuccessful_result() {
        // Error path: rebuild command runs but reports failure.
        let runner = MockCommandRunner::new().with_response(
            "nixos-rebuild",
            CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: "no such generation: 999\n".to_string(),
                exit_code: Some(1),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::NixOS, Box::new(runner));
        let result = sys.rollback_to(999).await.unwrap();
        assert!(!result.success);
        assert_eq!(result.action, "rollback-to-999");
        assert!(result.log.contains("no such generation"));
    }

    #[tokio::test]
    async fn rollback_to_command_not_found_propagates() {
        // Edge case: rebuild binary missing entirely.
        let runner = MockCommandRunner::new();
        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys.rollback_to(1).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SystemError::CommandNotFound(cmd) => assert_eq!(cmd, "darwin-rebuild"),
            other => panic!("expected CommandNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rollback_to_zero_generation() {
        // Edge case: generation 0 should be allowed (some systems start at 0/1).
        let runner = MockCommandRunner::new().with_response(
            "darwin-rebuild",
            CommandOutput {
                success: true,
                stdout: "switched to generation 0\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys.rollback_to(0).await.unwrap();
        assert!(result.success);
        assert_eq!(result.generation, Some(0));
        assert_eq!(result.action, "rollback-to-0");
    }

    // ── rebuild_native() tests ───────────────────────────────

    /// A multi-command mock that responds differently based on both the
    /// program name and the first argument (to distinguish `nix eval` from
    /// `nix build`).
    struct MultiMockRunner {
        responses: std::collections::BTreeMap<String, CommandOutput>,
    }

    impl MultiMockRunner {
        fn new() -> Self {
            Self {
                responses: std::collections::BTreeMap::new(),
            }
        }

        /// Register a response keyed by `"program:first_arg"` (e.g. `"nix:eval"`).
        /// Falls back to `"program"` if no first-arg-specific key matches.
        fn with_response(mut self, key: &str, output: CommandOutput) -> Self {
            self.responses.insert(key.to_string(), output);
            self
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for MultiMockRunner {
        async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, CommandError> {
            // Try program:first_arg first, then program alone
            let compound_key = if let Some(first) = args.first() {
                format!("{program}:{first}")
            } else {
                program.to_string()
            };
            self.responses
                .get(&compound_key)
                .or_else(|| self.responses.get(program))
                .cloned()
                .ok_or_else(|| CommandError::NotFound(program.to_string()))
        }
    }

    // ── Helper: create a minimal temp flake for testing rebuild_native ──

    /// Create a temp flake directory with a derivation nested at the
    /// expected system attribute path.  Returns (flake_dir, attr_name).
    fn make_test_flake(platform: Platform, attr: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();

        let platform_key = match platform {
            Platform::Darwin => "darwinConfigurations",
            Platform::NixOS  => "nixosConfigurations",
        };

        // A minimal flake.nix whose outputs contain a derivation at
        // <platformKey>.<attr>.config.system.build.toplevel.
        let flake_nix = format!(
            r#"{{
  outputs = {{ self }}:
    let
      drv = builtins.derivation {{
        name = "test-system";
        system = "x86_64-linux";
        builder = "/bin/sh";
      }};
    in {{
      {platform_key}.{attr}.config.system.build.toplevel = drv;
    }};
}}"#,
        );

        std::fs::write(dir.path().join("flake.nix"), flake_nix).unwrap();
        dir
    }

    #[tokio::test]
    async fn rebuild_native_success_build_only() {
        let flake_dir = make_test_flake(Platform::Darwin, "cid");
        let flake_ref = format!("{}#cid", flake_dir.path().display());

        let runner = MultiMockRunner::new().with_response(
            "nix:build",
            CommandOutput {
                success: true,
                stdout: "/nix/store/xyz-system\n".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys
            .rebuild_native(&flake_ref, RebuildAction::Build)
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.action, "build");
        assert!(result.log.contains("/nix/store/xyz-system"));
    }

    #[tokio::test]
    async fn rebuild_native_eval_failure() {
        // Create a flake that evaluates but has no matching attribute.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("flake.nix"),
            r#"{ outputs = { self }: { }; }"#,
        ).unwrap();
        let flake_ref = format!("{}#cid", dir.path().display());

        let runner = MultiMockRunner::new();
        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys
            .rebuild_native(&flake_ref, RebuildAction::Switch)
            .await;
        // Native eval fails because the attribute path doesn't exist.
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("navigate attrs") || err_msg.contains("not found"),
            "unexpected error: {err_msg}"
        );
    }

    #[tokio::test]
    async fn rebuild_native_build_failure() {
        let flake_dir = make_test_flake(Platform::NixOS, "node");
        let flake_ref = format!("{}#node", flake_dir.path().display());

        let runner = MultiMockRunner::new().with_response(
            "nix:build",
            CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: "builder for /nix/store/abc.drv failed\n".to_string(),
                exit_code: Some(1),
            },
        );

        let sys = SystemOrchestrator::with_runner(Platform::NixOS, Box::new(runner));
        let result = sys
            .rebuild_native(&flake_ref, RebuildAction::Switch)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.log.contains("builder for"));
    }

    #[tokio::test]
    async fn rebuild_native_invalid_flake_ref() {
        let runner = MultiMockRunner::new();
        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys
            .rebuild_native("no-hash-here", RebuildAction::Switch)
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SystemError::RebuildFailed(msg) => {
                assert!(msg.contains("missing '#attribute'"));
            }
            other => panic!("expected RebuildFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rebuild_native_switch_activates_system() {
        let flake_dir = make_test_flake(Platform::NixOS, "node");
        let flake_ref = format!("{}#node", flake_dir.path().display());

        let runner = MultiMockRunner::new()
            .with_response(
                "nix:build",
                CommandOutput {
                    success: true,
                    stdout: "/nix/store/xyz-system\n".to_string(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            )
            .with_response(
                "nix-env",
                CommandOutput {
                    success: true,
                    stdout: "  1   2024-01-01 (current)\n".to_string(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            )
            .with_response(
                "/nix/store/xyz-system/activate",
                CommandOutput {
                    success: true,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            );

        let sys = SystemOrchestrator::with_runner(Platform::NixOS, Box::new(runner));
        let result = sys
            .rebuild_native(&flake_ref, RebuildAction::Switch)
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.action, "switch");
        assert_eq!(result.generation, Some(1));
    }

    #[tokio::test]
    async fn rebuild_native_boot_sets_profile_without_activate() {
        let flake_dir = make_test_flake(Platform::NixOS, "node");
        let flake_ref = format!("{}#node", flake_dir.path().display());

        let runner = MultiMockRunner::new()
            .with_response(
                "nix:build",
                CommandOutput {
                    success: true,
                    stdout: "/nix/store/xyz-system\n".to_string(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            )
            .with_response(
                "nix-env",
                CommandOutput {
                    success: true,
                    stdout: "  5   2024-06-01 (current)\n".to_string(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            );

        let sys = SystemOrchestrator::with_runner(Platform::NixOS, Box::new(runner));
        let result = sys
            .rebuild_native(&flake_ref, RebuildAction::Boot)
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.action, "boot");
        assert_eq!(result.generation, Some(5));
    }

    #[tokio::test]
    async fn rebuild_native_nix_command_not_found() {
        // With native eval, the eval step succeeds but the build step
        // fails because the runner has no responses.
        let flake_dir = make_test_flake(Platform::Darwin, "cid");
        let flake_ref = format!("{}#cid", flake_dir.path().display());

        let runner = MultiMockRunner::new(); // no responses → nix build not found
        let sys = SystemOrchestrator::with_runner(Platform::Darwin, Box::new(runner));
        let result = sys
            .rebuild_native(&flake_ref, RebuildAction::Build)
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SystemError::CommandNotFound(msg) => assert!(msg.contains("nix")),
            other => panic!("expected CommandNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rebuild_native_darwin_attr_prefix() {
        // Verify Darwin uses darwinConfigurations prefix
        let flake_dir = make_test_flake(Platform::Darwin, "cid");
        let flake_ref = format!("{}#cid", flake_dir.path().display());

        let captor = Arc::new(CapturingRunner::new(CommandOutput {
            success: true,
            stdout: "/nix/store/xyz-system\n".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
        }));
        struct Forwarder2(Arc<CapturingRunner>);
        #[async_trait::async_trait]
        impl CommandRunner for Forwarder2 {
            async fn run(
                &self,
                program: &str,
                args: &[&str],
            ) -> Result<CommandOutput, CommandError> {
                self.0.run(program, args).await
            }
        }
        let sys = SystemOrchestrator::with_runner(
            Platform::Darwin,
            Box::new(Forwarder2(Arc::clone(&captor))),
        );
        // The eval step is native now — the build step uses the runner.
        let _ = sys
            .rebuild_native(&flake_ref, RebuildAction::Build)
            .await;
        let calls = captor.calls();
        // The build step should reference darwinConfigurations in the
        // installable passed to `nix build`.
        assert!(!calls.is_empty(), "expected at least one nix build call");
        let build_args = &calls[0].1;
        let build_ref = build_args.iter().find(|a| a.contains("darwinConfigurations"));
        assert!(
            build_ref.is_some(),
            "expected darwinConfigurations in build args: {build_args:?}"
        );
    }

    #[tokio::test]
    async fn rebuild_native_nixos_attr_prefix() {
        let flake_dir = make_test_flake(Platform::NixOS, "node");
        let flake_ref = format!("{}#node", flake_dir.path().display());

        let captor = Arc::new(CapturingRunner::new(CommandOutput {
            success: true,
            stdout: "/nix/store/xyz-system\n".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
        }));
        struct Forwarder3(Arc<CapturingRunner>);
        #[async_trait::async_trait]
        impl CommandRunner for Forwarder3 {
            async fn run(
                &self,
                program: &str,
                args: &[&str],
            ) -> Result<CommandOutput, CommandError> {
                self.0.run(program, args).await
            }
        }
        let sys = SystemOrchestrator::with_runner(
            Platform::NixOS,
            Box::new(Forwarder3(Arc::clone(&captor))),
        );
        let _ = sys
            .rebuild_native(&flake_ref, RebuildAction::Build)
            .await;
        let calls = captor.calls();
        assert!(!calls.is_empty(), "expected at least one nix build call");
        let build_args = &calls[0].1;
        let build_ref = build_args.iter().find(|a| a.contains("nixosConfigurations"));
        assert!(
            build_ref.is_some(),
            "expected nixosConfigurations in build args: {build_args:?}"
        );
    }
}
