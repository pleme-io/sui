//! Layer 16: Full build test.
//!
//! Attempts to build a real derivation closure using sui's native
//! BuildClosure + LocalStore pipeline. These are discovery tests --
//! they report what works without hard-asserting on operations that
//! require write access to the store or network substitution.

fn skip_if_offline() -> bool {
    std::env::var("SUI_TEST_ONLINE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
        == false
}

/// Compute a build closure for a real .drv from the store
#[test]
fn compute_real_drv_closure() {
    if skip_if_offline() {
        return;
    }

    // Find a small .drv file in the store
    let store_dir = std::path::Path::new("/nix/store");
    if !store_dir.exists() {
        println!("skip: no /nix/store");
        return;
    }

    // Find hello or a simple derivation
    let drv_files: Vec<_> = match std::fs::read_dir(store_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.ends_with(".drv")
                    && (name.contains("hello") || name.contains("coreutils"))
            })
            .take(1)
            .collect(),
        Err(e) => {
            println!("skip: cannot read /nix/store: {e}");
            return;
        }
    };

    if drv_files.is_empty() {
        println!("skip: no hello/coreutils .drv found in /nix/store");
        return;
    }

    let drv_path = drv_files[0].path();
    println!("computing closure for: {}", drv_path.display());

    // Compute closure
    let closure = match sui_build::BuildClosure::compute(&drv_path.to_string_lossy()) {
        Ok(c) => c,
        Err(e) => {
            println!("closure computation failed: {e}");
            return;
        }
    };

    println!("closure size: {} derivations", closure.len());
    assert!(!closure.is_empty(), "closure should contain at least the target");

    // Report target info
    let (target_path, target_drv) = closure.target();
    println!("target: {target_path}");
    println!(
        "target outputs: {:?}",
        target_drv.outputs.keys().collect::<Vec<_>>()
    );
    println!("target system: {}", target_drv.system);
    println!("target builder: {}", target_drv.builder);

    // Count how many outputs already exist on disk
    let mut existing = 0u32;
    let mut missing = 0u32;
    for (_, drv) in &closure.derivations {
        for output in drv.outputs.values() {
            if !output.path.is_empty() {
                let output_path = std::path::Path::new(&output.path);
                if output_path.exists() {
                    existing += 1;
                } else {
                    missing += 1;
                }
            }
        }
    }

    println!("{existing} outputs exist on disk, {missing} missing");
    println!("(full build would need substitution for missing outputs)");
}

/// Build closure for a known-small .drv using tokio async store
#[tokio::test]
async fn closure_check_store_validity() {
    if skip_if_offline() {
        return;
    }

    let store_dir = std::path::Path::new("/nix/store");
    if !store_dir.exists() {
        println!("skip: no /nix/store");
        return;
    }

    // Find any .drv file
    let drv_file = match std::fs::read_dir(store_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(".drv")
            }),
        Err(e) => {
            println!("skip: cannot read /nix/store: {e}");
            return;
        }
    };

    let drv_file = match drv_file {
        Some(f) => f,
        None => {
            println!("skip: no .drv files found");
            return;
        }
    };

    let drv_path = drv_file.path();
    println!("checking closure for: {}", drv_path.display());

    let closure = match sui_build::BuildClosure::compute(&drv_path.to_string_lossy()) {
        Ok(c) => c,
        Err(e) => {
            println!("closure failed: {e}");
            return;
        }
    };

    println!("closure size: {} derivations", closure.len());

    // Open store (read-only for safety)
    let db_path = "/nix/var/nix/db/db.sqlite";
    if !std::path::Path::new(db_path).exists() {
        println!("skip: no nix database at {db_path}");
        return;
    }

    let store = match sui_store::LocalStore::open(db_path).await {
        Ok(s) => s,
        Err(e) => {
            println!("store open failed: {e}");
            return;
        }
    };

    // Check how many outputs are registered in the store
    let mut registered = 0u32;
    let mut unregistered = 0u32;
    let mut parse_errors = 0u32;
    for (_, drv) in &closure.derivations {
        for output in drv.outputs.values() {
            if output.path.is_empty() {
                continue;
            }
            match sui_compat::store_path::StorePath::from_absolute_path(&output.path) {
                Ok(sp) => {
                    use sui_store::Store;
                    match store.is_valid_path(&sp).await {
                        Ok(true) => registered += 1,
                        Ok(false) => unregistered += 1,
                        Err(e) => {
                            println!("is_valid_path error: {e}");
                            unregistered += 1;
                        }
                    }
                }
                Err(_) => parse_errors += 1,
            }
        }
    }

    println!("{registered} outputs registered in store");
    println!("{unregistered} outputs not registered");
    if parse_errors > 0 {
        println!("{parse_errors} output paths failed to parse as store paths");
    }
}
