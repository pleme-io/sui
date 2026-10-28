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

    let original = match declared_original(flake_dir, input_name) {
        Some(declared) => declared,
        None => node.original.clone().ok_or(FlakeLockUpdateError::InvalidFormat)?,
    };

    // Resolve the latest revision from the original reference.
    let new_locked = resolve_latest(&original)?;

    // Mutate the node in place.
    let node_mut = lock
        .nodes
        .get_mut(&node_name)
        .ok_or_else(|| FlakeLockUpdateError::InputNotFound(node_name.clone()))?;
    node_mut.locked = Some(new_locked);
    node_mut.original = Some(original);

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

// ── Stale overrides ───────────────────────────────────────────

/// An `inputs.<a>.inputs.<b>.follows` (or `.url`) aimed at an input `<a>`'s
/// flake does not declare: cppnix's `lockFlake` warning
/// `input '<a>' has an override for a non-existent input '<b>'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleOverride {
    pub input: String,
    pub missing: String,
}

impl std::fmt::Display for StaleOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "input '{}' has an override for a non-existent input '{}'", self.input, self.missing)
    }
}

/// Every stale override in the flake's input tree, by cppnix's rule
/// (`computeLocks` in libflake/flake.cc): overrides go into one map, the
/// outermost declaration kept; at each input path `P`, an override `P/x`
/// whose `x` is not one of that flake's inputs is stale. A kept input is
/// walked from its lock entry and its own flake.nix is not read; it is read
/// only when cppnix would refetch it — an untrusted node holding a `follows`
/// that no override still declares. A source is never fetched.
pub fn stale_overrides(flake_dir: &Path) -> Result<Vec<StaleOverride>, FlakeLockUpdateError> {
    let lock: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(flake_dir.join("flake.lock"))?)?;
    let nodes = lock.get("nodes").and_then(serde_json::Value::as_object).ok_or(FlakeLockUpdateError::InvalidFormat)?;
    let root = lock.get("root").and_then(serde_json::Value::as_str).unwrap_or("root");
    let mut overrides = std::collections::BTreeSet::new();
    let mut found = Vec::new();
    walk_overrides(nodes, root, &[], Some(flake_dir.to_path_buf()), false, &mut overrides, &mut found);
    Ok(found)
}

fn walk_overrides(
    nodes: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    prefix: &[String],
    source: Option<std::path::PathBuf>,
    trust_lock: bool,
    overrides: &mut std::collections::BTreeSet<Vec<String>>,
    found: &mut Vec<StaleOverride>,
) {
    let Some(node) = nodes.get(key) else { return };
    let empty = serde_json::Map::new();
    let locked_inputs = node.get("inputs").and_then(serde_json::Value::as_object).unwrap_or(&empty);
    let declared = source
        .and_then(|dir| declared_overrides(&dir, prefix, overrides))
        .unwrap_or_else(|| locked_inputs.keys().cloned().collect());
    for path in overrides.iter() {
        if path.len() == prefix.len() + 1 && path[..prefix.len()] == *prefix && !declared.contains(&path[prefix.len()]) {
            found.push(StaleOverride { input: prefix.join("/"), missing: path[prefix.len()].clone() });
        }
    }
    for (id, target) in locked_inputs {
        let Some(child) = target.as_str() else { continue };
        let Some(child_node) = nodes.get(child) else { continue };
        if child_node.get("flake").and_then(serde_json::Value::as_bool) == Some(false) {
            continue;
        }
        let mut path = prefix.to_vec();
        path.push(id.clone());
        let must_refetch = !trust_lock
            && child_node.get("inputs").and_then(serde_json::Value::as_object).is_some_and(|inputs| {
                inputs.iter().any(|(i, t)| {
                    let mut over = path.clone();
                    over.push(i.clone());
                    t.is_array() && !overrides.contains(&over)
                })
            });
        let source = if must_refetch { locked_source(child_node) } else { None };
        walk_overrides(nodes, child, &path, source, !must_refetch, overrides, found);
    }
}

fn locked_source(node: &serde_json::Value) -> Option<std::path::PathBuf> {
    let locked = node.get("locked")?;
    let nar_hash = locked.get("narHash")?.as_str()?;
    let digest = sui_compat::hash::decode_hash_any(sui_compat::hash::HashAlgorithm::Sha256, nar_hash).ok()?;
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let in_store = std::path::PathBuf::from(sui_compat::store_path::make_source_path(&hex, &[], false, "source"));
    let base = if in_store.join("flake.nix").exists() || in_store.is_dir() {
        in_store
    } else {
        crate::fetcher::cached_input_dir(nar_hash)?
    };
    let dir = locked.get("dir").and_then(serde_json::Value::as_str).map_or(base.clone(), |d| base.join(d));
    dir.join("flake.nix").exists().then_some(dir)
}

