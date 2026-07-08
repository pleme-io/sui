//! Typed `docker build` invocation + the mockable command-runner seam.
//!
//! Per the fleet's TYPED EMISSION rule, the subprocess argv is built via
//! typed `.arg()` calls on [`DockerBuildInvocation`] — never string
//! concatenation — and every consumer of the invocation (real subprocess,
//! or a test's recording mock) goes through the [`CommandRunner`] trait so
//! the fallback path is provable without ever shelling out in a unit test.

use std::path::Path;
use std::process::Command;

/// One `docker build` (or `docker buildx build`) invocation, built up via
/// typed setters rather than any string template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerBuildInvocation {
    /// The program to exec — `"docker"` by default.
    pub program: String,
    /// Positional + flag arguments, in the order they will be passed to
    /// [`Command::arg`].
    pub args: Vec<String>,
}

impl DockerBuildInvocation {
    /// Build the canonical `docker build -f <dockerfile> -t <tag> --build-arg
    /// K=V ... <context>` invocation.
    #[must_use]
    pub fn build(
        dockerfile_path: &Path,
        context_dir: &Path,
        image_tag: &str,
        build_args: &std::collections::BTreeMap<String, String>,
    ) -> Self {
        let mut args = vec![
            "build".to_string(),
            "-f".to_string(),
            dockerfile_path.display().to_string(),
            "-t".to_string(),
            image_tag.to_string(),
        ];
        for (k, v) in build_args {
            args.push("--build-arg".to_string());
            let mut kv = k.clone();
            kv.push('=');
            kv.push_str(v);
            args.push(kv);
        }
        args.push(context_dir.display().to_string());
        Self { program: "docker".to_string(), args }
    }

    /// Build a `docker pull <image_ref>` invocation — used to materialize a
    /// full cache hit's already-built image rather than re-running `docker
    /// build`.
    #[must_use]
    pub fn pull(image_ref: &str) -> Self {
        Self {
            program: "docker".to_string(),
            args: vec!["pull".to_string(), image_ref.to_string()],
        }
    }

    /// Convert to a real [`Command`], ready to `.output()`.
    #[must_use]
    pub fn to_std_command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        cmd
    }
}

/// The outcome of running a [`DockerBuildInvocation`] — never a panic, a
/// typed exit status + captured output either way.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutcome {
    /// The last `n` bytes of stderr, decoded lossily — used for the typed
    /// `BuildFailed` receipt so a failing build's tail is captured without
    /// unbounded log growth.
    #[must_use]
    pub fn stderr_tail(&self, n: usize) -> String {
        let start = self.stderr.len().saturating_sub(n);
        String::from_utf8_lossy(&self.stderr[start..]).into_owned()
    }
}

/// Errors launching the subprocess itself (not the subprocess's own exit
/// status — that is captured in [`CommandOutcome`]).
#[derive(Debug, thiserror::Error)]
pub enum CommandRunError {
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// The injectable side-effect seam: real runs shell out, tests record the
/// invocation and return a canned outcome.
pub trait CommandRunner {
    /// Run the invocation to completion, returning its typed outcome.
    ///
    /// # Errors
    ///
    /// Returns [`CommandRunError`] only if the process could not be
    /// spawned at all (e.g. the binary is missing) — a non-zero exit
    /// status is a normal (`success: false`) [`CommandOutcome`], not an
    /// error.
    fn run(&self, invocation: &DockerBuildInvocation) -> Result<CommandOutcome, CommandRunError>;
}

/// The production runner — shells out to the real `docker` binary via
/// [`std::process::Command`].
#[derive(Debug, Default, Clone, Copy)]
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, invocation: &DockerBuildInvocation) -> Result<CommandOutcome, CommandRunError> {
        let output = invocation
            .to_std_command()
            .output()
            .map_err(|source| CommandRunError::Spawn { program: invocation.program.clone(), source })?;
        Ok(CommandOutcome {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// A recording mock runner for tests — never spawns a real process. Records
/// every invocation it was asked to run and returns a pre-scripted outcome
/// (defaulting to success with empty output).
#[derive(Debug, Default)]
pub struct MockCommandRunner {
    pub invocations: std::sync::Mutex<Vec<DockerBuildInvocation>>,
    pub scripted_outcome: Option<CommandOutcome>,
}

impl MockCommandRunner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_outcome(outcome: CommandOutcome) -> Self {
        Self { invocations: std::sync::Mutex::new(Vec::new()), scripted_outcome: Some(outcome) }
    }

    /// Snapshot of every invocation recorded so far.
    #[must_use]
    pub fn recorded(&self) -> Vec<DockerBuildInvocation> {
        self.invocations.lock().expect("mock mutex poisoned").clone()
    }
}

impl CommandRunner for MockCommandRunner {
    fn run(&self, invocation: &DockerBuildInvocation) -> Result<CommandOutcome, CommandRunError> {
        self.invocations
            .lock()
            .expect("mock mutex poisoned")
            .push(invocation.clone());
        Ok(self.scripted_outcome.clone().unwrap_or(CommandOutcome {
            success: true,
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }))
    }
}

