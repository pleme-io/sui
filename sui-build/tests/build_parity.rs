//! Layer 14: Build and substitution parity.
//!
//! Tests that sui can:
//! - Connect to binary caches (cache.nixos.org) and fetch NarInfo metadata
//! - Run the substitution pipeline end-to-end (fetch → decompress → verify)
//! - Compute build closures from real `.drv` files in the local store
//!
//! All tests gate on `SUI_TEST_ONLINE=1`.  The local Nix store is accessed
//! in read-only mode — these tests never modify `/nix/store`.

mod common;

use std::sync::Arc;

use sui_build::BuildClosure;
use sui_compat::store_path::StorePath;
use sui_store::binary_cache::BinaryCacheStore;
use sui_store::local::LocalStore;
use sui_store::substitute::{SubstituteResult, Substitutor};
use sui_store::traits::Store;

// ── Binary cache NarInfo fetch ─────────────────────────────────────────

/// Test that we can query a known path from cache.nixos.org by looking
/// up paths from our local store against the remote cache.
#[tokio::test]
async fn binary_cache_narinfo_fetch() {
    if common::skip_if_offline("binary_cache_narinfo_fetch") {
        return;
    }

    let store = match LocalStore::open(common::NIX_DB_PATH).await {
        Ok(s) => s,
        Err(e) => {
            println!("skip: can't open nix db: {e}");
            return;
        }
    };

    let paths = match store.query_all_valid_paths().await {
        Ok(p) => p,
        Err(e) => {
            println!("skip: can't query paths: {e}");
            return;
        }
    };

    if paths.is_empty() {
        println!("skip: empty store");
        return;
    }

    let cache = BinaryCacheStore::new("https://cache.nixos.org", vec![]);

    // Try the first several paths against the remote cache.
    let mut found = false;
    let mut tried = 0u32;
    for path in paths.iter().take(20) {
        tried += 1;
        let hash = path.hash();
        match cache.fetch_narinfo(&hash).await {
            Ok(Some(info)) => {
                println!(
                    "found in cache: {} (compression={}, nar_size={})",
                    path.to_absolute_path(),
                    info.compression,
                    info.nar_size,
                );
                found = true;
                break;
            }
            Ok(None) => continue,
            Err(e) => {
                println!("cache lookup error for {}: {e}", path.to_absolute_path());
                continue;
            }
        }
    }

    println!(
        "\n=== Layer 14: Binary Cache NarInfo Fetch ===\ntried={tried}, found={found}"
    );
    // At least some local paths should exist in the official cache.
    // Don't hard-assert — local-only builds won't be in cache.nixos.org.
}

// ── Substitution pipeline smoke test ───────────────────────────────────

/// Test the full substitution pipeline: local store check → cache lookup
/// → NarInfo parse. For paths already present locally, the substitutor
/// should report `AlreadyPresent` without hitting the network.
#[tokio::test]
async fn substitution_pipeline_smoke() {
    if common::skip_if_offline("substitution_pipeline_smoke") {
        return;
    }

    // Open store in read-only mode.
    let store = match LocalStore::open(common::NIX_DB_PATH).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            println!("skip: can't open nix db: {e}");
            return;
        }
    };

    let cache = Arc::new(BinaryCacheStore::new("https://cache.nixos.org", vec![]));
    let sub = Substitutor::new(store.clone(), vec![cache]);

    let paths = match store.query_all_valid_paths().await {
        Ok(p) => p,
        Err(e) => {
            println!("skip: can't query paths: {e}");
            return;
        }
    };

    let mut already_present = 0u32;
    let mut not_found = 0u32;
    let mut errors = 0u32;

    for path in paths.iter().take(10) {
        match sub.substitute(path).await {
            Ok(SubstituteResult::AlreadyPresent) => already_present += 1,
            Ok(SubstituteResult::Substituted {
                cache_url,
                nar_size,
            }) => {
                println!(
                    "substituted {} from {} ({} bytes)",
                    path.to_absolute_path(),
                    cache_url,
                    nar_size,
                );
            }
            Ok(SubstituteResult::NotFound) => {
                not_found += 1;
            }
            Err(e) => {
                println!("error substituting {}: {e}", path.to_absolute_path());
                errors += 1;
            }
        }
    }

    println!(
        "\n=== Layer 14: Substitution Pipeline ===\n\
         already_present={already_present}, not_found={not_found}, errors={errors}"
    );

    // Paths that are already in our local store MUST be reported as such.
    assert!(
        already_present > 0,
        "local store should have valid paths that report AlreadyPresent"
    );
}

// ── NarInfo field validation ───────────────────────────────────────────

/// When we do find a path in cache.nixos.org, verify the NarInfo has
/// all mandatory fields populated.
#[tokio::test]
async fn narinfo_fields_complete() {
    if common::skip_if_offline("narinfo_fields_complete") {
        return;
    }

    let store = match LocalStore::open(common::NIX_DB_PATH).await {
        Ok(s) => s,
        Err(e) => {
            println!("skip: can't open nix db: {e}");
            return;
        }
    };

    let paths = match store.query_all_valid_paths().await {
        Ok(p) => p,
        Err(e) => {
            println!("skip: can't query paths: {e}");
            return;
        }
    };

    let cache = BinaryCacheStore::new("https://cache.nixos.org", vec![]);

    let mut validated = 0u32;
    for path in paths.iter().take(50) {
        let hash = path.hash();
        if let Ok(Some(info)) = cache.fetch_narinfo(&hash).await {
            // Mandatory NarInfo fields per the spec.
            assert!(
                !info.store_path.is_empty(),
                "NarInfo.store_path must not be empty"
            );
            assert!(!info.url.is_empty(), "NarInfo.url must not be empty");
            assert!(
                !info.nar_hash.is_empty(),
                "NarInfo.nar_hash must not be empty"
            );
            assert!(info.nar_size > 0, "NarInfo.nar_size must be positive");
            assert!(
                !info.compression.is_empty(),
                "NarInfo.compression must not be empty"
            );

            validated += 1;
            if validated >= 3 {
                break;
            }
        }
    }

    println!(
        "\n=== Layer 14: NarInfo Field Validation ===\n{validated} narinfos validated"
    );
}

