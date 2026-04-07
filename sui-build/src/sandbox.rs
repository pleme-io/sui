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

impl SandboxConfig {
    /// Construct a `SandboxConfig` from a derivation and a build directory.
    ///
    /// Maps derivation fields to sandbox parameters:
    /// - `input_paths` ← `drv.input_sources`
    /// - `output_paths` ← values of `drv.outputs`
    /// - `allow_network` ← true when `__noChroot=1` in env
    /// - `builder`, `args`, `env` ← derivation fields
    pub fn from_derivation(drv: &sui_compat::derivation::Derivation, build_dir: &str) -> Self {
        Self {
            input_paths: drv.input_sources.clone(),
            build_dir: build_dir.to_owned(),
            output_paths: drv.outputs.values().map(|o| o.path.clone()).collect(),
            allow_network: drv
                .env
                .get("__noChroot")
                .is_some_and(|v| v == "1"),
            builder: drv.builder.clone(),
            args: drv.args.clone(),
            env: drv
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }
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
    /// Process exit code (0 = success).
    pub exit_code: i32,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}

/// Sandbox errors.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// The sandbox environment could not be prepared (e.g. namespace/mount failure).
    #[error("sandbox setup failed: {0}")]
    Setup(String),
    /// The sandboxed process failed to execute.
    #[error("sandbox execution failed: {0}")]
    Execution(String),
    /// An underlying I/O error.
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

    // ── NoSandbox prepare/cleanup are no-ops ─────────────────

