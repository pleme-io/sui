//! `sui flip-probe` — one attribute, both engines, one receipt.
//!
//! Evaluates `<installable>.drvPath` with sui and with CppNix, each as its own
//! child process (so each gets its own wall clock and peak RSS, and a crash in
//! one cannot take the probe down), and writes a [`Receipt`]. When the two
//! drvPaths differ it walks both `.drv` graphs to the first drv that differs
//! while all its same-name inputs agree, and names the fields that differ
//! there ([`sui_compat::drv_graph`]).
//!
//! The receipt is the evidence a flip gate consumes, so its shape is a
//! contract: `docs/FLIP-PROBE.md` documents every field, and
//! `receipt_schema_is_stable` fails when a field is added, removed or renamed
//! without the schema version moving.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sui_compat::drv_graph::{self, FieldDiff};

/// The receipt schema this build writes.
pub const SCHEMA: &str = "sui.flip-probe/v1";

/// How much of a failing engine's stderr the receipt keeps.
const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// Frontier pairs listed beyond `first` (the count is always complete).
const FRONTIER_LISTED: usize = 50;

/// One probe's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Receipt {
    /// Always [`SCHEMA`].
    pub schema: String,
    /// The installable as given (`<flake>#<attr>`).
    pub installable: String,
    /// The flake reference part.
    pub flake_ref: String,
    /// The attribute path part (without `.drvPath`).
    pub attr_path: String,
    /// What CppNix locked the flake to.
    pub flake: FlakeIdentity,
    /// The host the probe ran on.
    pub host: Host,
    /// Unix seconds when the probe started.
    pub started_at_unix: u64,
    /// sui's run.
    pub sui: EngineRun,
    /// CppNix's run.
    pub cppnix: EngineRun,
    /// The verdict.
    pub verdict: Verdict,
    /// Present exactly when `verdict` is `diverge`.
    pub divergence: Option<Divergence>,
}

/// The flake as CppNix resolved it (`nix flake metadata --json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FlakeIdentity {
    /// The locked git revision; `null` for a dirty tree or a non-git source.
    pub rev: Option<String>,
    /// The revision a dirty tree was taken from, when dirty.
    pub dirty_rev: Option<String>,
    /// The locked NAR hash of the flake source, when CppNix reports one.
    pub nar_hash: Option<String>,
    /// The store path CppNix copied the flake source to: content-addressed, so
    /// it identifies the evaluated source even when there is no revision.
    pub source_path: Option<String>,
}

/// Where the probe ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Host {
    /// Rust's `<arch>-<os>` for this build of sui, e.g. `aarch64-macos`.
    pub platform: String,
}

/// One engine's evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EngineRun {
    /// `sui` or `cppnix`.
    pub engine: String,
    /// The engine's own version string.
    pub version: String,
    /// The exact command line run.
    pub argv: Vec<String>,
    /// The drvPath printed, when the run succeeded.
    pub drv_path: Option<String>,
    /// Exit code, when the process exited.
    pub exit_code: Option<i32>,
    /// Terminating signal, when the process was killed.
    pub signal: Option<i32>,
    /// Wall-clock seconds.
    pub wall_seconds: f64,
    /// Peak resident set size, bytes (`wait4` rusage, normalised across OSes).
    pub max_rss_bytes: u64,
    /// The last bytes of stderr when the run failed; empty otherwise.
    pub stderr_tail: String,
}

impl EngineRun {
    fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && self.drv_path.is_some()
    }
}

/// The probe's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Verdict {
    /// Byte-identical drvPaths.
    Match,
    /// Both produced a drvPath; they differ.
    Diverge,
    /// sui failed; CppNix produced a drvPath.
    SuiFailed,
    /// CppNix failed; sui produced a drvPath.
    CppnixFailed,
    /// Neither produced a drvPath.
    BothFailed,
}

/// Where the two graphs part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Divergence {
    /// The shallowest frontier drv: it differs, every same-name input agrees.
    /// `null` when the walk reached no frontier (see `missing`/`unreadable`).
    pub first: Option<DivergentDrv>,
    /// How many frontier drvs the walk found in total.
    pub frontier_count: usize,
    /// Up to 50 frontier pairs after `first`.
    pub frontier: Vec<DrvPair>,
    /// Input names present a different number of times on the two sides.
    pub missing: Vec<MissingInput>,
    /// Drvs the walk could not read.
    pub unreadable: Vec<Unreadable>,
    /// Drv pairs visited.
    pub visited: usize,
    /// True when the walk stopped at its node budget: the frontier is partial.
    pub truncated: bool,
}

/// The first differing drv, with its differing fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DivergentDrv {
    /// The drv's name (store path without hash).
    pub name: String,
    /// Names from the top drv down to this one.
    pub trail: Vec<String>,
    pub sui_drv: String,
    pub cppnix_drv: String,
    /// Every differing field, in `field_diffs` order.
    pub fields: Vec<FieldDiff>,
}