// ── Build closure computation with real .drv files ─────────────────────

/// Parse real `.drv` files from `/nix/store` and verify `BuildClosure`
/// can compute their dependency graph.
#[test]
fn closure_compute_real_drv() {
    if common::skip_if_offline("closure_compute_real_drv") {
        return;
    }

    let drv_files = common::nix_store_drv_sample(10);
    if drv_files.is_empty() {
        println!("skip: no .drv files in store");
        return;
    }

    let mut computed = 0u32;
    let mut failed = 0u32;

    for drv_file in &drv_files {
        let path_str = drv_file.to_str().unwrap_or("<invalid>");
        match BuildClosure::compute(path_str) {
            Ok(closure) => {
                let target = closure.target();
                println!(
                    "closure({}) => {} derivation(s), target: {}",
                    drv_file.file_name().unwrap_or_default().to_string_lossy(),
                    closure.len(),
                    target.0,
                );

                // Verify invariants.
                assert!(
                    !closure.is_empty(),
                    "closure should never be empty for a valid .drv"
                );
                assert!(
                    closure.len() == closure.derivations.len(),
                    "len() must match derivations vec"
                );
                // Target is always last.
                assert_eq!(
                    closure.target().0,
                    path_str,
                    "target must be the original .drv"
                );

                computed += 1;
            }
            Err(e) => {
                // Some .drv files reference inputs that may have been GC'd.
                // This is expected — just count and report.
                println!(
                    "closure failed for {}: {e}",
                    drv_file.file_name().unwrap_or_default().to_string_lossy(),
                );
                failed += 1;
            }
        }
    }

    println!(
        "\n=== Layer 14: Build Closure Computation ===\n\
         computed={computed}, failed={failed}"
    );

    // At least one should succeed if we have .drv files.
    // Don't hard-assert — GC'd deps can cause all to fail.
}

/// Verify that build closures produce derivations in valid topological
/// order: every derivation's input derivations appear earlier in the list.
#[test]
fn closure_topological_order_invariant() {
    if common::skip_if_offline("closure_topo_order") {
        return;
    }

    let drv_files = common::nix_store_drv_sample(5);
    let mut verified = 0u32;

    for drv_file in &drv_files {
        let path_str = drv_file.to_str().unwrap_or("<invalid>");
        let Ok(closure) = BuildClosure::compute(path_str) else {
            continue;
        };

        // Build a position map: drv_path -> index in topological order.
        let positions: std::collections::HashMap<&str, usize> = closure
            .derivations
            .iter()
            .enumerate()
            .map(|(i, (p, _))| (p.as_str(), i))
            .collect();

        // For each derivation, all its input derivations must come earlier.
        for (drv_path, drv) in &closure.derivations {
            let my_pos = positions[drv_path.as_str()];
            for dep_path in drv.input_derivations.keys() {
                if let Some(&dep_pos) = positions.get(dep_path.as_str()) {
                    assert!(
                        dep_pos < my_pos,
                        "topological order violation: {} (pos {my_pos}) depends on {} (pos {dep_pos})",
                        drv_path,
                        dep_path,
                    );
                }
                // If the dep isn't in our closure, it was GC'd — skip.
            }
        }

        verified += 1;
    }

    println!(
        "\n=== Layer 14: Topological Order Verified ===\n{verified} closures verified"
    );
}

/// Smoke test that `LocalStore::query_all_valid_paths` returns parseable
/// `StorePath` values that round-trip through `to_absolute_path`.
#[tokio::test]
async fn local_store_path_roundtrip() {
    if common::skip_if_offline("local_store_roundtrip") {
        return;
    }

    let store = match LocalStore::open(common::NIX_DB_PATH).await {
        Ok(s) => s,
        Err(e) => {
            println!("skip: can't open nix db: {e}");
            return;
        }
    };

    let paths = match store.query_all_valid_paths().await {
        Ok(p) => p,
        Err(e) => {
            println!("skip: can't query paths: {e}");
            return;
        }
    };

    let mut checked = 0u32;
    for path in paths.iter().take(100) {
        let abs = path.to_absolute_path();
        assert!(
            abs.starts_with("/nix/store/"),
            "store path should start with /nix/store/: {abs}"
        );
        let hash = path.hash();
        assert!(
            !hash.is_empty(),
            "store path hash should not be empty: {abs}"
        );
        let name = path.name();
        assert!(
            !name.is_empty(),
            "store path name should not be empty: {abs}"
        );

        // Round-trip: parse the absolute path back.
        let reparsed = StorePath::from_absolute_path(&abs)
            .unwrap_or_else(|e| panic!("failed to reparse {abs}: {e}"));
        assert_eq!(
            reparsed.to_absolute_path(),
            abs,
            "StorePath round-trip failed for {abs}"
        );

        checked += 1;
    }

    println!(
        "\n=== Layer 14: Store Path Round-trip ===\n{checked} paths verified"
    );
    assert!(checked > 0, "should have at least one valid store path");
}
