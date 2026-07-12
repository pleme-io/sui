//! Centralized path resolution for the Nix evaluator.
//!
//! All path operations (normalization, relative resolution, import resolution)
//! go through this module to ensure consistent behavior.

use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};

thread_local! {
    /// Registry of fetched flake-input source trees: each entry maps the
    /// cppnix `/nix/store/<narhash>-source` STORE-PATH STRING (what an
    /// input's `outPath`/`sourceInfo.outPath` exposes for byte-parity) to
    /// the ACTUAL on-disk directory the tree lives at (the sui fetcher
    /// cache `~/.cache/sui/inputs/…`, or the literal path for a
    /// `type = "path"` input).
    ///
    /// sui does NOT copy fetched trees into `/nix/store` at that path, so a
    /// flake's own Nix code that reads `${input.outPath}/<subpath>` (an
    /// `import`, `readFile`, `pathExists`, `readDir`, …) would resolve
    /// against a directory that does not exist. `materialize` redirects the
    /// FILESYSTEM READ to the real tree while the store-path STRING flowing
    /// through eval stays byte-correct — no hash, no derivation, no value
    /// ever changes.
    ///
    /// The marquee darwin root (2026-07-11) set the store-path string; this
    /// registry closes its sibling: arbitrary `${outPath}/subpath` reads
    /// (the prior peel special-cased only reading `flake.nix`).
    static INPUT_SOURCE_MAP: RefCell<Vec<(PathBuf, PathBuf)>> = const { RefCell::new(Vec::new()) };
}

/// Register a fetched flake-input source tree so subsequent filesystem
/// reads under its `/nix/store/<narhash>-source` store path resolve to the
/// real on-disk `read_dir`. Idempotent per `store_path`. Does not affect
/// any string value — only where reads land on disk.
pub fn register_input_source(store_path: &Path, read_dir: &Path) {
    // Only store paths need remapping — a `type = "path"` input whose
    // outPath already equals its real on-disk dir is a no-op (and would
    // shadow nothing), so skip it.
    if store_path == read_dir {
        return;
    }
    INPUT_SOURCE_MAP.with(|m| {
        let mut m = m.borrow_mut();
        if m.iter().any(|(sp, _)| sp == store_path) {
            return;
        }
        m.push((store_path.to_path_buf(), read_dir.to_path_buf()));
    });
}

/// If `path` lies under a registered flake-input store-path prefix, rewrite
/// that prefix to the input's real on-disk `read_dir`; otherwise return
/// `path` unchanged. This is a FILESYSTEM-READ-ONLY redirect — callers use
/// the result to touch disk, never to build a value the evaluator observes.
#[must_use]
pub fn materialize(path: &Path) -> PathBuf {
    INPUT_SOURCE_MAP.with(|m| {
        for (store_path, read_dir) in m.borrow().iter() {
            if path == store_path {
                return read_dir.clone();
            }
            if let Ok(suffix) = path.strip_prefix(store_path) {
                return read_dir.join(suffix);
            }
        }
        path.to_path_buf()
    })
}

/// Convenience: `materialize` a `&str` path, returning an owned `String`.
#[must_use]
pub fn materialize_str(path: &str) -> String {
    materialize(Path::new(path)).to_string_lossy().into_owned()
}

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

