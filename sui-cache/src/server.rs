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
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use crate::config::CacheConfig;
use crate::storage::StorageBackend;

/// Shared application state for all handlers.
#[derive(Clone)]
pub struct AppState {
    /// The storage backend.
    pub storage: Arc<dyn StorageBackend>,
    /// Cache configuration.
    pub config: CacheConfig,
}

/// Build the axum router for the binary cache server.
#[must_use]
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/nix-cache-info", get(cache_info))
        .route("/{hash_narinfo}", get(get_narinfo).put(put_narinfo))
        .route("/nar/{*path}", get(get_nar).put(put_nar))
        .with_state(state)
}

/// Start the cache server and listen for connections.
///
/// # Errors
///
/// Returns an error if binding or serving fails.
pub async fn serve(config: CacheConfig, storage: Arc<dyn StorageBackend>) -> Result<(), crate::CacheError> {
    let listen = config.listen.clone();
    let state = AppState {
        storage,
        config,
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
        Err(e) => {
            tracing::error!("get_narinfo error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
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

    match state.storage.put_narinfo(hash, &content).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            tracing::error!("put_narinfo error: {e}");
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
        Err(e) => {
            tracing::error!("get_nar error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
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
    match state.storage.put_nar(&nar_path, &body).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            tracing::error!("put_nar error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendConfig;
    use crate::storage::local::LocalStorage;
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
        };
        build_router(AppState { storage, config })
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
