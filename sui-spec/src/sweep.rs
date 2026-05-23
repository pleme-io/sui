//! Library entry point for the shadow-sweep loop.
//!
//! Both the `sui-sweep` binary and the (future) `sui rebuild-shadow`
//! subcommand wrap this module — there is exactly one driver for the
//! "load corpus × walk flakes × run dual subprocess × classify ×
//! report" pipeline.  Per the prime directive: solve once, in one
//! place, at one time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::exec::{command_argv, dual_run, run_with_timeout, CapturedOutput};
use crate::parity::{
    excerpt, ParityCheck, ProbeContext, ProbeRecord, ShadowReport, Verdict,
};

/// Size cap (bytes) for stdout/stderr excerpts kept in the report.
pub const EXCERPT_CAP: usize = 4096;

/// Default per-probe timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Which corpus (or all of them) to drive in one sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corpus {
    /// `parity_probes.lisp` — single-expression eval probes.
    Parity,
    /// `builtin_smoke_probes.lisp` — per-builtin-module smoke tests.
    BuiltinSmoke,
    /// `rebuild_probes.lisp` — host-aware rebuild-stage probes.
    Rebuild,
    /// All three corpora.
    All,
}

impl Corpus {
    /// Parse from operator-supplied string.  Returns `None` if the
    /// string doesn't match any known corpus.
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "parity"        => Some(Corpus::Parity),
            "builtins" | "builtin-smoke" => Some(Corpus::BuiltinSmoke),
            "rebuild"       => Some(Corpus::Rebuild),
            "all"           => Some(Corpus::All),
            _               => None,
        }
    }

    /// Stable identifier for log / report consumption.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Corpus::Parity       => "parity",
            Corpus::BuiltinSmoke => "builtins",
            Corpus::Rebuild      => "rebuild",
            Corpus::All          => "all",
        }
    }
}

/// Sweep configuration.  Defaults match the legacy `sui-sweep` shape;
/// the operator overrides only what they want.
#[derive(Debug, Clone)]
pub struct SweepConfig {
    /// Path to the sui binary under test.
    pub sui_bin: PathBuf,
    /// Path to the cppnix oracle binary.
    pub nix_bin: PathBuf,
    /// Root directory to walk for `flake.nix` files when no explicit
    /// flakes are supplied.
    pub flakes_root: PathBuf,
    /// Explicit list of flake directories.  If non-empty, overrides
    /// `flakes_root`.
    pub explicit_flakes: Vec<PathBuf>,
    /// Probes must carry at least one of these tags to be selected.
    /// Empty = no include filter.
    pub include_tags: Vec<String>,
    /// Probes carrying any of these tags are excluded.
    pub exclude_tags: Vec<String>,
    /// Per-probe timeout.
    pub timeout: Duration,
    /// Which corpus (or all) to drive.
    pub corpus: Corpus,
    /// `true` = print per-probe diagnostics to stderr.
    pub verbose: bool,
    /// Where to write the JSON report.  `None` = skip write but still
    /// return the report value to the caller.
    pub report_path: Option<PathBuf>,
}

impl SweepConfig {
    /// Defaults aligned with the legacy `sui-sweep` binary.
    #[must_use]
    pub fn defaults() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let sui_bin = std::env::current_dir()
            .map(|p| p.join("target/release/sui"))
            .unwrap_or_else(|_| PathBuf::from("sui"));
        Self {
            sui_bin,
            nix_bin: PathBuf::from("nix"),
            flakes_root: PathBuf::from(format!("{home}/code/github/pleme-io")),
            explicit_flakes: Vec::new(),
            include_tags: Vec::new(),
            exclude_tags: Vec::new(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            corpus: Corpus::All,
            verbose: false,
            report_path: None,
        }
    }
}

