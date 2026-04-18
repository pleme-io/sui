//! Flake registry resolution: `flake:nixpkgs` → `github:NixOS/nixpkgs`.
//!
//! CppNix ships a 37-entry global registry of common flake ids
//! (`nixpkgs`, `home-manager`, `flake-utils`, …) that map indirect
//! refs to concrete targets. That registry normally lives in
//! `~/.cache/nix/flake-registry-v6.json` after being fetched once
//! from `https://channels.nixos.org/flake-registry.json`, with
//! user-level overrides in `~/.config/nix/registry.json` and
//! system-level in `/etc/nix/registry.json`.
//!
//! For sui we vendor the global registry as static data so that
//! `getFlake "nixpkgs"` works out of the box with no network call
//! and no per-user setup. Filesystem registries are merged on top
//! so users can still pin or override as they do with CppNix.
//!
//! # Semantics (matches CppNix `flake/flakeref-registry`)
//!
//! Registry lookup order (highest priority first):
//!   1. Flake-local overrides (not handled here; applied by callers)
//!   2. User registry (`$XDG_CONFIG_HOME/nix/registry.json`)
//!   3. System registry (`/etc/nix/registry.json`)
//!   4. Global registry (this module's `DEFAULT_REGISTRY`)
//!
//! A single lookup iterates the layers in order, returning the
//! first match. Each layer can **chain** (its `to` field may itself
//! be indirect) — we follow up to `MAX_CHAIN_DEPTH` hops to guard
//! against accidental cycles.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use crate::value::{EvalError, NixAttrs, Value};

use super::flake_parse::parse_flake_ref;

/// Max number of indirect → indirect hops before we declare a cycle.
/// Flake registries in the wild rarely chain at all; 8 is paranoid.
const MAX_CHAIN_DEPTH: usize = 8;

/// The vendored global registry. Snapshotted from
/// `nix registry list` on Determinate Nix 3.17.0 (2.33.3) —
/// 37 entries covering nixpkgs, home-manager, nix-darwin, flake-
/// utils, flake-parts, systems, and the rest of the common pool.
///
/// Each entry is `(indirect_id, concrete_flake_ref_string)`.  The
/// string is parsed through [`parse_flake_ref`] lazily at lookup
/// time, so no allocation happens for unused entries.
pub const DEFAULT_REGISTRY: &[(&str, &str)] = &[
    ("agda", "github:agda/agda"),
    ("arion", "github:hercules-ci/arion"),
    ("blender-bin", "github:edolstra/nix-warez?dir=blender"),
    ("bundlers", "github:NixOS/bundlers"),
    ("cachix", "github:cachix/cachix"),
    ("composable", "github:ComposableFi/composable"),
    ("disko", "github:nix-community/disko"),
    ("dreampkgs", "github:nix-community/dreampkgs"),
    ("dwarffs", "github:edolstra/dwarffs"),
    ("emacs-overlay", "github:nix-community/emacs-overlay"),
    ("fenix", "github:nix-community/fenix"),
    ("flake-parts", "github:hercules-ci/flake-parts"),
    ("flake-utils", "github:numtide/flake-utils"),
    ("helix", "github:helix-editor/helix"),
    ("hercules-ci-agent", "github:hercules-ci/hercules-ci-agent"),
    ("hercules-ci-effects", "github:hercules-ci/hercules-ci-effects"),
    ("home-manager", "github:nix-community/home-manager"),
    ("hydra", "github:NixOS/hydra"),
    ("mach-nix", "github:DavHau/mach-nix"),
    ("ngipkgs", "github:ngi-nix/ngipkgs"),
    ("nickel", "github:tweag/nickel"),
    ("nix", "github:NixOS/nix"),
    ("nix-darwin", "github:nix-darwin/nix-darwin"),
    ("nix-serve", "github:edolstra/nix-serve"),
    ("nixops", "github:NixOS/nixops"),
    ("nixos-anywhere", "github:nix-community/nixos-anywhere"),
    ("nixos-hardware", "github:NixOS/nixos-hardware"),
    ("nixos-homepage", "github:NixOS/nixos-homepage"),
    ("nixos-search", "github:NixOS/nixos-search"),
    ("nixpkgs", "github:NixOS/nixpkgs/nixpkgs-unstable"),
    ("nur", "github:nix-community/NUR"),
    ("patchelf", "github:NixOS/patchelf"),
    ("poetry2nix", "github:nix-community/poetry2nix"),
    ("pridefetch", "github:SpyHoodle/pridefetch"),
    ("sops-nix", "github:Mic92/sops-nix"),
    ("systems", "github:nix-systems/default"),
    ("templates", "github:NixOS/templates"),
];

