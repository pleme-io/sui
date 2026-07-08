//! `bigorna` — keyway CLI for the typed buildx-native-multi-arch primitive.
//!
//! Shape: typed config (YAML/JSON) in, typed JSON receipt out, exit `0`
//! (success) / `1` (runtime failure — a docker spawn error or a non-zero buildx
//! step) / `2` (usage/config error). Every `docker buildx …` call is issued by
//! the typed [`sui_bigorna`] driver through the real [`RealCommandRunner`] — the
//! CLI itself never builds an argv string or shells out directly.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sui_bigorna::{
    build, inspect, plan, setup, teardown, BigornaConfig, BuildOutcome, BuildSpec,
};
use sui_dockerfile_wrapper::RealCommandRunner;

/// Exit codes (keyway convention).
const EXIT_OK: u8 = 0;
const EXIT_RUNTIME: u8 = 1;
const EXIT_USAGE: u8 = 2;

#[derive(Debug, Parser)]
#[command(
    name = "bigorna",
    about = "Set up a buildx builder with native per-arch nodes (no QEMU) + a cache front",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Render the argv `setup` would run — shadow-first, mutates nothing.
    Plan {
        /// Path to a [`BigornaConfig`] (`.yaml`/`.yml`/`.json`).
        #[arg(long)]
        config: PathBuf,
    },
    /// Create the builder + register its native per-arch nodes.
    Setup {
        #[arg(long)]
        config: PathBuf,
    },
    /// Inspect a builder and report its running nodes + platforms.
    Inspect {
        #[arg(long)]
        builder: String,
        /// Pass `--bootstrap` (start `BuildKit` before inspecting).
        #[arg(long, default_value_t = true)]
        bootstrap: bool,
    },
    /// Remove a builder (`docker buildx rm`).
    Teardown {
        #[arg(long)]
        builder: String,
    },
    /// Drive a `docker buildx build` for a [`BuildSpec`].
    Build {
        /// Path to a [`BuildSpec`] (`.yaml`/`.yml`/`.json`).
        #[arg(long)]
        config: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Plan { config } => run_plan(&config),
        Command::Setup { config } => run_setup(&config),
        Command::Inspect { builder, bootstrap } => run_inspect(&builder, bootstrap),
        Command::Teardown { builder } => run_teardown(&builder),
        Command::Build { config } => run_build(&config),
    };
    ExitCode::from(code)
}

/// Load a typed config from a `.yaml`/`.yml` (`serde_yaml_ng`) or `.json`
/// (`serde_json`) file.
fn load<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        let mut m = String::from("reading ");
        m.push_str(&path.display().to_string());
        m.push_str(": ");
        m.push_str(&e.to_string());
        m
    })?;
    let is_yaml = matches!(path.extension().and_then(|e| e.to_str()), Some("yaml" | "yml"));
    if is_yaml {
        serde_yaml_ng::from_str(&raw).map_err(|e| e.to_string())
    } else {
        serde_json::from_str(&raw).map_err(|e| e.to_string())
    }
}

/// Print a receipt as pretty JSON to stdout. Returns `EXIT_RUNTIME` if the
/// value somehow fails to serialize (never expected for owned types).
fn emit<T: Serialize>(value: &T) -> u8 {
    match serde_json::to_string_pretty(value) {
        Ok(json) => {
            println!("{json}");
            EXIT_OK
        }
        Err(e) => {
            eprintln!("error serializing receipt: {e}");
            EXIT_RUNTIME
        }
    }
}

fn run_plan(config: &Path) -> u8 {
    let cfg: BigornaConfig = match load(config) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    match plan(cfg) {
        Ok(receipt) => emit(&receipt),
        Err(e) => usage_error(&e.to_string()),
    }
}

fn run_setup(config: &Path) -> u8 {
    let cfg: BigornaConfig = match load(config) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    match setup(cfg, &RealCommandRunner) {
        Ok(receipt) => {
            let ok = receipt.ok;
            let code = emit(&receipt);
            if code != EXIT_OK {
                code
            } else if ok {
                EXIT_OK
            } else {
                EXIT_RUNTIME
            }
        }
        Err(e) => runtime_error(&e.to_string()),
    }
}

fn run_inspect(builder: &str, bootstrap: bool) -> u8 {
    match inspect(builder, bootstrap, &RealCommandRunner) {
        Ok(report) => emit(&report),
        Err(e) => runtime_error(&e.to_string()),
    }
}

fn run_teardown(builder: &str) -> u8 {
    match teardown(builder, &RealCommandRunner) {
        Ok(step) => {
            let ok = step.success;
            let code = emit(&step);
            if code != EXIT_OK {
                code
            } else if ok {
                EXIT_OK
            } else {
                EXIT_RUNTIME
            }
        }
        Err(e) => runtime_error(&e.to_string()),
    }
}

fn run_build(config: &Path) -> u8 {
    let spec: BuildSpec = match load(config) {
        Ok(s) => s,
        Err(e) => return usage_error(&e),
    };
    match build(&spec, &RealCommandRunner) {
        Ok(receipt) => {
            let failed = matches!(receipt.outcome, BuildOutcome::Failed { .. });
            let code = emit(&receipt);
            if code != EXIT_OK {
                code
            } else if failed {
                EXIT_RUNTIME
            } else {
                EXIT_OK
            }
        }
        Err(e) => runtime_error(&e.to_string()),
    }
}

fn usage_error(msg: &str) -> u8 {
    eprintln!("config error: {msg}");
    EXIT_USAGE
}

fn runtime_error(msg: &str) -> u8 {
    eprintln!("error: {msg}");
    EXIT_RUNTIME
}
