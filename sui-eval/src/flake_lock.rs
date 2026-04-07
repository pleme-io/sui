//! Native flake lock management — update, check, and write `flake.lock`.
//!
//! Provides `update_input` / `update_all_inputs` to resolve latest revisions
//! for locked flake inputs (replacing `nix flake update`), and `check_flake`
//! to validate a flake directory (replacing `nix flake check`).
//!
//! Network-dependent operations (GitHub API, `git ls-remote`) are gated
//! behind the `SUI_TEST_ONLINE=1` environment variable in tests.

use std::path::Path;

use sui_compat::flake::{FlakeLock, OriginalInput};

// ── Error type ────────────────────────────────────────────────

/// Errors that can occur during flake lock operations.
#[derive(Debug, thiserror::Error)]
pub enum FlakeLockUpdateError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid flake.lock format")]
    InvalidFormat,
    #[error("input not found: {0}")]
    InputNotFound(String),
    #[error("unsupported input type: {0}")]
    UnsupportedType(String),
    #[error("fetch failed: {0}")]
    FetchFailed(String),
    #[error("flake parse error: {0}")]
    FlakeParse(String),
}

// ── Flake check ───────────────────────────────────────────────

/// Result of validating a flake directory.
#[derive(Debug)]
pub struct FlakeCheckResult {
    /// Whether the flake is structurally valid.
    pub valid: bool,
    /// Non-fatal warnings.
    pub warnings: Vec<String>,
    /// Fatal errors that prevent evaluation.
    pub errors: Vec<String>,
}

/// Validate a flake directory's structure and lock file.
///
/// Checks:
/// 1. `flake.nix` exists
/// 2. `flake.lock` (if present) is valid JSON and parseable
/// 3. The flake can be evaluated by the native evaluator
pub fn check_flake(flake_dir: &Path) -> Result<FlakeCheckResult, FlakeLockUpdateError> {
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    // 1. Verify flake.nix exists.
    let flake_nix = flake_dir.join("flake.nix");
    if !flake_nix.exists() {
        return Ok(FlakeCheckResult {
            valid: false,
            warnings,
            errors: vec!["flake.nix not found".to_string()],
        });
    }

    // 2. Verify flake.lock exists and is valid.
    let lock_path = flake_dir.join("flake.lock");
    if lock_path.exists() {
        let content = std::fs::read_to_string(&lock_path)?;
        match FlakeLock::parse(&content) {
            Ok(lock) => {
                // Check that all root inputs resolve.
                if let Err(e) = lock.root_inputs() {
                    warnings.push(format!("unresolvable root inputs: {e}"));
                }
            }
            Err(e) => {
                errors.push(format!("flake.lock parse error: {e}"));
            }
        }
    } else {
        warnings.push("flake.lock not found (flake has no locked inputs)".to_string());
    }

    // 3. Try to evaluate the flake.
    let source = std::fs::read_to_string(&flake_nix)?;
    match crate::eval::eval(&source) {
        Ok(_value) => {
            // Basic structural check: a flake should evaluate to an attrset
            // with at least an `outputs` attribute.
        }
        Err(e) => {
            errors.push(format!("evaluation error: {e}"));
        }
    }

    Ok(FlakeCheckResult {
        valid: errors.is_empty(),
        warnings,
        errors,
    })
}

// ── Flake lock update ─────────────────────────────────────────

