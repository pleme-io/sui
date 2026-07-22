//! Path algorithms for the IR engine — **byte-for-byte mirrors** of
//! `sui-eval/src/path.rs` (`canon_abs`, `normalize`, `resolve_relative`,
//! `resolve_import`) plus the NIX_PATH search-path resolver from
//! `sui-eval/src/builtins/nav.rs` (`parse_nix_path`, `resolve_search_path`),
//! all parity-tested against their originals in `tests/path_parity.rs`.
//!
//! # Mirror, not reuse — and why
//!
//! The walker's functions are `pub`, but `sui-eval` is a **dev-dependency
//! only** of this crate (the representational blocker documented in
//! [`crate::eval_ir`]): the library cannot link the live engine. So the
//! algorithms are mirrored here, and the mirror is **parity-tested against
//! the originals** in `tests/path_parity.rs` (which *can* import
//! `sui_eval::path` as a dev-dep) — drift between mirror and original is a
//! red test, not a silent divergence.
//!
//! Deliberate simplification: the walker's `materialize`/`dematerialize`
//! store↔cache seam (fetched flake inputs) has no counterpart here — the
//! pure-subset engine has no fetcher, so the seam is the identity map. The
//! walker's seam is also the identity for any path not under a fetched
//! input, which is every path this engine can encounter.

use std::path::{Component, Path, PathBuf};

/// Canonicalize an ABSOLUTE path string exactly the way CppNix's
/// `canonPath` does. Mirror of `sui_eval::path::canon_abs`:
/// `.` and empty components vanish, `..` pops but **clamps at the root**
/// (`/..` → `/`), redundant separators collapse, no trailing `/` except
/// the root itself. Non-absolute input is returned unchanged.
#[must_use]
pub fn canon_abs(raw: &str) -> String {
    if !raw.starts_with('/') {
        return raw.to_string();
    }
    let mut components: Vec<&str> = Vec::new();
    for seg in raw.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            other => components.push(other),
        }
    }
    if components.is_empty() {
        "/".to_string()
    } else {
        let mut out = String::with_capacity(raw.len());
        for c in &components {
            out.push('/');
            out.push_str(c);
        }
        out
    }
}

/// Normalize a path by removing `.` components and resolving `..`
/// components. Mirror of `sui_eval::path::normalize` — including its
/// root-popping `ParentDir` arm (which `canon_abs` exists to avoid for
/// absolute path *values*; `normalize` is what the walker uses for
/// relative-literal resolution and import canonicalization).
#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    if out.is_empty() {
        PathBuf::from(".")
    } else {
        out.iter().collect()
    }
}

/// Resolve a relative path against a base directory, normalizing the
/// result. Mirror of `sui_eval::path::resolve_relative`.
#[must_use]
pub fn resolve_relative(base: &Path, relative: &str) -> PathBuf {
    normalize(&base.join(relative))
}

/// Resolve an import path. Mirror of `sui_eval::path::resolve_import`
/// (minus the store↔cache `materialize` seam — identity here, see the
/// module docs):
/// - absolute paths are normalized;
/// - relative paths resolve against `base_dir` (error when absent);
/// - a directory target gets `/default.nix` appended.
///
/// # Errors
///
/// Returns an error when the path is relative and no `base_dir` exists.
pub fn resolve_import(base_dir: Option<&Path>, raw: &str) -> Result<PathBuf, String> {
    let resolved = if Path::new(raw).is_absolute() {
        normalize(Path::new(raw))
    } else {
        let base = base_dir.ok_or_else(|| {
            let mut msg = String::from("relative import '");
            msg.push_str(raw);
            msg.push_str("' with no base directory");
            msg
        })?;
        resolve_relative(base, raw)
    };
    if resolved.is_dir() {
        Ok(resolved.join("default.nix"))
    } else {
        Ok(resolved)
    }
}

// ── NIX_PATH search-path resolution (mirror of `sui_eval::builtins::nav`) ──

