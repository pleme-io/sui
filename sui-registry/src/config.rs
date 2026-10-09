//! porto's typed configuration surface — a [`shikumi::TieredConfig`].
//!
//! [`RegistryConfig`] is resolved the fleet-standard way: the `PORTO_TIER`
//! environment variable selects the tier (`bare`, `default`, or a path to a
//! YAML file laid over the prescribed default), then the `PORTO_*` runtime
//! overrides apply. The type also derives [`schemars::JsonSchema`]; `porto
//! config-schema` prints that schema, which is the input substrate's
//! `types.jsonSchema` turns into NixOS module options — so the module is
//! generated from this file, never restated by hand.
//!
//! ## One deliberate divergence from shikumi's default resolution
//!
//! `shikumi::TieredConfig::resolve_tier` falls back to `prescribed_default()`
//! (with a `warn!`) when the custom-tier YAML is unreadable or malformed. For a
//! registry whose config names the layouts it serves, that fallback is a
//! silent wrong answer: porto would start, bind, and serve an empty catalog.
//! [`RegistryConfig::load`] therefore parses the custom tier STRICTLY and
//! returns a typed [`ConfigError`]; a bad config is a non-zero exit, never a
//! running server with a different config than the one written.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use shikumi::{ConfigTier, TieredConfig};

use sui_castore::BackendConfig;

/// The shikumi tier-selector environment variable (fleet convention
/// `<APP>_TIER`): unset/`default` → prescribed default, `bare` → the floor,
/// anything else → a path to a YAML overlay.
pub const PORTO_TIER_ENV: &str = "PORTO_TIER";

/// The default local-fs root for porto's content-addressed store when no
/// [`backend`](RegistryConfig::backend) is configured AND no layouts are
/// mounted. Sibling of sui-cache's own `/var/cache/sui` default — a distinct
/// directory so porto's OCI objects and a co-resident Nix cache never collide.
const DEFAULT_STORE_PATH: &str = "/var/cache/porto";

/// How a mounted layout's blobs are proven to match their digests.
///
/// Either way a corrupted store path is REFUSED, never served: the choice is
/// only *when* the bytes are hashed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BlobVerification {
    /// Hash every referenced blob at startup; a mismatch is a startup error.
    /// Right for charts (small) and the prescribed default.
    #[default]
    Eager,
    /// Hash a blob on its first read and cache the verdict; a mismatch is
    /// a refused request (500, logged). For multi-GB image layouts where
    /// hashing everything at boot would stall the server.
    Lazy,
}

/// One read-only OCI image layout mounted into a repository namespace.
///
/// Every manifest in the layout's `index.json` is served under `repository`.
/// Its `org.opencontainers.image.ref.name` annotation decides the tag:
///
/// - `"<tag>"` → `repository:<tag>`
/// - `"<path>:<tag>"` → `repository/<path>:<tag>` — so ONE layout can carry a
///   whole chart repo: `repository: pleme-io/charts` + ref
///   `"pleme-porto:0.1.0"` is served at
///   `oci://<host>/pleme-io/charts/pleme-porto:0.1.0`.
///
/// Several mounts may contribute to one repository; two mounts that assign
/// the same tag to DIFFERENT digests are a startup error, never last-wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LayoutMount {
    /// The repository namespace the layout is served under (OCI name grammar).
    pub repository: String,
    /// The OCI image layout directory (holds `oci-layout`, `index.json`,
    /// `blobs/`), normally an immutable `/nix/store` path.
    pub layout: PathBuf,
    /// When this layout's blobs are digest-verified.
    #[serde(default)]
    pub verify: BlobVerification,
}

/// The registry server configuration.
///
/// `#[serde(default)]` makes a YAML file an overlay (absent fields come from
/// [`Default`], which IS [`TieredConfig::prescribed_default`]);
/// `deny_unknown_fields` makes a misspelt key a parse-time rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RegistryConfig {
    /// The network address to listen on.
    pub listen: String,
    /// The maximum accepted blob/manifest body size in bytes. `None` disables
    /// the limit (real image layers routinely exceed axum's 2 MiB default).
    pub max_body_bytes: Option<usize>,
    /// The durable, WRITABLE storage backend, passed straight to
    /// [`sui_castore::build_backend`]. See [`RegistryConfig::resolve_backend`]
    /// for what absence means.
    pub backend: Option<BackendConfig>,
    /// Read-only OCI image layouts to serve. Their repositories refuse every
    /// write with `DENIED`; their tags come from each `index.json`, so they
    /// survive a restart by construction.
    pub layouts: Vec<LayoutMount>,
}