/// An in-memory registry — either the static default or a parsed
/// user/system registry.json. Kept simple: just `id → ref-string`
/// pairs.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    pub entries: Vec<(String, String)>,
}

impl Registry {
    /// Look up a single `id` in this layer. Returns the raw ref
    /// string the caller should feed into [`parse_flake_ref`] (or
    /// recurse on if it parses as indirect again).
    #[must_use]
    pub fn lookup(&self, id: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| v.as_str())
    }

    /// Construct a view over the static `DEFAULT_REGISTRY`. Cheap
    /// (one clone of 37 string pairs) and avoids forcing a separate
    /// static-mutex lifetime.
    #[must_use]
    pub fn global() -> Self {
        Self {
            entries: DEFAULT_REGISTRY
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }
}

/// Parse a `registry.json` body (CppNix v2 schema):
///
/// ```json
/// {
///   "version": 2,
///   "flakes": [
///     { "from": {"type": "indirect", "id": "nixpkgs"},
///       "to":   {"type": "github",   "owner": "NixOS", "repo": "nixpkgs"} }
///   ]
/// }
/// ```
///
/// We accept both `to` (attrset of fields) and `exact`/`from`
/// variants; unknown shapes are silently dropped rather than
/// failing the whole load.
#[must_use]
pub fn parse_registry_json(body: &str) -> Registry {
    let Ok(v): Result<serde_json::Value, _> = serde_json::from_str(body) else {
        return Registry::default();
    };
    let Some(flakes) = v.get("flakes").and_then(serde_json::Value::as_array) else {
        return Registry::default();
    };
    let mut entries = Vec::new();
    for entry in flakes {
        let Some(from) = entry.get("from") else { continue };
        let Some(to) = entry.get("to") else { continue };
        if from.get("type").and_then(serde_json::Value::as_str) != Some("indirect") {
            continue;
        }
        let Some(id) = from.get("id").and_then(serde_json::Value::as_str) else { continue };
        let to_str = flake_ref_json_to_string(to);
        if to_str.is_empty() {
            continue;
        }
        entries.push((id.to_string(), to_str));
    }
    Registry { entries }
}

/// Render a registry-entry `to` attrset back to its canonical
/// flake-ref string. Mirrors `flakeRefToString` but operates on
/// `serde_json::Value` instead of `NixAttrs` so we don't require a
/// live Interner during parse.
fn flake_ref_json_to_string(v: &serde_json::Value) -> String {
    let ty = v.get("type").and_then(serde_json::Value::as_str).unwrap_or("");
    match ty {
        "github" | "gitlab" | "sourcehut" => {
            let owner = v.get("owner").and_then(serde_json::Value::as_str).unwrap_or("");
            let repo = v.get("repo").and_then(serde_json::Value::as_str).unwrap_or("");
            let mut out = format!("{ty}:{owner}/{repo}");
            if let Some(rev) = v.get("rev").and_then(serde_json::Value::as_str) {
                out.push('/');
                out.push_str(rev);
            } else if let Some(reff) = v.get("ref").and_then(serde_json::Value::as_str) {
                out.push('/');
                out.push_str(reff);
            }
            if let Some(dir) = v.get("dir").and_then(serde_json::Value::as_str) {
                out.push_str("?dir=");
                out.push_str(dir);
            }
            out
        }
        "tarball" => {
            let url = v.get("url").and_then(serde_json::Value::as_str).unwrap_or("");
            format!("tarball+{url}")
        }
        "git" => {
            let url = v.get("url").and_then(serde_json::Value::as_str).unwrap_or("");
            format!("git+{url}")
        }
        "path" => {
            let path = v.get("path").and_then(serde_json::Value::as_str).unwrap_or("");
            format!("path:{path}")
        }
        "indirect" => {
            let id = v.get("id").and_then(serde_json::Value::as_str).unwrap_or("");
            format!("flake:{id}")
        }
        _ => String::new(),
    }
}