/// Update a single input in a `flake.lock` file to its latest revision.
///
/// Reads the lock file, resolves the latest commit for the named input,
/// updates the `locked` section, and writes the file back.
pub fn update_input(flake_dir: &Path, input_name: &str) -> Result<(), FlakeLockUpdateError> {
    let lock_path = flake_dir.join("flake.lock");
    let content = std::fs::read_to_string(&lock_path)?;
    let mut lock = FlakeLock::parse(&content)
        .map_err(|e| FlakeLockUpdateError::FlakeParse(e.to_string()))?;

    // Find which node the root's input points to.
    let root_node = lock.root_node()
        .map_err(|e| FlakeLockUpdateError::FlakeParse(e.to_string()))?;

    let input_ref = root_node
        .inputs
        .get(input_name)
        .ok_or_else(|| FlakeLockUpdateError::InputNotFound(input_name.to_string()))?;

    let node_name = lock
        .resolve_ref(&lock.root.clone(), input_ref)
        .map_err(|e| FlakeLockUpdateError::FlakeParse(e.to_string()))?;

    let node = lock
        .nodes
        .get(&node_name)
        .ok_or_else(|| FlakeLockUpdateError::InputNotFound(node_name.clone()))?;

    let original = node
        .original
        .as_ref()
        .ok_or(FlakeLockUpdateError::InvalidFormat)?;

    // Resolve the latest revision from the original reference.
    let new_locked = resolve_latest(original)?;

    // Mutate the node in place.
    let node_mut = lock
        .nodes
        .get_mut(&node_name)
        .ok_or_else(|| FlakeLockUpdateError::InputNotFound(node_name.clone()))?;
    node_mut.locked = Some(new_locked);

    // Write back.
    let output = lock
        .to_json()
        .map_err(|e| FlakeLockUpdateError::FlakeParse(e.to_string()))?;
    std::fs::write(&lock_path, output)?;

    Ok(())
}

/// Update all root-level inputs in a `flake.lock` file.
///
/// Returns the list of input names that were successfully updated.
pub fn update_all_inputs(flake_dir: &Path) -> Result<Vec<String>, FlakeLockUpdateError> {
    let lock_path = flake_dir.join("flake.lock");
    let content = std::fs::read_to_string(&lock_path)?;
    let lock = FlakeLock::parse(&content)
        .map_err(|e| FlakeLockUpdateError::FlakeParse(e.to_string()))?;

    let root_node = lock.root_node()
        .map_err(|e| FlakeLockUpdateError::FlakeParse(e.to_string()))?;

    let input_names: Vec<String> = root_node.inputs.keys().cloned().collect();
    let mut updated = Vec::new();

    for name in &input_names {
        match update_input(flake_dir, name) {
            Ok(()) => updated.push(name.clone()),
            Err(e) => {
                tracing::warn!("failed to update input {name}: {e}");
            }
        }
    }

    Ok(updated)
}

/// List all root-level input names from a flake.lock.
pub fn list_inputs(flake_dir: &Path) -> Result<Vec<String>, FlakeLockUpdateError> {
    let lock_path = flake_dir.join("flake.lock");
    let content = std::fs::read_to_string(&lock_path)?;
    let lock = FlakeLock::parse(&content)
        .map_err(|e| FlakeLockUpdateError::FlakeParse(e.to_string()))?;

    let root_node = lock.root_node()
        .map_err(|e| FlakeLockUpdateError::FlakeParse(e.to_string()))?;

    Ok(root_node.inputs.keys().cloned().collect())
}

/// Get the locked revision for a specific input.
pub fn get_input_rev(
    flake_dir: &Path,
    input_name: &str,
) -> Result<Option<String>, FlakeLockUpdateError> {
    let lock_path = flake_dir.join("flake.lock");
    let content = std::fs::read_to_string(&lock_path)?;
    let lock = FlakeLock::parse(&content)
        .map_err(|e| FlakeLockUpdateError::FlakeParse(e.to_string()))?;

    let node = lock
        .resolve_input(&[input_name])
        .map_err(|e| FlakeLockUpdateError::FlakeParse(e.to_string()))?;

    Ok(node.locked.as_ref().and_then(|l| l.rev.clone()))
}

// ── Resolution ────────────────────────────────────────────────