/// Run the sweep, returning a [`ShadowReport`].  Writes the report to
/// `config.report_path` if set.
///
/// # Errors
///
/// Returns an error if any corpus fails to compile, or if writing the
/// report file fails.
pub fn run(config: &SweepConfig) -> Result<ShadowReport, crate::SpecError> {
    let probes = load_selected(config.corpus)?;
    let flakes = resolve_flakes(config);

    eprintln!(
        "{}  {}  {}  {}  {}",
        crate::style::glyph_snowflake(),
        crate::style::header("sui-sweep"),
        crate::style::muted(&format!("corpus={}", config.corpus.name())),
        crate::style::muted(&format!("probes={}", probes.len())),
        crate::style::muted(&format!(
            "flakes={}  timeout={}s",
            flakes.len(),
            config.timeout.as_secs(),
        )),
    );

    let mut records = Vec::new();
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();

    for flake in &flakes {
        let ctx = ProbeContext::current(flake.clone());
        for probe in &probes {
            // Honour tag filters first — cheaper than constructing an
            // invocation we'll discard.
            if !tag_filter_admits(probe.tags(), &config.include_tags, &config.exclude_tags) {
                continue;
            }
            let verdict = if probe.applies(&ctx) {
                run_one(&**probe, &ctx, config, &mut records)
            } else {
                push_record(&**probe, &ctx, None, None, Verdict::NotApplicable, &mut records);
                Verdict::NotApplicable
            };
            *tally.entry(verdict.name().to_string()).or_default() += 1;
            eprint!("{}", verdict.glyph_styled());
            if config.verbose {
                eprintln!(" {} :: {}",
                    crate::style::ident(probe.name()),
                    crate::style::muted(&ctx.flake_label),
                );
            }
        }
        eprintln!("  {}", crate::style::muted(&ctx.flake_label));
    }

    let report = ShadowReport {
        generated_at: utc_now_iso8601(),
        generator: format!("sui-sweep {}", env!("CARGO_PKG_VERSION")),
        host: crate::parity::current_hostname(),
        system: crate::parity::current_nix_system(),
        os: crate::parity::TargetOs::current().as_str().to_string(),
        user: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
        sui_version: detect_version(&config.sui_bin),
        nix_version: detect_version(&config.nix_bin),
        records,
        tally,
    };

    if let Some(path) = config.report_path.as_deref() {
        write_report(path, &report).map_err(|e| crate::SpecError::Interp {
            phase: "sweep::report-write".into(),
            message: format!("{}: {e}", path.display()),
        })?;
        eprintln!("\nreport: {}", path.display());
    }

    let runs = report.records.len();
    let diverged = report.divergence_count();
    let passed = runs - diverged;
    let summary_glyph = if diverged == 0 {
        crate::style::glyph_ok()
    } else {
        crate::style::glyph_fail()
    };
    println!(
        "\n{}  {}  {}  {}  {}",
        summary_glyph,
        crate::style::header("sui-sweep complete"),
        crate::style::ident(&format!("{runs} runs")),
        crate::style::success(&format!("{passed} passed")),
        if diverged == 0 {
            crate::style::muted("0 diverged")
        } else {
            crate::style::error(&format!("{diverged} diverged"))
        },
    );

    Ok(report)
}

/// Decide on a default report path: `~/.cache/sui/shadow-reports/<host>-<ts>.json`.
#[must_use]
pub fn default_report_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let host = crate::parity::current_hostname();
    let ts = utc_now_iso8601().replace(':', "-");
    PathBuf::from(home)
        .join(".cache")
        .join("sui")
        .join("shadow-reports")
        .join(format!("{host}-{ts}.json"))
}

// ── Internals ──────────────────────────────────────────────────────

fn run_one(
    probe: &dyn ParityCheck,
    ctx: &ProbeContext,
    config: &SweepConfig,
    records: &mut Vec<ProbeRecord>,
) -> Verdict {
    let mut sui_cmd = probe.sui_invocation(ctx, &config.sui_bin);
    let mut nix_cmd = probe.nix_invocation(ctx, &config.nix_bin);
    let sui_argv = command_argv(&sui_cmd);
    let nix_argv = command_argv(&nix_cmd);
    let (sui_out, nix_out) = dual_run(&mut sui_cmd, &mut nix_cmd, config.timeout);
    let verdict = probe.classify(&sui_out, &nix_out);
    push_full_record(
        probe, ctx, sui_argv, nix_argv, &sui_out, &nix_out, verdict, records,
    );
    verdict
}