/// Resolve a `<nix/sub>` path to an embedded corepkg file — mirror of the
/// walker's `resolve_corepkg` (`sui_eval::builtins::nav`), same embedded
/// `fetchurl.nix` content, same `sui-corepkgs` temp dir.
fn resolve_corepkg(sub: &str) -> Option<String> {
    use std::sync::OnceLock;
    static COREPKGS_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

    let dir = COREPKGS_DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join("sui-corepkgs");
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(
            dir.join("fetchurl.nix"),
            include_str!("../../sui-eval/src/corepkgs/fetchurl.nix"),
        )
        .ok()?;
        Some(dir)
    });

    let dir = dir.as_ref()?;
    let path = dir.join(sub);
    if path.exists() {
        Some(path.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// Parse a `NIX_PATH` env-var value into `(prefix, path)` pairs. Mirror of
/// `sui_eval::builtins::parse_nix_path`: `prefix1=path1:prefix2=path2:…`; an
/// entry with no `=` has an empty prefix; empty entries are skipped.
#[must_use]
pub fn parse_nix_path(s: &str) -> Vec<(String, String)> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(':')
        .filter(|e| !e.is_empty())
        .map(|entry| match entry.split_once('=') {
            Some((prefix, path)) => (prefix.to_string(), path.to_string()),
            None => (String::new(), entry.to_string()),
        })
        .collect()
}

/// Resolve a `<name>` / `<name/sub>` search-path token to an absolute
/// filesystem path by walking the `NIX_PATH` entries. Mirror of
/// `sui_eval::builtins::resolve_search_path`, minus the `<nix/…>` embedded
/// corepkgs branch — the pure-subset engine ships no corepkgs, so a
/// `nix/`-prefixed token resolves to `None` (a miss → a typed throw at the
/// eval site) rather than a temp-materialized corepkg file. Parity for the
/// NIX_PATH-driven cases is gated in `tests/path_parity.rs`.
#[must_use]
pub fn resolve_search_path(name: &str) -> Option<String> {
    // Embedded corepkgs (`<nix/fetchurl.nix>`): mirror the walker's
    // `resolve_corepkg` (nav.rs) — the darwin stdenv's `fetchurlBoot` imports
    // `<nix/fetchurl.nix>` for the bootstrap-tools FOD, so real nixpkgs eval
    // needs this. Same embedded content + same temp dir as the walker.
    if let Some(sub) = name.strip_prefix("nix/") {
        return resolve_corepkg(sub);
    }

    let nix_path = std::env::var("NIX_PATH").unwrap_or_default();
    if nix_path.is_empty() {
        return None;
    }
    for (prefix, path) in parse_nix_path(&nix_path) {
        if !prefix.is_empty() && name == prefix {
            if Path::new(&path).exists() {
                return Some(path);
            }
            continue;
        }
        if !prefix.is_empty() {
            let needle = format!("{prefix}/");
            if let Some(rest) = name.strip_prefix(&needle) {
                let full = format!("{path}/{rest}");
                if Path::new(&full).exists() {
                    return Some(full);
                }
                continue;
            }
        }
        if prefix.is_empty() {
            let full = format!("{path}/{name}");
            if Path::new(&full).exists() {
                return Some(full);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canon_abs_mirror_cases() {
        assert_eq!(canon_abs("/."), "/");
        assert_eq!(canon_abs("/"), "/");
        assert_eq!(canon_abs("/foo/./bar"), "/foo/bar");
        assert_eq!(canon_abs("/foo/../bar"), "/bar");
        assert_eq!(canon_abs("/.."), "/");
        assert_eq!(canon_abs("/../.."), "/");
        assert_eq!(canon_abs("/a/../.."), "/");
        assert_eq!(canon_abs("/foo//bar"), "/foo/bar");
        assert_eq!(canon_abs("/nix/store/"), "/nix/store");
        assert_eq!(canon_abs("relative/kept"), "relative/kept");
    }

    #[test]
    fn normalize_mirror_cases() {
        assert_eq!(normalize(Path::new("/a/./b")), PathBuf::from("/a/b"));
        assert_eq!(normalize(Path::new("/a/x/../b")), PathBuf::from("/a/b"));
        assert_eq!(normalize(Path::new("a/./b")), PathBuf::from("a/b"));
    }

    #[test]
    fn resolve_relative_mirror_cases() {
        assert_eq!(
            resolve_relative(Path::new("/base"), "sub/file.nix"),
            PathBuf::from("/base/sub/file.nix")
        );
        assert_eq!(
            resolve_relative(Path::new("/base/sub"), "../file.nix"),
            PathBuf::from("/base/file.nix")
        );
    }

    #[test]
    fn resolve_import_relative_without_base_errors() {
        assert!(resolve_import(None, "lib.nix").is_err());
    }
}
