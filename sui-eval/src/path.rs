//! Centralized path resolution for the Nix evaluator.
//!
//! All path operations (normalization, relative resolution, import resolution)
//! go through this module to ensure consistent behavior.

use std::path::{Component, Path, PathBuf};

/// Normalize a path by removing `.` components and resolving `..` components.
/// Unlike `canonicalize()`, this doesn't require the path to exist on disk.
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

/// Resolve a relative path against a base directory, normalizing the result.
#[must_use]
pub fn resolve_relative(base: &Path, relative: &str) -> PathBuf {
    normalize(&base.join(relative))
}

/// Resolve an import path.
/// - Absolute paths are returned as-is (normalized).
/// - Relative paths are resolved against `base_dir`.
/// - If the result is a directory, append `/default.nix`.
///
/// # Errors
///
/// Returns an error if the path is relative but no `base_dir` is provided.
pub fn resolve_import(base_dir: Option<&Path>, raw: &str) -> Result<PathBuf, String> {
    let resolved = if Path::new(raw).is_absolute() {
        normalize(Path::new(raw))
    } else {
        let base = base_dir.ok_or_else(|| {
            format!("relative import '{raw}' with no base directory")
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
    fn normalize_removes_dot() {
        assert_eq!(normalize(Path::new("/a/./b")), PathBuf::from("/a/b"));
    }

    #[test]
    fn normalize_resolves_dotdot() {
        assert_eq!(normalize(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
    }

    #[test]
    fn normalize_multiple_dots() {
        assert_eq!(
            normalize(Path::new("/a/./b/./c")),
            PathBuf::from("/a/b/c")
        );
    }

    #[test]
    fn normalize_preserves_absolute() {
        assert_eq!(normalize(Path::new("/a/b/c")), PathBuf::from("/a/b/c"));
    }

    #[test]
    fn normalize_empty_result_becomes_dot() {
        assert_eq!(normalize(Path::new(".")), PathBuf::from("."));
    }

    #[test]
    fn resolve_relative_basic() {
        assert_eq!(
            resolve_relative(Path::new("/base"), "sub/file.nix"),
            PathBuf::from("/base/sub/file.nix")
        );
    }

    #[test]
    fn resolve_relative_with_dotdot() {
        assert_eq!(
            resolve_relative(Path::new("/base/sub"), "../file.nix"),
            PathBuf::from("/base/file.nix")
        );
    }

    #[test]
    fn resolve_import_absolute() {
        let r = resolve_import(None, "/absolute/path.nix").unwrap();
        assert_eq!(r, PathBuf::from("/absolute/path.nix"));
    }

    #[test]
    fn resolve_import_relative_needs_base() {
        assert!(resolve_import(None, "./relative.nix").is_err());
    }

    #[test]
    fn resolve_import_absolute_with_dotdot() {
        let r = resolve_import(None, "/a/b/../c.nix").unwrap();
        assert_eq!(r, PathBuf::from("/a/c.nix"));
    }

    #[test]
    fn resolve_import_directory_appends_default_nix() {
        // Use a known directory that exists on all systems.
        let r = resolve_import(None, "/tmp").unwrap();
        assert_eq!(r, PathBuf::from("/tmp/default.nix"));
    }

    #[test]
    fn resolve_import_relative_with_base() {
        // The relative path won't be a directory on disk, so no default.nix append.
        let r = resolve_import(Some(Path::new("/base")), "sub/file.nix").unwrap();
        assert_eq!(r, PathBuf::from("/base/sub/file.nix"));
    }
}
