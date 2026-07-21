//! Path algorithms for the IR engine — **byte-for-byte mirrors** of
//! `sui-eval/src/path.rs` (`canon_abs`, `normalize`, `resolve_relative`,
//! `resolve_import`).
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
