//! `sui-sweep` — differential parity runner.
//!
//! Reads the three canonical probe corpora (eval / builtin-smoke /
//! rebuild), walks the configured flakes, runs every probe through both
//! `sui` and the cppnix oracle, classifies each result, and writes a
//! typed [`ShadowReport`] as JSON.  All heavy lifting lives in
//! [`sui_spec::sweep::run`] — this binary is a thin clap-style arg
//! parser around it.  The same library function backs the future
//! `sui rebuild-shadow` subcommand: solve once, drive both surfaces.
//!
//! ## Usage
//!
//! ```text
//! sui-sweep [--sui <path>] [--nix <path>] [--flakes-root <dir>]
//!           [--corpus parity|builtins|rebuild|all]
//!           [--tag <tag>]... [--skip-tag <tag>]...
//!           [--timeout-secs <n>] [--report <path>] [--no-report]
//!           [--verbose | -v] [<flake>...]
//! ```
//!
//! If no flake arguments are supplied, `--flakes-root` is walked
//! (default `~/code/github/pleme-io/`) for every direct child
//! containing a `flake.nix`.  If one or more `--tag <tag>` flags are
//! supplied, only probes that carry at least one of those tags are
//! run; `--skip-tag <tag>` excludes by tag (taking precedence).
//!
//! Output: a per-probe progress glyph line, a one-line summary, and a
//! typed JSON report written to `--report` (default
//! `~/.cache/sui/shadow-reports/<host>-<ts>.json`).  Pass `--no-report`
//! to skip the write.
//!
//! [`ShadowReport`]: sui_spec::parity::ShadowReport
//! [`sui_spec::sweep::run`]: sui_spec::sweep::run

use std::path::PathBuf;
use std::time::Duration;

use sui_spec::sweep::{self, Corpus, SweepConfig};

struct Args {
    config: SweepConfig,
}

fn print_help() {
    println!(
        "sui-sweep — differential parity runner for sui vs CppNix.\n\n\
         Usage:\n  sui-sweep [options] [<flake-path>...]\n\n\
         Options:\n  \
         --sui <path>          Path to sui binary (default: target/release/sui)\n  \
         --nix <path>          Path to nix binary (default: nix in $PATH)\n  \
         --flakes-root <dir>   Root to walk for flake.nix files\n                        \
         (default: ~/code/github/pleme-io)\n  \
         --corpus <name>       parity | builtins | rebuild | all (default: all)\n  \
         --tag <tag>           Only run probes carrying <tag>.  May repeat.\n  \
         --skip-tag <tag>      Exclude probes carrying <tag>.  May repeat.\n  \
         --timeout-secs <n>    Per-probe timeout (default: 30)\n  \
         --report <path>       JSON report output path\n                        \
         (default: ~/.cache/sui/shadow-reports/<host>-<ts>.json)\n  \
         --no-report           Skip writing the JSON report\n  \
         --verbose / -v        Print per-probe diagnostics\n\n\
         Probe corpora are compiled from:\n  \
         sui-spec/specs/parity_probes.lisp\n  \
         sui-spec/specs/builtin_smoke_probes.lisp\n  \
         sui-spec/specs/rebuild_probes.lisp"
    );
}

#[allow(clippy::too_many_lines)]
fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut config = SweepConfig::defaults();
    let mut no_report = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sui" => {
                config.sui_bin = args.next().map(PathBuf::from)
                    .ok_or("--sui needs value")?;
            }
            "--nix" => {
                config.nix_bin = args.next().map(PathBuf::from)
                    .ok_or("--nix needs value")?;
            }
            "--flakes-root" => {
                config.flakes_root = args.next().map(PathBuf::from)
                    .ok_or("--flakes-root needs value")?;
            }
            "--corpus" => {
                let v = args.next().ok_or("--corpus needs value")?;
                config.corpus = Corpus::from_str(&v)
                    .ok_or_else(|| format!("unknown --corpus value `{v}`"))?;
            }
            "--tag" => {
                config.include_tags.push(args.next().ok_or("--tag needs value")?);
            }
            "--skip-tag" => {
                config.exclude_tags.push(args.next().ok_or("--skip-tag needs value")?);
            }
            "--timeout-secs" => {
                let v = args.next().ok_or("--timeout-secs needs value")?;
                let secs: u64 = v.parse()
                    .map_err(|_| "--timeout-secs needs integer".to_string())?;
                config.timeout = Duration::from_secs(secs);
            }
            "--report" => {
                config.report_path = Some(args.next().map(PathBuf::from)
                    .ok_or("--report needs value")?);
            }
            "--no-report" => no_report = true,
            "--verbose" | "-v" => config.verbose = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            _ => config.explicit_flakes.push(PathBuf::from(arg)),
        }
    }
    if no_report {
        config.report_path = None;
    } else if config.report_path.is_none() {
        config.report_path = Some(sweep::default_report_path());
    }
    Ok(Args { config })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let report = sweep::run(&args.config)?;
    // Non-zero exit on any divergence so the operator's CI / wrapper
    // can react to drift.  Drift is drift whether or not we wrote the
    // JSON report.
    if report.all_pass() {
        Ok(())
    } else {
        std::process::exit(1);
    }
}
