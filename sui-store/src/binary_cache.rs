//! Binary cache store — HTTP client for cache.nixos.org, Cachix, Attic.
//!
//! Implements the NarInfo + NAR download protocol for substitution.

use sui_compat::narinfo::NarInfo;
use sui_compat::store_path::StorePath;

use crate::http::{HttpClient, ReqwestHttpClient};
use crate::traits::{PathInfo, Store, StoreError, StoreResult};

/// A read-only binary cache store accessed over HTTP.
pub struct BinaryCacheStore {
    client: Box<dyn HttpClient>,
    /// Base URL (e.g., `https://cache.nixos.org`).
    base_url: String,
    /// Trusted public keys for signature verification (`keyname:base64pubkey`).
    trusted_keys: Vec<String>,
}

impl BinaryCacheStore {
    /// Create a new binary cache client with default HTTP backend.
    pub fn new(base_url: &str, trusted_keys: Vec<String>) -> Self {
        Self {
            client: Box::new(ReqwestHttpClient::new()),
            base_url: base_url.trim_end_matches('/').to_string(),
            trusted_keys,
        }
    }

    /// Create a new binary cache client with a custom HTTP backend.
    pub fn with_http_client(
        base_url: &str,
        trusted_keys: Vec<String>,
        client: Box<dyn HttpClient>,
    ) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            trusted_keys,
        }
    }

    /// Fetch NarInfo for a store path hash.
    pub async fn fetch_narinfo(&self, hash: &str) -> StoreResult<Option<NarInfo>> {
        let url = format!("{}/{hash}.narinfo", self.base_url);

        let response = self
            .client
            .get(&url, &[("Accept", "text/x-nix-narinfo")])
            .await
            .map_err(|e| StoreError::Database(format!("HTTP error: {e}")))?;

        if response.status == 404 {
            return Ok(None);
        }

        if response.status < 200 || response.status >= 300 {
            return Err(StoreError::Database(format!(
                "HTTP {}: {}",
                response.status, url
            )));
        }

        let info = NarInfo::parse(&response.body)
            .map_err(|e| StoreError::Database(format!("NarInfo parse error: {e}")))?;

        Ok(Some(info))
    }

    /// Download a NAR file from the cache.
    pub async fn fetch_nar(&self, url_path: &str) -> StoreResult<Vec<u8>> {
        let url = format!("{}/{url_path}", self.base_url);

        self.client
            .get_bytes(&url)
            .await
            .map_err(|e| StoreError::Database(format!("HTTP error: {e}")))
    }

    /// Convert a NarInfo to our PathInfo type.
    fn narinfo_to_path_info(info: &NarInfo) -> PathInfo {
        PathInfo {
            path: info.store_path.clone(),
            nar_hash: info.nar_hash.clone(),
            nar_size: info.nar_size as i64,
            references: info.references.clone(),
            deriver: info.deriver.clone(),
            signatures: info.signatures.clone(),
            registration_time: 0,
        }
    }

    /// Get the store path hash (first 32 chars of the basename).
    fn store_path_hash(path: &StorePath) -> String {
        let basename = path.to_basename();
        basename[..32.min(basename.len())].to_string()
    }
}

#[async_trait::async_trait]
impl Store for BinaryCacheStore {
    async fn query_path_info(&self, path: &StorePath) -> StoreResult<Option<PathInfo>> {
        let hash = Self::store_path_hash(path);
        match self.fetch_narinfo(&hash).await? {
            Some(info) => Ok(Some(Self::narinfo_to_path_info(&info))),
            None => Ok(None),
        }
    }

    async fn is_valid_path(&self, path: &StorePath) -> StoreResult<bool> {
        let hash = Self::store_path_hash(path);
        Ok(self.fetch_narinfo(&hash).await?.is_some())
    }

    async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
        // Binary caches don't support listing all paths.
        Err(StoreError::Database(
            "binary cache does not support listing all paths".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_path_hash_extraction() {
        let path = StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();
        let hash = BinaryCacheStore::store_path_hash(&path);
        assert_eq!(hash, "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6");
    }

    #[test]
    fn narinfo_to_path_info_conversion() {
        let narinfo = sui_compat::narinfo::NarInfo {
            store_path: "/nix/store/abc-hello".to_string(),
            url: "nar/abc.nar.xz".to_string(),
            compression: "xz".to_string(),
            file_hash: "sha256:aaa".to_string(),
            file_size: 1000,
            nar_hash: "sha256:bbb".to_string(),
            nar_size: 5000,
            references: vec!["dep1".to_string()],
            deriver: Some("abc.drv".to_string()),
            signatures: vec!["key:sig".to_string()],
            ca: None,
        };
        let info = BinaryCacheStore::narinfo_to_path_info(&narinfo);
        assert_eq!(info.path, "/nix/store/abc-hello");
        assert_eq!(info.nar_size, 5000);
        assert_eq!(info.references.len(), 1);
    }

    #[test]
    fn with_http_client_constructor() {
        // Verify the custom client constructor works
        let client = Box::new(ReqwestHttpClient::new());
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org/",
            vec![],
            client,
        );
        assert_eq!(store.base_url, "https://cache.nixos.org");
    }
}
