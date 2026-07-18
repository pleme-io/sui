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

/// The registry server configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryConfig {
    /// The network address to listen on.
    pub listen: String,
    /// The maximum accepted blob/manifest body size in bytes. `None` disables
    /// the limit (real image layers routinely exceed axum's 2 MiB default, so
    /// the prescribed default is generous).
    #[serde(default)]
    pub max_body_bytes: Option<usize>,
}

impl RegistryConfig {
    /// The bare, zero-assumption config: bind loopback on an ephemeral port,
    /// no body-size ceiling. Used as the tier-0 base a resolver folds over.
    #[must_use]
    pub fn bare() -> Self {
        Self {
            listen: "127.0.0.1:0".to_string(),
            max_body_bytes: None,
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
        self
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
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: RegistryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cfg);
    }
}
