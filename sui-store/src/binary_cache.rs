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
            content_address: info.ca.clone(),
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
    use crate::http::{HttpError, HttpResponse};

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

    // ── MockHttpClient (local to binary_cache tests) ─────────

    struct MockHttpClient {
        responses: std::collections::HashMap<String, HttpResponse>,
    }

    impl MockHttpClient {
        fn new() -> Self {
            Self {
                responses: std::collections::HashMap::new(),
            }
        }
        fn with_response(mut self, url: &str, resp: HttpResponse) -> Self {
            self.responses.insert(url.to_string(), resp);
            self
        }
    }

    #[async_trait::async_trait]
    impl HttpClient for MockHttpClient {
        async fn get(
            &self,
            url: &str,
            _h: &[(&str, &str)],
        ) -> Result<HttpResponse, HttpError> {
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| HttpError::Request(format!("no mock: {url}")))
        }
        async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, HttpError> {
            Ok(self.get(url, &[]).await?.body.into_bytes())
        }
    }

    // Valid NarInfo text for mock responses.
    const MOCK_NARINFO: &str = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References: 3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37-8
Deriver: abc.drv
Sig: cache.nixos.org-1:sig==
";

    fn hello_store_path() -> StorePath {
        StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap()
    }

    // ── fetch_narinfo with valid response ────────────────────

    #[tokio::test]
    async fn fetch_narinfo_valid_response() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: MOCK_NARINFO.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        let narinfo = store
            .fetch_narinfo("sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6")
            .await
            .unwrap();
        assert!(narinfo.is_some());
        let info = narinfo.unwrap();
        assert_eq!(info.nar_size, 5000);
        assert_eq!(info.references.len(), 1);
        assert!(info
            .store_path
            .contains("hello-2.12.1"));
    }

    // ── fetch_narinfo with 404 ──────────────────────────────

    #[tokio::test]
    async fn fetch_narinfo_404_returns_none() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/nonexistenthash000000000000000000.narinfo",
            HttpResponse {
                status: 404,
                body: "not found".to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        let narinfo = store
            .fetch_narinfo("nonexistenthash000000000000000000")
            .await
            .unwrap();
        assert!(narinfo.is_none());
    }

    // ── fetch_narinfo with HTTP error status ────────────────

    #[tokio::test]
    async fn fetch_narinfo_500_returns_error() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/abc00000000000000000000000000000.narinfo",
            HttpResponse {
                status: 500,
                body: "server error".to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        let result = store
            .fetch_narinfo("abc00000000000000000000000000000")
            .await;
        assert!(result.is_err());
    }

    // ── query_path_info through Store trait ──────────────────

    #[tokio::test]
    async fn query_path_info_via_store_trait() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: MOCK_NARINFO.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        let path_info = store
            .query_path_info(&hello_store_path())
            .await
            .unwrap();
        assert!(path_info.is_some());
        let info = path_info.unwrap();
        assert_eq!(info.path, "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1");
        assert_eq!(info.nar_hash, "sha256:bbb");
        assert_eq!(info.nar_size, 5000);
        assert_eq!(info.signatures, vec!["cache.nixos.org-1:sig=="]);
    }

    // ── is_valid_path through Store trait ─────────────────────

    #[tokio::test]
    async fn is_valid_path_true_when_exists() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: MOCK_NARINFO.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        assert!(store.is_valid_path(&hello_store_path()).await.unwrap());
    }

    #[tokio::test]
    async fn is_valid_path_false_when_missing() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 404,
                body: String::new(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        assert!(!store.is_valid_path(&hello_store_path()).await.unwrap());
    }

    // ── query_all_valid_paths is unsupported ─────────────────

    #[tokio::test]
    async fn query_all_valid_paths_unsupported() {
        let client = MockHttpClient::new();
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        let result = store.query_all_valid_paths().await;
        assert!(result.is_err());
    }

    // ── narinfo_to_path_info preserves content_address ───────

    #[test]
    fn narinfo_to_path_info_preserves_ca() {
        let narinfo = NarInfo {
            store_path: "/nix/store/abc-src.tar.gz".to_string(),
            url: "nar/abc.nar".to_string(),
            compression: "none".to_string(),
            file_hash: "sha256:fff".to_string(),
            file_size: 500,
            nar_hash: "sha256:eee".to_string(),
            nar_size: 1000,
            references: vec![],
            deriver: None,
            signatures: vec![],
            ca: Some("fixed:out:r:sha256:deadbeef".to_string()),
        };
        let info = BinaryCacheStore::narinfo_to_path_info(&narinfo);
        assert_eq!(
            info.content_address,
            Some("fixed:out:r:sha256:deadbeef".to_string())
        );
        assert_eq!(info.registration_time, 0);
    }

    // ── store_path_hash with short basename ──────────────────

    #[test]
    fn store_path_hash_extracts_exactly_32_chars() {
        let path = StorePath::from_absolute_path(
            "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-hierarchical-0.1.0.1",
        )
        .unwrap();
        let hash = BinaryCacheStore::store_path_hash(&path);
        assert_eq!(hash.len(), 32);
        assert_eq!(hash, "00bgd045z0d4icpbc2yyz4gx48ak44la");
    }

    // ── base_url trailing slash normalization ─────────────────

    #[test]
    fn base_url_trailing_slashes_stripped() {
        let client = MockHttpClient::new();
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org///",
            vec![],
            Box::new(client),
        );
        // Only one trailing slash should be stripped by trim_end_matches
        // but the URL should not have a trailing slash
        assert!(!store.base_url.ends_with('/'));
    }
}