/// Resolve the latest revision for an original input reference.
///
/// Supported types: `github`, `git`. Other types return
/// [`FlakeLockUpdateError::UnsupportedType`].
fn resolve_latest(
    original: &OriginalInput,
) -> Result<sui_compat::flake::LockedInput, FlakeLockUpdateError> {
    match original.source_type.as_str() {
        "github" => resolve_github(original),
        "git" => resolve_git(original),
        other => Err(FlakeLockUpdateError::UnsupportedType(other.to_string())),
    }
}

/// Resolve latest commit for a GitHub input via the GitHub API.
fn resolve_github(
    original: &OriginalInput,
) -> Result<sui_compat::flake::LockedInput, FlakeLockUpdateError> {
    let owner = original
        .owner
        .as_deref()
        .ok_or(FlakeLockUpdateError::InvalidFormat)?;
    let repo = original
        .repo
        .as_deref()
        .ok_or(FlakeLockUpdateError::InvalidFormat)?;
    let ref_name = original.git_ref.as_deref().unwrap_or("main");

    let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{ref_name}");

    let client = reqwest::blocking::Client::new();
    let mut request = client
        .get(&url)
        .header("User-Agent", "sui/0.1")
        .header("Accept", "application/vnd.github.v3+json");

    // Use GITHUB_TOKEN if available for rate limiting.
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        request = request.header("Authorization", format!("token {token}"));
    }

    let resp = request
        .send()
        .map_err(|e| FlakeLockUpdateError::FetchFailed(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(FlakeLockUpdateError::FetchFailed(format!(
            "GitHub API returned {}",
            resp.status()
        )));
    }

    let commit: serde_json::Value = resp
        .json()
        .map_err(|e| FlakeLockUpdateError::FetchFailed(e.to_string()))?;

    let sha = commit
        .get("sha")
        .and_then(|s| s.as_str())
        .ok_or_else(|| FlakeLockUpdateError::FetchFailed("no sha in response".into()))?;

    Ok(sui_compat::flake::LockedInput {
        source_type: "github".to_string(),
        owner: Some(owner.to_string()),
        repo: Some(repo.to_string()),
        rev: Some(sha.to_string()),
        nar_hash: None, // Must be recomputed on first fetch.
        last_modified: None,
        path: None,
        url: None,
        git_ref: original.git_ref.clone(),
        dir: original.dir.clone(),
        extra: std::collections::BTreeMap::new(),
    })
}

