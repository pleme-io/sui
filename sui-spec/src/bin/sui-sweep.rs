//! `sui-sweep` — differential parity runner.
//!
//! Reads the canonical probe corpus from
//! `sui-spec/specs/parity_probes.lisp`, walks a list of flakes,
//! runs every probe through both `sui eval` and `nix eval`, and
//! classifies each result according to the probe's `classify`.
//!
//! Usage:
//!
//! ```
//! sui-sweep [--sui <path>] [--nix <path>] [--flakes-root <dir>]
//!           [--tag <tag>]... [--timeout-secs <n>] [<flake>...]
//! ```
//!
//! If no flake arguments are supplied, `--flakes-root` is walked
//! (default `~/code/github/pleme-io/`) for every `flake.nix`.
//! If one or more `--tag <tag>` flags are supplied, only probes
//! that carry at least one of those tags are run.
//!
//! Output: a line per probe/flake with the classify verdict, and
//! a final summary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use sui_spec::probe::{Classify, Probe};

struct Args {
    sui: PathBuf,
    nix: PathBuf,
    flakes_root: PathBuf,
    tags: Vec<String>,
    timeout_secs: u64,
    explicit_flakes: Vec<PathBuf>,
    verbose: bool,
}

impl Args {
    fn parse() -> Self {
        let mut args = std::env::args().skip(1);
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let default_sui = std::env::current_dir()
            .map(|p| p.join("target/release/sui"))
            .unwrap_or_else(|_| PathBuf::from("sui"));
        let mut out = Args {
            sui: default_sui,
            nix: PathBuf::from("nix"),
            flakes_root: PathBuf::from(format!("{home}/code/github/pleme-io")),
            tags: Vec::new(),
            timeout_secs: 30,
            explicit_flakes: Vec::new(),
            verbose: false,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--sui"          => out.sui = args.next().map(PathBuf::from).expect("--sui needs value"),
                "--nix"          => out.nix = args.next().map(PathBuf::from).expect("--nix needs value"),
                "--flakes-root"  => out.flakes_root = args.next().map(PathBuf::from).expect("--flakes-root needs value"),
                "--tag"          => out.tags.push(args.next().expect("--tag needs value")),
                "--timeout-secs" => out.timeout_secs = args.next().and_then(|s| s.parse().ok()).expect("--timeout-secs needs integer"),
                "--verbose" | "-v" => out.verbose = true,
                "-h" | "--help"  => { print_help(); std::process::exit(0); }
                _                => out.explicit_flakes.push(PathBuf::from(arg)),
            }
        }
        out
    }
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
         --tag <tag>           Only run probes carrying <tag>.  May repeat.\n  \
         --timeout-secs <n>    Per-probe timeout (default: 30)\n  \
         --verbose / -v        Print per-probe diagnostics\n\n\
         Probe corpus is compiled from sui-spec/specs/parity_probes.lisp."
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Verdict {
    Match,
    Differ,
    SuiFailOnly,
    NixFailOnly,
    BothFail,
}

impl Verdict {
    fn glyph(self) -> char {
        match self {
            Verdict::Match       => '.',
            Verdict::Differ      => 'D',
            Verdict::SuiFailOnly => 'S',
            Verdict::NixFailOnly => 'N',
            Verdict::BothFail    => '?',
        }
    }
}

fn find_flakes(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(root) else { return out; };
    for entry in read.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let flake_nix = p.join("flake.nix");
            if flake_nix.exists() {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn run_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
) -> std::io::Result<(std::process::ExitStatus, String)> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn()?;
    // Simplest portable timeout — we spawn a watchdog thread that
    // kills the child after `timeout`.  For the sweep this is fine
    // because we don't run many concurrent probes.
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if rx.recv_timeout(timeout).is_err() {
            // Timeout — send SIGKILL via libc.
            #[cfg(unix)]
            unsafe { libc::kill(pid as i32, libc::SIGKILL); }
        }
    });
    let output = child.wait_with_output()?;
    let _ = tx.send(());  // child already exited; signal watchdog to stop.
    let combined = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    Ok((output.status, combined))
}

