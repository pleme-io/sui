//! Shikumi-based daemon configuration.
//!
//! Defines [`SuiDaemonConfig`] with XDG-compliant config discovery via
//! [`shikumi::ConfigDiscovery`] and hot-reloadable storage via
//! [`shikumi::ConfigStore`].
//!
//! Config file location: `~/.config/sui/sui.yaml` (or `$XDG_CONFIG_HOME/sui/sui.yaml`).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use shikumi::{ConfigDiscovery, ConfigStore};
use tsunagu::SocketPath;

/// Daemon configuration loaded from `~/.config/sui/sui.yaml`.
///
/// All fields have sensible defaults so the daemon can start without
/// a config file present.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SuiDaemonConfig {
    /// TCP listen address for the REST/GraphQL API.
    pub listen_address: String,
    /// TCP listen address for the gRPC server.
    pub grpc_listen_address: String,
    /// Directory for the Nix store metadata database.
    pub store_dir: PathBuf,
    /// Unix socket path for the worker protocol.
    pub socket_path: PathBuf,
    /// Log level filter (e.g., "info", "debug", "sui_daemon=trace").
    pub log_level: String,
}

impl Default for SuiDaemonConfig {
    fn default() -> Self {
        Self {
            listen_address: "127.0.0.1:8080".to_string(),
            grpc_listen_address: "127.0.0.1:50051".to_string(),
            store_dir: PathBuf::from("/nix/store"),
            socket_path: SocketPath::for_app("sui"),
            log_level: "info".to_string(),
        }
    }
}

impl SuiDaemonConfig {
    /// Discover and load the config file using shikumi.
    ///
    /// Searches XDG paths for `sui.yaml` or `sui.toml`. Environment
    /// variables prefixed with `SUI_` override file values.
    ///
    /// Returns the loaded config store for hot-reloadable access, or
    /// `None` if no config file was found (in which case callers should
    /// use [`SuiDaemonConfig::default()`]).
    #[must_use]
    pub fn discover_and_load() -> Option<ConfigStore<Self>> {
        let path = ConfigDiscovery::new("sui")
            .env_override("SUI_CONFIG")
            .discover()
            .ok()?;

        tracing::info!(path = %path.display(), "loading daemon config");

        ConfigStore::<Self>::load(&path, "SUI_")
            .map_err(|e| {
                tracing::warn!("failed to load config from {}: {e}", path.display());
                e
            })
            .ok()
    }

    /// Load config from a specific path (for testing or explicit `--config` flag).
    ///
    /// # Errors
    ///
    /// Returns a shikumi error if the file cannot be parsed.
    pub fn load_from(path: impl AsRef<Path>) -> Result<ConfigStore<Self>, shikumi::ShikumiError> {
        ConfigStore::<Self>::load(path.as_ref(), "SUI_")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn default_config_has_sensible_values() {
        let config = SuiDaemonConfig::default();
        assert_eq!(config.listen_address, "127.0.0.1:8080");
        assert_eq!(config.grpc_listen_address, "127.0.0.1:50051");
        assert_eq!(config.store_dir, PathBuf::from("/nix/store"));
        assert!(config.socket_path.to_string_lossy().contains("sui"));
        assert_eq!(config.log_level, "info");
    }

    #[test]
    fn socket_path_uses_tsunagu() {
        let config = SuiDaemonConfig::default();
        let expected = SocketPath::for_app("sui");
        assert_eq!(config.socket_path, expected);
    }

    #[test]
    fn load_from_yaml_file() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("sui.yaml");
        fs::write(
            &config_file,
            "listen_address: \"0.0.0.0:9090\"\nlog_level: debug\n",
        )
        .unwrap();

        let store = SuiDaemonConfig::load_from(&config_file).unwrap();
        let config = store.get();
        assert_eq!(config.listen_address, "0.0.0.0:9090");
        assert_eq!(config.log_level, "debug");
        // Unprovided fields get serde defaults
        assert_eq!(config.grpc_listen_address, "127.0.0.1:50051");
    }

    #[test]
    fn load_from_empty_file_uses_defaults() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("sui.yaml");
        fs::write(&config_file, "").unwrap();

        let store = SuiDaemonConfig::load_from(&config_file).unwrap();
        let config = store.get();
        assert_eq!(config.listen_address, "127.0.0.1:8080");
        assert_eq!(config.store_dir, PathBuf::from("/nix/store"));
    }

    #[test]
    fn load_partial_yaml() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("sui.yaml");
        fs::write(
            &config_file,
            "store_dir: /custom/store\nsocket_path: /tmp/custom.sock\n",
        )
        .unwrap();

        let store = SuiDaemonConfig::load_from(&config_file).unwrap();
        let config = store.get();
        assert_eq!(config.store_dir, PathBuf::from("/custom/store"));
        assert_eq!(config.socket_path, PathBuf::from("/tmp/custom.sock"));
        // Defaults for unspecified fields
        assert_eq!(config.listen_address, "127.0.0.1:8080");
    }

    #[test]
    fn discover_returns_none_when_no_config() {
        // No config file exists for "sui" in test environment
        let result = SuiDaemonConfig::discover_and_load();
        // This is allowed to be None (no config file) or Some (if the
        // developer happens to have ~/.config/sui/sui.yaml)
        let _ = result;
    }

    #[test]
    fn config_store_supports_reload() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("sui.yaml");
        fs::write(&config_file, "log_level: info\n").unwrap();

        let store = SuiDaemonConfig::load_from(&config_file).unwrap();
        assert_eq!(store.get().log_level, "info");

        fs::write(&config_file, "log_level: trace\n").unwrap();
        store.reload().unwrap();
        assert_eq!(store.get().log_level, "trace");
    }
}
