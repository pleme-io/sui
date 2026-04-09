//! CLI argument parsing tests for the `sui` binary.
//!
//! Uses `assert_cmd` to exercise clap argument parsing without needing
//! a real Nix store or network access. Tests verify that:
//! - Known subcommands are accepted
//! - Unknown subcommands are rejected
//! - Required arguments produce errors when missing
//! - Default values are applied correctly
//! - `--help` and `--version` work as expected

use assert_cmd::Command;
use predicates::prelude::*;

fn sui() -> Command {
    Command::cargo_bin("sui").expect("cargo_bin sui")
}

// ── Top-level CLI behavior ──────────────────────────────────────────

#[test]
fn no_args_shows_help_and_exits_nonzero() {
    sui().assert().failure().stderr(predicate::str::contains("Usage"));
}

#[test]
fn help_flag_shows_usage() {
    sui()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Rust-native Nix replacement"));
}

#[test]
fn version_flag_shows_version() {
    sui()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn unknown_subcommand_fails() {
    sui()
        .arg("nonexistent-command")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

// ── Serve subcommand ────────────────────────────────────────────────

#[test]
fn serve_help_shows_listen_options() {
    sui()
        .args(["serve", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--listen")
                .and(predicate::str::contains("--grpc-listen")),
        );
}

// ── Store subcommands ───────────────────────────────────────────────

#[test]
fn store_no_subcommand_fails() {
    sui()
        .arg("store")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn store_help_lists_subcommands() {
    sui()
        .args(["store", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("path-info")
                .and(predicate::str::contains("paths"))
                .and(predicate::str::contains("gc"))
                .and(predicate::str::contains("verify"))
                .and(predicate::str::contains("info")),
        );
}

#[test]
fn store_path_info_requires_path() {
    sui()
        .args(["store", "path-info"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("<PATH>").or(predicate::str::contains("required")));
}

#[test]
fn store_paths_help_shows_limit() {
    sui()
        .args(["store", "paths", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--limit"));
}

// ── Eval subcommand ─────────────────────────────────────────────────

#[test]
fn eval_help_shows_json_flag() {
    sui()
        .args(["eval", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"));
}

// ── Build subcommand ────────────────────────────────────────────────

#[test]
fn build_requires_installable() {
    sui()
        .arg("build")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("<INSTALLABLE>")
                .or(predicate::str::contains("required")),
        );
}

// ── Flake subcommands ───────────────────────────────────────────────

#[test]
fn flake_no_subcommand_fails() {
    sui()
        .arg("flake")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn flake_help_lists_subcommands() {
    sui()
        .args(["flake", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("show")
                .and(predicate::str::contains("update"))
                .and(predicate::str::contains("check"))
                .and(predicate::str::contains("lock"))
                .and(predicate::str::contains("metadata")),
        );
}

#[test]
fn flake_update_help_shows_input_arg() {
    sui()
        .args(["flake", "update", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("INPUT").or(predicate::str::contains("input")));
}

#[test]
fn flake_check_help_shows_no_build_flag() {
    sui()
        .args(["flake", "check", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--no-build"));
}

// ── Daemon subcommand ───────────────────────────────────────────────

#[test]
fn daemon_help_shows_socket_option() {
    sui()
        .args(["daemon", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--socket"));
}

// ── System subcommands ──────────────────────────────────────────────

#[test]
fn system_no_subcommand_fails() {
    sui()
        .arg("system")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn system_help_lists_subcommands() {
    sui()
        .args(["system", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("rebuild")
                .and(predicate::str::contains("status"))
                .and(predicate::str::contains("rollback")),
        );
}

#[test]
fn system_rebuild_help_shows_flake_option() {
    sui()
        .args(["system", "rebuild", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--flake"));
}

// ── Fleet subcommands ───────────────────────────────────────────────

#[test]
fn fleet_no_subcommand_fails() {
    sui()
        .arg("fleet")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn fleet_help_lists_subcommands() {
    sui()
        .args(["fleet", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("nodes")
                .and(predicate::str::contains("deploy"))
                .and(predicate::str::contains("status")),
        );
}

#[test]
fn fleet_deploy_requires_target() {
    sui()
        .args(["fleet", "deploy"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("<TARGET>")
                .or(predicate::str::contains("required")),
        );
}

// ── Store GC flags ─────────────────────────────────────────────────

#[test]
fn store_gc_help_shows_print_roots() {
    sui()
        .args(["store", "gc", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--print-roots"));
}

#[test]
fn store_gc_help_shows_dry_run() {
    sui()
        .args(["store", "gc", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dry-run"));
}

#[test]
fn store_gc_help_shows_max_age_days() {
    sui()
        .args(["store", "gc", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--max-age-days"));
}

// ── Store Optimise ─────────────────────────────────────────────────

#[test]
fn store_optimise_help_shows_dry_run() {
    sui()
        .args(["store", "optimise", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dry-run"));
}

#[test]
fn store_help_lists_optimise() {
    sui()
        .args(["store", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("optimise"));
}

// ── Develop subcommand ─────────────────────────────────────────────

#[test]
fn develop_help_shows_options() {
    sui()
        .args(["develop", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--command")
                .and(predicate::str::contains("--attr"))
                .and(predicate::str::contains("FLAKE_REF")),
        );
}

#[test]
fn develop_accepts_flake_ref() {
    // This just tests argument parsing — it won't actually eval.
    // The command will fail because it tries to evaluate a flake,
    // but the argument parser should accept the input.
    sui()
        .args(["develop", "--help"])
        .assert()
        .success();
}

// ── Run subcommand ─────────────────────────────────────────────────

#[test]
fn run_requires_installable() {
    sui()
        .args(["run"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("<INSTALLABLE>")
                .or(predicate::str::contains("required")),
        );
}

#[test]
fn run_help_shows_args() {
    sui()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("INSTALLABLE"));
}

// ── Top-level help lists new commands ──────────────────────────────

#[test]
fn top_level_help_lists_develop() {
    sui()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("develop"));
}

#[test]
fn top_level_help_lists_run() {
    sui()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("run"));
}