/// A frontier pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DrvPair {
    pub name: String,
    pub sui_drv: String,
    pub cppnix_drv: String,
}

/// An input name whose multiplicity differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MissingInput {
    pub parent: String,
    pub name: String,
    pub count_sui: usize,
    pub count_cppnix: usize,
}

/// A drv the walk could not read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Unreadable {
    pub path: String,
    /// `sui` or `cppnix`.
    pub side: String,
}

/// Decide the verdict and, on divergence, walk the graphs. `load` reads a
/// `.drv` (the store, in production).
pub fn judge(
    sui: &EngineRun,
    cppnix: &EngineRun,
    max_nodes: usize,
    load: impl FnMut(&str) -> Option<Vec<u8>>,
) -> (Verdict, Option<Divergence>) {
    let (s, n) = match (sui.succeeded(), cppnix.succeeded()) {
        (false, false) => return (Verdict::BothFailed, None),
        (false, true) => return (Verdict::SuiFailed, None),
        (true, false) => return (Verdict::CppnixFailed, None),
        (true, true) => (sui.drv_path.as_deref().unwrap_or_default(), cppnix.drv_path.as_deref().unwrap_or_default()),
    };
    if s == n {
        return (Verdict::Match, None);
    }
    let report = drv_graph::bisect_drv_graph(s, n, max_nodes, load);
    let mut leaves = report.leaves.iter();
    let first = leaves.next().map(|l| DivergentDrv {
        name: drv_graph::drv_name(&l.nix_path),
        trail: l.trail.clone(),
        sui_drv: l.sui_path.clone(),
        cppnix_drv: l.nix_path.clone(),
        fields: drv_graph::field_diffs(&l.sui, &l.nix),
    });
    let frontier = leaves
        .take(FRONTIER_LISTED)
        .map(|l| DrvPair {
            name: drv_graph::drv_name(&l.nix_path),
            sui_drv: l.sui_path.clone(),
            cppnix_drv: l.nix_path.clone(),
        })
        .collect();
    let divergence = Divergence {
        first,
        frontier_count: report.leaves.len(),
        frontier,
        missing: report
            .missing
            .iter()
            .map(|m| MissingInput {
                parent: m.parent.clone(),
                name: m.name.clone(),
                count_sui: m.count_sui,
                count_cppnix: m.count_nix,
            })
            .collect(),
        unreadable: report
            .unreadable
            .iter()
            .map(|(path, side)| Unreadable {
                path: path.clone(),
                side: if *side == "nix" { "cppnix".into() } else { (*side).to_string() },
            })
            .collect(),
        visited: report.visited,
        truncated: report.truncated,
    };
    (Verdict::Diverge, Some(divergence))
}

/// What to probe and with what.
#[derive(Debug, Clone)]
pub struct ProbeOptions {
    /// `<flake>#<attr>`.
    pub installable: String,
    /// The sui binary to evaluate with (normally the running one).
    pub sui: std::path::PathBuf,
    /// The CppNix `nix` binary.
    pub nix: std::path::PathBuf,
    /// Node budget for the graph walk.
    pub max_nodes: usize,
}

/// Why a probe could not run at all (as opposed to an engine failing, which
/// is a verdict).
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("installable {0:?} is not <flake>#<attr>")]
    Installable(String),
    #[error("cannot run {program}: {cause}")]
    Spawn {
        program: String,
        #[source]
        cause: std::io::Error,
    },
}

const NIX_FEATURES: [&str; 2] = ["--extra-experimental-features", "nix-command flakes"];

/// Run the probe.
///
/// # Errors
/// A malformed installable or an engine binary that cannot be started.
pub fn probe(opts: &ProbeOptions) -> Result<Receipt, ProbeError> {
    let (flake_ref, attr_path) = opts
        .installable
        .split_once('#')
        .filter(|(f, a)| !f.is_empty() && !a.is_empty())
        .ok_or_else(|| ProbeError::Installable(opts.installable.clone()))?;
    let target = format!("{flake_ref}#{attr_path}.drvPath");
    let started_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    let sui_version = format!("sui {}", env!("CARGO_PKG_VERSION"));
    let sui_argv = vec![
        opts.sui.display().to_string(),
        "eval".into(),
        "--no-eval-cache".into(),
        "--raw".into(),
        target.clone(),
    ];
    let sui = measure("sui", sui_version, &sui_argv)?;

    let nix = opts.nix.display().to_string();
    let cppnix_version = capture(&[nix.clone(), "--version".into()]).unwrap_or_default();
    let mut cppnix_argv = vec![nix.clone(), "eval".into()];
    cppnix_argv.extend(NIX_FEATURES.iter().map(|s| (*s).to_string()));
    cppnix_argv.extend(["--raw".to_string(), target]);
    let cppnix = measure("cppnix", cppnix_version, &cppnix_argv)?;

    let mut meta_argv = vec![nix, "flake".into(), "metadata".into(), "--json".into()];
    meta_argv.extend(NIX_FEATURES.iter().map(|s| (*s).to_string()));
    meta_argv.push(flake_ref.to_string());
    let flake = capture(&meta_argv).map(|j| flake_identity(&j)).unwrap_or_default();

    let (verdict, divergence) = judge(&sui, &cppnix, opts.max_nodes, |p| std::fs::read(p).ok());
    Ok(Receipt {
        schema: SCHEMA.into(),
        installable: opts.installable.clone(),
        flake_ref: flake_ref.into(),
        attr_path: attr_path.into(),
        flake,
        host: Host { platform: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS) },
        started_at_unix,
        sui,
        cppnix,
        verdict,
        divergence,
    })
}