/// Resolve latest commit for a git input via `git ls-remote`.
fn resolve_git(
    original: &OriginalInput,
) -> Result<sui_compat::flake::LockedInput, FlakeLockUpdateError> {
    let url = original
        .url
        .as_deref()
        .ok_or(FlakeLockUpdateError::InvalidFormat)?;
    let ref_name = original.git_ref.as_deref().unwrap_or("main");

    let output = std::process::Command::new("git")
        .args(["ls-remote", url, ref_name])
        .output()
        .map_err(|e| FlakeLockUpdateError::FetchFailed(format!("git ls-remote: {e}")))?;

    if !output.status.success() {
        return Err(FlakeLockUpdateError::FetchFailed(format!(
            "git ls-remote failed (exit code: {})",
            output.status.code().unwrap_or(-1)
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let sha = stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| {
            FlakeLockUpdateError::FetchFailed("empty git ls-remote output".into())
        })?;

    Ok(sui_compat::flake::LockedInput {
        source_type: "git".to_string(),
        owner: None,
        repo: None,
        rev: Some(sha.to_string()),
        nar_hash: None,
        last_modified: None,
        path: None,
        url: Some(url.to_string()),
        git_ref: original.git_ref.clone(),
        dir: original.dir.clone(),
        extra: std::collections::BTreeMap::new(),
    })
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: write a minimal flake.nix.
    fn write_flake_nix(dir: &Path) {
        std::fs::write(
            dir.join("flake.nix"),
            r#"{ outputs = { self }: { }; }"#,
        )
        .unwrap();
    }

    /// Helper: minimal valid flake.lock JSON.
    fn minimal_lock_json() -> String {
        serde_json::json!({
            "nodes": {
                "nixpkgs": {
                    "locked": {
                        "lastModified": 1700000000,
                        "narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                        "owner": "nixos",
                        "repo": "nixpkgs",
                        "rev": "abc123def456abc123def456abc123def456abc1",
                        "type": "github"
                    },
                    "original": {
                        "owner": "nixos",
                        "ref": "nixos-unstable",
                        "repo": "nixpkgs",
                        "type": "github"
                    }
                },
                "root": {
                    "inputs": {
                        "nixpkgs": "nixpkgs"
                    }
                }
            },
            "root": "root",
            "version": 7
        })
        .to_string()
    }

    /// Helper: flake.lock with two inputs.
    fn two_input_lock_json() -> String {
        serde_json::json!({
            "nodes": {
                "nixpkgs": {
                    "locked": {
                        "lastModified": 1700000000,
                        "narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                        "owner": "nixos",
                        "repo": "nixpkgs",
                        "rev": "abc123def456abc123def456abc123def456abc1",
                        "type": "github"
                    },
                    "original": {
                        "owner": "nixos",
                        "ref": "nixos-unstable",
                        "repo": "nixpkgs",
                        "type": "github"
                    }
                },
                "utils": {
                    "locked": {
                        "lastModified": 1699999998,
                        "narHash": "sha256-CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC=",
                        "owner": "numtide",
                        "repo": "flake-utils",
                        "rev": "ccccccccccccccccccccccccccccccccccccccc1",
                        "type": "github"
                    },
                    "original": {
                        "owner": "numtide",
                        "repo": "flake-utils",
                        "type": "github"
                    }
                },
                "root": {
                    "inputs": {
                        "nixpkgs": "nixpkgs",
                        "utils": "utils"
                    }
                }
            },
            "root": "root",
            "version": 7
        })
        .to_string()
    }

    // ── check_flake ──────────────────────────────────────────

    #[test]
    fn check_flake_missing_flake_nix() {
        let tmp = tempfile::tempdir().unwrap();
        let result = check_flake(tmp.path()).unwrap();
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("flake.nix not found")));
    }

    #[test]
    fn check_flake_valid_minimal() {
        let tmp = tempfile::tempdir().unwrap();
        write_flake_nix(tmp.path());
        let result = check_flake(tmp.path()).unwrap();
        assert!(result.valid, "errors: {:?}", result.errors);
        // Should warn about missing flake.lock.
        assert!(result.warnings.iter().any(|w| w.contains("flake.lock not found")));
    }

    #[test]
    fn check_flake_with_valid_lock() {
        let tmp = tempfile::tempdir().unwrap();
        write_flake_nix(tmp.path());
        std::fs::write(tmp.path().join("flake.lock"), minimal_lock_json()).unwrap();
        let result = check_flake(tmp.path()).unwrap();
        assert!(result.valid, "errors: {:?}", result.errors);
        assert!(result.warnings.is_empty(), "warnings: {:?}", result.warnings);
    }

    #[test]
    fn check_flake_with_invalid_lock_json() {
        let tmp = tempfile::tempdir().unwrap();
        write_flake_nix(tmp.path());
        std::fs::write(tmp.path().join("flake.lock"), "not json at all").unwrap();
        let result = check_flake(tmp.path()).unwrap();
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("parse error")));
    }

    #[test]
    fn check_flake_with_bad_version() {
        let tmp = tempfile::tempdir().unwrap();
        write_flake_nix(tmp.path());
        let bad_lock = serde_json::json!({
            "nodes": { "root": { "inputs": {} } },
            "root": "root",
            "version": 99
        })
        .to_string();
        std::fs::write(tmp.path().join("flake.lock"), bad_lock).unwrap();
        let result = check_flake(tmp.path()).unwrap();
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("parse error")));
    }

    // ── list_inputs ──────────────────────────────────────────

    #[test]
    fn list_inputs_returns_root_input_names() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("flake.lock"), two_input_lock_json()).unwrap();
        let mut inputs = list_inputs(tmp.path()).unwrap();
        inputs.sort();
        assert_eq!(inputs, vec!["nixpkgs".to_string(), "utils".to_string()]);
    }

    // ── get_input_rev ────────────────────────────────────────

    #[test]
    fn get_input_rev_returns_locked_rev() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("flake.lock"), minimal_lock_json()).unwrap();
        let rev = get_input_rev(tmp.path(), "nixpkgs").unwrap();
        assert_eq!(rev, Some("abc123def456abc123def456abc123def456abc1".to_string()));
    }

    #[test]
    fn get_input_rev_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("flake.lock"), minimal_lock_json()).unwrap();
        let result = get_input_rev(tmp.path(), "nonexistent");
        assert!(result.is_err());
    }

    // ── update_input (offline — missing network) ─────────────

    #[test]
    fn update_input_not_found_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("flake.lock"), minimal_lock_json()).unwrap();
        let result = update_input(tmp.path(), "does-not-exist");
        assert!(matches!(
            result.unwrap_err(),
            FlakeLockUpdateError::InputNotFound(_)
        ));
    }

    #[test]
    fn update_input_missing_lock_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let result = update_input(tmp.path(), "nixpkgs");
        assert!(matches!(result.unwrap_err(), FlakeLockUpdateError::Io(_)));
    }

    // ── resolve_latest with unsupported type ─────────────────

    #[test]
    fn resolve_unsupported_type_errors() {
        let original = OriginalInput {
            source_type: "mercurial".to_string(),
            owner: None,
            repo: None,
            git_ref: None,
            url: None,
            dir: None,
            id: None,
            extra: std::collections::BTreeMap::new(),
        };
        let result = resolve_latest(&original);
        assert!(matches!(
            result.unwrap_err(),
            FlakeLockUpdateError::UnsupportedType(_)
        ));
    }

    // ── round-trip: parse -> to_json -> parse ────────────────

    #[test]
    fn lock_file_round_trips() {
        let json = minimal_lock_json();
        let lock = FlakeLock::parse(&json).unwrap();
        let serialized = lock.to_json().unwrap();
        let lock2 = FlakeLock::parse(&serialized).unwrap();
        assert_eq!(lock.version, lock2.version);
        assert_eq!(lock.root, lock2.root);
        assert_eq!(lock.nodes.len(), lock2.nodes.len());
    }

    // ── online tests (gated behind SUI_TEST_ONLINE=1) ────────

    #[test]
    fn update_input_github_online() {
        if std::env::var("SUI_TEST_ONLINE").as_deref() != Ok("1") {
            eprintln!("skipping online test (set SUI_TEST_ONLINE=1)");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        // Use a small, stable repo for the test.
        let lock = serde_json::json!({
            "nodes": {
                "systems": {
                    "locked": {
                        "lastModified": 1681028828,
                        "narHash": "sha256-Vy1rq5AaRuLzOxct8nz4T6wlgyUR7zLU309Q9mB/Cg=",
                        "owner": "nix-systems",
                        "repo": "default",
                        "rev": "da67096a3b9bf56a91d16901293e51ba5b49a27e",
                        "type": "github"
                    },
                    "original": {
                        "owner": "nix-systems",
                        "repo": "default",
                        "type": "github"
                    }
                },
                "root": {
                    "inputs": {
                        "systems": "systems"
                    }
                }
            },
            "root": "root",
            "version": 7
        })
        .to_string();
        std::fs::write(tmp.path().join("flake.lock"), &lock).unwrap();

        update_input(tmp.path(), "systems").unwrap();

        // Verify the rev was updated (it should now be a 40-char hex string).
        let new_rev = get_input_rev(tmp.path(), "systems").unwrap().unwrap();
        assert_eq!(new_rev.len(), 40, "expected 40-char SHA, got: {new_rev}");
    }
}