fn push_record(
    probe: &dyn ParityCheck,
    ctx: &ProbeContext,
    sui_argv: Option<Vec<String>>,
    nix_argv: Option<Vec<String>>,
    verdict: Verdict,
    records: &mut Vec<ProbeRecord>,
) {
    records.push(ProbeRecord {
        name: probe.name().to_string(),
        kind: probe.kind(),
        tags: probe.tags().to_vec(),
        flake: ctx.flake_label.clone(),
        sui_argv: sui_argv.unwrap_or_default(),
        nix_argv: nix_argv.unwrap_or_default(),
        sui_exit: None, nix_exit: None,
        sui_stdout_excerpt: String::new(),
        nix_stdout_excerpt: String::new(),
        sui_stderr_excerpt: String::new(),
        nix_stderr_excerpt: String::new(),
        sui_duration_ms: 0, nix_duration_ms: 0,
        sui_timed_out: false, nix_timed_out: false,
        verdict,
    });
}

#[allow(clippy::too_many_arguments)]
fn push_full_record(
    probe: &dyn ParityCheck,
    ctx: &ProbeContext,
    sui_argv: Vec<String>,
    nix_argv: Vec<String>,
    sui: &CapturedOutput,
    nix: &CapturedOutput,
    verdict: Verdict,
    records: &mut Vec<ProbeRecord>,
) {
    records.push(ProbeRecord {
        name: probe.name().to_string(),
        kind: probe.kind(),
        tags: probe.tags().to_vec(),
        flake: ctx.flake_label.clone(),
        sui_argv,
        nix_argv,
        sui_exit: sui.exit_code,
        nix_exit: nix.exit_code,
        sui_stdout_excerpt: excerpt(&sui.stdout, EXCERPT_CAP),
        nix_stdout_excerpt: excerpt(&nix.stdout, EXCERPT_CAP),
        sui_stderr_excerpt: excerpt(&sui.stderr, EXCERPT_CAP),
        nix_stderr_excerpt: excerpt(&nix.stderr, EXCERPT_CAP),
        sui_duration_ms: sui.duration.as_millis(),
        nix_duration_ms: nix.duration.as_millis(),
        sui_timed_out: sui.timed_out,
        nix_timed_out: nix.timed_out,
        verdict,
    });
}

fn load_selected(corpus: Corpus) -> Result<Vec<Box<dyn ParityCheck>>, crate::SpecError> {
    let mut out: Vec<Box<dyn ParityCheck>> = Vec::new();
    match corpus {
        Corpus::Parity => {
            for p in crate::probe::load_canonical()? {
                out.push(Box::new(p));
            }
        }
        Corpus::BuiltinSmoke => {
            for p in crate::probe::load_builtin_smoke()? {
                // builtin-smoke probes carry the "builtin-smoke" tag,
                // but the kind is reported via the ProbeKind enum on
                // the wrapper below.
                out.push(Box::new(crate::probe::BuiltinSmokeProbe(p)));
            }
        }
        Corpus::Rebuild => {
            for p in crate::rebuild::load_canonical()? {
                out.push(Box::new(p));
            }
        }
        Corpus::All => {
            for p in crate::probe::load_canonical()? {
                out.push(Box::new(p));
            }
            for p in crate::probe::load_builtin_smoke()? {
                out.push(Box::new(crate::probe::BuiltinSmokeProbe(p)));
            }
            for p in crate::rebuild::load_canonical()? {
                out.push(Box::new(p));
            }
        }
    }
    Ok(out)
}

fn resolve_flakes(config: &SweepConfig) -> Vec<PathBuf> {
    if !config.explicit_flakes.is_empty() {
        return config.explicit_flakes.clone();
    }
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(&config.flakes_root) else { return out; };
    for entry in read.flatten() {
        let p = entry.path();
        if p.is_dir() && p.join("flake.nix").exists() {
            out.push(p);
        }
    }
    out.sort();
    out
}

fn tag_filter_admits(tags: &[String], include: &[String], exclude: &[String]) -> bool {
    if !exclude.is_empty() && tags.iter().any(|t| exclude.iter().any(|x| x == t)) {
        return false;
    }
    if include.is_empty() {
        return true;
    }
    tags.iter().any(|t| include.iter().any(|x| x == t))
}

fn detect_version(bin: &Path) -> Option<String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("--version");
    match run_with_timeout(&mut cmd, Duration::from_secs(5)) {
        Ok(out) if out.success => Some(out.stdout.trim().to_string()),
        _ => None,
    }
}

fn write_report(path: &Path, report: &ShadowReport) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(report)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(path, body)
}

