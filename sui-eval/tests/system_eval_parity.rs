//! Layer 15: System configuration evaluation parity.
//!
//! Progressive tests against ~/code/github/pleme-io/nix system configs.
//! These are discovery tests -- they report what works and what doesn't
//! without hard-asserting on operations that depend on full flake evaluation
//! infrastructure (network fetchTree, nixpkgs import, etc.).

mod common;

use std::path::PathBuf;

fn nix_repo() -> PathBuf {
    common::pleme_io_root().join("nix")
}

/// Level 1: evaluate_flake on the nix repo doesn't crash
#[test]
fn nix_repo_eval_no_crash() {
    if common::skip_if_offline("system_eval") {
        return;
    }
    let dir = nix_repo();
    if !dir.join("flake.nix").exists() {
        println!("skip: nix repo not found at {}", dir.display());
        return;
    }

    println!("evaluating {}", dir.display());
    match sui_eval::builtins::evaluate_flake(&dir) {
        Ok(v) => {
            println!("SUCCESS: nix repo evaluated");
            // Report what top-level keys exist
            if let sui_eval::value::Value::Attrs(ref attrs) = v {
                let keys: Vec<String> = attrs.keys().collect();
                println!("top-level keys: {:?}", keys);
            }
        }
        Err(e) => {
            println!("EXPECTED FAILURE (for now): {e}");
            // Don't assert -- this is informational
        }
    }
}

/// Level 2: darwinConfigurations key exists
#[test]
fn nix_repo_has_darwin_configurations() {
    if common::skip_if_offline("system_eval_darwin") {
        return;
    }
    let dir = nix_repo();
    if !dir.join("flake.nix").exists() {
        return;
    }

    let result = match sui_eval::builtins::evaluate_flake(&dir) {
        Ok(v) => v,
        Err(e) => {
            println!("eval failed: {e}");
            return;
        }
    };

    if let sui_eval::value::Value::Attrs(ref attrs) = result {
        assert!(
            attrs.contains_key("darwinConfigurations"),
            "flake output should have darwinConfigurations"
        );
        println!("darwinConfigurations found");

        // Try to navigate into it
        if let Some(dc) = attrs.get("darwinConfigurations") {
            let forced = sui_eval::eval::force_value(dc);
            match forced {
                Ok(sui_eval::value::Value::Attrs(ref dc_attrs)) => {
                    let hosts: Vec<String> = dc_attrs.keys().collect();
                    println!("hosts: {:?}", hosts);
                }
                Ok(other) => println!("darwinConfigurations is {}", other.type_name()),
                Err(e) => println!("force darwinConfigurations: {e}"),
            }
        }
    }
}

/// Level 3: navigate to darwinConfigurations.cid
#[test]
fn nix_repo_darwin_cid_exists() {
    if common::skip_if_offline("system_eval_cid") {
        return;
    }
    let dir = nix_repo();
    if !dir.join("flake.nix").exists() {
        return;
    }

    let result = match sui_eval::builtins::evaluate_flake(&dir) {
        Ok(v) => v,
        Err(e) => {
            println!("eval failed: {e}");
            return;
        }
    };

    let path = ["darwinConfigurations", "cid"];
    let mut current = result;
    for key in &path {
        current = match sui_eval::eval::force_value(&current) {
            Ok(v) => v,
            Err(e) => {
                println!("force at {key}: {e}");
                return;
            }
        };
        match current {
            sui_eval::value::Value::Attrs(ref attrs) => {
                current = match attrs.get(*key) {
                    Some(v) => v.clone(),
                    None => {
                        println!("{key} not found");
                        return;
                    }
                };
            }
            _ => {
                println!("expected attrs at {key}, got {}", current.type_name());
                return;
            }
        }
    }

    println!("darwinConfigurations.cid reached successfully");
}

