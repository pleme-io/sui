//! `nix.conf` — reading nix's configuration the way CppNix does.
//!
//! Clean-room, from CppNix's documented behaviour (`nix.conf(5)`) and checked
//! against `nix config show` in `sui-eval/tests/nix_conf_oracle.rs`.
//!
//! # Sources, lowest precedence first
//!
//! 1. `$NIX_CONF_DIR/nix.conf` (default `/etc/nix/nix.conf`).
//! 2. The user files: `$NIX_USER_CONF_FILES` (colon-separated) when set,
//!    otherwise `<dir>/nix/nix.conf` for `$XDG_CONFIG_HOME` (default
//!    `~/.config`) followed by each of `$XDG_CONFIG_DIRS` (default `/etc/xdg`).
//!    They are applied LAST TO FIRST, so the first-listed file wins.
//! 3. `$NIX_CONFIG`, applied as the contents of one more file.
//!
//! A file that is missing or cannot be read is skipped, as CppNix skips it. sui
//! records the unreadable ones ([`NixConfig::unreadable`]) so a caller can say
//! so instead of failing mysteriously later.
//!
//! # Line syntax
//!
//! Everything from the first `#` on a line is a comment. The rest is split on
//! whitespace. `include <path>` and `!include <path>` splice in another file,
//! resolved relative to the including file's directory; a missing `include` is
//! an error and a missing `!include` is not. Every other line is
//! `<name> = <value…>`, the value being the remaining words joined by one space.
//! Any other shape is an error that names the line and the file.
//!
//! # `access-tokens`
//!
//! A map of `host-or-prefix=token` words. Setting `access-tokens` replaces the
//! map; `extra-access-tokens` adds entries whose keys are not present yet
//! (CppNix inserts without overwriting). Within one value, the first entry for
//! a key wins. Token values never appear in `Debug` output or error messages.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One `name = value` assignment, in the order CppNix applies it.
#[derive(Clone, PartialEq, Eq)]
pub struct Assignment {
    pub name: String,
    pub value: String,
    /// The file it came from, or `NIX_CONFIG`.
    pub origin: String,
}

impl std::fmt::Debug for Assignment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Values can be secrets; name and origin are enough to locate one.
        f.debug_struct("Assignment")
            .field("name", &self.name)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

/// Why a configuration could not be read. Never carries a setting's value.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NixConfError {
    /// A line that is neither an include nor `name = value`.
    #[error("illegal configuration line {line_number} in '{origin}'")]
    IllegalLine { origin: String, line_number: usize },
    /// `include <path>` names a file that does not exist.
    #[error("file '{path}' included from '{origin}' not found")]
    IncludeNotFound { path: PathBuf, origin: String },
}

/// Where the configuration comes from. [`ConfigSources::from_env`] is CppNix's
/// resolution; tests build one directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSources {
    pub system: PathBuf,
    /// Highest precedence first, as CppNix lists them.
    pub user: Vec<PathBuf>,
    pub nix_config: Option<String>,
}

impl ConfigSources {
    /// Resolve the sources from the environment, the way CppNix does.
    #[must_use]
    pub fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).ok();
        let conf_dir = env("NIX_CONF_DIR").unwrap_or_else(|| "/etc/nix".to_string());
        let user = match env("NIX_USER_CONF_FILES") {
            Some(files) => split_colon(&files).map(PathBuf::from).collect(),
            None => {
                let config_home = env("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| {
                    env("HOME").map(|h| PathBuf::from(h).join(".config"))
                });
                let dirs = env("XDG_CONFIG_DIRS").unwrap_or_else(|| "/etc/xdg".to_string());
                config_home
                    .into_iter()
                    .chain(split_colon(&dirs).map(PathBuf::from))
                    .map(|d| d.join("nix/nix.conf"))
                    .collect()
            }
        };
        Self {
            system: PathBuf::from(conf_dir).join("nix.conf"),
            user,
            nix_config: env("NIX_CONFIG"),
        }
    }
}

