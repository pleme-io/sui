//! Typed border for the flake registry — the lookup that resolves
//! short input refs (`nixpkgs`, `github:owner/repo`) to concrete
//! `from` → `to` mappings.
//!
//! Cppnix has three registries (system, user, flake-local), with
//! precedence order.  Each is a JSON file with `version` + `flakes`
//! (list of entries: `from`, `to`, `exact`).

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defregistry-format")]
pub struct RegistryFormat {
    pub name: String,
    pub version: u32,
    pub scope: RegistryScope,
    /// Precedence rank — lower = checked first.  cppnix:
    /// flake-local 0, user 1, system 2.
    pub precedence: u32,
    /// Path the registry conventionally lives at.
    #[serde(rename = "defaultPath")]
    pub default_path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegistryScope {
    /// Per-flake `nixConfig.flake-registry` entries.
    FlakeLocal,
    /// `~/.config/nix/registry.json`.
    User,
    /// `/etc/nix/registry.json`.
    System,
    /// The upstream flake registry from `flake-registry` setting
    /// (typically `https://channels.nixos.org/flake-registry.json`).
    Global,
}

pub const CANONICAL_REGISTRY_LISP: &str =
    include_str!("../specs/registry.lisp");

/// Compile every authored registry format.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<RegistryFormat>, SpecError> {
    crate::loader::load_all::<RegistryFormat>(CANONICAL_REGISTRY_LISP)
}

// ── M3.0 registry resolver ─────────────────────────────────────────

/// One registry entry — a `from` → `to` mapping with optional
/// `exact` flag (cppnix lockfile semantics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    pub from: String,
    pub to: String,
    pub exact: bool,
}

/// Resolved registry entries grouped by scope, in precedence
/// order (lowest first wins).
pub type Registries = Vec<(RegistryScope, Vec<RegistryEntry>)>;

/// Resolve a flake reference through the precedence chain.
/// Returns the FIRST match (lowest-precedence scope wins per
/// cppnix convention).
///
/// # Errors
///
/// `registry-unresolved` if no scope has a matching entry.
pub fn resolve(
    registries: &Registries,
    flake_ref: &str,
) -> Result<RegistryEntry, SpecError> {
    let mut sorted: Vec<&(RegistryScope, Vec<RegistryEntry>)> =
        registries.iter().collect();
    sorted.sort_by_key(|(scope, _)| scope_precedence(*scope));
    for (_, entries) in sorted {
        for entry in entries {
            if entry.from == flake_ref {
                return Ok(entry.clone());
            }
        }
    }
    Err(SpecError::Interp {
        phase: "registry-unresolved".into(),
        message: format!("no registry entry for `{flake_ref}` across any scope"),
    })
}

fn scope_precedence(scope: RegistryScope) -> u32 {
    match scope {
        RegistryScope::FlakeLocal => 0,
        RegistryScope::User       => 1,
        RegistryScope::System     => 2,
        RegistryScope::Global     => 3,
    }
}

// ── M3.1 disk loader ───────────────────────────────────────────────

/// Parse a registry JSON document into `Vec<RegistryEntry>`.
///
/// cppnix registry shape (v2):
/// ```json
/// {
///   "version": 2,
///   "flakes": [
///     { "from": {"type": "indirect", "id": "nixpkgs"},
///       "to":   {"type": "github", "owner": "NixOS", "repo": "nixpkgs"},
///       "exact": false }
///   ]
/// }
/// ```
///
/// Returns entries with `from` flattened to its `id` (per cppnix
/// `indirect` semantics) and `to` flattened to a typed ref string.
///
/// # Errors
///
/// - `registry-parse` if the JSON shape is invalid.
/// - `registry-version` if the version isn't 2.
pub fn parse_entries(text: &str) -> Result<Vec<RegistryEntry>, SpecError> {
    let doc: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| SpecError::Interp {
            phase: "registry-parse".into(),
            message: format!("invalid JSON: {e}"),
        })?;

    let version = doc.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
    if version != 2 {
        return Err(SpecError::Interp {
            phase: "registry-version".into(),
            message: format!("expected version 2, got {version}"),
        });
    }

    let flakes = doc.get("flakes").and_then(|v| v.as_array())
        .ok_or_else(|| SpecError::Interp {
            phase: "registry-parse".into(),
            message: "missing `flakes` array".into(),
        })?;

    let mut out = Vec::with_capacity(flakes.len());
    for (i, entry) in flakes.iter().enumerate() {
        let from = entry.get("from").ok_or_else(|| SpecError::Interp {
            phase: "registry-parse".into(),
            message: format!("flake #{i}: missing `from`"),
        })?;
        let to = entry.get("to").ok_or_else(|| SpecError::Interp {
            phase: "registry-parse".into(),
            message: format!("flake #{i}: missing `to`"),
        })?;
        let exact = entry.get("exact").and_then(|v| v.as_bool()).unwrap_or(false);

        out.push(RegistryEntry {
            from: flatten_ref(from),
            to: flatten_ref(to),
            exact,
        });
    }
    Ok(out)
}

