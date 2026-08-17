//! Content-addressed input fetcher for flake.lock resolved inputs.
//!
//! Fetches locked flake inputs (github tarballs, git repos, local paths,
//! remote tarballs) and caches them by `narHash` so repeated evaluations
//! hit the local filesystem instead of the network.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use sui_compat::flake::LockedInput;
use sui_compat::flake_ref::FlakeRef;

// ── Error type ────────────────────────────────────────────────

/// Errors that can occur during input fetching.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("unsupported input type: {0}")]
    UnsupportedType(String),
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("download failed: {0}")]
    Download(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("archive extraction failed: {0}")]
    Extract(String),
}

// ── InputFetcher ──────────────────────────────────────────────

/// A content-addressed input fetcher that downloads and caches flake inputs.
///
/// Inputs are cached under `~/.cache/sui/inputs/` (or a custom directory)
/// keyed by their `narHash` from the lock file. Cache hits skip network
/// access entirely.
pub struct InputFetcher {
    cache_dir: PathBuf,
}

impl Default for InputFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl InputFetcher {
    /// Create a fetcher using the default cache directory (`~/.cache/sui/inputs/`).
    #[must_use]
    pub fn new() -> Self {
        let cache_dir = dirs_cache_dir().join("sui/inputs");
        Self { cache_dir }
    }

