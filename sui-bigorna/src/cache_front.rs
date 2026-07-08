//! `BuildKitCacheFront` — the store-backed cache endpoint fronting the same
//! builder bigorna sets up, so native-multi-arch and layer caching land through
//! ONE integration point.
//!
//! `BuildKit` already speaks a cache import/export protocol (`--cache-from` /
//! `--cache-to`). bigorna does not re-implement it — it *points* it at an
//! endpoint that fronts the shipped sui tiered content-addressed store
//! (`sui_cache::storage::StorageBackend`: Redis L1 → Postgres L2 → object
//! store L3). The wire `BuildKit` talks is one of the protocols it natively
//! understands (`registry` / `s3` / `local` / `inline`); the sui store sits
//! behind that wire. The map from a sui [`BackendConfig`] to a `BuildKit` cache
//! endpoint is [`CacheEndpoint::from_backend_config`].
//!
//! Every endpoint token (`type=registry,ref=…,mode=max`) is rendered by a
//! [`std::fmt::Display`] impl — the one sanctioned typed-emission surface — not
//! a `format!()` at a call site.

use std::fmt;

use serde::{Deserialize, Serialize};
use sui_cache::config::BackendConfig;

/// How much of the build graph's cache is exported. `Max` exports intermediate
/// layers too (the useful setting for cross-build reuse); `Min` exports only
/// the final image's layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    Min,
    #[default]
    Max,
}

impl CacheMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CacheMode::Min => "min",
            CacheMode::Max => "max",
        }
    }
}

/// One `BuildKit` cache endpoint — a wire protocol plus its coordinates. This is
/// the `BuildKitCacheProtocol` of the design: the concrete wire `BuildKit` talks,
/// with the sui store behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "wire", rename_all = "lowercase")]
pub enum CacheEndpoint {
    /// `type=registry,ref=<ref>[,mode=<mode>]` — a registry-fronted store (the
    /// canonical `BuildKitCacheFront` over a tiered sui store: `BuildKit` can't
    /// speak Redis/Postgres, so the tiered store is fronted by a registry ref).
    Registry {
        r#ref: String,
        #[serde(default)]
        mode: Option<CacheMode>,
    },
    /// `type=s3,region=<r>,bucket=<b>[,endpoint_url=<e>][,name=<n>][,mode=<mode>]`
    /// — maps 1:1 from a sui [`BackendConfig::S3`].
    S3 {
        region: String,
        bucket: String,
        #[serde(default)]
        endpoint_url: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        mode: Option<CacheMode>,
    },
    /// `type=local,dest=<path>` (export) / `src=<path>` (import) — maps from a
    /// sui [`BackendConfig::Local`].
    Local { path: String },
    /// `type=inline` — export the cache into the image itself (export-only).
    Inline,
}

impl CacheEndpoint {
    /// Bridge a sui storage-backend config into the `BuildKit` cache endpoint
    /// that fronts it.
    ///
    /// - [`BackendConfig::S3`] → an [`S3`](CacheEndpoint::S3) endpoint (direct).
    /// - [`BackendConfig::Local`] → a [`Local`](CacheEndpoint::Local) endpoint.
    /// - [`BackendConfig::Redis`] / [`BackendConfig::Pg`] /
    ///   [`BackendConfig::Tiered`] → a store `BuildKit` cannot speak directly;
    ///   these MUST be fronted by a registry ref, so they require
    ///   `registry_front` to be supplied. Absent it, this is a typed error, not
    ///   a silent wrong wire.
    ///
    /// # Errors
    ///
    /// Returns [`CacheFrontError::RequiresRegistryFront`] when a
    /// Redis/Pg/Tiered store is given without a registry ref to front it.
    pub fn from_backend_config(
        config: &BackendConfig,
        registry_front: Option<&str>,
        mode: Option<CacheMode>,
    ) -> Result<Self, CacheFrontError> {
        match config {
            BackendConfig::Local { path } => {
                Ok(CacheEndpoint::Local { path: path.display().to_string() })
            }
            BackendConfig::S3 { bucket, region, endpoint } => Ok(CacheEndpoint::S3 {
                region: region.clone(),
                bucket: bucket.clone(),
                endpoint_url: endpoint.clone(),
                name: None,
                mode,
            }),
            BackendConfig::Redis { .. } | BackendConfig::Pg { .. } | BackendConfig::Tiered { .. } => {
                registry_front.map_or_else(
                    || Err(CacheFrontError::RequiresRegistryFront),
                    |r#ref| Ok(CacheEndpoint::Registry { r#ref: r#ref.to_string(), mode }),
                )
            }
        }
    }
}

