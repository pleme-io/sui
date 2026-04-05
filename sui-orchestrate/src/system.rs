//! System rebuild orchestration — darwin-rebuild/nixos-rebuild replacement.

use std::process::Stdio;
use tokio::process::Command;

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
    pub success: bool,
    pub generation: Option<i64>,
    pub action: String,
    pub log: String,
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
}

impl SystemOrchestrator {
    /// Create a new orchestrator, auto-detecting the platform.
    pub fn new() -> Result<Self, SystemError> {
        let platform = Platform::detect().ok_or(SystemError::UnsupportedPlatform)?;
        Ok(Self { platform })
    }

    /// Create with an explicit platform.
    pub fn with_platform(platform: Platform) -> Self {
        Self { platform }
    }

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

        let mut cmd = Command::new(cmd_name);
        cmd.arg(action.as_str());

        if let Some(flake_ref) = flake {
            cmd.arg("--flake").arg(flake_ref);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        tracing::info!("running: {cmd_name} {action} {}", flake.unwrap_or(""));

        let output = cmd.output().await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SystemError::CommandNotFound(cmd_name.to_string())
            } else {
                SystemError::Io(e)
            }
        })?;

        let duration = start.elapsed().as_secs_f64();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let log = format!("{stdout}{stderr}");

        let generation = extract_generation(&log);

        if output.status.success() {
            tracing::info!("rebuild succeeded in {duration:.1}s");
            Ok(RebuildResult {
                success: true,
                generation,
                action: action.to_string(),
                log,
                duration_secs: duration,
            })
        } else {
            tracing::error!("rebuild failed: {}", stderr.lines().last().unwrap_or("unknown error"));
            Err(SystemError::RebuildFailed(log))
        }
    }

    /// Get the current system generation number.
    pub async fn current_generation(&self) -> Result<i64, SystemError> {
        let output = Command::new("nix-env")
            .args(["--list-generations", "--profile", "/nix/var/nix/profiles/system"])
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        // Parse the last line to find current generation
        for line in stdout.lines().rev() {
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

        let output = Command::new("nix-env")
            .args(["--list-generations", "--profile", profile])
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut generations = Vec::new();

        for line in stdout.lines() {
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

        let output = Command::new(cmd_name)
            .arg("switch")
            .arg("--rollback")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        let duration = start.elapsed().as_secs_f64();
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        Ok(RebuildResult {
            success: output.status.success(),
            generation: extract_generation(&log),
            action: "rollback".to_string(),
            log,
            duration_secs: duration,
        })
    }
}

/// Generation info.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GenerationInfo {
    pub number: i64,
    pub date: String,
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
}
