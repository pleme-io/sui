//! Path builtins: baseNameOf, dirOf, toPath, storePath, pathExists, readFile,
//! readDir, readFileType, path, filterSource.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    // baseNameOf — extract filename from path.
    // CppNix strips EXACTLY ONE trailing slash, then takes the last component
    // (everything after the last remaining `/`). Verified vs `nix eval`:
    //   "/a/b/" → "b"   "a/" → "a"   "a//" → ""   "a/b//" → ""   "" → ""
    // (The prior `trim_end_matches('/')` stripped ALL trailing slashes, so it
    // wrongly returned "a" for "a//" where nix returns "".)
    fn base_name_of(s: &str) -> String {
        let trimmed = s.strip_suffix('/').unwrap_or(s);
        trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
    }
    register_builtin(builtins, "baseNameOf", |args| {
        match &args[0] {
            Value::String(ns) => {
                let base = base_name_of(&ns.chars);
                // nix: baseNameOf preserves string context (verified
                // `nix eval` hasContext=true) — the store-path dep survives.
                Ok(Value::String(std::rc::Rc::new(
                    crate::value::NixString::with_context(base, ns.context.clone()),
                )))
            }
            Value::Path(p) => Ok(Value::string(base_name_of(&p.to_string()))),
            _ => Err(EvalError::TypeError("baseNameOf: expected string or path".to_string())),
        }
    });

    // dirOf — extract directory from path
    register_builtin(builtins, "dirOf", |args| {
        let (s, ctx) = match &args[0] {
            // nix: dirOf preserves string context for a String arg (verified
            // `nix eval` hasContext=true).
            Value::String(ns) => (ns.chars.to_string(), Some(ns.context.clone())),
            Value::Path(p) => {
                let s = p.to_string();
                let dir = match s.rfind('/') {
                    Some(0) => "/".to_string(),
                    Some(i) => s[..i].to_string(),
                    None => ".".to_string(),
                };
                return Ok(Value::Path(Box::new(SmolStr::from(dir.as_str()))));
            }
            _ => return Err(EvalError::TypeError("dirOf: expected string or path".to_string())),
        };
        let dir = match s.rfind('/') {
            Some(0) => "/".to_string(),
            Some(i) => s[..i].to_string(),
            None => ".".to_string(),
        };
        match ctx {
            Some(ctx) => Ok(Value::String(std::rc::Rc::new(
                crate::value::NixString::with_context(dir, ctx),
            ))),
            None => Ok(Value::string(dir)),
        }
    });

    // readFile — read file contents to string.  Consults the
    // sui-tofile-cache fallback when /nix/store/<x> is missing,
    // so that `builtins.readFile (builtins.toFile name content)`
    // round-trips even when the operator isn't a nixbld user.
    register_builtin(builtins, "readFile", |args| {
        // `coerce_to_realized_path` triggers import-from-derivation: reading a
        // file under a derivation's `outPath` realizes that output first.
        let path = args[0].coerce_to_realized_path("readFile")?;
        let contents = read_file_with_tofile_fallback(&path)
            .map_err(|e| EvalError::IoError { context: "readFile".into(), message: e.to_string() })?;
        Ok(Value::string(contents))
    });

    register_builtin(builtins, "readFileType", |args| {
        let path = crate::path::materialize_str(&args[0].as_string()?);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                let kind = if meta.is_symlink() {
                    "symlink"
                } else if meta.is_dir() {
                    "directory"
                } else if meta.is_file() {
                    "regular"
                } else {
                    "unknown"
                };
                Ok(Value::string(kind))
            }
            Err(e) => Err(EvalError::IoError { context: "readFileType".into(), message: e.to_string() }),
        }
    });

    register_builtin(builtins, "readDir", |args| {
        // IFD: reading a directory under a derivation's `outPath` realizes it.
        let path_str = crate::path::materialize_str(&args[0].coerce_to_realized_path("readDir")?);
        let mut attrs = NixAttrs::new();
        for entry in std::fs::read_dir(&path_str)
            .map_err(|e| EvalError::IoError { context: "readDir".into(), message: e.to_string() })?
        {
            let entry = entry.map_err(|e| EvalError::IoError { context: "readDir".into(), message: e.to_string() })?;
            let name = entry.file_name().to_string_lossy().to_string();
            let ft = entry.file_type().map_err(|e| EvalError::IoError { context: "readDir".into(), message: e.to_string() })?;
            let type_str = if ft.is_dir() {
                "directory"
            } else if ft.is_symlink() {
                "symlink"
            } else {
                "regular"
            };
            attrs.insert(name, Value::string(type_str));
        }
        Ok(Value::Attrs(Rc::new(attrs)))
    });

    register_builtin(builtins, "toPath", |args| {
        let s = args[0].as_string()?;
        if !s.starts_with('/') {
            return Err(EvalError::TypeError(format!("toPath: path must be absolute: {s}")));
        }
        Ok(Value::Path(Box::new(SmolStr::from(s))))
    });

    register_builtin(builtins, "storePath", |args| {
        let s = args[0].as_string()?;
        if !s.starts_with("/nix/store/") {
            return Err(EvalError::TypeError(format!("storePath: not a store path: {s}")));
        }
        Ok(Value::Path(Box::new(SmolStr::from(s))))
    });

    // pathExists — cppnix REALIZES a derivation argument (verified: `nix eval
    // 'builtins.pathExists "${drv}"'` builds the drv, then returns true), so a
    // derivation coercion goes through the IFD-aware helper.
    register_builtin(builtins, "pathExists", |args| {
        let path_str = crate::path::materialize_str(&args[0].coerce_to_realized_path("pathExists")?);
        Ok(Value::Bool(std::path::Path::new(&path_str).exists()))
    });

    // builtins.path { path; name?; sha256?; recursive?; filter?; }
    //
    // CppNix builds the store path by NAR-serializing the source tree
    // (the default `recursive`/"source" mode), sha256-hashing the NAR,
    // and computing a `fixed:out:r:sha256:<hex>` store path named by
    // `name` (or the path's basename). This is the SAME machinery a
    // plain path-value copy-to-store uses (`nar_hash_source_tree`),
    // proven byte-identical to nix. The previous impl hand-rolled a
    // `sha256(file-content-bytes)[..32]` hex placeholder — a wrong
    // store path that diverged every `builtins.path`-produced input
    // (e.g. llvm's `getVersionFile` patches → the whole LLVM/Rust
    // toolchain, ~13 packages).
    register_builtin(builtins, "path", |args| {
        let attrs = args[0].to_attrs()?;
        let path_val = attrs
            .get("path")
            .ok_or_else(|| EvalError::AttrNotFound("path".into()))?;
        let path_forced = crate::eval::force_value(path_val)?;
        // IFD: `builtins.path { path = <drv>; }` realizes the derivation output
        // before NAR-hashing the source tree.
        let path_str = path_forced.coerce_to_realized_path("path")?;

        // Absolutize (relative literals resolve against the evaluating
        // file's dir, matching CppNix's parse-time absolutization) then
        // canonicalize (realpath) — identical to the copy-to-store
        // coercion arm in `Value::coerce_to_string_impl`.
        let pb = std::path::Path::new(&path_str);
        let abs = if pb.is_absolute() {
            pb.to_path_buf()
        } else if let Some(dir) = crate::eval::current_eval_dir() {
            dir.join(pb)
        } else {
            std::env::current_dir()
                .map_err(|e| EvalError::IoError {
                    context: format!("builtins.path: {path_str}"),
                    message: e.to_string(),
                })?
                .join(pb)
        };
        // Redirect a fetched flake input's `-source` store path to its real
        // on-disk tree before realpath (the store path isn't materialized);
        // the resulting store hash is computed from the SAME tree cppnix
        // NAR-hashed, so the value is byte-identical.
        let canon = crate::path::materialize(&abs).canonicalize().map_err(|_| {
            EvalError::TypeError(format!("path '{}' does not exist", abs.display()))
        })?;

        let name = attrs
            .get("name")
            .map(|v| crate::eval::force_value(v).and_then(|f| f.to_str()))
            .transpose()?
            .unwrap_or_else(|| {
                canon
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });

        // A `filter` (Nix `path -> type -> bool`) prunes the tree BEFORE
        // NAR-hashing — `lib.cleanSourceWith` (crane, cleanCargoSource,
        // and every filtered nixpkgs `src`) is `builtins.path { filter;
        // path; }`, so ignoring the filter NAR-hashes the WRONG tree and
        // diverges the store path. Materialize the kept subtree in a
        // deterministic temp dir, then NAR-hash that (giving the exact
        // same store path CppNix computes over the filtered contents).
        let filter = attrs.get("filter").filter(|v| {
            !matches!(
                crate::eval::force_value(v),
                Ok(Value::Null) | Err(_)
            )
        });
        let hash_root: std::path::PathBuf = if let Some(filter_fn) = filter {
            let filter_fn = crate::eval::force_value(filter_fn)?;
            // The filter must see the path string CppNix would pass — rooted
            // at the ORIGINAL src the caller wrote (`abs`, e.g. a
            // `/nix/store/<h>-source` flake-input path), not the materialized
            // on-disk cache tree (`canon`) sui walks. They differ only for a
            // redirected flake input.
            materialize_filtered(&canon, &filter_fn, &name, &abs)?
        } else {
            canon.clone()
        };

        let src = sui_compat::source::nar_hash_source_tree(&hash_root, &name).map_err(|e| {
            EvalError::TypeError(format!(
                "builtins.path: NAR-hashing '{}': {e}",
                hash_root.display()
            ))
        })?;

        // Optional integrity check: nix verifies the supplied `sha256`
        // (SRI / bare-base64 / hex / base32) against the NAR hash.
        if let Some(expected) = attrs.get("sha256") {
            let expected_forced = crate::eval::force_value(expected)?;
            let expected_str = expected_forced.to_str()?;
            let want = sui_compat::hash::NixHash::parse_any(
                sui_compat::hash::HashAlgorithm::Sha256,
                expected_str.trim_start_matches("sha256-"),
            )
            .or_else(|_| {
                sui_compat::hash::NixHash::parse_any(
                    sui_compat::hash::HashAlgorithm::Sha256,
                    &expected_str,
                )
            });
            if let Ok(want) = want {
                let got = sui_compat::hash::NixHash::parse_any(
                    sui_compat::hash::HashAlgorithm::Sha256,
                    src.nar_hash_sri.trim_start_matches("sha256-"),
                );
                if let Ok(got) = got {
                    if want.digest != got.digest {
                        return Err(EvalError::TypeError(format!(
                            "path: sha256 mismatch: expected {expected_str}, got {}",
                            src.nar_hash_sri
                        )));
                    }
                }
            }
        }

        // CppNix's `builtins.path` returns a STRING carrying the store
        // path as opaque (`Plain`) context — NOT a Path value. This is
        // load-bearing: a Path value would be RE-copied when coerced
        // into a derivation env (per the copy-to-store arm's "a Path is
        // always copied" rule), producing a doubled `<hash>-<hash>-name`
        // store path and diverging every consumer (e.g. llvm's patches
        // → the whole LLVM/Rust toolchain). A String-with-context is
        // referenced verbatim, exactly like nix.
        let mut ctx = StringContext::new();
        ctx.add_plain(src.store_path.clone());
        Ok(Value::String(std::rc::Rc::new(NixString::with_context(
            SmolStr::from(src.store_path.as_str()),
            ctx,
        ))))
    });

    // filterSource — `builtins.filterSource pred src` is exactly
    // `builtins.path { path = src; filter = pred; }` in CppNix: same
    // root-dumped-unconditionally + descendants-filtered semantics, same
    // NAR-hash-of-the-materialized-tree store path. Route it through the
    // shared `materialize_filtered` + `nar_hash_source_tree` engine so the
    // two builtins can never drift, and so filterSource returns a real
    // `/nix/store/…` NAR path (the prior impl returned a bespoke temp-dir
    // path hashed over relative-path bytes — wrong shape AND wrong hash —
    // and shared the same root-filter pruning bug as builtins.path did).
    register_curried(builtins, "filterSource", |pred, src| {
        let src_path = src.coerce_to_path("filterSource")?;
        // Redirect a fetched flake input's `-source` store subpath to its
        // real on-disk tree (the store path isn't materialized by sui); the
        // store path returned below is NAR-hashed from tree CONTENT, so it is
        // byte-identical whether read from the store path or the cache.
        let src_path_buf = crate::path::materialize(std::path::Path::new(&src_path));
        if !src_path_buf.exists() {
            return Err(EvalError::IoError {
                context: format!("filterSource: {src_path}"),
                message: "no such file or directory".into(),
            });
        }
        let canon = std::fs::canonicalize(&src_path_buf).unwrap_or(src_path_buf.clone());
        // ── ★ STRIP THE STORE-PATH HASH FROM THE NAME ─────────────────────
        // `canon` is the MATERIALIZED path, so when the source already lives in
        // the store its basename is `<hash>-<name>` — and feeding that to
        // `nar_hash_source_tree` as the NAME produces `<newhash>-<oldhash>-<name>`,
        // a store path with the old hash embedded in it.
        //
        // MEASURED 2026-08-11, and it is the root of R2 on `minimal`:
        //   nix src: /nix/store/ar6s9jl9…-nixos-firewall-tool
        //   sui src: /nix/store/nr1fj1g7…-ar6s9jl9…-nixos-firewall-tool
        // 66 extra bytes in the ATerm, which changes `out`, which changes the
        // drvPath, which changes system-path, which changes the whole node
        // toplevel. One package out of 189 — nixos-firewall-tool is the only one
        // in that closure calling `filterSource` on an in-store path.
        //
        // CppNix's `addToStore` takes the name as a SEPARATE argument and never
        // re-derives it from a store basename, which is why nix is unaffected.
        // Stripping the 32-char nix-base32 hash + '-' restores that: a
        // non-store path has no such prefix and is unchanged.
        let raw_name = canon
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "source".into());
        let name = sui_compat::source::strip_store_hash_prefix(&raw_name);
        let pred_fn = crate::eval::force_value(&pred)?;
        // Filter sees the ORIGINAL src path string (`src_path`, the store
        // path a flake input carries), not the materialized cache tree
        // (`canon`) sui walks. See materialize_filtered's CRITICAL semantic #2.
        let hash_root =
            materialize_filtered(&canon, &pred_fn, &name, std::path::Path::new(&*src_path))?;
        let s = sui_compat::source::nar_hash_source_tree(&hash_root, &name).map_err(|e| {
            EvalError::IoError {
                context: format!("filterSource: NAR-hashing '{}'", hash_root.display()),
                message: e.to_string(),
            }
        })?;
        Ok(Value::Path(Box::new(SmolStr::from(s.store_path.as_str()))))
    });
}

