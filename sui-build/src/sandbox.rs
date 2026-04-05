//! Build sandbox abstraction.
//!
//! Platform-specific sandbox implementations isolate build processes.

/// Sandbox configuration derived from a derivation.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Paths to bind-mount read-only into the sandbox.
    pub input_paths: Vec<String>,
    /// The build directory (tmpfs).
    pub build_dir: String,
    /// Output paths to create.
    pub output_paths: Vec<String>,
    /// Whether network access is allowed.
    pub allow_network: bool,
    /// Builder executable.
    pub builder: String,
    /// Builder arguments.
    pub args: Vec<String>,
    /// Environment variables.
    pub env: Vec<(String, String)>,
}

/// Sandbox implementation trait.
pub trait Sandbox: Send + Sync {
    /// Prepare the sandbox environment.
    fn prepare(&self, config: &SandboxConfig) -> Result<(), SandboxError>;

    /// Execute the build inside the sandbox.
    fn execute(&self, config: &SandboxConfig) -> Result<SandboxResult, SandboxError>;

    /// Clean up the sandbox.
    fn cleanup(&self, config: &SandboxConfig) -> Result<(), SandboxError>;
}

/// Sandbox execution result.
#[derive(Debug)]
pub struct SandboxResult {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Sandbox errors.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox setup failed: {0}")]
    Setup(String),
    #[error("sandbox execution failed: {0}")]
    Execution(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// No-op sandbox for unsandboxed builds (development/testing).
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn prepare(&self, _config: &SandboxConfig) -> Result<(), SandboxError> {
        Ok(())
    }

    fn execute(&self, config: &SandboxConfig) -> Result<SandboxResult, SandboxError> {
        use std::process::Command;

        let mut cmd = Command::new(&config.builder);
        cmd.args(&config.args);
        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        let output = cmd.output()?;

        Ok(SandboxResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn cleanup(&self, _config: &SandboxConfig) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// Linux namespace-based sandbox (Phase 5 full implementation).
#[cfg(target_os = "linux")]
pub struct LinuxSandbox;

#[cfg(target_os = "linux")]
impl Sandbox for LinuxSandbox {
    fn prepare(&self, _config: &SandboxConfig) -> Result<(), SandboxError> {
        // TODO: Create namespaces, bind mounts, tmpfs
        Err(SandboxError::Setup("Linux sandbox not yet implemented".to_string()))
    }

    fn execute(&self, _config: &SandboxConfig) -> Result<SandboxResult, SandboxError> {
        Err(SandboxError::Execution("Linux sandbox not yet implemented".to_string()))
    }

    fn cleanup(&self, _config: &SandboxConfig) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// macOS sandbox-exec based sandbox (Phase 5 full implementation).
#[cfg(target_os = "macos")]
pub struct DarwinSandbox;

#[cfg(target_os = "macos")]
impl Sandbox for DarwinSandbox {
    fn prepare(&self, _config: &SandboxConfig) -> Result<(), SandboxError> {
        // TODO: Generate sandbox-exec profile
        Err(SandboxError::Setup("Darwin sandbox not yet implemented".to_string()))
    }

    fn execute(&self, _config: &SandboxConfig) -> Result<SandboxResult, SandboxError> {
        Err(SandboxError::Execution("Darwin sandbox not yet implemented".to_string()))
    }

    fn cleanup(&self, _config: &SandboxConfig) -> Result<(), SandboxError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_sandbox_execute() {
        let sandbox = NoSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/echo".to_string(),
            args: vec!["hello".to_string()],
            env: vec![],
        };
        let result = sandbox.execute(&config).unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "hello");
    }

    #[test]
    fn sandbox_config_defaults() {
        let config = SandboxConfig {
            input_paths: vec!["/nix/store/abc".to_string()],
            build_dir: "/tmp/sui-build-123".to_string(),
            output_paths: vec!["/nix/store/out".to_string()],
            allow_network: false,
            builder: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "echo hi".to_string()],
            env: vec![("HOME".to_string(), "/homeless-shelter".to_string())],
        };
        assert!(!config.allow_network);
        assert_eq!(config.input_paths.len(), 1);
    }
}
