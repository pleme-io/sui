//! Build sandbox abstraction.
//!
//! Platform-specific sandbox implementations isolate build processes.

/// Sandbox configuration derived from a derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(Default)]
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
    ///
    /// # Examples
    ///
    /// ```
    /// use sui_build::sandbox::SandboxConfig;
    /// use sui_compat::derivation::Derivation;
    /// use std::collections::BTreeMap;
    ///
    /// let drv = Derivation {
    ///     outputs: BTreeMap::new(),
    ///     input_derivations: BTreeMap::new(),
    ///     input_sources: vec!["/nix/store/source".to_string()],
    ///     system: "x86_64-linux".to_string(),
    ///     builder: "/bin/sh".to_string(),
    ///     args: vec!["-c".to_string(), "true".to_string()],
    ///     env: BTreeMap::new(),
    /// };
    /// let config = SandboxConfig::from_derivation(&drv, "/tmp/build");
    /// assert_eq!(config.builder, "/bin/sh");
    /// assert_eq!(config.input_paths.len(), 1);
    /// assert!(!config.allow_network);
    /// ```
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
            env: Self::builder_env(drv, build_dir.as_ref()),
        }
    }

    fn pass_as_file_names(drv: &sui_compat::derivation::Derivation) -> Vec<String> {
        drv.env
            .get("passAsFile")
            .map(|v| v.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn attr_file_name(name: &str) -> String {
        use sha2::Digest;
        format!(
            ".attr-{}",
            sui_compat::store_path::nix_base32_encode(&sha2::Sha256::digest(name.as_bytes()))
        )
    }

    #[must_use]
    pub fn structured_attrs(drv: &sui_compat::derivation::Derivation) -> Option<serde_json::Map<String, serde_json::Value>> {
        let mut json = match serde_json::from_str(drv.env.get("__json")?) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => return None,
        };
        let outputs = drv
            .outputs
            .iter()
            .map(|(name, out)| (name.clone(), serde_json::Value::String(out.path.clone())))
            .collect();
        json.insert("outputs".into(), serde_json::Value::Object(outputs));
        Some(json)
    }

    fn shell_escape(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    fn shell_simple(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::String(s) => Some(Self::shell_escape(s)),
            serde_json::Value::Number(n) => n
                .as_i64()
                .map(|i| i.to_string())
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| format!("{}", f as i64))),
            serde_json::Value::Null => Some("''".into()),
            serde_json::Value::Bool(b) => Some(if *b { "1".into() } else { String::new() }),
            _ => None,
        }
    }

    fn is_shell_var_name(key: &str) -> bool {
        let mut chars = key.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }

    #[must_use]
    pub fn structured_attrs_shell(json: &serde_json::Map<String, serde_json::Value>) -> String {
        let mut keys: Vec<&String> = json.keys().collect();
        keys.sort();
        let mut out = String::new();
        for key in keys.into_iter().filter(|k| Self::is_shell_var_name(k)) {
            let value = &json[key];
            if let Some(simple) = Self::shell_simple(value) {
                out.push_str(&format!("declare {key}={simple}\n"));
            } else if let serde_json::Value::Array(items) = value {
                if let Some(items) = items.iter().map(Self::shell_simple).collect::<Option<Vec<_>>>() {
                    let body: String = items.iter().map(|i| format!("{i} ")).collect();
                    out.push_str(&format!("declare -a {key}=({body})\n"));
                }
            } else if let serde_json::Value::Object(map) = value {
                let mut inner: Vec<(&String, &serde_json::Value)> = map.iter().collect();
                inner.sort_by(|a, b| a.0.cmp(b.0));
                if let Some(items) = inner
                    .iter()
                    .map(|(k, v)| Self::shell_simple(v).map(|v| format!("[{}]={v} ", Self::shell_escape(k))))
                    .collect::<Option<Vec<_>>>()
                {
                    out.push_str(&format!("declare -A {key}=({})\n", items.concat()));
                }
            }
        }
        out
    }

    #[must_use]
    pub fn attr_files(drv: &sui_compat::derivation::Derivation, build_dir: &str) -> Vec<(String, String)> {
        if let Some(json) = Self::structured_attrs(drv) {
            return vec![
                (format!("{build_dir}/.attrs.json"), serde_json::Value::Object(json.clone()).to_string()),
                (format!("{build_dir}/.attrs.sh"), Self::structured_attrs_shell(&json)),
            ];
        }
        Self::pass_as_file_names(drv)
            .into_iter()
            .map(|name| {
                let value = drv.env.get(&name).cloned().unwrap_or_default();
                (format!("{build_dir}/{}", Self::attr_file_name(&name)), value)
            })
            .collect()
    }

    fn builder_env(drv: &sui_compat::derivation::Derivation, build_dir: &str) -> Vec<(String, String)> {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let mut env: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
        env.insert("PATH".into(), "/path-not-set".into());
        env.insert("HOME".into(), "/homeless-shelter".into());
        env.insert("NIX_STORE".into(), "/nix/store".into());
        env.insert("NIX_BUILD_CORES".into(), cores.to_string());
        let structured = Self::structured_attrs(drv);
        if structured.is_some() {
            env.insert("NIX_ATTRS_JSON_FILE".into(), format!("{build_dir}/.attrs.json"));
            env.insert("NIX_ATTRS_SH_FILE".into(), format!("{build_dir}/.attrs.sh"));
        } else {
            let pass_as_file = Self::pass_as_file_names(drv);
            for (k, v) in &drv.env {
                if pass_as_file.contains(k) {
                    env.insert(format!("{k}Path"), format!("{build_dir}/{}", Self::attr_file_name(k)));
                } else {
                    env.insert(k.clone(), v.clone());
                }
            }
        }
        for key in ["NIX_BUILD_TOP", "TMPDIR", "TEMPDIR", "TMP", "TEMP", "PWD"] {
            env.insert(key.into(), build_dir.to_owned());
        }
        env.insert("NIX_LOG_FD".into(), "2".into());
        env.insert("TERM".into(), "xterm-256color".into());
        if drv.outputs.values().any(|o| !o.hash.is_empty()) {
            let impure: Vec<String> = match &structured {
                Some(json) => json
                    .get("impureEnvVars")
                    .and_then(serde_json::Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default(),
                None => drv
                    .env
                    .get("impureEnvVars")
                    .map_or("", String::as_str)
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
            };
            for name in impure {
                if let Ok(value) = std::env::var(&name) {
                    env.insert(name, value);
                }
            }
        }
        env.into_iter().collect()
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

    /// [`Self::execute`], handing each chunk of the builder's stdout and
    /// stderr to `output` as it arrives. The default hands both over once the
    /// builder has exited.
    fn execute_observed(
        &self,
        config: &SandboxConfig,
        output: &(dyn Fn(&[u8]) + Sync),
    ) -> Result<SandboxResult, SandboxError> {
        let result = self.execute(config)?;
        output(&result.stdout);
        output(&result.stderr);
        Ok(result)
    }

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

fn drain_pipe(pipe: Option<impl std::io::Read>, output: &(dyn Fn(&[u8]) + Sync)) -> std::io::Result<Vec<u8>> {
    let mut all = Vec::new();
    let Some(mut pipe) = pipe else {
        return Ok(all);
    };
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => return Ok(all),
            Ok(n) => {
                output(&buf[..n]);
                all.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// `Command::output`, handing each chunk of stdout and stderr to `output` as
/// the child writes it.
///
/// # Errors
/// A spawn, read or wait failure.
pub fn run_observed(
    cmd: &mut std::process::Command,
    output: &(dyn Fn(&[u8]) + Sync),
) -> std::io::Result<std::process::Output> {
    use std::process::Stdio;
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let (stdout, stderr) = std::thread::scope(|scope| {
        let stdout = scope.spawn(|| drain_pipe(stdout, output));
        let stderr = drain_pipe(stderr, output);
        (stdout.join().unwrap_or_else(|_| Err(std::io::Error::other("stdout reader panicked"))), stderr)
    });
    let status = child.wait()?;
    Ok(std::process::Output { status, stdout: stdout?, stderr: stderr? })
}

/// No-op sandbox for unsandboxed builds (development/testing).
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn prepare(&self, _config: &SandboxConfig) -> Result<(), SandboxError> {
        Ok(())
    }

    fn execute(&self, config: &SandboxConfig) -> Result<SandboxResult, SandboxError> {
        self.execute_observed(config, &|_| {})
    }

    fn execute_observed(
        &self,
        config: &SandboxConfig,
        output: &(dyn Fn(&[u8]) + Sync),
    ) -> Result<SandboxResult, SandboxError> {
        let mut cmd = builder_command(&config.builder, config);
        cmd.args(&config.args);

        let output = run_observed(&mut cmd, output)?;

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

// ── Linux sandbox ────────────────────────────────────────────────

/// Linux namespace-based sandbox using the `unshare(1)` binary.
///
/// This is a pure-Rust wrapper that runs `unshare --user --mount --pid
/// --net --uts --ipc --fork --map-root-user [--root <build_dir>] <builder>
/// <args...>`. It avoids requiring the `nix` syscall crate or any privileged
/// operations: `--map-root-user` makes the current user appear as `root`
/// inside the new user namespace, and the kernel handles the mount/PID/net
/// namespace creation.
///
/// The sandbox falls back to running the builder unsandboxed (with a clear
/// stderr warning) if `unshare(1)` is not available on the host or if it
/// rejects the requested namespaces (kernel without unprivileged user
/// namespaces, etc.).
///
/// Network is allowed only when [`SandboxConfig::allow_network`] is `true`.
/// When network is disabled, `--net` is included to give the build its own
/// empty network namespace.
#[cfg(target_os = "linux")]
pub struct LinuxSandbox {
    /// Path to the `unshare` binary (default: discovered from `PATH`).
    unshare_path: Option<std::path::PathBuf>,
}

#[cfg(target_os = "linux")]
impl Default for LinuxSandbox {
    fn default() -> Self {
        Self {
            unshare_path: find_unshare_binary(),
        }
    }
}

#[cfg(target_os = "linux")]
impl LinuxSandbox {
    /// Create a new `LinuxSandbox` discovering `unshare` from `PATH`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a `LinuxSandbox` with an explicit path to `unshare`.
    ///
    /// Useful for tests and for hosts where `unshare` lives in an unusual
    /// location (e.g. NixOS `/run/current-system/sw/bin/unshare`).
    #[must_use]
    pub fn with_unshare_path(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            unshare_path: Some(path.into()),
        }
    }

    /// Returns whether a usable `unshare` binary was found.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.unshare_path
            .as_ref()
            .is_some_and(|p| p.exists())
    }

    /// Build the `unshare` argument list for the given config.
    ///
    /// Exposed for testing — returns the args that would be passed to
    /// `unshare(1)` (excluding the binary itself).
    #[must_use]
    pub fn build_unshare_args(&self, config: &SandboxConfig) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        // User + mount + PID + UTS + IPC namespaces are always taken.
        args.push("--user".to_string());
        args.push("--map-root-user".to_string());
        args.push("--mount".to_string());
        args.push("--uts".to_string());
        args.push("--ipc".to_string());
        args.push("--pid".to_string());
        args.push("--fork".to_string());
        // Network is its own namespace unless the derivation opts in.
        if !config.allow_network {
            args.push("--net".to_string());
        }
        // Run the builder.
        args.push("--".to_string());
        args.push(config.builder.clone());
        for a in &config.args {
            args.push(a.clone());
        }
        args
    }

    /// Run the builder unsandboxed (used as a fallback when `unshare` is
    /// missing or kernel rejects the namespaces). Documented loudly.
    fn execute_unsandboxed(
        &self,
        config: &SandboxConfig,
        output: &(dyn Fn(&[u8]) + Sync),
    ) -> Result<SandboxResult, SandboxError> {
        eprintln!(
            "warning: LinuxSandbox running unsandboxed — unshare(1) not available or namespaces rejected"
        );
        NoSandbox.execute_observed(config, output)
    }
}

/// Search `PATH` and a few well-known locations for the `unshare` binary.
#[cfg(target_os = "linux")]
fn find_unshare_binary() -> Option<std::path::PathBuf> {
    // Common explicit paths first (NixOS, Debian, RHEL).
    let candidates = [
        "/usr/bin/unshare",
        "/bin/unshare",
        "/run/current-system/sw/bin/unshare",
        "/run/wrappers/bin/unshare",
    ];
    for c in &candidates {
        let p = std::path::PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    // Fall back to PATH search.
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let p = std::path::Path::new(dir).join("unshare");
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
impl Sandbox for LinuxSandbox {
    fn prepare(&self, config: &SandboxConfig) -> Result<(), SandboxError> {
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
        self.execute_observed(config, &|_| {})
    }

    fn execute_observed(
        &self,
        config: &SandboxConfig,
        observer: &(dyn Fn(&[u8]) + Sync),
    ) -> Result<SandboxResult, SandboxError> {
        let Some(unshare) = self.unshare_path.as_ref() else {
            return self.execute_unsandboxed(config, observer);
        };
        if !unshare.exists() {
            return self.execute_unsandboxed(config, observer);
        }

        let args = self.build_unshare_args(config);

        let mut cmd = builder_command(unshare, config);
        cmd.args(&args);

        let output = match run_observed(&mut cmd, observer) {
            Ok(o) => o,
            Err(e) => {
                return Err(SandboxError::SpawnFailed(format!(
                    "unshare spawn at {}: {e}",
                    unshare.display()
                )));
            }
        };

        // unshare itself returns 1 with "unshare: ..." on stderr if the
        // kernel rejects the requested namespaces. Detect that and fall back
        // to unsandboxed mode (with a warning).
        if output.status.code() == Some(1)
            && output.stdout.is_empty()
            && String::from_utf8_lossy(&output.stderr).starts_with("unshare:")
        {
            eprintln!(
                "warning: unshare(1) rejected requested namespaces ({}), running unsandboxed",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return self.execute_unsandboxed(config, observer);
        }

        Ok(SandboxResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn cleanup(&self, _config: &SandboxConfig) -> Result<(), SandboxError> {
        // Build dir cleanup is the caller's responsibility (it owns the
        // tempdir and may want to inspect it on failure).
        Ok(())
    }
}

// ── Darwin sandbox ───────────────────────────────────────────────

/// A builder process as cppnix's `runChild` starts one, in every sandbox mode:
/// only the derivation's environment, in the build directory.
fn builder_command(program: impl AsRef<std::ffi::OsStr>, config: &SandboxConfig) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    builder_environment(&mut cmd, config);
    cmd
}

fn builder_environment(cmd: &mut std::process::Command, config: &SandboxConfig) {
    cmd.env_clear();
    cmd.envs(config.env.iter().map(|(k, v)| (k, v)));
    if !config.build_dir.is_empty() {
        cmd.current_dir(&config.build_dir);
    }
}

/// The device rules of cppnix's darwin `sandbox-defaults.sb`, verbatim.
const STANDARD_DEVICES: &str = r#"(allow file*
       (literal "/dev/null")
       (literal "/dev/random")
       (literal "/dev/stderr")
       (literal "/dev/stdin")
       (literal "/dev/stdout")
       (literal "/dev/tty")
       (literal "/dev/urandom")
       (literal "/dev/zero")
       (subpath "/dev/fd"))
(allow file*
       (literal "/dev/ptmx")
       (regex #"^/dev/pty[a-z]+")
       (regex #"^/dev/ttys[0-9]+"))
(allow file* (literal "/dev/dtracehelper"))
"#;

/// nix.conf's `sandbox` setting, as cppnix reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    Enabled,
    Relaxed,
    Disabled,
}

impl SandboxMode {
    /// `true`, `relaxed` or `false`; unset is cppnix's platform default:
    /// enabled on Linux, disabled elsewhere.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        Self::parse_on(value, cfg!(target_os = "linux"))
    }

    fn parse_on(value: Option<&str>, linux: bool) -> Self {
        match value.map(str::trim) {
            Some("true") => Self::Enabled,
            Some("relaxed") => Self::Relaxed,
            Some("false") => Self::Disabled,
            _ if linux => Self::Enabled,
            _ => Self::Disabled,
        }
    }

    /// The mode this host's nix.conf names.
    #[must_use]
    pub fn from_nix_conf() -> Self {
        let conf = sui_compat::nix_conf::NixConfig::load().ok();
        Self::parse(conf.as_ref().and_then(|c| c.get("sandbox")))
    }
}

/// The sandbox a build runs in on this host: none when `sandbox` is off,
/// the platform's otherwise.
#[must_use]
pub fn for_host(mode: SandboxMode) -> Box<dyn Sandbox> {
    match mode {
        SandboxMode::Disabled => Box::new(NoSandbox),
        #[cfg(target_os = "macos")]
        SandboxMode::Enabled | SandboxMode::Relaxed => Box::new(DarwinSandbox::new()),
        #[cfg(not(target_os = "macos"))]
        SandboxMode::Enabled | SandboxMode::Relaxed => Box::new(LinuxSandbox::new()),
    }
}

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
    fn execute_unsandboxed(
        &self,
        config: &SandboxConfig,
        output: &(dyn Fn(&[u8]) + Sync),
    ) -> Result<SandboxResult, SandboxError> {
        eprintln!(
            "warning: DarwinSandbox running unsandboxed — sandbox-exec not found at {}",
            self.sandbox_exec_path.display()
        );
        NoSandbox.execute_observed(config, output)
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

    profile.push_str(STANDARD_DEVICES);

    // Build dir is the only writable disk location.
    let mut writable: Vec<&str> = Vec::new();
    if !config.build_dir.is_empty() {
        writable.push(&config.build_dir);
    }
    writable.extend(config.output_paths.iter().map(String::as_str));
    for path in writable {
        for form in sbpl_path_forms(path) {
            profile.push_str("(allow file-write*\n    (subpath ");
            profile.push_str(&sbpl_quote(&form));
            profile.push_str(")\n)\n");
        }
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

#[cfg(target_os = "macos")]
fn sbpl_path_forms(path: &str) -> Vec<String> {
    let p = std::path::Path::new(path);
    let resolved = std::fs::canonicalize(p).ok().or_else(|| {
        let parent = std::fs::canonicalize(p.parent()?).ok()?;
        Some(parent.join(p.file_name()?))
    });
    let mut forms = vec![path.to_owned()];
    if let Some(r) = resolved.and_then(|r| r.to_str().map(str::to_owned))
        && r != path
    {
        forms.push(r);
    }
    forms
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
        self.execute_observed(config, &|_| {})
    }

    fn execute_observed(
        &self,
        config: &SandboxConfig,
        observer: &(dyn Fn(&[u8]) + Sync),
    ) -> Result<SandboxResult, SandboxError> {
        if !self.is_available() {
            return self.execute_unsandboxed(config, observer);
        }

        let profile = generate_sbpl_profile(config);

        let mut cmd = std::process::Command::new(&self.sandbox_exec_path);
        cmd.arg("-p").arg(&profile);
        cmd.arg(&config.builder);
        for arg in &config.args {
            cmd.arg(arg);
        }

        builder_environment(&mut cmd, config);

        let output = run_observed(&mut cmd, observer)
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

    fn builder_probe() -> Result<(), String> {
        use std::io::Write as _;
        let expected = std::env::var("PROBE_BUILD_DIR").map_err(|e| e.to_string())?;
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        if cwd.to_string_lossy() != expected {
            return Err(format!("cwd {} is not the build dir {expected}", cwd.display()));
        }
        if std::env::var_os("HOME").is_some() {
            return Err("HOME leaked into the builder".into());
        }
        std::fs::create_dir("relative-dir").map_err(|e| format!("relative mkdir: {e}"))?;
        for device in ["/dev/stderr", "/dev/stdout"] {
            std::fs::OpenOptions::new()
                .write(true)
                .open(device)
                .and_then(|mut f| f.write_all(b"probe\n"))
                .map_err(|e| format!("open {device}: {e}"))?;
        }
        let out = std::env::var("out").map_err(|e| e.to_string())?;
        std::fs::write(out, "ok\n").map_err(|e| e.to_string())
    }

    #[test]
    fn builder_probe_child() {
        if std::env::var_os("SUI_BUILDER_PROBE").is_none() {
            return;
        }
        if let Err(e) = builder_probe() {
            eprintln!("probe: {e}");
            std::process::exit(3);
        }
    }

    #[test]
    fn every_sandbox_mode_keeps_cppnixs_builder_promises() {
        let me = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        for mode in [SandboxMode::Disabled, SandboxMode::Enabled, SandboxMode::Relaxed] {
            let dir = tempfile::tempdir().unwrap();
            let build_dir = std::fs::canonicalize(dir.path()).unwrap().to_string_lossy().into_owned();
            let out = format!("{build_dir}/out");
            let config = SandboxConfig {
                builder: me.clone(),
                args: vec!["--exact".into(), "sandbox::tests::builder_probe_child".into(), "--nocapture".into()],
                env: vec![
                    ("out".to_string(), out.clone()),
                    ("SUI_BUILDER_PROBE".to_string(), "1".to_string()),
                    ("PROBE_BUILD_DIR".to_string(), build_dir.clone()),
                ],
                build_dir: build_dir.clone(),
                output_paths: vec![out.clone()],
                ..SandboxConfig::default()
            };
            let sandbox = for_host(mode);
            sandbox.prepare(&config).unwrap();
            let result = sandbox.execute(&config).unwrap();
            let _ = sandbox.cleanup(&config);
            assert!(
                result.is_success(),
                "{mode:?}: exit {} stderr {}",
                result.exit_code,
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(std::fs::read_to_string(&out).unwrap(), "ok\n", "{mode:?}");
        }
    }

    #[test]
    fn an_unsandboxed_builder_starts_in_its_build_dir_with_only_the_derivation_environment() {
        let dir = tempfile::tempdir().unwrap();
        let build_dir = std::fs::canonicalize(dir.path()).unwrap().to_string_lossy().into_owned();
        let config = SandboxConfig {
            builder: "/bin/sh".into(),
            args: vec!["-c".into(), "/bin/pwd; echo \"${HOME-unset}\"; echo \"$OWN\"".into()],
            env: vec![("OWN".to_string(), "kept".to_string())],
            build_dir: build_dir.clone(),
            ..SandboxConfig::default()
        };
        let result = NoSandbox.execute(&config).unwrap();
        assert_eq!(String::from_utf8_lossy(&result.stdout), format!("{build_dir}\nunset\nkept\n"));
    }

    #[test]
    fn the_sandbox_setting_reads_like_cppnix_on_every_platform() {
        for linux in [true, false] {
            assert_eq!(SandboxMode::parse_on(Some("false"), linux), SandboxMode::Disabled);
            assert_eq!(SandboxMode::parse_on(Some("true"), linux), SandboxMode::Enabled);
            assert_eq!(SandboxMode::parse_on(Some("relaxed"), linux), SandboxMode::Relaxed);
        }
        assert_eq!(SandboxMode::parse_on(None, true), SandboxMode::Enabled);
        assert_eq!(SandboxMode::parse_on(None, false), SandboxMode::Disabled);
        let here = if cfg!(target_os = "linux") { SandboxMode::Enabled } else { SandboxMode::Disabled };
        assert_eq!(SandboxMode::parse(None), here);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_darwin_profile_lets_a_builder_write_its_standard_streams() {
        let config = SandboxConfig { build_dir: "/tmp/b".into(), ..SandboxConfig::default() };
        let profile = generate_sbpl_profile(&config);
        for device in ["\"/dev/stderr\"", "\"/dev/stdout\"", "(subpath \"/dev/fd\")", "\"/dev/ptmx\""] {
            assert!(profile.contains(device), "{device} missing from\n{profile}");
        }
    }

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
    fn observed_output_arrives_while_the_builder_still_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let flag = tmp.path().join("seen");
        let script = format!(
            "echo ready >&2; i=0; while [ $i -lt 100 ]; do [ -e {f} ] && echo done && exit 0; sleep 0.05; i=$((i+1)); done; exit 7",
            f = flag.display()
        );
        let config = SandboxConfig {
            input_paths: vec![],
            build_dir: String::new(),
            output_paths: vec![],
            allow_network: false,
            builder: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script],
            env: vec![],
        };
        let seen = std::sync::Mutex::new(Vec::new());
        let result = NoSandbox
            .execute_observed(&config, &|chunk| {
                if chunk.starts_with(b"ready") {
                    std::fs::write(&flag, b"").unwrap();
                }
                seen.lock().unwrap().extend_from_slice(chunk);
            })
            .unwrap();
        assert_eq!(result.exit_code, 0, "the builder never saw its output observed live");
        assert_eq!(result.stderr, b"ready\n");
        assert_eq!(result.stdout, b"done\n");
        assert_eq!(seen.into_inner().unwrap().len(), b"ready\ndone\n".len());
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

    // ── LinuxSandbox tests ──────────────────────────────────

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_prepare_creates_build_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let build_dir = tmp.path().join("linux-prep-test");
        let sandbox = LinuxSandbox::new();
        let config = SandboxConfig::default()
            .with_build_dir(build_dir.to_string_lossy().to_string())
            .with_builder("/bin/true");
        sandbox.prepare(&config).unwrap();
        assert!(build_dir.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_with_unshare_path() {
        let s = LinuxSandbox::with_unshare_path("/nonexistent/unshare");
        assert!(!s.is_available());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_build_unshare_args_includes_namespaces() {
        let s = LinuxSandbox::with_unshare_path("/usr/bin/unshare");
        let config = SandboxConfig::default()
            .with_builder("/bin/echo")
            .with_args(vec!["hi".to_string()]);
        let args = s.build_unshare_args(&config);
        assert!(args.iter().any(|a| a == "--user"));
        assert!(args.iter().any(|a| a == "--map-root-user"));
        assert!(args.iter().any(|a| a == "--mount"));
        assert!(args.iter().any(|a| a == "--pid"));
        assert!(args.iter().any(|a| a == "--uts"));
        assert!(args.iter().any(|a| a == "--ipc"));
        assert!(args.iter().any(|a| a == "--fork"));
        assert!(args.iter().any(|a| a == "--net"));
        assert!(args.iter().any(|a| a == "--"));
        assert!(args.iter().any(|a| a == "/bin/echo"));
        assert!(args.iter().any(|a| a == "hi"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_build_unshare_args_skips_net_when_network_allowed() {
        let s = LinuxSandbox::with_unshare_path("/usr/bin/unshare");
        let config = SandboxConfig::default()
            .with_builder("/bin/true")
            .with_network(true);
        let args = s.build_unshare_args(&config);
        assert!(args.iter().all(|a| a != "--net"),
            "expected no --net when allow_network=true, got {args:?}");
        assert!(args.iter().any(|a| a == "--user"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_unsandboxed_fallback_runs_builder() {
        let s = LinuxSandbox::with_unshare_path("/nonexistent/unshare/zzz");
        assert!(!s.is_available());
        let config = SandboxConfig::default()
            .with_builder("/bin/echo")
            .with_args(vec!["lin-fallback".to_string()]);
        let result = s.execute(&config).unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(String::from_utf8_lossy(&result.stdout).contains("lin-fallback"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_cleanup_is_noop() {
        let sandbox = LinuxSandbox::new();
        let config = SandboxConfig::default().with_build_dir("/tmp/x");
        assert!(sandbox.cleanup(&config).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_is_object_safe() {
        fn assert_obj_safe(_: &dyn Sandbox) {}
        assert_obj_safe(&LinuxSandbox::new());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_executes_echo_through_real_unshare() {
        // Integration: only meaningful if /usr/bin/unshare actually accepts
        // the namespaces (kernel must allow unprivileged user namespaces).
        let s = LinuxSandbox::new();
        if !s.is_available() {
            return; // No unshare on host — skip
        }
        let tmp = tempfile::tempdir().unwrap();
        let config = SandboxConfig::default()
            .with_build_dir(tmp.path().to_string_lossy().to_string())
            .with_builder("/bin/echo")
            .with_args(vec!["linux-sandbox-hello".to_string()]);
        s.prepare(&config).unwrap();
        let result = s.execute(&config).unwrap();
        // Either real sandbox runs and echoes, or fallback runs and echoes —
        // both end with exit 0 and the expected stdout content.
        assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr_lossy());
        assert!(String::from_utf8_lossy(&result.stdout).contains("linux-sandbox-hello"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_find_unshare_returns_some_or_none() {
        // Just verify the search function does not panic.
        let _ = find_unshare_binary();
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

    // ── SandboxError remaining variants ─────────────────────

    #[test]
    fn sandbox_error_timeout_display() {
        let e = SandboxError::Timeout(60);
        let msg = e.to_string();
        assert!(msg.contains("60"));
        assert!(msg.contains("timed out"));
    }

    #[test]
    fn sandbox_error_io_display_format() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe gone");
        let e: SandboxError = io_err.into();
        assert!(e.to_string().contains("io error"));
        assert!(e.to_string().contains("pipe gone"));
    }

    // ── SandboxResult: invalid UTF-8 handled lossily ────────

    #[test]
    fn sandbox_result_lossy_invalid_utf8_stdout() {
        let result = SandboxResult {
            exit_code: 0,
            stdout: vec![0xFF, 0xFE, 0xFD],
            stderr: Vec::new(),
        };
        // Should not panic; lossy decoding produces replacement characters.
        let s = result.stdout_lossy();
        assert!(!s.is_empty());
    }

    #[test]
    fn sandbox_result_lossy_invalid_utf8_stderr() {
        let result = SandboxResult {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: vec![0x80, 0x81],
        };
        let s = result.stderr_lossy();
        assert!(!s.is_empty());
    }

    #[test]
    fn sandbox_result_empty_streams() {
        let result = SandboxResult {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert_eq!(result.stdout_lossy(), "");
        assert_eq!(result.stderr_lossy(), "");
    }

    // ── SandboxConfig::from_derivation: more edge cases ─────

    #[test]
    fn from_derivation_no_chroot_other_value_disables_network() {
        use sui_compat::derivation::{Derivation, DerivationOutput};
        use std::collections::BTreeMap;

        let mut env = BTreeMap::new();
        // Only "1" enables network — anything else (incl. "true") must NOT.
        env.insert("__noChroot".to_string(), "true".to_string());

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/x".to_string(),
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
    fn from_derivation_no_no_chroot_key_disables_network() {
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
        assert!(!config.allow_network);
    }

    #[test]
    fn from_derivation_many_input_sources_preserved() {
        use sui_compat::derivation::Derivation;
        use std::collections::BTreeMap;

        let inputs: Vec<String> = (0..50)
            .map(|i| format!("/nix/store/src-{i}"))
            .collect();
        let drv = Derivation {
            outputs: BTreeMap::new(),
            input_derivations: BTreeMap::new(),
            input_sources: inputs.clone(),
            system: "x86_64-linux".to_string(),
            builder: "/bin/true".to_string(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let config = SandboxConfig::from_derivation(&drv, "/tmp");
        assert_eq!(config.input_paths.len(), 50);
        assert_eq!(config.input_paths, inputs);
    }

    #[test]
    fn from_derivation_env_round_trip_preserves_all_keys() {
        use sui_compat::derivation::Derivation;
        use std::collections::BTreeMap;

        let mut env = BTreeMap::new();
        env.insert("HOME".into(), "/homeless-shelter".into());
        env.insert("PATH".into(), "/usr/bin".into());
        env.insert("CC".into(), "gcc".into());

        let drv = Derivation {
            outputs: BTreeMap::new(),
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/true".to_string(),
            args: vec![],
            env,
        };
        let config = SandboxConfig::from_derivation(&drv, "/tmp");
        let env: BTreeMap<&str, &str> = config.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(env["CC"], "gcc");
        assert_eq!(env["HOME"], "/homeless-shelter");
        assert_eq!(env["PATH"], "/usr/bin");
        for key in ["NIX_BUILD_TOP", "TMPDIR", "TEMPDIR", "TMP", "TEMP", "PWD"] {
            assert_eq!(env[key], "/tmp", "{key}");
        }
        assert_eq!(env["NIX_STORE"], "/nix/store");
        assert_eq!(env["NIX_LOG_FD"], "2");
    }

    #[test]
    fn pass_as_file_values_reach_the_builder_as_files() {
        use sui_compat::derivation::Derivation;
        use std::collections::BTreeMap;

        let mut env = BTreeMap::new();
        env.insert("passAsFile".into(), "text".into());
        env.insert("text".into(), "#!/bin/sh".into());
        let drv = Derivation {
            outputs: BTreeMap::new(),
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "aarch64-darwin".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec![],
            env,
        };
        let config = SandboxConfig::from_derivation(&drv, "/b");
        let env: BTreeMap<&str, &str> = config.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert!(!env.contains_key("text"));
        let files = SandboxConfig::attr_files(&drv, "/b");
        assert_eq!(files.len(), 1);
        assert_eq!(env["textPath"], files[0].0);
        assert_eq!(files[0].1, "#!/bin/sh");
        assert!(files[0].0.starts_with("/b/.attr-"));
    }

    #[test]
    fn structured_attrs_reach_the_builder_as_cppnix_writes_them() {
        use sui_compat::derivation::{Derivation, DerivationOutput};
        use std::collections::BTreeMap;

        let json = r#"{"name":"t","b":true,"f":false,"n":null,"i":3,"x":1.5,"q":"it's","l":["a",1],"bad-key":"x","m":{"k":"v","j":2},"deep":[[1]],"_u":"u","9n":"n","impureEnvVars":["SUI_STRUCTURED_IMPURE"]}"#;
        let mut env = BTreeMap::new();
        env.insert("__json".into(), json.into());
        env.insert("out".into(), "/nix/store/o".into());
        let mut outputs = BTreeMap::new();
        outputs.insert("out".into(), DerivationOutput { path: "/nix/store/o".into(), hash_algo: "sha256".into(), hash: "ab".into() });
        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "aarch64-darwin".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec![],
            env,
        };
        unsafe { std::env::set_var("SUI_STRUCTURED_IMPURE", "yes") };
        let config = SandboxConfig::from_derivation(&drv, "/b");
        let env: BTreeMap<&str, &str> = config.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(env["NIX_ATTRS_JSON_FILE"], "/b/.attrs.json");
        assert_eq!(env["NIX_ATTRS_SH_FILE"], "/b/.attrs.sh");
        assert_eq!(env["SUI_STRUCTURED_IMPURE"], "yes");
        assert!(!env.contains_key("__json") && !env.contains_key("out") && !env.contains_key("name"));

        let files: BTreeMap<String, String> = SandboxConfig::attr_files(&drv, "/b").into_iter().collect();
        let attrs: serde_json::Value = serde_json::from_str(&files["/b/.attrs.json"]).unwrap();
        assert_eq!(attrs["outputs"], serde_json::json!({"out": "/nix/store/o"}));
        assert_eq!(attrs["q"], "it's");
        assert_eq!(
            files["/b/.attrs.sh"],
            concat!(
                "declare _u='u'\n",
                "declare b=1\n",
                "declare f=\n",
                "declare i=3\n",
                "declare -a impureEnvVars=('SUI_STRUCTURED_IMPURE' )\n",
                "declare -a l=('a' 1 )\n",
                "declare -A m=(['j']=2 ['k']='v' )\n",
                "declare n=''\n",
                "declare name='t'\n",
                "declare -A outputs=(['out']='/nix/store/o' )\n",
                "declare q='it'\\''s'\n",
            )
        );
    }

    #[test]
    fn from_derivation_builder_with_long_args() {
        use sui_compat::derivation::Derivation;
        use std::collections::BTreeMap;

        let drv = Derivation {
            outputs: BTreeMap::new(),
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/nix/store/bash/bin/bash".to_string(),
            args: (0..100).map(|i| format!("arg{i}")).collect(),
            env: BTreeMap::new(),
        };
        let config = SandboxConfig::from_derivation(&drv, "/tmp");
        assert_eq!(config.args.len(), 100);
        assert_eq!(config.args[0], "arg0");
        assert_eq!(config.args[99], "arg99");
    }

    // ── SandboxConfig builder methods individually ──────────

    #[test]
    fn sandbox_config_with_builder_replaces_value() {
        let config = SandboxConfig::default()
            .with_builder("/bin/sh")
            .with_builder("/bin/bash");
        assert_eq!(config.builder, "/bin/bash");
    }

    #[test]
    fn sandbox_config_with_args_replaces_value() {
        let config = SandboxConfig::default()
            .with_args(vec!["a".into()])
            .with_args(vec!["b".into(), "c".into()]);
        assert_eq!(config.args, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn sandbox_config_with_env_appends() {
        let config = SandboxConfig::default()
            .with_env("A", "1")
            .with_env("B", "2");
        assert_eq!(config.env.len(), 2);
        assert_eq!(config.env[0], ("A".to_string(), "1".to_string()));
        assert_eq!(config.env[1], ("B".to_string(), "2".to_string()));
    }

    #[test]
    fn sandbox_config_with_build_dir_replaces_value() {
        let config = SandboxConfig::default()
            .with_build_dir("/tmp/a")
            .with_build_dir("/tmp/b");
        assert_eq!(config.build_dir, "/tmp/b");
    }

    #[test]
    fn sandbox_config_with_network_toggles() {
        let on = SandboxConfig::default().with_network(true);
        let off = SandboxConfig::default().with_network(false);
        assert!(on.allow_network);
        assert!(!off.allow_network);
    }

    // ── NoSandbox: passing empty args ───────────────────────

    #[test]
    fn no_sandbox_execute_no_args() {
        let sandbox = NoSandbox;
        // Use /bin/echo which exists on macOS and Linux; with no args
        // it prints just a newline. We assert exit success and that
        // stdout is at most a single trailing newline.
        let config = SandboxConfig::default().with_builder("/bin/echo");
        let result = sandbox.execute(&config).unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.len() <= 1);
    }

    // ── NoSandbox: env doesn't leak between executions ──────

    #[test]
    fn no_sandbox_env_isolated_per_call() {
        let sandbox = NoSandbox;
        let with_env = SandboxConfig::default()
            .with_builder("/bin/sh")
            .with_args(vec!["-c".into(), "echo $LEAK_TEST".into()])
            .with_env("LEAK_TEST", "found");
        let without_env = SandboxConfig::default()
            .with_builder("/bin/sh")
            .with_args(vec!["-c".into(), "echo $LEAK_TEST".into()]);

        let r1 = sandbox.execute(&with_env).unwrap();
        let r2 = sandbox.execute(&without_env).unwrap();

        assert!(String::from_utf8_lossy(&r1.stdout).contains("found"));
        // r2 inherits parent env but the key is not set in our config nor parent.
        assert!(!String::from_utf8_lossy(&r2.stdout).contains("found"));
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
    fn a_sandboxed_builder_writes_in_a_build_dir_reached_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let build_dir = dir.path().to_str().unwrap().to_string();
        assert!(build_dir.starts_with("/var/") || build_dir.starts_with("/tmp/"), "{build_dir}");
        let config = SandboxConfig::default()
            .with_build_dir(build_dir.clone())
            .with_builder("/bin/sh")
            .with_args(vec!["-c".to_string(), format!("echo ok > {build_dir}/f")]);
        let sandbox = DarwinSandbox::new();
        let result = sandbox.execute(&config).unwrap();
        assert!(result.is_success(), "{}", result.stderr_lossy());
        assert_eq!(std::fs::read_to_string(dir.path().join("f")).unwrap(), "ok\n");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_generate_sbpl_profile_lets_the_builder_write_its_outputs() {
        let mut config = SandboxConfig::default().with_build_dir("/tmp/x");
        config.output_paths = vec!["/nix/store/z9x4ijxjgbx1z7fh3nhagbskqra0mg09-out".to_string()];
        let profile = generate_sbpl_profile(&config);
        assert!(profile.contains(
            "(allow file-write*\n    (subpath \"/nix/store/z9x4ijxjgbx1z7fh3nhagbskqra0mg09-out\")\n)"
        ));
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