/// CppNix's `tokenizeString(s, ":")`: empty pieces are dropped.
fn split_colon(s: &str) -> impl Iterator<Item = &str> {
    s.split(':').filter(|p| !p.is_empty())
}

/// The resolved configuration: every assignment in application order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NixConfig {
    assignments: Vec<Assignment>,
    unreadable: Vec<(PathBuf, std::io::ErrorKind)>,
}

impl NixConfig {
    /// Read the configuration CppNix would read in this environment.
    ///
    /// # Errors
    /// A malformed line or a missing `include`, as CppNix refuses both.
    pub fn load() -> Result<Self, NixConfError> {
        Self::load_from(&ConfigSources::from_env())
    }

    /// Read the configuration from explicit sources.
    ///
    /// # Errors
    /// A malformed line or a missing `include`.
    pub fn load_from(sources: &ConfigSources) -> Result<Self, NixConfError> {
        let mut cfg = Self::default();
        cfg.apply_file(&sources.system)?;
        for file in sources.user.iter().rev() {
            cfg.apply_file(file)?;
        }
        if let Some(text) = &sources.nix_config {
            cfg.apply_text(text, "NIX_CONFIG", Path::new("."))?;
        }
        Ok(cfg)
    }

    fn apply_file(&mut self, path: &Path) -> Result<(), NixConfError> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let base = path.parent().unwrap_or(Path::new("."));
                self.apply_text(&text, &path.display().to_string(), base)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                self.unreadable.push((path.to_path_buf(), e.kind()));
                Ok(())
            }
        }
    }

    fn apply_text(&mut self, text: &str, origin: &str, base: &Path) -> Result<(), NixConfError> {
        for (i, raw) in text.split('\n').enumerate() {
            let line = raw.split('#').next().unwrap_or("");
            let tokens: Vec<&str> = line
                .split([' ', '\t', '\r'])
                .filter(|t| !t.is_empty())
                .collect();
            if tokens.is_empty() {
                continue;
            }
            let illegal = || NixConfError::IllegalLine {
                origin: origin.to_string(),
                line_number: i + 1,
            };
            if tokens.len() < 2 {
                return Err(illegal());
            }
            if tokens[0] == "include" || tokens[0] == "!include" {
                if tokens.len() != 2 {
                    return Err(illegal());
                }
                let path = base.join(tokens[1]);
                if path.exists() {
                    self.apply_file(&path)?;
                } else if tokens[0] == "include" {
                    return Err(NixConfError::IncludeNotFound {
                        path,
                        origin: origin.to_string(),
                    });
                }
                continue;
            }
            if tokens[1] != "=" {
                return Err(illegal());
            }
            self.assignments.push(Assignment {
                name: tokens[0].to_string(),
                value: tokens[2..].join(" "),
                origin: origin.to_string(),
            });
        }
        Ok(())
    }

    /// Every assignment, in the order it was applied.
    #[must_use]
    pub fn assignments(&self) -> &[Assignment] {
        &self.assignments
    }

    /// The value of scalar setting `name`: its last assignment, as CppNix's
    /// `set` replaces. `None` when never assigned.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.assignments.iter().rev().find(|a| a.name == name).map(|a| a.value.as_str())
    }

    /// Files that exist but could not be read (skipped, as CppNix skips them).
    #[must_use]
    pub fn unreadable(&self) -> &[(PathBuf, std::io::ErrorKind)] {
        &self.unreadable
    }

    /// The resolved `access-tokens` map.
    #[must_use]
    pub fn access_tokens(&self) -> AccessTokens {
        let mut map = BTreeMap::new();
        for a in &self.assignments {
            match a.name.as_str() {
                "access-tokens" => map = parse_string_map(&a.value),
                "extra-access-tokens" => {
                    for (k, v) in parse_string_map(&a.value) {
                        map.entry(k).or_insert(v);
                    }
                }
                _ => {}
            }
        }
        AccessTokens(map)
    }
}