/// Apply a Nix `filter` (`path -> type -> bool`) to a source tree and
/// materialize the KEPT entries into a deterministic temp directory,
/// returning that directory so it can be NAR-hashed. This is the
/// `builtins.path { filter; … }` / `lib.cleanSourceWith` engine: nix
/// calls `filter <abs-path> <type>` for each entry (`type` ∈
/// {"regular","directory","symlink"}), keeps it iff the result is
/// true, and does NOT recurse into a pruned directory.
///
/// The temp dir is keyed by the origin path + a BLAKE-free content
/// tag so repeated evals of the same filtered source reuse it; the
/// NAR hash over the materialized tree is what determines the store
/// path, so the temp location itself never leaks into the result.
fn materialize_filtered(
    src_root: &std::path::Path,
    filter_fn: &Value,
    name: &str,
    reported_root: &std::path::Path,
) -> Result<std::path::PathBuf, EvalError> {
    // Recursively decide + record kept entries (relative paths).
    //
    // CRITICAL semantic: CppNix NEVER applies the filter to the ROOT
    // path handed to `builtins.path` / `filterSource` — the root is
    // dumped unconditionally, and the filter governs only which of its
    // *descendants* are included. So `walk` always keeps `current` and
    // only consults the filter for each *child* before recursing into
    // it. Applying the filter to the root and pruning on a `false`
    // result (the prior bug) empties the whole tree whenever the root's
    // basename fails the predicate — which is EXACTLY what every
    // `lib.cleanSourceWith { filter = p: t: elem (baseNameOf p) [...] }`
    // does (the root dir's basename is never in the keep-list). That
    // made `documentation-highlighter`'s filtered `src` NAR-hash to the
    // empty-directory store path (0ccnxa25…) instead of nix's l2kdhi9h…,
    // and every drv consuming a filtered source diverged its output path
    // (Darwin Root #2).
    //
    // CRITICAL semantic #2 (cid Root: rust_sui-compat filtered `src`):
    // the path STRING handed to the filter must be the one CppNix would
    // pass — i.e. rooted at the ORIGINAL src path the caller wrote
    // (`reported_root`, typically a `/nix/store/<h>-source` flake-input
    // path), NOT the real on-disk tree sui walks (`base`, a materialized
    // `~/.cache/sui/inputs/…` cache dir). A fetched flake input's store
    // path is REDIRECTED to its cache dir before the walk (so reads land
    // on real bytes), but the filter is a value the evaluator observes,
    // so it MUST see the store path. Passing the cache path broke every
    // `lib.cleanSourceWith { filter = p: t: … removePrefix (toString src)
    // … }` filter (`removePrefix` no-ops on the mismatched prefix, so the
    // filter's `rel == "src"` / `hasPrefix "src/"` checks never fire and
    // the excluded `src/` subtree is wrongly KEPT). `reported_root` +
    // `base` differ only when the src is a redirected flake input; when
    // they're equal this is a no-op.
    fn walk(
        base: &std::path::Path,
        reported_root: &std::path::Path,
        current: &std::path::Path,
        current_md: &std::fs::Metadata,
        filter_fn: &Value,
        kept: &mut Vec<(std::path::PathBuf, std::fs::Metadata)>,
    ) -> Result<(), EvalError> {
        // `current` is already decided-kept by the caller (the root is
        // kept unconditionally; a child is kept iff it passed the
        // filter). Record it, then descend into a kept directory's
        // children, filtering each.
        let rel = current.strip_prefix(base).unwrap_or(current).to_path_buf();
        if !rel.as_os_str().is_empty() {
            kept.push((rel, current_md.clone()));
        }
        if current_md.is_dir() {
            let mut children: Vec<std::path::PathBuf> = std::fs::read_dir(current)
                .map_err(|e| EvalError::IoError {
                    context: format!("builtins.path filter: {}", current.display()),
                    message: e.to_string(),
                })?
                .flatten()
                .map(|e| e.path())
                .collect();
            children.sort();
            for child in children {
                let child_md = std::fs::symlink_metadata(&child).map_err(|e| EvalError::IoError {
                    context: format!("builtins.path filter: {}", child.display()),
                    message: e.to_string(),
                })?;
                let type_str = if child_md.is_dir() {
                    "directory"
                } else if child_md.file_type().is_symlink() {
                    "symlink"
                } else {
                    "regular"
                };
                // nix passes the absolute path string as the first arg —
                // rooted at the ORIGINAL src (`reported_root`), NOT the
                // materialized on-disk cache tree (`base`) sui actually
                // walks. Rewrite the child's `base`-relative suffix onto
                // `reported_root` so a user filter comparing against
                // `toString src` sees the store path CppNix would pass.
                let child_rel = child.strip_prefix(base).unwrap_or(&child);
                let reported_path = reported_root.join(child_rel);
                let path_arg = Value::string(reported_path.to_string_lossy().to_string());
                let type_arg = Value::string(type_str);
                let partial = crate::eval::apply(filter_fn.clone(), path_arg)?;
                let keep = crate::eval::apply_and_force(partial, type_arg)?.as_bool()?;
                if !keep {
                    continue;
                }
                walk(base, reported_root, &child, &child_md, filter_fn, kept)?;
            }
        }
        Ok(())
    }

    let mut kept: Vec<(std::path::PathBuf, std::fs::Metadata)> = Vec::new();
    let root_md = std::fs::symlink_metadata(src_root).map_err(|e| EvalError::IoError {
        context: format!("builtins.path filter: {}", src_root.display()),
        message: e.to_string(),
    })?;
    walk(src_root, reported_root, src_root, &root_md, filter_fn, &mut kept)?;

    // Deterministic temp dir keyed by the source path + the sorted set
    // of kept relative paths (so two different filters over the same
    // source don't collide, and the same filter reuses the tree).
    use sha2::{Digest, Sha256};
    let mut tag = Sha256::new();
    tag.update(src_root.to_string_lossy().as_bytes());
    tag.update([0u8]);
    for (rel, _) in &kept {
        tag.update(rel.to_string_lossy().as_bytes());
        tag.update([0u8]);
    }
    let tag_hex: String = tag.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let target = std::env::temp_dir()
        .join("sui-path-filter")
        .join(format!("{}-{name}", &tag_hex[..32]));

    // Materialize (idempotent).
    std::fs::create_dir_all(&target).map_err(|e| EvalError::IoError {
        context: format!("builtins.path filter: {}", target.display()),
        message: e.to_string(),
    })?;
    for (rel, md) in &kept {
        let dst = target.join(rel);
        let src = src_root.join(rel);
        if md.is_dir() {
            std::fs::create_dir_all(&dst).map_err(|e| EvalError::IoError {
                context: format!("builtins.path filter: {}", dst.display()),
                message: e.to_string(),
            })?;
        } else {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if dst.exists() {
                continue;
            }
            if md.file_type().is_symlink() {
                let link = std::fs::read_link(&src).map_err(|e| EvalError::IoError {
                    context: format!("builtins.path filter: {}", src.display()),
                    message: e.to_string(),
                })?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(&link, &dst).ok();
            } else {
                std::fs::copy(&src, &dst).map_err(|e| EvalError::IoError {
                    context: format!("builtins.path filter: {}", dst.display()),
                    message: e.to_string(),
                })?;
            }
        }
    }
    Ok(target)
}

/// Read a UTF-8 file, consulting the sui-tofile-cache fallback when
/// the primary path is missing.  Pairs with
/// [`super::misc::write_store_text_object`] (toFile) so that
/// `readFile (toFile name content) == content` even on hosts where
/// the operator can't write to `/nix/store` directly.
fn read_file_with_tofile_fallback(path: &str) -> Result<String, std::io::Error> {
    // Redirect a fetched flake input's `-source` store path to its real
    // on-disk tree (the store path isn't materialized by sui); a no-op for
    // any other path.
    let path = &crate::path::materialize_str(path);
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
                && path.starts_with("/nix/store/") => {
            let basename = std::path::Path::new(path)
                .file_name()
                .ok_or_else(|| std::io::Error::other(
                    format!("readFile: cannot derive basename from {path}"),
                ))?;
            let fallback = std::env::temp_dir()
                .join("sui-tofile-cache")
                .join(basename);
            std::fs::read_to_string(&fallback)
        }
        Err(e) => Err(e),
    }
}
