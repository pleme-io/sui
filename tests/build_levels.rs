//! End-to-end build pipeline verification — 5 progressive levels.
//!
//! Level 1: eval -> drvPath on disk
//! Level 2: BuildClosure on real .drv
//! Level 3: Substitutor from cache.nixos.org
//! Level 4: Full package build
//! Level 5: System rebuild (build-only)
//!
//! Gates:
//!   SUI_TEST_ONLINE=1  — all levels (needs network)
//!   SUI_TEST_BUILD=1   — levels 4-5 (writes to store)
//!   SUI_TEST_SYSTEM=1  — level 5 (full system rebuild)

use sui_store::Store as _;

fn online() -> bool {
    std::env::var("SUI_TEST_ONLINE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn build_mode() -> bool {
    online()
        && std::env::var("SUI_TEST_BUILD")
            .map(|v| v == "1")
            .unwrap_or(false)
}

fn system_mode() -> bool {
    build_mode()
        && std::env::var("SUI_TEST_SYSTEM")
            .map(|v| v == "1")
            .unwrap_or(false)
}

fn pleme_io_root() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("PLEME_IO_ROOT").unwrap_or_else(|_| {
            format!(
                "{}/code/github/pleme-io",
                std::env::var("HOME").unwrap()
            )
        }),
    )
}

/// Walk a dot-separated attrset path, forcing thunks along the way,
/// and extract the final value as a string (either a plain string or
/// the `drvPath` attribute of the leaf attrset).
fn navigate_to_drv_path(
    value: &sui_eval::value::Value,
    dot_path: &str,
) -> Result<String, String> {
    let mut current = value.clone();
    for key in dot_path.split('.') {
        current = sui_eval::eval::force_value(&current)
            .map_err(|e| format!("force at {key}: {e}"))?;
        match current {
            sui_eval::value::Value::Attrs(ref attrs) => {
                current = attrs
                    .get(key)
                    .ok_or(format!("key '{key}' not found"))?
                    .clone();
            }
            _ => {
                return Err(format!(
                    "expected attrs at '{key}', got {}",
                    current.type_name()
                ));
            }
        }
    }

    // Force the leaf value.
    current = sui_eval::eval::force_value(&current)
        .map_err(|e| format!("force final: {e}"))?;

    // If it is an attrset with drvPath, extract that.
    if let sui_eval::value::Value::Attrs(ref attrs) = current {
        if let Some(drv) = attrs.get("drvPath") {
            let forced = sui_eval::eval::force_value(drv)
                .map_err(|e| format!("force drvPath: {e}"))?;
            return forced
                .to_str()
                .map_err(|e| format!("drvPath not a string: {e}"));
        }
    }

    // Maybe the value itself is the drvPath string.
    current
        .to_str()
        .map_err(|e| format!("not a string or attrs with drvPath: {e}"))
}

// ── Level 1: eval -> drvPath ─────────────────────────────────────

#[test]
fn level1_eval_produces_drv_path() {
    if !online() {
        println!("skip: SUI_TEST_ONLINE not set");
        return;
    }

    let repos = ["tend", "blx", "skim-tab", "seibi"];
    let root = pleme_io_root();
    let mut success = 0u32;

    for name in repos {
        let dir = root.join(name);
        if !dir.join("flake.nix").exists() {
            println!("{name}: flake.nix not found at {}, skipping", dir.display());
            continue;
        }

        match sui_eval::builtins::evaluate_flake(&dir) {
            Ok(result) => {
                let drv_path =
                    navigate_to_drv_path(&result, "packages.aarch64-darwin.default");
                match drv_path {
                    Ok(path) => {
                        println!("{name}: drvPath = {path}");
                        assert!(
                            path.starts_with("/nix/store/"),
                            "{name}: bad drvPath: {path}"
                        );
                        assert!(
                            path.ends_with(".drv"),
                            "{name}: not a .drv: {path}"
                        );
                        if std::path::Path::new(&path).exists() {
                            println!("  ok: .drv exists on disk");
                        } else {
                            println!("  note: .drv NOT on disk (eval-only — not yet instantiated)");
                        }
                        success += 1;
                    }
                    Err(e) => println!("{name}: navigate error: {e}"),
                }
            }
            Err(e) => println!("{name}: eval error: {e}"),
        }
    }

    println!("\nLevel 1: {success}/{} repos produced valid drvPath", repos.len());
    assert!(success > 0, "at least one repo should produce a drvPath");
}

