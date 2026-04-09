//! Cache configuration types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Top-level cache configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Network address to listen on.
    pub listen: String,
    /// Storage backend configuration.
    pub backend: BackendConfig,
    /// Path to the ed25519 signing secret key file.
    pub signing_key: Option<PathBuf>,
    /// Cache priority (lower = preferred). Reported in nix-cache-info.
    pub priority: u32,
    /// Whether to want mass query (narinfo pipelining).
    pub want_mass_query: bool,
    /// The Nix store directory (almost always `/nix/store`).
    pub store_dir: String,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:5000".to_string(),
            backend: BackendConfig::default(),
            signing_key: None,
            priority: 40,
            want_mass_query: true,
            store_dir: "/nix/store".to_string(),
        }
    }
}

/// Storage backend selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BackendConfig {
    /// Local filesystem storage.
    Local {
        /// Root directory for NAR and narinfo files.
        path: PathBuf,
    },
    /// S3-compatible object storage (stub).
    S3 {
        bucket: String,
        region: String,
        endpoint: Option<String>,
    },
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self::Local {
            path: PathBuf::from("/var/cache/sui"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sane_values() {
        let config = CacheConfig::default();
        assert_eq!(config.listen, "0.0.0.0:5000");
        assert_eq!(config.store_dir, "/nix/store");
        assert_eq!(config.priority, 40);
        assert!(config.want_mass_query);
        assert!(config.signing_key.is_none());
    }

    #[test]
    fn default_backend_is_local() {
        let config = CacheConfig::default();
        assert!(matches!(config.backend, BackendConfig::Local { .. }));
    }

    #[test]
    fn config_serializes_to_json() {
        let config = CacheConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("local"));
        assert!(json.contains("5000"));
    }

    #[test]
    fn config_roundtrips_through_json() {
        let config = CacheConfig {
            listen: "127.0.0.1:8080".to_string(),
            backend: BackendConfig::S3 {
                bucket: "my-cache".to_string(),
                region: "us-east-1".to_string(),
                endpoint: Some("http://localhost:9000".to_string()),
            },
            signing_key: Some(PathBuf::from("/tmp/key.sec")),
            priority: 30,
            want_mass_query: false,
            store_dir: "/nix/store".to_string(),
        };
        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: CacheConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.listen, "127.0.0.1:8080");
        assert_eq!(parsed.priority, 30);
        assert!(!parsed.want_mass_query);
        assert!(matches!(parsed.backend, BackendConfig::S3 { .. }));
    }
}