/// Load the user-level registry (`$XDG_CONFIG_HOME/nix/registry.json`
/// or `~/.config/nix/registry.json`). Returns an empty registry if
/// the file doesn't exist or isn't readable — this is the common
/// case on fresh machines.
#[must_use]
pub fn load_user_registry() -> Registry {
    let path = user_registry_path();
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .map(|body| parse_registry_json(&body))
        .unwrap_or_default()
}

/// Load the system-level registry (`/etc/nix/registry.json`).
#[must_use]
pub fn load_system_registry() -> Registry {
    std::fs::read_to_string("/etc/nix/registry.json")
        .ok()
        .map(|body| parse_registry_json(&body))
        .unwrap_or_default()
}

fn user_registry_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("nix/registry.json"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/nix/registry.json"))
}

thread_local! {
    /// Layered registries, cached per thread on first access so we
    /// don't re-read `registry.json` 1000× for a flake-heavy eval.
    static REGISTRY_CACHE: RefCell<Option<Vec<Registry>>> = const { RefCell::new(None) };
}

/// Return the user + system + global registries, in priority order
/// (user wins, then system, then global). Cached per thread.
fn layered_registries() -> Vec<Registry> {
    REGISTRY_CACHE.with(|c| {
        if let Some(ref layers) = *c.borrow() {
            return layers.clone();
        }
        let layers = vec![load_user_registry(), load_system_registry(), Registry::global()];
        *c.borrow_mut() = Some(layers.clone());
        layers
    })
}

/// Force a fresh registry read on the next lookup. Tests use this
/// to isolate from the shared thread-local state; a future
/// runtime-reload path (e.g. "user edited registry.json, re-read
/// it without restarting sui") is the production use case.
#[allow(dead_code)]
pub fn invalidate_cache() {
    REGISTRY_CACHE.with(|c| *c.borrow_mut() = None);
}

