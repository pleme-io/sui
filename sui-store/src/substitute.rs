//! Substitution pipeline — fetch store paths from binary caches.
//!
//! The [`Substitutor`] connects binary cache fetching with local store
//! registration. For each store path it: checks the local store, tries
//! each configured binary cache in order, downloads + decompresses the
//! NAR, and registers the result in the local store.

use std::sync::Arc;

use sui_compat::store_path::StorePath;

use crate::binary_cache::BinaryCacheStore;
use crate::nar::decompress_nar;
use crate::traits::{Store, StoreError, StoreResult};

/// Result of a substitution attempt for a single store path.
#[derive(Debug)]
pub enum SubstituteResult {
    /// Path already existed in local store.
    AlreadyPresent,
    /// Successfully substituted from a binary cache.
    Substituted {
        /// Base URL of the cache that provided the path.
        cache_url: String,
        /// Size of the uncompressed NAR in bytes.
        nar_size: u64,
    },
    /// Not found in any configured cache — needs local build.
    NotFound,
}

impl SubstituteResult {
    /// Returns `true` if the path was already present in the local store.
    #[must_use]
    pub fn is_already_present(&self) -> bool {
        matches!(self, Self::AlreadyPresent)
    }

    /// Returns `true` if the path was successfully substituted.
    #[must_use]
    pub fn is_substituted(&self) -> bool {
        matches!(self, Self::Substituted { .. })
    }

    /// Returns `true` if the path was not found in any cache.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound)
    }
}

/// Connects binary cache fetching with local store registration.
///
/// For each store path, the substitutor:
/// 1. Checks if the path already exists in the local store
/// 2. Tries each binary cache in order
/// 3. Downloads and decompresses the NAR
/// 4. Registers the path in the local store
pub struct Substitutor {
    local_store: Arc<dyn Store>,
    caches: Vec<Arc<BinaryCacheStore>>,
}

impl Substitutor {
    /// Create a new substitutor with a local store and a list of binary caches.
    ///
    /// Caches are tried in order — put the fastest/most-likely cache first.
    pub fn new(local_store: Arc<dyn Store>, caches: Vec<Arc<BinaryCacheStore>>) -> Self {
        Self {
            local_store,
            caches,
        }
    }

    /// Try to substitute a store path from binary caches.
    ///
    /// Returns `Ok(SubstituteResult)` indicating what happened:
    /// - `AlreadyPresent` — path was already in the local store
    /// - `Substituted` — fetched from a cache and registered locally
    /// - `NotFound` — not available in any configured cache
    pub async fn substitute(&self, path: &StorePath) -> StoreResult<SubstituteResult> {
        // 1. Check if already in local store
        if self.local_store.is_valid_path(path).await? {
            return Ok(SubstituteResult::AlreadyPresent);
        }

        // 2. Try each binary cache in order
        for cache in &self.caches {
            match self.try_cache(cache, path).await {
                Ok(Some(result)) => return Ok(result),
                Ok(None) => continue,
                Err(e) => {
                    // Log warning but try next cache
                    tracing::warn!(
                        cache_url = cache.base_url(),
                        path = %path,
                        error = %e,
                        "binary cache substitution failed, trying next"
                    );
                    continue;
                }
            }
        }

        Ok(SubstituteResult::NotFound)
    }