/// A config that could not be loaded. Every variant names the file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The custom-tier file could not be read.
    #[error("cannot read porto config {path}: {source}")]
    Read {
        /// The file named by `PORTO_TIER`.
        path: PathBuf,
        /// The I/O failure.
        source: std::io::Error,
    },
    /// The custom-tier file is not a valid [`RegistryConfig`].
    #[error("invalid porto config {path}: {source}")]
    Parse {
        /// The file named by `PORTO_TIER`.
        path: PathBuf,
        /// The YAML/shape failure (unknown key, wrong type, …).
        source: serde_yaml_ng::Error,
    },
}

impl TieredConfig for RegistryConfig {
    /// The zero-assumption floor: loopback on an ephemeral port, no body
    /// ceiling, no explicit backend, nothing mounted.
    fn bare() -> Self {
        Self {
            listen: "127.0.0.1:0".to_string(),
            max_body_bytes: None,
            backend: None,
            layouts: Vec::new(),
        }
    }

    /// The prescribed baseline: the conventional registry port `5000`.
    fn prescribed_default() -> Self {
        Self {
            listen: "0.0.0.0:5000".to_string(),
            ..Self::bare()
        }
    }
}

impl Default for RegistryConfig {
    /// Delegates to [`TieredConfig::prescribed_default`] so `Default` and the
    /// tiered resolution can never describe two different registries.
    fn default() -> Self {
        <Self as TieredConfig>::prescribed_default()
    }
}

impl RegistryConfig {
    /// The bare tier (inherent alias of [`TieredConfig::bare`] so callers need
    /// not import the trait).
    #[must_use]
    pub fn bare() -> Self {
        <Self as TieredConfig>::bare()
    }

    /// The prescribed tier (inherent alias of
    /// [`TieredConfig::prescribed_default`]).
    #[must_use]
    pub fn prescribed_default() -> Self {
        <Self as TieredConfig>::prescribed_default()
    }

    /// Resolve the config the fleet-standard way: `PORTO_TIER` selects the
    /// tier, then the `PORTO_*` runtime overrides apply ([`Self::from_env`]).
    ///
    /// # Errors
    ///
    /// A custom-tier file that is unreadable or not a valid config — see the
    /// module docs for why this does not fall back like shikumi's default.
    pub fn load() -> Result<Self, ConfigError> {
        Ok(Self::load_tier(ConfigTier::from_env(PORTO_TIER_ENV))?.from_env())
    }

    /// Resolve one tier, strictly for the custom (file) tier.
    ///
    /// # Errors
    ///
    /// See [`Self::load`].
    pub fn load_tier(tier: ConfigTier) -> Result<Self, ConfigError> {
        match tier {
            ConfigTier::Custom(path) => Self::from_yaml_file(&path),
            other => Ok(<Self as TieredConfig>::resolve_tier(other)),
        }
    }

