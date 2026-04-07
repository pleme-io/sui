//! Layer 12: Categorized flake evaluation by complexity tier.
//!
//! Tests repos grouped by structural complexity. Each tier has
//! progressively looser assertions:
//!
//! - **Tier A** (zero/minimal inputs): highest pass rate expected
//! - **Tier B** (simple Rust tools): high pass rate
//! - **Tier C** (Rust workspaces): moderate pass rate
//! - **Tier D** (Nix modules): lower pass rate (complex module system)
//! - **Tier E** (system configs): discovery only (no assertions)
//!
//! Gated on `SUI_TEST_ONLINE=1`.

mod common;

use std::path::Path;

// ── Shared helpers ──────────────────────────────────────────────────────

/// Try to evaluate a flake directory; return Ok(key_list) or Err(message).
fn eval_flake_keys(dir: &Path) -> Result<Vec<String>, String> {
    match sui_eval::builtins::evaluate_flake(dir) {
        Ok(v) => match v {
            sui_eval::Value::Attrs(ref attrs) => {
                Ok(attrs.keys().cloned().collect())
            }
            _ => Ok(vec![]), // non-attrs result
        },
        Err(e) => Err(format!("{e}")),
    }
}

/// Run a tier test: evaluate each repo, collect failures, print report.
/// Returns (passed, failures) for assertion.
fn run_tier(
    tier_name: &str,
    repo_names: &[&str],
    required_keys: &[&str],
) -> (usize, Vec<(String, String)>) {
    let root = common::pleme_io_root();
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut skipped = 0usize;
    let mut passed = 0usize;

    for &name in repo_names {
        let dir = root.join(name);
        if !dir.join("flake.nix").exists() {
            skipped += 1;
            continue;
        }
        match eval_flake_keys(&dir) {
            Ok(keys) => {
                // Check that at least one required key is present (if specified).
                if !required_keys.is_empty()
                    && !required_keys.iter().any(|k| keys.contains(&k.to_string()))
                {
                    failures.push((
                        name.to_string(),
                        format!(
                            "missing required output keys (have: {}; want one of: {})",
                            keys.join(", "),
                            required_keys.join(", "),
                        ),
                    ));
                } else {
                    passed += 1;
                }
            }
            Err(e) => failures.push((name.to_string(), e)),
        }
    }

    let attempted = repo_names.len() - skipped;
    println!("\n=== {tier_name} ===");
    println!(
        "{passed}/{attempted} passed ({skipped} skipped -- no flake.nix)"
    );
    for (name, err) in &failures {
        let short = if err.len() > 120 { &err[..120] } else { err };
        println!("  FAIL {name}: {short}");
    }

    (passed, failures)
}

// ── Tier A: Zero / minimal inputs ───────────────────────────────────────

/// Repos with no or trivial flake inputs (may not even have a flake.lock).
/// These should have the highest eval success rate.
#[test]
fn tier_a_minimal_inputs() {
    if common::skip_if_offline("tier_a") {
        return;
    }

    // Repos known to have zero or very few inputs.
    let repos = [
        "irodori",
        "hasami",
        "tsuuchi",
        "awase",
        "soushi",
        "tsunagu",
        "mojiban",
        "todoku",
        "shikumi",
        "hayai",
        "meimei",
        "sekkei",
        "takumi",
        "kaname",
    ];

    let (_passed, failures) = run_tier(
        "Tier A: Minimal Inputs (shared Rust libraries)",
        &repos,
        &[], // no required keys -- just eval success
    );

    // Tier A: very strict -- at most 3 failures allowed.
    assert!(
        failures.len() <= 3,
        "Tier A: too many failures ({}/{}): {:?}",
        failures.len(),
        repos.len(),
        failures
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
    );
}

// ── Tier B: Simple Rust tools ───────────────────────────────────────────

/// Single-crate Rust CLI tools built via substrate's rust-tool-release or
/// rust-binary builders. Should produce packages and/or overlays.
#[test]
fn tier_b_simple_rust_tools() {
    if common::skip_if_offline("tier_b") {
        return;
    }

    let repos = [
        "tend",
        "codesearch",
        "zoekt-mcp",
        "umbra",
        "amimori",
        "kurage",
        "blx",
        "skim-tab",
        "bm-complete",
        "seibi",
        "kontena",
        "kindling",
        "skill-lint",
        "workspace-config",
        "guardrail",
        "akeyless-matrix",
        "akeyless-nix",
        "slack-forge",
        "kikai",
        "shihaisha",
    ];

    let (_passed, failures) = run_tier(
        "Tier B: Simple Rust Tools",
        &repos,
        &["packages", "overlays", "devShells"],
    );

    // Tier B: strict -- at most 4 failures.
    assert!(
        failures.len() <= 4,
        "Tier B: too many failures ({}/{}): {:?}",
        failures.len(),
        repos.len(),
        failures
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
    );
}

