//! Axum HTTP server implementing the Nix binary cache protocol.
//!
//! Endpoints:
//! - `GET /nix-cache-info` — cache metadata
//! - `GET /{hash}.narinfo` — narinfo metadata
//! - `PUT /{hash}.narinfo` — upload narinfo
//! - `GET /nar/{path}` — download NAR blob
//! - `PUT /nar/{path}` — upload NAR blob

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use crate::config::CacheConfig;
use crate::signing::CacheSigner;
use crate::StorageBackend;
use sui_compat::narinfo::NarInfo;

/// Shared application state for all handlers.
#[derive(Clone)]
pub struct AppState {
    /// The storage backend.
    pub storage: Arc<dyn StorageBackend>,
    /// Cache configuration.
    pub config: CacheConfig,
    /// The ed25519 signer, loaded from `config.signing_key` at startup.
    ///
    /// When present, every narinfo is signed at ingest (`put_narinfo`) so
    /// the durable tier carries a `Sig:` field and every serving tier
    /// inherits it — the signature is content-addressed with the store path
    /// (the fingerprint is over the path), so it deduplicates for free. When
    /// `None`, the cache serves narinfo bytes verbatim (the legacy
    /// pass-through, fail-open behavior).
    pub signer: Option<Arc<CacheSigner>>,
}

/// Build the axum router for the binary cache server.
#[must_use]
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/nix-cache-info", get(cache_info))
        .route("/{hash_narinfo}", get(get_narinfo).put(put_narinfo))
        .route("/nar/{*path}", get(get_nar).put(put_nar))
        // Real Nix NARs routinely exceed axum's default 2 MiB body limit
        // (Go binaries, dockerTools image layers). Disable it so
        // `nix copy --to http://<sui>` write-through stores large NARs
        // instead of returning HTTP 413. (Closes ground-truth Gap B.)
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}

/// Start the cache server and listen for connections.
///
/// # Errors
///
/// Returns an error if binding or serving fails.
pub async fn serve(config: CacheConfig, storage: Arc<dyn StorageBackend>) -> Result<(), crate::CacheError> {
    let listen = config.listen.clone();

    // Load the ed25519 signing key (if configured) at startup. The key is
    // sourced from a file path — in production that path is a cofre/ESO-
    // materialized Kubernetes Secret mount, never a plaintext literal. When
    // no key is configured the daemon serves unsigned (the legacy behavior);
    // a warning is logged so the fail-open posture is never silent.
    let signer = match &config.signing_key {
        Some(path) => {
            let key_str = std::fs::read_to_string(path).map_err(crate::CacheError::Io)?;
            let signer = CacheSigner::from_secret_key_string(key_str.trim())?;
            tracing::info!(
                key_name = signer.key_name(),
                public_key = %signer.public_key_string(),
                "sui-cache signing ENABLED — every ingested narinfo is signed; \
                 distribute the public key to consumers as a trusted-public-key",
            );
            Some(Arc::new(signer))
        }
        None => {
            tracing::warn!(
                "sui-cache signing DISABLED (no signing_key configured) — narinfo \
                 served unsigned; consumers cannot verify integrity. Set a \
                 cofre/ESO-backed signing key to close the poisoned-write hole.",
            );
            None
        }
    };

    let state = AppState {
        storage,
        config,
        signer,
    };
    let app = build_router(state);

    tracing::info!("sui-cache listening on {listen}");
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(crate::CacheError::Io)?;
    axum::serve(listener, app)
        .await
        .map_err(crate::CacheError::Io)?;
    Ok(())
}

/// Sign narinfo text at ingest, returning the signed text.
///
/// Idempotent: if the narinfo already carries a signature under this
/// signer's key name, the text is returned unchanged (so a re-`put` of an
/// already-signed path does not double-sign). Otherwise the signer's
/// `keyname:base64sig` is appended and the narinfo re-serialized.
///
/// # Errors
///
/// Returns [`CacheError::NarInfo`](crate::CacheError::NarInfo) if the text
/// cannot be parsed as a narinfo.
fn sign_narinfo_text(signer: &CacheSigner, content: &str) -> Result<String, crate::CacheError> {
    let mut info = NarInfo::parse(content)
        .map_err(|e| crate::CacheError::NarInfo(e.to_string()))?;

    let key_prefix = format!("{}:", signer.key_name());
    if info.signatures.iter().any(|s| s.starts_with(&key_prefix)) {
        // Already signed by us — do not double-sign; return as-is.
        return Ok(content.to_string());
    }

    let sig = signer.sign_narinfo(&info);
    info.signatures.push(sig);
    Ok(info.serialize())
}