    /// Parse a YAML overlay file strictly (unknown keys rejected; absent keys
    /// take the prescribed default).
    ///
    /// # Errors
    ///
    /// See [`Self::load`].
    pub fn from_yaml_file(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        serde_yaml_ng::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// The JSON Schema of this config (draft 2020-12, from schemars) — the
    /// input for a generated NixOS/HM option surface.
    #[must_use]
    pub fn json_schema() -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(RegistryConfig)).unwrap_or_default()
    }

    /// Overlay the runtime (env) tier onto `self`.
    ///
    /// Reads `PORTO_LISTEN` and `PORTO_MAX_BODY_BYTES`; an unset var leaves the
    /// field untouched. A malformed `PORTO_MAX_BODY_BYTES` is ignored (the
    /// value is advisory, not correctness-bearing). `PORTO_BACKEND` carries a
    /// full JSON [`BackendConfig`]; a bare `PORTO_STORE_PATH` re-roots a local
    /// backend. Layout mounts have no env form: they are structured data and
    /// belong in the config file.
    #[must_use]
    pub fn from_env(mut self) -> Self {
        if let Ok(listen) = std::env::var("PORTO_LISTEN") {
            self.listen = listen;
        }
        if let Ok(raw) = std::env::var("PORTO_MAX_BODY_BYTES")
            && let Ok(n) = raw.parse::<usize>()
        {
            self.max_body_bytes = Some(n);
        }
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

    /// Resolve the WRITABLE backend this config names — the single place the
    /// local-fs default is decided:
    ///
    /// | `backend` | `layouts`  | result                                   |
    /// |-----------|------------|------------------------------------------|
    /// | set       | any        | that backend                             |
    /// | absent    | empty      | local fs at `/var/cache/porto`           |
    /// | absent    | non-empty  | `None` — a pure read-only mirror         |
    ///
    /// The third row is why this returns `Option`: a node that only serves
    /// Nix-built layouts must not silently grow a writable disk store nobody
    /// asked for.
    #[must_use]
    pub fn resolve_backend(&self) -> Option<BackendConfig> {
        match (&self.backend, self.layouts.is_empty()) {
            (Some(b), _) => Some(b.clone()),
            (None, true) => Some(BackendConfig::Local {
                path: DEFAULT_STORE_PATH.into(),
            }),
            (None, false) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_binds_loopback_ephemeral() {
        let bare = RegistryConfig::bare();
        assert_eq!(bare.listen, "127.0.0.1:0");
        assert!(bare.max_body_bytes.is_none());
        assert!(bare.backend.is_none());
        assert!(bare.layouts.is_empty());
    }

    #[test]
    fn prescribed_default_uses_registry_port() {
        assert_eq!(RegistryConfig::prescribed_default().listen, "0.0.0.0:5000");
        assert_eq!(RegistryConfig::default(), RegistryConfig::prescribed_default());
    }

    #[test]
    fn unknown_key_is_rejected() {
        let json = r#"{ "listen": "0.0.0.0:5000", "bogus": true }"#;
        assert!(serde_json::from_str::<RegistryConfig>(json).is_err());
        let yaml = "layouts:\n  - repository: a\n    layout: /x\n    bogus: 1\n";
        assert!(serde_yaml_ng::from_str::<RegistryConfig>(yaml).is_err());
    }

    #[test]
    fn roundtrips_through_json() {
        let cfg = RegistryConfig {
            listen: "127.0.0.1:8080".to_string(),
            max_body_bytes: Some(1024),
            backend: None,
            layouts: vec![LayoutMount {
                repository: "pleme-io/charts".into(),
                layout: "/nix/store/x-charts".into(),
                verify: BlobVerification::Lazy,
            }],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<RegistryConfig>(&json).unwrap(), cfg);
    }

    #[test]
    fn resolve_backend_defaults_to_local_fs_without_layouts() {
        match RegistryConfig::bare().resolve_backend() {
            Some(BackendConfig::Local { path }) => {
                assert_eq!(path, PathBuf::from(DEFAULT_STORE_PATH));
            }
            other => panic!("expected the prescribed local backend, got {other:?}"),
        }
    }

    #[test]
    fn a_layout_only_config_has_no_writable_backend() {
        let cfg = RegistryConfig {
            layouts: vec![LayoutMount {
                repository: "pleme-io/charts".into(),
                layout: "/nix/store/x".into(),
                verify: BlobVerification::Eager,
            }],
            ..RegistryConfig::bare()
        };
        assert_eq!(cfg.resolve_backend(), None);
    }

    #[test]
    fn resolve_backend_returns_the_explicit_selection() {
        let cfg = RegistryConfig {
            backend: Some(BackendConfig::S3 {
                bucket: "porto-store".to_string(),
                region: "us-east-1".to_string(),
                endpoint: None,
            }),
            layouts: vec![LayoutMount {
                repository: "r".into(),
                layout: "/l".into(),
                verify: BlobVerification::Eager,
            }],
            ..RegistryConfig::prescribed_default()
        };
        assert!(matches!(cfg.resolve_backend(), Some(BackendConfig::S3 { .. })));
    }

    #[test]
    fn a_yaml_overlay_changes_what_it_names_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("porto.yaml");
        std::fs::write(
            &path,
            "layouts:\n  - repository: pleme-io/charts\n    layout: /nix/store/abc-charts\n",
        )
        .unwrap();
        let cfg = RegistryConfig::load_tier(ConfigTier::Custom(path)).unwrap();
        assert_eq!(cfg.listen, "0.0.0.0:5000", "unmentioned field keeps its default");
        assert_eq!(cfg.layouts.len(), 1);
        assert_eq!(cfg.layouts[0].verify, BlobVerification::Eager);
    }

    #[test]
    fn a_malformed_custom_tier_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("porto.yaml");
        std::fs::write(&path, "layouts: [ { repository: x } ]\n").unwrap();
        assert!(matches!(
            RegistryConfig::load_tier(ConfigTier::Custom(path)),
            Err(ConfigError::Parse { .. })
        ));
        let missing = dir.path().join("absent.yaml");
        assert!(matches!(
            RegistryConfig::load_tier(ConfigTier::Custom(missing)),
            Err(ConfigError::Read { .. })
        ));
    }

    #[test]
    fn the_schema_names_every_top_level_field() {
        let schema = RegistryConfig::json_schema();
        let props = schema["properties"].as_object().unwrap();
        for field in ["listen", "max_body_bytes", "backend", "layouts"] {
            assert!(props.contains_key(field), "schema lacks {field}");
        }
    }
}