/// Convert a registry input/output ref object into a string.
///
/// cppnix encodes refs as objects with a `type` discriminator
/// and per-type fields.  We flatten them into the human-readable
/// shorthand operators see (e.g. `nixpkgs`, `github:NixOS/nixpkgs`).
fn flatten_ref(v: &serde_json::Value) -> String {
    let kind = v.get("type").and_then(|x| x.as_str()).unwrap_or("?");
    match kind {
        "indirect" => v.get("id").and_then(|x| x.as_str())
            .unwrap_or("?").to_string(),
        "github" => {
            let owner = v.get("owner").and_then(|x| x.as_str()).unwrap_or("?");
            let repo  = v.get("repo").and_then(|x| x.as_str()).unwrap_or("?");
            let r#ref = v.get("ref").and_then(|x| x.as_str());
            match r#ref {
                Some(r) => format!("github:{owner}/{repo}/{r}"),
                None    => format!("github:{owner}/{repo}"),
            }
        }
        "git" => {
            let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("?");
            format!("git:{url}")
        }
        "path" => {
            let path = v.get("path").and_then(|x| x.as_str()).unwrap_or("?");
            format!("path:{path}")
        }
        "tarball" => {
            let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("?");
            format!("tarball:{url}")
        }
        other => format!("{other}:?"),
    }
}

