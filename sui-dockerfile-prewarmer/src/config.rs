//! Typed configuration for the pre-warming service.
//!
//! Follows `sui-daemon`'s shikumi discovery/load pattern: a config file
//! (`~/.config/sui-dockerfile-prewarmer/prewarmer.yaml`, or an explicit
//! `--config` path) deserializes into [`RawPrewarmerConfig`], which is
//! then validated into a [`PrewarmerConfig`] — a malformed watched-path
//! entry (missing `owner`/`repo`/`path`/`image_tag`) is a typed
//! [`ConfigError`] at validation time, never a runtime panic.

use std::path::Path;

use serde::{Deserialize, Serialize};
use shikumi::{ConfigDiscovery, ConfigStore};

/// One Dockerfile to watch for new commits, as deserialized from the
/// config file — every field optional at the serde layer so a missing
/// field is a typed [`ConfigError`], not a deserialize failure that
/// hides which field was missing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RawWatchedDockerfile {
    pub owner: String,
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub path: String,
    pub image_tag: String,
}

/// A validated, non-empty-everywhere watched Dockerfile — the type
/// every downstream poll/pre-warm consumer accepts, so a malformed
/// entry cannot flow past [`PrewarmerConfig::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WatchedDockerfile {
    pub owner: String,
    pub repo: String,
    pub git_ref: String,
    pub path: String,
    pub image_tag: String,
}

/// Typed config-validation error — surfaced at parse/validate time,
/// never a runtime panic on first use.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("watched dockerfile entry #{index}: missing required field `{field}`")]
    MissingField { index: usize, field: &'static str },
    #[error("poll_interval_secs must be > 0 (got {0})")]
    ZeroPollInterval(u64),
}

impl RawWatchedDockerfile {
    /// Validate a single raw entry, tagging any failure with its index
    /// in the `watched` list so an operator can locate the bad entry.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::MissingField`] if `owner`, `repo`, `path`,
    /// or `image_tag` is empty. `git_ref` defaults to `"main"` when
    /// empty rather than erroring — most repos' default branch.
    pub fn validate(&self, index: usize) -> Result<WatchedDockerfile, ConfigError> {
        if self.owner.is_empty() {
            return Err(ConfigError::MissingField { index, field: "owner" });
        }
        if self.repo.is_empty() {
            return Err(ConfigError::MissingField { index, field: "repo" });
        }
        if self.path.is_empty() {
            return Err(ConfigError::MissingField { index, field: "path" });
        }
        if self.image_tag.is_empty() {
            return Err(ConfigError::MissingField { index, field: "image_tag" });
        }
        let git_ref = if self.git_ref.is_empty() { "main".to_string() } else { self.git_ref.clone() };
        Ok(WatchedDockerfile {
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            git_ref,
            path: self.path.clone(),
            image_tag: self.image_tag.clone(),
        })
    }
}

/// Default poll interval: 15 minutes, per the Phase 3 spec.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 15 * 60;

fn default_poll_interval() -> u64 {
    DEFAULT_POLL_INTERVAL_SECS
}

fn default_github_api_base() -> String {
    "https://api.github.com".to_string()
}

fn default_github_raw_base() -> String {
    "https://raw.githubusercontent.com".to_string()
}

/// As-deserialized top-level config — every field defaulted so an
/// empty/partial YAML file still loads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RawPrewarmerConfig {
    pub watched: Vec<RawWatchedDockerfile>,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_github_api_base")]
    pub github_api_base: String,
    #[serde(default = "default_github_raw_base")]
    pub github_raw_base: String,
    #[serde(default)]
    pub cache_backend: sui_cache::config::BackendConfig,
}

impl Default for RawPrewarmerConfig {
    fn default() -> Self {
        Self {
            watched: Vec::new(),
            poll_interval_secs: default_poll_interval(),
            github_api_base: default_github_api_base(),
            github_raw_base: default_github_raw_base(),
            cache_backend: sui_cache::config::BackendConfig::default(),
        }
    }
}