// ── Tier C: Rust workspaces ─────────────────────────────────────────────

/// Multi-crate Rust workspaces built via substrate's
/// rust-workspace-release or custom builders.
#[test]
fn tier_c_rust_workspaces() {
    if common::skip_if_offline("tier_c") {
        return;
    }

    let repos = [
        "mamorigami",
        "nexus",
        "mado",
        "sui",
        "maboroshi",
        "kagerou",
        "kakureyado",
        "kurayami",
        "kagami",
        "kagemusha",
        "iac-forge",
        "iac-forge-cli",
        "forge-gen",
        "mcp-forge",
    ];

    let (_passed, failures) = run_tier(
        "Tier C: Rust Workspaces",
        &repos,
        &["packages", "overlays", "devShells"],
    );

    // Tier C: moderate -- at most half can fail.
    let max_fail = (repos.len() + 1) / 2;
    assert!(
        failures.len() <= max_fail,
        "Tier C: too many failures ({}/{}): {:?}",
        failures.len(),
        repos.len(),
        failures
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
    );
}

// ── Tier D: Nix modules ─────────────────────────────────────────────────

/// Pure Nix module repos (blackmatter ecosystem). These are complex and
/// may exercise deep module-system evaluation. Looser assertions.
#[test]
fn tier_d_nix_modules() {
    if common::skip_if_offline("tier_d") {
        return;
    }

    let repos = [
        "blackmatter",
        "blackmatter-shell",
        "blackmatter-nvim",
        "blackmatter-desktop",
        "blackmatter-claude",
        "blackmatter-kubernetes",
        "blackmatter-security",
        "blackmatter-pleme",
        "blackmatter-akeyless",
        "blackmatter-atlassian",
        "blackmatter-anvil",
        "blackmatter-cursor",
        "blackmatter-movie",
        "blackmatter-ayatsuri",
        "blackmatter-ghostty",
        "blackmatter-macos",
        "substrate",
        "kindling-profiles",
    ];

    let (_passed, failures) = run_tier(
        "Tier D: Nix Modules (blackmatter + infrastructure)",
        &repos,
        &[], // don't require specific keys -- module repos vary
    );

    // Tier D: lenient -- we just want some to work. Allow up to 75% failure.
    let max_fail = (repos.len() * 3 + 3) / 4;
    assert!(
        failures.len() <= max_fail,
        "Tier D: too many failures ({}/{}): {:?}",
        failures.len(),
        repos.len(),
        failures
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
    );
}

// ── Tier E: System configs ──────────────────────────────────────────────

