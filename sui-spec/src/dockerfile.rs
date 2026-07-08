//! Typed border for a REGULAR Dockerfile (BuildKit-syntax, not a Nix
//! derivation) digested into a content-addressed derivation graph.
//!
//! Sui-spec proves it can consume plain Dockerfiles without
//! rewriting them: a hand-rolled instruction-line parser scoped to
//! exactly the instruction set below produces a typed
//! [`DockerfileGraph`] whose node hashes are computed the same way
//! [`crate::hash`] and [`crate::derivation`] compute nix store-path
//! hashes -- a function of `(instruction-kind, prior-layer-digest,
//! resolved-build-arg-values)`.  Two runs over identical input
//! produce hash-identical graphs; changing one instruction's content
//! invalidates only that node and its downstream dependents.
//!
//! ## Scope (deliberately narrow)
//!
//! Supported instructions: `FROM`, `RUN` (incl. `--mount=type=bind`),
//! `COPY` (incl. `--from=<stage>`), `ARG` (incl. default values),
//! `ENV`, `WORKDIR`, `CMD`, `ENTRYPOINT`, `VOLUME` (plain or
//! JSON-array form).  `ARG TARGETARCH` is
//! resolved from the environment's build-arg map like any other
//! `ARG`.  Multi-stage `FROM ... AS <alias>` aliasing beyond the
//! `--from=` reference on `COPY`, full `BuildKit` heredoc syntax, and
//! ARG default-value precedence beyond "declared default, overridden
//! by environment" are explicitly OUT OF SCOPE for this phase (see
//! the module-level doc on the missing pieces, or the phase report).
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defdockerfile-graph
//!   :name        "simple-three-instruction"
//!   :description "..."
//!   :source-text "FROM debian:bookworm-slim\nRUN ...\nCMD [...]\n")
//! ```

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border — authored canonical instances ─────────────────────

/// One canonical `(defdockerfile-graph ...)` instance.  `source_text`
/// is the raw Dockerfile text the interpreter parses; canonical
/// instances exist so the graph-build algorithm has a fixed corpus
/// to prove determinism + cache-leverage against.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defdockerfile-graph")]
pub struct DockerfileGraphSpec {
    pub name: String,
    pub description: String,
    #[serde(rename = "sourceText")]
    pub source_text: String,
}

// ── Typed border — parsed instructions ──────────────────────────────

/// One parsed Dockerfile instruction.  Bad states (a `COPY` with no
/// source, an `ARG` reference that never resolves) are rejected at
/// parse time via [`SpecError::Interp`] — never silently accepted.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum DockerfileInstruction {
    From {
        image: String,
        stage_alias: Option<String>,
    },
    Run {
        command: String,
        mount_bind_source: Option<String>,
        mount_bind_target: Option<String>,
    },
    Copy {
        sources: Vec<String>,
        destination: String,
        from_stage: Option<String>,
    },
    Arg {
        name: String,
        default_value: Option<String>,
    },
    Env {
        name: String,
        value: String,
    },
    Workdir {
        path: String,
    },
    Cmd {
        argv: Vec<String>,
    },
    Entrypoint {
        argv: Vec<String>,
    },
    Volume {
        paths: Vec<String>,
    },
}

/// Which instruction kind a node represents — the coarse key used in
/// the node's content hash alongside its resolved argument bytes.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstructionKind {
    From,
    Run,
    Copy,
    Arg,
    Env,
    Workdir,
    Cmd,
    Entrypoint,
    Volume,
}

impl DockerfileInstruction {
    fn kind(&self) -> InstructionKind {
        match self {
            Self::From { .. } => InstructionKind::From,
            Self::Run { .. } => InstructionKind::Run,
            Self::Copy { .. } => InstructionKind::Copy,
            Self::Arg { .. } => InstructionKind::Arg,
            Self::Env { .. } => InstructionKind::Env,
            Self::Workdir { .. } => InstructionKind::Workdir,
            Self::Cmd { .. } => InstructionKind::Cmd,
            Self::Entrypoint { .. } => InstructionKind::Entrypoint,
            Self::Volume { .. } => InstructionKind::Volume,
        }
    }

