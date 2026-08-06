//! Cache configuration types.
//!
//! [`BackendConfig`] (the storage backend selector) lives in `sui-castore` and
//! is re-exported here for backward compatibility. [`CacheConfig`] (the
//! cache-server configuration) is owned by this module.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use shikumi::TieredConfig;

use crate::push::{NarCodec, XzLevel, ZstdLevel};

// BackendConfig is defined in sui-castore; import it for use in CacheConfig.
pub use sui_castore::BackendConfig;
pub use sui_castore::WritePolicy;

/// The shikumi tier-selector environment variable for this cache.
///
/// Fleet convention (`<APP>_TIER`): unset or `default` → [`prescribed
/// default`](CacheConfig::prescribed_default); `bare` → the honest floor;
/// anything else is read as a path to a YAML overlay laid over the prescribed
/// default. See [`CacheConfig::resolve`].
pub const CACHE_TIER_ENV: &str = "SUI_CACHE_TIER";

/// Top-level cache configuration.
///
/// `#[serde(default)]` is what makes this an *overlay* rather than a
/// replacement: shikumi's `Custom` tier deserializes the operator's YAML into
/// this type whole, so without it a file that wants to change one field would
/// have to restate every other one — and a file that failed to would be
/// silently discarded (shikumi falls back to `prescribed_default` on a parse
/// error). Absent fields now come from [`Default`], which *is*
/// [`TieredConfig::prescribed_default`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
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

    /// How a pushed NAR is packed — the codec **and** its level, as one
    /// inseparable value (see [`NarCodec`]).
    ///
    /// This is the deployment knob the benchmark in [`NarCodec`]'s docs argues
    /// about. rio is a *local* origin serving a handful of fleet nodes over
    /// tailscale: CPU-bound, bandwidth-cheap, so zstd -12 is right and is the
    /// prescribed default. A bandwidth-bound origin — one paying egress, or
    /// seeding cold clients over the public internet — legitimately wants
    /// `{ codec: xz, level: 6 }` and now gets it from a config file instead of
    /// a recompile.
    ///
    /// A mixed cache needs no migration: every narinfo declares its own codec,
    /// so flipping this changes only what *new* pushes look like.
    #[serde(default)]
    pub nar_codec: NarCodec,
}

impl Default for CacheConfig {
    /// Delegates to [`TieredConfig::prescribed_default`] so the standard idiom
    /// (`CacheConfig::default()`) and the tiered resolution can never describe
    /// two different caches.
    fn default() -> Self {
        <Self as TieredConfig>::prescribed_default()
    }
}

impl CacheConfig {
    /// Resolve this cache's configuration the fleet-standard way — the one
    /// call site every entry point uses (★★ CONFIGURATION MANAGEMENT).
    ///
    /// Precedence is shikumi's: the [`CACHE_TIER_ENV`] environment variable
    /// selects the tier, and when it names a path that YAML file is overlaid
    /// on the prescribed default. Unset → the prescribed default, unchanged
    /// from what this cache did before it had a config surface.
    #[must_use]
    pub fn resolve() -> Self {
        <Self as TieredConfig>::resolve_from_env(CACHE_TIER_ENV)
    }
}

impl TieredConfig for CacheConfig {
    /// Tier 0 — the honest floor: **sui-cache exactly as it shipped before
    /// 2026-08-05**. xz -6 packing, no signing key, no fail-closed
    /// advertisement, the on-disk backend.
    ///
    /// This tier is not a worse default, it is the *documented past*: every
    /// `.nar.xz` already in a fleet cache was written by it, and an origin that
    /// wants the old ratio back asks for it by name (`SUI_CACHE_TIER=bare`)
    /// rather than by editing a constant.
    fn bare() -> Self {
        Self {
            listen: "0.0.0.0:5000".to_string(),
            backend: BackendConfig::default(),
            signing_key: None,
            priority: 40,
            want_mass_query: true,
            store_dir: "/nix/store".to_string(),
            require_sigs: false,
            nar_codec: NarCodec::Xz {
                level: XzLevel::default(),
            },
        }
    }

