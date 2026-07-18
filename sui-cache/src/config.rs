//! Cache configuration types.
//!
//! [`BackendConfig`] (the storage backend selector) lives in `sui-castore` and
//! is re-exported here for backward compatibility. [`CacheConfig`] (the
//! cache-server configuration) is owned by this module.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// BackendConfig is defined in sui-castore; import it for use in CacheConfig.
pub use sui_castore::BackendConfig;
pub use sui_castore::WritePolicy;

/// Top-level cache configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Network address to listen on.
    pub listen: String,
    /// Storage backend configuration.
    pub backend: BackendConfig,
    /// Path to the ed25519 signing secret key file.
    ///
    /// In production this path is a cofre/ESO-materialized Kubernetes Secret
    /// mount, never a plaintext literal. When set, the daemon signs every
    /// ingested narinfo (see [`serve`](crate::server::serve)).
    pub signing_key: Option<PathBuf>,
    /// Cache priority (lower = preferred). Reported in nix-cache-info.
    pub priority: u32,
    /// Whether to want mass query (narinfo pipelining).
    pub want_mass_query: bool,
    /// The Nix store directory (almost always `/nix/store`).
    pub store_dir: String,
    /// Whether this cache's consumers should require a valid signature.
    ///
    /// This is a serving-side advertisement of the fail-closed posture: a
    /// signing cache SHOULD publish `require_sigs = true` so operators know
    /// the served paths are signed and consumers must verify. It does not by
    /// itself change what the daemon serves (signing is driven by
    /// `signing_key`); it is the typed knob a consuming config reads to know
    /// the cache is trustworthy fail-closed. Defaults to `false` to preserve
    /// legacy behavior for caches that have not yet been given a key.
    #[serde(default)]
    pub require_sigs: bool,
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
            require_sigs: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
            require_sigs: true,
        };
        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: CacheConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.listen, "127.0.0.1:8080");
        assert_eq!(parsed.priority, 30);
        assert!(!parsed.want_mass_query);
        assert!(parsed.require_sigs);
        assert!(matches!(parsed.backend, BackendConfig::S3 { .. }));
    }

    #[test]
    fn require_sigs_defaults_to_false_when_absent() {
        // A config JSON that omits require_sigs deserializes to false
        // (the serde default) — legacy configs keep working.
        let json = r#"{
            "listen": "0.0.0.0:5000",
            "backend": { "type": "local", "path": "/var/cache/sui" },
            "signing_key": null,
            "priority": 40,
            "want_mass_query": true,
            "store_dir": "/nix/store"
        }"#;
        let parsed: CacheConfig = serde_json::from_str(json).unwrap();
        assert!(!parsed.require_sigs);
    }

    #[test]
    fn tiered_backend_roundtrips_through_json() {
        let backend = BackendConfig::Tiered {
            l1: Box::new(BackendConfig::Redis {
                url: "redis://redis:6379".to_string(),
                ttl_secs: Some(3600),
            }),
            l2: Box::new(BackendConfig::Pg {
                url: "postgres://pg:5432/sui".to_string(),
                max_conns: 16,
            }),
            l3: Box::new(BackendConfig::S3 {
                bucket: "sui-super-cache".to_string(),
                region: "us-east-1".to_string(),
                endpoint: None,
            }),
            write_policy: WritePolicy::WriteThrough,
        };
        let json = serde_json::to_string_pretty(&backend).unwrap();
        assert!(json.contains("tiered"));
        assert!(json.contains("redis"));
        assert!(json.contains("write-through"));
        let parsed: BackendConfig = serde_json::from_str(&json).unwrap();
        match parsed {
            BackendConfig::Tiered { l1, l2, l3, write_policy } => {
                assert!(matches!(*l1, BackendConfig::Redis { .. }));
                assert!(matches!(*l2, BackendConfig::Pg { .. }));
                assert!(matches!(*l3, BackendConfig::S3 { .. }));
                assert_eq!(write_policy, WritePolicy::WriteThrough);
            }
            other => panic!("expected tiered, got {other:?}"),
        }
    }

    #[test]
    fn tiered_write_policy_defaults_when_absent() {
        // A tiered config that omits `write_policy` deserializes to the default.
        let json = r#"{
            "type": "tiered",
            "l1": { "type": "redis", "url": "redis://r:6379" },
            "l2": { "type": "pg", "url": "postgres://p:5432/s", "max_conns": 8 },
            "l3": { "type": "local", "path": "/var/cache/sui" }
        }"#;
        let parsed: BackendConfig = serde_json::from_str(json).unwrap();
        match parsed {
            BackendConfig::Tiered { write_policy, .. } => {
                assert_eq!(write_policy, WritePolicy::default());
                assert_eq!(write_policy, WritePolicy::WriteThrough);
            }
            other => panic!("expected tiered, got {other:?}"),
        }
    }

    #[test]
    fn redis_ttl_defaults_to_none() {
        let json = r#"{ "type": "redis", "url": "redis://r:6379" }"#;
        let parsed: BackendConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(parsed, BackendConfig::Redis { ttl_secs: None, .. }));
    }
}
