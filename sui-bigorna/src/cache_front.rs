//! `BuildKitCacheFront` — the store-backed cache endpoint fronting the same
//! builder bigorna sets up, so native-multi-arch and layer caching land through
//! ONE integration point.
//!
//! `BuildKit` already speaks a cache import/export protocol (`--cache-from` /
//! `--cache-to`). bigorna does not re-implement it — it *points* it at an
//! endpoint that fronts the shipped sui tiered content-addressed store
//! (`sui_castore::StorageBackend`: Redis L1 → Postgres L2 → object
//! store L3). The wire `BuildKit` talks is one of the protocols it natively
//! understands (`registry` / `s3` / `local` / `inline`); the sui store sits
//! behind that wire. The map from a sui [`BackendConfig`] to a `BuildKit` cache
//! endpoint is [`CacheEndpoint::from_backend_config`].
//!
//! Every endpoint token (`type=registry,ref=…,mode=max`) is rendered by a
//! [`std::fmt::Display`] impl — the one sanctioned typed-emission surface — not
//! a `format!()` at a call site.
//!
//! ## Import vs export are not always the same token
//!
//! `registry` / `s3` / `inline` are symmetric — `BuildKit` accepts the exact
//! same `type=<wire>,…` token on both `--cache-from` (import) and `--cache-to`
//! (export). The `local` wire is **not**: an export writes `type=local,dest=…`
//! and an import reads `type=local,src=…`. Passing a `dest=` token to
//! `--cache-from` fails the whole build (`local cache importer requires src`),
//! so the token a [`CacheEndpoint`] renders depends on the [`CacheDirection`]
//! it is used in. [`CacheEndpoint::render`] takes that direction; the
//! [`std::fmt::Display`] impl renders the export form (the historical
//! behavior, and the correct token for every symmetric wire and for
//! `--cache-to`).

use std::fmt;

use serde::{Deserialize, Serialize};
use sui_cache::config::BackendConfig;

/// Which side of the cache front an endpoint token is rendered for. Only the
/// `local` wire renders differently per direction (`dest=` export vs `src=`
/// import); every other wire renders identically, so a symmetric wire ignores
/// this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDirection {
    /// `--cache-from` — read cached layers. `local` reads `src=<path>`.
    Import,
    /// `--cache-to` — write layers back. `local` writes `dest=<path>`.
    Export,
}

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

    /// Render the `type=<wire>,…` token for the given [`CacheDirection`].
    ///
    /// Every wire but `local` renders identically for import and export; the
    /// `local` wire renders `src=<path>` on [`CacheDirection::Import`] and
    /// `dest=<path>` on [`CacheDirection::Export`] — passing the wrong one to
    /// `--cache-from` makes `BuildKit` fail with `local cache importer requires
    /// src`.
    ///
    /// # Errors
    ///
    /// This never errors; it returns [`fmt::Result`] only to compose with the
    /// [`fmt::Display`] impl.
    pub fn render(&self, direction: CacheDirection, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
            CacheEndpoint::Local { path } => match direction {
                CacheDirection::Import => write!(f, "type=local,src={path}"),
                CacheDirection::Export => write!(f, "type=local,dest={path}"),
            },
            CacheEndpoint::Inline => f.write_str("type=inline"),
        }
    }

    /// Render this endpoint as a `--cache-from` (import) token — a small typed
    /// wrapper over [`render`](CacheEndpoint::render) so a call site never has
    /// to name a `Formatter`.
    #[must_use]
    pub fn to_import_token(&self) -> String {
        DirectedEndpoint { endpoint: self, direction: CacheDirection::Import }.to_string()
    }
}

/// An endpoint bound to a direction, so its [`Display`] renders the correct
/// (import vs export) token. The typed-emission surface a directional call
/// site formats through.
struct DirectedEndpoint<'a> {
    endpoint: &'a CacheEndpoint,
    direction: CacheDirection,
}

impl fmt::Display for DirectedEndpoint<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.endpoint.render(self.direction, f)
    }
}

impl fmt::Display for CacheEndpoint {
    /// The **export** form (`--cache-to`). For `local` this is `dest=<path>`;
    /// every other wire is direction-agnostic. Use
    /// [`to_import_token`](CacheEndpoint::to_import_token) for a `--cache-from`
    /// token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.render(CacheDirection::Export, f)
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
        // Display == the export form (`--cache-to`).
        assert_eq!(ep.to_string(), "type=local,dest=/var/cache/sui");
    }

    #[test]
    fn local_endpoint_is_asymmetric_import_uses_src_export_uses_dest() {
        // Regression: buildx rejects `--cache-from type=local,dest=…` with
        // `local cache importer requires src`. The import token MUST be `src=`.
        let ep = CacheEndpoint::Local { path: "/var/cache/sui".to_string() };
        assert_eq!(ep.to_import_token(), "type=local,src=/var/cache/sui");
        assert_eq!(ep.to_string(), "type=local,dest=/var/cache/sui");
        assert_ne!(
            ep.to_import_token(),
            ep.to_string(),
            "a local endpoint's import and export tokens differ (src vs dest)",
        );
    }

    #[test]
    fn symmetric_wires_render_identically_in_both_directions() {
        // registry / s3 / inline take the same token on --cache-from and
        // --cache-to; only `local` is direction-sensitive.
        for ep in [
            CacheEndpoint::Registry {
                r#ref: "ghcr.io/pleme-io/camada:buildcache".to_string(),
                mode: Some(CacheMode::Max),
            },
            CacheEndpoint::S3 {
                region: "us-east-1".to_string(),
                bucket: "sui-camada".to_string(),
                endpoint_url: None,
                name: None,
                mode: None,
            },
            CacheEndpoint::Inline,
        ] {
            assert_eq!(
                ep.to_import_token(),
                ep.to_string(),
                "symmetric wire {ep:?} must render the same import + export token",
            );
        }
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
            write_policy: sui_cache::WritePolicy::default(),
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