/// Full system configurations (darwinConfigurations, nixosConfigurations).
/// These pull in massive dependency trees and are the hardest to evaluate.
/// Discovery only -- no assertions.
#[test]
fn tier_e_system_configs() {
    if common::skip_if_offline("tier_e") {
        return;
    }

    let root = common::pleme_io_root();
    let nix_dir = root.join("nix");

    println!("\n=== Tier E: System Configurations ===");

    if !nix_dir.join("flake.nix").exists() {
        println!("  SKIP: nix repo not found at {}", nix_dir.display());
        return;
    }

    // Try evaluating the nix flake (the entire repo).
    match sui_eval::builtins::evaluate_flake(&nix_dir) {
        Ok(v) => {
            if let sui_eval::Value::Attrs(ref attrs) = v {
                let keys: Vec<&String> = attrs.keys().collect();
                println!(
                    "  PASS: nix flake evaluated ({} top-level keys: {})",
                    keys.len(),
                    keys.iter()
                        .take(10)
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            } else {
                println!("  PASS: nix flake evaluated (non-attrs result)");
            }
        }
        Err(e) => {
            let msg = format!("{e}");
            let short = if msg.len() > 200 { &msg[..200] } else { &msg };
            println!("  FAIL: nix flake: {short}");
        }
    }

    // Also try k8s repo if present.
    let k8s_dir = root.join("k8s");
    if k8s_dir.join("flake.nix").exists() {
        match sui_eval::builtins::evaluate_flake(&k8s_dir) {
            Ok(_) => println!("  PASS: k8s flake evaluated"),
            Err(e) => {
                let msg = format!("{e}");
                let short = if msg.len() > 200 { &msg[..200] } else { &msg };
                println!("  FAIL: k8s flake: {short}");
            }
        }
    }
}

// ── Tier cross-check: GPU apps ──────────────────────────────────────────

/// GPU application repos. These depend on garasu/egaku/shikumi etc.
/// and use the single-crate or workspace builder. Moderate expectations.
#[test]
fn tier_gpu_apps() {
    if common::skip_if_offline("tier_gpu_apps") {
        return;
    }

    let repos = [
        "mado",
        "hibiki",
        "kagi",
        "kekkai",
        "nami",
        "namimado",
        "tobira",
        "hikyaku",
        "ayatsuri",
        "fumi",
        "tanken",
        "myaku",
        "hikki",
        "shashin",
        "koyomiban",
        "shirase",
    ];

    let (_passed, failures) = run_tier(
        "Tier GPU: GPU Applications",
        &repos,
        &["packages", "overlays", "homeManagerModules"],
    );

    // GPU apps: moderate -- at most 60% can fail.
    let max_fail = (repos.len() * 3 + 4) / 5;
    assert!(
        failures.len() <= max_fail,
        "Tier GPU: too many failures ({}/{}): {:?}",
        failures.len(),
        repos.len(),
        failures
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
    );
}

// ── Tier cross-check: IaC forge pipeline ────────────────────────────────

/// IaC forge repos (iac-forge, terraform-forge, etc.).
#[test]
fn tier_iac_forge() {
    if common::skip_if_offline("tier_iac_forge") {
        return;
    }

    let repos = [
        "iac-forge",
        "terraform-forge",
        "pulumi-forge",
        "crossplane-forge",
        "ansible-forge",
        "pangea-forge",
        "steampipe-forge",
        "iac-forge-cli",
        "mcp-forge",
        "completion-forge",
        "forge-gen",
        "openapi-forge",
    ];

    let (_passed, failures) = run_tier(
        "Tier IaC: Forge Pipeline",
        &repos,
        &["packages", "overlays", "devShells"],
    );

    // IaC: moderate -- at most half can fail.
    let max_fail = (repos.len() + 1) / 2;
    assert!(
        failures.len() <= max_fail,
        "Tier IaC: too many failures ({}/{}): {:?}",
        failures.len(),
        repos.len(),
        failures
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
    );
}

// ── Tier cross-check: Privacy suite ─────────────────────────────────────

/// Privacy connectivity suite repos.
#[test]
fn tier_privacy_suite() {
    if common::skip_if_offline("tier_privacy") {
        return;
    }

    let repos = [
        "kakuremino",
        "maboroshi",
        "kurayami",
        "kagerou",
        "kakureyado",
        "kagami",
        "kagemusha",
        "mamorigami",
    ];

    let (_passed, failures) = run_tier(
        "Tier Privacy: Connectivity Suite",
        &repos,
        &["packages", "overlays"],
    );

    // Privacy: moderate -- at most half can fail.
    let max_fail = (repos.len() + 1) / 2;
    assert!(
        failures.len() <= max_fail,
        "Tier Privacy: too many failures ({}/{}): {:?}",
        failures.len(),
        repos.len(),
        failures
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
    );
}

// ── Tier cross-check: Server apps ───────────────────────────────────────

/// Server application repos (hiroba, taimen).
#[test]
fn tier_server_apps() {
    if common::skip_if_offline("tier_server_apps") {
        return;
    }

    let repos = ["hiroba", "taimen"];

    let (_passed, failures) = run_tier(
        "Tier Server: Server Applications",
        &repos,
        &["packages", "overlays"],
    );

    // Server: lenient -- both can fail.
    println!(
        "  ({} of {} passed)",
        repos.len() - failures.len(),
        repos.len()
    );
}

// ── Tier cross-check: Attestation ───────────────────────────────────────

/// Attestation ecosystem repos.
#[test]
fn tier_attestation() {
    if common::skip_if_offline("tier_attestation") {
        return;
    }

    let repos = [
        "tameshi",
        "sekiban",
        "kensa",
        "inshou",
        "iac-test-runner",
    ];

    let (_passed, failures) = run_tier(
        "Tier Attestation: Integrity Platform",
        &repos,
        &["packages", "overlays", "devShells"],
    );

    // Attestation: moderate -- at most half can fail.
    let max_fail = (repos.len() + 1) / 2;
    assert!(
        failures.len() <= max_fail,
        "Tier Attestation: too many failures ({}/{}): {:?}",
        failures.len(),
        repos.len(),
        failures
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
    );
}