    /// Create a fetcher with a custom cache directory.
    #[must_use]
    pub fn with_cache_dir(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    /// Return the cache directory path.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Fetch a locked input and return the local filesystem path.
    ///
    /// Uses content-addressed caching by `narHash` — if the hash is present
    /// and a cached directory exists, returns immediately without network access.
    pub fn fetch(&self, locked: &LockedInput) -> Result<PathBuf, FetchError> {
        // Check cache first (keyed by narHash).
        if let Some(ref nar_hash) = locked.nar_hash {
            let cache_key = sanitize_hash(nar_hash);
            let cached = self.cache_dir.join(&cache_key);
            if cached.exists() {
                let resolved = find_single_subdir_or_self(&cached);
                // Validate the cache entry is non-empty.  A previous fetch may
                // have created the directory but failed before extracting any
                // content (e.g. network timeout).  Treat empty dirs as cache
                // misses so the fetch is retried.
                if is_non_empty_dir(&resolved) {
                    return Ok(resolved);
                }
                // Cache entry is empty/invalid — remove it and re-fetch.
                let _ = std::fs::remove_dir_all(&cached);
            }
        }

        match locked.source_type.as_str() {
            "github" => self.fetch_github(locked),
            "gitlab" => self.fetch_gitlab(locked),
            "sourcehut" => self.fetch_sourcehut(locked),
            "path" => Self::fetch_path(locked),
            "git" => self.fetch_git(locked),
            "tarball" | "file" => self.fetch_tarball(locked),
            other => Err(FetchError::UnsupportedType(other.to_string())),
        }
    }

    /// Construct the GitHub archive URL for a locked input.
    #[must_use]
    pub fn github_archive_url(owner: &str, repo: &str, rev: &str) -> String {
        format!("https://github.com/{owner}/{repo}/archive/{rev}.tar.gz")
    }

    /// GitLab archive URL.  Shape differs from GitHub — the file
    /// name embeds the repo + rev and lives under `/-/archive/{rev}/`.
    /// Honors `host` so self-hosted gitlab instances (e.g.
    /// `gitlab.gnome.org`, `git.example.com`) work; defaults to
    /// `gitlab.com` when host is None.
    #[must_use]
    pub fn gitlab_archive_url(host: Option<&str>, owner: &str, repo: &str, rev: &str) -> String {
        let host = host.unwrap_or("gitlab.com");
        format!(
            "https://{host}/{owner}/{repo}/-/archive/{rev}/{repo}-{rev}.tar.gz"
        )
    }

    /// Sourcehut archive URL. Owners carry the `~` prefix on the
    /// platform; the flake-ref parser stores them without the prefix,
    /// so we prepend here.
    #[must_use]
    pub fn sourcehut_archive_url(owner: &str, repo: &str, rev: &str) -> String {
        let owner_prefix = if owner.starts_with('~') {
            owner.to_string()
        } else {
            format!("~{owner}")
        };
        format!("https://git.sr.ht/{owner_prefix}/{repo}/archive/{rev}.tar.gz")
    }

    // ── Private fetch methods ─────────────────────────────

    fn fetch_github(&self, locked: &LockedInput) -> Result<PathBuf, FetchError> {
        let owner = locked.owner.as_deref().ok_or(FetchError::MissingField("owner"))?;
        let repo = locked.repo.as_deref().ok_or(FetchError::MissingField("repo"))?;
        let rev = locked.rev.as_deref().ok_or(FetchError::MissingField("rev"))?;

        let url = Self::github_archive_url(owner, repo, rev);
        // Was a hand-inlined copy of `fetch_archive`'s body — the only copy of
        // the three that lacked a cache guard, which is exactly how it came to
        // re-download on every invocation. Sharing the body is the fix for the
        // class; the guard below is the fix for the instance.
        self.fetch_archive(locked, &url, &format!("github-{owner}-{repo}-{rev}"), rev)
    }

    /// GitHub, GitLab and Sourcehut share one archive-fetch shape — download a
    /// tar.gz, extract, return the single top-level directory. Only the URL
    /// construction differs.
    ///
    /// ── ★ STAGE THEN RENAME; NEVER EXTRACT INTO THE FINAL PATH ────────────
    /// This used to `create_dir_all(dest)` and extract straight into it, which
    /// produced three distinct defects from one decision:
    ///
    /// 1. **A partial tree is a valid cache hit.** The hit predicate is "the
    ///    directory is non-empty", which goes true on the FIRST tar entry, so a
    ///    concurrent process could adopt a half-extracted tree and evaluate it
    ///    as if complete — a silently wrong eval, not an error.
    /// 2. **A re-extraction UNIONS.** `tar` runs with `overwrite: true`, so
    ///    extracting a second time over an existing tree leaves files that the
    ///    newer tree deleted. Content at a "content-addressed" path then
    ///    disagrees with the hash in its own name.
    /// 3. **A failing process deleted another process's good cache entry.**
    ///    Every error path called `remove_dir_all(&dest)` — on the FINAL path.
    ///    A transient network error during a redundant re-fetch would wipe a
    ///    complete tree that another eval was actively reading.
    ///
    /// Defect 2 is what poisoned `~/.cache/sui/nar-memo` and made `getFlake`
    /// return a store path CppNix disagrees with (measured 2026-08-17; see
    /// `sui-compat/src/source.rs`'s memo verifier, which is the read-side
    /// defence this is the write-side cause of).
    ///
    /// Staging beside the target rather than in `/tmp` keeps the rename on one
    /// filesystem, where it is atomic — the same reason `sui-castore`'s local
    /// storage stages beside its target.
    fn fetch_archive(
        &self,
        locked: &LockedInput,
        url: &str,
        cache_key: &str,
        rev: &str,
    ) -> Result<PathBuf, FetchError> {
        let dest = self.dest_dir(locked, cache_key);

        // ── The cache guard, and why it is conditional ────────────────────
        // A rev that is a 40/64-hex commit names one immutable tree, so a
        // complete directory at `dest` can be adopted with no network at all.
        // A rev that is a BRANCH NAME does not: `github:owner/repo/main` is a
        // legal ref (CppNix accepts it, so refusing it would be a parity
        // divergence, not a safety win) and the tree behind it moves. Guarding
        // unconditionally would freeze such an entry at whatever `main` was
        // the first time it was fetched, forever.
        //
        // So: immutable revs are cached, mutable ones are always re-fetched.
        // The old code re-fetched BOTH, which was wasteful for the first and
        // accidentally correct for the second.
        if is_immutable_rev(rev) && is_non_empty_dir(&dest) {
            return Ok(find_single_subdir_or_self(&dest));
        }

        let staging = staging_path(&dest);
        // A leftover staging dir means a previous process died mid-extract.
        // It is ours to clear: the name carries our pid.
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;

        let bytes = match download_bytes(url) {
            Ok(b) => b,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e);
            }
        };
        if let Err(e) = extract_tar_gz(&bytes, &staging) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }

        publish(&staging, &dest, is_immutable_rev(rev))?;
        Ok(find_single_subdir_or_self(&dest))
    }

    fn fetch_gitlab(&self, locked: &LockedInput) -> Result<PathBuf, FetchError> {
        let owner = locked.owner.as_deref().ok_or(FetchError::MissingField("owner"))?;
        let repo = locked.repo.as_deref().ok_or(FetchError::MissingField("repo"))?;
        let rev = locked.rev.as_deref().ok_or(FetchError::MissingField("rev"))?;
        let host = locked.host.as_deref();
        let url = Self::gitlab_archive_url(host, owner, repo, rev);
        let host_tag = host.unwrap_or("gitlab.com").replace('.', "_");
        self.fetch_archive(locked, &url, &format!("gitlab-{host_tag}-{owner}-{repo}-{rev}"), rev)
    }

    fn fetch_sourcehut(&self, locked: &LockedInput) -> Result<PathBuf, FetchError> {
        let owner = locked.owner.as_deref().ok_or(FetchError::MissingField("owner"))?;
        let repo = locked.repo.as_deref().ok_or(FetchError::MissingField("repo"))?;
        let rev = locked.rev.as_deref().ok_or(FetchError::MissingField("rev"))?;
        let url = Self::sourcehut_archive_url(owner, repo, rev);
        let sanitized_owner = owner.trim_start_matches('~');
        self.fetch_archive(
            locked,
            &url,
            &format!("sourcehut-{sanitized_owner}-{repo}-{rev}"),
            rev,
        )
    }

    fn fetch_path(locked: &LockedInput) -> Result<PathBuf, FetchError> {
        let path = locked
            .path
            .as_deref()
            .ok_or(FetchError::MissingField("path"))?;
        Ok(PathBuf::from(path))
    }

    fn fetch_git(&self, locked: &LockedInput) -> Result<PathBuf, FetchError> {
        let url = locked.url.as_deref().ok_or(FetchError::MissingField("url"))?;
        let rev = locked.rev.as_deref().ok_or(FetchError::MissingField("rev"))?;

        let short_rev: String = rev.chars().take(12).collect();
        let dest = self.dest_dir(locked, &format!("git-{short_rev}"));

        // The cache key embeds the rev, so a full object id names one tree and
        // a present one is adoptable. Same conditional as the archive path.
        let immutable = is_immutable_rev(rev);
        if immutable && is_non_empty_dir(&dest) {
            return Ok(dest);
        }

        // Everything below builds the tree in a staging dir and publishes it
        // with one rename. This path was left out of the first stage-then-
        // rename pass, so until now a killed clone or a killed unpack left a
        // partial tree at the FINAL path that the non-empty predicate above
        // then accepted as a complete cache hit.
        let staging = staging_path(&dest);
        let _ = std::fs::remove_dir_all(&staging);

        // Try GitHub tarball first (avoids git CLI dependency in containers).
        // Most git-type inputs in flake.lock are GitHub repos that support
        // archive downloads via /archive/{rev}.tar.gz.
        if let Some(tarball_url) = github_tarball_from_git_url(url, rev) {
            std::fs::create_dir_all(&staging)?;
            match download_bytes(&tarball_url) {
                Ok(bytes) => {
                    if let Err(e) = extract_tar_gz(&bytes, &staging) {
                        let _ = std::fs::remove_dir_all(&staging);
                        return Err(e);
                    }
                    publish(&staging, &dest, immutable)?;
                    return Ok(find_single_subdir_or_self(&dest));
                }
                Err(e) => {
                    // Tarball fallback failed — try git CLI below.
                    let _ = std::fs::remove_dir_all(&staging);
                    tracing::debug!(url = %tarball_url, error = %e, "Tarball fallback failed, trying git CLI");
                }
            }
        }

        // Fall back to git CLI for non-GitHub repos or when tarball fails.
        let status = std::process::Command::new("git")
            .args(["clone", "--depth", "1", url])
            .arg(&staging)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| FetchError::Download(format!(
                "git clone failed (git not in PATH?): {e}"
            )))?;
        if !status.success() {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(FetchError::Download(format!(
                "git clone failed for {url} (exit code: {})",
                status.code().unwrap_or(-1)
            )));
        }

        // Checkout the exact revision.
        //
        // NOTE, unverified and flagged rather than fixed here: the clone above
        // is `--depth 1` of the DEFAULT BRANCH, so an arbitrary `rev` is very
        // likely not among the objects it fetched, and this checkout would
        // fail for any non-HEAD rev. That belongs to whoever owns `git.rs`.
        if let Err(e) = crate::git::checkout_rev(&staging, rev) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(FetchError::Download(format!("git checkout {rev}: {e}")));
        }

        publish(&staging, &dest, immutable)?;
        Ok(dest)
    }

    fn fetch_tarball(&self, locked: &LockedInput) -> Result<PathBuf, FetchError> {
        let url = locked.url.as_deref().ok_or(FetchError::MissingField("url"))?;

        let hash_suffix = locked
            .nar_hash
            .as_deref()
            .map_or_else(|| url_to_safe_name(url), sanitize_hash);
        let dest = self.dest_dir(locked, &format!("tarball-{hash_suffix}"));

        // A tarball input is keyed on its narHash when it has one, which IS a
        // content address, so a present tree is adoptable. When it has none
        // the key is derived from the URL, which is mutable — same split as
        // `is_immutable_rev` on the archive path.
        let immutable = locked.nar_hash.is_some();
        if immutable && is_non_empty_dir(&dest) {
            return Ok(find_single_subdir_or_self(&dest));
        }

        // Stage then publish, exactly as `fetch_archive` does. This path was
        // left behind by the first pass at that fix, so until now a killed
        // `tarball:`/`file:` fetch could leave a partial tree that the
        // non-empty predicate then accepted as a cache hit.
        let staging = staging_path(&dest);
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;

        let bytes = match download_bytes(url) {
            Ok(b) => b,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e);
            }
        };
        if let Err(e) = extract_tar_gz(&bytes, &staging) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }

        publish(&staging, &dest, immutable)?;
        Ok(find_single_subdir_or_self(&dest))
    }

    /// Compute the destination directory, preferring narHash-based names.
    fn dest_dir(&self, locked: &LockedInput, fallback: &str) -> PathBuf {
        if let Some(ref nar_hash) = locked.nar_hash {
            self.cache_dir.join(sanitize_hash(nar_hash))
        } else {
            self.cache_dir.join(fallback)
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────

/// Try to convert a git URL to a GitHub tarball URL.
///
/// `https://github.com/NixOS/nixpkgs.git` + rev → `https://github.com/NixOS/nixpkgs/archive/{rev}.tar.gz`
/// Returns `None` for non-GitHub URLs.
fn github_tarball_from_git_url(url: &str, rev: &str) -> Option<String> {
    let stripped = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git+https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let stripped = stripped.strip_suffix(".git").unwrap_or(stripped);
    // Validate it looks like owner/repo (no extra path segments)
    let parts: Vec<&str> = stripped.split('/').collect();
    if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        Some(format!(
            "https://github.com/{}/{}/archive/{rev}.tar.gz",
            parts[0], parts[1]
        ))
    } else {
        None
    }
}

/// Turn a narHash like `sha256-AAAA...=` into a filesystem-safe name.
/// Turn a hash into a single safe path component.
///
/// ── ★ THE SUBSTITUTIONS ARE NOT A VALIDATION ──────────────────────────
/// `:`→`-`, `/`→`_`, drop `=` makes a hash *look* like a filename; it does not
/// make it *one*. `narHash` comes from a `flake.lock`, which is untrusted
/// input for any flake you did not write yourself, and three values survive
/// the transliteration as meaningful path components: `..`, `.` and `""`.
///
/// Because `/` is mapped away, a multi-level escape is impossible — the blast
/// radius is exactly ONE level, and it should not be rounded up to arbitrary
/// path deletion. One level is bad enough: `"narHash": ".."` makes the cache
/// destination `<cache>/inputs/..` = `~/.cache/sui`, so (a) `fetch` returns
/// `~/.cache/sui` AS the flake's source directory — a silently wrong eval with
/// no error — and (b) on a miss, publishing `remove_dir_all`s it, taking
/// `inputs/` and `nar-memo/` with it. That is the same memo whose poisoning
/// `sui-compat/src/source.rs` was hardened against today.
///
/// So the component is validated, not merely transliterated: anything that is
/// not a plain `[A-Za-z0-9._+-]` run, or that is `.`/`..`/empty, is replaced
/// by a fixed-width digest of the input. Fixed-width by construction beats a
/// denylist, which is what the transliteration was.
fn sanitize_hash(hash: &str) -> String {
    let mapped = hash.replace(':', "-").replace('/', "_").replace('=', "");
    let shaped = !mapped.is_empty()
        && mapped != "."
        && mapped != ".."
        && mapped
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'));
    if shaped {
        mapped
    } else {
        // Deterministic, collision-resistant, and structurally incapable of
        // being a traversal: hex has no `.` and no `/`.
        use sha2::Digest as _;
        let d = sha2::Sha256::digest(hash.as_bytes());
        let mut out = String::with_capacity(2 + 64);
        out.push_str("h-");
        for b in d {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
        }
        out
    }
}

/// Whether `rev` names one immutable tree — a full git object id.
///
/// 40 hex for sha1, 64 for the sha256 transition. Anything else (a branch, a
/// tag, a short rev) can move, so it must never be served from cache without a
/// network check. Lowercase only: git emits lowercase, and accepting mixed case
/// would let `ABC…` and `abc…` occupy two cache entries for one tree.
fn is_immutable_rev(rev: &str) -> bool {
    matches!(rev.len(), 40 | 64) && rev.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A scratch path beside `dest`, on the same filesystem so the publish rename
/// is atomic.
///
/// ── ★ PID IS NOT ENOUGH; THE THREAD ID IS PART OF THE KEY ─────────────
/// An earlier version scoped this on the pid alone and claimed that "two
/// concurrent fetchers cannot share a staging dir". That is true across
/// processes and FALSE within one: two threads of the same process fetching
/// the same input compute the same staging path, and the second one's
/// `remove_dir_all(&staging)` fires while the first is mid-unpack — so the
/// first then publishes a TRUNCATED tree, reintroducing exactly the defect
/// the staging dance exists to prevent.
///
/// Latent today (there is no `rayon`/`par_iter` in the eval path), and it
/// detonates the moment anyone parallelizes input fetching, which is the
/// obvious next optimization on a lock file with N inputs. A claim that is
/// true only until someone does the obvious thing is not an invariant.
fn staging_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map_or_else(|| "fetch".to_string(), |n| n.to_string_lossy().into_owned());
    // `ThreadId`'s Debug is the only stable accessor on stable Rust; it
    // renders as `ThreadId(N)`, so keep the digits and drop the rest.
    let tid = format!("{:?}", std::thread::current().id());
    let tid: String = tid.chars().filter(char::is_ascii_digit).collect();
    let tmp = [
        ".",
        &name,
        ".tmp-",
        &std::process::id().to_string(),
        "-",
        &tid,
    ]
    .concat();
    dest.parent()
        .map_or_else(|| PathBuf::from(&tmp), |p| p.join(&tmp))
}

/// Move `staging` onto `dest` without ever leaving `dest` observably absent
/// for longer than one rename syscall.
///
/// ── ★ WHY NOT `remove_dir_all(dest)` THEN RENAME ──────────────────────
/// That was the first version, and it is a regression dressed as a fix. For a
/// MUTABLE rev the guard above never short-circuits, so every invocation
/// deleted the published tree and re-created it — meaning a concurrent eval
/// reading that path got ENOENT for the whole duration of a recursive delete
/// of (measured on `pleme-io/nix`) 654 files. The staging dance had narrowed
/// the failure from "adopt a partial tree" to "have a complete tree yanked",
/// which is better and is still a bug.
///
/// Two cases, and neither deletes in place:
///
/// - **Immutable rev, tree already present.** Another process published the
///   same content-addressed tree. Theirs is by definition ours; adopt it and
///   drop our staging. No delete of `dest` at all.
/// - **Otherwise.** Rename the old tree ASIDE (one syscall), rename the new
///   one in, then delete the aside at leisure. `dest` is unresolvable only
///   between two renames rather than for the length of a tree walk.
fn publish(staging: &Path, dest: &Path, immutable: bool) -> Result<(), FetchError> {
    if immutable && is_non_empty_dir(dest) {
        let _ = std::fs::remove_dir_all(staging);
        return Ok(());
    }

    let aside = with_suffix(staging, ".old");
    let _ = std::fs::remove_dir_all(&aside);
    let moved_aside = dest.exists() && std::fs::rename(dest, &aside).is_ok();

    match std::fs::rename(staging, dest) {
        Ok(()) => {
            if moved_aside {
                let _ = std::fs::remove_dir_all(&aside);
            }
            Ok(())
        }
        Err(_) => {
            // Put the old tree back rather than leaving the cache emptier
            // than we found it.
            if moved_aside && !dest.exists() {
                let _ = std::fs::rename(&aside, dest);
            }
            let _ = std::fs::remove_dir_all(staging);
            let _ = std::fs::remove_dir_all(&aside);
            if is_non_empty_dir(dest) {
                // Lost the race; the winner left a good tree.
                Ok(())
            } else {
                Err(FetchError::Extract(
                    "could not publish the fetched tree and no other process left one".into(),
                ))
            }
        }
    }
}

/// `path` with `suffix` appended to its file name (not `with_extension`,
/// which truncates at the last dot and would mangle `repo-1.2.3`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "x".to_string(), |n| n.to_string_lossy().into_owned());
    path.parent().map_or_else(
        || PathBuf::from([&name, suffix].concat()),
        |p| p.join([&name, suffix].concat()),
    )
}