fn declared_overrides(
    dir: &Path,
    prefix: &[String],
    overrides: &mut std::collections::BTreeSet<Vec<String>>,
) -> Option<std::collections::BTreeSet<String>> {
    let flake_nix = dir.join("flake.nix");
    let source = std::fs::read_to_string(&flake_nix).ok()?;
    let flake = crate::eval::eval_with_file(&source, Some(flake_nix)).ok()?;
    let Some(inputs) = flake.to_attrs().ok()?.get("inputs").cloned() else {
        return Some(std::collections::BTreeSet::new());
    };
    let inputs = crate::eval::force_value(&inputs).ok()?.to_attrs().ok()?;
    let mut names = std::collections::BTreeSet::new();
    for (id, input) in inputs.iter() {
        let mut path = prefix.to_vec();
        path.push(id.clone());
        collect_overrides(input, &path, overrides);
        names.insert(id);
    }
    Some(names)
}

fn collect_overrides(input: &crate::Value, path: &[String], overrides: &mut std::collections::BTreeSet<Vec<String>>) {
    let Ok(input) = crate::eval::force_value(input) else { return };
    let Ok(attrs) = input.to_attrs() else { return };
    let Some(nested) = attrs.get("inputs") else { return };
    let Ok(nested) = crate::eval::force_value(nested) else { return };
    let Ok(nested) = nested.to_attrs() else { return };
    for (id, over) in nested.iter() {
        let mut child = path.to_vec();
        child.push(id);
        let is_ref = crate::eval::force_value(over)
            .ok()
            .and_then(|o| o.to_attrs().ok())
            .is_some_and(|o| o.keys().any(|k| k != "inputs" && k != "flake"));
        if is_ref {
            overrides.insert(child.clone());
        }
        collect_overrides(over, &child, overrides);
    }
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
pub(crate) fn resolve_github(
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
    let ref_name = pinned_rev(original).or(original.git_ref.as_deref()).unwrap_or("HEAD");

    let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{ref_name}");

    let mut request = crate::fetcher::http_agent()
        .get(&url)
        .header("User-Agent", "sui/0.1")
        .header("Accept", "application/vnd.github.v3+json");

    if let Some(token) = crate::fetcher::github_token_for_url(&url)
        .map_err(|e| FlakeLockUpdateError::FetchFailed(e.to_string()))?
    {
        request = request.header("Authorization", &format!("token {token}"));
    }

    let resp = request
        .call()
        .map_err(|e| FlakeLockUpdateError::FetchFailed(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(FlakeLockUpdateError::FetchFailed(format!(
            "GitHub API returned {}",
            resp.status().as_u16()
        )));
    }

    let body = resp
        .into_body()
        .read_to_string()
        .map_err(|e| FlakeLockUpdateError::FetchFailed(e.to_string()))?;

    let commit: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| FlakeLockUpdateError::FetchFailed(e.to_string()))?;

    let sha = commit
        .get("sha")
        .and_then(|s| s.as_str())
        .ok_or_else(|| FlakeLockUpdateError::FetchFailed("no sha in response".into()))?;

    let last_modified = commit
        .pointer("/commit/committer/date")
        .and_then(|d| d.as_str())
        .and_then(rfc3339_utc_seconds)
        .ok_or_else(|| FlakeLockUpdateError::FetchFailed(format!("no committer date for {owner}/{repo}@{sha}")))?;
    let fetch_key = sui_compat::flake::LockedInput {
        source_type: "github".to_string(),
        owner: Some(owner.to_string()),
        repo: Some(repo.to_string()),
        rev: Some(sha.to_string()),
        ..sui_compat::flake::LockedInput::default()
    };
    let tree = crate::fetcher::InputFetcher::new()
        .fetch(&fetch_key)
        .map_err(|e| FlakeLockUpdateError::FetchFailed(format!("{owner}/{repo}@{sha}: {e}")))?;
    let nar_hash = sui_compat::source::nar_hash_source_tree(&tree, "source")
        .map_err(|e| FlakeLockUpdateError::FetchFailed(format!("narHash {owner}/{repo}@{sha}: {e}")))?
        .nar_hash_sri;

    Ok(github_locked(original, owner, repo, sha, nar_hash, last_modified))
}

fn github_locked(
    original: &OriginalInput,
    owner: &str,
    repo: &str,
    sha: &str,
    nar_hash: String,
    last_modified: u64,
) -> sui_compat::flake::LockedInput {
    sui_compat::flake::LockedInput {
        source_type: "github".to_string(),
        owner: Some(owner.to_string()),
        repo: Some(repo.to_string()),
        rev: Some(sha.to_string()),
        nar_hash: Some(nar_hash),
        last_modified: Some(last_modified),
        path: None,
        url: None,
        git_ref: None,
        dir: original.dir.clone(),
        host: None,
        extra: std::collections::BTreeMap::new(),
    }
}

fn declared_original(flake_dir: &Path, input_name: &str) -> Option<OriginalInput> {
    let flake_nix = flake_dir.join("flake.nix");
    let source = std::fs::read_to_string(&flake_nix).ok()?;
    let flake = crate::eval::eval_with_file(&source, Some(flake_nix)).ok()?;
    let inputs = crate::eval::force_value(flake.to_attrs().ok()?.get("inputs")?).ok()?;
    let input = crate::eval::force_value(inputs.to_attrs().ok()?.get(input_name)?).ok()?;
    let url = match &input {
        crate::Value::Attrs(a) => crate::eval::force_value(a.get("url")?).ok()?.as_string().ok()?.to_string(),
        other => other.as_string().ok()?.to_string(),
    };
    github_original(&url)
}

fn github_original(url: &str) -> Option<OriginalInput> {
    let rest = url.strip_prefix("github:")?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut parts = path.splitn(3, '/');
    let (owner, repo, third) = (parts.next()?, parts.next()?, parts.next());
    let mut value = serde_json::json!({ "type": "github", "owner": owner, "repo": repo });
    if let Some(third) = third.filter(|t| !t.is_empty()) {
        let key = if third.len() == 40 && third.bytes().all(|b| b.is_ascii_hexdigit()) { "rev" } else { "ref" };
        value[key] = serde_json::Value::String(third.to_string());
    }
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=')?;
        if matches!(k, "ref" | "rev" | "dir" | "host") {
            value[k] = serde_json::Value::String(v.to_string());
        }
    }
    serde_json::from_value(value).ok()
}

