//! Content-addressed input fetcher for flake.lock resolved inputs.
//!
//! Fetches locked flake inputs (github tarballs, git repos, local paths,
//! remote tarballs) and caches them by `narHash` so repeated evaluations
//! hit the local filesystem instead of the network.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use sui_compat::flake::LockedInput;

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
        let dest = self.dest_dir(locked, &format!("github-{owner}-{repo}-{rev}"));
        std::fs::create_dir_all(&dest)?;

        // Download and extract; on failure remove the (potentially empty) dest
        // directory so the next attempt doesn't see a stale cache hit.
        let bytes = match download_bytes(&url) {
            Ok(b) => b,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dest);
                return Err(e);
            }
        };
        if let Err(e) = extract_tar_gz(&bytes, &dest) {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(e);
        }

        Ok(find_single_subdir_or_self(&dest))
    }

    /// GitLab and Sourcehut share the same archive-fetch shape as
    /// GitHub — download a tar.gz, extract, return the single top-
    /// level directory. Only the URL construction differs.
    fn fetch_archive(
        &self,
        locked: &LockedInput,
        url: &str,
        cache_key: &str,
    ) -> Result<PathBuf, FetchError> {
        let dest = self.dest_dir(locked, cache_key);
        std::fs::create_dir_all(&dest)?;
        let bytes = match download_bytes(url) {
            Ok(b) => b,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dest);
                return Err(e);
            }
        };
        if let Err(e) = extract_tar_gz(&bytes, &dest) {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(e);
        }
        Ok(find_single_subdir_or_self(&dest))
    }

    fn fetch_gitlab(&self, locked: &LockedInput) -> Result<PathBuf, FetchError> {
        let owner = locked.owner.as_deref().ok_or(FetchError::MissingField("owner"))?;
        let repo = locked.repo.as_deref().ok_or(FetchError::MissingField("repo"))?;
        let rev = locked.rev.as_deref().ok_or(FetchError::MissingField("rev"))?;
        let host = locked.host.as_deref();
        let url = Self::gitlab_archive_url(host, owner, repo, rev);
        let host_tag = host.unwrap_or("gitlab.com").replace('.', "_");
        self.fetch_archive(locked, &url, &format!("gitlab-{host_tag}-{owner}-{repo}-{rev}"))
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

        if dest.exists() {
            if is_non_empty_dir(&dest) {
                return Ok(dest);
            }
            let _ = std::fs::remove_dir_all(&dest);
        }

        // Try GitHub tarball first (avoids git CLI dependency in containers).
        // Most git-type inputs in flake.lock are GitHub repos that support
        // archive downloads via /archive/{rev}.tar.gz.
        if let Some(tarball_url) = github_tarball_from_git_url(url, rev) {
            std::fs::create_dir_all(&dest)?;
            match download_bytes(&tarball_url) {
                Ok(bytes) => {
                    if let Err(e) = extract_tar_gz(&bytes, &dest) {
                        let _ = std::fs::remove_dir_all(&dest);
                        return Err(e);
                    }
                    return Ok(find_single_subdir_or_self(&dest));
                }
                Err(e) => {
                    // Tarball fallback failed — try git CLI below.
                    let _ = std::fs::remove_dir_all(&dest);
                    tracing::debug!(url = %tarball_url, error = %e, "Tarball fallback failed, trying git CLI");
                }
            }
        }

        // Fall back to git CLI for non-GitHub repos or when tarball fails.
        let status = std::process::Command::new("git")
            .args(["clone", "--depth", "1", url])
            .arg(&dest)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| FetchError::Download(format!(
                "git clone failed (git not in PATH?): {e}"
            )))?;
        if !status.success() {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(FetchError::Download(format!(
                "git clone failed for {url} (exit code: {})",
                status.code().unwrap_or(-1)
            )));
        }

        // Checkout the exact revision.
        crate::git::checkout_rev(&dest, rev)
            .map_err(|e| FetchError::Download(format!("git checkout {rev}: {e}")))?;

        Ok(dest)
    }

    fn fetch_tarball(&self, locked: &LockedInput) -> Result<PathBuf, FetchError> {
        let url = locked.url.as_deref().ok_or(FetchError::MissingField("url"))?;

        let hash_suffix = locked
            .nar_hash
            .as_deref()
            .map_or_else(|| url_to_safe_name(url), sanitize_hash);
        let dest = self.dest_dir(locked, &format!("tarball-{hash_suffix}"));

        if dest.exists() {
            let resolved = find_single_subdir_or_self(&dest);
            if is_non_empty_dir(&resolved) {
                return Ok(resolved);
            }
            let _ = std::fs::remove_dir_all(&dest);
        }

        std::fs::create_dir_all(&dest)?;
        let bytes = match download_bytes(url) {
            Ok(b) => b,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dest);
                return Err(e);
            }
        };
        if let Err(e) = extract_tar_gz(&bytes, &dest) {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(e);
        }

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
fn sanitize_hash(hash: &str) -> String {
    hash.replace(':', "-").replace('/', "_").replace('=', "")
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
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.is_empty() {
            return PathBuf::from(xdg);
        }
    if let Some(home) = std::env::var_os("HOME") {
        let default = PathBuf::from(home).join(".cache");
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
