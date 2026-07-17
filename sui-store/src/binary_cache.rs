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
    /// Optional authorization header (`("Bearer", "<token>")` or `("Basic", "<creds>")`).
    auth_header: Option<(String, String)>,
    /// Whether a valid trusted-key signature is REQUIRED before a path
    /// from this cache is accepted (nix's `require-sigs`, default `true`).
    ///
    /// When `true` (the secure default), a substituted path must carry a
    /// valid signature from one of `trusted_keys` OR be a self-verifying
    /// content-addressed path — otherwise it is refused. When `false`
    /// (explicit opt-out, e.g. a trusted local cache the operator
    /// controls) the signature gate is skipped. Turning off signatures
    /// does NOT turn off the NAR-hash byte-integrity check.
    require_signatures: bool,
}

/// Builder for [`BinaryCacheStore`].
pub struct BinaryCacheStoreBuilder {
    base_url: String,
    trusted_keys: Vec<String>,
    client: Option<Box<dyn HttpClient>>,
    auth_header: Option<(String, String)>,
    require_signatures: bool,
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

    /// Set an authorization header (e.g., `("Bearer", "<token>")` for Attic).
    #[must_use]
    pub fn auth_header(mut self, scheme: &str, credentials: &str) -> Self {
        self.auth_header = Some((scheme.to_string(), credentials.to_string()));
        self
    }

    /// Set whether a valid trusted-key signature is required to accept a
    /// path from this cache (nix's `require-sigs`; the default is `true`).
    ///
    /// Pass `false` only for a cache the operator fully controls where the
    /// transport itself is the trust boundary. The NAR-hash integrity
    /// check still runs either way.
    #[must_use]
    pub fn require_signatures(mut self, require: bool) -> Self {
        self.require_signatures = require;
        self
    }

    /// Build the [`BinaryCacheStore`].
    #[must_use]
    pub fn build(self) -> BinaryCacheStore {
        BinaryCacheStore {
            client: self.client.unwrap_or_else(|| Box::new(ReqwestHttpClient::new())),
            base_url: self.base_url,
            trusted_keys: self.trusted_keys,
            auth_header: self.auth_header,
            require_signatures: self.require_signatures,
        }
    }
}