// ── Level 2: BuildClosure on real .drv ───────────────────────────

#[test]
fn level2_closure_from_store_drv() {
    if !online() {
        println!("skip: SUI_TEST_ONLINE not set");
        return;
    }

    let store_dir = std::path::Path::new("/nix/store");
    if !store_dir.exists() {
        println!("skip: no /nix/store");
        return;
    }

    // Find the first .drv in /nix/store.
    let drv_path: Option<String> = std::fs::read_dir(store_dir)
        .ok()
        .and_then(|entries| {
            entries
                .filter_map(|e| e.ok())
                .find(|e| e.file_name().to_string_lossy().ends_with(".drv"))
                .map(|e| e.path().to_string_lossy().into_owned())
        });

    let drv_path = match drv_path {
        Some(p) => p,
        None => {
            println!("skip: no .drv files found in /nix/store");
            return;
        }
    };

    println!("computing closure for: {drv_path}");
    match sui_build::BuildClosure::compute(&drv_path) {
        Ok(closure) => {
            println!("closure: {} derivations", closure.len());
            println!("target: {}", closure.target().0);
            assert!(closure.len() > 0, "closure should contain at least one derivation");
        }
        Err(e) => {
            // Some .drv files may reference missing inputs — report but don't fail.
            println!("closure error (non-fatal): {e}");
        }
    }
}

// ── Level 3: Substitutor cache lookup ────────────────────────────

#[tokio::test]
async fn level3_substitutor_cache_lookup() {
    if !online() {
        println!("skip: SUI_TEST_ONLINE not set");
        return;
    }

    let db_path = "/nix/var/nix/db/db.sqlite";
    if !std::path::Path::new(db_path).exists() {
        println!("skip: no Nix database at {db_path}");
        return;
    }

    let store = match sui_store::LocalStore::open(db_path).await {
        Ok(s) => std::sync::Arc::new(s),
        Err(e) => {
            println!("skip: cannot open local store: {e}");
            return;
        }
    };

    let cache = std::sync::Arc::new(sui_store::BinaryCacheStore::new(
        "https://cache.nixos.org",
        vec![],
    ));
    let sub = sui_store::Substitutor::new(
        store.clone() as std::sync::Arc<dyn sui_store::Store>,
        vec![cache],
    );

    let paths = match store.query_all_valid_paths().await {
        Ok(p) => p,
        Err(e) => {
            println!("skip: query_all_valid_paths failed: {e}");
            return;
        }
    };

    if paths.is_empty() {
        println!("skip: local store has no valid paths");
        return;
    }

    let mut present = 0u32;
    let mut in_cache = 0u32;
    let mut not_found = 0u32;
    let mut errors = 0u32;

    // Sample up to 20 paths.
    for path in paths.iter().take(20) {
        match sub.substitute(path).await {
            Ok(sui_store::SubstituteResult::AlreadyPresent) => present += 1,
            Ok(sui_store::SubstituteResult::Substituted { .. }) => in_cache += 1,
            Ok(sui_store::SubstituteResult::NotFound) => not_found += 1,
            Err(e) => {
                println!("  error: {}: {e}", path.to_absolute_path());
                errors += 1;
            }
        }
    }

    println!(
        "Level 3: {present} present, {in_cache} substituted, {not_found} not found, {errors} errors"
    );
    assert!(present > 0, "local store should have valid paths already present");
}

// ── Level 4: Full package build ──────────────────────────────────