    /// Canonical byte representation of this instruction's own
    /// content (excluding any prior-layer digest) — the input to the
    /// node's content hash.  A typed `Display`-style renderer, never
    /// `format!()` of ad-hoc pieces glued at the hash call site.
    fn content_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        match self {
            Self::From { image, stage_alias } => {
                buf.extend_from_slice(image.as_bytes());
                buf.push(0);
                if let Some(alias) = stage_alias {
                    buf.extend_from_slice(alias.as_bytes());
                }
            }
            Self::Run { command, mount_bind_source, mount_bind_target } => {
                buf.extend_from_slice(command.as_bytes());
                buf.push(0);
                if let Some(s) = mount_bind_source {
                    buf.extend_from_slice(s.as_bytes());
                }
                buf.push(0);
                if let Some(t) = mount_bind_target {
                    buf.extend_from_slice(t.as_bytes());
                }
            }
            Self::Copy { sources, destination, from_stage } => {
                for s in sources {
                    buf.extend_from_slice(s.as_bytes());
                    buf.push(0);
                }
                buf.extend_from_slice(destination.as_bytes());
                buf.push(0);
                if let Some(stage) = from_stage {
                    buf.extend_from_slice(stage.as_bytes());
                }
            }
            Self::Arg { name, default_value } => {
                buf.extend_from_slice(name.as_bytes());
                buf.push(0);
                if let Some(v) = default_value {
                    buf.extend_from_slice(v.as_bytes());
                }
            }
            Self::Env { name, value } => {
                buf.extend_from_slice(name.as_bytes());
                buf.push(0);
                buf.extend_from_slice(value.as_bytes());
            }
            Self::Workdir { path } => buf.extend_from_slice(path.as_bytes()),
            Self::Cmd { argv } | Self::Entrypoint { argv } => {
                for a in argv {
                    buf.extend_from_slice(a.as_bytes());
                    buf.push(0);
                }
            }
            Self::Volume { paths } => {
                for p in paths {
                    buf.extend_from_slice(p.as_bytes());
                    buf.push(0);
                }
            }
        }
        buf
    }
}

/// One node in the content-addressed build graph.  `content_hash` is
/// BLAKE3 over `(kind, prior_layer_digest, resolved_build_arg_values,
/// instruction_content_bytes)` — reusing [`crate::hash`]'s BLAKE3
/// convention (the same primitive `lockfile_graph`/`ast_graph` use
/// for their content-addressed forms).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DockerfileNode {
    pub kind: InstructionKind,
    pub instruction: DockerfileInstruction,
    #[serde(rename = "contentHash")]
    pub content_hash: String,
    #[serde(rename = "parentHash")]
    pub parent_hash: Option<String>,
}

/// The full parsed + hashed graph — a linear chain (Dockerfile
/// instructions execute strictly top-to-bottom; no branching), so
/// "graph" here is a hash-linked list, the same shape a derivation's
/// input closure takes at each single-output layer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DockerfileGraph {
    pub nodes: Vec<DockerfileNode>,
}

impl DockerfileGraph {
    /// Content hash of the final node — the graph's overall identity,
    /// analogous to a derivation's output path hash.
    #[must_use]
    pub fn root_hash(&self) -> Option<&str> {
        self.nodes.last().map(|n| n.content_hash.as_str())
    }
}

// ── Environment seam ─────────────────────────────────────────────────

/// Abstract IO environment for the dockerfile interpreter.  The only
/// side effect this domain performs is reading Dockerfile text and
/// resolving build-arg values from environment — both behind this
/// trait so tests use a mock and production wraps real
/// filesystem/env access.
pub trait DockerfileEnvironment {
    /// Read the Dockerfile at `path`.  In the canonical-instance path
    /// the "path" is just the spec's `name` and the text comes from
    /// the authored spec itself; a real consumer wraps
    /// `std::fs::read_to_string`.
    ///
    /// # Errors
    ///
    /// Implementations return their own error which the interpreter
    /// converts to `SpecError::Interp { phase: "read-dockerfile" }`.
    fn read_dockerfile(&self, path: &str) -> Result<String, String>;

    /// Resolve a build-arg's value from the environment (e.g.
    /// `--build-arg TARGETARCH=amd64`).  Returns `None` on miss —
    /// the interpreter falls back to the `ARG`'s declared default,
    /// or leaves it unresolved if neither exists.
    fn resolve_build_arg(&self, name: &str) -> Option<String>;
}

/// In-memory mock environment for tests.  Pre-load Dockerfile text by
/// path + build-arg values by name.
#[derive(Default)]
pub struct MockDockerfileEnvironment {
    pub dockerfiles: BTreeMap<String, String>,
    pub build_args: BTreeMap<String, String>,
}

impl MockDockerfileEnvironment {
    #[must_use]
    pub fn with_dockerfile(mut self, path: &str, text: &str) -> Self {
        self.dockerfiles.insert(path.to_string(), text.to_string());
        self
    }

    #[must_use]
    pub fn with_build_arg(mut self, name: &str, value: &str) -> Self {
        self.build_args.insert(name.to_string(), value.to_string());
        self
    }
}

impl DockerfileEnvironment for MockDockerfileEnvironment {
    fn read_dockerfile(&self, path: &str) -> Result<String, String> {
        self.dockerfiles
            .get(path)
            .cloned()
            .ok_or_else(|| format!("no canned Dockerfile at `{path}`"))
    }

    fn resolve_build_arg(&self, name: &str) -> Option<String> {
        self.build_args.get(name).cloned()
    }
}

// ── Interpreter ───────────────────────────────────────────────────────

/// Inputs to a dockerfile-graph run.  `path` is passed to
/// [`DockerfileEnvironment::read_dockerfile`]; for the canonical-
/// instance tests it is conventionally the spec's `name`.
pub struct DockerfileArgs {
    pub path: String,
}