fn pinned_rev(original: &OriginalInput) -> Option<&str> {
    original.extra.get("rev").and_then(serde_json::Value::as_str)
}

fn rfc3339_utc_seconds(date: &str) -> Option<u64> {
    let date = date.strip_suffix('Z')?;
    let (day, time) = date.split_once('T')?;
    let mut d = day.split('-').map(str::parse::<i64>);
    let (y, m, dd) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':').map(str::parse::<i64>);
    let (hh, mm, ss) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + dd - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3_600 + mm * 60 + ss).ok()
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

    let sha = match pinned_rev(original) {
        Some(rev) => rev.to_string(),
        None => crate::git::ls_remote(url, ref_name)
            .map_err(|e| FlakeLockUpdateError::FetchFailed(format!("git ls-remote: {e}")))?,
    };

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
        host: None,
        extra: std::collections::BTreeMap::new(),
    })
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    fn stale_fixture(flake_nix: &str, lock: &serde_json::Value) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("flake.nix"), flake_nix).unwrap();
        std::fs::write(dir.path().join("flake.lock"), serde_json::to_string(lock).unwrap()).unwrap();
        dir
    }

    #[test]
    fn an_override_of_an_input_the_flake_lacks_is_stale_and_named_by_its_path() {
        let dir = stale_fixture(
            r#"{
              inputs.devenv.url = "github:o/devenv";
              inputs.devenv.inputs.cachix.follows = "";
              inputs.devenv.inputs.nixpkgs.follows = "nixpkgs";
              inputs.m.url = "github:o/m";
              inputs.m.inputs.h.inputs.forge.follows = "";
              inputs.nixpkgs.url = "github:o/nixpkgs";
              outputs = _: {};
            }"#,
            &serde_json::json!({
              "root": "root", "version": 7,
              "nodes": {
                "root": {"inputs": {"devenv": "devenv", "m": "m", "nixpkgs": "nixpkgs"}},
                "devenv": {"inputs": {"nixpkgs": ["nixpkgs"]}},
                "m": {"inputs": {"h": "h"}},
                "h": {"inputs": {"nixpkgs": ["nixpkgs"]}},
                "nixpkgs": {}
              }
            }),
        );
        let found = stale_overrides(dir.path()).unwrap();
        assert_eq!(
            found.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![
                "input 'devenv' has an override for a non-existent input 'cachix'".to_string(),
                "input 'm/h' has an override for a non-existent input 'forge'".to_string(),
            ]
        );
    }

    #[test]
    fn an_override_of_a_declared_input_is_not_stale() {
        let dir = stale_fixture(
            r#"{
              inputs.a.url = "github:o/a";
              inputs.a.inputs.nixpkgs.follows = "nixpkgs";
              inputs.nixpkgs.url = "github:o/nixpkgs";
              outputs = _: {};
            }"#,
            &serde_json::json!({
              "root": "root", "version": 7,
              "nodes": {
                "root": {"inputs": {"a": "a", "nixpkgs": "nixpkgs"}},
                "a": {"inputs": {"nixpkgs": ["nixpkgs"]}},
                "nixpkgs": {}
              }
            }),
        );
        assert!(stale_overrides(dir.path()).unwrap().is_empty());
    }

    use super::*;

    #[test]
    fn a_github_url_reads_as_its_original() {
        let pinned = github_original("github:nix-community/fenix/e691190ae78c9551d1e6a0cba156555c8f7cceca").unwrap();
        assert_eq!(pinned_rev(&pinned), Some("e691190ae78c9551d1e6a0cba156555c8f7cceca"));
        assert_eq!(pinned.git_ref, None);
        let branch = github_original("github:LnL7/nix-darwin/nix-darwin-25.11").unwrap();
        assert_eq!(branch.git_ref.as_deref(), Some("nix-darwin-25.11"));
        assert_eq!(pinned_rev(&branch), None);
        let dir = github_original("github:pleme-io/blue?dir=bidamas/kirikae").unwrap();
        assert_eq!(dir.dir.as_deref(), Some("bidamas/kirikae"));
        assert!(github_original("git+https://example.com/x").is_none());
    }

    #[test]
    fn a_changed_url_in_flake_nix_wins_over_the_lock_original() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("flake.nix"),
            r#"{ inputs.fenix.url = "github:nix-community/fenix/e691190ae78c9551d1e6a0cba156555c8f7cceca"; inputs.plain = { url = "github:pleme-io/sui"; }; outputs = _: { }; }"#,
        )
        .unwrap();
        assert_eq!(
            declared_original(dir.path(), "fenix").as_ref().and_then(pinned_rev),
            Some("e691190ae78c9551d1e6a0cba156555c8f7cceca")
        );
        assert_eq!(declared_original(dir.path(), "plain").map(|o| o.repo), Some(Some("sui".to_string())));
        assert!(declared_original(dir.path(), "absent").is_none());
    }

    #[test]
    fn a_rev_in_the_input_url_is_the_rev_that_is_locked() {
        let pinned: OriginalInput = serde_json::from_value(serde_json::json!({
            "type": "github", "owner": "pleme-io", "repo": "hibiki", "rev": "aecc5a9c014772f0f4006c7f47d362992ad3290f"
        }))
        .unwrap();
        assert_eq!(pinned_rev(&pinned), Some("aecc5a9c014772f0f4006c7f47d362992ad3290f"));
        let floating: OriginalInput = serde_json::from_value(serde_json::json!({
            "type": "github", "owner": "pleme-io", "repo": "hibiki", "ref": "main"
        }))
        .unwrap();
        assert_eq!(pinned_rev(&floating), None);
    }

    #[test]
    fn a_locked_github_node_carries_rev_and_never_ref() {
        let original: OriginalInput = serde_json::from_value(serde_json::json!({
            "type": "github", "owner": "LnL7", "repo": "nix-darwin", "ref": "nix-darwin-25.11"
        }))
        .unwrap();
        let locked = github_locked(&original, "LnL7", "nix-darwin", "ebec37af18215214173c98cf6356d0aca24a2585", "sha256-x".into(), 1);
        let json = serde_json::to_value(&locked).unwrap();
        assert_eq!(json["rev"], "ebec37af18215214173c98cf6356d0aca24a2585");
        assert!(json.get("ref").is_none(), "{json}");
    }

    #[test]
    fn committer_date_converts_to_the_last_modified_nix_records() {
        assert_eq!(rfc3339_utc_seconds("2026-10-24T22:48:51Z"), Some(1_792_882_131));
        assert_eq!(rfc3339_utc_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_utc_seconds("2000-02-29T12:00:00Z"), Some(951_825_600));
        assert_eq!(rfc3339_utc_seconds("2026-10-24 22:48:51"), None);
    }

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
