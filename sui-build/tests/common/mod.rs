//! Shared helpers for sui-build integration tests.

#![allow(dead_code)]

use std::env;
use std::path::PathBuf;

/// Returns `true` when `SUI_TEST_ONLINE=1` is set in the environment.
pub fn online_mode() -> bool {
    env::var("SUI_TEST_ONLINE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// If online mode is not enabled, emit a skip note and return `true`.
pub fn skip_if_offline(test_name: &str) -> bool {
    if !online_mode() {
        eprintln!("skip {test_name}: SUI_TEST_ONLINE not set");
        return true;
    }
    false
}

/// Root directory of the local nix store.
pub fn nix_store_root() -> PathBuf {
    env::var("NIX_STORE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/nix/store"))
}

/// Default path to the Nix SQLite database.
pub const NIX_DB_PATH: &str = "/nix/var/nix/db/db.sqlite";

/// Return up to `n` `.drv` files from the nix store, lex-sorted.
pub fn nix_store_drv_sample(n: usize) -> Vec<PathBuf> {
    let root = nix_store_root();
    if !root.is_dir() {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = match std::fs::read_dir(&root) {
        Ok(it) => it
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("drv"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    out.sort();
    out.truncate(n);
    out
}