/// The identity fields of `nix flake metadata --json`.
fn flake_identity(json: &str) -> FlakeIdentity {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    let s = |ptr: &str| v.pointer(ptr).and_then(|x| x.as_str()).map(str::to_string);
    FlakeIdentity {
        rev: s("/revision"),
        dirty_rev: s("/dirtyRevision"),
        nar_hash: s("/locked/narHash"),
        source_path: s("/path"),
    }
}

/// Run a command and return its trimmed stdout when it succeeds.
fn capture(argv: &[String]) -> Option<String> {
    let out = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run one engine as a child, measuring wall time and peak RSS.
fn measure(engine: &str, version: String, argv: &[String]) -> Result<EngineRun, ProbeError> {
    let start = Instant::now();
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|cause| ProbeError::Spawn { program: argv[0].clone(), cause })?;
    let mut out_pipe = child.stdout.take().expect("piped");
    let mut err_pipe = child.stderr.take().expect("piped");
    let out_reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out_pipe.read_to_end(&mut b);
        b
    });
    let err_reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err_pipe.read_to_end(&mut b);
        b
    });
    let status = crate::child_usage::wait(child.id()).map_err(|cause| ProbeError::Spawn {
        program: argv[0].clone(),
        cause,
    })?;
    let wall_seconds = start.elapsed().as_secs_f64();
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();

    let printed = String::from_utf8_lossy(&stdout).trim().to_string();
    let ok = status.exit_code == Some(0) && printed.starts_with('/') && printed.ends_with(".drv");
    let stderr_tail = if ok {
        String::new()
    } else {
        let start = stderr.len().saturating_sub(STDERR_TAIL_BYTES);
        String::from_utf8_lossy(&stderr[start..]).into_owned()
    };
    Ok(EngineRun {
        engine: engine.into(),
        version,
        argv: argv.to_vec(),
        drv_path: ok.then_some(printed),
        exit_code: status.exit_code,
        signal: status.signal,
        wall_seconds,
        max_rss_bytes: status.max_rss_bytes,
        stderr_tail,
    })
}

