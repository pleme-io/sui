//! Layer 11: Mass flake evaluation smoke test.
//!
//! Evaluates every pleme-io repo's flake.nix through sui and reports
//! pass/fail rates.  This is a discovery test -- it does NOT assert on
//! pass rates, only prints a categorized report so we can track
//! evaluator coverage over time.
//!
//! Gated on `SUI_TEST_ONLINE=1`.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

// ── Layer 11a: Mass Flake Eval Smoke ────────────────────────────────────

#[test]
fn mass_flake_eval_smoke() {
    if common::skip_if_offline("mass_flake_eval") {
        return;
    }

    let repos = common::pleme_io_flake_nix_sample(500); // get all
    let mut pass = 0u32;
    let mut fail: Vec<(String, String)> = Vec::new();

    for flake_nix in &repos {
        let dir = flake_nix.parent().unwrap();
        let name = dir.file_name().unwrap().to_string_lossy().to_string();

        match sui_eval::builtins::evaluate_flake(dir) {
            Ok(_) => pass += 1,
            Err(e) => fail.push((name, format!("{e}"))),
        }
    }

    let total = repos.len();
    let pct = if total > 0 {
        (pass as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    println!("\n=== Layer 11: Mass Flake Eval ===");
    println!("{pass}/{total} repos evaluated ({pct:.1}%)");
    println!("{} failures:", fail.len());

    // Categorize failures by error type.
    let mut by_error: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, err) in &fail {
        let category = categorize_error(err);
        by_error.entry(category).or_default().push(name.clone());
    }

    for (category, names) in &by_error {
        let preview: Vec<&str> = names
            .iter()
            .take(5)
            .map(|s| s.as_str())
            .collect();
        println!(
            "  [{category}] ({} repos): {}",
            names.len(),
            preview.join(", ")
        );
    }
}

// ── Layer 11b: Description Parity ───────────────────────────────────────

#[test]
fn mass_flake_eval_description_parity() {
    if common::skip_if_offline("mass_flake_eval_description") {
        return;
    }

    let repos = common::pleme_io_flake_nix_sample(50); // smaller sample for parity checks
    let mut match_count = 0u32;
    let mut mismatch: Vec<(String, String, String)> = Vec::new();
    let mut skip_count = 0u32;

    for flake_nix in &repos {
        let dir = flake_nix.parent().unwrap();
        let name = dir.file_name().unwrap().to_string_lossy().to_string();

        // sui eval
        let sui_desc = match sui_eval::builtins::evaluate_flake(dir) {
            Ok(v) => extract_description(&v),
            Err(_) => {
                skip_count += 1;
                continue; // skip repos that fail eval
            }
        };

        // nix oracle
        let nix_desc = nix_flake_description(dir);

        if sui_desc == nix_desc {
            match_count += 1;
        } else {
            mismatch.push((name, sui_desc, nix_desc));
        }
    }

    println!("\n=== Layer 11: Description Parity ===");
    println!(
        "{match_count} descriptions match nix ({skip_count} skipped due to eval failure)"
    );
    if !mismatch.is_empty() {
        println!("{} mismatches:", mismatch.len());
        for (name, sui, nix) in mismatch.iter().take(10) {
            println!("  {name}: sui={sui:?} nix={nix:?}");
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Extract the `description` attribute from a flake evaluation result.
fn extract_description(v: &sui_eval::Value) -> String {
    match v {
        sui_eval::Value::Attrs(attrs) => match attrs.get("description") {
            Some(sui_eval::Value::String(s)) => s.chars.to_string(),
            Some(sui_eval::Value::Thunk(thunk)) => {
                match thunk.force(&|e, env| sui_eval::eval::eval_expr(e, env)) {
                    Ok(forced) => match &forced {
                        sui_eval::Value::String(s) => s.chars.to_string(),
                        _ => "<non-string>".to_string(),
                    },
                    Err(_) => "<thunk-error>".to_string(),
                }
            }
            Some(_) => "<non-string>".to_string(),
            None => "<missing>".to_string(),
        },
        _ => "<not-attrs>".to_string(),
    }
}

/// Run `nix flake metadata --json` to get the description from real nix.
fn nix_flake_description(dir: &Path) -> String {
    let ref_str = format!("path:{}", dir.display());
    let out = Command::new("nix")
        .args([
            "flake",
            "metadata",
            "--json",
            "--no-write-lock-file",
            "--extra-experimental-features",
            "nix-command flakes",
            &ref_str,
        ])
        .output();
    let out = match out {
        Ok(o) => o,
        Err(_) => return "<nix-spawn-error>".to_string(),
    };
    if !out.status.success() {
        return "<nix-error>".to_string();
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let meta: serde_json::Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(_) => return "<nix-json-error>".to_string(),
    };
    meta.get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("<missing>")
        .to_string()
}

/// Classify an error string into a stable category for reporting.
fn categorize_error(err: &str) -> String {
    if err.contains("undefined variable") {
        "undefined-var".into()
    } else if err.contains("not yet implemented") {
        "not-implemented".into()
    } else if err.contains("attribute not found") {
        "attr-not-found".into()
    } else if err.contains("I/O error") {
        "io-error".into()
    } else if err.contains("type error") {
        "type-error".into()
    } else if err.contains("parse error") {
        "parse-error".into()
    } else if err.contains("fetchGit") || err.contains("fetchTree") {
        "fetch-error".into()
    } else if err.contains("infinite recursion") {
        "infinite-recursion".into()
    } else if err.contains("assertion failed") {
        "assertion-failed".into()
    } else {
        "other".into()
    }
}
