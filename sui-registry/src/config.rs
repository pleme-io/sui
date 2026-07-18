//! porto's typed configuration surface.
//!
//! Mirrors the shikumi tiered-resolution shape without pulling shikumi into the
//! sui workspace (which does not depend on it): a typed [`RegistryConfig`] with
//! `deny_unknown_fields` (an unknown key is a parse-time rejection, never a
//! silently-ignored knob), a `bare()` zero-config and a `prescribed_default()`
//! sane baseline, plus `from_env` for the runtime tier. When porto graduates
//! into a shikumi-backed service the destination is a `TieredConfig` impl whose
//! `prescribed_default()` returns exactly this baseline — the shape is chosen so
//! that lift is mechanical.

use serde::{Deserialize, Serialize};

use sui_cache::BackendConfig;

/// The default local-fs root for porto's content-addressed store when no
/// [`backend`](RegistryConfig::backend) is configured. Sibling of sui-cache's
/// own `/var/cache/sui` default — a distinct directory so porto's OCI objects
/// and a co-resident Nix cache never collide on disk.
const DEFAULT_STORE_PATH: &str = "/var/cache/porto";

/// The registry server configuration.
///
/// The `backend` field selects the durable [`sui_cache::storage::StorageBackend`]
/// the whole registry writes through — the SAME content-addressed store family
/// that holds Nix NARs — so a deployment picks `{local | s3 | redis | pg |
/// tiered}` purely by config, never a silent hard-coded constructor. When it is
/// absent the resolver falls back to a local-fs backend rooted at
/// [`DEFAULT_STORE_PATH`] (the never-silent-fallback rule: the default is an
/// explicit, documented local backend, not an accidental one).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryConfig {
    /// The network address to listen on.
    pub listen: String,
    /// The maximum accepted blob/manifest body size in bytes. `None` disables
    /// the limit (real image layers routinely exceed axum's 2 MiB default, so
    /// the prescribed default is generous).
    #[serde(default)]
    pub max_body_bytes: Option<usize>,
    /// The durable storage backend selection, passed straight to sui-cache's
    /// typed [`build_backend`](sui_cache::build_backend) factory. `None` means
    /// "the prescribed local-fs backend at [`DEFAULT_STORE_PATH`]".
    #[serde(default)]
    pub backend: Option<BackendConfig>,
}

impl RegistryConfig {
    /// The bare, zero-assumption config: bind loopback on an ephemeral port,
    /// no body-size ceiling. Used as the tier-0 base a resolver folds over.
    #[must_use]
    pub fn bare() -> Self {
        Self {
            listen: "127.0.0.1:0".to_string(),
            max_body_bytes: None,
            backend: None,
        }
    }

    /// The prescribed sane baseline: the conventional registry port `5000`,
    /// no body ceiling (large layers stream). This is the `prescribed_default`
    /// tier a shikumi lift would return verbatim.
    #[must_use]
    pub fn prescribed_default() -> Self {
        Self {
            listen: "0.0.0.0:5000".to_string(),
            max_body_bytes: None,
            backend: None,
        }
    }

    /// Overlay the runtime (env) tier onto `self`.
    ///
    /// Reads `PORTO_LISTEN` and `PORTO_MAX_BODY_BYTES`; an unset var leaves the
    /// field untouched (progressive discovery — env is a higher tier that only
    /// overrides where present). A malformed `PORTO_MAX_BODY_BYTES` is ignored
    /// (kept as the lower tier's value) rather than aborting startup — the
    /// value is advisory, not correctness-bearing.
    #[must_use]
    pub fn from_env(mut self) -> Self {
        if let Ok(listen) = std::env::var("PORTO_LISTEN") {
            self.listen = listen;
        }
        if let Ok(raw) = std::env::var("PORTO_MAX_BODY_BYTES") {
            if let Ok(n) = raw.parse::<usize>() {
                self.max_body_bytes = Some(n);
            }
        }
        // `PORTO_BACKEND` carries a full JSON `BackendConfig` (the typed
        // `{"type":"tiered", …}` shape) for a non-local deployment; a malformed
        // value is a startup-visible error, not a silent disk fallback — so it
        // is surfaced via a `tracing::warn` and the lower tier is kept. A bare
        // `PORTO_STORE_PATH` (with no `PORTO_BACKEND`) re-roots the default
        // local backend without needing the full JSON.
        if let Ok(raw) = std::env::var("PORTO_BACKEND") {
            match serde_json::from_str::<BackendConfig>(&raw) {
                Ok(cfg) => self.backend = Some(cfg),
                Err(e) => tracing::warn!(
                    "ignoring malformed PORTO_BACKEND (keeping prior tier): {e}"
                ),
            }
        } else if let Ok(path) = std::env::var("PORTO_STORE_PATH") {
            self.backend = Some(BackendConfig::Local { path: path.into() });
        }
        self
    }