/// Resolve an indirect flake ref (`{ type = "indirect"; id = …; }`)
/// into the concrete ref attrset it points at. Returns an error
/// when the id isn't in any registry layer, or when chaining
/// exceeds [`MAX_CHAIN_DEPTH`].
///
/// Preserves the caller's `ref` / `rev` overrides: if you pass
/// `{ type="indirect"; id="nixpkgs"; ref="nixos-25.05"; }` the
/// resolved github entry inherits that `ref` (the resolver value
/// wins over any `ref` in the registry `to` field).
pub fn resolve_indirect(indirect: &NixAttrs) -> Result<Value, EvalError> {
    let id = indirect
        .get("id")
        .ok_or_else(|| EvalError::AttrNotFound("id".into()))?
        .to_str()?;
    // Caller overrides (captured once up front so they survive the
    // chain loop below — they apply to the FINAL resolved ref, not
    // intermediate indirects).
    let caller_ref = indirect.get("ref").and_then(|v| v.to_str().ok());
    let caller_rev = indirect.get("rev").and_then(|v| v.to_str().ok());
    let caller_dir = indirect.get("dir").and_then(|v| v.to_str().ok());

    let layers = layered_registries();
    let mut current_id = id.clone();
    for _ in 0..MAX_CHAIN_DEPTH {
        let Some(ref_str) = layers.iter().find_map(|r| r.lookup(&current_id)) else {
            return Err(EvalError::TypeError(format!(
                "resolveFlakeRef: id '{current_id}' not found in any registry"
            )));
        };
        let parsed = parse_flake_ref(ref_str)?;
        let Value::Attrs(ref parsed_attrs) = parsed else {
            return Err(EvalError::TypeError(format!(
                "resolveFlakeRef: registry entry for '{current_id}' did not parse to an attrset",
            )));
        };
        // If the target is itself indirect, follow the chain.
        if parsed_attrs
            .get("type")
            .and_then(|v| v.to_str().ok())
            .as_deref()
            == Some("indirect")
        {
            current_id = parsed_attrs
                .get("id")
                .ok_or_else(|| EvalError::AttrNotFound("id".into()))?
                .to_str()?;
            continue;
        }
        // Concrete — merge in caller-level overrides.
        let mut out = (**parsed_attrs).clone();
        if let Some(r) = caller_ref {
            out.insert("ref".into(), Value::string(r));
        }
        if let Some(r) = caller_rev {
            out.insert("rev".into(), Value::string(r));
        }
        if let Some(d) = caller_dir {
            out.insert("dir".into(), Value::string(d));
        }
        return Ok(Value::Attrs(Rc::new(out)));
    }
    Err(EvalError::TypeError(format!(
        "resolveFlakeRef: chain depth exceeded for id '{id}' (probable cycle)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_contains_nixpkgs() {
        let reg = Registry::global();
        assert_eq!(
            reg.lookup("nixpkgs"),
            Some("github:NixOS/nixpkgs/nixpkgs-unstable")
        );
        assert_eq!(
            reg.lookup("home-manager"),
            Some("github:nix-community/home-manager")
        );
    }

    #[test]
    fn default_registry_has_all_37() {
        assert_eq!(DEFAULT_REGISTRY.len(), 37, "vendored registry drifted from 37 entries");
    }

    #[test]
    fn parse_registry_json_roundtrip() {
        let body = r#"{
            "version": 2,
            "flakes": [
                {
                    "from": {"type": "indirect", "id": "mystuff"},
                    "to":   {"type": "github",   "owner": "me", "repo": "stuff"}
                }
            ]
        }"#;
        let reg = parse_registry_json(body);
        assert_eq!(reg.lookup("mystuff"), Some("github:me/stuff"));
    }

    #[test]
    fn parse_registry_json_tolerates_garbage() {
        assert!(parse_registry_json("not json").entries.is_empty());
        assert!(parse_registry_json(r#"{"version": 2}"#).entries.is_empty());
        assert!(parse_registry_json(r#"{"flakes": []}"#).entries.is_empty());
    }

    #[test]
    fn resolve_indirect_nixpkgs() {
        invalidate_cache();
        let mut attrs = NixAttrs::new();
        attrs.insert("type".into(), Value::string("indirect"));
        attrs.insert("id".into(), Value::string("nixpkgs"));
        let resolved = resolve_indirect(&attrs).expect("resolves");
        let Value::Attrs(r) = resolved else { panic!("not attrs") };
        assert_eq!(r.get("type").unwrap().to_str().unwrap(), "github");
        assert_eq!(r.get("owner").unwrap().to_str().unwrap(), "NixOS");
        assert_eq!(r.get("repo").unwrap().to_str().unwrap(), "nixpkgs");
    }

    #[test]
    fn resolve_indirect_preserves_caller_ref() {
        invalidate_cache();
        let mut attrs = NixAttrs::new();
        attrs.insert("type".into(), Value::string("indirect"));
        attrs.insert("id".into(), Value::string("nixpkgs"));
        attrs.insert("ref".into(), Value::string("nixos-25.05"));
        let resolved = resolve_indirect(&attrs).unwrap();
        let Value::Attrs(r) = resolved else { panic!("not attrs") };
        // Caller's ref should win over the registry's nixpkgs-unstable
        assert_eq!(r.get("ref").unwrap().to_str().unwrap(), "nixos-25.05");
    }

    #[test]
    fn resolve_unknown_id_errors() {
        invalidate_cache();
        let mut attrs = NixAttrs::new();
        attrs.insert("type".into(), Value::string("indirect"));
        attrs.insert("id".into(), Value::string("this-id-does-not-exist-anywhere"));
        assert!(resolve_indirect(&attrs).is_err());
    }
}
