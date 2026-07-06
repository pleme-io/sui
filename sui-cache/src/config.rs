//! Cache configuration types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::storage::WritePolicy;

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
    /// Redis L1 hot cache (requires the `redis-client` feature to construct).
    Redis {
        /// Connection URL, e.g. `redis://redis.super-cache-ci.svc:6379`.
        url: String,
        /// Optional per-write TTL in seconds; `None` relies on `maxmemory` LRU.
        #[serde(default)]
        ttl_secs: Option<u64>,
    },
    /// Postgres L2 durable cache tier (requires the `postgres` feature to
    /// construct).
    Pg {
        /// Connection URL, e.g. `postgres://user@postgres.super-cache-ci.svc:5432/sui`.
        url: String,
        /// Connection-pool ceiling.
        max_conns: u32,
    },
    /// Tiered `L1 → L2 → L3` resolver composing three nested backends.
    ///
    /// The canonical super-cache shape: `l1: Redis`, `l2: Pg`, `l3: S3`. Any
    /// nesting is legal (the arms recurse), so a deployment can pick `{disk |
    /// tiered}` — or any composition — purely by config.
    Tiered {
        /// L1 hot tier (typically [`Redis`](BackendConfig::Redis)).
        l1: Box<BackendConfig>,
        /// L2 durable tier (typically [`Pg`](BackendConfig::Pg)).
        l2: Box<BackendConfig>,
        /// L3 object tier (typically [`S3`](BackendConfig::S3)).
        l3: Box<BackendConfig>,
        /// How `put`s propagate across the tiers.
        #[serde(default)]
        write_policy: WritePolicy,
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
