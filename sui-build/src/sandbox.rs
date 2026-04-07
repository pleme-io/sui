//! Build sandbox abstraction.
//!
//! Platform-specific sandbox implementations isolate build processes.

/// Sandbox configuration derived from a derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
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

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            input_paths: Vec::new(),
            build_dir: String::new(),
            output_paths: Vec::new(),
            allow_network: false,
            builder: String::new(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }
}

impl SandboxConfig {
    /// Construct a `SandboxConfig` from a derivation and a build directory.
    ///
    /// Maps derivation fields to sandbox parameters:
    /// - `input_paths` ← `drv.input_sources`
    /// - `output_paths` ← values of `drv.outputs`
    /// - `allow_network` ← true when `__noChroot=1` in env
    /// - `builder`, `args`, `env` ← derivation fields
    #[must_use]
    pub fn from_derivation(drv: &sui_compat::derivation::Derivation, build_dir: impl AsRef<str>) -> Self {
        Self {
            input_paths: drv.input_sources.clone(),
            build_dir: build_dir.as_ref().to_owned(),
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

    /// Set the builder executable path.
    #[must_use]
    pub fn with_builder(mut self, builder: impl Into<String>) -> Self {
        self.builder = builder.into();
        self
    }

    /// Set the build directory.
    #[must_use]
    pub fn with_build_dir(mut self, dir: impl Into<String>) -> Self {
        self.build_dir = dir.into();
        self
    }

    /// Set the builder arguments.
    #[must_use]
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    /// Set whether network access is allowed.
    #[must_use]
    pub fn with_network(mut self, allow: bool) -> Self {
        self.allow_network = allow;
        self
    }

    /// Add an environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxResult {
    /// Process exit code (0 = success).
    pub exit_code: i32,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}

impl SandboxResult {
    /// Returns `true` if the process exited successfully (code 0).
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.exit_code == 0
    }

    /// Returns stdout decoded as lossy UTF-8.
    #[must_use]
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Returns stderr decoded as lossy UTF-8.
    #[must_use]
    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Sandbox errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    /// The sandbox process exceeded its time limit.
    #[error("sandbox timed out after {0} seconds")]
    Timeout(u64),
    /// The platform sandbox executable (`sandbox-exec`, `unshare`, etc.) was not found.
    #[error("sandbox executable not found: {0}")]
    NoSandboxExec(String),
    /// Failed to create or set up the build directory.
    #[error("build directory error: {0}")]
    BuildDirError(String),
    /// Failed to write the sandbox profile to a temporary file.
    #[error("sandbox profile write failed: {0}")]
    ProfileWriteFailed(String),
    /// The builder process exited with a non-zero status.
    #[error("builder exited with status {exit_code}")]
    BuilderExitNonZero {
        /// Exit code reported by the OS (or `-1` if killed by signal).
        exit_code: i32,
    },
    /// Failed to spawn the sandboxed process.
    #[error("failed to spawn sandboxed process: {0}")]
    SpawnFailed(String),
    /// A bind mount or chroot operation failed.
    #[error("bind mount failed: {0}")]
    BindMountFailed(String),
    /// A chroot operation failed.
    #[error("chroot failed: {0}")]
    ChrootFailed(String),
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

// ── Darwin sandbox ───────────────────────────────────────────────

/// macOS `sandbox-exec` based sandbox.
///
/// Generates a Sandbox Profile Language (SBPL) profile that:
/// - Denies all by default (`deny default`)
/// - Allows file-read access to input store paths and standard system paths
/// - Allows file-write access only to the build directory
/// - Allows process-fork and process-exec
/// - Allows network operations only when [`SandboxConfig::allow_network`] is true
///
/// Falls back to running the builder unsandboxed if `/usr/bin/sandbox-exec` is
/// not available (only relevant on non-macOS hosts via cross-test).
#[cfg(target_os = "macos")]
pub struct DarwinSandbox {
    /// Path to the `sandbox-exec` binary. Defaults to `/usr/bin/sandbox-exec`.
    sandbox_exec_path: std::path::PathBuf,
}

#[cfg(target_os = "macos")]
impl Default for DarwinSandbox {
    fn default() -> Self {
        Self {
            sandbox_exec_path: std::path::PathBuf::from("/usr/bin/sandbox-exec"),
        }
    }
}

#[cfg(target_os = "macos")]
impl DarwinSandbox {
    /// Create a new `DarwinSandbox` using the default `/usr/bin/sandbox-exec`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a `DarwinSandbox` pointing at a custom `sandbox-exec` binary.
    ///
    /// Mostly useful for tests.
    #[must_use]
    pub fn with_exec_path(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            sandbox_exec_path: path.into(),
        }
    }

    /// Returns whether the configured `sandbox-exec` binary exists and is
    /// executable.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.sandbox_exec_path.exists()
    }