    /// Attempt to fetch and register a store path from a single cache.
    ///
    /// Returns `Ok(Some(result))` on success, `Ok(None)` if the path is not
    /// in this cache, or `Err` on a hard failure.
    async fn try_cache(
        &self,
        cache: &BinaryCacheStore,
        path: &StorePath,
    ) -> StoreResult<Option<SubstituteResult>> {
        // 1. Fetch narinfo
        let hash = path.hash();
        let narinfo = match cache.fetch_narinfo(&hash).await? {
            Some(info) => info,
            None => return Ok(None),
        };

        // 2. Verify signatures if the cache has trusted keys
        let trusted_keys = cache.trusted_keys();
        if !trusted_keys.is_empty() {
            let valid = BinaryCacheStore::verify_narinfo_signatures(&narinfo, trusted_keys)?;
            if !valid {
                return Err(StoreError::Http(format!(
                    "no valid signature for {} from cache {}",
                    path, cache.base_url()
                )));
            }
        }

        // 3. Download NAR
        let compressed_nar = cache
            .fetch_nar(&narinfo.url)
            .await
            .map_err(|e| StoreError::Http(format!("NAR download failed: {e}")))?;

        // 4. Decompress
        let nar_data = decompress_nar(&compressed_nar, &narinfo.compression)?;

        // 5. Add to local store
        let name = path.name();
        let store_dir = sui_compat::store_path::DEFAULT_STORE_DIR;
        let refs: Vec<String> = narinfo
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

        let _ = self.local_store.add_to_store(name, &nar_data, &refs).await?;

        Ok(Some(SubstituteResult::Substituted {
            cache_url: cache.base_url().to_string(),
            nar_size: narinfo.nar_size,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{HttpClient, HttpError, HttpResponse};
    use crate::traits::PathInfo;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ── Mock HTTP Client ────────────────────────────────────────

    /// A mock HTTP client that returns pre-configured responses.
    struct MockHttpClient {
        /// Map from URL to (status, body) for text responses.
        text_responses: HashMap<String, (u16, String)>,
        /// Map from URL to (status, bytes) for binary responses.
        byte_responses: HashMap<String, Vec<u8>>,
    }

    impl MockHttpClient {
        fn new() -> Self {
            Self {
                text_responses: HashMap::new(),
                byte_responses: HashMap::new(),
            }
        }

        fn with_text(mut self, url: &str, status: u16, body: &str) -> Self {
            self.text_responses
                .insert(url.to_string(), (status, body.to_string()));
            self
        }

        fn with_bytes(mut self, url: &str, data: Vec<u8>) -> Self {
            self.byte_responses.insert(url.to_string(), data);
            self
        }
    }

    #[async_trait::async_trait]
    impl HttpClient for MockHttpClient {
        async fn get(
            &self,
            url: &str,
            _headers: &[(&str, &str)],
        ) -> Result<HttpResponse, HttpError> {
            match self.text_responses.get(url) {
                Some((status, body)) => Ok(HttpResponse {
                    status: *status,
                    body: body.clone(),
                }),
                None => Ok(HttpResponse {
                    status: 404,
                    body: "not found".to_string(),
                }),
            }
        }

        async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, HttpError> {
            match self.byte_responses.get(url) {
                Some(data) => Ok(data.clone()),
                None => Err(HttpError::Request(format!("not found: {url}"))),
            }
        }
    }

    // ── Mock Store ──────────────────────────────────────────────

    /// A mock local store for testing substitution.
    struct MockLocalStore {
        valid_paths: Mutex<Vec<String>>,
        added: Mutex<Vec<(String, Vec<u8>, Vec<String>)>>,
    }

    impl MockLocalStore {
        fn new() -> Self {
            Self {
                valid_paths: Mutex::new(Vec::new()),
                added: Mutex::new(Vec::new()),
            }
        }

        fn with_valid_path(self, path: &str) -> Self {
            self.valid_paths.lock().unwrap().push(path.to_string());
            self
        }

        fn added_count(&self) -> usize {
            self.added.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl Store for MockLocalStore {
        async fn query_path_info(&self, path: &StorePath) -> StoreResult<Option<PathInfo>> {
            let abs = path.to_absolute_path();
            let valid = self.valid_paths.lock().unwrap();
            if valid.contains(&abs) {
                Ok(Some(PathInfo::new(&abs, "sha256:mock")))
            } else {
                Ok(None)
            }
        }

        async fn is_valid_path(&self, path: &StorePath) -> StoreResult<bool> {
            let abs = path.to_absolute_path();
            Ok(self.valid_paths.lock().unwrap().contains(&abs))
        }

        async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
            Ok(Vec::new())
        }

        async fn add_to_store(
            &self,
            name: &str,
            nar_data: &[u8],
            references: &[String],
        ) -> StoreResult<PathInfo> {
            self.added.lock().unwrap().push((
                name.to_string(),
                nar_data.to_vec(),
                references.to_vec(),
            ));
            // Also register as valid
            // We need a store path, but we don't know it from just the name.
            // For testing, we just record the addition.
            Ok(PathInfo::new(
                &format!("/nix/store/mock-{name}"),
                "sha256:mock",
            ))
        }
    }

    // ── Test helpers ────────────────────────────────────────────

    const TEST_HASH: &str = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
    const TEST_PATH: &str = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1";

    fn test_store_path() -> StorePath {
        StorePath::from_absolute_path(TEST_PATH).unwrap()
    }

    fn make_narinfo_text(compression: &str) -> String {
        format!(
            "StorePath: {TEST_PATH}\n\
             URL: nar/{TEST_HASH}.nar.{compression}\n\
             Compression: {compression}\n\
             FileHash: sha256:aaaa\n\
             FileSize: 100\n\
             NarHash: sha256:bbbb\n\
             NarSize: 200\n\
             References: \n\
             Sig: cache.nixos.org-1:fakesig\n"
        )
    }

    /// Create a minimal valid NAR (single regular file).
    fn make_nar_bytes() -> Vec<u8> {
        use sui_compat::nar::{NarNode, NarWriter};
        let node = NarNode::Regular {
            executable: false,
            contents: b"hello".to_vec(),
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        buf
    }

    fn compress_xz(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut compressed = Vec::new();
        let mut encoder = xz2::write::XzEncoder::new(&mut compressed, 1);
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap();
        compressed
    }

    fn compress_zstd(data: &[u8]) -> Vec<u8> {
        zstd::encode_all(std::io::Cursor::new(data), 1).unwrap()
    }

    fn make_cache_with_narinfo(
        base_url: &str,
        narinfo_text: &str,
        nar_url_path: &str,
        nar_bytes: Vec<u8>,
    ) -> Arc<BinaryCacheStore> {
        let client = MockHttpClient::new()
            .with_text(
                &format!("{base_url}/{TEST_HASH}.narinfo"),
                200,
                narinfo_text,
            )
            .with_bytes(&format!("{base_url}/{nar_url_path}"), nar_bytes);

        Arc::new(
            BinaryCacheStore::builder(base_url)
                .http_client(Box::new(client))
                .build(),
        )
    }

    fn make_empty_cache(base_url: &str) -> Arc<BinaryCacheStore> {
        let client = MockHttpClient::new();
        Arc::new(
            BinaryCacheStore::builder(base_url)
                .http_client(Box::new(client))
                .build(),
        )
    }

    fn make_error_cache(base_url: &str) -> Arc<BinaryCacheStore> {
        // Returns 500 for narinfo
        let client = MockHttpClient::new().with_text(
            &format!("{base_url}/{TEST_HASH}.narinfo"),
            500,
            "internal error",
        );
        Arc::new(
            BinaryCacheStore::builder(base_url)
                .http_client(Box::new(client))
                .build(),
        )
    }

    // ── Tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn substitute_already_present() {
        let store = Arc::new(MockLocalStore::new().with_valid_path(TEST_PATH));
        let sub = Substitutor::new(store.clone(), vec![]);

        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_already_present());
        assert_eq!(store.added_count(), 0);
    }

    #[tokio::test]
    async fn substitute_not_found_no_caches() {
        let store = Arc::new(MockLocalStore::new());
        let sub = Substitutor::new(store.clone(), vec![]);

        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_not_found());
    }

    #[tokio::test]
    async fn substitute_not_found_in_cache() {
        let store = Arc::new(MockLocalStore::new());
        let cache = make_empty_cache("https://cache.example.com");
        let sub = Substitutor::new(store.clone(), vec![cache]);

        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_not_found());
        assert_eq!(store.added_count(), 0);
    }

    #[tokio::test]
    async fn substitute_from_cache_uncompressed() {
        let nar = make_nar_bytes();
        let narinfo = make_narinfo_text("none");
        let store = Arc::new(MockLocalStore::new());
        let cache = make_cache_with_narinfo(
            "https://cache.example.com",
            &narinfo,
            &format!("nar/{TEST_HASH}.nar.none"),
            nar,
        );
        let sub = Substitutor::new(store.clone(), vec![cache]);

        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_substituted());
        assert_eq!(store.added_count(), 1);

        if let SubstituteResult::Substituted {
            cache_url,
            nar_size,
        } = result
        {
            assert_eq!(cache_url, "https://cache.example.com");
            assert_eq!(nar_size, 200);
        }
    }

    #[tokio::test]
    async fn substitute_from_cache_xz() {
        let nar = make_nar_bytes();
        let compressed = compress_xz(&nar);
        let narinfo = make_narinfo_text("xz");
        let store = Arc::new(MockLocalStore::new());
        let cache = make_cache_with_narinfo(
            "https://cache.example.com",
            &narinfo,
            &format!("nar/{TEST_HASH}.nar.xz"),
            compressed,
        );
        let sub = Substitutor::new(store.clone(), vec![cache]);

        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_substituted());
        assert_eq!(store.added_count(), 1);
    }