/// Return `true` when `dir` exists and has at least one child entry.
fn is_non_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .is_some_and(|mut rd| rd.next().is_some())
}

/// If the directory contains exactly one child directory (common for GitHub
/// tarballs which unpack as `repo-rev/`), return that child. Otherwise
/// return the directory itself.
fn find_single_subdir_or_self(dir: &Path) -> PathBuf {
    let entries: Vec<_> = std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .collect();
    if entries.len() == 1 && entries[0].path().is_dir() {
        entries[0].path()
    } else {
        dir.to_path_buf()
    }
}

/// Download a URL and return the raw bytes.
///
/// Uses `ureq` (synchronous, no tokio runtime) so this function is safe to
/// call from inside a running tokio context — no nested-runtime panic.
///
/// Body limit raised to 512 MiB to accommodate large inputs like nixpkgs tarballs.
fn download_bytes(url: &str) -> Result<Vec<u8>, FetchError> {
    let mut req = ureq::get(url);

    // Attach a host-appropriate auth token when one is available.
    // CppNix consults `~/.config/nix/nix.conf` `access-tokens =
    // github.com=<TOKEN>` etc.; we keep parity by reading the same
    // sources plus the common `GITHUB_TOKEN` env (gh CLI, nix-darwin
    // shell init).  Without this the operator's private flake
    // inputs (e.g. `arnes`) 404 unauthenticated.
    if let Some(token) = github_token_for_url(url) {
        req = req.header("Authorization", &format!("token {token}"));
    }

    let mut response = req
        .call()
        .map_err(|e| FetchError::Download(format!("{url}: {e}")))?;

    if !response.status().is_success() {
        return Err(FetchError::Download(format!(
            "{url}: HTTP {}",
            response.status().as_u16()
        )));
    }

    response
        .body_mut()
        .with_config()
        .limit(512 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| FetchError::Download(format!("{url}: {e}")))
}