    /// Run the builder unsandboxed (used as a fallback when `sandbox-exec` is
    /// missing — never reached on real macOS hosts).
    fn execute_unsandboxed(&self, config: &SandboxConfig) -> Result<SandboxResult, SandboxError> {
        eprintln!(
            "warning: DarwinSandbox running unsandboxed — sandbox-exec not found at {}",
            self.sandbox_exec_path.display()
        );
        NoSandbox.execute(config)
    }
}

/// Generate a Sandbox Profile Language (SBPL) profile for the given config.
///
/// The generated profile is a deterministic function of the config, so two
/// configs with the same fields produce identical profiles. Suitable for
/// passing via `sandbox-exec -p`.
///
/// Profile structure (matches Nix's own macOS sandbox approach):
/// - Default deny
/// - Allow `process-fork`/`process-exec`/`signal (target self)`
/// - Allow `sysctl-read` and `mach-lookup` (needed by dyld and libSystem)
/// - Allow `file-read*` globally — restricting reads breaks dyld/dyld-cache.
///   The `input_paths` from the config are included as a comment for audit.
/// - Allow `file-write*` only inside the build directory and the standard
///   `/dev` device nodes.
/// - Restrict network: deny all unless `allow_network` is set; loopback always
///   allowed so tools can bind localhost ports during the build.
#[cfg(target_os = "macos")]
#[must_use]
pub fn generate_sbpl_profile(config: &SandboxConfig) -> String {
    let mut profile = String::new();
    profile.push_str("(version 1)\n");
    profile.push_str("(deny default)\n");

    // Process / signal primitives.
    profile.push_str("(allow process-fork)\n");
    profile.push_str("(allow process-exec)\n");
    profile.push_str("(allow signal (target self))\n");

    // Required for dyld + libSystem startup.
    profile.push_str("(allow sysctl-read)\n");
    profile.push_str("(allow mach-lookup)\n");

    // Filesystem reads — wildcard. Subpath restrictions on reads are hostile
    // to dyld and the system shared cache; production Nix uses the same
    // approach. The audit list of input_paths is emitted as a comment.
    if !config.input_paths.is_empty() {
        profile.push_str("; declared input_paths:\n");
        for input in &config.input_paths {
            profile.push_str(";   ");
            profile.push_str(input);
            profile.push('\n');
        }
    }
    profile.push_str("(allow file-read*)\n");

    // Writable device nodes.
    profile.push_str("(allow file-write*\n");
    profile.push_str("    (literal \"/dev/null\")\n");
    profile.push_str("    (literal \"/dev/zero\")\n");
    profile.push_str("    (literal \"/dev/dtracehelper\")\n");
    profile.push_str("    (literal \"/dev/tty\")\n");
    profile.push_str(")\n");

    // Build dir is the only writable disk location.
    if !config.build_dir.is_empty() {
        profile.push_str("(allow file-write*\n    (subpath ");
        profile.push_str(&sbpl_quote(&config.build_dir));
        profile.push_str(")\n)\n");
    }

    // Network operations.
    if config.allow_network {
        profile.push_str("(allow network*)\n");
        profile.push_str("(allow system-socket)\n");
    } else {
        // Loopback only — useful for build helpers binding localhost ports.
        profile.push_str("(allow network* (local ip \"localhost:*\"))\n");
        profile.push_str("(allow network* (remote ip \"localhost:*\"))\n");
    }

    profile
}

/// Escape a path for inclusion as an SBPL string literal.
///
/// SBPL strings use double quotes; backslashes and double quotes inside
/// must be escaped with a backslash.
#[cfg(target_os = "macos")]
fn sbpl_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(target_os = "macos")]
impl Sandbox for DarwinSandbox {
    fn prepare(&self, config: &SandboxConfig) -> Result<(), SandboxError> {
        if !self.is_available() {
            // Not fatal — execute() falls back to unsandboxed mode.
            return Ok(());
        }
        // Make sure the build directory exists; the caller may have already
        // created it but we should not fail if it has.
        if !config.build_dir.is_empty() {
            std::fs::create_dir_all(&config.build_dir).map_err(|e| {
                SandboxError::BuildDirError(format!(
                    "failed to create build dir {}: {e}",
                    config.build_dir
                ))
            })?;
        }
        Ok(())
    }

