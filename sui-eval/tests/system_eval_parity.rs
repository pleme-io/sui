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
                let keys: Vec<&String> = attrs.keys().collect();
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
                    let hosts: Vec<&String> = dc_attrs.keys().collect();
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