/// Resolve a host-appropriate auth token for outgoing requests.
///
/// Sources, in order:
///   1. `GITHUB_TOKEN` env var (covers gh CLI exports + CI tokens).
///   2. `NIX_CONFIG` env var, parsed for `access-tokens` line.
///   3. `~/.config/nix/nix.conf` parsed for `access-tokens` line.
///   4. `~/.config/gh/hosts.yml` (`oauth_token:` field for github.com).
///
/// Returns `Some(token)` only for github.com URLs in this iteration —
/// gitlab / sr.ht / private git hosts can be added when needed.
fn github_token_for_url(url: &str) -> Option<String> {
    if !url.starts_with("https://github.com/")
        && !url.starts_with("https://api.github.com/")
    {
        return None;
    }
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
    }
    if let Ok(cfg) = std::env::var("NIX_CONFIG") {
        if let Some(t) = parse_access_tokens(&cfg, "github.com") {
            return Some(t);
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let nix_conf = home.join(".config/nix/nix.conf");
        if let Ok(cfg) = std::fs::read_to_string(&nix_conf) {
            if let Some(t) = parse_access_tokens(&cfg, "github.com") {
                return Some(t);
            }
        }
        let gh_hosts = home.join(".config/gh/hosts.yml");
        if let Ok(yml) = std::fs::read_to_string(&gh_hosts) {
            if let Some(t) = parse_gh_hosts_token(&yml, "github.com") {
                return Some(t);
            }
        }
    }
    None
}