/// CppNix's `StringMap` setting parse: whitespace-separated `key=value` words,
/// split at the first `=`; words without one are ignored; the first entry for a
/// key wins.
fn parse_string_map(value: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for word in value.split_whitespace() {
        if let Some((k, v)) = word.split_once('=') {
            map.entry(k.to_string()).or_insert_with(|| v.to_string());
        }
    }
    map
}

/// The resolved `access-tokens` setting. `Debug` shows keys only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct AccessTokens(BTreeMap<String, String>);

impl std::fmt::Debug for AccessTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

impl AccessTokens {
    /// The configured keys (hosts or host/path prefixes), never the tokens.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// CppNix's `getAccessToken(host, url)`: the token whose key is the longest
    /// substring of `url` (for GitHub, `github.com/<owner>/<repo>`), else the
    /// token keyed exactly by `host`.
    #[must_use]
    pub fn for_url(&self, host: &str, url: &str) -> Option<&str> {
        let mut best: Option<(&str, usize)> = None;
        if !url.is_empty() {
            for (key, token) in &self.0 {
                if url.contains(key.as_str()) && key.len() > best.map_or(0, |(_, n)| n) {
                    best = Some((token.as_str(), key.len()));
                }
            }
        }
        match best {
            Some((token, _)) if !token.is_empty() => Some(token),
            _ => self.0.get(host).map(String::as_str),
        }
    }