impl fmt::Display for CacheEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheEndpoint::Registry { r#ref: reference, mode } => {
                write!(f, "type=registry,ref={reference}")?;
                if let Some(m) = mode {
                    write!(f, ",mode={}", m.as_str())?;
                }
                Ok(())
            }
            CacheEndpoint::S3 { region, bucket, endpoint_url, name, mode } => {
                write!(f, "type=s3,region={region},bucket={bucket}")?;
                if let Some(e) = endpoint_url {
                    write!(f, ",endpoint_url={e}")?;
                }
                if let Some(n) = name {
                    write!(f, ",name={n}")?;
                }
                if let Some(m) = mode {
                    write!(f, ",mode={}", m.as_str())?;
                }
                Ok(())
            }
            CacheEndpoint::Local { path } => write!(f, "type=local,dest={path}"),
            CacheEndpoint::Inline => f.write_str("type=inline"),
        }
    }
}

/// The cache front wired onto a builder: the import endpoints (`--cache-from`)
/// and export endpoints (`--cache-to`). Both are optional — an empty front is a
/// builder set up for native multi-arch with no layer cache.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CacheFront {
    /// `--cache-from` endpoints (read-through: seen layers answered warm).
    #[serde(default)]
    pub from: Vec<CacheEndpoint>,
    /// `--cache-to` endpoints (write-through: built layers warmed for next time).
    #[serde(default)]
    pub to: Vec<CacheEndpoint>,
}

impl CacheFront {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.from.is_empty() && self.to.is_empty()
    }
}

/// Errors constructing a cache front.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CacheFrontError {
    #[error(
        "a Redis/Postgres/Tiered sui store is not a wire BuildKit speaks — it must be \
         fronted by a registry ref (BuildKitCacheFront); supply `registry_front`"
    )]
    RequiresRegistryFront,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn registry_endpoint_renders_with_mode() {
        let ep = CacheEndpoint::Registry {
            r#ref: "ghcr.io/pleme-io/camada:buildcache".to_string(),
            mode: Some(CacheMode::Max),
        };
        assert_eq!(ep.to_string(), "type=registry,ref=ghcr.io/pleme-io/camada:buildcache,mode=max");
    }

    #[test]
    fn s3_endpoint_maps_from_sui_backend_config() {
        let cfg = BackendConfig::S3 {
            bucket: "sui-camada".to_string(),
            region: "us-east-1".to_string(),
            endpoint: Some("https://s3.example".to_string()),
        };
        let ep = CacheEndpoint::from_backend_config(&cfg, None, Some(CacheMode::Max)).unwrap();
        assert_eq!(
            ep.to_string(),
            "type=s3,region=us-east-1,bucket=sui-camada,endpoint_url=https://s3.example,mode=max"
        );
    }

    #[test]
    fn local_endpoint_maps_from_sui_backend_config() {
        let cfg = BackendConfig::Local { path: PathBuf::from("/var/cache/sui") };
        let ep = CacheEndpoint::from_backend_config(&cfg, None, None).unwrap();
        assert_eq!(ep.to_string(), "type=local,dest=/var/cache/sui");
    }

    #[test]
    fn tiered_store_requires_a_registry_front() {
        let cfg = BackendConfig::Tiered {
            l1: Box::new(BackendConfig::Redis { url: "redis://x".to_string(), ttl_secs: None }),
            l2: Box::new(BackendConfig::Pg { url: "postgres://x".to_string(), max_conns: 8 }),
            l3: Box::new(BackendConfig::S3 {
                bucket: "b".to_string(),
                region: "r".to_string(),
                endpoint: None,
            }),
            write_policy: sui_cache::storage::WritePolicy::default(),
        };
        // Without a registry front → typed error (never a silent wrong wire).
        assert_eq!(
            CacheEndpoint::from_backend_config(&cfg, None, None).unwrap_err(),
            CacheFrontError::RequiresRegistryFront
        );
        // With one → the BuildKitCacheFront registry endpoint.
        let ep = CacheEndpoint::from_backend_config(
            &cfg,
            Some("ghcr.io/pleme-io/camada:buildcache"),
            Some(CacheMode::Max),
        )
        .unwrap();
        assert!(matches!(ep, CacheEndpoint::Registry { .. }));
    }
}