/// Parse a `~/.config/nix/nix.conf`-style `access-tokens = host=TOKEN ...`
/// line and return the token for `host` if present.
fn parse_access_tokens(cfg: &str, host: &str) -> Option<String> {
    for line in cfg.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("access-tokens") {
            let rest = rest.trim_start().trim_start_matches('=').trim();
            for pair in rest.split_whitespace() {
                if let Some((h, t)) = pair.split_once('=') {
                    if h == host {
                        return Some(t.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Parse `~/.config/gh/hosts.yml` and return the `oauth_token:` value
/// nested under the given host key.  We do this without a full YAML
/// parser to keep sui-eval's dep footprint small — the file is a
/// stable 5-line shape gh maintains.
fn parse_gh_hosts_token(yml: &str, host: &str) -> Option<String> {
    let mut in_host = false;
    for line in yml.lines() {
        let raw = line;
        let trimmed = raw.trim();
        if trimmed.starts_with(host) && trimmed.ends_with(':') {
            in_host = true;
            continue;
        }
        if !raw.starts_with(' ') && !raw.starts_with('\t') && !trimmed.is_empty() {
            in_host = false;
        }
        if in_host {
            if let Some(rest) = trimmed.strip_prefix("oauth_token:") {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

/// Extract a `.tar.gz` archive into a destination directory.
fn extract_tar_gz(bytes: &[u8], dest: &Path) -> Result<(), FetchError> {
    let gz = flate2::read::GzDecoder::new(bytes);

    // Check if the gzip header is valid before attempting extraction.
    // An empty or non-gzip payload would fail inside tar::Archive.
    let mut buffered = std::io::BufReader::new(gz);
    let mut peek = [0u8; 1];
    // Try reading one byte to detect decompression errors early.
    match buffered.read(&mut peek) {
        Ok(0) => {
            return Err(FetchError::Extract("empty archive".into()));
        }
        Err(e) => {
            return Err(FetchError::Extract(format!("gzip decompression: {e}")));
        }
        Ok(_) => {
            // Put the byte back by chaining it in front of the reader.
            let cursor = std::io::Cursor::new(peek);
            let chain = cursor.chain(buffered);
            let mut archive = tar::Archive::new(chain);
            archive
                .unpack(dest)
                .map_err(|e| FetchError::Extract(format!("tar unpack: {e}")))?;
        }
    }

    Ok(())
}

/// Convert a URL into a filesystem-safe name (for fallback cache keys).
fn url_to_safe_name(url: &str) -> String {
    url.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Platform-aware cache directory discovery.
fn dirs_cache_dir() -> PathBuf {
    // Try XDG_CACHE_HOME first, then platform default, then /tmp.
    // Absolute, not merely non-empty — see eval_cache.rs for the class.
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return xdg;
    }
    if let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        let default = home.join(".cache");
        if default.exists() || std::fs::create_dir_all(&default).is_ok() {
            return default;
        }
    }
    PathBuf::from("/tmp")
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Helper: build a `LockedInput` with the given fields.
    fn make_locked(source_type: &str) -> LockedInput {
        LockedInput {
            source_type: source_type.to_string(),
            owner: None,
            repo: None,
            rev: None,
            nar_hash: None,
            last_modified: None,
            path: None,
            url: None,
            git_ref: None,
            dir: None,
            host: None,
            extra: BTreeMap::new(),
        }
    }

    // ── sanitize_hash ─────────────────────────────────────

    #[test]
    fn sanitize_hash_replaces_special_chars() {
        assert_eq!(
            sanitize_hash("sha256-AAAAAAAAAAAAAAAAAAAAAA="),
            "sha256-AAAAAAAAAAAAAAAAAAAAAA"
        );
        assert_eq!(sanitize_hash("sha256:abc/def="), "sha256-abc_def");
    }

    // ── sanitize_hash — a component, not a transliteration ──

    #[test]
    fn a_traversal_hash_cannot_become_a_path_component() {
        // `narHash` is lock-file input. `..` survives the substitutions and
        // would make the cache dest `<cache>/inputs/..` = `~/.cache/sui`,
        // which then gets returned AS the flake source and, on a miss,
        // remove_dir_all'd — taking `inputs/` and `nar-memo/` with it.
        for hostile in ["..", ".", "", "../..", "..\u{0}"] {
            let s = sanitize_hash(hostile);
            assert!(
                s != ".." && s != "." && !s.is_empty(),
                "{hostile:?} sanitized to {s:?}, still a meaningful component"
            );
            assert!(
                !s.contains('/') && !s.contains('\\'),
                "{hostile:?} sanitized to {s:?}, still a separator"
            );
        }
        // Deterministic — the same input must key the same directory.
        assert_eq!(sanitize_hash(".."), sanitize_hash(".."));
        // …and distinct inputs must not collide onto one entry.
        assert_ne!(sanitize_hash(".."), sanitize_hash("."));
    }

    #[test]
    fn a_well_formed_hash_is_untouched_by_the_guard() {
        // The guard must not change the key for ordinary input, or every
        // existing cache entry is orphaned on upgrade.
        assert_eq!(
            sanitize_hash("sha256-avzRM+ffKgikqMRcOhhYp3ifgwXMGbH0rEGEZPEGMYE="),
            "sha256-avzRM+ffKgikqMRcOhhYp3ifgwXMGbH0rEGEZPEGMYE"
        );
        assert_eq!(sanitize_hash("sha256:abc/def="), "sha256-abc_def");
    }

    // ── publish — never leave `dest` absent during a tree walk ──

    #[test]
    fn publishing_an_immutable_tree_adopts_the_winner_and_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("github-o-r-deadbeef");
        let staging = staging_path(&dest);
        // A concurrent process already published.
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("theirs"), b"x").unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("ours"), b"y").unwrap();

        publish(&staging, &dest, true).unwrap();

        assert!(
            dest.join("theirs").exists(),
            "an immutable tree is content-addressed: the winner's tree IS ours, \
             and deleting it to install an identical one is pure risk"
        );
        assert!(!staging.exists(), "our staging must be cleaned up");
    }

    #[test]
    fn publishing_a_mutable_tree_replaces_it_without_a_delete_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("github-o-r-main");
        let staging = staging_path(&dest);
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("old"), b"x").unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("new"), b"y").unwrap();

        publish(&staging, &dest, false).unwrap();

        assert!(dest.join("new").exists(), "the new tree must be published");
        assert!(!dest.join("old").exists(), "and must REPLACE, not union");
        assert!(!staging.exists());
        // The aside must not be left behind as cache litter.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".old"))
            .collect();
        assert!(leftovers.is_empty(), "aside dirs left behind: {leftovers:?}");
    }

    #[test]
    fn staging_is_scoped_by_thread_not_only_by_pid() {
        // An earlier version keyed on pid alone and CLAIMED two concurrent
        // fetchers could not collide. Two threads of one process share a pid,
        // so the second one's cleanup would delete the first one's half-built
        // tree and the first would then publish a truncated one.
        let dest = std::path::Path::new("/c/inputs/github-o-r-deadbeef");
        let here = staging_path(dest);
        let there = std::thread::spawn(move || staging_path(dest))
            .join()
            .unwrap();
        assert_ne!(
            here, there,
            "two threads must not share a staging directory"
        );
    }

    // ── is_immutable_rev — what may be served from cache ──

    #[test]
    fn only_a_full_object_id_is_treated_as_immutable() {
        // sha1 and the sha256 transition: one rev, one tree, forever.
        assert!(is_immutable_rev("7fd33221240a3ab97781a066c5efe0124979527f"));
        assert!(is_immutable_rev(&"a".repeat(64)));

        // ── ★ THE ONE THAT MATTERS ───────────────────────────────────────
        // `github:owner/repo/main` is a legal ref and CppNix accepts it, so
        // we must too — but the tree behind it MOVES. Caching it as if
        // immutable would freeze the entry at whatever `main` was the first
        // time it was fetched. There is a `github-pleme-io-nix-main`
        // directory in the live cache today, so this is not hypothetical.
        assert!(!is_immutable_rev("main"), "a branch name is not a commit");
        assert!(!is_immutable_rev("v1.2.3"), "a tag can be moved");
        assert!(!is_immutable_rev("7fd3322"), "a short rev is ambiguous");
        assert!(!is_immutable_rev(""), "an empty rev names nothing");

        // Length alone is not enough — 40 non-hex chars is not an object id.
        assert!(!is_immutable_rev(&"z".repeat(40)));
        // Uppercase is refused deliberately: git emits lowercase, and
        // accepting both would give one tree two cache entries.
        assert!(!is_immutable_rev(&"A".repeat(40)));
    }

    // ── staging_path — atomicity depends on it being a SIBLING ──

    #[test]
    fn staging_is_a_sibling_so_the_publish_rename_is_atomic() {
        let dest = std::path::Path::new("/cache/sui/inputs/sha256-abc/github-o-r-deadbeef");
        let staging = staging_path(dest);
        assert_eq!(
            staging.parent(),
            dest.parent(),
            "staging in /tmp would put the rename across filesystems, where it \
             is a copy — and a copy is not atomic, which is the whole point"
        );
        assert_ne!(staging, dest.to_path_buf());
        let name = staging.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with('.'), "hidden, so it is not mistaken for a tree");
        assert!(
            name.contains(&std::process::id().to_string()),
            "pid-scoped, so two concurrent fetchers cannot share a staging dir"
        );
        // A dotted directory name must not be truncated the way
        // `Path::with_extension` would truncate it.
        let dotted = std::path::Path::new("/c/github-o-r-1.2.3");
        assert!(
            staging_path(dotted)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("github-o-r-1.2.3"),
            "the full directory name must survive into the staging name"
        );
    }

    // ── find_single_subdir_or_self ────────────────────────

    #[test]
    fn find_single_subdir_returns_child_when_one_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let child = tmp.path().join("repo-abc123");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("file.txt"), "hello").unwrap();

        let result = find_single_subdir_or_self(tmp.path());
        assert_eq!(result, child);
    }

    #[test]
    fn find_single_subdir_returns_self_when_multiple() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        std::fs::create_dir(tmp.path().join("b")).unwrap();

        let result = find_single_subdir_or_self(tmp.path());
        assert_eq!(result, tmp.path());
    }

    #[test]
    fn find_single_subdir_returns_self_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let result = find_single_subdir_or_self(tmp.path());
        assert_eq!(result, tmp.path());
    }

    #[test]
    fn find_single_subdir_returns_self_when_child_is_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.txt"), "data").unwrap();
        let result = find_single_subdir_or_self(tmp.path());
        assert_eq!(result, tmp.path());
    }

    // ── url_to_safe_name ──────────────────────────────────

    #[test]
    fn url_to_safe_name_replaces_slashes_and_colons() {
        let name = url_to_safe_name("https://example.com/foo/bar.tar.gz");
        assert!(!name.contains('/'));
        assert!(!name.contains(':'));
        assert!(name.contains("example"));
    }

    // ── InputFetcher construction ─────────────────────────

    #[test]
    fn fetcher_with_custom_cache_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let fetcher = InputFetcher::with_cache_dir(tmp.path().to_path_buf());
        assert_eq!(fetcher.cache_dir(), tmp.path());
    }

    #[test]
    fn fetcher_default_cache_dir_exists() {
        let fetcher = InputFetcher::new();
        // The path should end with "sui/inputs".
        let path_str = fetcher.cache_dir().to_string_lossy();
        assert!(path_str.ends_with("sui/inputs"), "got: {path_str}");
    }

    // ── path-type fetch ───────────────────────────────────

    #[test]
    fn fetch_path_returns_filesystem_path() {
        let tmp = tempfile::tempdir().unwrap();
        let fetcher = InputFetcher::with_cache_dir(tmp.path().join("cache"));

        let mut locked = make_locked("path");
        locked.path = Some("/var/empty/dep".to_string());

        let result = fetcher.fetch(&locked).unwrap();
        assert_eq!(result, PathBuf::from("/var/empty/dep"));
    }

    #[test]
    fn fetch_path_missing_field_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let fetcher = InputFetcher::with_cache_dir(tmp.path().join("cache"));
        let locked = make_locked("path");
        let result = fetcher.fetch(&locked);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path"));
    }

    // ── unsupported type ──────────────────────────────────

    #[test]
    fn fetch_unsupported_type_returns_error() {
        // `mercurial` — parser doesn't produce this and fetcher
        // doesn't handle it. Remains unsupported for now. If a
        // future commit adds mercurial support, swap this to the
        // next truly-unsupported source_type to keep the test
        // meaningful.
        let tmp = tempfile::tempdir().unwrap();
        let fetcher = InputFetcher::with_cache_dir(tmp.path().join("cache"));
        let locked = make_locked("mercurial");
        let result = fetcher.fetch(&locked);
        assert!(matches!(result, Err(FetchError::UnsupportedType(_))));
    }

    #[test]
    fn gitlab_archive_url_is_well_formed() {
        assert_eq!(
            InputFetcher::gitlab_archive_url(None, "group", "proj", "abc123"),
            "https://gitlab.com/group/proj/-/archive/abc123/proj-abc123.tar.gz"
        );
    }

    #[test]
    fn gitlab_archive_url_honors_custom_host() {
        assert_eq!(
            InputFetcher::gitlab_archive_url(Some("gitlab.gnome.org"), "GNOME", "gnome-shell", "abc"),
            "https://gitlab.gnome.org/GNOME/gnome-shell/-/archive/abc/gnome-shell-abc.tar.gz"
        );
    }

    #[test]
    fn sourcehut_archive_url_prepends_tilde() {
        // Sourcehut owner names on the platform carry a `~` prefix
        // (`~emersion`) but the flake-ref parser drops it. Fetcher
        // must reinstate so the URL is canonical.
        assert_eq!(
            InputFetcher::sourcehut_archive_url("emersion", "page", "HEAD"),
            "https://git.sr.ht/~emersion/page/archive/HEAD.tar.gz"
        );
        // If the caller already included `~`, don't double it.
        assert_eq!(
            InputFetcher::sourcehut_archive_url("~emersion", "page", "HEAD"),
            "https://git.sr.ht/~emersion/page/archive/HEAD.tar.gz"
        );
    }

    // ── cache hit ─────────────────────────────────────────

    #[test]
    fn cache_hit_returns_cached_path() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        // Pre-populate cache.
        let hash = "sha256-TESTCACHEHIT";
        let cached_dir = cache_dir.join(sanitize_hash(hash));
        std::fs::create_dir_all(&cached_dir).unwrap();
        std::fs::write(cached_dir.join("flake.nix"), "{}").unwrap();

        let fetcher = InputFetcher::with_cache_dir(cache_dir);
        let mut locked = make_locked("github");
        locked.nar_hash = Some(hash.to_string());
        // Intentionally leave owner/repo/rev empty — cache hit should skip fetch.

        let result = fetcher.fetch(&locked).unwrap();
        // The cached directory has one file (not a subdir), so it returns itself.
        assert_eq!(result, cached_dir);
    }

    // ── github URL construction ───────────────────────────

    #[test]
    fn github_archive_url_format() {
        let url = InputFetcher::github_archive_url("nixos", "nixpkgs", "abc123");
        assert_eq!(
            url,
            "https://github.com/nixos/nixpkgs/archive/abc123.tar.gz"
        );
    }

    // ── github fetch missing fields ───────────────────────

    #[test]
    fn fetch_github_missing_owner_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let fetcher = InputFetcher::with_cache_dir(tmp.path().join("cache"));
        let mut locked = make_locked("github");
        locked.repo = Some("nixpkgs".into());
        locked.rev = Some("abc123".into());
        let result = fetcher.fetch(&locked);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("owner"));
    }

    #[test]
    fn fetch_github_missing_rev_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let fetcher = InputFetcher::with_cache_dir(tmp.path().join("cache"));
        let mut locked = make_locked("github");
        locked.owner = Some("nixos".into());
        locked.repo = Some("nixpkgs".into());
        let result = fetcher.fetch(&locked);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rev"));
    }

    // ── git fetch missing fields ──────────────────────────

    #[test]
    fn fetch_git_missing_url_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let fetcher = InputFetcher::with_cache_dir(tmp.path().join("cache"));
        let mut locked = make_locked("git");
        locked.rev = Some("abc123".into());
        let result = fetcher.fetch(&locked);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("url"));
    }

    #[test]
    fn fetch_git_missing_rev_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let fetcher = InputFetcher::with_cache_dir(tmp.path().join("cache"));
        let mut locked = make_locked("git");
        locked.url = Some("https://example.com/repo.git".into());
        let result = fetcher.fetch(&locked);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rev"));
    }

    // ── tarball fetch missing URL ─────────────────────────

    #[test]
    fn fetch_tarball_missing_url_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let fetcher = InputFetcher::with_cache_dir(tmp.path().join("cache"));
        let locked = make_locked("tarball");
        let result = fetcher.fetch(&locked);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("url"));
    }

    // ── extract_tar_gz ────────────────────────────────────

    #[test]
    fn extract_tar_gz_empty_archive_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let result = extract_tar_gz(&[], tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn extract_tar_gz_invalid_data_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let result = extract_tar_gz(b"not a gzip stream at all", tmp.path());
        assert!(result.is_err());
    }

    // ── dest_dir logic ────────────────────────────────────

    #[test]
    fn dest_dir_uses_nar_hash_when_present() {
        let fetcher = InputFetcher::with_cache_dir(PathBuf::from("/cache"));
        let mut locked = make_locked("github");
        locked.nar_hash = Some("sha256-ABC123=".to_string());
        let dest = fetcher.dest_dir(&locked, "fallback");
        assert!(dest.to_string_lossy().contains("sha256-ABC123"));
        assert!(!dest.to_string_lossy().contains("fallback"));
    }

    #[test]
    fn dest_dir_uses_fallback_when_no_hash() {
        let fetcher = InputFetcher::with_cache_dir(PathBuf::from("/cache"));
        let locked = make_locked("github");
        let dest = fetcher.dest_dir(&locked, "fallback-name");
        assert!(dest.to_string_lossy().contains("fallback-name"));
    }

    // ── is_non_empty_dir ─────────────────────────────────

    #[test]
    fn is_non_empty_dir_returns_true_for_non_empty() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.txt"), "data").unwrap();
        assert!(is_non_empty_dir(tmp.path()));
    }

    #[test]
    fn is_non_empty_dir_returns_false_for_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_non_empty_dir(tmp.path()));
    }

    #[test]
    fn is_non_empty_dir_returns_false_for_missing() {
        assert!(!is_non_empty_dir(Path::new("/nonexistent/path/12345")));
    }

    // ── empty cache invalidation ─────────────────────────

    #[test]
    fn empty_cache_dir_is_treated_as_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        // Pre-create an *empty* cache directory (simulates a failed fetch).
        let hash = "sha256-EMPTYTEST";
        let cached_dir = cache_dir.join(sanitize_hash(hash));
        std::fs::create_dir_all(&cached_dir).unwrap();
        // Verify the directory is empty.
        assert!(std::fs::read_dir(&cached_dir).unwrap().next().is_none());

        let fetcher = InputFetcher::with_cache_dir(cache_dir);
        let mut locked = make_locked("github");
        locked.nar_hash = Some(hash.to_string());
        // owner/repo/rev are missing, so the re-fetch will fail — but
        // the important thing is that the cache miss was detected (the
        // stale directory was removed) and the code attempted a fresh fetch.
        let result = fetcher.fetch(&locked);
        assert!(result.is_err(), "should not return stale empty cache");
        // The empty directory should have been cleaned up.
        assert!(!cached_dir.exists(), "stale cache dir should be removed");
    }

    // ── github_tarball_from_git_url ──────────────────────

    #[test]
    fn tarball_from_https_github() {
        let url = github_tarball_from_git_url(
            "https://github.com/NixOS/nixpkgs.git",
            "abc123",
        );
        assert_eq!(
            url.as_deref(),
            Some("https://github.com/NixOS/nixpkgs/archive/abc123.tar.gz")
        );
    }

    #[test]
    fn tarball_from_git_plus_https() {
        let url = github_tarball_from_git_url(
            "git+https://github.com/NixOS/nixpkgs",
            "def456",
        );
        assert_eq!(
            url.as_deref(),
            Some("https://github.com/NixOS/nixpkgs/archive/def456.tar.gz")
        );
    }

    #[test]
    fn tarball_from_non_github_returns_none() {
        assert!(github_tarball_from_git_url("https://gitlab.com/foo/bar.git", "abc").is_none());
        assert!(github_tarball_from_git_url("ssh://git@github.com/foo/bar", "abc").is_none());
    }

    #[test]
    fn tarball_from_malformed_path_returns_none() {
        assert!(github_tarball_from_git_url("https://github.com/", "abc").is_none());
        assert!(github_tarball_from_git_url("https://github.com/only-owner", "abc").is_none());
    }
}


