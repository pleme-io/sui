//! The pure `SuperCacheCiConfig -> sui_cache::BackendConfig` translation.
//!
//! This is the load-bearing map that turns the shikumi-typed store/cache
//! posture (STORE = Postgres L2, CACHE = Redis L1, L3 = object store) into the
//! concrete tiered `BackendConfig` that `sui_cache::build_backend` dispatches at
//! `sui cache serve`. It is **pure and I/O-free** — a total function over the
//! config, unit-testable against every tier arm, with no network, filesystem,
//! or database reach. `build_backend` performs the actual connect; this only
//! shapes the config it consumes.
//!
//! The map closes the crate's `LiveTODO(tiered)` on the *config-select* axis:
//! the schema no longer merely *describes* a tiered posture, it *produces* the
//! `BackendConfig::Tiered { l1, l2, l3, write_policy }` the daemon runs. What
//! remains a LiveTODO is the live-cluster proof (a real Postgres + Redis run),
//! which the ground-truth recipe under `contrib/ground-truth/` exercises.

use sui_cache::{BackendConfig, WritePolicy};

use crate::{
    CacheBackendKind, CacheCfg, ObjectStoreCfg, ObjectStoreKind, StoreBackendKind, StoreCfg,
    SuperCacheCiConfig,
};

/// Errors from translating a [`SuperCacheCiConfig`] into a runnable
/// [`BackendConfig`].
///
/// The translation is *total for the destination posture* (Postgres store +
/// Redis cache + an object-store L3). The error arm exists for the honest
/// [`bare`](SuperCacheCiConfig::bare) floor and any tier that names a backend
/// with no runnable transport — surfacing the gap as a typed value rather than
/// silently degrading to disk (the never-silent-hardcode rule).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BackendConfigError {
    /// The store tier selects a backend the tiered `cache serve` path cannot
    /// realize as a `sui_cache::StorageBackend` L2 (e.g. the on-disk
    /// `GraphStore` floor, which is the durable-store axis, not the cache
    /// factory's L2 arm).
    #[error("store backend {0:?} has no cache-serve L2 mapping (it is the durable-store axis, not the tiered cache factory)")]
    UnmappableStore(StoreBackendKind),

    /// The cache tier selects a backend the tiered factory cannot realize as an
    /// L1 (e.g. the on-disk `Redb` floor).
    #[error("cache backend {0:?} has no tiered-L1 mapping (it is the on-disk floor, not a super-cache L1)")]
    UnmappableCache(CacheBackendKind),

    /// A required endpoint is unset (host empty or port zero) — the honest
    /// `bare()` floor, which selects no Postgres/Redis service.
    #[error("endpoint for {0} is unset (host/port empty) — the bare() floor selects no service")]
    UnsetEndpoint(&'static str),
}

impl SuperCacheCiConfig {
    /// Translate the store/cache/object-store posture into the concrete
    /// [`BackendConfig::Tiered`] the `cache serve` daemon dispatches.
    ///
    /// L1 ← [`CacheCfg`] (Redis), L2 ← [`StoreCfg`] (Postgres), L3 ←
    /// [`ObjectStoreCfg`] (S3-compatible or a local floor). The
    /// [`WritePolicy`] is the tiered default ([`WriteThrough`](WritePolicy::WriteThrough))
    /// — durable-first, then warm L1, so a crash never loses a write that only
    /// reached the hot tier.
    ///
    /// Pure: no I/O. `build_backend` performs the connect against the returned
    /// config.
    ///
    /// # Errors
    ///
    /// Returns [`BackendConfigError`] when the posture is the honest floor or
    /// names a backend with no runnable tiered transport, rather than silently
    /// substituting disk.
    pub fn to_backend_config(&self) -> Result<BackendConfig, BackendConfigError> {
        let l1 = cache_to_l1(&self.cache)?;
        let l2 = store_to_l2(&self.store)?;
        let l3 = object_store_to_l3(&self.store.l3);
        Ok(BackendConfig::Tiered {
            l1: Box::new(l1),
            l2: Box::new(l2),
            l3: Box::new(l3),
            // Durable-first: a write reaches L2 (+ L3) before warming L1, so a
            // crash never strands data in the hot tier alone.
            write_policy: WritePolicy::WriteThrough,
        })
    }
}