/// Validated top-level config — the type every runtime consumer (the
/// poll loop, the CLI) accepts.
#[derive(Debug, Clone)]
pub struct PrewarmerConfig {
    pub watched: Vec<WatchedDockerfile>,
    pub poll_interval_secs: u64,
    pub github_api_base: String,
    pub github_raw_base: String,
    pub cache_backend: sui_cache::config::BackendConfig,
}

impl RawPrewarmerConfig {
    /// Validate every watched entry, surfacing the first failure with
    /// its index.
    ///
    /// # Errors
    ///
    /// Returns the first [`ConfigError`] encountered — either a
    /// malformed watched entry or a zero poll interval.
    pub fn validate(&self) -> Result<PrewarmerConfig, ConfigError> {
        if self.poll_interval_secs == 0 {
            return Err(ConfigError::ZeroPollInterval(0));
        }
        let watched = self
            .watched
            .iter()
            .enumerate()
            .map(|(index, raw)| raw.validate(index))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PrewarmerConfig {
            watched,
            poll_interval_secs: self.poll_interval_secs,
            github_api_base: self.github_api_base.clone(),
            github_raw_base: self.github_raw_base.clone(),
            cache_backend: self.cache_backend.clone(),
        })
    }
}

impl PrewarmerConfig {
    /// Discover and load `~/.config/sui-dockerfile-prewarmer/prewarmer.yaml`
    /// (or `$SUI_DOCKERFILE_PREWARMER_CONFIG`), then validate it.
    ///
    /// # Errors
    ///
    /// Returns `None` if no config file was found (callers should fall
    /// back to an explicit empty [`RawPrewarmerConfig::default`]
    /// validated config), or `Some(Err(...))` if a file was found but
    /// failed to parse/validate.
    #[must_use]
    pub fn discover_and_load() -> Option<Result<Self, ConfigLoadError>> {
        let path = ConfigDiscovery::new("sui-dockerfile-prewarmer")
            .env_override("SUI_DOCKERFILE_PREWARMER_CONFIG")
            .discover()
            .ok()?;
        Some(Self::load_from(&path))
    }

    /// Load + validate config from an explicit path.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigLoadError::Shikumi`] if the file can't be parsed,
    /// or [`ConfigLoadError::Config`] if it parses but fails validation.
    #[allow(clippy::result_large_err)]
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, ConfigLoadError> {
        let store = ConfigStore::<RawPrewarmerConfig>::load(path.as_ref(), "SUI_DOCKERFILE_PREWARMER_")
            .map_err(ConfigLoadError::Shikumi)?;
        store.get().validate().map_err(ConfigLoadError::Config)
    }
}