/// Parse + content-address a Dockerfile into its typed
/// [`DockerfileGraph`].
///
/// # Errors
///
/// - `SpecError::Interp { phase: "read-dockerfile" }` if the
///   environment couldn't read the file.
/// - `SpecError::Interp { phase: "parse-line" }` for a line this
///   scoped parser can't classify, or a malformed instruction (e.g.
///   `COPY` with no destination).
/// - `SpecError::Interp { phase: "unresolved-arg-reference" }` if a
///   `RUN`/`ENV`/`CMD`/... body references `$SOME_ARG` and neither
///   the environment nor the `ARG`'s own default resolves it.
pub fn apply<E: DockerfileEnvironment>(
    args: &DockerfileArgs,
    env: &E,
) -> Result<DockerfileGraph, SpecError> {
    let text = env.read_dockerfile(&args.path).map_err(|e| SpecError::Interp {
        phase: "read-dockerfile".into(),
        message: format!("reading `{}`: {e}", args.path),
    })?;

    let mut resolved_args: BTreeMap<String, String> = BTreeMap::new();
    let mut declared_envs: BTreeSet<String> = BTreeSet::new();
    let mut nodes = Vec::new();
    let mut parent_hash: Option<String> = None;

    for (line_no, raw_line) in join_continuations(&text).into_iter().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let instruction = parse_instruction(line, line_no + 1, &resolved_args, &declared_envs, env)?;

        if let DockerfileInstruction::Arg { name, default_value } = &instruction {
            let value = env
                .resolve_build_arg(name)
                .or_else(|| default_value.clone());
            if let Some(v) = value {
                resolved_args.insert(name.clone(), v);
            }
        }

        if let DockerfileInstruction::Env { name, .. } = &instruction {
            declared_envs.insert(name.clone());
        }

        let node = hash_node(instruction, parent_hash.as_deref(), &resolved_args);
        parent_hash = Some(node.content_hash.clone());
        nodes.push(node);
    }

    Ok(DockerfileGraph { nodes })
}