/// Map the hot-cache tier to the tiered L1 `BackendConfig`.
fn cache_to_l1(cache: &CacheCfg) -> Result<BackendConfig, BackendConfigError> {
    match cache.backend {
        CacheBackendKind::Redis => {
            require_endpoint("cache.redis", &cache.redis.host, cache.redis.port)?;
            Ok(BackendConfig::Redis {
                url: format_redis_url(&cache.redis.host, cache.redis.port),
                // The band drives LRU eviction at the ceiling; leave per-write
                // TTL to `maxmemory` LRU (None) so the hot set is bounded by RAM
                // rather than wall-clock age.
                ttl_secs: None,
            })
        }
        // The on-disk floors are the durable/redb axis, not a super-cache L1.
        other => Err(BackendConfigError::UnmappableCache(other)),
    }
}

/// Map the durable store tier to the tiered L2 `BackendConfig`.
fn store_to_l2(store: &StoreCfg) -> Result<BackendConfig, BackendConfigError> {
    match store.backend {
        StoreBackendKind::Postgres => {
            require_endpoint("store.pg", &store.pg.host, store.pg.port)?;
            Ok(BackendConfig::Pg {
                url: format_pg_url(&store.pg.host, store.pg.port),
                max_conns: store.pg_pool,
            })
        }
        // GraphStore / Local are the durable-store axis (sui-store's Store
        // trait), not the tiered cache factory's L2 arm.
        other => Err(BackendConfigError::UnmappableStore(other)),
    }
}

/// Map the L3 object-store tier to a `BackendConfig`.
///
/// This arm is total: an S3-compatible endpoint becomes `BackendConfig::S3`; the
/// `None`/Attic/Armazem floors become a local sink so the tiered resolver always
/// has a bottom tier (`build_backend`'s Tiered arm requires all three). The
/// local sink is the honest floor for a single-node ground-truth run where no
/// external object store is provisioned.
fn object_store_to_l3(l3: &ObjectStoreCfg) -> BackendConfig {
    match l3.kind {
        ObjectStoreKind::S3 => BackendConfig::S3 {
            bucket: l3.bucket.clone(),
            region: "us-east-1".to_string(),
            endpoint: l3.endpoint.as_ref().map(|e| format!("http://{}:{}", e.host, e.port)),
        },
        // Attic / Armazem / None: no S3-wire L3 for the ground-truth node. The
        // tiered resolver still needs a bottom tier, so the durable pair
        // (Redis L1 + Postgres L2) is fronted by a local L3 floor. On a
        // never-touch-disk node this path is exercised only on an L1+L2 miss,
        // which the single-node run does not hit for its own writes.
        _ => BackendConfig::Local {
            path: std::path::PathBuf::from("/var/cache/sui-l3"),
        },
    }
}

/// A Redis connection URL.
fn format_redis_url(host: &str, port: u16) -> String {
    format!("redis://{host}:{port}")
}

/// A Postgres connection URL for the sui super-cache database.
fn format_pg_url(host: &str, port: u16) -> String {
    // The `sui` role + `sui` database are provisioned by the ground-truth
    // recipe (postgres:17-alpine, POSTGRES_PASSWORD=sui).
    format!("postgres://sui:sui@{host}:{port}/sui")
}

