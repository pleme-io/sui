//! `canteiro-emit-gha` — generate the canteiro plane-(a) multi-worker crux
//! workflow (theory/CANTEIRO.md §5).
//!
//! Builds the REAL 2-node sui `CiRun` — `build` (`cargo build -p
//! sui-supercacheci`) then `test` (`cargo test -p sui-supercacheci canteiro`,
//! depending on `build`), both `env=None` — projects it through
//! [`emit_gha_yaml`], and writes the result to
//! `.github/workflows/canteiro-crux.yml`. The committed workflow IS this
//! binary's output (persisted-spec discipline: a hand-edit is a drift the
//! `emit_gha_yaml` round-trip test catches).
//!
//! Run from the sui repo root: `cargo run -p sui-supercacheci --bin canteiro-emit-gha`.
//!
//! HONEST SCOPE (plane (a)): canteiro DERIVES the job graph; GitHub Actions
//! schedules the jobs onto separate ARC workers. canteiro is the DAG source +
//! emitter, NOT the cross-worker runtime scheduler.

use std::path::Path;
use std::process::ExitCode;

use sui_supercacheci::canteiro::{ActionRef, CiNode, CiRun, EnvClass};
use sui_supercacheci::canteiro_gha::emit_gha_yaml;

/// The path the emitted workflow is written to, relative to the sui repo root.
const OUT_PATH: &str = ".github/workflows/canteiro-crux.yml";

/// The real sui crux run: `build` → `test`, both `env=None`, REAL cargo actions.
fn sui_crux_run() -> CiRun {
    let build = CiNode::new(
        "build",
        EnvClass::None,
        ActionRef {
            name: "build".to_string(),
            command: "cargo".to_string(),
            args: vec![
                "build".to_string(),
                "-p".to_string(),
                "sui-supercacheci".to_string(),
            ],
        },
        vec![],
    );
    let test = CiNode::new(
        "test",
        EnvClass::None,
        ActionRef {
            name: "test".to_string(),
            command: "cargo".to_string(),
            args: vec![
                "test".to_string(),
                "-p".to_string(),
                "sui-supercacheci".to_string(),
                "canteiro".to_string(),
            ],
        },
        vec!["build".to_string()],
    );
    CiRun {
        workspace: "pleme-io".to_string(),
        repo: "sui".to_string(),
        nodes: vec![build, test],
    }
}

fn main() -> ExitCode {
    let run = sui_crux_run();
    let yaml = match emit_gha_yaml(&run) {
        Ok(y) => y,
        Err(e) => {
            eprintln!("canteiro-emit-gha: emit failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std::fs::write(Path::new(OUT_PATH), &yaml) {
        eprintln!("canteiro-emit-gha: writing {OUT_PATH} failed: {e}");
        return ExitCode::FAILURE;
    }
    println!("canteiro-emit-gha: wrote {} ({} bytes)", OUT_PATH, yaml.len());
    ExitCode::SUCCESS
}