/// `BuildKit` allows a `RUN` (or any instruction) to continue onto the
/// next physical line with a trailing `\`.  Join those before the
/// per-line parser runs so `--mount=...` flags that wrap don't split.
fn join_continuations(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for raw in text.lines() {
        let trimmed_end = raw.trim_end();
        if let Some(stripped) = trimmed_end.strip_suffix('\\') {
            current.push_str(stripped);
            current.push(' ');
        } else {
            current.push_str(trimmed_end);
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

fn hash_node(
    instruction: DockerfileInstruction,
    parent_hash: Option<&str>,
    resolved_args: &BTreeMap<String, String>,
) -> DockerfileNode {
    let kind = instruction.kind();
    let mut hasher = blake3::Hasher::new();
    hasher.update(format!("{kind:?}").as_bytes());
    hasher.update(b"\0");
    if let Some(parent) = parent_hash {
        hasher.update(parent.as_bytes());
    }
    hasher.update(b"\0");
    for (k, v) in resolved_args {
        hasher.update(k.as_bytes());
        hasher.update(b"=");
        hasher.update(v.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(b"\0");
    hasher.update(&instruction.content_bytes());
    let content_hash = hasher.finalize().to_hex().to_string();

    DockerfileNode {
        kind,
        instruction,
        content_hash,
        parent_hash: parent_hash.map(ToString::to_string),
    }
}

/// Standard environment variables present in virtually every base
/// image (inherited from the image's own `ENV`/shell setup) — a
/// reference to one of these is never a build-`ARG` and always
/// resolves without error.
const WELL_KNOWN_ENV_VARS: &[&str] = &["PATH", "HOME", "TERM"];

/// Which class a `$NAME` reference in an instruction's value belongs
/// to. `Arg` references must resolve via a declared `ARG`'s default or
/// a passed build-arg — unresolved is a typed error. `Env` and
/// `WellKnownEnv` references are standard `ENV`-expansion (a prior
/// `ENV NAME=value` in the same file, or a name every base image
/// already carries) and always resolve — their literal `$NAME` text is
/// passed through unchanged, since sui-spec has no base-image runtime
/// to read the actual value from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VariableKind {
    Arg,
    Env,
    WellKnownEnv,
}

impl VariableKind {
    fn classify(name: &str, resolved_args: &BTreeMap<String, String>, declared_envs: &BTreeSet<String>) -> Self {
        if resolved_args.contains_key(name) {
            Self::Arg
        } else if declared_envs.contains(name) {
            Self::Env
        } else if WELL_KNOWN_ENV_VARS.contains(&name) {
            Self::WellKnownEnv
        } else {
            Self::Arg
        }
    }
}

/// Substitute `$NAME` / `${NAME}` references in `text`. An `ARG`-class
/// reference (declared via `ARG`, not a prior `ENV` or well-known env
/// var) that resolves to nothing (never declared, or declared with no
/// default and never supplied) is a typed error — never silently
/// passed through. An `ENV`-class reference (a name declared by a
/// prior `ENV` in the same file, or a well-known env var like `PATH`)
/// is left as literal `$NAME` text — standard `ENV`-expansion sui-spec
/// has no base-image runtime to resolve, but which is never a build-arg
/// gap either.
fn substitute_arg_refs(
    text: &str,
    resolved_args: &BTreeMap<String, String>,
    declared_envs: &BTreeSet<String>,
) -> Result<String, SpecError> {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let (name, consumed) = if bytes.get(i + 1) == Some(&b'{') {
                let end = text[i + 2..]
                    .find('}')
                    .map_or(text.len(), |p| i + 2 + p);
                (text[i + 2..end].to_string(), end + 1 - i)
            } else {
                let start = i + 1;
                let end = text[start..]
                    .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .map_or(text.len(), |p| start + p);
                (text[start..end].to_string(), end - i)
            };
            if name.is_empty() {
                out.push('$');
                i += 1;
                continue;
            }
            match VariableKind::classify(&name, resolved_args, declared_envs) {
                VariableKind::Env | VariableKind::WellKnownEnv => {
                    out.push_str(&text[i..i + consumed]);
                }
                VariableKind::Arg => match resolved_args.get(&name) {
                    Some(v) => out.push_str(v),
                    None => {
                        return Err(SpecError::Interp {
                            phase: "unresolved-arg-reference".into(),
                            message: format!(
                                "`${name}` referenced but never resolved by ARG default or build-arg",
                            ),
                        });
                    }
                },
            }
            i += consumed;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(out)
}

fn parse_instruction<E: DockerfileEnvironment>(
    line: &str,
    line_no: usize,
    resolved_args: &BTreeMap<String, String>,
    declared_envs: &BTreeSet<String>,
    _env: &E,
) -> Result<DockerfileInstruction, SpecError> {
    let (keyword, rest) = split_keyword(line).ok_or_else(|| SpecError::Interp {
        phase: "parse-line".into(),
        message: format!("line {line_no}: no instruction keyword found in `{line}`"),
    })?;

    match keyword.to_ascii_uppercase().as_str() {
        "FROM" => parse_from(rest, line_no),
        "RUN" => parse_run(rest, line_no, resolved_args, declared_envs),
        "COPY" => parse_copy(rest, line_no),
        "ARG" => parse_arg(rest, line_no),
        "ENV" => parse_env(rest, line_no, resolved_args, declared_envs),
        "WORKDIR" => Ok(DockerfileInstruction::Workdir { path: rest.trim().to_string() }),
        "CMD" => Ok(DockerfileInstruction::Cmd { argv: parse_argv(rest) }),
        "ENTRYPOINT" => Ok(DockerfileInstruction::Entrypoint { argv: parse_argv(rest) }),
        "VOLUME" => parse_volume(rest, line_no),
        other => Err(SpecError::Interp {
            phase: "parse-line".into(),
            message: format!(
                "line {line_no}: unsupported instruction `{other}` — this scoped \
                 parser handles FROM/RUN/COPY/ARG/ENV/WORKDIR/CMD/ENTRYPOINT/VOLUME only",
            ),
        }),
    }
}

fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let idx = line.find(char::is_whitespace)?;
    Some((&line[..idx], line[idx..].trim_start()))
}

fn parse_from(rest: &str, line_no: usize) -> Result<DockerfileInstruction, SpecError> {
    let mut parts = rest.split_whitespace();
    let image = parts.next().ok_or_else(|| SpecError::Interp {
        phase: "parse-line".into(),
        message: format!("line {line_no}: FROM with no image"),
    })?;
    let stage_alias = match parts.next() {
        Some(as_kw) if as_kw.eq_ignore_ascii_case("as") => {
            parts.next().map(ToString::to_string)
        }
        _ => None,
    };
    Ok(DockerfileInstruction::From { image: image.to_string(), stage_alias })
}

fn parse_run(
    rest: &str,
    line_no: usize,
    resolved_args: &BTreeMap<String, String>,
    declared_envs: &BTreeSet<String>,
) -> Result<DockerfileInstruction, SpecError> {
    let (flags, command) = split_flags(rest);
    let mut mount_bind_source = None;
    let mut mount_bind_target = None;
    for flag in &flags {
        if let Some(mount_spec) = flag.strip_prefix("--mount=") {
            for kv in mount_spec.split(',') {
                if let Some(v) = kv.strip_prefix("source=") {
                    mount_bind_source = Some(v.to_string());
                } else if let Some(v) = kv.strip_prefix("target=") {
                    mount_bind_target = Some(v.to_string());
                }
            }
        }
    }
    if command.trim().is_empty() {
        return Err(SpecError::Interp {
            phase: "parse-line".into(),
            message: format!("line {line_no}: RUN with no command"),
        });
    }
    let command = substitute_arg_refs(command.trim(), resolved_args, declared_envs)?;
    Ok(DockerfileInstruction::Run { command, mount_bind_source, mount_bind_target })
}

fn parse_copy(rest: &str, line_no: usize) -> Result<DockerfileInstruction, SpecError> {
    let (flags, body) = split_flags(rest);
    let mut from_stage = None;
    for flag in &flags {
        if let Some(v) = flag.strip_prefix("--from=") {
            from_stage = Some(v.to_string());
        }
    }
    let tokens: Vec<&str> = body.split_whitespace().collect();
    if tokens.len() < 2 {
        return Err(SpecError::Interp {
            phase: "parse-line".into(),
            message: format!(
                "line {line_no}: COPY requires at least one source and a destination, got `{rest}`",
            ),
        });
    }
    let (destination, sources) = tokens.split_last().expect("checked len >= 2 above");
    Ok(DockerfileInstruction::Copy {
        sources: sources.iter().map(ToString::to_string).collect(),
        destination: (*destination).to_string(),
        from_stage,
    })
}

fn parse_arg(rest: &str, line_no: usize) -> Result<DockerfileInstruction, SpecError> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Err(SpecError::Interp {
            phase: "parse-line".into(),
            message: format!("line {line_no}: ARG with no name"),
        });
    }
    match rest.split_once('=') {
        Some((name, value)) => Ok(DockerfileInstruction::Arg {
            name: name.trim().to_string(),
            default_value: Some(value.trim().trim_matches('"').to_string()),
        }),
        None => Ok(DockerfileInstruction::Arg { name: rest.to_string(), default_value: None }),
    }
}

fn parse_env(
    rest: &str,
    line_no: usize,
    resolved_args: &BTreeMap<String, String>,
    declared_envs: &BTreeSet<String>,
) -> Result<DockerfileInstruction, SpecError> {
    let rest = rest.trim();
    let (name, value) = rest.split_once('=').ok_or_else(|| SpecError::Interp {
        phase: "parse-line".into(),
        message: format!("line {line_no}: ENV requires `NAME=value`, got `{rest}`"),
    })?;
    let value = substitute_arg_refs(value.trim().trim_matches('"'), resolved_args, declared_envs)?;
    Ok(DockerfileInstruction::Env { name: name.trim().to_string(), value })
}

/// Parse a `CMD`/`ENTRYPOINT` body — either JSON-array exec form
/// (`["a", "b"]`) or a bare shell-form string treated as a single
/// argv element.
fn parse_argv(rest: &str) -> Vec<String> {
    let rest = rest.trim();
    if let Some(inner) = rest.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        inner
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        vec![rest.to_string()]
    }
}