/// `GET /nix-cache-info` — returns cache metadata.
async fn cache_info(State(state): State<AppState>) -> impl IntoResponse {
    let body = format!(
        "StoreDir: {}\nWantMassQuery: {}\nPriority: {}\n",
        state.config.store_dir,
        if state.config.want_mass_query { 1 } else { 0 },
        state.config.priority,
    );
    (
        StatusCode::OK,
        [("content-type", "text/x-nix-cache-info")],
        body,
    )
}

/// `GET /{hash}.narinfo` — returns narinfo text.
async fn get_narinfo(
    State(state): State<AppState>,
    Path(hash_narinfo): Path<String>,
) -> impl IntoResponse {
    let Some(hash) = hash_narinfo.strip_suffix(".narinfo") else {
        return StatusCode::NOT_FOUND.into_response();
    };

    match state.storage.get_narinfo(hash).await {
        Ok(Some(content)) => (
            StatusCode::OK,
            [("content-type", "text/x-nix-narinfo")],
            content,
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        // A cache read is DEFINITIONALLY optional: if the storage cannot answer,
        // the honest answer to the client is "I don't have it" (404), never
        // "something is broken" (500). Nix treats a 404 as a cache miss and
        // builds; it treats a 500 as fatal, retries, and aborts the build. So a
        // 500 here converts a cold accelerator into a hard dependency and takes
        // down every consuming pipeline — which is exactly what it did.
        //
        // Loud at ERROR so the degradation is never silent: the request
        // survives, the fault stays visible.
        Err(e) => {
            tracing::error!(
                hash = %hash,
                error = %e,
                "get_narinfo: storage backend failed — DEGRADING TO CACHE MISS (404) so the \
                 client rebuilds instead of aborting; the backend needs attention",
            );
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// `PUT /{hash}.narinfo` — uploads narinfo text.
async fn put_narinfo(
    State(state): State<AppState>,
    Path(hash_narinfo): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let Some(hash) = hash_narinfo.strip_suffix(".narinfo") else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let content = match String::from_utf8(body.to_vec()) {
        Ok(s) => s,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    // Sign at ingest when a signer is configured, so the durable tier stores
    // the signed narinfo and every serving tier inherits the `Sig:`.
    let to_store = match &state.signer {
        Some(signer) => match sign_narinfo_text(signer, &content) {
            Ok(signed) => signed,
            Err(e) => {
                tracing::error!("put_narinfo signing error: {e}");
                return StatusCode::BAD_REQUEST.into_response();
            }
        },
        None => content,
    };

    // WRITE-PATH POLICY — deliberately NOT symmetric with the read path.
    //
    // A read has a well-defined "I don't have it" answer in the Nix binary-cache
    // protocol (404), and the client's correct response to it is to build. A
    // write has NO "I did not store it" success answer: returning 200 on a
    // failed write tells the client the path is cached when it is not, so the
    // push pipeline silently does nothing forever and no operator ever learns
    // the cache stopped filling. That is the silent-degradation bug this whole
    // change is against, just pointed the other way.
    //
    // So a failed write stays a 5xx — but the failure it reports is now much
    // rarer and much more honest: `TieredBackend` attempts EVERY durable tier
    // and succeeds if any one accepted the write, so this fires only when
    // nothing was stored anywhere. One broken durable tier (the Postgres-OOM
    // case) no longer fails the push.
    match state.storage.put_narinfo(hash, &to_store).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            tracing::error!(
                hash = %hash,
                error = %e,
                "put_narinfo: EVERY durable tier rejected the write — nothing stored; \
                 reporting failure rather than falsely acknowledging the upload",
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /nar/{path}` — returns a compressed NAR blob.
async fn get_nar(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let nar_path = format!("nar/{path}");
    match state.storage.get_nar(&nar_path).await {
        Ok(Some(data)) => {
            let content_type = if path.ends_with(".xz") {
                "application/x-xz"
            } else if path.ends_with(".zstd") || path.ends_with(".zst") {
                "application/zstd"
            } else {
                "application/x-nix-nar"
            };
            let mut headers = HeaderMap::new();
            headers.insert("content-type", content_type.parse().unwrap());
            (StatusCode::OK, headers, data).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        // Same rule as `get_narinfo`: an unanswerable read is a miss, not a
        // server error. See that handler for why 500 here is load-bearing-fatal.
        Err(e) => {
            tracing::error!(
                nar_path = %nar_path,
                error = %e,
                "get_nar: storage backend failed — DEGRADING TO CACHE MISS (404) so the \
                 client rebuilds instead of aborting; the backend needs attention",
            );
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// `PUT /nar/{path}` — uploads a compressed NAR blob.
async fn put_nar(
    State(state): State<AppState>,
    Path(path): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let nar_path = format!("nar/{path}");
    // See `put_narinfo` for the write-path policy and why it is deliberately
    // asymmetric with the read path.
    match state.storage.put_nar(&nar_path, &body).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            tracing::error!(
                nar_path = %nar_path,
                error = %e,
                "put_nar: EVERY durable tier rejected the write — nothing stored; \
                 reporting failure rather than falsely acknowledging the upload",
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendConfig;
    use crate::LocalStorage;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_app(dir: &std::path::Path) -> Router {
        let storage: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir));
        let config = CacheConfig {
            listen: "127.0.0.1:0".to_string(),
            backend: BackendConfig::Local {
                path: dir.to_path_buf(),
            },
            priority: 40,
            want_mass_query: true,
            store_dir: "/nix/store".to_string(),
            signing_key: None,
            require_sigs: false,
        };
        build_router(AppState { storage, config, signer: None })
    }

    async fn body_string(response: axum::http::Response<Body>) -> String {
        let body = response.into_body();
        let bytes = body.collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn body_bytes(response: axum::http::Response<Body>) -> Vec<u8> {
        let body = response.into_body();
        body.collect().await.unwrap().to_bytes().to_vec()
    }

    #[tokio::test]
    async fn cache_info_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path());

        let req = axum::http::Request::builder()
            .uri("/nix-cache-info")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = body_string(resp).await;
        assert!(body.contains("StoreDir: /nix/store"));
        assert!(body.contains("WantMassQuery: 1"));
        assert!(body.contains("Priority: 40"));
    }

    #[tokio::test]
    async fn get_narinfo_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path());

        let req = axum::http::Request::builder()
            .uri("/abc.narinfo")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put_then_get_narinfo() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path());

        let narinfo = "StorePath: /nix/store/abc-hello\nURL: nar/abc.nar.xz\nCompression: xz\nFileHash: sha256:aaa\nFileSize: 100\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n";

        // PUT narinfo.
        let req = axum::http::Request::builder()
            .method("PUT")
            .uri("/abc.narinfo")
            .body(Body::from(narinfo.to_string()))
            .unwrap();

        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // GET narinfo.
        let req = axum::http::Request::builder()
            .uri("/abc.narinfo")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = body_string(resp).await;
        assert!(body.contains("StorePath: /nix/store/abc-hello"));
    }

    #[tokio::test]
    async fn get_nar_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path());

        let req = axum::http::Request::builder()
            .uri("/nar/abc.nar.xz")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put_then_get_nar() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path());

        let nar_data = b"fake nar blob data";

        // PUT NAR.
        let req = axum::http::Request::builder()
            .method("PUT")
            .uri("/nar/xyz.nar.xz")
            .body(Body::from(nar_data.to_vec()))
            .unwrap();

        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // GET NAR.
        let req = axum::http::Request::builder()
            .uri("/nar/xyz.nar.xz")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = body_bytes(resp).await;
        assert_eq!(body, nar_data);
    }

    #[tokio::test]
    async fn get_narinfo_content_type() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage
            .put_narinfo("ct", "StorePath: /nix/store/ct-pkg\nURL: nar/ct.nar.xz\nCompression: xz\nFileHash: sha256:a\nFileSize: 1\nNarHash: sha256:b\nNarSize: 2\nReferences: \n")
            .await
            .unwrap();

        let app = test_app(dir.path());
        let req = axum::http::Request::builder()
            .uri("/ct.narinfo")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/x-nix-narinfo"
        );
    }

    #[tokio::test]
    async fn get_nar_xz_content_type() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage
            .put_nar("nar/test.nar.xz", b"data")
            .await
            .unwrap();

        let app = test_app(dir.path());
        let req = axum::http::Request::builder()
            .uri("/nar/test.nar.xz")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/x-xz"
        );
    }

    #[tokio::test]
    async fn cache_info_custom_priority() {
        let dir = tempfile::tempdir().unwrap();
        let storage: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir.path()));
        let config = CacheConfig {
            priority: 10,
            want_mass_query: false,
            ..CacheConfig::default()
        };
        let app = build_router(AppState {
            storage,
            config,
            signer: None,
        });

        let req = axum::http::Request::builder()
            .uri("/nix-cache-info")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let body = body_string(resp).await;
        assert!(body.contains("Priority: 10"));
        assert!(body.contains("WantMassQuery: 0"));
    }

    /// Sign-on-ingest proof: with a signer configured, a `PUT`-then-`GET`
    /// narinfo comes back carrying a `Sig:` that verifies against the
    /// signer's public key. This exercises the exact serve-path wiring
    /// (`put_narinfo` → `sign_narinfo_text`), not just the library.
    #[tokio::test]
    async fn put_narinfo_signs_at_ingest_and_get_returns_verifiable_sig() {
        use crate::signing::{verify_narinfo_signature, CacheSigner};

        let dir = tempfile::tempdir().unwrap();
        let storage: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir.path()));
        let signer = Arc::new(CacheSigner::generate("ingest-key".to_string()));
        let pk = signer.public_key_string();
        let config = CacheConfig {
            listen: "127.0.0.1:0".to_string(),
            backend: BackendConfig::Local { path: dir.path().to_path_buf() },
            priority: 40,
            want_mass_query: true,
            store_dir: "/nix/store".to_string(),
            signing_key: None,
            require_sigs: false,
        };
        let app = build_router(AppState { storage, config, signer: Some(signer.clone()) });

        // Unsigned narinfo (references deliberately unsorted).
        let narinfo = "StorePath: /nix/store/abc-hello\n\
                       URL: nar/abc.nar.xz\n\
                       Compression: xz\n\
                       FileHash: sha256:aaa\n\
                       FileSize: 100\n\
                       NarHash: sha256:bbb\n\
                       NarSize: 200\n\
                       References: zzz-b aaa-a\n";

        let req = axum::http::Request::builder()
            .method("PUT")
            .uri("/abc.narinfo")
            .body(Body::from(narinfo))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = axum::http::Request::builder()
            .uri("/abc.narinfo")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;

        let parsed = NarInfo::parse(&body).unwrap();
        assert_eq!(parsed.signatures.len(), 1, "GET must return a signed narinfo");
        assert!(parsed.signatures[0].starts_with("ingest-key:"));
        assert!(
            verify_narinfo_signature(&parsed, &parsed.signatures[0], &pk).unwrap(),
            "the ingest signature must verify against the signer public key",
        );
    }

    /// Re-`PUT` of an already-signed narinfo does not double-sign.
    #[tokio::test]
    async fn put_narinfo_is_idempotent_under_our_key() {
        use crate::signing::CacheSigner;

        let dir = tempfile::tempdir().unwrap();
        let storage: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir.path()));
        let signer = Arc::new(CacheSigner::generate("dedupe-key".to_string()));
        let config = CacheConfig {
            listen: "127.0.0.1:0".to_string(),
            backend: BackendConfig::Local { path: dir.path().to_path_buf() },
            priority: 40,
            want_mass_query: true,
            store_dir: "/nix/store".to_string(),
            signing_key: None,
            require_sigs: false,
        };
        let app = build_router(AppState { storage, config, signer: Some(signer) });

        let narinfo = "StorePath: /nix/store/def-x\n\
                       URL: nar/def.nar.xz\n\
                       Compression: xz\n\
                       FileHash: sha256:a\n\
                       FileSize: 1\n\
                       NarHash: sha256:b\n\
                       NarSize: 2\n\
                       References: \n";

        // First PUT (signs), GET the signed text, PUT it back.
        for uri in ["/def.narinfo"] {
            let req = axum::http::Request::builder()
                .method("PUT").uri(uri).body(Body::from(narinfo)).unwrap();
            assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
        }
        let req = axum::http::Request::builder().uri("/def.narinfo").body(Body::empty()).unwrap();
        let signed = body_string(app.clone().oneshot(req).await.unwrap()).await;

        let req = axum::http::Request::builder()
            .method("PUT").uri("/def.narinfo").body(Body::from(signed.clone())).unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);

        let req = axum::http::Request::builder().uri("/def.narinfo").body(Body::empty()).unwrap();
        let final_text = body_string(app.oneshot(req).await.unwrap()).await;
        let parsed = NarInfo::parse(&final_text).unwrap();
        assert_eq!(parsed.signatures.len(), 1, "must not double-sign on re-PUT");
    }

    // ── a broken backend degrades to a MISS, never a 500 (the incident) ────

    /// A backend that is reachable but cannot answer — the exact shape of the
    /// Postgres L2 whose tables were destroyed with its `emptyDir`.
    struct BrokenStorage;

    #[async_trait::async_trait]
    impl StorageBackend for BrokenStorage {
        async fn get_narinfo(&self, _hash: &str) -> Result<Option<String>, crate::CacheError> {
            Err(crate::CacheError::Io(std::io::Error::other(
                "postgres: error returned from database: relation \"sui_cache_narinfo\" does not exist",
            )))
        }
        async fn put_narinfo(&self, _hash: &str, _content: &str) -> Result<(), crate::CacheError> {
            Err(crate::CacheError::Io(std::io::Error::other("postgres: down")))
        }
        async fn get_nar(&self, _path: &str) -> Result<Option<Vec<u8>>, crate::CacheError> {
            Err(crate::CacheError::Io(std::io::Error::other(
                "postgres: error returned from database: relation \"sui_cache_nar\" does not exist",
            )))
        }
        async fn put_nar(&self, _path: &str, _data: &[u8]) -> Result<(), crate::CacheError> {
            Err(crate::CacheError::Io(std::io::Error::other("postgres: down")))
        }
        async fn delete(&self, _hash: &str) -> Result<(), crate::CacheError> {
            Err(crate::CacheError::Io(std::io::Error::other("postgres: down")))
        }
        async fn list_narinfos(&self) -> Result<Vec<String>, crate::CacheError> {
            Err(crate::CacheError::Io(std::io::Error::other("postgres: down")))
        }
    }

    fn broken_app() -> Router {
        let storage: Arc<dyn StorageBackend> = Arc::new(BrokenStorage);
        build_router(AppState {
            storage,
            config: CacheConfig::default(),
            signer: None,
        })
    }

    #[tokio::test]
    async fn broken_backend_narinfo_read_is_a_miss_not_a_server_error() {
        // THE defect. nix treats 404 as a cache miss and builds; it treats 500
        // as fatal, retries 5x, and aborts the build ~35s in before compiling
        // anything. An optional accelerator must never be able to do that.
        let resp = broken_app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/abc.narinfo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "a backend that cannot answer must report a MISS, never a 500",
        );
        assert_ne!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn broken_backend_nar_read_is_a_miss_not_a_server_error() {
        let resp = broken_app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/nar/abc.nar.xz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cache_info_still_answers_while_the_backend_is_broken() {
        // The cache must still advertise itself, so nix's substituter probe
        // succeeds and the miss path is exercised normally.
        let resp = broken_app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/nix-cache-info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_totally_failed_write_still_reports_failure() {
        // The deliberate asymmetry: there is no "I did not store it" success
        // answer in the protocol, so acknowledging a write that landed nowhere
        // would silently stop the cache from ever filling. Writes stay honest.
        let narinfo = "StorePath: /nix/store/abc-hello\nURL: nar/abc.nar.xz\nCompression: xz\nFileHash: sha256:a\nFileSize: 1\nNarHash: sha256:b\nNarSize: 2\nReferences: \n";
        let resp = broken_app()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/abc.narinfo")
                    .body(Body::from(narinfo))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn put_narinfo_bad_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path());

        let req = axum::http::Request::builder()
            .method("PUT")
            .uri("/bad.narinfo")
            .body(Body::from(vec![0xFF, 0xFE, 0xFD]))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
