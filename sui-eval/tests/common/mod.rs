//! Shared oracle helpers for sui integration tests.
//!
//! These helpers let a test compare sui's evaluator against a real Nix
//! installation sitting on the machine. The oracle is `nix-instantiate
//! --eval --json --strict`, which produces JSON in the same shape as
//! `sui_eval::Value::to_json`, so the comparison is a plain structural
//! `serde_json::Value` diff.
//!
//! ## Run modes
//!
//! - `cargo test` — offline mode. Tests that require the oracle are
//!   skipped via [`skip_if_offline`] unless `SUI_TEST_ONLINE=1` is set.
//! - `SUI_TEST_ONLINE=1 cargo test` — online mode. Oracle-backed tests
//!   run and shell out to `nix-instantiate`, `nix-store`, `nix path-info`
//!   as needed.
//!
//! Oracle-backed tests also *skip* (not fail) when the nix CLIs are not
//! on `PATH`, so contributors on non-nix machines don't see red X's.
//!
//! ## Corpus discovery
//!
//! Two environment variables (both optional) let CI and contributors
//! point the harness at different roots:
//!
//! - `PLEME_IO_ROOT` — directory containing cloned pleme-io repos,
//!   defaults to `~/code/github/pleme-io`.
//! - `NIX_STORE_ROOT` — the store directory, defaults to `/nix/store`.
//!
//! Corpus samples are deterministic: the discovery functions sort paths
//! lexicographically and take the first `n`, so repeated runs on the
//! same machine produce identical test sets.

#![allow(dead_code)] // helpers are consumed by multiple integration test files

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

// ── Lisp-authored corpus (oracle + perf + specs) ────────────────────────

/// A single oracle test case. The fields mirror the Lisp authoring
/// surface exactly; `#[derive(TataraDomain)]` turns this into the
/// `(defnix …)` keyword.
///
/// `expected_json` is a **raw JSON string** parsed at test time. Using
/// a string rather than `serde_json::Value` sidesteps the Sexp → JSON
/// round-trip ambiguity in tatara-lisp (no `[]`/`{}` literals; lists
/// and kwargs both use `(…)`). Authors write the JSON they mean; the
/// harness parses it.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[tatara(keyword = "defnix")]
pub struct NixProgramSpec {
    /// The Nix source text to evaluate.
    pub source: String,
    /// Expected result as raw JSON. Parsed into `serde_json::Value`
    /// at test time; diffed against `sui_eval::eval(source).to_json()`.
    /// When `expected_error` is set, this field is ignored (use `"null"`
    /// as a placeholder or omit in future schema revisions).
    pub expected_json: String,
    /// When set, this program is expected to FAIL, and the error
    /// message from sui (and from CppNix in the differential oracle)
    /// must contain this substring. Typical values:
    /// `"infinite recursion"`, `"undefined variable"`, `"type error"`,
    /// `"assertion"`, `"assertion failed"`, `"abort"`. The match is
    /// case-insensitive + substring-only so it survives minor wording
    /// drift between Nix versions.
    ///
    /// Error-case coverage closes the class of bugs where sui returns
    /// `Ok(wrong_value)` on programs CppNix rejects — the fix at
    /// `ac7ce0a` was that bug's concrete instance.
    #[serde(default)]
    pub expected_error: String,
    /// Optional categorization — `("arith" "trivial")` style.
    #[serde(default)]
    pub tags: Vec<String>,
    /// When true, skip the case. Used by `99_executable_specs.lisp`
    /// to document unimplemented builtins as expected-behavior specs.
    #[serde(default)]
    pub skip: bool,
    /// Human-readable rationale. Shown on failure. Optional.
    #[serde(default)]
    pub note: String,
}

