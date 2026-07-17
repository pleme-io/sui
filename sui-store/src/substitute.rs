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
        tracing::debug!(path = %path.to_absolute_path(), "checking store for path");

        // 1. Check if already in local store
        if self.local_store.is_valid_path(path).await? {
            return Ok(SubstituteResult::AlreadyPresent);
        }

        // 2. Try each binary cache in order
        for cache in &self.caches {
            match self.try_cache(cache, path).await {
                Ok(Some(result)) => return Ok(result),
                Ok(None) => {
                    tracing::debug!(
                        path = %path.to_absolute_path(),
                        cache_url = cache.base_url(),
                        "not found in cache",
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.to_absolute_path(),
                        cache_url = cache.base_url(),
                        error = %e,
                        "cache error, trying next",
                    );
                    continue;
                }
            }
        }

        tracing::debug!(path = %path.to_absolute_path(), "not found in any cache");
        Ok(SubstituteResult::NotFound)
    }

    /// Attempt to fetch and register a store path from a single cache.
    ///
    /// Returns `Ok(Some(result))` on success, `Ok(None)` if the path is not
    /// in this cache, or `Err` on a hard failure.
    ///
    /// The accept path is only reachable through a [`VerifiedNar`] — a value
    /// whose sole constructor runs BOTH the trust check (a valid trusted-key
    /// signature or a self-verifying CA path, per nix's `require-sigs`) AND
    /// the NAR-hash byte-integrity check. `add_to_store` is called only with
    /// a `VerifiedNar`, so registering an unverified path is unrepresentable
    /// in this pipeline.
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

        // 2. Trust gate (nix's exact model): a path is acceptable iff the
        //    cache does not require signatures, OR the path is a
        //    self-verifying content-addressed path, OR the narinfo carries a
        //    valid signature from one of this cache's trusted keys. A path
        //    with no valid trusted signature is REFUSED — never substituted.
        cache.check_narinfo_acceptable(&narinfo)?;

        // 3. Download NAR
        let compressed_nar = cache
            .fetch_nar(&narinfo.url)
            .await
            .map_err(|e| StoreError::Http(format!("NAR download failed: {e}")))?;

        // 4. Decompress
        let nar_data = decompress_nar(&compressed_nar, &narinfo.compression)?;

        // 5. NAR-hash byte-integrity gate: hash the received (decompressed)
        //    NAR bytes and assert they equal narinfo.narHash. A corrupt or
        //    MITM'd cache that returns different bytes than it advertised is
        //    rejected here — the only way to obtain a `VerifiedNar` is to
        //    pass this check.
        let verified = VerifiedNar::verify(cache, &narinfo, nar_data)?;

        // 6. Add to local store — takes VERIFIED bytes only.
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

        let _ = self
            .local_store
            .add_to_store(name, verified.bytes(), &refs)
            .await?;

        tracing::info!(
            path = %path.to_absolute_path(),
            cache_url = cache.base_url(),
            nar_size = narinfo.nar_size,
            "substituted from cache (signature + NAR-hash verified)",
        );

        Ok(Some(SubstituteResult::Substituted {
            cache_url: cache.base_url().to_string(),
            nar_size: narinfo.nar_size,
        }))
    }
}

/// A NAR whose bytes have passed BOTH the substituter trust check and the
/// NAR-hash byte-integrity check.
///
/// This is a proof-carrying newtype: the ONLY constructor,
/// [`VerifiedNar::verify`], runs the two checks and returns `Err` on
/// failure. Downstream code (`add_to_store`) accepts only a `VerifiedNar`,
/// so an unverified NAR cannot flow to the local store by construction —
/// the "download and TRUST" bug class is unrepresentable in this pipeline.
///
/// (Tier: this is truly-unrepresentable WITHIN the substitute pipeline — no
/// expressible path in `try_cache` reaches `add_to_store` without a
/// `VerifiedNar`. The underlying `Store::add_to_store` trait method still
/// takes raw bytes for other callers such as local builds, so at the trait
/// boundary the guarantee is checked-gate, not type-enforced.)
pub struct VerifiedNar {
    nar_data: Vec<u8>,
}