/// Guard: reject an unset endpoint (the `bare()` floor) as a typed error.
fn require_endpoint(name: &'static str, host: &str, port: u16) -> Result<(), BackendConfigError> {
    if host.is_empty() || port == 0 {
        return Err(BackendConfigError::UnsetEndpoint(name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Endpoint, MemoryBand};
    use shikumi::TieredConfig;

    #[test]
    fn prescribed_default_translates_to_a_tiered_backend() {
        let cfg = SuperCacheCiConfig::prescribed_default();
        let backend = cfg.to_backend_config().expect("destination posture must translate");
        match backend {
            BackendConfig::Tiered { l1, l2, l3, write_policy } => {
                assert!(matches!(*l1, BackendConfig::Redis { .. }), "L1 is Redis");
                assert!(matches!(*l2, BackendConfig::Pg { .. }), "L2 is Postgres");
                // L3 is the local floor because prescribed_default selects Attic
                // (no S3-wire endpoint) — the tiered resolver still has a bottom.
                assert!(matches!(*l3, BackendConfig::Local { .. }), "L3 floor is local");
                assert_eq!(
                    write_policy,
                    WritePolicy::WriteThrough,
                    "durable-first write policy"
                );
            }
            other => panic!("expected Tiered, got {other:?}"),
        }
    }

    #[test]
    fn redis_l1_carries_the_configured_endpoint() {
        let cfg = SuperCacheCiConfig::prescribed_default();
        let BackendConfig::Tiered { l1, .. } = cfg.to_backend_config().unwrap() else {
            panic!("expected tiered");
        };
        match *l1 {
            BackendConfig::Redis { url, ttl_secs } => {
                assert!(url.starts_with("redis://"));
                assert!(url.contains("6379"));
                // Band-bounded (maxmemory LRU), not wall-clock TTL.
                assert_eq!(ttl_secs, None);
            }
            other => panic!("expected Redis L1, got {other:?}"),
        }
    }

    #[test]
    fn postgres_l2_carries_pool_and_endpoint() {
        let cfg = SuperCacheCiConfig::prescribed_default();
        let BackendConfig::Tiered { l2, .. } = cfg.to_backend_config().unwrap() else {
            panic!("expected tiered");
        };
        match *l2 {
            BackendConfig::Pg { url, max_conns } => {
                assert!(url.starts_with("postgres://"));
                assert!(url.contains("5432"));
                assert_eq!(max_conns, 16, "prescribed pg_pool");
            }
            other => panic!("expected Pg L2, got {other:?}"),
        }
    }

    #[test]
    fn s3_object_store_maps_to_s3_l3() {
        // A posture that names an S3 L3 endpoint produces a BackendConfig::S3
        // bottom tier (not the local floor).
        let mut cfg = SuperCacheCiConfig::prescribed_default();
        cfg.store.l3 = ObjectStoreCfg {
            kind: ObjectStoreKind::S3,
            endpoint: Some(Endpoint::at("minio.local", 9000)),
            bucket: "sui-super-cache".to_string(),
        };
        let BackendConfig::Tiered { l3, .. } = cfg.to_backend_config().unwrap() else {
            panic!("expected tiered");
        };
        match *l3 {
            BackendConfig::S3 { bucket, endpoint, .. } => {
                assert_eq!(bucket, "sui-super-cache");
                assert_eq!(endpoint.as_deref(), Some("http://minio.local:9000"));
            }
            other => panic!("expected S3 L3, got {other:?}"),
        }
    }

    #[test]
    fn bare_floor_is_a_typed_error_not_a_silent_disk_fallback() {
        // The honest floor selects GraphStore + Redb (on-disk) and unset
        // endpoints. Translating it must FAIL with a typed error rather than
        // silently produce a disk-backed tiered config — the never-silent rule.
        let bare = SuperCacheCiConfig::bare();
        let err = bare.to_backend_config().expect_err("bare() must not translate to a tiered backend");
        // Cache is checked first (Redb has no L1 mapping).
        assert_eq!(err, BackendConfigError::UnmappableCache(CacheBackendKind::Redb));
    }

    #[test]
    fn unset_redis_endpoint_is_rejected() {
        // Redis selected but endpoint unset (a malformed override) → typed error.
        let mut cfg = SuperCacheCiConfig::prescribed_default();
        cfg.cache.redis = Endpoint::unset();
        let err = cfg.to_backend_config().expect_err("unset redis endpoint must be rejected");
        assert_eq!(err, BackendConfigError::UnsetEndpoint("cache.redis"));
    }

    #[test]
    fn unset_postgres_endpoint_is_rejected() {
        let mut cfg = SuperCacheCiConfig::prescribed_default();
        cfg.store.pg = Endpoint::unset();
        let err = cfg.to_backend_config().expect_err("unset pg endpoint must be rejected");
        assert_eq!(err, BackendConfigError::UnsetEndpoint("store.pg"));
    }

    #[test]
    fn localhost_ground_truth_posture_translates() {
        // The single-podman-VM posture: Redis + Postgres on localhost, band
        // sized for the 8 GiB VM. This is the exact shape the ground-truth
        // recipe's tiered.toml encodes; prove it translates end to end.
        let mut cfg = SuperCacheCiConfig::prescribed_default();
        cfg.store.pg = Endpoint::at("127.0.0.1", 5432);
        cfg.store.pg_pool = 8;
        cfg.store.l3 = ObjectStoreCfg {
            kind: ObjectStoreKind::None,
            endpoint: None,
            bucket: String::new(),
        };
        cfg.cache.redis = Endpoint::at("127.0.0.1", 6379);
        cfg.cache.memory_band = MemoryBand {
            target_pct: 80,
            headroom_pct: 20,
            max_mib: 2048,
            dry_run: true,
        };
        let backend = cfg.to_backend_config().expect("localhost posture translates");
        let BackendConfig::Tiered { l1, l2, l3, .. } = backend else {
            panic!("expected tiered");
        };
        assert!(matches!(*l1, BackendConfig::Redis { .. }));
        assert!(matches!(*l2, BackendConfig::Pg { .. }));
        // None object store → local L3 floor.
        assert!(matches!(*l3, BackendConfig::Local { .. }));
    }
}