    fn execute(&self, config: &SandboxConfig) -> Result<SandboxResult, SandboxError> {
        if !self.is_available() {
            return self.execute_unsandboxed(config);
        }

        let profile = generate_sbpl_profile(config);

        let mut cmd = std::process::Command::new(&self.sandbox_exec_path);
        cmd.arg("-p").arg(&profile);
        cmd.arg(&config.builder);
        for arg in &config.args {
            cmd.arg(arg);
        }

        // Strip the parent environment so the build is hermetic by default,
        // then re-export only what the derivation specifies.
        cmd.env_clear();
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        if !config.build_dir.is_empty() {
            cmd.current_dir(&config.build_dir);
        }

        let output = cmd
            .output()
            .map_err(|e| SandboxError::SpawnFailed(format!("sandbox-exec spawn: {e}")))?;

        Ok(SandboxResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn cleanup(&self, _config: &SandboxConfig) -> Result<(), SandboxError> {
        // Build directory cleanup is the caller's responsibility — sandbox-exec
        // does not own the directory.
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

    // ── MockSandbox ──────────────────────────────────────────

    struct MockSandbox {
        exit_code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }

    impl MockSandbox {
        fn success() -> Self {
            Self {
                exit_code: 0,
                stdout: b"mock success".to_vec(),
                stderr: Vec::new(),
            }
        }

        fn failure(exit_code: i32, stderr: &str) -> Self {
            Self {
                exit_code,
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            }
        }
    }

    impl Sandbox for MockSandbox {
        fn prepare(&self, _config: &SandboxConfig) -> Result<(), SandboxError> {
            Ok(())
        }

        fn execute(&self, _config: &SandboxConfig) -> Result<SandboxResult, SandboxError> {
            Ok(SandboxResult {
                exit_code: self.exit_code,
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
            })
        }

        fn cleanup(&self, _config: &SandboxConfig) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    #[test]
    fn mock_sandbox_success() {
        let sandbox = MockSandbox::success();
        let config = SandboxConfig::default().with_builder("/bin/true");
        let result = sandbox.execute(&config).unwrap();
        assert!(result.is_success());
        assert_eq!(result.stdout_lossy(), "mock success");
    }

    #[test]
    fn mock_sandbox_failure() {
        let sandbox = MockSandbox::failure(1, "build failed");
        let config = SandboxConfig::default().with_builder("/bin/false");
        let result = sandbox.execute(&config).unwrap();
        assert!(!result.is_success());
        assert_eq!(result.stderr_lossy(), "build failed");
        assert_eq!(result.exit_code, 1);
    }

    #[test]
    fn mock_sandbox_is_object_safe() {
        fn assert_obj(_: &dyn Sandbox) {}
        assert_obj(&MockSandbox::success());
    }

    // ── SandboxConfig builder pattern tests ─────────────────

    #[test]
    fn sandbox_config_builder_pattern() {
        let config = SandboxConfig::default()
            .with_builder("/bin/sh")
            .with_build_dir("/tmp/build")
            .with_args(vec!["-c".to_string(), "echo hi".to_string()])
            .with_network(true)
            .with_env("HOME", "/homeless-shelter");

        assert_eq!(config.builder, "/bin/sh");
        assert_eq!(config.build_dir, "/tmp/build");
        assert_eq!(config.args, vec!["-c", "echo hi"]);
        assert!(config.allow_network);
        assert_eq!(config.env, vec![("HOME".to_string(), "/homeless-shelter".to_string())]);
    }

    #[test]
    fn sandbox_config_default_values() {
        let config = SandboxConfig::default();
        assert!(config.input_paths.is_empty());
        assert!(config.build_dir.is_empty());
        assert!(config.output_paths.is_empty());
        assert!(!config.allow_network);
        assert!(config.builder.is_empty());
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
    }

    #[test]
    fn sandbox_config_eq() {
        let a = SandboxConfig::default().with_builder("/bin/sh");
        let b = SandboxConfig::default().with_builder("/bin/sh");
        assert_eq!(a, b);
    }

    // ── SandboxResult convenience methods ───────────────────

    #[test]
    fn sandbox_result_is_success() {
        let result = SandboxResult {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(result.is_success());
    }

    #[test]
    fn sandbox_result_is_not_success() {
        let result = SandboxResult {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(!result.is_success());
    }

    #[test]
    fn sandbox_result_lossy_decoders() {
        let result = SandboxResult {
            exit_code: 0,
            stdout: b"hello stdout".to_vec(),
            stderr: b"hello stderr".to_vec(),
        };
        assert_eq!(result.stdout_lossy(), "hello stdout");
        assert_eq!(result.stderr_lossy(), "hello stderr");
    }

    #[test]
    fn sandbox_result_clone_eq() {
        let a = SandboxResult {
            exit_code: 0,
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // ── Darwin sandbox tests ────────────────────────────────

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_sandbox_default_uses_system_path() {
        let s = DarwinSandbox::new();
        assert!(s.is_available(), "/usr/bin/sandbox-exec should exist on macOS");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_sandbox_with_missing_exec_falls_back() {
        let s = DarwinSandbox::with_exec_path("/nonexistent/sandbox-exec/zzz");
        assert!(!s.is_available());
        let config = SandboxConfig::default()
            .with_builder("/bin/echo")
            .with_args(vec!["fallback".to_string()]);
        let result = s.execute(&config).expect("fallback execute should succeed");
        assert_eq!(result.exit_code, 0);
        assert!(String::from_utf8_lossy(&result.stdout).contains("fallback"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_generate_sbpl_profile_has_required_directives() {
        let config = SandboxConfig::default()
            .with_builder("/bin/echo")
            .with_build_dir("/tmp/sui-build-darwin");
        let profile = generate_sbpl_profile(&config);
        assert!(profile.starts_with("(version 1)"));
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow process-fork)"));
        assert!(profile.contains("(allow process-exec)"));
        assert!(profile.contains("(allow file-read*)"));
        assert!(profile.contains("(allow sysctl-read)"));
        assert!(profile.contains("(allow mach-lookup)"));
        assert!(profile.contains("/dev/null"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_generate_sbpl_profile_includes_input_paths_audit() {
        let mut config = SandboxConfig::default();
        config.input_paths.push("/nix/store/abc-foo".to_string());
        config.input_paths.push("/nix/store/def-bar".to_string());
        let profile = generate_sbpl_profile(&config);
        // Inputs are emitted as audit comments since reads are wildcard.
        assert!(profile.contains("declared input_paths"));
        assert!(profile.contains("/nix/store/abc-foo"));
        assert!(profile.contains("/nix/store/def-bar"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_generate_sbpl_profile_includes_build_dir_write() {
        let config = SandboxConfig::default()
            .with_build_dir("/tmp/sui-build-write-test");
        let profile = generate_sbpl_profile(&config);
        // file-write* must contain the build dir as a subpath
        let write_section_idx = profile
            .find("(allow file-write*")
            .expect("expected file-write* section");
        let write_section = &profile[write_section_idx..];
        assert!(write_section.contains("/tmp/sui-build-write-test"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_generate_sbpl_profile_network_disabled_by_default() {
        let config = SandboxConfig::default().with_build_dir("/tmp/x");
        let profile = generate_sbpl_profile(&config);
        assert!(!profile.contains("(allow network*)"));
        // Localhost-only is OK
        assert!(profile.contains("localhost"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_generate_sbpl_profile_network_enabled() {
        let config = SandboxConfig::default()
            .with_build_dir("/tmp/x")
            .with_network(true);
        let profile = generate_sbpl_profile(&config);
        assert!(profile.contains("(allow network*)"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_sbpl_quote_escapes_special_chars() {
        assert_eq!(sbpl_quote("/tmp/foo"), "\"/tmp/foo\"");
        assert_eq!(sbpl_quote("/path/with\"quote"), "\"/path/with\\\"quote\"");
        assert_eq!(sbpl_quote("/back\\slash"), "\"/back\\\\slash\"");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_sandbox_prepare_creates_build_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let build_dir = tmp.path().join("build-prep-test");
        let config = SandboxConfig::default()
            .with_build_dir(build_dir.to_string_lossy().to_string())
            .with_builder("/bin/true");
        let s = DarwinSandbox::new();
        s.prepare(&config).unwrap();
        assert!(build_dir.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_sandbox_executes_echo_through_real_sandbox_exec() {
        // Integration test — runs /bin/echo via real sandbox-exec.
        let tmp = tempfile::tempdir().unwrap();
        let config = SandboxConfig::default()
            .with_build_dir(tmp.path().to_string_lossy().to_string())
            .with_builder("/bin/echo")
            .with_args(vec!["hello".to_string()]);
        let s = DarwinSandbox::new();
        if !s.is_available() {
            // Skip if sandbox-exec missing (CI sometimes hides it).
            return;
        }
        s.prepare(&config).unwrap();
        let result = s.execute(&config).unwrap();
        assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr_lossy());
        assert!(
            String::from_utf8_lossy(&result.stdout).contains("hello"),
            "expected stdout to contain 'hello', got {:?}",
            result.stdout_lossy()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_sandbox_cleanup_is_noop() {
        let s = DarwinSandbox::new();
        let config = SandboxConfig::default().with_build_dir("/tmp/x");
        assert!(s.cleanup(&config).is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_sandbox_is_object_safe() {
        fn assert_obj_safe(_: &dyn Sandbox) {}
        assert_obj_safe(&DarwinSandbox::new());
    }
}