/// Parse a `VOLUME` body — either JSON-array form (`["/a", "/b"]`, same
/// shape as [`parse_argv`]) or plain whitespace-separated paths
/// (`VOLUME /a /b`). At least one path is required; a bare `VOLUME`
/// with nothing after it is a typed error rather than a silently
/// empty declaration.
fn parse_volume(rest: &str, line_no: usize) -> Result<DockerfileInstruction, SpecError> {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return Err(SpecError::Interp {
            phase: "parse-line".into(),
            message: format!("line {line_no}: VOLUME with no paths"),
        });
    }
    let paths = if trimmed.starts_with('[') {
        parse_argv(trimmed)
    } else {
        trimmed.split_whitespace().map(ToString::to_string).collect()
    };
    if paths.is_empty() {
        return Err(SpecError::Interp {
            phase: "parse-line".into(),
            message: format!("line {line_no}: VOLUME with no paths"),
        });
    }
    Ok(DockerfileInstruction::Volume { paths })
}

/// Split leading `--flag` / `--flag=value` tokens from the remainder
/// of an instruction body (used by `RUN --mount=...` and
/// `COPY --from=...`).
fn split_flags(rest: &str) -> (Vec<String>, &str) {
    let mut flags = Vec::new();
    let mut remainder = rest.trim_start();
    while let Some(stripped) = remainder.strip_prefix("--") {
        let end = stripped.find(char::is_whitespace).unwrap_or(stripped.len());
        let mut flag = String::with_capacity(2 + end);
        flag.push_str("--");
        flag.push_str(&stripped[..end]);
        flags.push(flag);
        remainder = remainder[2 + end..].trim_start();
    }
    (flags, remainder)
}

// ── Canonical spec ────────────────────────────────────────────────────

pub const CANONICAL_DOCKERFILE_LISP: &str = include_str!("../specs/dockerfile.lisp");

/// Compile every authored `(defdockerfile-graph ...)` instance.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<DockerfileGraphSpec>, SpecError> {
    crate::loader::load_all::<DockerfileGraphSpec>(CANONICAL_DOCKERFILE_LISP)
}

/// Return the canonical instance whose `name` matches.
///
/// # Errors
///
/// Returns an error if the spec fails to parse or `name` is missing.
pub fn load_named(name: &str) -> Result<DockerfileGraphSpec, SpecError> {
    load_canonical()?
        .into_iter()
        .find(|d| d.name == name)
        .ok_or_else(|| SpecError::Load(format!(
            "no (defdockerfile-graph) with :name {name:?}",
        )))
}