/// Load registry entries from a JSON file on disk.
///
/// Returns an empty Vec when the file is missing (cppnix:
/// missing-registry-is-empty), or a typed error when the file
/// exists but is malformed.
///
/// # Errors
///
/// - `registry-read` for I/O errors other than NotFound.
/// - Returns from `parse_entries` for parse / version errors.
pub fn load_entries_from_disk(path: &std::path::Path)
    -> Result<Vec<RegistryEntry>, SpecError>
{
    match std::fs::read_to_string(path) {
        Ok(text) => parse_entries(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(SpecError::Interp {
            phase: "registry-read".into(),
            message: format!("reading {}: {e}", path.display()),
        }),
    }
}

/// Discover every canonical registry path on this system, returning
/// `(scope, entries)` pairs for every scope whose file exists.  System
/// + user paths come from the canonical Lisp spec; flake-local +
/// global aren't disk-loadable in the same way (flake-local is per-
/// flake; global is fetched from the upstream URL).
///
/// HOME expansion mirrors cppnix: `~/` → `$HOME/`.
///
/// # Errors
///
/// Returns the first per-scope load error encountered.
pub fn discover_disk_registries() -> Result<Registries, SpecError> {
    let formats = load_canonical()?;
    let mut out: Registries = Vec::new();
    for f in &formats {
        // Skip scopes that don't live in one fixed disk file.
        if matches!(f.scope, RegistryScope::FlakeLocal | RegistryScope::Global) {
            continue;
        }
        let path = expand_home(&f.default_path);
        let entries = load_entries_from_disk(&path)?;
        out.push((f.scope, entries));
    }
    Ok(out)
}

fn expand_home(p: &str) -> std::path::PathBuf {
    if let Some(suffix) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home).join(suffix);
        }
    }
    std::path::PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_registry_parses() {
        let formats = load_canonical().unwrap();
        assert!(!formats.is_empty());
    }

    #[test]
    fn all_four_scopes_present() {
        let formats = load_canonical().unwrap();
        let scopes: std::collections::HashSet<RegistryScope> =
            formats.iter().map(|f| f.scope).collect();
        for required in [
            RegistryScope::FlakeLocal,
            RegistryScope::User,
            RegistryScope::System,
            RegistryScope::Global,
        ] {
            assert!(
                scopes.contains(&required),
                "missing registry scope {required:?}",
            );
        }
    }

    #[test]
    fn precedence_order_matches_cppnix() {
        let formats = load_canonical().unwrap();
        let prec = |s: RegistryScope| -> u32 {
            formats.iter().find(|f| f.scope == s).unwrap().precedence
        };
        // cppnix order: flake-local < user < system < global
        assert!(prec(RegistryScope::FlakeLocal) < prec(RegistryScope::User));
        assert!(prec(RegistryScope::User) < prec(RegistryScope::System));
        assert!(prec(RegistryScope::System) < prec(RegistryScope::Global));
    }

    // ── M3.0 resolver tests ────────────────────────────────────

    fn entry(from: &str, to: &str) -> RegistryEntry {
        RegistryEntry { from: from.into(), to: to.into(), exact: false }
    }

    #[test]
    fn resolve_finds_lowest_precedence_match() {
        let registries: Registries = vec![
            (RegistryScope::Global, vec![entry("nixpkgs", "github:NixOS/nixpkgs/global")]),
            (RegistryScope::User,   vec![entry("nixpkgs", "github:NixOS/nixpkgs/user")]),
            (RegistryScope::FlakeLocal, vec![entry("nixpkgs", "github:NixOS/nixpkgs/local")]),
        ];
        let resolved = resolve(&registries, "nixpkgs").unwrap();
        assert_eq!(resolved.to, "github:NixOS/nixpkgs/local");
    }

    #[test]
    fn resolve_falls_through_to_system_when_local_absent() {
        let registries: Registries = vec![
            (RegistryScope::User, vec![entry("home-manager", "github:nix-community/home-manager")]),
            (RegistryScope::System, vec![entry("home-manager", "github:nix-community/system-hm")]),
        ];
        let resolved = resolve(&registries, "home-manager").unwrap();
        assert_eq!(resolved.to, "github:nix-community/home-manager");
    }

    #[test]
    fn resolve_errors_on_unknown_input() {
        let registries: Registries = vec![
            (RegistryScope::Global, vec![entry("known", "github:x/y")]),
        ];
        let err = resolve(&registries, "unknown").unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "registry-unresolved"),
            _ => panic!("expected registry-unresolved"),
        }
    }

    // ── M3.1 disk-loader tests ──────────────────────────────────

    const SAMPLE_REGISTRY_JSON: &str = r#"{
        "version": 2,
        "flakes": [
            {
                "from": {"type": "indirect", "id": "nixpkgs"},
                "to":   {"type": "github", "owner": "NixOS", "repo": "nixpkgs"},
                "exact": false
            },
            {
                "from": {"type": "indirect", "id": "home-manager"},
                "to":   {"type": "github", "owner": "nix-community", "repo": "home-manager", "ref": "master"},
                "exact": true
            },
            {
                "from": {"type": "indirect", "id": "local-flake"},
                "to":   {"type": "path", "path": "/Users/me/code/some-flake"}
            }
        ]
    }"#;

    #[test]
    fn parse_entries_handles_canonical_shape() {
        let entries = parse_entries(SAMPLE_REGISTRY_JSON).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].from, "nixpkgs");
        assert_eq!(entries[0].to, "github:NixOS/nixpkgs");
        assert!(!entries[0].exact);
        assert_eq!(entries[1].to, "github:nix-community/home-manager/master");
        assert!(entries[1].exact);
        assert_eq!(entries[2].to, "path:/Users/me/code/some-flake");
    }

    #[test]
    fn parse_entries_errors_on_wrong_version() {
        let bad = r#"{"version": 1, "flakes": []}"#;
        let err = parse_entries(bad).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "registry-version"),
            _ => panic!("expected registry-version"),
        }
    }

    #[test]
    fn parse_entries_errors_on_garbage() {
        let err = parse_entries("not json at all").unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "registry-parse"),
            _ => panic!("expected registry-parse"),
        }
    }

    #[test]
    fn load_entries_from_missing_file_returns_empty() {
        let path = std::path::Path::new("/nonexistent/path/registry.json");
        let entries = load_entries_from_disk(path).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn load_entries_from_disk_parses_real_file() {
        let tmp = std::env::temp_dir().join("sui-spec-test-registry.json");
        std::fs::write(&tmp, SAMPLE_REGISTRY_JSON).unwrap();
        let entries = load_entries_from_disk(&tmp).unwrap();
        assert_eq!(entries.len(), 3);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn flatten_ref_handles_all_known_types() {
        let v = serde_json::json!({"type": "github", "owner": "x", "repo": "y"});
        assert_eq!(flatten_ref(&v), "github:x/y");

        let v = serde_json::json!({"type": "github", "owner": "x", "repo": "y", "ref": "main"});
        assert_eq!(flatten_ref(&v), "github:x/y/main");

        let v = serde_json::json!({"type": "git", "url": "https://example.com/x.git"});
        assert_eq!(flatten_ref(&v), "git:https://example.com/x.git");

        let v = serde_json::json!({"type": "path", "path": "/x/y"});
        assert_eq!(flatten_ref(&v), "path:/x/y");

        let v = serde_json::json!({"type": "tarball", "url": "https://x/y.tar.gz"});
        assert_eq!(flatten_ref(&v), "tarball:https://x/y.tar.gz");

        let v = serde_json::json!({"type": "indirect", "id": "nixpkgs"});
        assert_eq!(flatten_ref(&v), "nixpkgs");

        let v = serde_json::json!({"type": "unknown-type"});
        assert_eq!(flatten_ref(&v), "unknown-type:?");
    }
}
