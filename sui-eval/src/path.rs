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
    /// Tuple is `(store_path, read_dir, read_dir_canon)` — the third field is
    /// `read_dir.canonicalize()` computed ONCE at registration (falling back to
    /// `read_dir` on failure, the same semantics the per-call fallback had).
    ///
    /// WHY REGISTRATION-TIME (perf root, found live 2026-07-21): `dematerialize`
    /// used to call `read_dir.canonicalize()` INSIDE its per-entry loop, on
    /// every call — and its callers are the path-literal arms of `eval_expr`,
    /// i.e. every `./x` in every .nix file. On the cid marquee eval (~60
    /// registered inputs), sampling the live process showed **61% of the eval
    /// thread's wall-clock inside `__getattrlist`**, the syscall macOS
    /// `realpath` issues per path component. One canonicalize per registration
    /// replaces N-per-dematerialize-call; the registered trees are immutable
    /// fetcher caches, so the value cannot go stale.
    static INPUT_SOURCE_MAP: RefCell<Vec<(PathBuf, PathBuf, PathBuf)>> = const { RefCell::new(Vec::new()) };

    /// Registry of `builtins.fetch*` result trees: each entry maps the REAL
    /// on-disk fetcher-cache directory (e.g. `$TMPDIR/sui-fetchGit/<cache-hash>`)
    /// to the STORE-PATH NAME CppNix gives the tree when it is copied into the
    /// store as a derivation `src` — `"source"` for `fetchGit`/`fetchTree`/
    /// `fetchTarball` (or the explicit `name` arg where one is honored).
    ///
    /// sui's fetchers return a raw temp `Value::Path` whose basename is a
    /// content-addressing cache-hash (a 64-char sha256 hex), NOT a store path.
    /// When that path is later coerced-to-store as a derivation `src`
    /// (`zshSynHlSrc = builtins.fetchGit {…}`), the copy-to-store code named
    /// the store path after the temp basename (`<store-hash>-<cache-hash>`)
    /// instead of CppNix's `<store-hash>-source`. This registry is the
    /// fetcher-family sibling of `INPUT_SOURCE_MAP`: it records the correct
    /// `-source` NAME for each fetcher result so `source_name_for_read_dir`
    /// resolves the copy-to-store name to CppNix's convention. Only the NAME
    /// changes — the bytes (→ NAR hash) are identical either way.
    /// Tuple is `(read_dir, name, read_dir_canon)` — canon computed once at
    /// registration, same rationale as `INPUT_SOURCE_MAP` above.
    static FETCHED_SOURCE_NAMES: RefCell<Vec<(PathBuf, String, PathBuf)>> = const { RefCell::new(Vec::new()) };

    /// Success-only memo for probe-side `canonicalize` calls.
    ///
    /// `dematerialize`/`source_name_for_read_dir` canonicalize their argument
    /// on every call, and eval hands them the same paths over and over (every
    /// path literal in a file, every position report). A SUCCESSFUL
    /// canonicalization of a source path is stable for the life of an eval —
    /// the same frozen-source assumption CppNix itself makes when it copies a
    /// tree to the store. FAILURES are deliberately NOT memoized: a path that
    /// does not exist yet (an IFD output, a store path about to be realized)
    /// may exist later, and caching the failure would wrongly pin it.
    static CANON_MEMO: RefCell<std::collections::HashMap<PathBuf, PathBuf>> =
        RefCell::new(std::collections::HashMap::new());
}

/// `path.canonicalize()` through the success-only thread-local memo.
fn canonicalize_memo(path: &Path) -> Option<PathBuf> {
    if let Some(hit) = CANON_MEMO.with(|m| m.borrow().get(path).cloned()) {
        return Some(hit);
    }
    match path.canonicalize() {
        Ok(canon) => {
            CANON_MEMO.with(|m| {
                m.borrow_mut().insert(path.to_path_buf(), canon.clone());
            });
            Some(canon)
        }
        Err(_) => None,
    }
}

/// Register a `builtins.fetch*` result directory with the store-path NAME
/// CppNix would give its copied-to-store tree (`"source"` for
/// `fetchGit`/`fetchTree`/`fetchTarball`). Idempotent per `read_dir`. Does
/// not affect any string value — only the name a later copy-to-store
/// coercion of this exact path assigns to its store path.
pub fn register_fetched_source(read_dir: &Path, name: &str) {
    FETCHED_SOURCE_NAMES.with(|m| {
        let mut m = m.borrow_mut();
        if m.iter().any(|(rd, _, _)| rd == read_dir) {
            return;
        }
        // Canon once here instead of per lookup — the tree is an immutable
        // fetcher cache, so this cannot go stale. Fallback mirrors the old
        // per-call `unwrap_or_else`.
        let canon = read_dir
            .canonicalize()
            .unwrap_or_else(|_| read_dir.to_path_buf());
        m.push((read_dir.to_path_buf(), name.to_string(), canon));
    });
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
        if m.iter().any(|(sp, _, _)| sp == store_path) {
            return;
        }
        // Canon once at registration — see the INPUT_SOURCE_MAP doc for the
        // measured 61%-of-eval-in-getattrlist root this replaces.
        let canon = read_dir
            .canonicalize()
            .unwrap_or_else(|_| read_dir.to_path_buf());
        m.push((store_path.to_path_buf(), read_dir.to_path_buf(), canon));
    });
}