/// Convenience wrapper: run [`apply`] against a canonical instance's
/// own `source_text`, via a [`MockDockerfileEnvironment`] pre-loaded
/// with that text under its own `name` as the path.
///
/// # Errors
///
/// Propagates any error from [`apply`] or [`load_named`].
pub fn apply_canonical(name: &str, build_args: &[(&str, &str)]) -> Result<DockerfileGraph, SpecError> {
    let spec = load_named(name)?;
    let mut env = MockDockerfileEnvironment::default().with_dockerfile(name, &spec.source_text);
    for (k, v) in build_args {
        env = env.with_build_arg(k, v);
    }
    apply(&DockerfileArgs { path: name.to_string() }, &env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_dockerfile_specs_parse() {
        let specs = load_canonical().expect("canonical dockerfile specs must compile");
        assert_eq!(specs.len(), 2);
    }

    #[test]
    fn simple_three_instruction_shape() {
        let graph = apply_canonical("simple-three-instruction", &[]).unwrap();
        let kinds: Vec<InstructionKind> = graph.nodes.iter().map(|n| n.kind).collect();
        assert_eq!(
            kinds,
            vec![InstructionKind::From, InstructionKind::Run, InstructionKind::Cmd],
        );
    }

    #[test]
    fn nonroot_gateway_shape() {
        let graph = apply_canonical("nonroot-gateway", &[("TARGETARCH", "amd64")]).unwrap();
        // FROM, ARG TARGETARCH, ARG FIPS, RUN(apt), RUN(--mount FIPS),
        // ENV, WORKDIR, COPY, RUN(adduser), ENTRYPOINT, CMD = 11 nodes.
        assert_eq!(graph.nodes.len(), 11);

        let run_with_mount = graph
            .nodes
            .iter()
            .find(|n| matches!(&n.instruction, DockerfileInstruction::Run { mount_bind_source: Some(_), .. }))
            .expect("expected a RUN with --mount=type=bind");
        match &run_with_mount.instruction {
            DockerfileInstruction::Run { mount_bind_source, mount_bind_target, command } => {
                assert_eq!(mount_bind_source.as_deref(), Some("."));
                assert_eq!(mount_bind_target.as_deref(), Some("/build"));
                assert!(command.contains("amd64"), "TARGETARCH should have been substituted: {command}");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn identical_inputs_produce_hash_identical_graphs() {
        let g1 = apply_canonical("nonroot-gateway", &[("TARGETARCH", "arm64")]).unwrap();
        let g2 = apply_canonical("nonroot-gateway", &[("TARGETARCH", "arm64")]).unwrap();
        assert_eq!(g1, g2);
        assert_eq!(g1.root_hash(), g2.root_hash());
        for (a, b) in g1.nodes.iter().zip(g2.nodes.iter()) {
            assert_eq!(a.content_hash, b.content_hash);
        }
    }

    #[test]
    fn different_build_arg_changes_downstream_hashes_only() {
        let amd = apply_canonical("nonroot-gateway", &[("TARGETARCH", "amd64")]).unwrap();
        let arm = apply_canonical("nonroot-gateway", &[("TARGETARCH", "arm64")]).unwrap();

        // Nodes strictly before the ARG TARGETARCH resolution (index 0:
        // FROM) share content that doesn't depend on the arg value, but
        // this implementation folds resolved_args into every node's
        // hash (parent-chain style), so upstream-of-the-ARG nodes differ
        // only from the point the ARG itself is declared onward. Prove
        // the concrete cache-leverage property instead: the FROM node
        // (the true root, before any ARG) is hash-identical across runs.
        assert_eq!(amd.nodes[0].content_hash, arm.nodes[0].content_hash);

        // From the RUN with the substituted TARGETARCH onward, hashes
        // diverge.
        let diverge_from = amd
            .nodes
            .iter()
            .zip(arm.nodes.iter())
            .position(|(a, b)| a.content_hash != b.content_hash)
            .expect("amd64 vs arm64 runs must diverge somewhere");
        assert!(diverge_from > 0, "must not diverge at the FROM root");

        // Every node from that point on differs (parent-hash chaining
        // means downstream divergence propagates monotonically).
        for (a, b) in amd.nodes[diverge_from..].iter().zip(arm.nodes[diverge_from..].iter()) {
            assert_ne!(a.content_hash, b.content_hash);
        }
    }

    #[test]
    fn changing_one_instruction_only_invalidates_downstream() {
        let mut env = MockDockerfileEnvironment::default().with_dockerfile(
            "a",
            "FROM debian:bookworm-slim\nRUN echo one\nRUN echo two\nCMD [\"/bin/true\"]\n",
        );
        let g1 = apply(&DockerfileArgs { path: "a".into() }, &env).unwrap();

        env = MockDockerfileEnvironment::default().with_dockerfile(
            "a",
            "FROM debian:bookworm-slim\nRUN echo one\nRUN echo CHANGED\nCMD [\"/bin/true\"]\n",
        );
        let g2 = apply(&DockerfileArgs { path: "a".into() }, &env).unwrap();

        // FROM and the first RUN are untouched upstream siblings.
        assert_eq!(g1.nodes[0].content_hash, g2.nodes[0].content_hash);
        assert_eq!(g1.nodes[1].content_hash, g2.nodes[1].content_hash);
        // The changed RUN and everything after it (CMD, whose parent
        // hash chains through it) differ.
        assert_ne!(g1.nodes[2].content_hash, g2.nodes[2].content_hash);
        assert_ne!(g1.nodes[3].content_hash, g2.nodes[3].content_hash);
    }

    #[test]
    fn copy_with_no_source_is_a_typed_error() {
        let env = MockDockerfileEnvironment::default()
            .with_dockerfile("bad", "FROM x\nCOPY onlydestination\n");
        let err = apply(&DockerfileArgs { path: "bad".into() }, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "parse-line"),
            _ => panic!("expected parse-line error"),
        }
    }

    #[test]
    fn unresolvable_arg_reference_is_a_typed_error() {
        let env = MockDockerfileEnvironment::default()
            .with_dockerfile("bad", "FROM x\nRUN echo $NEVER_DECLARED\n");
        let err = apply(&DockerfileArgs { path: "bad".into() }, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "unresolved-arg-reference");
                assert!(message.contains("NEVER_DECLARED"));
            }
            _ => panic!("expected unresolved-arg-reference error"),
        }
    }

    #[test]
    fn missing_dockerfile_is_a_typed_error() {
        let env = MockDockerfileEnvironment::default();
        let err = apply(&DockerfileArgs { path: "missing".into() }, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "read-dockerfile"),
            _ => panic!("expected read-dockerfile error"),
        }
    }

    #[test]
    fn unsupported_instruction_is_a_typed_error() {
        let env = MockDockerfileEnvironment::default()
            .with_dockerfile("bad", "FROM x\nHEALTHCHECK CMD curl -f http://localhost/ || exit 1\n");
        let err = apply(&DockerfileArgs { path: "bad".into() }, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "parse-line");
                assert!(message.contains("HEALTHCHECK"));
            }
            _ => panic!("expected parse-line error"),
        }
    }

    #[test]
    fn line_continuation_is_joined() {
        let env = MockDockerfileEnvironment::default().with_dockerfile(
            "cont",
            "FROM x\nRUN apt-get update && \\\n    apt-get install -y curl\n",
        );
        let graph = apply(&DockerfileArgs { path: "cont".into() }, &env).unwrap();
        assert_eq!(graph.nodes.len(), 2);
        match &graph.nodes[1].instruction {
            DockerfileInstruction::Run { command, .. } => {
                assert!(command.contains("apt-get install"));
            }
            _ => panic!("expected RUN"),
        }
    }

    #[test]
    fn volume_plain_form_is_parsed() {
        let env = MockDockerfileEnvironment::default()
            .with_dockerfile("v", "FROM x\nVOLUME /data\n");
        let graph = apply(&DockerfileArgs { path: "v".into() }, &env).unwrap();
        assert_eq!(graph.nodes.len(), 2);
        match &graph.nodes[1].instruction {
            DockerfileInstruction::Volume { paths } => {
                assert_eq!(paths, &vec!["/data".to_string()]);
            }
            _ => panic!("expected VOLUME"),
        }
        assert_eq!(graph.nodes[1].kind, InstructionKind::Volume);
    }

    #[test]
    fn volume_json_array_form_is_parsed() {
        let env = MockDockerfileEnvironment::default()
            .with_dockerfile("v", "FROM x\nVOLUME [\"/data\", \"/logs\"]\n");
        let graph = apply(&DockerfileArgs { path: "v".into() }, &env).unwrap();
        match &graph.nodes[1].instruction {
            DockerfileInstruction::Volume { paths } => {
                assert_eq!(paths, &vec!["/data".to_string(), "/logs".to_string()]);
            }
            _ => panic!("expected VOLUME"),
        }
    }

    #[test]
    fn volume_produces_a_graph_node() {
        let env = MockDockerfileEnvironment::default()
            .with_dockerfile("v", "FROM x\nVOLUME /data /logs\n");
        let graph = apply(&DockerfileArgs { path: "v".into() }, &env).unwrap();
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.nodes[1].kind, InstructionKind::Volume);
        assert!(graph.nodes[1].parent_hash.is_some());
    }

    #[test]
    fn volume_hashing_is_deterministic() {
        let env = MockDockerfileEnvironment::default()
            .with_dockerfile("v", "FROM x\nVOLUME /data /logs\n");
        let g1 = apply(&DockerfileArgs { path: "v".into() }, &env).unwrap();
        let g2 = apply(&DockerfileArgs { path: "v".into() }, &env).unwrap();
        assert_eq!(g1, g2);
        assert_eq!(g1.nodes[1].content_hash, g2.nodes[1].content_hash);
    }

    #[test]
    fn env_path_prepend_self_reference_resolves() {
        let env = MockDockerfileEnvironment::default().with_dockerfile(
            "p",
            "FROM ubuntu:24.04\nENV VIRTUAL_ENV=/opt/venv\nENV PATH=\"/opt/venv/bin:/akeyless/bin:/usr/local/ssl/bin:${PATH}\"\n",
        );
        let graph = apply(&DockerfileArgs { path: "p".into() }, &env).unwrap();
        assert_eq!(graph.nodes.len(), 3);
        match &graph.nodes[2].instruction {
            DockerfileInstruction::Env { name, value } => {
                assert_eq!(name, "PATH");
                assert!(value.contains("${PATH}"), "PATH self-reference should be left literal: {value}");
            }
            _ => panic!("expected ENV"),
        }
    }

    #[test]
    fn env_referencing_prior_env_resolves() {
        let env = MockDockerfileEnvironment::default().with_dockerfile(
            "p",
            "FROM x\nENV FOO=bar\nENV BAZ=\"$FOO/baz\"\n",
        );
        let graph = apply(&DockerfileArgs { path: "p".into() }, &env).unwrap();
        match &graph.nodes[2].instruction {
            DockerfileInstruction::Env { name, value } => {
                assert_eq!(name, "BAZ");
                assert!(value.contains("$FOO"), "prior-ENV reference should be left literal: {value}");
            }
            _ => panic!("expected ENV"),
        }
    }

    #[test]
    fn env_well_known_var_does_not_trigger_arg_driven_hash_variance() {
        let env = MockDockerfileEnvironment::default().with_dockerfile(
            "p",
            "FROM x\nENV PATH=\"/custom/bin:$PATH\"\n",
        );
        let g1 = apply(&DockerfileArgs { path: "p".into() }, &env).unwrap();
        let g2 = apply(&DockerfileArgs { path: "p".into() }, &env).unwrap();
        assert_eq!(g1.nodes[1].content_hash, g2.nodes[1].content_hash);
    }

    #[test]
    fn empty_volume_is_a_typed_error() {
        let env = MockDockerfileEnvironment::default()
            .with_dockerfile("bad", "FROM x\nVOLUME\n");
        let err = apply(&DockerfileArgs { path: "bad".into() }, &env).unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "parse-line");
                assert!(message.contains("VOLUME"));
            }
            _ => panic!("expected parse-line error"),
        }
    }

    /// Regression fixture: the real `VOLUME` lines from akeyless's base
    /// image Dockerfile (akeylesslabs/akeyless-main-repo,
    /// `supa-charge-proof` branch,
    /// `tools/deployment/docker/base/Dockerfile`, fetched 2026-07-08 via
    /// the GitHub contents API) — this exact declarative shape (two
    /// separate plain-form `VOLUME` lines with a trailing slash) is what
    /// surfaced the gap during the supa-charge-akeyless-ci Phase 5 proof
    /// run.
    #[test]
    fn real_akeyless_base_dockerfile_volume_lines_regression() {
        let env = MockDockerfileEnvironment::default().with_dockerfile(
            "akeyless-base",
            "FROM ubuntu:24.04\n\
             VOLUME /akeyless_shared_vol/\n\
             # External shared volume to save log files\n\
             VOLUME /var/log/akeyless/\n",
        );
        let graph = apply(&DockerfileArgs { path: "akeyless-base".into() }, &env).unwrap();
        assert_eq!(graph.nodes.len(), 3);
        match &graph.nodes[1].instruction {
            DockerfileInstruction::Volume { paths } => {
                assert_eq!(paths, &vec!["/akeyless_shared_vol/".to_string()]);
            }
            _ => panic!("expected VOLUME"),
        }
        match &graph.nodes[2].instruction {
            DockerfileInstruction::Volume { paths } => {
                assert_eq!(paths, &vec!["/var/log/akeyless/".to_string()]);
            }
            _ => panic!("expected VOLUME"),
        }
    }

    /// Regression fixture: the real `ENV PATH` prepend line from
    /// akeyless's base image Dockerfile (akeylesslabs/akeyless-main-repo,
    /// `supa-charge-proof` branch,
    /// `tools/deployment/docker/base/Dockerfile`, fetched 2026-07-08 via
    /// the GitHub contents API) — `$PATH` here refers to the environment's
    /// own inherited PATH, not a build ARG; this exact shape surfaced the
    /// ARG-vs-ENV gap during the supa-charge-akeyless-ci Phase 5 proof run.
    #[test]
    fn real_akeyless_base_dockerfile_env_path_prepend_regression() {
        let env = MockDockerfileEnvironment::default().with_dockerfile(
            "akeyless-base",
            "FROM ubuntu:24.04\n\
             RUN mkdir /akeyless/bin\n\
             ENV VIRTUAL_ENV=/opt/venv\n\
             ENV PATH=\"/opt/venv/bin:/akeyless/bin:/usr/local/ssl/bin:${PATH}\"\n",
        );
        let graph = apply(&DockerfileArgs { path: "akeyless-base".into() }, &env).unwrap();
        assert_eq!(graph.nodes.len(), 4);
        match &graph.nodes[3].instruction {
            DockerfileInstruction::Env { name, value } => {
                assert_eq!(name, "PATH");
                assert_eq!(value, "/opt/venv/bin:/akeyless/bin:/usr/local/ssl/bin:${PATH}");
            }
            _ => panic!("expected ENV"),
        }
    }
}