    /// The resolved map as `key=token` pairs, sorted — the shape
    /// `nix config show access-tokens` prints. For comparison in tests only.
    #[must_use]
    pub fn to_config_value(&self) -> String {
        self.0
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(files: &[(&str, &str)], user: &[&str], nix_config: Option<&str>) -> (tempfile::TempDir, Result<NixConfig, NixConfError>) {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let sources = ConfigSources {
            system: dir.path().join("etc/nix.conf"),
            user: user.iter().map(|u| dir.path().join(u)).collect(),
            nix_config: nix_config.map(str::to_string),
        };
        let cfg = NixConfig::load_from(&sources);
        (dir, cfg)
    }

    #[test]
    fn bang_include_splices_relative_to_the_including_file() {
        let (_d, cfg) = load(
            &[("etc/nix.conf", "!include tokens\n"), ("etc/tokens", "access-tokens = github.com=T1\n")],
            &[],
            None,
        );
        let t = cfg.unwrap().access_tokens();
        assert_eq!(t.for_url("github.com", "github.com/o/r"), Some("T1"));
    }

    #[test]
    fn a_missing_bang_include_is_skipped_and_a_missing_include_is_an_error() {
        let (_d, cfg) = load(&[("etc/nix.conf", "!include nope\nsubstituters = x\n")], &[], None);
        assert_eq!(cfg.unwrap().assignments().len(), 1);
        let (_d, cfg) = load(&[("etc/nix.conf", "include nope\n")], &[], None);
        assert!(matches!(cfg, Err(NixConfError::IncludeNotFound { .. })));
    }

    #[test]
    fn precedence_is_system_then_user_last_to_first_then_nix_config() {
        let (_d, cfg) = load(
            &[
                ("etc/nix.conf", "access-tokens = github.com=SYS\n"),
                ("u1.conf", "access-tokens = github.com=U1\n"),
                ("u2.conf", "access-tokens = github.com=U2\n"),
            ],
            &["u1.conf", "u2.conf"],
            None,
        );
        let t = cfg.unwrap().access_tokens();
        assert_eq!(t.for_url("github.com", ""), Some("U1"), "first user file wins");
        let (_d, cfg) = load(
            &[("etc/nix.conf", "access-tokens = github.com=SYS\n")],
            &[],
            Some("access-tokens = github.com=ENV"),
        );
        assert_eq!(cfg.unwrap().access_tokens().for_url("github.com", ""), Some("ENV"));
    }

    #[test]
    fn extra_adds_without_overwriting_and_set_replaces() {
        let (_d, cfg) = load(
            &[("etc/nix.conf",
               "access-tokens = github.com=A example.com=E\n\
                extra-access-tokens = github.com=B gitlab.com=G\n")],
            &[],
            None,
        );
        assert_eq!(cfg.unwrap().access_tokens().to_config_value(), "example.com=E github.com=A gitlab.com=G");
        let (_d, cfg) = load(
            &[("etc/nix.conf", "access-tokens = github.com=A example.com=E\naccess-tokens = gitlab.com=G\n")],
            &[],
            None,
        );
        assert_eq!(cfg.unwrap().access_tokens().to_config_value(), "gitlab.com=G");
    }

    #[test]
    fn the_longest_matching_prefix_wins_then_the_host() {
        let (_d, cfg) = load(
            &[("etc/nix.conf", "access-tokens = github.com=H github.com/org=ORG github.com/org/repo=REPO\n")],
            &[],
            None,
        );
        let t = cfg.unwrap().access_tokens();
        assert_eq!(t.for_url("github.com", "github.com/org/repo"), Some("REPO"));
        assert_eq!(t.for_url("github.com", "github.com/org/other"), Some("ORG"));
        assert_eq!(t.for_url("github.com", "github.com/else/x"), Some("H"));
        assert_eq!(t.for_url("gitlab.com", "gitlab.com/x/y"), None);
    }

    #[test]
    fn comments_whitespace_and_illegal_lines() {
        let (_d, cfg) = load(
            &[("etc/nix.conf", "  # full comment\n\taccess-tokens\t=  github.com=T # trailing\r\n\n")],
            &[],
            None,
        );
        assert_eq!(cfg.unwrap().access_tokens().for_url("github.com", ""), Some("T"));
        for bad in ["access-tokens=github.com=T\n", "access-tokens =github.com=T\n", "lonely\n", "!include a b\n"] {
            let (_d, cfg) = load(&[("etc/nix.conf", bad)], &[], None);
            assert!(matches!(cfg, Err(NixConfError::IllegalLine { .. })), "{bad:?}");
        }
    }

    #[test]
    fn a_scalar_setting_is_its_last_assignment() {
        let (_d, cfg) = load(&[("etc/nix.conf", "max-jobs = 2\nmax-jobs = auto\n")], &[], None);
        let cfg = cfg.unwrap();
        assert_eq!(cfg.get("max-jobs"), Some("auto"));
        assert_eq!(cfg.get("cores"), None);
    }

    #[test]
    fn debug_and_errors_never_show_a_token() {
        let (_d, cfg) = load(&[("etc/nix.conf", "access-tokens = github.com=SECRET-VALUE\n")], &[], None);
        let cfg = cfg.unwrap();
        let shown = format!("{cfg:?} {:?}", cfg.access_tokens());
        assert!(!shown.contains("SECRET-VALUE"), "{shown}");
        assert!(shown.contains("github.com"));
    }

    #[test]
    fn an_unreadable_file_is_skipped_and_recorded() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let tokens = dir.path().join("tokens");
        std::fs::write(&tokens, "access-tokens = github.com=T\n").unwrap();
        if std::fs::metadata(&tokens).unwrap().uid() == 0 {
            return; // root reads mode-000 files
        }
        std::fs::set_permissions(&tokens, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::write(dir.path().join("nix.conf"), "!include tokens\n").unwrap();
        let cfg = NixConfig::load_from(&ConfigSources {
            system: dir.path().join("nix.conf"),
            user: vec![],
            nix_config: None,
        })
        .unwrap();
        std::fs::set_permissions(&tokens, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(cfg.access_tokens().for_url("github.com", ""), None);
        assert_eq!(cfg.unreadable().len(), 1);
        assert_eq!(cfg.unreadable()[0].1, std::io::ErrorKind::PermissionDenied);
    }
}
