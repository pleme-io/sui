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
                return Ok(find_single_subdir_or_self(&cached));
            }
        }

        match locked.source_type.as_str() {
            "github" => self.fetch_github(locked),
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

    // ── Private fetch methods ─────────────────────────────

    fn fetch_github(&self, locked: &LockedInput) -> Result<PathBuf, FetchError> {
        let owner = locked.owner.as_deref().ok_or(FetchError::MissingField("owner"))?;
        let repo = locked.repo.as_deref().ok_or(FetchError::MissingField("repo"))?;
        let rev = locked.rev.as_deref().ok_or(FetchError::MissingField("rev"))?;

        let url = Self::github_archive_url(owner, repo, rev);
        let dest = self.dest_dir(locked, &format!("github-{owner}-{repo}-{rev}"));
        std::fs::create_dir_all(&dest)?;

        let bytes = download_bytes(&url)?;
        extract_tar_gz(&bytes, &dest)?;

        Ok(find_single_subdir_or_self(&dest))
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
            return Ok(dest);
        }

        let status = std::process::Command::new("git")
            .args(["clone", "--depth", "1", url])
            .arg(dest.as_os_str())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| FetchError::Download(format!("git clone: {e}")))?;

        if !status.success() {
            return Err(FetchError::Download(format!(
                "git clone failed for {url} (exit code: {})",
                status.code().unwrap_or(-1)
            )));
        }

        // Checkout the exact revision.
        let checkout_status = std::process::Command::new("git")
            .args(["-C"])
            .arg(dest.as_os_str())
            .args(["checkout", rev])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| FetchError::Download(format!("git checkout: {e}")))?;

        if !checkout_status.success() {
            return Err(FetchError::Download(format!(
                "git checkout {rev} failed for {url}"
            )));
        }

        Ok(dest)
    }

    fn fetch_tarball(&self, locked: &LockedInput) -> Result<PathBuf, FetchError> {
        let url = locked.url.as_deref().ok_or(FetchError::MissingField("url"))?;

        let hash_suffix = locked
            .nar_hash
            .as_deref()
            .map_or_else(|| url_to_safe_name(url), |h| sanitize_hash(h));
        let dest = self.dest_dir(locked, &format!("tarball-{hash_suffix}"));

        if dest.exists() {
            return Ok(find_single_subdir_or_self(&dest));
        }

        std::fs::create_dir_all(&dest)?;
        let bytes = download_bytes(url)?;
        extract_tar_gz(&bytes, &dest)?;

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

/// Turn a narHash like `sha256-AAAA...=` into a filesystem-safe name.
fn sanitize_hash(hash: &str) -> String {
    hash.replace(':', "-").replace('/', "_").replace('=', "")
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
fn download_bytes(url: &str) -> Result<Vec<u8>, FetchError> {
    let response = reqwest::blocking::get(url)
        .map_err(|e| FetchError::Download(format!("{url}: {e}")))?;

    if !response.status().is_success() {
        return Err(FetchError::Download(format!(
            "{url}: HTTP {}",
            response.status()
        )));
    }

    response
        .bytes()
        .map(|b| b.to_vec())
        .map_err(|e| FetchError::Download(format!("{url}: {e}")))
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
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg);
        }
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
        let tmp = tempfile::tempdir().unwrap();
        let fetcher = InputFetcher::with_cache_dir(tmp.path().join("cache"));
        let locked = make_locked("sourcehut");
        let result = fetcher.fetch(&locked);
        assert!(matches!(result, Err(FetchError::UnsupportedType(_))));
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
}
