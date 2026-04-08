//! Binary cache store — HTTP client for cache.nixos.org, Cachix, Attic.
//!
//! Implements the NarInfo + NAR download protocol for substitution.

// TODO(scope): NarInfo lives in sui-compat — add `impl FromStr for NarInfo`
// there so callers can use `"...".parse::<NarInfo>()` instead of `NarInfo::parse()`.
use sui_compat::narinfo::{NarInfo, NarInfoError};
use sui_compat::store_path::StorePath;

use crate::http::{HttpClient, HttpError, ReqwestHttpClient};
use crate::traits::{PathInfo, Store, StoreError, StoreResult};

/// Typed errors for binary cache operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BinaryCacheError {
    /// HTTP client returned an error (network, DNS, TLS, etc.).
    #[error("http client error: {0}")]
    HttpClient(#[from] HttpError),
    /// Server returned an unexpected (non-2xx, non-404) HTTP status.
    #[error("unexpected HTTP status {status} for {url}")]
    UnexpectedStatus {
        /// The HTTP status code received.
        status: u16,
        /// The URL that was requested.
        url: String,
    },
    /// The NarInfo response body could not be parsed.
    #[error("narinfo parse error: {0}")]
    NarInfoParse(#[from] NarInfoError),
}

impl From<BinaryCacheError> for StoreError {
    fn from(e: BinaryCacheError) -> Self {
        match &e {
            BinaryCacheError::HttpClient(_) | BinaryCacheError::UnexpectedStatus { .. } => {
                StoreError::Http(e.to_string())
            }
            BinaryCacheError::NarInfoParse(_) => StoreError::NarInfo(e.to_string()),
        }
    }
}

/// A read-only binary cache store accessed over HTTP.
pub struct BinaryCacheStore {
    client: Box<dyn HttpClient>,
    /// Base URL (e.g., `https://cache.nixos.org`).
    base_url: String,
    /// Trusted public keys for signature verification (`keyname:base64pubkey`).
    trusted_keys: Vec<String>,
}

/// Builder for [`BinaryCacheStore`].
pub struct BinaryCacheStoreBuilder {
    base_url: String,
    trusted_keys: Vec<String>,
    client: Option<Box<dyn HttpClient>>,
}

impl BinaryCacheStoreBuilder {
    /// Set the trusted public keys for signature verification.
    #[must_use]
    pub fn trusted_keys(mut self, keys: Vec<String>) -> Self {
        self.trusted_keys = keys;
        self
    }

    /// Use a custom HTTP client implementation (e.g., for testing).
    #[must_use]
    pub fn http_client(mut self, client: Box<dyn HttpClient>) -> Self {
        self.client = Some(client);
        self
    }

    /// Build the [`BinaryCacheStore`].
    #[must_use]
    pub fn build(self) -> BinaryCacheStore {
        BinaryCacheStore {
            client: self.client.unwrap_or_else(|| Box::new(ReqwestHttpClient::new())),
            base_url: self.base_url,
            trusted_keys: self.trusted_keys,
        }
    }
}

impl BinaryCacheStore {
    /// Create a builder for a binary cache store with the given base URL.
    #[must_use]
    pub fn builder(base_url: &str) -> BinaryCacheStoreBuilder {
        BinaryCacheStoreBuilder {
            base_url: base_url.trim_end_matches('/').to_string(),
            trusted_keys: Vec::new(),
            client: None,
        }
    }

    /// Create a new binary cache client with default HTTP backend.
    #[must_use]
    pub fn new(base_url: &str, trusted_keys: Vec<String>) -> Self {
        Self::builder(base_url).trusted_keys(trusted_keys).build()
    }

    /// Create a new binary cache client with a custom HTTP backend.
    #[must_use]
    pub fn with_http_client(
        base_url: &str,
        trusted_keys: Vec<String>,
        client: Box<dyn HttpClient>,
    ) -> Self {
        Self::builder(base_url)
            .trusted_keys(trusted_keys)
            .http_client(client)
            .build()
    }

    /// Fetch NarInfo for a store path hash.
    pub async fn fetch_narinfo(&self, hash: &str) -> StoreResult<Option<NarInfo>> {
        let url = format!("{}/{hash}.narinfo", self.base_url);

        let response = self
            .client
            .get(&url, &[("Accept", "text/x-nix-narinfo")])
            .await
            .map_err(BinaryCacheError::from)?;

        if response.status == 404 {
            return Ok(None);
        }

        if !response.is_success() {
            return Err(BinaryCacheError::UnexpectedStatus {
                status: response.status,
                url,
            }
            .into());
        }

        let info = NarInfo::parse(&response.body).map_err(BinaryCacheError::from)?;

        Ok(Some(info))
    }

    /// Return the base URL of this binary cache (without trailing slash).
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Return the trusted public keys used for signature verification.
    #[must_use]
    pub fn trusted_keys(&self) -> &[String] {
        &self.trusted_keys
    }

    /// Download a NAR file from the cache.
    pub async fn fetch_nar(&self, url_path: &str) -> StoreResult<Vec<u8>> {
        let url = format!("{}/{url_path}", self.base_url);

        self.client
            .get_bytes(&url)
            .await
            .map_err(BinaryCacheError::from)
            .map_err(StoreError::from)
    }

    /// Convert a NarInfo to our PathInfo type.
    ///
    /// Delegates to the [`From<&NarInfo>`](PathInfo::from) impl.
    #[cfg(test)]
    fn narinfo_to_path_info(info: &NarInfo) -> PathInfo {
        PathInfo::from(info)
    }

    /// Get the store path hash (first 32 chars of the basename).
    fn store_path_hash(path: &StorePath) -> String {
        let basename = path.to_basename();
        basename[..32.min(basename.len())].to_string()
    }

    /// Verify that a NarInfo has at least one valid signature from the trusted keys.
    ///
    /// The NarInfo fingerprint is: `1;{storePath};{narHash};{narSize};{sortedReferences}`.
    /// Each signature in the NarInfo is in `keyname:base64sig` format. Each trusted key
    /// is in `keyname:base64pubkey` format.
    ///
    /// Returns `Ok(true)` if at least one signature matches a trusted key,
    /// `Ok(false)` if no trusted keys are provided or no signatures match.
    pub fn verify_narinfo_signatures(
        narinfo: &NarInfo,
        trusted_keys: &[String],
    ) -> StoreResult<bool> {
        use sui_compat::signature::{StorePathSignature, compute_fingerprint};
        use sui_compat::hash::base64_decode;

        if trusted_keys.is_empty() {
            return Ok(false);
        }

        // Build the sorted references for the fingerprint.
        let mut sorted_refs: Vec<String> = narinfo.references.clone();
        sorted_refs.sort();

        let fingerprint = compute_fingerprint(
            &narinfo.store_path,
            &narinfo.nar_hash,
            narinfo.nar_size,
            &sorted_refs,
        );

        // Build a map of key_name -> public_key_bytes from trusted keys.
        let mut key_map: std::collections::HashMap<String, Vec<u8>> =
            std::collections::HashMap::new();
        for key_str in trusted_keys {
            if let Some((name, b64_pubkey)) = key_str.split_once(':')
                && let Ok(pubkey_bytes) = base64_decode(b64_pubkey) {
                    key_map.insert(name.to_string(), pubkey_bytes);
                }
        }

        // Check each signature against the matching trusted key.
        for sig_str in &narinfo.signatures {
            let parsed = match StorePathSignature::parse(sig_str) {
                Ok(s) => s,
                Err(_) => continue,
            };

            if let Some(pubkey_bytes) = key_map.get(&parsed.key_name)
                && pubkey_bytes.len() == 32 {
                    let pubkey: [u8; 32] = pubkey_bytes
                        .as_slice()
                        .try_into()
                        .expect("length checked");
                    if parsed.verify(&fingerprint, &pubkey).is_ok() {
                        return Ok(true);
                    }
                }
        }

        Ok(false)
    }
}

#[async_trait::async_trait]
impl Store for BinaryCacheStore {
    async fn query_path_info(&self, path: &StorePath) -> StoreResult<Option<PathInfo>> {
        let hash = Self::store_path_hash(path);
        Ok(self
            .fetch_narinfo(&hash)
            .await?
            .as_ref()
            .map(PathInfo::from))
    }

    async fn is_valid_path(&self, path: &StorePath) -> StoreResult<bool> {
        let hash = Self::store_path_hash(path);
        Ok(self.fetch_narinfo(&hash).await?.is_some())
    }

    async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
        Err(StoreError::NotSupported(
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
        // NarInfo references are bare basenames; the PathInfo conversion
        // must prefix them with the store directory.
        let narinfo = sui_compat::narinfo::NarInfo {
            store_path: "/nix/store/abc-hello".to_string(),
            url: "nar/abc.nar.xz".to_string(),
            compression: "xz".to_string(),
            file_hash: "sha256:aaa".to_string(),
            file_size: 1000,
            nar_hash: "sha256:bbb".to_string(),
            nar_size: 5000,
            references: vec![
                "3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37-8".to_string(),
            ],
            deriver: Some("abc.drv".to_string()),
            signatures: vec!["key:sig".to_string()],
            ca: None,
        };
        let info = BinaryCacheStore::narinfo_to_path_info(&narinfo);
        assert_eq!(info.path, "/nix/store/abc-hello");
        assert_eq!(info.nar_size, 5000);
        assert_eq!(info.references.len(), 1);
        assert_eq!(
            info.references[0],
            "/nix/store/3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37-8"
        );
    }

    #[test]
    fn with_http_client_constructor() {
        let client = Box::new(ReqwestHttpClient::new());
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org/",
            vec![],
            client,
        );
        assert_eq!(store.base_url, "https://cache.nixos.org");
    }

    #[test]
    fn base_url_accessor() {
        let store = BinaryCacheStore::new("https://cache.nixos.org/", vec![]);
        assert_eq!(store.base_url(), "https://cache.nixos.org");
    }

    #[test]
    fn trusted_keys_accessor_returns_keys() {
        let keys = vec![
            "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=".to_string(),
        ];
        let store = BinaryCacheStore::new("https://cache.nixos.org", keys.clone());
        assert_eq!(store.trusted_keys(), &keys[..]);
    }

    #[test]
    fn trusted_keys_accessor_empty() {
        let store = BinaryCacheStore::new("https://cache.nixos.org", vec![]);
        assert!(store.trusted_keys().is_empty());
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

    // ── fetch_nar with MockHttpClient ───────────────────────

    #[tokio::test]
    async fn fetch_nar_returns_bytes() {
        let nar_content = b"fake-nar-content-with-binary-data\x00\xff\xfe";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/nar/abc.nar.xz",
            HttpResponse {
                status: 200,
                body: String::from_utf8_lossy(nar_content).to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        let data = store.fetch_nar("nar/abc.nar.xz").await.unwrap();
        assert!(!data.is_empty());
    }

    #[tokio::test]
    async fn fetch_nar_http_error() {
        let client = MockHttpClient::new();
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        let result = store.fetch_nar("nar/missing.nar.xz").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_nar_empty_body() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/nar/empty.nar",
            HttpResponse {
                status: 200,
                body: String::new(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        let data = store.fetch_nar("nar/empty.nar").await.unwrap();
        assert!(data.is_empty());
    }

    // ── fetch_narinfo edge cases ──────────────────────────────

    #[tokio::test]
    async fn fetch_narinfo_unknown_fields_ignored() {
        let narinfo_with_extra = "\
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
FutureField: should-be-ignored
AnotherUnknown: 42
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: narinfo_with_extra.to_string(),
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
        assert_eq!(narinfo.unwrap().nar_size, 5000);
    }

    #[tokio::test]
    async fn fetch_narinfo_malformed_body_returns_error() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/abc00000000000000000000000000000.narinfo",
            HttpResponse {
                status: 200,
                body: "this is not valid narinfo content at all".to_string(),
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

    #[tokio::test]
    async fn fetch_narinfo_missing_required_field() {
        let incomplete_narinfo = "\
StorePath: /nix/store/abc-hello
Compression: xz
NarHash: sha256:bbb
NarSize: 5000
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/abc00000000000000000000000000000.narinfo",
            HttpResponse {
                status: 200,
                body: incomplete_narinfo.to_string(),
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

    #[tokio::test]
    async fn fetch_narinfo_whitespace_in_body() {
        let narinfo_with_whitespace = "\
  StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
  URL: nar/abc.nar.xz
  Compression: xz
  FileHash: sha256:aaa
  FileSize: 1000
  NarHash: sha256:bbb
  NarSize: 5000
  References:
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: narinfo_with_whitespace.to_string(),
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
    }

    #[tokio::test]
    async fn fetch_narinfo_http_client_error() {
        let client = MockHttpClient::new();
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        let result = store
            .fetch_narinfo("nonexistent0000000000000000000000")
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_narinfo_302_redirect_returns_error() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/abc00000000000000000000000000000.narinfo",
            HttpResponse {
                status: 302,
                body: String::new(),
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

    #[tokio::test]
    async fn fetch_narinfo_no_signatures() {
        let narinfo_no_sigs = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References:
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: narinfo_no_sigs.to_string(),
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
            .unwrap()
            .unwrap();
        assert!(narinfo.signatures.is_empty());
        assert!(narinfo.references.is_empty());
    }

    #[tokio::test]
    async fn fetch_narinfo_multiple_signatures() {
        let narinfo_multi_sigs = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References:
Sig: key1:aaa==
Sig: key2:bbb==
Sig: key3:ccc==
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: narinfo_multi_sigs.to_string(),
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
            .unwrap()
            .unwrap();
        assert_eq!(narinfo.signatures.len(), 3);
        assert_eq!(narinfo.signatures[0], "key1:aaa==");
        assert_eq!(narinfo.signatures[2], "key3:ccc==");
    }

    // ── Store trait with dyn Store (Arc<dyn Store> pattern) ──

    #[tokio::test]
    async fn dyn_store_query_path_info() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: MOCK_NARINFO.to_string(),
            },
        );
        let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(
            BinaryCacheStore::with_http_client(
                "https://cache.nixos.org",
                vec![],
                Box::new(client),
            ),
        );

        let info = store.query_path_info(&hello_store_path()).await.unwrap();
        assert!(info.is_some());
        assert_eq!(info.unwrap().nar_size, 5000);
    }

    #[tokio::test]
    async fn dyn_store_is_valid_path() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: MOCK_NARINFO.to_string(),
            },
        );
        let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(
            BinaryCacheStore::with_http_client(
                "https://cache.nixos.org",
                vec![],
                Box::new(client),
            ),
        );

        assert!(store.is_valid_path(&hello_store_path()).await.unwrap());
    }

    #[tokio::test]
    async fn dyn_store_query_all_valid_paths_unsupported() {
        let client = MockHttpClient::new();
        let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(
            BinaryCacheStore::with_http_client(
                "https://cache.nixos.org",
                vec![],
                Box::new(client),
            ),
        );

        let result = store.query_all_valid_paths().await;
        assert!(result.is_err());
    }


    // ── BinaryCacheError → StoreError conversion ─────────────

    #[test]
    fn binary_cache_error_http_client_converts_to_store_http() {
        let http_err = HttpError::Request("dns failure".to_string());
        let bc_err: BinaryCacheError = http_err.into();
        let store_err: StoreError = bc_err.into();
        match store_err {
            StoreError::Http(msg) => assert!(msg.contains("dns failure")),
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn binary_cache_error_unexpected_status_converts_to_store_http() {
        let bc_err = BinaryCacheError::UnexpectedStatus {
            status: 503,
            url: "https://cache.test/abc.narinfo".to_string(),
        };
        let store_err: StoreError = bc_err.into();
        match store_err {
            StoreError::Http(msg) => {
                assert!(msg.contains("503"));
                assert!(msg.contains("cache.test"));
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn binary_cache_error_narinfo_parse_converts_to_store_narinfo() {
        let parse_err = sui_compat::narinfo::NarInfoError::MissingField("StorePath".to_string());
        let bc_err: BinaryCacheError = parse_err.into();
        let store_err: StoreError = bc_err.into();
        match store_err {
            StoreError::NarInfo(msg) => {
                assert!(msg.contains("StorePath") || msg.contains("missing"));
            }
            other => panic!("expected NarInfo, got {other:?}"),
        }
    }

    #[test]
    fn binary_cache_error_display_unexpected_status() {
        let err = BinaryCacheError::UnexpectedStatus {
            status: 418,
            url: "https://teapot.test/x.narinfo".to_string(),
        };
        let s = err.to_string();
        assert!(s.contains("418"));
        assert!(s.contains("teapot.test"));
    }

    #[test]
    fn binary_cache_error_debug_format() {
        let err = BinaryCacheError::UnexpectedStatus {
            status: 500,
            url: "x".to_string(),
        };
        let debug = format!("{err:?}");
        assert!(debug.contains("UnexpectedStatus"));
        assert!(debug.contains("500"));
    }

    // ── Builder pattern ─────────────────────────────────────

    #[test]
    fn builder_default_is_reqwest_client() {
        let store = BinaryCacheStore::builder("https://cache.nixos.org").build();
        assert_eq!(store.base_url(), "https://cache.nixos.org");
        assert!(store.trusted_keys().is_empty());
    }

    #[test]
    fn builder_with_trusted_keys() {
        let keys = vec!["k1:abc==".to_string(), "k2:def==".to_string()];
        let store = BinaryCacheStore::builder("https://cache.nixos.org")
            .trusted_keys(keys.clone())
            .build();
        assert_eq!(store.trusted_keys().len(), 2);
        assert_eq!(store.trusted_keys()[0], "k1:abc==");
    }

    #[test]
    fn builder_chaining_order_independent() {
        let client = Box::new(MockHttpClient::new());
        let keys = vec!["k:s".to_string()];
        let store = BinaryCacheStore::builder("https://cache.nixos.org")
            .http_client(client)
            .trusted_keys(keys.clone())
            .build();
        assert_eq!(store.trusted_keys(), &keys[..]);
        assert_eq!(store.base_url(), "https://cache.nixos.org");
    }

    #[test]
    fn builder_strips_trailing_slash() {
        let store = BinaryCacheStore::builder("https://cache.nixos.org/").build();
        assert_eq!(store.base_url(), "https://cache.nixos.org");
    }

    #[test]
    fn builder_strips_multiple_trailing_slashes() {
        let store = BinaryCacheStore::builder("https://cache.nixos.org////").build();
        assert!(!store.base_url().ends_with('/'));
    }

    // ── store_path_hash edge cases ──────────────────────────

    #[test]
    fn store_path_hash_for_drv_path() {
        let path = StorePath::from_absolute_path(
            "/nix/store/xb4y5iklhya4blk42k1cfkb8k07dpp4n-hello-2.12.1.drv",
        )
        .unwrap();
        let hash = BinaryCacheStore::store_path_hash(&path);
        assert_eq!(hash, "xb4y5iklhya4blk42k1cfkb8k07dpp4n");
        assert_eq!(hash.len(), 32);
    }

    // ── narinfo with different compression algorithms ────────

    #[tokio::test]
    async fn fetch_narinfo_zstd_compression() {
        let body = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar.zst
Compression: zstd
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References:
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: body.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let info = store
            .fetch_narinfo("sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info.compression, "zstd");
    }

    #[tokio::test]
    async fn fetch_narinfo_no_compression() {
        let body = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar
Compression: none
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References:
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: body.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let info = store
            .fetch_narinfo("sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info.compression, "none");
    }

    #[tokio::test]
    async fn fetch_narinfo_bzip2_compression() {
        let body = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar.bz2
Compression: bzip2
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References:
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: body.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let info = store
            .fetch_narinfo("sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info.compression, "bzip2");
    }

    // ── narinfo with content-address (CA) field ──────────────

    #[tokio::test]
    async fn fetch_narinfo_with_ca_field() {
        let body = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-source.tar.gz
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References:
CA: fixed:out:r:sha256:cafebabedeadbeef
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: body.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let info = store
            .fetch_narinfo("sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            info.ca,
            Some("fixed:out:r:sha256:cafebabedeadbeef".to_string())
        );
        // Ensure conversion to PathInfo carries CA
        let path_info = PathInfo::from(&info);
        assert_eq!(
            path_info.content_address,
            Some("fixed:out:r:sha256:cafebabedeadbeef".to_string())
        );
    }

    // ── narinfo with many references on a single line ───────

    #[tokio::test]
    async fn fetch_narinfo_many_references_on_one_line() {
        let body = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References: dep1 dep2 dep3 dep4 dep5 dep6 dep7 dep8 dep9 dep10
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: body.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let info = store
            .fetch_narinfo("sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info.references.len(), 10);
        assert_eq!(info.references[0], "dep1");
        assert_eq!(info.references[9], "dep10");
    }

    // ── narinfo without optional Deriver field ───────────────

    #[tokio::test]
    async fn fetch_narinfo_no_deriver() {
        let body = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References:
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: body.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let info = store
            .fetch_narinfo("sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6")
            .await
            .unwrap()
            .unwrap();
        assert!(info.deriver.is_none());
    }

    // ── narinfo with empty Deriver value ─────────────────────

    #[tokio::test]
    async fn fetch_narinfo_empty_deriver_treated_as_none() {
        let body = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References:
Deriver:
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: body.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let info = store
            .fetch_narinfo("sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6")
            .await
            .unwrap()
            .unwrap();
        assert!(info.deriver.is_none());
    }

    // ── HTTP status code variations ──────────────────────────

    #[tokio::test]
    async fn fetch_narinfo_503_returns_error() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/abc00000000000000000000000000000.narinfo",
            HttpResponse {
                status: 503,
                body: "service unavailable".to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let result = store.fetch_narinfo("abc00000000000000000000000000000").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_narinfo_403_returns_error() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/abc00000000000000000000000000000.narinfo",
            HttpResponse {
                status: 403,
                body: "forbidden".to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let result = store.fetch_narinfo("abc00000000000000000000000000000").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_narinfo_301_redirect_returns_error() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/abc00000000000000000000000000000.narinfo",
            HttpResponse {
                status: 301,
                body: String::new(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let result = store.fetch_narinfo("abc00000000000000000000000000000").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_narinfo_201_created_treated_as_success() {
        let body = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References:
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 201,
                body: body.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let info = store
            .fetch_narinfo("sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6")
            .await
            .unwrap();
        assert!(info.is_some());
    }

    // ── fetch_nar 4xx/5xx errors ─────────────────────────────

    #[tokio::test]
    async fn fetch_nar_returns_correct_url_path() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/nar/some/nested/path.nar.xz",
            HttpResponse {
                status: 200,
                body: "data".to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let bytes = store.fetch_nar("nar/some/nested/path.nar.xz").await.unwrap();
        assert_eq!(bytes, b"data");
    }

    // ── Default trait methods on BinaryCacheStore ────────────

    #[tokio::test]
    async fn binary_cache_collect_garbage_unsupported() {
        use crate::traits::GcOptions;
        let client = MockHttpClient::new();
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let result = store.collect_garbage(&GcOptions::default()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn binary_cache_add_to_store_unsupported() {
        let client = MockHttpClient::new();
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let result = store.add_to_store("hello", b"data", &[]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn binary_cache_register_path_unsupported() {
        let client = MockHttpClient::new();
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let info = PathInfo::new("/nix/store/abc-x", "sha256:aaa");
        let result = store.register_path(&info).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn binary_cache_query_referrers_unsupported() {
        let client = MockHttpClient::new();
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let result = store.query_referrers(&hello_store_path()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn binary_cache_add_signatures_unsupported() {
        let client = MockHttpClient::new();
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        let result = store
            .add_signatures(&hello_store_path(), &["sig".to_string()])
            .await;
        assert!(result.is_err());
    }

    // ── query_references via BinaryCacheStore ────────────────
    //
    // BinaryCacheStore.query_path_info populates PathInfo.references with
    // absolute store paths (bare NarInfo basenames are prefixed with
    // /nix/store/ at conversion time). The default query_references in the
    // Store trait then parses each entry via StorePath::from_absolute_path,
    // so the full reference list flows through end to end.

    #[tokio::test]
    async fn binary_cache_query_references_round_trip() {
        let body = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References: 3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37 00bgd045z0d4icpbc2yyz4gx48ak44la-bash-5.2
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: body.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );
        // PathInfo.references are absolute store paths after the conversion.
        let info = store.query_path_info(&hello_store_path()).await.unwrap().unwrap();
        assert_eq!(info.references.len(), 2);
        assert_eq!(
            info.references[0],
            "/nix/store/3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37"
        );

        // query_references parses those absolute paths back into StorePaths,
        // yielding the full reference list.
        let refs = store.query_references(&hello_store_path()).await.unwrap();
        assert_eq!(refs.len(), 2);
    }

    // ── Box<dyn Store> dispatch ──────────────────────────────

    #[tokio::test]
    async fn box_dyn_binary_cache_store_query_path_info() {
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: MOCK_NARINFO.to_string(),
            },
        );
        let store: Box<dyn Store> = Box::new(BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        ));
        let info = store.query_path_info(&hello_store_path()).await.unwrap();
        assert!(info.is_some());
    }

    // ── Reference-prefix gap fix regression tests ────────────

    /// Round-trip a NarInfo with multiple bare-basename references through
    /// `BinaryCacheStore::query_path_info` and verify every reference comes
    /// out as a `/nix/store/`-prefixed absolute store path.
    #[tokio::test]
    async fn query_path_info_references_are_absolute_store_paths() {
        let narinfo_multi_refs = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References: 3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37-8 00bgd045z0d4icpbc2yyz4gx48ak44la-bash-5.2 sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
Deriver: abc.drv
Sig: cache.nixos.org-1:sig==
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: narinfo_multi_refs.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        let info = store
            .query_path_info(&hello_store_path())
            .await
            .unwrap()
            .expect("path info should be present");

        assert_eq!(info.references.len(), 3);
        for r in &info.references {
            assert!(
                r.starts_with("/nix/store/"),
                "reference should be absolute store path, got {r:?}"
            );
        }
        assert_eq!(
            info.references[0],
            "/nix/store/3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37-8"
        );
        assert_eq!(
            info.references[1],
            "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-bash-5.2"
        );
        assert_eq!(
            info.references[2],
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1"
        );
    }

    /// `Store::query_references` (the default trait method) must return a
    /// non-empty Vec when the underlying NarInfo had references — proving
    /// the silent-drop bug is fixed end to end.
    #[tokio::test]
    async fn query_references_via_store_returns_full_prefixed_paths() {
        // Tiny in-memory mock store that returns a fixed PathInfo whose
        // references already came from a NarInfo round-trip.
        struct MockStore {
            info: PathInfo,
        }

        #[async_trait::async_trait]
        impl Store for MockStore {
            async fn query_path_info(
                &self,
                _path: &StorePath,
            ) -> StoreResult<Option<PathInfo>> {
                Ok(Some(self.info.clone()))
            }
            async fn is_valid_path(&self, _path: &StorePath) -> StoreResult<bool> {
                Ok(true)
            }
            async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
                Ok(vec![])
            }
        }

        let narinfo = NarInfo {
            store_path: "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1".to_string(),
            url: "nar/abc.nar.xz".to_string(),
            compression: "xz".to_string(),
            file_hash: "sha256:aaa".to_string(),
            file_size: 1000,
            nar_hash: "sha256:bbb".to_string(),
            nar_size: 5000,
            references: vec![
                "3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37-8".to_string(),
                "00bgd045z0d4icpbc2yyz4gx48ak44la-bash-5.2".to_string(),
            ],
            deriver: None,
            signatures: vec![],
            ca: None,
        };
        let mock = MockStore {
            info: PathInfo::from(&narinfo),
        };

        let refs = mock.query_references(&hello_store_path()).await.unwrap();
        assert_eq!(
            refs.len(),
            2,
            "default query_references must yield both NarInfo references"
        );
        let absolute: Vec<String> = refs.iter().map(StorePath::to_absolute_path).collect();
        assert!(absolute.contains(
            &"/nix/store/3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37-8".to_string()
        ));
        assert!(absolute.contains(
            &"/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-bash-5.2".to_string()
        ));
    }

    /// A NarInfo whose `References:` line is empty must produce an empty
    /// `PathInfo.references` vec (no spurious entries from prefixing logic).
    #[tokio::test]
    async fn query_path_info_empty_references_yields_empty_vec() {
        let narinfo_no_refs = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:aaa
FileSize: 1000
NarHash: sha256:bbb
NarSize: 5000
References:
";
        let client = MockHttpClient::new().with_response(
            "https://cache.nixos.org/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6.narinfo",
            HttpResponse {
                status: 200,
                body: narinfo_no_refs.to_string(),
            },
        );
        let store = BinaryCacheStore::with_http_client(
            "https://cache.nixos.org",
            vec![],
            Box::new(client),
        );

        let info = store
            .query_path_info(&hello_store_path())
            .await
            .unwrap()
            .expect("path info should be present");
        assert!(info.references.is_empty());
    }

    // ── verify_narinfo_signatures ──────────────────────────────

    fn make_signed_narinfo() -> (NarInfo, String) {
        use ed25519_dalek::{Signer, SigningKey};
        use sui_compat::hash::base64_encode;
        use sui_compat::signature::compute_fingerprint;

        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();

        let narinfo = NarInfo {
            store_path: "/nix/store/abc-hello".to_string(),
            url: "nar/abc.nar.xz".to_string(),
            compression: "xz".to_string(),
            file_hash: "sha256:aaa".to_string(),
            file_size: 1000,
            nar_hash: "sha256:bbb".to_string(),
            nar_size: 5000,
            references: vec![],
            deriver: None,
            signatures: vec![],
            ca: None,
        };

        let fingerprint = compute_fingerprint(
            &narinfo.store_path,
            &narinfo.nar_hash,
            narinfo.nar_size,
            &narinfo.references,
        );
        let sig = signing_key.sign(fingerprint.as_bytes());
        let sig_str = format!(
            "test-key:{}",
            base64_encode(&sig.to_bytes())
        );
        let trusted_key = format!(
            "test-key:{}",
            base64_encode(verifying_key.as_bytes())
        );

        let mut signed = narinfo;
        signed.signatures = vec![sig_str];

        (signed, trusted_key)
    }

    #[test]
    fn verify_narinfo_signatures_valid() {
        let (narinfo, trusted_key) = make_signed_narinfo();
        let result = BinaryCacheStore::verify_narinfo_signatures(
            &narinfo,
            &[trusted_key],
        )
        .unwrap();
        assert!(result);
    }

    #[test]
    fn verify_narinfo_signatures_invalid_key() {
        use sui_compat::hash::base64_encode;

        let (narinfo, _) = make_signed_narinfo();
        // Use a different key — should fail.
        let wrong_key = format!(
            "test-key:{}",
            base64_encode(&[99u8; 32])
        );
        let result = BinaryCacheStore::verify_narinfo_signatures(
            &narinfo,
            &[wrong_key],
        )
        .unwrap();
        assert!(!result);
    }

    #[test]
    fn verify_narinfo_signatures_empty_trusted_keys_returns_false() {
        let (narinfo, _) = make_signed_narinfo();
        let result = BinaryCacheStore::verify_narinfo_signatures(
            &narinfo,
            &[],
        )
        .unwrap();
        assert!(!result);
    }

    #[test]
    fn verify_narinfo_signatures_no_matching_key_name() {
        use sui_compat::hash::base64_encode;

        let (narinfo, _) = make_signed_narinfo();
        // Trusted key has a different name.
        let wrong_name_key = format!(
            "other-key:{}",
            base64_encode(&[42u8; 32])
        );
        let result = BinaryCacheStore::verify_narinfo_signatures(
            &narinfo,
            &[wrong_name_key],
        )
        .unwrap();
        assert!(!result);
    }

    #[test]
    fn verify_narinfo_signatures_unsigned_narinfo() {
        let narinfo = NarInfo {
            store_path: "/nix/store/abc-hello".to_string(),
            url: "nar/abc.nar.xz".to_string(),
            compression: "xz".to_string(),
            file_hash: "sha256:aaa".to_string(),
            file_size: 1000,
            nar_hash: "sha256:bbb".to_string(),
            nar_size: 5000,
            references: vec![],
            deriver: None,
            signatures: vec![],
            ca: None,
        };
        let result = BinaryCacheStore::verify_narinfo_signatures(
            &narinfo,
            &["key:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string()],
        )
        .unwrap();
        assert!(!result);
    }

    #[test]
    fn verify_narinfo_signatures_with_references() {
        use ed25519_dalek::{Signer, SigningKey};
        use sui_compat::hash::base64_encode;
        use sui_compat::signature::compute_fingerprint;

        let signing_key = SigningKey::from_bytes(&[10u8; 32]);
        let verifying_key = signing_key.verifying_key();

        let refs = vec![
            "dep-b".to_string(),
            "dep-a".to_string(),
        ];

        let narinfo = NarInfo {
            store_path: "/nix/store/xyz-pkg".to_string(),
            url: "nar/xyz.nar".to_string(),
            compression: "none".to_string(),
            file_hash: "sha256:fff".to_string(),
            file_size: 2000,
            nar_hash: "sha256:eee".to_string(),
            nar_size: 3000,
            references: refs.clone(),
            deriver: None,
            signatures: vec![],
            ca: None,
        };

        // The verify method sorts references, so we must sign with sorted refs.
        let mut sorted_refs = refs;
        sorted_refs.sort();
        let fingerprint = compute_fingerprint(
            &narinfo.store_path,
            &narinfo.nar_hash,
            narinfo.nar_size,
            &sorted_refs,
        );
        let sig = signing_key.sign(fingerprint.as_bytes());
        let sig_str = format!("k:{}", base64_encode(&sig.to_bytes()));
        let trusted_key = format!("k:{}", base64_encode(verifying_key.as_bytes()));

        let mut signed = narinfo;
        signed.signatures = vec![sig_str];

        let result = BinaryCacheStore::verify_narinfo_signatures(
            &signed,
            &[trusted_key],
        )
        .unwrap();
        assert!(result);
    }
}