fn utc_now_iso8601() -> String {
    // Tiny dependency-free formatter — good enough for filenames + sort
    // order; full chrono would compound a giant transitive dep tree for
    // a six-line job.  Output: `YYYY-MM-DDTHH-MM-SSZ` (colons replaced
    // because they'd be illegal on Windows + awkward on macOS).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    format_unix_seconds(now)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::similar_names)]
fn format_unix_seconds(mut secs: i64) -> String {
    // 1970-01-01 is a Thursday; we walk days forward and account for
    // leap years.  Range: 1970 .. 2999 — good enough for any operator
    // who's ever going to read this report.
    let sec_of_day = (secs % 86_400) as u32;
    secs /= 86_400;
    let hour = sec_of_day / 3600;
    let min = (sec_of_day % 3600) / 60;
    let sec = sec_of_day % 60;
    let (year, month, day) = epoch_days_to_ymd(secs as i32);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}-{min:02}-{sec:02}Z")
}

fn epoch_days_to_ymd(mut days: i32) -> (i32, u32, u32) {
    let mut year = 1970;
    while days >= year_len(year) {
        days -= year_len(year);
        year += 1;
    }
    let mut month = 1u32;
    while days >= month_len(year, month) as i32 {
        days -= month_len(year, month) as i32;
        month += 1;
    }
    (year, month, (days + 1) as u32)
}

fn year_len(year: i32) -> i32 {
    if is_leap(year) { 366 } else { 365 }
}

fn month_len(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11              => 30,
        2 if is_leap(year)          => 29,
        2                            => 28,
        _                            => 0,
    }
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_from_str_canonical_names() {
        assert_eq!(Corpus::from_str("parity"), Some(Corpus::Parity));
        assert_eq!(Corpus::from_str("builtins"), Some(Corpus::BuiltinSmoke));
        assert_eq!(Corpus::from_str("builtin-smoke"), Some(Corpus::BuiltinSmoke));
        assert_eq!(Corpus::from_str("rebuild"), Some(Corpus::Rebuild));
        assert_eq!(Corpus::from_str("all"), Some(Corpus::All));
        assert_eq!(Corpus::from_str("nope"), None);
    }

    #[test]
    fn tag_filter_include_only() {
        let tags = vec!["smoke".to_string(), "regression".to_string()];
        assert!(tag_filter_admits(&tags, &["smoke".into()], &[]));
        assert!(!tag_filter_admits(&tags, &["other".into()], &[]));
        assert!(tag_filter_admits(&tags, &[], &[]));
    }

    #[test]
    fn tag_filter_exclude_takes_priority() {
        let tags = vec!["smoke".to_string(), "expensive".to_string()];
        assert!(!tag_filter_admits(&tags, &[], &["expensive".into()]));
        assert!(!tag_filter_admits(&tags, &["smoke".into()], &["expensive".into()]));
    }

    #[test]
    fn timestamp_renders_iso_shape() {
        // Anchors that don't drift with the system clock:
        // 0           = 1970-01-01 00:00:00 UTC (epoch).
        // 946_684_800 = 2000-01-01 00:00:00 UTC (Y2K).
        // 1_704_067_200 = 2024-01-01 00:00:00 UTC (leap year sanity).
        assert_eq!(format_unix_seconds(0),             "1970-01-01T00-00-00Z");
        assert_eq!(format_unix_seconds(946_684_800),   "2000-01-01T00-00-00Z");
        assert_eq!(format_unix_seconds(1_704_067_200), "2024-01-01T00-00-00Z");
        // Mid-day formatting — 1970-01-02 12:34:56 UTC.
        let mid = 86_400 + 12 * 3600 + 34 * 60 + 56;
        assert_eq!(format_unix_seconds(mid), "1970-01-02T12-34-56Z");
    }

    #[test]
    fn default_report_path_is_under_cache_dir() {
        let p = default_report_path();
        let s = p.display().to_string();
        assert!(s.contains(".cache/sui/shadow-reports"));
        assert!(s.ends_with(".json"));
    }

    #[test]
    fn writing_report_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/report.json");
        let report = ShadowReport {
            generated_at: "ts".into(), generator: "gen".into(),
            host: "h".into(), system: "s".into(),
            os: "linux".into(), user: "u".into(),
            sui_version: None, nix_version: None,
            records: Vec::new(), tally: BTreeMap::new(),
        };
        write_report(&path, &report).unwrap();
        assert!(path.exists());
    }
}