    #[test]
    fn no_sandbox_prepare_is_noop() {
        let sandbox = NoSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/true".to_string(),
            args: vec![],
            env: vec![],
        };
        assert!(sandbox.prepare(&config).is_ok());
    }

    #[test]
    fn no_sandbox_cleanup_is_noop() {
        let sandbox = NoSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/true".to_string(),
            args: vec![],
            env: vec![],
        };
        assert!(sandbox.cleanup(&config).is_ok());
    }

    // ── NoSandbox execute with failing command ───────────────

    #[test]
    fn no_sandbox_execute_failing_command() {
        let sandbox = NoSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "exit 42".to_string()],
            env: vec![],
        };
        let result = sandbox.execute(&config).unwrap();
        assert_eq!(result.exit_code, 42);
    }

    #[test]
    fn no_sandbox_execute_nonexistent_builder() {
        let sandbox = NoSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/nonexistent/builder/12345".to_string(),
            args: vec![],
            env: vec![],
        };
        // Should return Io error since the binary doesn't exist
        assert!(sandbox.execute(&config).is_err());
    }

    // ── NoSandbox execute with env vars ──────────────────────

    #[test]
    fn no_sandbox_execute_passes_env() {
        let sandbox = NoSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "echo $MY_TEST_VAR".to_string()],
            env: vec![("MY_TEST_VAR".to_string(), "test_value_42".to_string())],
        };
        let result = sandbox.execute(&config).unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(String::from_utf8_lossy(&result.stdout).contains("test_value_42"));
    }

    // ── SandboxConfig from Derivation fields ─────────────────

    #[test]
    fn sandbox_config_from_derivation_fields() {
        use sui_compat::derivation::{Derivation, DerivationOutput};
        use std::collections::BTreeMap;

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/out-hello".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );
        let mut env = BTreeMap::new();
        env.insert("HOME".to_string(), "/homeless-shelter".to_string());
        env.insert("NIX_BUILD_TOP".to_string(), "/build".to_string());

        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec!["/nix/store/src-file".to_string()],
            system: "x86_64-linux".to_string(),
            builder: "/nix/store/bash/bin/bash".to_string(),
            args: vec!["-e".to_string(), "/nix/store/setup".to_string()],
            env,
        };

        let config = SandboxConfig::from_derivation(&drv, "/tmp/sui-build");

        assert_eq!(config.builder, "/nix/store/bash/bin/bash");
        assert_eq!(config.args, vec!["-e", "/nix/store/setup"]);
        assert_eq!(config.output_paths, vec!["/nix/store/out-hello"]);
        assert!(!config.allow_network);
        assert_eq!(config.input_paths, vec!["/nix/store/src-file"]);
        assert!(config.env.contains(&("HOME".to_string(), "/homeless-shelter".to_string())));
    }

    // ── Sandbox trait: object safety ─────────────────────────

    #[test]
    fn sandbox_trait_is_object_safe() {
        fn assert_obj_safe(_: &dyn Sandbox) {}
        assert_obj_safe(&NoSandbox);
    }

    // ── SandboxError display messages ────────────────────────

    #[test]
    fn sandbox_error_display() {
        let e = SandboxError::Setup("mount failed".to_string());
        assert!(e.to_string().contains("mount failed"));

        let e = SandboxError::Execution("process killed".to_string());
        assert!(e.to_string().contains("process killed"));
    }

    #[test]
    fn sandbox_error_from_io() {
        let io_err = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied",
        );
        let sandbox_err: SandboxError = io_err.into();
        assert!(sandbox_err.to_string().contains("access denied"));
    }

    // ── SandboxResult fields ─────────────────────────────────

    #[test]
    fn sandbox_result_captures_stderr() {
        let sandbox = NoSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "echo err >&2".to_string()],
            env: vec![],
        };
        let result = sandbox.execute(&config).unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(String::from_utf8_lossy(&result.stderr).contains("err"));
    }

    // ── SandboxConfig::from_derivation edge cases ───────────

    #[test]
    fn from_derivation_with_no_chroot_enables_network() {
        use sui_compat::derivation::{Derivation, DerivationOutput};
        use std::collections::BTreeMap;

        let mut env = BTreeMap::new();
        env.insert("__noChroot".to_string(), "1".to_string());

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/out".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );

        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/true".to_string(),
            args: vec![],
            env,
        };

        let config = SandboxConfig::from_derivation(&drv, "/tmp");
        assert!(config.allow_network);
    }

    #[test]
    fn from_derivation_no_chroot_zero_disables_network() {
        use sui_compat::derivation::{Derivation, DerivationOutput};
        use std::collections::BTreeMap;

        let mut env = BTreeMap::new();
        env.insert("__noChroot".to_string(), "0".to_string());

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/out".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );

        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/true".to_string(),
            args: vec![],
            env,
        };

        let config = SandboxConfig::from_derivation(&drv, "/tmp");
        assert!(!config.allow_network);
    }

    #[test]
    fn from_derivation_multiple_outputs() {
        use sui_compat::derivation::{Derivation, DerivationOutput};
        use std::collections::BTreeMap;

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/out-pkg".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );
        outputs.insert(
            "dev".to_string(),
            DerivationOutput {
                path: "/nix/store/dev-pkg".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );
        outputs.insert(
            "lib".to_string(),
            DerivationOutput {
                path: "/nix/store/lib-pkg".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );

        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/true".to_string(),
            args: vec![],
            env: BTreeMap::new(),
        };

        let config = SandboxConfig::from_derivation(&drv, "/build");
        assert_eq!(config.output_paths.len(), 3);
        assert_eq!(config.build_dir, "/build");
    }

    #[test]
    fn from_derivation_empty_outputs() {
        use sui_compat::derivation::Derivation;
        use std::collections::BTreeMap;

        let drv = Derivation {
            outputs: BTreeMap::new(),
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/true".to_string(),
            args: vec![],
            env: BTreeMap::new(),
        };

        let config = SandboxConfig::from_derivation(&drv, "/tmp");
        assert!(config.output_paths.is_empty());
    }

    #[test]
    fn sandbox_config_clone() {
        let config = SandboxConfig {
            input_paths: vec!["/a".into(), "/b".into()],
            build_dir: "/tmp".into(),
            output_paths: vec!["/out".into()],
            allow_network: true,
            builder: "/bin/sh".into(),
            args: vec!["-c".into(), "true".into()],
            env: vec![("K".into(), "V".into())],
        };
        let cloned = config.clone();
        assert_eq!(cloned.builder, config.builder);
        assert_eq!(cloned.env, config.env);
        assert_eq!(cloned.allow_network, config.allow_network);
    }

    // ── NoSandbox: multiple env vars ────────────────────────

    #[test]
    fn no_sandbox_execute_multiple_env_vars() {
        let sandbox = NoSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "echo $A $B".to_string()],
            env: vec![
                ("A".to_string(), "hello".to_string()),
                ("B".to_string(), "world".to_string()),
            ],
        };
        let result = sandbox.execute(&config).unwrap();
        assert_eq!(result.exit_code, 0);
        let out = String::from_utf8_lossy(&result.stdout);
        assert!(out.contains("hello"));
        assert!(out.contains("world"));
    }

    // ── NoSandbox: stdout + stderr combined ─────────────────

    #[test]
    fn no_sandbox_captures_both_streams() {
        let sandbox = NoSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                "echo stdout-line && echo stderr-line >&2".to_string(),
            ],
            env: vec![],
        };
        let result = sandbox.execute(&config).unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(String::from_utf8_lossy(&result.stdout).contains("stdout-line"));
        assert!(String::from_utf8_lossy(&result.stderr).contains("stderr-line"));
    }

    // ── NoSandbox: prepare→execute→cleanup lifecycle ────────

    #[test]
    fn no_sandbox_full_lifecycle() {
        let sandbox = NoSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/echo".to_string(),
            args: vec!["lifecycle-test".to_string()],
            env: vec![],
        };
        sandbox.prepare(&config).unwrap();
        let result = sandbox.execute(&config).unwrap();
        sandbox.cleanup(&config).unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(String::from_utf8_lossy(&result.stdout).contains("lifecycle-test"));
    }

    // ── LinuxSandbox stub tests ─────────────────────────────

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_prepare_returns_not_implemented() {
        let sandbox = LinuxSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/true".to_string(),
            args: vec![],
            env: vec![],
        };
        let err = sandbox.prepare(&config).unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_execute_returns_not_implemented() {
        let sandbox = LinuxSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/true".to_string(),
            args: vec![],
            env: vec![],
        };
        let err = sandbox.execute(&config).unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_cleanup_succeeds() {
        let sandbox = LinuxSandbox;
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: "/tmp".to_string(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/true".to_string(),
            args: vec![],
            env: vec![],
        };
        assert!(sandbox.cleanup(&config).is_ok());
    }
}