/// Read a receipt back (for gates and tests).
///
/// # Errors
/// Unreadable file or a document that is not a receipt of this schema.
pub fn read_receipt(path: &Path) -> Result<Receipt, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let r: Receipt = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if r.schema != SCHEMA {
        return Err(format!("{}: schema {} is not {SCHEMA}", path.display(), r.schema));
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use sui_compat::derivation::{Derivation, DerivationOutput};

    fn drv(name: &str, inputs: &[&str], env: &[(&str, &str)]) -> Derivation {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput { path: format!("/nix/store/{name:0>32}-{name}"), hash_algo: String::new(), hash: String::new() },
        );
        Derivation {
            outputs,
            input_derivations: inputs.iter().map(|p| ((*p).to_string(), vec!["out".to_string()])).collect(),
            input_sources: Vec::new(),
            system: "x86_64-linux".into(),
            builder: "/bin/sh".into(),
            args: Vec::new(),
            env: env.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
        }
    }

    fn run(engine: &str, drv_path: &str) -> EngineRun {
        EngineRun {
            engine: engine.into(),
            version: "v".into(),
            argv: vec![],
            drv_path: Some(drv_path.into()),
            exit_code: Some(0),
            signal: None,
            wall_seconds: 1.0,
            max_rss_bytes: 1,
            stderr_tail: String::new(),
        }
    }

    /// Two graphs, top → mid → leaf, differing only in one env value of the
    /// leaf. Every ancestor's path differs by cascade; the probe must name the
    /// LEAF and its field, never the root.
    #[test]
    fn a_divergence_in_one_leaf_is_named_at_the_leaf() {
        let mut store: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut put = |key: &str, d: &Derivation| {
            let p = format!("/nix/store/{key:0>32}.drv");
            store.insert(p.clone(), d.serialize().into_bytes());
            p
        };
        let other_s = put("s-other", &drv("other", &[], &[]));
        let other_n = other_s.clone();
        let leaf_s = put("s-leaf", &drv("leaf", &[], &[("flags", "-O2")]));
        let leaf_n = put("n-leaf", &drv("leaf", &[], &[("flags", "-O3")]));
        let mid_s = put("s-mid", &drv("mid", &[&leaf_s, &other_s], &[]));
        let mid_n = put("n-mid", &drv("mid", &[&leaf_n, &other_n], &[]));
        let top_s = put("s-top", &drv("top", &[&mid_s], &[]));
        let top_n = put("n-top", &drv("top", &[&mid_n], &[]));

        let (verdict, div) = judge(&run("sui", &top_s), &run("cppnix", &top_n), 1000, |p| store.get(p).cloned());
        assert_eq!(verdict, Verdict::Diverge);
        let div = div.unwrap();
        let first = div.first.expect("a frontier");
        assert_ne!(first.cppnix_drv, top_n, "named the root");
        assert_eq!(first.cppnix_drv, leaf_n);
        assert_eq!(first.sui_drv, leaf_s);
        assert_eq!(div.frontier_count, 1);
        assert_eq!(first.fields, vec![FieldDiff {
            field: "env.flags".into(),
            sui: Some("-O2".into()),
            cppnix: Some("-O3".into()),
            hash_cascade_only: false,
        }]);
    }

    #[test]
    fn identical_paths_match_and_failures_are_their_own_verdicts() {
        let a = run("sui", "/nix/store/x.drv");
        let b = run("cppnix", "/nix/store/x.drv");
        assert_eq!(judge(&a, &b, 10, |_| None), (Verdict::Match, None));
        let mut failed = a.clone();
        failed.exit_code = Some(1);
        failed.drv_path = None;
        assert_eq!(judge(&failed, &b, 10, |_| None).0, Verdict::SuiFailed);
        assert_eq!(judge(&b, &failed, 10, |_| None).0, Verdict::CppnixFailed);
        assert_eq!(judge(&failed, &failed, 10, |_| None).0, Verdict::BothFailed);
    }

    /// The receipt's key set is a contract (docs/FLIP-PROBE.md). A change here
    /// must move SCHEMA and the doc in the same commit.
    #[test]
    fn receipt_schema_is_stable() {
        let r = Receipt {
            schema: SCHEMA.into(),
            installable: "f#a".into(),
            flake_ref: "f".into(),
            attr_path: "a".into(),
            flake: FlakeIdentity::default(),
            host: Host { platform: "p".into() },
            started_at_unix: 0,
            sui: run("sui", "/nix/store/a.drv"),
            cppnix: run("cppnix", "/nix/store/b.drv"),
            verdict: Verdict::Diverge,
            divergence: Some(Divergence {
                first: Some(DivergentDrv {
                    name: "n".into(),
                    trail: vec![],
                    sui_drv: "s".into(),
                    cppnix_drv: "c".into(),
                    fields: vec![FieldDiff { field: "f".into(), sui: None, cppnix: None, hash_cascade_only: false }],
                }),
                frontier_count: 1,
                frontier: vec![DrvPair { name: "n".into(), sui_drv: "s".into(), cppnix_drv: "c".into() }],
                missing: vec![MissingInput { parent: "p".into(), name: "n".into(), count_sui: 1, count_cppnix: 0 }],
                unreadable: vec![Unreadable { path: "p".into(), side: "sui".into() }],
                visited: 1,
                truncated: false,
            }),
        };
        let v = serde_json::to_value(&r).unwrap();
        let keys = |v: &serde_json::Value| -> Vec<String> {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(SCHEMA, "sui.flip-probe/v1");
        assert_eq!(keys(&v), ["attrPath", "cppnix", "divergence", "flake", "flakeRef", "host", "installable", "schema", "startedAtUnix", "sui", "verdict"]);
        assert_eq!(keys(&v["sui"]), ["argv", "drvPath", "engine", "exitCode", "maxRssBytes", "signal", "stderrTail", "version", "wallSeconds"]);
        assert_eq!(keys(&v["flake"]), ["dirtyRev", "narHash", "rev", "sourcePath"]);
        assert_eq!(keys(&v["divergence"]), ["first", "frontier", "frontierCount", "missing", "truncated", "unreadable", "visited"]);
        assert_eq!(keys(&v["divergence"]["first"]), ["cppnixDrv", "fields", "name", "suiDrv", "trail"]);
        assert_eq!(keys(&v["divergence"]["first"]["fields"][0]), ["cppnix", "field", "hashCascadeOnly", "sui"]);
        assert_eq!(v["verdict"], "diverge");
        let back: Receipt = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }
}