fn run_sui(
    sui: &Path,
    expr: &str,
    timeout: Duration,
) -> (bool, String) {
    let mut cmd = Command::new(sui);
    cmd.args(["eval", "--json", expr]);
    match run_with_timeout(&mut cmd, timeout) {
        Ok((status, body)) => (status.success(), body.trim().to_string()),
        Err(e) => (false, format!("spawn failed: {e}")),
    }
}

fn run_nix(
    nix: &Path,
    expr: &str,
    timeout: Duration,
) -> (bool, String) {
    let mut cmd = Command::new(nix);
    cmd.args([
        "eval", "--impure", "--json",
        "--extra-experimental-features", "nix-command flakes",
        "--expr", expr,
    ]);
    match run_with_timeout(&mut cmd, timeout) {
        Ok((status, body)) => (status.success(), body.trim().to_string()),
        Err(e) => (false, format!("spawn failed: {e}")),
    }
}

fn classify(probe: &Probe, sui_ok: bool, sui_body: &str, nix_ok: bool, nix_body: &str) -> Verdict {
    match (sui_ok, nix_ok) {
        (true, true) => {
            let equal = match probe.classify {
                Classify::JsonEqual => sui_body == nix_body,
                Classify::AttrNamesEqual => {
                    let sui_v: Option<Vec<String>> = serde_json::from_str(sui_body).ok();
                    let nix_v: Option<Vec<String>> = serde_json::from_str(nix_body).ok();
                    match (sui_v, nix_v) {
                        (Some(mut a), Some(mut b)) => {
                            a.sort();
                            b.sort();
                            a == b
                        }
                        _ => false,
                    }
                }
                Classify::BothAreStorePaths => {
                    let is_sp = |s: &str| s.trim_matches('"').starts_with("/nix/store/");
                    is_sp(sui_body) && is_sp(nix_body)
                }
            };
            if equal { Verdict::Match } else { Verdict::Differ }
        }
        (false, true) => Verdict::SuiFailOnly,
        (true, false) => Verdict::NixFailOnly,
        (false, false) => Verdict::BothFail,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let probes = sui_spec::probe::load_canonical()?;

    let selected: Vec<&Probe> = if args.tags.is_empty() {
        probes.iter().collect()
    } else {
        probes.iter()
            .filter(|p| p.tags.iter().any(|t| args.tags.contains(t)))
            .collect()
    };

    let flakes = if args.explicit_flakes.is_empty() {
        find_flakes(&args.flakes_root)
    } else {
        args.explicit_flakes.clone()
    };

    eprintln!(
        "sui-sweep: {} probes × {} flakes ({} probe×flake combos)\n",
        selected.len(),
        flakes.len(),
        selected.len() * flakes.len()
    );

    let timeout = Duration::from_secs(args.timeout_secs);
    let mut tallies: BTreeMap<Verdict, usize> = BTreeMap::new();
    let mut differs: Vec<(String, String, String, String)> = Vec::new();

    for flake in &flakes {
        let flake_str = flake.to_string_lossy().to_string();
        for probe in &selected {
            let expr = probe.expr.replace("$FLAKE", &flake_str);
            let (sui_ok, sui_body) = run_sui(&args.sui, &expr, timeout);
            let (nix_ok, nix_body) = run_nix(&args.nix, &expr, timeout);
            let verdict = classify(probe, sui_ok, &sui_body, nix_ok, &nix_body);
            *tallies.entry(verdict).or_default() += 1;
            if matches!(verdict, Verdict::Differ | Verdict::SuiFailOnly) {
                differs.push((
                    flake.file_name().map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| flake_str.clone()),
                    probe.name.clone(),
                    sui_body.clone(),
                    nix_body.clone(),
                ));
            }
            eprint!("{}", verdict.glyph());
            if args.verbose {
                eprintln!(" {} :: {}", probe.name, flake.display());
            }
        }
        eprintln!(" {}", flake.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
    }

    println!("\n── tallies ──");
    for (v, n) in &tallies {
        println!("{:3}  {:?}", n, v);
    }
    if !differs.is_empty() {
        println!("\n── first 10 disagreements ──");
        for (flake, probe, sui, nix) in differs.iter().take(10) {
            println!("{flake} :: {probe}");
            println!("  sui: {}", truncate(sui, 120));
            println!("  nix: {}", truncate(nix, 120));
        }
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() }
    else { format!("{}...", &s[..max]) }
}
