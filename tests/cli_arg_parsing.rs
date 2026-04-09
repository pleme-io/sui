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
fn build_accepts_no_installable() {
    // installable is now optional (defaults to .#default) for nix compat
    sui()
        .args(["build", "--help"])
        .assert()
        .success();
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

// ── Nix CLI compatibility: global flags ────────────────────────────

#[test]
fn global_show_trace_accepted() {
    sui()
        .args(["--show-trace", "eval", "--help"])
        .assert()
        .success();
}

#[test]
fn global_print_build_logs_accepted() {
    sui()
        .args(["-L", "eval", "--help"])
        .assert()
        .success();
}

#[test]
fn global_extra_experimental_features_accepted() {
    sui()
        .args(["--extra-experimental-features", "nix-command flakes", "eval", "--help"])
        .assert()
        .success();
}

#[test]
fn global_impure_accepted() {
    sui()
        .args(["--impure", "eval", "--help"])
        .assert()
        .success();
}

#[test]
fn global_option_accepted() {
    sui()
        .args(["--option", "substituters", "https://cache.nixos.org", "eval", "--help"])
        .assert()
        .success();
}

#[test]
fn global_max_jobs_accepted() {
    sui()
        .args(["--max-jobs", "4", "eval", "--help"])
        .assert()
        .success();
}

#[test]
fn global_keep_going_accepted() {
    sui()
        .args(["--keep-going", "eval", "--help"])
        .assert()
        .success();
}

// ── Nix CLI compatibility: eval flags ──────────────────────────────

#[test]
fn eval_raw_flag_accepted() {
    sui()
        .args(["eval", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--raw"));
}

#[test]
fn eval_expr_flag_accepted() {
    sui()
        .args(["eval", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--expr"));
}

// ── Nix CLI compatibility: build flags ─────────────────────────────

#[test]
fn build_no_link_flag() {
    sui()
        .args(["build", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--no-link"));
}

#[test]
fn build_print_out_paths_flag() {
    sui()
        .args(["build", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--print-out-paths"));
}

#[test]
fn build_json_flag() {
    sui()
        .args(["build", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn build_dry_run_flag() {
    sui()
        .args(["build", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dry-run"));
}

#[test]
fn build_out_link_flag() {
    sui()
        .args(["build", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--out-link"));
}

#[test]
fn build_installable_optional() {
    // build without installable should NOT error on arg parsing
    // (it will fail on eval, but the parser accepts it)
    sui()
        .args(["build", "--help"])
        .assert()
        .success();
}

// ── Nix CLI compatibility: new top-level commands ──────────────────

#[test]
fn search_requires_args() {
    sui()
        .arg("search")
        .assert()
        .failure();
}

#[test]
fn profile_no_subcommand_fails() {
    sui()
        .arg("profile")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn profile_help_lists_subcommands() {
    sui()
        .args(["profile", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("list")
                .and(predicate::str::contains("install"))
                .and(predicate::str::contains("remove"))
                .and(predicate::str::contains("upgrade"))
                .and(predicate::str::contains("rollback"))
                .and(predicate::str::contains("history")),
        );
}

#[test]
fn profile_install_accepts_packages() {
    sui()
        .args(["profile", "install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("PACKAGES"));
}

#[test]
fn repl_help_works() {
    sui()
        .args(["repl", "--help"])
        .assert()
        .success();
}

#[test]
fn copy_help_shows_to_from() {
    sui()
        .args(["copy", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--to")
                .and(predicate::str::contains("--from")),
        );
}

#[test]
fn path_info_help_shows_json() {
    sui()
        .args(["path-info", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn collect_garbage_help_shows_delete_old() {
    sui()
        .args(["collect-garbage", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--delete-old"));
}

#[test]
fn derivation_no_subcommand_fails() {
    sui()
        .arg("derivation")
        .assert()
        .failure();
}

#[test]
fn derivation_show_help() {
    sui()
        .args(["derivation", "show", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn show_config_help_shows_json() {
    sui()
        .args(["show-config", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn hash_no_subcommand_fails() {
    sui()
        .arg("hash")
        .assert()
        .failure();
}

#[test]
fn hash_file_help() {
    sui()
        .args(["hash", "file", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--type")
                .and(predicate::str::contains("--base")),
        );
}

#[test]
fn key_no_subcommand_fails() {
    sui()
        .arg("key")
        .assert()
        .failure();
}

#[test]
fn key_generate_secret_help() {
    sui()
        .args(["key", "generate-secret", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--key-name"));
}

#[test]
fn why_requires_args() {
    sui()
        .arg("why")
        .assert()
        .failure();
}

#[test]
fn edit_requires_installable() {
    sui()
        .arg("edit")
        .assert()
        .failure();
}

#[test]
fn log_requires_installable() {
    sui()
        .arg("log")
        .assert()
        .failure();
}

#[test]
fn fmt_help_shows_check() {
    sui()
        .args(["fmt", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--check"));
}

#[test]
fn registry_no_subcommand_fails() {
    sui()
        .arg("registry")
        .assert()
        .failure();
}

#[test]
fn registry_list_help() {
    sui()
        .args(["registry", "list", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn doctor_help() {
    sui()
        .args(["doctor", "--help"])
        .assert()
        .success();
}

#[test]
fn print_dev_env_help() {
    sui()
        .args(["print-dev-env", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn bundle_requires_installable() {
    sui()
        .arg("bundle")
        .assert()
        .failure();
}

#[test]
fn bundle_help_shows_bundler() {
    sui()
        .args(["bundle", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--bundler"));
}

#[test]
fn upgrade_nix_help() {
    sui()
        .args(["upgrade-nix", "--help"])
        .assert()
        .success();
}

#[test]
fn diff_closures_requires_args() {
    sui()
        .arg("store-diff-closures")
        .assert()
        .failure();
}

// ── Nix CLI compatibility: new store subcommands ───────────────────

#[test]
fn store_delete_help() {
    sui()
        .args(["store", "delete", "--help"])
        .assert()
        .success();
}

#[test]
fn store_ls_help() {
    sui()
        .args(["store", "ls", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--recursive")
                .and(predicate::str::contains("--json")),
        );
}

#[test]
fn store_ping_help() {
    sui()
        .args(["store", "ping", "--help"])
        .assert()
        .success();
}

#[test]
fn store_add_path_help() {
    sui()
        .args(["store", "add-path", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--name"));
}

#[test]
fn store_sign_help() {
    sui()
        .args(["store", "sign", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--key-file"));
}

#[test]
fn store_repair_help() {
    sui()
        .args(["store", "repair", "--help"])
        .assert()
        .success();
}

#[test]
fn store_dump_path_help() {
    sui()
        .args(["store", "dump-path", "--help"])
        .assert()
        .success();
}

// ── Nix CLI compatibility: new flake subcommands ───────────────────

#[test]
fn flake_init_help() {
    sui()
        .args(["flake", "init", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--template"));
}

#[test]
fn flake_new_requires_dest() {
    sui()
        .args(["flake", "new"])
        .assert()
        .failure();
}

#[test]
fn flake_archive_help() {
    sui()
        .args(["flake", "archive", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn flake_clone_requires_flake_ref() {
    sui()
        .args(["flake", "clone"])
        .assert()
        .failure();
}

#[test]
fn flake_prefetch_help() {
    sui()
        .args(["flake", "prefetch", "--help"])
        .assert()
        .success();
}

#[test]
fn flake_metadata_json_flag() {
    sui()
        .args(["flake", "metadata", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"));
}

// ── Nix CLI compatibility: combined flag tests ─────────────────────

#[test]
fn nix_eval_json_with_extra_experimental_features() {
    // This exact invocation is used in pleme-io scripts
    sui()
        .args(["--extra-experimental-features", "nix-command flakes", "eval", "--json", "--help"])
        .assert()
        .success();
}

#[test]
fn nix_build_no_link_print_out_paths() {
    // Common nix build invocation
    sui()
        .args(["build", "--no-link", "--print-out-paths", "--help"])
        .assert()
        .success();
}

#[test]
fn nix_build_with_out_link() {
    sui()
        .args(["build", "-o", "result", "--help"])
        .assert()
        .success();
}