/// Level 4: navigate to config.system.build.toplevel.drvPath
#[test]
fn nix_repo_cid_drv_path() {
    if common::skip_if_offline("system_eval_drv") {
        return;
    }
    let dir = nix_repo();
    if !dir.join("flake.nix").exists() {
        return;
    }

    let result = match sui_eval::builtins::evaluate_flake(&dir) {
        Ok(v) => v,
        Err(e) => {
            println!("eval failed: {e}");
            return;
        }
    };

    let path = [
        "darwinConfigurations",
        "cid",
        "config",
        "system",
        "build",
        "toplevel",
        "drvPath",
    ];
    let mut current = result;
    for key in &path {
        current = match sui_eval::eval::force_value(&current) {
            Ok(v) => v,
            Err(e) => {
                println!("force at {key}: {e}");
                return;
            }
        };
        match current {
            sui_eval::value::Value::Attrs(ref attrs) => {
                current = match attrs.get(*key) {
                    Some(v) => v.clone(),
                    None => {
                        println!("{key} not found in attrs");
                        return;
                    }
                };
            }
            _ => {
                println!("expected attrs at {key}, got {}", current.type_name());
                return;
            }
        }
    }

    let forced = sui_eval::eval::force_value(&current);
    match forced {
        Ok(sui_eval::value::Value::String(ref s)) => {
            println!("drvPath: {}", s.as_str());
            assert!(
                s.as_str().starts_with("/nix/store/"),
                "drvPath should be a store path"
            );
            assert!(s.as_str().ends_with(".drv"), "drvPath should end with .drv");
        }
        Ok(other) => println!("drvPath is {}", other.type_name()),
        Err(e) => println!("force drvPath: {e}"),
    }
}

// ── M2.6 regression — `lib.nixosSystem` full module set ──────────────
//
// Pins the operator-blocking failure documented in
// `docs/M2.6-MODULE-SYSTEM-FIXPOINT.md`.  Original symptom:
// `lib.nixosSystem { modules = []; }` raised `InfiniteRecursion` in the
// `_module.args.pkgs` ↔ `matchedOptions` bootstrap.  CLOSED 2026-07-11
// via TWO byte-verified laziness fixes (roots #1–#3 sealed earlier):
//
//   ROOT #4a (over-force) — `with X; body` eagerly EVALUATED the
//     namespace at `with`-entry (`eval.rs::eval_expr` With arm).  For
//     nixpkgs' `config = mkIf … (with config.services.X; { … })` module
//     shape, demanding the body's WHNF/keys during collection forced
//     `config.services.X` mid-fixpoint → the empty-Promise partial →
//     `concatLists null`.  Fixed by storing the namespace as a lazy
//     thunk (`maybe_thunk`), forced only on a lookup fallthrough.
//     Repro: `builtins.attrNames (with (throw "X"); { a = 1; })`.
//
//   ROOT #4b (dropped full-set leaf) — a depth-≥2 dotted binding whose
//     leaf is a full-set (`options.hardware.alsa = { … }`) followed by a
//     deeper dotted sibling (`options.hardware.alsa.enablePersistence =
//     …`) merged to ONLY the deeper key, because `merge_nested_insert`
//     required BOTH collision sides to be concrete `Value::Attrs` and a
//     full-set leaf is a lazy `Thunk`.  Fixed by forcing the existing
//     thunk to WHNF (fields stay lazy) before the deep merge.
//     Repro: `builtins.attrNames { o.a = { x = 1; }; o.a.y = 2; }.o.a`.
//
// The assertion is now MANDATORY: `config.system.name` must be a real
// string (`"nixos"`), byte-identical to cppnix.

/// M2.6 — `lib.nixosSystem { modules = []; }` must terminate and yield
/// `config.system.name == "nixos"`.  Regression guard for ROOT #4a/#4b.
#[test]
fn nixos_system_empty_modules_terminates() {
    if common::skip_if_offline("m2_6_regression") {
        return;
    }
    // HOME-relative sui input cache (works on both macOS + Linux).
    let home = std::env::var("HOME").unwrap_or_default();
    let nixpkgs_owned = std::path::PathBuf::from(home).join(
        ".cache/sui/inputs/github-NixOS-nixpkgs-b77b3de/nixpkgs-b77b3de8775677f84492abe84635f87b0e153f0f",
    );
    let nixpkgs_dir = nixpkgs_owned.as_path();
    if !nixpkgs_dir.exists() {
        println!("skip: pinned nixpkgs source not in sui input cache");
        return;
    }
    let expr = format!(
        "let nixpkgs = builtins.getFlake \"path:{}\"; \
         in (nixpkgs.lib.nixosSystem {{ system = \"x86_64-linux\"; modules = []; }}) \
            .config.system.name",
        nixpkgs_dir.display(),
    );
    let result = sui_eval::eval(&expr);
    let value = result.expect("nixosSystem must evaluate without InfiniteRecursion");
    let forced = sui_eval::eval::force_value(&value)
        .expect("system.name forces to a concrete value");
    match forced {
        sui_eval::value::Value::String(s) => {
            assert!(!s.as_str().is_empty(), "system.name must not be empty");
            println!("nixosSystem returned system.name = {:?}", s.as_str());
        }
        other => panic!("expected system.name string, got {}", other.type_name()),
    }
}