impl BinaryCacheStore {
    /// Create a builder for a binary cache store with the given base URL.
    ///
    /// `require_signatures` defaults to `true` — the SECURE default. A path
    /// with no valid trusted-key signature (and no self-verifying CA) is
    /// refused unless the operator explicitly opts out via
    /// [`BinaryCacheStoreBuilder::require_signatures`].
    #[must_use]
    pub fn builder(base_url: &str) -> BinaryCacheStoreBuilder {
        BinaryCacheStoreBuilder {
            base_url: base_url.trim_end_matches('/').to_string(),
            trusted_keys: Vec::new(),
            client: None,
            auth_header: None,
            require_signatures: true,
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

    /// Build the request headers, including auth if configured.
    fn request_headers(&self, extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = extra
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        if let Some((scheme, creds)) = &self.auth_header {
            headers.push(("Authorization".to_string(), format!("{scheme} {creds}")));
        }
        headers
    }

    /// Fetch NarInfo for a store path hash.
    pub async fn fetch_narinfo(&self, hash: &str) -> StoreResult<Option<NarInfo>> {
        let url = format!("{}/{hash}.narinfo", self.base_url);
        let headers = self.request_headers(&[("Accept", "text/x-nix-narinfo")]);
        let header_refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        let response = self
            .client
            .get(&url, &header_refs)
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

    /// Whether this cache requires a valid trusted-key signature to accept
    /// a path (nix's `require-sigs`; default `true`).
    #[must_use]
    pub fn require_signatures(&self) -> bool {
        self.require_signatures
    }

    /// Return the configured authorization header, if any.
    #[must_use]
    pub fn auth_header(&self) -> Option<(&str, &str)> {
        self.auth_header.as_ref().map(|(s, c)| (s.as_str(), c.as_str()))
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

        // Nix's canonical fingerprint prints references as ABSOLUTE store
        // paths (`/nix/store/<hash>-<name>`), but the narinfo `References:`
        // wire field carries BARE basenames. Feeding the basenames verbatim
        // computes the WRONG fingerprint, so no real cache.nixos.org
        // signature ever verifies — empirically confirmed: the real
        // hello-2.12.3 sig verifies iff the references are absolutized
        // (`sui-compat/tests/real_cache_fingerprint.rs`). Absolutize here,
        // matching the exact prefixing the `PathInfo::from(&NarInfo)`
        // conversion already performs (basename → `/nix/store/<basename>`;
        // anything already absolute passes through unchanged).
        // `compute_fingerprint` then sorts into Nix's canonical order
        // internally, so signer and verifier fingerprint identical bytes.
        let store_dir = sui_compat::store_path::DEFAULT_STORE_DIR;
        let absolute_refs: Vec<String> = narinfo
            .references
            .iter()
            .map(|r| {
                if r.starts_with('/') {
                    r.clone()
                } else {
                    format!("{store_dir}/{r}")
                }
            })
            .collect();
        let fingerprint = compute_fingerprint(
            &narinfo.store_path,
            &narinfo.nar_hash,
            narinfo.nar_size,
            &absolute_refs,
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

    /// Decide whether a narinfo is acceptable to substitute FROM THIS CACHE,
    /// applying nix's exact trust model.
    ///
    /// A path is acceptable iff:
    /// 1. this cache does not require signatures (`require_signatures ==
    ///    false`, an explicit operator opt-out), OR
    /// 2. the path is content-addressed (`CA:` present) — a CA path is
    ///    self-verifying: its store-path digest is derived from its
    ///    content, so a valid CA assertion is trust independent of any
    ///    signer, exactly as nix treats it, OR
    /// 3. the narinfo carries ≥1 valid signature from one of this cache's
    ///    trusted keys.
    ///
    /// Otherwise the path is REFUSED with
    /// [`StoreError::SignatureVerificationFailed`] — matching nix, which
    /// declines to substitute an unsigned/untrusted path (falling back to a
    /// local build).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SignatureVerificationFailed`] when none of the
    /// acceptance conditions hold.
    pub fn check_narinfo_acceptable(&self, narinfo: &NarInfo) -> StoreResult<()> {
        // (1) Operator explicitly turned off the signature requirement.
        if !self.require_signatures {
            return Ok(());
        }

        // (2) Content-addressed paths are self-verifying — the store-path
        // digest is a function of the content, so a CA assertion needs no
        // signer. (The NAR-hash check downstream still enforces byte
        // integrity of the delivered bytes.)
        if narinfo.ca.as_deref().is_some_and(|ca| !ca.is_empty()) {
            return Ok(());
        }

        // (3) Require a valid signature from a trusted key.
        if Self::verify_narinfo_signatures(narinfo, &self.trusted_keys)? {
            return Ok(());
        }

        // No acceptance condition held — refuse.
        let reason = if self.trusted_keys.is_empty() {
            "no trusted public keys configured for this cache and the path is not content-addressed".to_string()
        } else if narinfo.signatures.is_empty() {
            "narinfo carries no signature".to_string()
        } else {
            "no signature matched a trusted public key".to_string()
        };
        Err(StoreError::SignatureVerificationFailed {
            path: narinfo.store_path.clone(),
            reason,
        })
    }

    /// Verify that the SHA-256 hash of the decompressed NAR bytes equals the
    /// `NarHash` declared in the narinfo.
    ///
    /// nix hashes the received NAR and asserts it matches `narinfo.narHash`
    /// before accepting a substituted path; a corrupt or MITM'd cache could
    /// otherwise inject arbitrary bytes. Both sides are normalized through
    /// [`NixHash::parse_any`](sui_compat::hash::NixHash::parse_any) so the
    /// comparison is over raw digest bytes and is independent of the
    /// declared encoding (nix uses `sha256:<nix-base32>`; sui's local store
    /// records `sha256:<hex>` — both decode to the same 32 bytes).
    ///
    /// # Errors
    ///
    /// - [`StoreError::NarHashMismatch`] if the computed digest differs from
    ///   the declared one.
    /// - [`StoreError::NarInfo`] if the declared `narHash` cannot be decoded.
    /// - [`StoreError::NotSupported`] if the algorithm is not SHA-256.
    pub fn verify_nar_hash(narinfo: &NarInfo, nar_bytes: &[u8]) -> StoreResult<()> {
        use sha2::{Digest, Sha256};
        use sui_compat::hash::{HashAlgorithm, NixHash};

        // nix narinfo NarHash is always sha256 in practice; be explicit.
        let raw = narinfo
            .nar_hash
            .strip_prefix("sha256:")
            .or_else(|| narinfo.nar_hash.strip_prefix("sha256-"))
            .unwrap_or(&narinfo.nar_hash);
        if narinfo.nar_hash.starts_with("sha1:")
            || narinfo.nar_hash.starts_with("md5:")
        {
            return Err(StoreError::NotSupported(format!(
                "unsupported NarHash algorithm in {}: {}",
                narinfo.store_path, narinfo.nar_hash
            )));
        }

        // Decode the declared hash to raw digest bytes (hex / nix-base32 /
        // SRI all accepted).
        let expected = NixHash::parse_any(HashAlgorithm::Sha256, raw).map_err(|e| {
            StoreError::NarInfo(format!(
                "cannot decode NarHash {:?} for {}: {e:?}",
                narinfo.nar_hash, narinfo.store_path
            ))
        })?;

        // Compute the digest of the actual bytes.
        let actual_digest = Sha256::digest(nar_bytes);

        if actual_digest.as_slice() != expected.digest.as_slice() {
            let actual = NixHash::new(HashAlgorithm::Sha256, actual_digest.to_vec());
            return Err(StoreError::NarHashMismatch {
                path: narinfo.store_path.clone(),
                expected: narinfo.nar_hash.clone(),
                actual: actual.to_nix_string(),
            });
        }

        Ok(())
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

    /// The poisoned-write-rejected proof: a validly-signed narinfo whose
    /// content is then TAMPERED (an attacker who wrote to Redis/PG mutates
    /// `nar_size` but reuses the honest `Sig:`) is REJECTED by the consumer,
    /// because the signature is over the fingerprint of the original bytes.
    /// This is the trust-of-a-poisoned-write closing on the consume side.
    #[test]
    fn verify_narinfo_signatures_tampered_content_rejected() {
        let (mut narinfo, trusted_key) = make_signed_narinfo();

        // Sanity: the untampered signed narinfo verifies.
        assert!(
            BinaryCacheStore::verify_narinfo_signatures(&narinfo, &[trusted_key.clone()]).unwrap(),
            "control: honest signed narinfo must verify",
        );

        // Attacker mutates the size (points the path at different bytes) but
        // cannot forge a new signature without the secret key, so reuses the
        // old one.
        narinfo.nar_size = 999_999;
        let result =
            BinaryCacheStore::verify_narinfo_signatures(&narinfo, &[trusted_key]).unwrap();
        assert!(!result, "a tampered signed narinfo must be REJECTED");
    }

    /// Same, but tampering the store path (the identity itself).
    #[test]
    fn verify_narinfo_signatures_tampered_store_path_rejected() {
        let (mut narinfo, trusted_key) = make_signed_narinfo();
        narinfo.store_path = "/nix/store/evil-swapped".to_string();
        let result =
            BinaryCacheStore::verify_narinfo_signatures(&narinfo, &[trusted_key]).unwrap();
        assert!(!result, "a store-path-swapped narinfo must be REJECTED");
    }

    #[test]
    fn verify_narinfo_signatures_with_references() {
        use ed25519_dalek::{Signer, SigningKey};
        use sui_compat::hash::base64_encode;
        use sui_compat::signature::compute_fingerprint;

        let signing_key = SigningKey::from_bytes(&[10u8; 32]);
        let verifying_key = signing_key.verifying_key();

        // The narinfo wire form carries BARE basenames…
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

        // …but nix (and now `verify_narinfo_signatures`) signs/verifies the
        // fingerprint over ABSOLUTE store paths, sorted. Reproduce that here
        // so the test signs exactly what the consumer will verify.
        let store_dir = sui_compat::store_path::DEFAULT_STORE_DIR;
        let mut sorted_refs: Vec<String> = refs
            .iter()
            .map(|r| format!("{store_dir}/{r}"))
            .collect();
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

    /// Regression for the ref-ordering bug (design §1.4 #5): a signature made
    /// over the references in the order they appear in the narinfo — WITHOUT
    /// pre-sorting — must still verify through the consumer path, because
    /// `compute_fingerprint` now canonicalizes the order on both sides. This
    /// mirrors what `sui_cache::CacheSigner` does at ingest (it passes
    /// `info.references` through unsorted). Before the fix this asserted
    /// `false`.
    #[test]
    fn verify_narinfo_signatures_unsorted_references_at_sign_time() {
        use ed25519_dalek::{Signer, SigningKey};
        use sui_compat::hash::base64_encode;
        use sui_compat::signature::compute_fingerprint;

        let signing_key = SigningKey::from_bytes(&[11u8; 32]);
        let verifying_key = signing_key.verifying_key();

        // References deliberately NOT in sorted order.
        let refs = vec![
            "/nix/store/zzz-late".to_string(),
            "/nix/store/aaa-early".to_string(),
            "/nix/store/mmm-mid".to_string(),
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

        // Sign the fingerprint of the UNSORTED references exactly as the
        // sui daemon signer does (it passes info.references verbatim).
        let fingerprint = compute_fingerprint(
            &narinfo.store_path,
            &narinfo.nar_hash,
            narinfo.nar_size,
            &refs,
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
        assert!(result, "an unsorted-reference signature must verify");
    }

    // ── verify_nar_hash (byte-integrity gate) ────────────────────

    fn make_hello_nar() -> Vec<u8> {
        use sui_compat::nar::{NarNode, NarWriter};
        let node = NarNode::Regular {
            executable: false,
            contents: b"hello".to_vec(),
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        buf
    }

    /// The real sha256 (hex) of `make_hello_nar()`.
    const HELLO_NAR_HASH_HEX: &str =
        "sha256:0a430879c266f8b57f4092a0f935cf3facd48bbccde5760d4748ca405171e969";

    fn narinfo_with_nar_hash(nar_hash: &str) -> NarInfo {
        NarInfo {
            store_path: "/nix/store/abc-hello".to_string(),
            url: "nar/abc.nar".to_string(),
            compression: "none".to_string(),
            file_hash: "sha256:aaa".to_string(),
            file_size: 10,
            nar_hash: nar_hash.to_string(),
            nar_size: 120,
            references: vec![],
            deriver: None,
            signatures: vec![],
            ca: None,
        }
    }

    #[test]
    fn verify_nar_hash_matches_hex() {
        let nar = make_hello_nar();
        let info = narinfo_with_nar_hash(HELLO_NAR_HASH_HEX);
        assert!(BinaryCacheStore::verify_nar_hash(&info, &nar).is_ok());
    }

    #[test]
    fn verify_nar_hash_matches_nix_base32() {
        // The SAME digest expressed in nix-base32 (the form real narinfos use)
        // must also verify — the check is over raw digest bytes, not encoding.
        use sui_compat::hash::{HashAlgorithm, NixHash};
        let nar = make_hello_nar();
        let raw = HELLO_NAR_HASH_HEX.strip_prefix("sha256:").unwrap();
        let digest = NixHash::parse_any(HashAlgorithm::Sha256, raw).unwrap();
        // Re-encode as nix-base32 via the store_path helper.
        let b32 = sui_compat::store_path::nix_base32_encode(&digest.digest);
        let info = narinfo_with_nar_hash(&format!("sha256:{b32}"));
        assert!(
            BinaryCacheStore::verify_nar_hash(&info, &nar).is_ok(),
            "nix-base32 NarHash of the same bytes must verify",
        );
    }

    #[test]
    fn verify_nar_hash_rejects_mismatch() {
        let nar = make_hello_nar();
        // Declared hash is of DIFFERENT bytes → must be rejected.
        let wrong = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let info = narinfo_with_nar_hash(wrong);
        match BinaryCacheStore::verify_nar_hash(&info, &nar) {
            Err(StoreError::NarHashMismatch { expected, .. }) => {
                assert_eq!(expected, wrong);
            }
            other => panic!("expected NarHashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_nar_hash_rejects_tampered_bytes() {
        // Correct declared hash, but the bytes are different → rejected.
        let info = narinfo_with_nar_hash(HELLO_NAR_HASH_HEX);
        let tampered = b"not the hello nar bytes at all".to_vec();
        assert!(matches!(
            BinaryCacheStore::verify_nar_hash(&info, &tampered),
            Err(StoreError::NarHashMismatch { .. })
        ));
    }

    #[test]
    fn verify_nar_hash_rejects_unsupported_algorithm() {
        let nar = make_hello_nar();
        let info = narinfo_with_nar_hash("sha1:deadbeef");
        assert!(matches!(
            BinaryCacheStore::verify_nar_hash(&info, &nar),
            Err(StoreError::NotSupported(_))
        ));
    }

    // ── check_narinfo_acceptable (the trust gate) ────────────────

    #[test]
    fn acceptable_when_require_signatures_off() {
        // Explicit opt-out: unsigned path is accepted.
        let store = BinaryCacheStore::builder("https://cache.example.com")
            .require_signatures(false)
            .build();
        let info = narinfo_with_nar_hash(HELLO_NAR_HASH_HEX);
        assert!(store.check_narinfo_acceptable(&info).is_ok());
    }

    #[test]
    fn acceptable_when_content_addressed() {
        // Secure default on, no trusted keys, but the path is CA → accepted.
        let store = BinaryCacheStore::builder("https://cache.example.com").build();
        assert!(store.require_signatures());
        let mut info = narinfo_with_nar_hash(HELLO_NAR_HASH_HEX);
        info.ca = Some("fixed:r:sha256:deadbeef".to_string());
        assert!(store.check_narinfo_acceptable(&info).is_ok());
    }

    #[test]
    fn refused_when_unsigned_and_require_signatures_on() {
        let store = BinaryCacheStore::builder("https://cache.example.com").build();
        let info = narinfo_with_nar_hash(HELLO_NAR_HASH_HEX);
        assert!(matches!(
            store.check_narinfo_acceptable(&info),
            Err(StoreError::SignatureVerificationFailed { .. })
        ));
    }

    #[test]
    fn acceptable_with_trusted_signature() {
        use ed25519_dalek::{Signer, SigningKey};
        use sui_compat::hash::base64_encode;
        use sui_compat::signature::compute_fingerprint;

        let signing_key = SigningKey::from_bytes(&[3u8; 32]);
        let mut info = narinfo_with_nar_hash(HELLO_NAR_HASH_HEX);
        // Non-empty references, given as basenames in the narinfo; the gate
        // must absolutize them to match nix's signed fingerprint.
        info.references = vec!["dep-b".to_string(), "dep-a".to_string()];

        let store_dir = sui_compat::store_path::DEFAULT_STORE_DIR;
        let abs: Vec<String> = info
            .references
            .iter()
            .map(|r| format!("{store_dir}/{r}"))
            .collect();
        let fingerprint =
            compute_fingerprint(&info.store_path, &info.nar_hash, info.nar_size, &abs);
        let sig = signing_key.sign(fingerprint.as_bytes());
        info.signatures = vec![format!("k:{}", base64_encode(&sig.to_bytes()))];

        let trusted = format!("k:{}", base64_encode(signing_key.verifying_key().as_bytes()));
        let store = BinaryCacheStore::builder("https://cache.example.com")
            .trusted_keys(vec![trusted])
            .build();
        assert!(
            store.check_narinfo_acceptable(&info).is_ok(),
            "a validly-signed (absolutized-refs) narinfo must be accepted",
        );
    }

    #[test]
    fn refused_with_untrusted_signature() {
        use ed25519_dalek::{Signer, SigningKey};
        use sui_compat::hash::base64_encode;
        use sui_compat::signature::compute_fingerprint;

        let signing_key = SigningKey::from_bytes(&[3u8; 32]);
        let mut info = narinfo_with_nar_hash(HELLO_NAR_HASH_HEX);
        let fingerprint =
            compute_fingerprint(&info.store_path, &info.nar_hash, info.nar_size, &[]);
        let sig = signing_key.sign(fingerprint.as_bytes());
        info.signatures = vec![format!("k:{}", base64_encode(&sig.to_bytes()))];

        // Trust a DIFFERENT key under the same name.
        let other = SigningKey::from_bytes(&[4u8; 32]);
        let trusted = format!("k:{}", base64_encode(other.verifying_key().as_bytes()));
        let store = BinaryCacheStore::builder("https://cache.example.com")
            .trusted_keys(vec![trusted])
            .build();
        assert!(matches!(
            store.check_narinfo_acceptable(&info),
            Err(StoreError::SignatureVerificationFailed { .. })
        ));
    }

    // ── Auth header tests ────────────────────────────────────────

    #[test]
    fn builder_auth_header_none_by_default() {
        let store = BinaryCacheStore::builder("https://cache.example.com").build();
        assert!(store.auth_header().is_none());
    }

    #[test]
    fn builder_auth_header_set() {
        let store = BinaryCacheStore::builder("https://cache.example.com")
            .auth_header("Bearer", "my-token-123")
            .build();
        let (scheme, creds) = store.auth_header().unwrap();
        assert_eq!(scheme, "Bearer");
        assert_eq!(creds, "my-token-123");
    }

    #[test]
    fn request_headers_without_auth() {
        let store = BinaryCacheStore::builder("https://cache.example.com").build();
        let headers = store.request_headers(&[("Accept", "text/plain")]);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0], ("Accept".to_string(), "text/plain".to_string()));
    }

    #[test]
    fn request_headers_with_auth() {
        let store = BinaryCacheStore::builder("https://cache.example.com")
            .auth_header("Bearer", "token123")
            .build();
        let headers = store.request_headers(&[("Accept", "text/plain")]);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[1], ("Authorization".to_string(), "Bearer token123".to_string()));
    }

    #[test]
    fn new_constructor_has_no_auth() {
        let store = BinaryCacheStore::new("https://cache.example.com", vec![]);
        assert!(store.auth_header().is_none());
    }
}