/// Canonicalize an ABSOLUTE path string exactly the way CppNix's
/// `canonPath` does — the byte-for-byte semantics of a Nix path *value*.
///
/// CppNix canonicalizes every path literal on evaluation: `.` components
/// vanish, `..` pops the preceding component **but is clamped at the
/// filesystem root** (`/..` → `/`, never below), and redundant separators
/// collapse. The result always begins with a single `/` and never carries
/// a trailing `/` (except the root itself, which is `/`).
///
/// This differs from [`normalize`] in the one load-bearing way the marquee
/// cid root exposed: `normalize` uses `Path::components()`, whose
/// `ParentDir` arm unconditionally `pop()`s — so `/..` collapses to `.`
/// (root is popped, out empties) instead of clamping to `/`. A path VALUE
/// must never escape its root, so absolute paths take this dedicated
/// root-aware canonicalizer.
///
/// Only ABSOLUTE inputs (leading `/`) are canonicalized here; a
/// non-absolute input is returned unchanged so callers can keep their own
/// resolution semantics (relative-to-eval-dir, `~`-home, `<search>`).
///
/// Examples (all verified against CppNix):
/// - `/.` → `/`            (the `lib.path.hasStorePathPrefix` root case)
/// - `/foo/./bar` → `/foo/bar`
/// - `/foo/../bar` → `/bar`
/// - `/..` → `/`           (root clamp)
/// - `/a/../..` → `/`      (root clamp after underflow)
/// - `/nix/store` → `/nix/store`  (identity)
#[must_use]
pub fn canon_abs(raw: &str) -> String {
    if !raw.starts_with('/') {
        return raw.to_string();
    }
    let mut components: Vec<&str> = Vec::new();
    for seg in raw.split('/') {
        match seg {
            // Empty (leading `/`, doubled `//`, trailing `/`) and `.` vanish.
            "" | "." => {}
            ".." => {
                // Clamp at root: popping an empty stack is a no-op, so an
                // absolute path can never escape below `/`.
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

    // The directory-vs-file probe must consult the REAL tree (a fetched
    // input's `/nix/store/<narhash>-source` prefix isn't materialized on
    // disk), but the RETURNED path keeps the store-path prefix so relative
    // imports inside the target re-enter this remap and eval-dir/string
    // tracking stays byte-correct. Only the on-disk read (at the call site)
    // is redirected via `materialize`.
    if materialize(&resolved).is_dir() {
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

    // ── canon_abs: CppNix path-value canonicalization ──────────────
    //
    // Every case verified against `nix eval --raw --expr 'toString <p>'`.
    // The marquee case is `/.` → `/` — the failing clause of
    // `lib.path.hasStorePathPrefix`'s root assertion in the cid closure.

    #[test]
    fn canon_abs_root_dot() {
        // THE marquee root: `/.` must canonicalize to `/`.
        assert_eq!(canon_abs("/."), "/");
    }

    #[test]
    fn canon_abs_root_identity() {
        assert_eq!(canon_abs("/"), "/");
    }

    #[test]
    fn canon_abs_removes_dot() {
        assert_eq!(canon_abs("/foo/./bar"), "/foo/bar");
    }

    #[test]
    fn canon_abs_resolves_dotdot() {
        assert_eq!(canon_abs("/foo/../bar"), "/bar");
    }

    #[test]
    fn canon_abs_dotdot_clamps_at_root() {
        // CppNix never escapes root: `/..` → `/`, `/a/../..` → `/`.
        assert_eq!(canon_abs("/.."), "/");
        assert_eq!(canon_abs("/../.."), "/");
        assert_eq!(canon_abs("/a/../.."), "/");
    }

    #[test]
    fn canon_abs_collapses_redundant_slashes() {
        assert_eq!(canon_abs("/foo//bar"), "/foo/bar");
        assert_eq!(canon_abs("/nix/store/"), "/nix/store");
    }

    #[test]
    fn canon_abs_store_path_identity() {
        assert_eq!(canon_abs("/nix/store"), "/nix/store");
        assert_eq!(
            canon_abs("/nix/store/nvl9ic0pj1fpyln3zaqrf4cclbqdfn1j-foo"),
            "/nix/store/nvl9ic0pj1fpyln3zaqrf4cclbqdfn1j-foo"
        );
    }

    #[test]
    fn canon_abs_leaves_relative_untouched() {
        // Non-absolute inputs are the caller's concern (eval-dir / ~ / <search>).
        assert_eq!(canon_abs("~/foo"), "~/foo");
        assert_eq!(canon_abs("./foo"), "./foo");
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

    // ── flake-input source materialization (marquee darwin root #3) ──
    //
    // A fetched flake input's `outPath` is a `/nix/store/<narhash>-source`
    // STORE-PATH STRING that sui never copies to disk; the real tree lives
    // in the fetcher cache. `register_input_source` + `materialize` redirect
    // the FILESYSTEM READ of `${outPath}/subpath` to the cache while the
    // store-path string is never mutated — the byte-parity invariant.

    #[test]
    fn materialize_remaps_registered_store_prefix() {
        register_input_source(
            Path::new("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-source"),
            Path::new("/home/u/.cache/sui/inputs/dead"),
        );
        // A read of `${outPath}/lib/foo.nix` lands in the real cache tree.
        assert_eq!(
            materialize(Path::new(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-source/lib/foo.nix"
            )),
            PathBuf::from("/home/u/.cache/sui/inputs/dead/lib/foo.nix"),
        );
        // The bare store path itself remaps to the tree root.
        assert_eq!(
            materialize(Path::new(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-source"
            )),
            PathBuf::from("/home/u/.cache/sui/inputs/dead"),
        );
    }

    #[test]
    fn materialize_passes_unregistered_paths_through() {
        // An unrelated store path (or any other path) is untouched — the
        // remap only fires for a registered input source prefix.
        let p = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-hello-2.12/bin/hello";
        assert_eq!(materialize(Path::new(p)), PathBuf::from(p));
        assert_eq!(materialize(Path::new("/etc/nix/nix.conf")), PathBuf::from("/etc/nix/nix.conf"));
    }

    #[test]
    fn register_input_source_ignores_identity_mapping() {
        // A `type = "path"` input whose outPath already equals its real dir
        // registers nothing (and cannot shadow the tree with itself).
        let dir = Path::new("/some/local/path-input");
        register_input_source(dir, dir);
        assert_eq!(materialize(dir), PathBuf::from(dir));
    }

    #[test]
    fn register_input_source_prefix_boundary_is_exact() {
        // A sibling store path that merely SHARES a textual prefix with a
        // registered one must NOT be remapped — `strip_prefix` is
        // path-component-aware, so `…-source-extra` is not under `…-source`.
        register_input_source(
            Path::new("/nix/store/cccccccccccccccccccccccccccccccc-source"),
            Path::new("/cache/c"),
        );
        let sibling = "/nix/store/cccccccccccccccccccccccccccccccc-source-extra/x";
        assert_eq!(materialize(Path::new(sibling)), PathBuf::from(sibling));
    }
}