/// Load every `.lisp` file under `tests/oracle_corpus/` and compile
/// each one into a stream of `NamedDefinition<NixProgramSpec>` via
/// `tatara_lisp::compile_named`. Sort order is deterministic — file
/// name ascending — so perf reports are stable across runs.
#[must_use]
pub fn load_corpus() -> Vec<tatara_lisp::NamedDefinition<NixProgramSpec>> {
    let corpus_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("oracle_corpus");

    let mut paths: Vec<PathBuf> = std::fs::read_dir(&corpus_dir)
        .unwrap_or_else(|e| panic!("corpus dir {}: {e}", corpus_dir.display()))
        .filter_map(|e| e.ok().map(|d| d.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("lisp"))
        .collect();
    paths.sort();

    let mut out = Vec::new();
    for path in paths {
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let mut defs = tatara_lisp::compile_named::<NixProgramSpec>(&src)
            .unwrap_or_else(|e| panic!("compile {}: {e}", path.display()));
        for def in &mut defs {
            def.name = format!(
                "{}::{}",
                path.file_stem().and_then(|s| s.to_str()).unwrap_or("?"),
                def.name
            );
        }
        out.extend(defs);
    }
    out
}

/// Convert a sui `Value` to a `serde_json::Value`. Thin wrapper kept
/// for naming clarity at call sites.
#[must_use]
pub fn value_to_json(v: &sui_eval::Value) -> serde_json::Value {
    v.to_json()
}

/// Returns `true` when `SUI_TEST_ONLINE=1` is set in the environment.
///
/// Oracle-backed tests gate on this to stay green on CI without nix.
pub fn online_mode() -> bool {
    env::var("SUI_TEST_ONLINE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Returns `true` when `nix-instantiate` is discoverable on `PATH`.
pub fn nix_available() -> bool {
    which("nix-instantiate").is_some()
}

/// Returns `true` when `nix-store` is discoverable on `PATH`.
pub fn nix_store_available() -> bool {
    which("nix-store").is_some()
}

/// If the oracle is not available, emit a skip note and return `true`
/// so the caller can early-return. Otherwise return `false`.
///
/// Call sites look like:
/// ```ignore
/// if common::skip_if_offline("diff_primitive") { return; }
/// ```
pub fn skip_if_offline(test_name: &str) -> bool {
    if !online_mode() {
        eprintln!("skip {test_name}: SUI_TEST_ONLINE not set");
        return true;
    }
    if !nix_available() {
        eprintln!("skip {test_name}: nix-instantiate not on PATH");
        return true;
    }
    false
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ── Oracle: run real nix-instantiate ────────────────────────────────────

/// Evaluate `expr` with real `nix-instantiate --eval --json --strict`.
///
/// Returns the parsed JSON on success, or an `__error` object on failure
/// (so "both failed" comparisons still work).
///
/// Expressions whose first character is `-` would otherwise be
/// parsed as a CLI flag; they are wrapped in an identity `(expr)`
/// so real nix sees them as a value.
pub fn nix_eval_json(expr: &str) -> serde_json::Value {
    let wrapped;
    let passed: &str = if expr.trim_start().starts_with('-') {
        wrapped = format!("({expr})");
        &wrapped
    } else {
        expr
    };
    run_nix_instantiate(&["--eval", "--json", "--strict", "-E", passed])
}

/// Evaluate a file with real `nix-instantiate --eval --json --strict <file>`.
pub fn nix_eval_json_file(path: &Path) -> serde_json::Value {
    run_nix_instantiate(&["--eval", "--json", "--strict", &path.to_string_lossy()])
}

fn run_nix_instantiate(args: &[&str]) -> serde_json::Value {
    let output = match Command::new("nix-instantiate").args(args).output() {
        Ok(o) => o,
        Err(e) => return error_json(format!("spawn nix-instantiate failed: {e}")),
    };
    if !output.status.success() {
        return error_json(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    // nix-instantiate prints warnings on stderr (e.g. json-format deprecation);
    // we only care about stdout. Parse it; if it isn't JSON, wrap as error.
    let stdout = String::from_utf8_lossy(&output.stdout);
    match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(e) => error_json(format!("nix stdout not json: {e}: {stdout}")),
    }
}

// ── sui side ────────────────────────────────────────────────────────────

/// Evaluate `expr` with sui's tree-walking evaluator and return JSON.
pub fn sui_eval_json(expr: &str) -> serde_json::Value {
    match sui_eval::eval(expr) {
        Ok(v) => v.to_json(),
        Err(e) => error_json(format!("{e}")),
    }
}

/// Evaluate a `.nix` file with sui's tree-walking evaluator and return JSON.
pub fn sui_eval_json_file(path: &Path) -> serde_json::Value {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return error_json(format!("read {}: {e}", path.display())),
    };
    match sui_eval::eval(&source) {
        Ok(v) => v.to_json(),
        Err(e) => error_json(format!("{e}")),
    }
}

fn error_json(msg: impl Into<String>) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert(
        "__error".to_string(),
        serde_json::Value::String(msg.into()),
    );
    serde_json::Value::Object(map)
}

/// Returns true when the given JSON is a normalized error object.
pub fn is_error_json(v: &serde_json::Value) -> bool {
    v.get("__error").is_some()
}

// ── Differential assertions ─────────────────────────────────────────────

/// Evaluate `expr` with both real nix and sui; assert JSON is equal.
///
/// Silently no-ops in offline mode so you can pepper these across
/// test files without blowing up non-nix CI.
pub fn assert_eq_nix(expr: &str) {
    if skip_if_offline("assert_eq_nix") {
        return;
    }
    let oracle = nix_eval_json(expr);
    let ours = sui_eval_json(expr);
    if oracle != ours {
        let oracle_s = serde_json::to_string(&oracle).unwrap_or_default();
        let ours_s = serde_json::to_string(&ours).unwrap_or_default();
        panic!(
            "differential mismatch on {expr:?}\n  nix:  {oracle_s}\n  sui:  {ours_s}"
        );
    }
}

/// Same as [`assert_eq_nix`] but for a file on disk.
pub fn assert_eq_nix_file(path: &Path) {
    if skip_if_offline("assert_eq_nix_file") {
        return;
    }
    let oracle = nix_eval_json_file(path);
    let ours = sui_eval_json_file(path);
    if oracle != ours {
        let oracle_s = serde_json::to_string(&oracle).unwrap_or_default();
        let ours_s = serde_json::to_string(&ours).unwrap_or_default();
        panic!(
            "differential mismatch on {}\n  nix:  {oracle_s}\n  sui:  {ours_s}",
            path.display()
        );
    }
}

// ── Corpus discovery ────────────────────────────────────────────────────

/// Root directory containing the pleme-io cloned repos.
pub fn pleme_io_root() -> PathBuf {
    if let Ok(v) = env::var("PLEME_IO_ROOT") {
        return PathBuf::from(v);
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("code/github/pleme-io")
}

/// Root directory of the local nix store.
pub fn nix_store_root() -> PathBuf {
    env::var("NIX_STORE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/nix/store"))
}

/// Return up to `n` `flake.nix` files under `PLEME_IO_ROOT`, lex-sorted.
///
/// Silently returns empty if the root doesn't exist so tests degrade
/// gracefully on non-developer machines.
pub fn pleme_io_flake_nix_sample(n: usize) -> Vec<PathBuf> {
    collect_files_by_name(&pleme_io_root(), "flake.nix", n, 3)
}

/// Return up to `n` `flake.lock` files under `PLEME_IO_ROOT`, lex-sorted.
pub fn pleme_io_flake_lock_sample(n: usize) -> Vec<PathBuf> {
    collect_files_by_name(&pleme_io_root(), "flake.lock", n, 3)
}

/// Return up to `n` top-level `*.drv` files under `NIX_STORE_ROOT`,
/// lex-sorted. Non-`.drv` entries and subdirectories are skipped.
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

/// Return up to `n` top-level store entries that are NOT `.drv` files
/// (suitable NAR test subjects), lex-sorted.
pub fn nix_store_path_sample(n: usize) -> Vec<PathBuf> {
    let root = nix_store_root();
    if !root.is_dir() {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = match std::fs::read_dir(&root) {
        Ok(it) => it
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) != Some("drv"))
            .filter(|p| {
                // Skip hidden entries and the db/sqlite files.
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| !n.starts_with('.') && n != ".links" && n != ".db")
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    out.sort();
    out.truncate(n);
    out
}

/// Recursively collect files named `file_name` under `root` up to
/// `max_depth` levels deep. Hidden directories (including `.git`,
/// `target`, `node_modules`, `result`, `result-*`) are skipped so we
/// don't walk into build artifacts.
fn collect_files_by_name(
    root: &Path,
    file_name: &str,
    limit: usize,
    max_depth: usize,
) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    walk(root, file_name, max_depth, &mut out);
    out.sort();
    out.truncate(limit);
    out
}

fn walk(dir: &Path, file_name: &str, depth_remaining: usize, out: &mut Vec<PathBuf>) {
    const SKIP_DIRS: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        "result",
        "dist",
        "build",
        ".direnv",
    ];
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if path.is_file() && name == file_name {
            out.push(path);
            continue;
        }
        if path.is_dir() && depth_remaining > 0 {
            if name.starts_with('.') && name != "." {
                continue;
            }
            if SKIP_DIRS.contains(&name.as_str()) || name.starts_with("result-") {
                continue;
            }
            walk(&path, file_name, depth_remaining - 1, out);
        }
    }
}

// ── Sanity tests for the helpers themselves ─────────────────────────────

#[cfg(test)]
mod sanity {
    use super::*;

    #[test]
    fn online_mode_defaults_off() {
        // If the developer already exported the var, just note it.
        if env::var("SUI_TEST_ONLINE").is_ok() {
            return;
        }
        assert!(!online_mode());
    }

    #[test]
    fn pleme_io_root_is_resolvable_even_if_missing() {
        let _ = pleme_io_root();
    }

    #[test]
    fn nix_store_root_defaults() {
        let root = nix_store_root();
        assert!(!root.as_os_str().is_empty());
    }
}