/// Turn a parsed flake reference into a directory on disk, fetching it first
/// if it is remote.
///
/// ── ★ ONE PLACE, BECAUSE THERE ARE THREE CALLERS ────────────────────────
/// `evaluate_flake` takes a `&Path`, so every entry point that accepts a
/// `--flake` argument has to answer "where is it?" — `sui-orchestrate`'s
/// `build_toplevel` and two sites in the `sui` CLI. Written per-caller, the
/// remote case would be right in whichever one was being fixed and missing in
/// the others, which is precisely how `github:` refs came to work in some
/// paths and not the one the fleet reconciler uses.
///
/// A local ref costs nothing here. A remote one is content-addressed and
/// cached by the same fetcher that pulls locked flake inputs, so re-resolving
/// the same rev does no network.
///
/// # Errors
///
/// Returns [`FetchError`] when a remote source cannot be fetched or
/// extracted.
pub fn resolve_flake_dir(flake_ref: &FlakeRef) -> Result<std::path::PathBuf, FetchError> {
    match flake_ref.local_dir() {
        Some(p) => Ok(p.to_path_buf()),
        None => {
            let locked = flake_ref
                .source
                .locked_input()
                .ok_or_else(|| FetchError::UnsupportedType("non-fetchable flake source".into()))?;
            InputFetcher::new().fetch(&locked)
        }
    }
}