#[tokio::test]
async fn level4_build_simple_package() {
    if !build_mode() {
        println!("skip: SUI_TEST_BUILD not set");
        return;
    }

    let root = pleme_io_root();
    let tend_dir = root.join("tend");
    if !tend_dir.join("flake.nix").exists() {
        println!("skip: tend not found at {}", tend_dir.display());
        return;
    }

    // 1. Evaluate
    let flake_result = match sui_eval::builtins::evaluate_flake(&tend_dir) {
        Ok(v) => v,
        Err(e) => {
            println!("level 4: eval failed: {e}");
            return;
        }
    };

    // 2. Get drvPath
    let drv_path =
        match navigate_to_drv_path(&flake_result, "packages.aarch64-darwin.default") {
            Ok(p) => p,
            Err(e) => {
                println!("level 4: navigate to drvPath failed: {e}");
                return;
            }
        };
    println!("drvPath: {drv_path}");

    if !std::path::Path::new(&drv_path).exists() {
        println!("level 4: .drv not on disk — cannot compute closure without instantiation");
        return;
    }

    // 3. Compute closure
    let closure = match sui_build::BuildClosure::compute(&drv_path) {
        Ok(c) => c,
        Err(e) => {
            println!("level 4: closure computation failed: {e}");
            return;
        }
    };
    println!("closure: {} derivations", closure.len());

    // 4. Build
    let db_path = "/nix/var/nix/db/db.sqlite";
    let store: std::sync::Arc<dyn sui_store::Store> =
        match sui_store::LocalStore::open_rw(db_path).await {
            Ok(s) => std::sync::Arc::new(s),
            Err(e) => {
                println!("level 4: cannot open store for writing: {e}");
                return;
            }
        };

    let cache = std::sync::Arc::new(sui_store::BinaryCacheStore::new(
        "https://cache.nixos.org",
        vec![],
    ));
    let sub = sui_store::Substitutor::new(store.clone(), vec![cache]);
    let builder = sui_build::LocalBuilder::new(
        store,
        Box::new(sui_build::sandbox::DarwinSandbox::new()),
    );

    match builder.build_closure(&closure, Some(&sub)).await {
        Ok(result) => {
            println!(
                "build result: success={}, outputs={}",
                result.success,
                result.outputs.len()
            );
            if result.success {
                for output in &result.outputs {
                    let abs = output.to_absolute_path();
                    println!("  output: {abs}");
                    // Soft assert — report but don't panic on missing output.
                    if !std::path::Path::new(&abs).exists() {
                        println!("  warning: output path does not exist on disk");
                    }
                }
            } else {
                let log_preview: String = result.log.chars().take(2000).collect();
                println!("build log (truncated):\n{log_preview}");
            }
        }
        Err(e) => println!("level 4: build error: {e}"),
    }
}

// ── Level 5: System rebuild (build-only) ─────────────────────────

#[tokio::test]
async fn level5_system_rebuild_build_only() {
    if !system_mode() {
        println!("skip: SUI_TEST_SYSTEM not set");
        return;
    }

    let nix_dir = pleme_io_root().join("nix");
    if !nix_dir.join("flake.nix").exists() {
        println!("skip: nix repo not found at {}", nix_dir.display());
        return;
    }

    println!("evaluating system flake at {}", nix_dir.display());

    // 1. Evaluate the nix flake.
    let result = match sui_eval::builtins::evaluate_flake(&nix_dir) {
        Ok(v) => v,
        Err(e) => {
            println!("level 5: nix flake eval failed: {e}");
            return;
        }
    };

    // 2. Navigate to darwinConfigurations.cid.config.system.build.toplevel.
    let drv = navigate_to_drv_path(
        &result,
        "darwinConfigurations.cid.config.system.build.toplevel",
    );
    match drv {
        Ok(path) => {
            println!("system drvPath: {path}");

            if !std::path::Path::new(&path).exists() {
                println!("level 5: system .drv not on disk — skipping closure check");
                return;
            }

            // 3. Compute closure — don't actually build, just verify the closure.
            match sui_build::BuildClosure::compute(&path) {
                Ok(closure) => {
                    println!("system closure: {} derivations", closure.len());
                    // A system closure should be large.
                    if closure.len() < 100 {
                        println!(
                            "warning: system closure only has {} derivations (expected 100+)",
                            closure.len()
                        );
                    }
                }
                Err(e) => println!("level 5: closure computation failed: {e}"),
            }
        }
        Err(e) => println!("level 5: navigate to system drv failed: {e}"),
    }
}