/// Union of the two ways loading a config can fail.
#[derive(Debug, thiserror::Error)]
pub enum ConfigLoadError {
    #[error("loading config file: {0}")]
    Shikumi(#[from] shikumi::ShikumiError),
    #[error("validating config: {0}")]
    Config(#[from] ConfigError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_default_has_sensible_values() {
        let raw = RawPrewarmerConfig::default();
        assert_eq!(raw.poll_interval_secs, 15 * 60);
        assert_eq!(raw.github_api_base, "https://api.github.com");
        assert!(raw.watched.is_empty());
    }

    #[test]
    fn valid_entry_validates() {
        let raw = RawWatchedDockerfile {
            owner: "akeylesslabs".to_string(),
            repo: "akeyless-main-repo".to_string(),
            git_ref: "master".to_string(),
            path: "tools/deployment/docker/base/Dockerfile".to_string(),
            image_tag: "akeyless/base:prewarm".to_string(),
        };
        let validated = raw.validate(0).unwrap();
        assert_eq!(validated.owner, "akeylesslabs");
        assert_eq!(validated.git_ref, "master");
    }

    #[test]
    fn missing_ref_defaults_to_main() {
        let raw = RawWatchedDockerfile {
            owner: "akeylesslabs".to_string(),
            repo: "akeyless-main-repo".to_string(),
            git_ref: String::new(),
            path: "Dockerfile".to_string(),
            image_tag: "img:tag".to_string(),
        };
        let validated = raw.validate(0).unwrap();
        assert_eq!(validated.git_ref, "main");
    }

    #[test]
    fn missing_repo_is_a_typed_config_error_not_a_panic() {
        let raw = RawWatchedDockerfile {
            owner: "akeylesslabs".to_string(),
            repo: String::new(),
            git_ref: "main".to_string(),
            path: "Dockerfile".to_string(),
            image_tag: "img:tag".to_string(),
        };
        let err = raw.validate(2).unwrap_err();
        assert_eq!(err, ConfigError::MissingField { index: 2, field: "repo" });
    }

    #[test]
    fn missing_owner_is_a_typed_config_error() {
        let raw = RawWatchedDockerfile {
            owner: String::new(),
            repo: "r".to_string(),
            git_ref: "main".to_string(),
            path: "Dockerfile".to_string(),
            image_tag: "img:tag".to_string(),
        };
        assert_eq!(raw.validate(0).unwrap_err(), ConfigError::MissingField { index: 0, field: "owner" });
    }

    #[test]
    fn missing_path_is_a_typed_config_error() {
        let raw = RawWatchedDockerfile {
            owner: "o".to_string(),
            repo: "r".to_string(),
            git_ref: "main".to_string(),
            path: String::new(),
            image_tag: "img:tag".to_string(),
        };
        assert_eq!(raw.validate(0).unwrap_err(), ConfigError::MissingField { index: 0, field: "path" });
    }

    #[test]
    fn missing_image_tag_is_a_typed_config_error() {
        let raw = RawWatchedDockerfile {
            owner: "o".to_string(),
            repo: "r".to_string(),
            git_ref: "main".to_string(),
            path: "Dockerfile".to_string(),
            image_tag: String::new(),
        };
        assert_eq!(raw.validate(0).unwrap_err(), ConfigError::MissingField { index: 0, field: "image_tag" });
    }

    #[test]
    fn top_level_validate_surfaces_first_bad_entry_index() {
        let raw = RawPrewarmerConfig {
            watched: vec![
                RawWatchedDockerfile {
                    owner: "o".to_string(),
                    repo: "r".to_string(),
                    git_ref: "main".to_string(),
                    path: "Dockerfile".to_string(),
                    image_tag: "img:tag".to_string(),
                },
                RawWatchedDockerfile::default(),
            ],
            ..RawPrewarmerConfig::default()
        };
        let err = raw.validate().unwrap_err();
        assert_eq!(err, ConfigError::MissingField { index: 1, field: "owner" });
    }

    #[test]
    fn zero_poll_interval_is_a_typed_config_error() {
        let raw = RawPrewarmerConfig { poll_interval_secs: 0, ..RawPrewarmerConfig::default() };
        assert_eq!(raw.validate().unwrap_err(), ConfigError::ZeroPollInterval(0));
    }

    #[test]
    fn load_from_yaml_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let config_file = dir.path().join("prewarmer.yaml");
        std::fs::write(
            &config_file,
            "poll_interval_secs: 60\nwatched:\n  - owner: akeylesslabs\n    repo: akeyless-main-repo\n    ref: master\n    path: tools/deployment/docker/base/Dockerfile\n    image_tag: akeyless/base:prewarm\n",
        )
        .unwrap();

        let cfg = PrewarmerConfig::load_from(&config_file).unwrap();
        assert_eq!(cfg.poll_interval_secs, 60);
        assert_eq!(cfg.watched.len(), 1);
        assert_eq!(cfg.watched[0].owner, "akeylesslabs");
        assert_eq!(cfg.watched[0].git_ref, "master");
    }

    #[test]
    fn load_from_yaml_with_malformed_entry_is_a_config_error() {
        let dir = tempfile::tempdir().unwrap();
        let config_file = dir.path().join("prewarmer.yaml");
        std::fs::write(
            &config_file,
            "watched:\n  - owner: akeylesslabs\n    path: Dockerfile\n    image_tag: img:tag\n",
        )
        .unwrap();

        let err = PrewarmerConfig::load_from(&config_file).unwrap_err();
        assert!(matches!(err, ConfigLoadError::Config(ConfigError::MissingField { field: "repo", .. })));
    }
}