    /// Tier 2 — the prescribed posture: identical to [`bare`](Self::bare)
    /// except that pushes pack with **zstd**, the measured fast path. The
    /// whole point of the 2026-08-05 change is that you get it without asking.
    fn prescribed_default() -> Self {
        Self {
            nar_codec: NarCodec::Zstd {
                level: ZstdLevel::default(),
            },
            ..Self::bare()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shikumi::ConfigTier;
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
            nar_codec: NarCodec::default(),
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
            BackendConfig::Tiered {
                l1,
                l2,
                l3,
                write_policy,
            } => {
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

    // ── The shikumi tier surface (★★ CONFIGURATION MANAGEMENT) ──────────

    #[test]
    fn the_prescribed_tier_is_the_measured_fast_path() {
        // An operator who configures nothing gets zstd at the measured knee.
        // This is the guarantee the whole 2026-08-05 change exists for, stated
        // at the CONFIG surface — the place a deployment can now move it.
        assert_eq!(
            CacheConfig::prescribed_default().nar_codec,
            NarCodec::Zstd {
                level: ZstdLevel::default()
            }
        );
        assert_eq!(CacheConfig::default(), CacheConfig::prescribed_default());
    }

    #[test]
    fn the_bare_tier_is_the_documented_past() {
        // Tier 0 is not "a worse default", it is what every already-stored
        // `.nar.xz` in the fleet was written by. Asking for it by name is how
        // a bandwidth-bound origin opts out of the CPU-cheap posture.
        assert_eq!(
            CacheConfig::bare().nar_codec,
            NarCodec::Xz {
                level: XzLevel::default()
            }
        );
        // …and the two tiers differ ONLY in the codec — the tier selector is
        // not a back door for changing the listen address or the backend.
        let promoted = CacheConfig {
            nar_codec: CacheConfig::prescribed_default().nar_codec,
            ..CacheConfig::bare()
        };
        assert_eq!(promoted, CacheConfig::prescribed_default());
    }

    #[test]
    fn a_partial_yaml_overlay_changes_one_field_and_keeps_the_rest() {
        // The overlay property, through shikumi's own loader. A file naming
        // only the codec must not silently reset the listen address, and must
        // not be discarded for being incomplete.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.yaml");
        std::fs::write(&path, "nar_codec:\n  codec: xz\n  level: 9\n").unwrap();

        let resolved = CacheConfig::resolve_tier(ConfigTier::Custom(path));
        assert_eq!(
            resolved.nar_codec,
            NarCodec::Xz {
                level: XzLevel::new(9).unwrap()
            },
            "the overlay must reach the codec"
        );
        assert_eq!(
            resolved.listen,
            CacheConfig::prescribed_default().listen,
            "an unmentioned field must keep its prescribed value"
        );
        assert_eq!(
            resolved.priority,
            CacheConfig::prescribed_default().priority
        );
    }

    #[test]
    fn a_yaml_overlay_may_omit_the_codec_and_keep_the_fast_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.yaml");
        std::fs::write(&path, "priority: 10\n").unwrap();

        let resolved = CacheConfig::resolve_tier(ConfigTier::Custom(path));
        assert_eq!(resolved.priority, 10);
        assert_eq!(
            resolved.nar_codec,
            CacheConfig::prescribed_default().nar_codec,
            "not naming a codec must leave the fast default in place"
        );
    }

    #[test]
    fn the_named_tiers_resolve_to_their_tier_methods() {
        assert_eq!(
            CacheConfig::resolve_tier(ConfigTier::Bare),
            CacheConfig::bare()
        );
        assert_eq!(
            CacheConfig::resolve_tier(ConfigTier::Default),
            CacheConfig::prescribed_default()
        );
    }

    #[test]
    fn the_config_round_trips_the_codec_through_json() {
        let cfg = CacheConfig {
            nar_codec: NarCodec::Xz {
                level: XzLevel::new(3).unwrap(),
            },
            ..CacheConfig::default()
        };
        let parsed: CacheConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn redis_ttl_defaults_to_none() {
        let json = r#"{ "type": "redis", "url": "redis://r:6379" }"#;
        let parsed: BackendConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(
            parsed,
            BackendConfig::Redis { ttl_secs: None, .. }
        ));
    }
}
