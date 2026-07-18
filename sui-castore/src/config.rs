//! Storage backend configuration types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::storage::WritePolicy;

/// Storage backend selection.
///
/// Dispatched by [`build_backend`](crate::build_backend) to the concrete
/// backend implementation. The `#[serde(tag = "type", rename_all =
/// "lowercase")]` shape is the stable JSON wire format; a config file's
/// `"type": "local"` / `"type": "tiered"` etc. is the operator's vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BackendConfig {
    /// Local filesystem storage.
    Local {
        /// Root directory for NAR and narinfo files.
        path: PathBuf,
    },
    /// S3-compatible object storage.
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
        /// Connection URL, e.g. `postgres://user@pg.svc:5432/sui`.
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
    fn default_backend_is_local() {
        assert!(matches!(BackendConfig::default(), BackendConfig::Local { .. }));
    }

    #[test]
    fn local_backend_roundtrips_through_json() {
        let cfg = BackendConfig::Local {
            path: PathBuf::from("/var/cache/sui"),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("local"));
        let parsed: BackendConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, BackendConfig::Local { .. }));
    }

    #[test]
    fn s3_backend_roundtrips_through_json() {
        let cfg = BackendConfig::S3 {
            bucket: "my-cache".to_string(),
            region: "us-east-1".to_string(),
            endpoint: Some("http://localhost:9000".to_string()),
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let parsed: BackendConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, BackendConfig::S3 { .. }));
    }

    #[test]
    fn redis_ttl_defaults_to_none() {
        let json = r#"{ "type": "redis", "url": "redis://r:6379" }"#;
        let parsed: BackendConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(parsed, BackendConfig::Redis { ttl_secs: None, .. }));
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
}