impl VerifiedNar {
    /// Verify `nar_data` against `narinfo` for a substitution from `cache`.
    ///
    /// Runs the trust check ([`BinaryCacheStore::check_narinfo_acceptable`])
    /// and the byte-integrity check
    /// ([`BinaryCacheStore::verify_nar_hash`]). Returns the wrapped bytes on
    /// success.
    ///
    /// # Errors
    ///
    /// - [`StoreError::SignatureVerificationFailed`] if the narinfo is not
    ///   trusted.
    /// - [`StoreError::NarHashMismatch`] if the bytes don't match the
    ///   declared NarHash.
    pub fn verify(
        cache: &BinaryCacheStore,
        narinfo: &sui_compat::narinfo::NarInfo,
        nar_data: Vec<u8>,
    ) -> StoreResult<Self> {
        // Re-assert the trust gate (defense in depth — verify() alone is a
        // complete gate even if a caller forgot the earlier check).
        cache.check_narinfo_acceptable(narinfo)?;
        // Byte integrity.
        BinaryCacheStore::verify_nar_hash(narinfo, &nar_data)?;
        Ok(Self { nar_data })
    }

    /// The verified NAR bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.nar_data
    }

    /// Consume and return the verified NAR bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.nar_data
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

    /// The real sha256 of `make_nar_bytes()` (120-byte NAR of a regular file
    /// containing `hello`). The NAR-hash gate compares the DECLARED NarHash to
    /// the hash of the actual bytes, so every mechanics test that expects a
    /// successful substitution MUST declare this exact hash — a fake NarHash
    /// is (correctly) rejected. Recompute with a probe if `make_nar_bytes`
    /// ever changes.
    const TEST_NAR_HASH: &str =
        "sha256:0a430879c266f8b57f4092a0f935cf3facd48bbccde5760d4748ca405171e969";
    const TEST_NAR_SIZE: u64 = 120;

    fn test_store_path() -> StorePath {
        StorePath::from_absolute_path(TEST_PATH).unwrap()
    }

    /// A narinfo whose `NarHash`/`NarSize` match `make_nar_bytes()`, so the
    /// NAR-hash integrity gate passes. References empty; the `Sig:` here is a
    /// placeholder — mechanics tests build the cache with
    /// `require_signatures(false)` so the trust gate is skipped (a cache the
    /// operator controls), isolating the download/decompress/register path.
    fn make_narinfo_text(compression: &str) -> String {
        format!(
            "StorePath: {TEST_PATH}\n\
             URL: nar/{TEST_HASH}.nar.{compression}\n\
             Compression: {compression}\n\
             FileHash: sha256:aaaa\n\
             FileSize: 100\n\
             NarHash: {TEST_NAR_HASH}\n\
             NarSize: {TEST_NAR_SIZE}\n\
             References: \n\
             Sig: cache.nixos.org-1:fakesig\n"
        )
    }

    /// Create a minimal valid NAR (single regular file). Its sha256 is
    /// [`TEST_NAR_HASH`] and its length is [`TEST_NAR_SIZE`].
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

    /// Build a mock cache serving `narinfo_text` + `nar_bytes`, with the
    /// signature trust gate turned OFF (`require_signatures(false)`) so these
    /// tests isolate the download/decompress/register + NAR-hash mechanics.
    /// The dedicated trust-gate tests below exercise `require_signatures(true)`
    /// with real keys.
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
                .require_signatures(false)
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
            assert_eq!(nar_size, TEST_NAR_SIZE);
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
        // Create narinfo with references (NarHash matches make_nar_bytes()).
        let narinfo_text = format!(
            "StorePath: {TEST_PATH}\n\
             URL: nar/{TEST_HASH}.nar.none\n\
             Compression: none\n\
             FileHash: sha256:aaaa\n\
             FileSize: 100\n\
             NarHash: {TEST_NAR_HASH}\n\
             NarSize: {TEST_NAR_SIZE}\n\
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
             NarHash: {TEST_NAR_HASH}\n\
             NarSize: {TEST_NAR_SIZE}\n\
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

    // ── Trust gate + NAR-hash gate (require_signatures = true) ──────────
    //
    // These exercise the SECURITY path end-to-end: with the secure default
    // (require_signatures = true), a substituted path is accepted only if it
    // carries a valid signature from a trusted key AND its NAR bytes hash to
    // the declared NarHash. A failed gate never reaches `add_to_store`.

    /// Sign the (empty-references) test narinfo with `signing_key` over Nix's
    /// canonical fingerprint (absolute refs — here none — sorted), returning
    /// `(narinfo_text, trusted_key_string)`.
    fn signed_narinfo_text(
        signing_key: &ed25519_dalek::SigningKey,
        key_name: &str,
    ) -> (String, String) {
        use ed25519_dalek::Signer;
        use sui_compat::hash::base64_encode;
        use sui_compat::signature::compute_fingerprint;

        // Empty references → fingerprint's ref field is empty; matches the
        // narinfo we build below (References: <empty>).
        let fingerprint =
            compute_fingerprint(TEST_PATH, TEST_NAR_HASH, TEST_NAR_SIZE, &[]);
        let sig = signing_key.sign(fingerprint.as_bytes());
        let sig_str = format!("{key_name}:{}", base64_encode(&sig.to_bytes()));
        let trusted_key =
            format!("{key_name}:{}", base64_encode(signing_key.verifying_key().as_bytes()));

        let narinfo_text = format!(
            "StorePath: {TEST_PATH}\n\
             URL: nar/{TEST_HASH}.nar.none\n\
             Compression: none\n\
             FileHash: sha256:aaaa\n\
             FileSize: 100\n\
             NarHash: {TEST_NAR_HASH}\n\
             NarSize: {TEST_NAR_SIZE}\n\
             References: \n\
             Sig: {sig_str}\n"
        );
        (narinfo_text, trusted_key)
    }

    /// (a) A valid signature from a TRUSTED key + a matching NAR hash is
    /// accepted and registered — real substitution still works with the gate
    /// on.
    #[tokio::test]
    async fn substitute_trusted_signature_accepted() {
        use ed25519_dalek::SigningKey;

        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let (narinfo_text, trusted_key) = signed_narinfo_text(&signing_key, "sui-test-1");

        let nar = make_nar_bytes();
        let store = Arc::new(MockLocalStore::new());
        let client = MockHttpClient::new()
            .with_text(
                &format!("https://cache.example.com/{TEST_HASH}.narinfo"),
                200,
                &narinfo_text,
            )
            .with_bytes(
                &format!("https://cache.example.com/nar/{TEST_HASH}.nar.none"),
                nar,
            );
        // require_signatures defaults to TRUE — do not opt out here.
        let cache = Arc::new(
            BinaryCacheStore::builder("https://cache.example.com")
                .http_client(Box::new(client))
                .trusted_keys(vec![trusted_key])
                .build(),
        );
        assert!(cache.require_signatures(), "secure default must be on");

        let sub = Substitutor::new(store.clone(), vec![cache]);
        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_substituted(), "trusted-signed path must substitute");
        assert_eq!(store.added_count(), 1);
    }

    /// (b1) A validly-signed narinfo whose signing key is NOT in the trusted
    /// set is REFUSED — nothing is registered, the result is NotFound (→ build).
    #[tokio::test]
    async fn substitute_untrusted_key_refused() {
        use ed25519_dalek::SigningKey;

        // Signed by key A, but the cache trusts an unrelated key B.
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let (narinfo_text, _real_key) = signed_narinfo_text(&signing_key, "sui-test-1");
        let untrusted = {
            use sui_compat::hash::base64_encode;
            let other = SigningKey::from_bytes(&[9u8; 32]);
            format!("sui-test-1:{}", base64_encode(other.verifying_key().as_bytes()))
        };

        let nar = make_nar_bytes();
        let store = Arc::new(MockLocalStore::new());
        let client = MockHttpClient::new()
            .with_text(
                &format!("https://cache.example.com/{TEST_HASH}.narinfo"),
                200,
                &narinfo_text,
            )
            .with_bytes(
                &format!("https://cache.example.com/nar/{TEST_HASH}.nar.none"),
                nar,
            );
        let cache = Arc::new(
            BinaryCacheStore::builder("https://cache.example.com")
                .http_client(Box::new(client))
                .trusted_keys(vec![untrusted])
                .build(),
        );

        let sub = Substitutor::new(store.clone(), vec![cache]);
        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_not_found(), "untrusted-key path must be refused");
        assert_eq!(store.added_count(), 0, "refused path must NOT be registered");
    }

    /// (b2) An UNSIGNED narinfo (no `Sig:`) with the secure default and no
    /// trusted keys is refused — nothing registered.
    #[tokio::test]
    async fn substitute_unsigned_refused_by_default() {
        let nar = make_nar_bytes();
        let store = Arc::new(MockLocalStore::new());
        // Narinfo with NO Sig line at all.
        let narinfo_text = format!(
            "StorePath: {TEST_PATH}\n\
             URL: nar/{TEST_HASH}.nar.none\n\
             Compression: none\n\
             FileHash: sha256:aaaa\n\
             FileSize: 100\n\
             NarHash: {TEST_NAR_HASH}\n\
             NarSize: {TEST_NAR_SIZE}\n\
             References: \n"
        );
        let client = MockHttpClient::new()
            .with_text(
                &format!("https://cache.example.com/{TEST_HASH}.narinfo"),
                200,
                &narinfo_text,
            )
            .with_bytes(
                &format!("https://cache.example.com/nar/{TEST_HASH}.nar.none"),
                nar,
            );
        // Default require_signatures = true, no trusted keys.
        let cache = Arc::new(
            BinaryCacheStore::builder("https://cache.example.com")
                .http_client(Box::new(client))
                .build(),
        );

        let sub = Substitutor::new(store.clone(), vec![cache]);
        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_not_found(), "unsigned path must be refused by default");
        assert_eq!(store.added_count(), 0);
    }

    /// (c) A path that passes the trust gate but whose downloaded NAR bytes do
    /// NOT hash to the declared NarHash is REJECTED by the byte-integrity gate
    /// — a corrupt/MITM'd cache cannot inject bytes. Nothing is registered.
    #[tokio::test]
    async fn substitute_nar_hash_mismatch_rejected() {
        use ed25519_dalek::SigningKey;

        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        // The narinfo declares TEST_NAR_HASH and is validly signed for it…
        let (narinfo_text, trusted_key) = signed_narinfo_text(&signing_key, "sui-test-1");

        // …but the cache serves DIFFERENT bytes than advertised.
        let tampered = {
            use sui_compat::nar::{NarNode, NarWriter};
            let node = NarNode::Regular {
                executable: false,
                contents: b"EVIL-INJECTED-PAYLOAD".to_vec(),
            };
            let mut buf = Vec::new();
            NarWriter::write(&mut buf, &node).unwrap();
            buf
        };

        let store = Arc::new(MockLocalStore::new());
        let client = MockHttpClient::new()
            .with_text(
                &format!("https://cache.example.com/{TEST_HASH}.narinfo"),
                200,
                &narinfo_text,
            )
            .with_bytes(
                &format!("https://cache.example.com/nar/{TEST_HASH}.nar.none"),
                tampered,
            );
        let cache = Arc::new(
            BinaryCacheStore::builder("https://cache.example.com")
                .http_client(Box::new(client))
                .trusted_keys(vec![trusted_key])
                .build(),
        );

        let sub = Substitutor::new(store.clone(), vec![cache]);
        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(
            result.is_not_found(),
            "NAR bytes != declared NarHash must be rejected (not registered)"
        );
        assert_eq!(store.added_count(), 0, "tampered NAR must NOT be registered");
    }

    /// A content-addressed (CA) path is accepted even with no trusted key and
    /// no signature — it is self-verifying — provided its NAR bytes still hash
    /// to the declared NarHash.
    #[tokio::test]
    async fn substitute_content_addressed_accepted_without_signature() {
        let nar = make_nar_bytes();
        let store = Arc::new(MockLocalStore::new());
        let narinfo_text = format!(
            "StorePath: {TEST_PATH}\n\
             URL: nar/{TEST_HASH}.nar.none\n\
             Compression: none\n\
             FileHash: sha256:aaaa\n\
             FileSize: 100\n\
             NarHash: {TEST_NAR_HASH}\n\
             NarSize: {TEST_NAR_SIZE}\n\
             References: \n\
             CA: fixed:r:sha256:{TEST_HASH}\n"
        );
        let client = MockHttpClient::new()
            .with_text(
                &format!("https://cache.example.com/{TEST_HASH}.narinfo"),
                200,
                &narinfo_text,
            )
            .with_bytes(
                &format!("https://cache.example.com/nar/{TEST_HASH}.nar.none"),
                nar,
            );
        // Secure default on, NO trusted keys — CA acceptance is the only path.
        let cache = Arc::new(
            BinaryCacheStore::builder("https://cache.example.com")
                .http_client(Box::new(client))
                .build(),
        );

        let sub = Substitutor::new(store.clone(), vec![cache]);
        let result = sub.substitute(&test_store_path()).await.unwrap();
        assert!(result.is_substituted(), "CA path must self-verify + substitute");
        assert_eq!(store.added_count(), 1);
    }
}