/// If `path` lies under a registered flake-input store-path prefix, rewrite
/// that prefix to the input's real on-disk `read_dir`; otherwise return
/// `path` unchanged. This is a FILESYSTEM-READ-ONLY redirect — callers use
/// the result to touch disk, never to build a value the evaluator observes.
#[must_use]
pub fn materialize(path: &Path) -> PathBuf {
    INPUT_SOURCE_MAP.with(|m| {
        for (store_path, read_dir, _) in m.borrow().iter() {
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

/// REVERSE of [`materialize`] for SOURCE POSITIONS: given a REAL on-disk
/// path (a fetcher-cache `~/.cache/sui/inputs/…` file that eval actually
/// read from), return the flake input's `/nix/store/<narhash>-source`
/// STORE-PATH equivalent (with the same relative subpath appended); returns
/// `path` unchanged when it is not under any registered input's cache dir.
///
/// This closes the position half of the store↔cache seam. `materialize`
/// redirects a store-path READ down to the cache; `dematerialize` lifts a
/// cache-path back up to the store path for REPORTING — so
/// `builtins.unsafeGetAttrPos`/`__curPos` reports the store-source `.file`
/// CppNix reports (`/nix/store/<h>-source/lib/foo.nix`), NOT the sui fetcher
/// cache dir. nix-darwin's `doc/manual` `hasPrefix <nix-darwin>.outPath decl`
/// rewrite only fires when `decl` carries the store prefix — the
/// `options.json` dock-declarations root.
///
/// Both sides are compared after `canonicalize` on the cache side (a
/// symlinked cache dir — macOS `/tmp` → `/private/tmp`, `~` expansion —
/// still matches the registered `read_dir`, which is canonicalized here
/// too). Only the reported STRING changes; no value the evaluator observes
/// is mutated — the byte-parity invariant.
#[must_use]
pub fn dematerialize(path: &Path) -> PathBuf {
    // Probe canon through the success-only memo; a failed canon falls back to
    // the raw path exactly as before. The per-entry `read_dir` canon is now
    // precomputed at registration — this loop used to re-realpath every
    // registered input on every call, which sampling showed as 61% of the
    // eval thread's wall-clock on the cid marquee (getattrlist per component,
    // per input, per path literal).
    let canon = canonicalize_memo(path);
    let probe: &Path = canon.as_deref().unwrap_or(path);
    INPUT_SOURCE_MAP.with(|m| {
        for (store_path, read_dir, rd_canon) in m.borrow().iter() {
            if probe == rd_canon.as_path() {
                return store_path.clone();
            }
            if let Ok(suffix) = probe.strip_prefix(rd_canon) {
                return store_path.join(suffix);
            }
            // Also try the un-canonicalized read_dir (registration may have
            // stored a symlinked path); harmless when it already matched above.
            if path == read_dir.as_path() {
                return store_path.clone();
            }
            if let Ok(suffix) = path.strip_prefix(read_dir) {
                return store_path.join(suffix);
            }
        }
        path.to_path_buf()
    })
}

/// Convenience: [`dematerialize`] a `&str` path, returning an owned `String`.
#[must_use]
pub fn dematerialize_str(path: &str) -> String {
    dematerialize(Path::new(path)).to_string_lossy().into_owned()
}

/// Reverse of [`materialize`] for the copy-to-store NAMING rule: given a
/// REAL on-disk directory (the fetcher-cache `read_dir` a `materialize`
/// already resolved to, then `canonicalize`d), return the store-path
/// BASENAME of the flake input that tree belongs to — i.e. CppNix's
/// `-source` name — when `real` IS that input's whole tree root.
///
/// This closes the darwin `system-path` root: a fetched flake input's
/// `src = ./.` copies the input's own tree back into the store; CppNix names
/// that copy after the input's `/nix/store/<h>-source` basename
/// (`<h>-source`), but sui reads the tree from `~/.cache/sui/inputs/<narhash>`
/// whose basename is `<repo>-<rev>`. Only the NAME needs correcting — the
/// bytes (→ NAR hash) are identical either way — so this maps the physical
/// read location back to the logical `-source` name.
///
/// Both sides are `canonicalize`d before comparison so a symlinked cache dir
/// (macOS `/tmp` → `/private/tmp`, `~` expansion) still matches. Returns
/// `None` when `real` is not a registered input root (a normal local
/// `src = ./.` keeps its own directory basename).
#[must_use]
pub fn source_name_for_read_dir(real: &Path) -> Option<String> {
    // Probe via the success-only memo; per-entry canons are precomputed at
    // registration (see the INPUT_SOURCE_MAP doc for the measured root).
    let real_canon = canonicalize_memo(real)?;
    // 1) Flake-input trees: a fetched input's `src = ./.` copies the input's
    //    whole tree, named after the input's `/nix/store/<h>-source` basename.
    let from_input = INPUT_SOURCE_MAP.with(|m| {
        for (store_path, _read_dir, rd_canon) in m.borrow().iter() {
            // Only the WHOLE tree root maps to the input's `-source` name; a
            // subpath (`src = ./subdir`) copies a sub-tree CppNix names after
            // that subdir, so require an exact root match.
            if real_canon == *rd_canon {
                return store_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned());
            }
        }
        None
    });
    if from_input.is_some() {
        return from_input;
    }
    // 2) `builtins.fetch*` result trees: `zshSynHlSrc = builtins.fetchGit {…}`
    //    coerced-to-store must be named `source` (CppNix's convention), NOT the
    //    fetcher-cache temp basename (a 64-char sha256 cache-hash). The fetcher
    //    registered the correct name for this exact dir.
    FETCHED_SOURCE_NAMES.with(|m| {
        for (_read_dir, name, rd_canon) in m.borrow().iter() {
            if real_canon == *rd_canon {
                return Some(name.clone());
            }
        }
        None
    })
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
    fn dematerialize_lifts_cache_path_to_store_source() {
        // The options.json dock-declarations root (Layer B): a source
        // position read from the fetcher-cache dir must be REPORTED as the
        // input's `/nix/store/<h>-source` path so nix-darwin's `hasPrefix`
        // rewrite fires. `dematerialize` is the reverse of `materialize`.
        let cache = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cache.path().join("modules/system")).unwrap();
        let store = Path::new(
            "/nix/store/npm9dap7j0i92l524y09x255zi9447qp-source",
        );
        register_input_source(store, cache.path());
        // A subpath under the cache dir lifts to the store path + subpath.
        let real = cache.path().join("modules/system/dock.nix");
        assert_eq!(
            dematerialize(&real),
            store.join("modules/system/dock.nix"),
        );
        // The cache root itself lifts to the bare store path.
        assert_eq!(dematerialize(cache.path()), store.to_path_buf());
    }

    #[test]
    fn dematerialize_passes_unregistered_paths_through() {
        // A local (unregistered) path is reported verbatim — only a
        // registered input's cache subtree is lifted to its store path.
        let unrelated = tempfile::tempdir().unwrap();
        let p = unrelated.path().join("some/file.nix");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"x").unwrap();
        assert_eq!(dematerialize(&p), p);
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

    #[test]
    fn source_name_maps_read_dir_root_to_store_source_name() {
        // The darwin `system-path` root: a fetched flake input's `src = ./.`
        // copies the tree read from the fetcher cache `read_dir`, but the copy
        // must carry the input's `/nix/store/<h>-source` BASENAME (CppNix's
        // name), not the cache dir's `<repo>-<rev>` basename.
        let cache = tempfile::tempdir().unwrap();
        let store_path = Path::new(
            "/nix/store/9qcaaxf4dyy09df0gv4ibfj93aplq3jk-source",
        );
        register_input_source(store_path, cache.path());
        // The whole-tree root maps to the input's `-source` name.
        assert_eq!(
            source_name_for_read_dir(cache.path()).as_deref(),
            Some("9qcaaxf4dyy09df0gv4ibfj93aplq3jk-source"),
        );
    }

    #[test]
    fn source_name_returns_none_for_unregistered_or_subpath() {
        // A local `src = ./.` (unregistered) keeps its own basename → None,
        // so the caller falls back to the real dir's file_name. A SUBpath of a
        // registered root also returns None (only the whole-tree root is the
        // `-source` copy — `src = ./subdir` is named after the subdir).
        let unrelated = tempfile::tempdir().unwrap();
        assert_eq!(source_name_for_read_dir(unrelated.path()), None);

        let cache = tempfile::tempdir().unwrap();
        std::fs::create_dir(cache.path().join("subdir")).unwrap();
        register_input_source(
            Path::new("/nix/store/dddddddddddddddddddddddddddddddd-source"),
            cache.path(),
        );
        assert_eq!(source_name_for_read_dir(&cache.path().join("subdir")), None);
    }

    #[test]
    fn fetched_source_dir_maps_to_source_name() {
        // The zsh-syntax-highlighting-config root: `builtins.fetchGit` returns a
        // temp dir whose basename is a 64-char sha256 cache-hash. Coerced-to-
        // store as a derivation `src`, CppNix names it `-source`, not the
        // cache-hash. `register_fetched_source` records that intended name.
        let cache = tempfile::tempdir().unwrap();
        register_fetched_source(cache.path(), "source");
        assert_eq!(
            source_name_for_read_dir(cache.path()).as_deref(),
            Some("source"),
        );
    }

    #[test]
    fn fetched_source_registry_honors_explicit_name() {
        // The registered name flows through verbatim (a fetcher that honors an
        // explicit `name` arg would register that name instead of "source").
        let cache = tempfile::tempdir().unwrap();
        register_fetched_source(cache.path(), "my-thing");
        assert_eq!(
            source_name_for_read_dir(cache.path()).as_deref(),
            Some("my-thing"),
        );
    }
}
