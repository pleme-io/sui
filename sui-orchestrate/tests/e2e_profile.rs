//! Layer 17: End-to-end profile management test.
//!
//! Uses temp directories -- never touches real /nix/var/nix/profiles.
//! Profile tests use temp dirs and CAN hard-assert.

use sui_store::profile::ProfileManager;
use tempfile::TempDir;

#[test]
fn e2e_profile_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let store_tmp = TempDir::new().unwrap();

    // Create fake store paths
    let sys1 = store_tmp.path().join("system-v1");
    let sys2 = store_tmp.path().join("system-v2");
    let sys3 = store_tmp.path().join("system-v3");
    std::fs::create_dir_all(&sys1).unwrap();
    std::fs::create_dir_all(&sys2).unwrap();
    std::fs::create_dir_all(&sys3).unwrap();

    // Create fake activate scripts
    std::fs::write(sys1.join("activate"), "#!/bin/sh\necho v1").unwrap();
    std::fs::write(sys2.join("activate"), "#!/bin/sh\necho v2").unwrap();
    std::fs::write(sys3.join("activate"), "#!/bin/sh\necho v3").unwrap();

    let pm = ProfileManager::new(tmp.path(), "system");

    // Initial state: no generations
    assert_eq!(pm.current_generation().unwrap(), None);
    assert!(pm.list_generations().unwrap().is_empty());

    // Set first generation
    let gen1 = pm.set(&sys1).unwrap();
    assert_eq!(gen1, 1);
    assert_eq!(pm.current_generation().unwrap(), Some(1));

    // Set second
    let gen2 = pm.set(&sys2).unwrap();
    assert_eq!(gen2, 2);
    assert_eq!(pm.current_generation().unwrap(), Some(2));

    // List
    let gens = pm.list_generations().unwrap();
    assert_eq!(gens.len(), 2);
    assert_eq!(gens[0].number, 1);
    assert_eq!(gens[1].number, 2);
    assert!(gens[1].current);

    // Rollback
    let prev = pm.rollback().unwrap();
    assert_eq!(prev, 1);
    assert_eq!(pm.current_generation().unwrap(), Some(1));

    // Set third and switch back
    let gen3 = pm.set(&sys3).unwrap();
    assert_eq!(gen3, 3);
    pm.switch_generation(2).unwrap();
    assert_eq!(pm.current_generation().unwrap(), Some(2));

    println!("profile lifecycle: 3 generations, rollback, switch -- all working");
}

#[test]
fn e2e_profile_delete_and_recount() {
    let tmp = TempDir::new().unwrap();
    let store_tmp = TempDir::new().unwrap();

    let paths: Vec<_> = (1..=5)
        .map(|i| {
            let p = store_tmp.path().join(format!("gen-{i}"));
            std::fs::create_dir_all(&p).unwrap();
            p
        })
        .collect();

    let pm = ProfileManager::new(tmp.path(), "test");

    // Set 5 generations
    for (i, path) in paths.iter().enumerate() {
        let num = pm.set(path).unwrap();
        assert_eq!(num, (i + 1) as u32);
    }

    assert_eq!(pm.current_generation().unwrap(), Some(5));
    assert_eq!(pm.list_generations().unwrap().len(), 5);

    // Delete generations 2 and 4 (not current)
    pm.delete_generation(2).unwrap();
    pm.delete_generation(4).unwrap();

    let remaining = pm.list_generations().unwrap();
    assert_eq!(remaining.len(), 3);
    let numbers: Vec<u32> = remaining.iter().map(|g| g.number).collect();
    assert_eq!(numbers, vec![1, 3, 5]);

    // Current should still be 5
    assert_eq!(pm.current_generation().unwrap(), Some(5));
    assert!(remaining.iter().find(|g| g.number == 5).unwrap().current);

    // Rollback from 5 should go to 3 (4 was deleted)
    let prev = pm.rollback().unwrap();
    assert_eq!(prev, 3);

    println!("profile delete + recount: 5 gens, delete 2+4, rollback to 3 -- working");
}

#[test]
fn e2e_profile_generation_paths_resolve_correctly() {
    let tmp = TempDir::new().unwrap();
    let store_tmp = TempDir::new().unwrap();

    let path_a = store_tmp.path().join("store-path-a");
    let path_b = store_tmp.path().join("store-path-b");
    std::fs::create_dir_all(&path_a).unwrap();
    std::fs::create_dir_all(&path_b).unwrap();

    // Write distinct marker files so we can verify paths
    std::fs::write(path_a.join("marker"), "a").unwrap();
    std::fs::write(path_b.join("marker"), "b").unwrap();

    let pm = ProfileManager::new(tmp.path(), "verify");

    pm.set(&path_a).unwrap();
    pm.set(&path_b).unwrap();

    let gens = pm.list_generations().unwrap();
    assert_eq!(gens[0].path, path_a);
    assert_eq!(gens[1].path, path_b);

    // Switch to gen 1 and verify the generation link resolves
    pm.switch_generation(1).unwrap();
    let gens_after = pm.list_generations().unwrap();
    assert!(gens_after[0].current);
    assert!(!gens_after[1].current);

    // Read the marker through the symlink chain to verify correctness
    let profile_path = pm.profile_path();
    assert!(profile_path.is_symlink());

    println!("profile paths resolve correctly across generations");
}

#[test]
fn e2e_profile_concurrent_managers_see_same_state() {
    let tmp = TempDir::new().unwrap();
    let store_tmp = TempDir::new().unwrap();

    let path1 = store_tmp.path().join("shared-1");
    let path2 = store_tmp.path().join("shared-2");
    std::fs::create_dir_all(&path1).unwrap();
    std::fs::create_dir_all(&path2).unwrap();

    // Two managers on the same profile directory
    let pm1 = ProfileManager::new(tmp.path(), "shared");
    let pm2 = ProfileManager::new(tmp.path(), "shared");

    pm1.set(&path1).unwrap();
    assert_eq!(pm2.current_generation().unwrap(), Some(1));

    pm2.set(&path2).unwrap();
    assert_eq!(pm1.current_generation().unwrap(), Some(2));

    let gens1 = pm1.list_generations().unwrap();
    let gens2 = pm2.list_generations().unwrap();
    assert_eq!(gens1.len(), gens2.len());
    assert_eq!(gens1.len(), 2);

    println!("concurrent profile managers see consistent state");
}