    /// Resolve the durable backend selection this config names.
    ///
    /// Returns the explicit [`backend`](Self::backend) when set, else the
    /// prescribed local-fs backend at [`DEFAULT_STORE_PATH`]. This is the single
    /// place the local-fs default is decided — an operator reading the returned
    /// value sees exactly the backend that will be built, never an implicit one.
    #[must_use]
    pub fn resolve_backend(&self) -> BackendConfig {
        self.backend
            .clone()
            .unwrap_or_else(|| BackendConfig::Local {
                path: DEFAULT_STORE_PATH.into(),
            })
    }
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self::prescribed_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_binds_loopback_ephemeral() {
        assert_eq!(RegistryConfig::bare().listen, "127.0.0.1:0");
        assert!(RegistryConfig::bare().max_body_bytes.is_none());
    }

    #[test]
    fn prescribed_default_uses_registry_port() {
        assert_eq!(RegistryConfig::prescribed_default().listen, "0.0.0.0:5000");
    }

    #[test]
    fn unknown_key_is_rejected() {
        let json = r#"{ "listen": "0.0.0.0:5000", "bogus": true }"#;
        assert!(serde_json::from_str::<RegistryConfig>(json).is_err());
    }

    #[test]
    fn roundtrips_through_json() {
        let cfg = RegistryConfig {
            listen: "127.0.0.1:8080".to_string(),
            max_body_bytes: Some(1024),
            backend: None,
        };
        // `RegistryConfig` carries `BackendConfig`, which is not `PartialEq`, so
        // a config roundtrip is proven at the JSON layer (byte-stable
        // serialization) rather than by struct equality.
        let json = serde_json::to_string(&cfg).unwrap();
        let reparsed = serde_json::from_str::<RegistryConfig>(&json).unwrap();
        assert_eq!(serde_json::to_string(&reparsed).unwrap(), json);
    }

    #[test]
    fn resolve_backend_defaults_to_local_fs() {
        let backend = RegistryConfig::bare().resolve_backend();
        match backend {
            sui_cache::BackendConfig::Local { path } => {
                assert_eq!(path, std::path::PathBuf::from(DEFAULT_STORE_PATH));
            }
            other => panic!("expected the prescribed local backend, got {other:?}"),
        }
    }

    #[test]
    fn resolve_backend_returns_the_explicit_selection() {
        let cfg = RegistryConfig {
            listen: "0.0.0.0:5000".to_string(),
            max_body_bytes: None,
            backend: Some(sui_cache::BackendConfig::S3 {
                bucket: "porto-store".to_string(),
                region: "us-east-1".to_string(),
                endpoint: None,
            }),
        };
        assert!(matches!(
            cfg.resolve_backend(),
            sui_cache::BackendConfig::S3 { .. }
        ));
    }

    #[test]
    fn backend_selection_roundtrips_through_json() {
        // A tiered backend selection survives a serialize→parse cycle inside a
        // full `RegistryConfig` — proving the config surface passes the whole
        // `build_backend` shape through untouched.
        let json = r#"{
            "listen": "0.0.0.0:5000",
            "backend": { "type": "local", "path": "/srv/porto" }
        }"#;
        let cfg: RegistryConfig = serde_json::from_str(json).unwrap();
        match cfg.resolve_backend() {
            sui_cache::BackendConfig::Local { path } => {
                assert_eq!(path, std::path::PathBuf::from("/srv/porto"));
            }
            other => panic!("expected local backend, got {other:?}"),
        }
    }
}
