//! Layer 12: Categorized flake evaluation by complexity tier.
//!
//! Tests repos grouped by structural complexity. Each tier tracks a
//! **minimum pass count** (regression guard). As the evaluator improves,
//! bump these floors upward. The tests also print detailed failure
//! reports for diagnostics.
//!
//! Current evaluator status (2026-04-07):
//!   - "cannot coerce set to string in interpolation" blocks most substrate-based repos
//!   - "import: expected path or string, got set" blocks rust-library repos
//!   - "missing argument 'nixpkgs'" blocks workspace repos using substrate import pattern
//!   - IaC forge repos (simple crate2nix flakes) have the best pass rate
//!
//! Gated on `SUI_TEST_ONLINE=1`.

mod common;

use std::collections::BTreeMap;
use std::path::Path;

// ── Shared helpers ──────────────────────────────────────────────────────

/// Try to evaluate a flake directory; return Ok(key_list) or Err(message).
fn eval_flake_keys(dir: &Path) -> Result<Vec<String>, String> {
    match sui_eval::builtins::evaluate_flake(dir) {
        Ok(v) => match v {
            sui_eval::Value::Attrs(ref attrs) => Ok(attrs.keys().cloned().collect()),
            _ => Ok(vec![]), // non-attrs result
        },
        Err(e) => Err(format!("{e}")),
    }
}

/// Classify an error string into a stable category for reporting.
fn categorize_error(err: &str) -> &'static str {
    if err.contains("cannot coerce set to string") {
        "coerce-set-to-string"
    } else if err.contains("import: expected path or string, got set") {
        "import-set-not-path"
    } else if err.contains("missing argument") {
        "missing-argument"
    } else if err.contains("undefined variable") {
        "undefined-var"
    } else if err.contains("not yet implemented") {
        "not-implemented"
    } else if err.contains("attribute not found") {
        "attr-not-found"
    } else if err.contains("I/O error") {
        "io-error"
    } else if err.contains("type error") {
        "type-error"
    } else if err.contains("parse error") {
        "parse-error"
    } else if err.contains("infinite recursion") {
        "infinite-recursion"
    } else {
        "other"
    }
}

/// Run a tier test: evaluate each repo, collect failures, print report.
///
/// `min_pass` is the regression guard -- the test asserts at least this
/// many repos succeed.  Set to 0 for discovery-only tiers.
fn run_tier(
    tier_name: &str,
    repo_names: &[&str],
    required_keys: &[&str],
    min_pass: usize,
) {
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

    // Print report.
    println!("\n=== {tier_name} ===");
    println!("{passed}/{attempted} passed ({skipped} skipped -- no flake.nix)");

    if !failures.is_empty() {
        // Group failures by error category.
        let mut by_category: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (name, err) in &failures {
            by_category
                .entry(categorize_error(err))
                .or_default()
                .push(name);
        }
        for (category, names) in &by_category {
            let preview: Vec<&&str> = names.iter().take(5).collect();
            let preview_str: Vec<&str> = preview.iter().map(|s| **s).collect();
            let suffix = if names.len() > 5 { ", ..." } else { "" };
            println!(
                "  [{category}] ({} repos): {}{}",
                names.len(),
                preview_str.join(", "),
                suffix,
            );
        }
    }

    // Regression guard: ensure we don't regress below the known minimum.
    assert!(
        passed >= min_pass,
        "{tier_name}: regression detected! Expected at least {min_pass} passes, got {passed}"
    );
}

// ── Tier A: Zero / minimal inputs ───────────────────────────────────────

/// Shared Rust libraries using substrate's rust-library.nix builder.
/// Currently blocked by "import: expected path or string, got set" --
/// substrate's import pattern uses a set that sui doesn't coerce yet.
#[test]
fn tier_a_minimal_inputs() {
    if common::skip_if_offline("tier_a") {
        return;
    }

    let repos = [
        "irodori", "hasami", "tsuuchi", "awase", "soushi", "tsunagu", "mojiban", "todoku",
        "shikumi", "hayai", "meimei", "sekkei", "takumi", "kaname",
    ];

    // Floor: 0 (all currently fail with import-set-not-path).
    // Bump this when the evaluator learns to coerce sets in import paths.
    run_tier(
        "Tier A: Minimal Inputs (shared Rust libraries)",
        &repos,
        &[], // no required keys -- just eval success
        0,
    );
}

// ── Tier B: Simple Rust tools ───────────────────────────────────────────

/// Single-crate Rust CLI tools. Currently blocked by "cannot coerce set
/// to string in interpolation" in substrate's builders.
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

    // Floor: 0 (all currently fail with coerce-set-to-string).
    run_tier(
        "Tier B: Simple Rust Tools",
        &repos,
        &["packages", "overlays", "devShells"],
        0,
    );
}

// ── Tier C: Rust workspaces ─────────────────────────────────────────────

/// Multi-crate Rust workspaces. Mixed failure modes: coercion errors,
/// missing arguments, attr-not-found.
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

    // Floor: 1 (iac-forge passes currently).
    run_tier(
        "Tier C: Rust Workspaces",
        &repos,
        &["packages", "overlays", "devShells"],
        1,
    );
}

// ── Tier D: Nix modules ─────────────────────────────────────────────────

/// Pure Nix module repos (blackmatter ecosystem). Complex module system
/// evaluation. Very lenient.
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

    // Floor: 0 (discovery only -- these are the hardest).
    run_tier(
        "Tier D: Nix Modules (blackmatter + infrastructure)",
        &repos,
        &[], // don't require specific keys -- module repos vary
        0,
    );
}

// ── Tier E: System configs ──────────────────────────────────────────────

/// Full system configurations. Discovery only -- no assertions.
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

/// GPU application repos.
#[test]
fn tier_gpu_apps() {
    if common::skip_if_offline("tier_gpu_apps") {
        return;
    }

    let repos = [
        "mado", "hibiki", "kagi", "kekkai", "nami", "namimado", "tobira", "hikyaku",
        "ayatsuri", "fumi", "tanken", "myaku", "hikki", "shashin", "koyomiban", "shirase",
    ];

    // Floor: 0 (all currently fail with coerce-set-to-string).
    run_tier(
        "Tier GPU: GPU Applications",
        &repos,
        &["packages", "overlays", "homeManagerModules"],
        0,
    );
}

// ── Tier cross-check: IaC forge pipeline ────────────────────────────────

/// IaC forge repos -- the best-performing tier. These use simpler
/// crate2nix-based flakes that the evaluator handles well.
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

    // Floor: 6 (currently 8/12 pass -- best tier).
    run_tier(
        "Tier IaC: Forge Pipeline",
        &repos,
        &["packages", "overlays", "devShells"],
        6,
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

    // Floor: 0 (all currently fail with missing-argument or coercion).
    run_tier(
        "Tier Privacy: Connectivity Suite",
        &repos,
        &["packages", "overlays"],
        0,
    );
}

// ── Tier cross-check: Server apps ───────────────────────────────────────

/// Server application repos (hiroba, taimen). Discovery only.
#[test]
fn tier_server_apps() {
    if common::skip_if_offline("tier_server_apps") {
        return;
    }

    let repos = ["hiroba", "taimen"];

    // Floor: 0 (both currently fail).
    run_tier(
        "Tier Server: Server Applications",
        &repos,
        &["packages", "overlays"],
        0,
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

    // Floor: 1 (tameshi passes currently).
    run_tier(
        "Tier Attestation: Integrity Platform",
        &repos,
        &["packages", "overlays", "devShells"],
        1,
    );
}