    #[tokio::test]
    async fn substitute_from_cache_zstd() {
        let nar = make_nar_bytes();
        let compressed = compress_zstd(&nar);
        let narinfo = make_narinfo_text("zstd");
        let store = Arc::new(MockLocalStore::new());
        let cache = make_cache_with_narinfo(
            "https://cache.example.com",
            &narinfo,
            &format!("nar/{TEST_HASH}.nar.zstd"),
            compressed,
        );
        let sub = Substitutor::new(store.clone(), vec![cache]);

        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_substituted());
        assert_eq!(store.added_count(), 1);
    }

    #[tokio::test]
    async fn substitute_multiple_caches_found_in_second() {
        let nar = make_nar_bytes();
        let narinfo = make_narinfo_text("none");
        let store = Arc::new(MockLocalStore::new());

        let cache1 = make_empty_cache("https://cache1.example.com");
        let cache2 = make_cache_with_narinfo(
            "https://cache2.example.com",
            &narinfo,
            &format!("nar/{TEST_HASH}.nar.none"),
            nar,
        );

        let sub = Substitutor::new(store.clone(), vec![cache1, cache2]);

        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_substituted());
        if let SubstituteResult::Substituted { cache_url, .. } = result {
            assert_eq!(cache_url, "https://cache2.example.com");
        }
    }

    #[tokio::test]
    async fn substitute_cache_error_falls_through() {
        let nar = make_nar_bytes();
        let narinfo = make_narinfo_text("none");
        let store = Arc::new(MockLocalStore::new());

        // First cache returns 500 error
        let cache1 = make_error_cache("https://broken.example.com");
        // Second cache works
        let cache2 = make_cache_with_narinfo(
            "https://good.example.com",
            &narinfo,
            &format!("nar/{TEST_HASH}.nar.none"),
            nar,
        );

        let sub = Substitutor::new(store.clone(), vec![cache1, cache2]);

        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_substituted());
        if let SubstituteResult::Substituted { cache_url, .. } = result {
            assert_eq!(cache_url, "https://good.example.com");
        }
    }

    #[tokio::test]
    async fn substitute_all_caches_error_returns_not_found() {
        let store = Arc::new(MockLocalStore::new());
        let cache1 = make_error_cache("https://broken1.example.com");
        let cache2 = make_error_cache("https://broken2.example.com");

        let sub = Substitutor::new(store.clone(), vec![cache1, cache2]);

        // Error caches return 500 which becomes Err, caught and continued
        // Eventually returns NotFound
        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_not_found());
    }

    #[tokio::test]
    async fn substitute_result_display_helpers() {
        assert!(SubstituteResult::AlreadyPresent.is_already_present());
        assert!(!SubstituteResult::AlreadyPresent.is_substituted());
        assert!(!SubstituteResult::AlreadyPresent.is_not_found());

        let sub = SubstituteResult::Substituted {
            cache_url: "https://example.com".to_string(),
            nar_size: 42,
        };
        assert!(sub.is_substituted());
        assert!(!sub.is_already_present());
        assert!(!sub.is_not_found());

        assert!(SubstituteResult::NotFound.is_not_found());
        assert!(!SubstituteResult::NotFound.is_already_present());
        assert!(!SubstituteResult::NotFound.is_substituted());
    }

    #[tokio::test]
    async fn substitute_registers_with_correct_references() {
        // Create narinfo with references
        let narinfo_text = format!(
            "StorePath: {TEST_PATH}\n\
             URL: nar/{TEST_HASH}.nar.none\n\
             Compression: none\n\
             FileHash: sha256:aaaa\n\
             FileSize: 100\n\
             NarHash: sha256:bbbb\n\
             NarSize: 200\n\
             References: abc123-glibc-2.37\n\
             Sig: cache.nixos.org-1:fakesig\n"
        );

        let nar = make_nar_bytes();
        let store = Arc::new(MockLocalStore::new());
        let cache = make_cache_with_narinfo(
            "https://cache.example.com",
            &narinfo_text,
            &format!("nar/{TEST_HASH}.nar.none"),
            nar,
        );

        let sub = Substitutor::new(store.clone(), vec![cache]);
        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_substituted());

        // Verify the reference was prefixed with /nix/store/
        let added = store.added.lock().unwrap();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].2, vec!["/nix/store/abc123-glibc-2.37"]);
    }

    #[tokio::test]
    async fn substitute_passes_absolute_references_through() {
        let narinfo_text = format!(
            "StorePath: {TEST_PATH}\n\
             URL: nar/{TEST_HASH}.nar.none\n\
             Compression: none\n\
             FileHash: sha256:aaaa\n\
             FileSize: 100\n\
             NarHash: sha256:bbbb\n\
             NarSize: 200\n\
             References: /nix/store/abc123-glibc-2.37\n\
             Sig: cache.nixos.org-1:fakesig\n"
        );

        let nar = make_nar_bytes();
        let store = Arc::new(MockLocalStore::new());
        let cache = make_cache_with_narinfo(
            "https://cache.example.com",
            &narinfo_text,
            &format!("nar/{TEST_HASH}.nar.none"),
            nar,
        );

        let sub = Substitutor::new(store.clone(), vec![cache]);
        sub.substitute(&test_store_path()).await.unwrap();

        let added = store.added.lock().unwrap();
        assert_eq!(added[0].2, vec!["/nix/store/abc123-glibc-2.37"]);
    }
}
