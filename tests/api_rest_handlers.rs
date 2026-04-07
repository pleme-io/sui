//! Integration tests for the REST API handlers.
//!
//! Exercises each REST endpoint by sending HTTP requests through the axum
//! router in stub mode (no real Nix store). Verifies status codes, response
//! shapes, and JSON structure without needing a TCP listener.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;

use sui::api::state::AppState;
use sui::api::types::*;

fn app() -> Router {
    let state = AppState::stub();
    let schema = sui::api::graphql::build_schema(state.clone());
    Router::new()
        .merge(sui::api::rest::router())
        .merge(sui::api::graphql::router(schema))
        .with_state(state)
}

async fn get_json<T: serde::de::DeserializeOwned>(uri: &str) -> (StatusCode, T) {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let value: T = serde_json::from_slice(&body).unwrap();
    (status, value)
}

async fn post_json<Req: serde::Serialize, Resp: serde::de::DeserializeOwned>(
    uri: &str,
    body: &Req,
) -> (StatusCode, Resp) {
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Resp = serde_json::from_slice(&bytes).unwrap();
    (status, value)
}

// ── Health ──────────────────────────────────────────────────────────

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let (status, body): (_, HealthResponse) = get_json("/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.status, "ok");
    assert!(!body.version.is_empty());
}

#[tokio::test]
async fn health_api_v1_endpoint_returns_ok() {
    let (status, body): (_, HealthResponse) = get_json("/api/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.status, "ok");
}

// ── Store (stub mode) ──────────────────────────────────────────────

#[tokio::test]
async fn list_paths_stub_returns_empty() {
    let (status, body): (_, Vec<String>) = get_json("/api/v1/store/paths").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}

#[tokio::test]
async fn list_paths_with_pagination() {
    let (status, body): (_, Vec<String>) =
        get_json("/api/v1/store/paths?limit=10&offset=0").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}

#[tokio::test]
async fn get_path_info_stub_returns_not_found() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/api/v1/store/path-info/abc123-hello")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn compute_closure_stub_returns_empty() {
    let req = ClosureRequest {
        paths: vec!["/nix/store/abc".into()],
    };
    let (status, body): (_, Vec<String>) = post_json("/api/v1/store/closure", &req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}

#[tokio::test]
async fn collect_garbage_stub_returns_zeros() {
    let req = GcRequest {
        max_freed: None,
        delete_older_than: None,
    };
    let (status, body): (_, GcResult) = post_json("/api/v1/store/gc", &req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.paths_deleted, 0);
    assert_eq!(body.bytes_freed, 0);
}

#[tokio::test]
async fn verify_store_stub_returns_zeros() {
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/store/verify")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let result: VerifyResult = serde_json::from_slice(&body).unwrap();
    assert_eq!(result.valid, 0);
    assert_eq!(result.invalid, 0);
    assert_eq!(result.missing, 0);
}

#[tokio::test]
async fn add_to_store_stub_returns_created() {
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/store/add")
                .header("content-type", "application/octet-stream")
                .body(Body::from(vec![0u8; 10]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let info: PathInfoResponse = serde_json::from_slice(&body).unwrap();
    assert!(info.path.contains("stub"));
}

// ── Eval ────────────────────────────────────────────────────────────

#[tokio::test]
async fn eval_expression_returns_result() {
    let req = EvalRequest {
        expression: "1 + 1".into(),
        flake_ref: None,
        attribute: None,
        pure: None,
    };
    let (status, body): (_, EvalResult) = post_json("/api/v1/eval", &req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.value, serde_json::json!(2));
    assert!(body.errors.is_empty());
}

#[tokio::test]
async fn eval_expression_invalid_returns_errors() {
    let req = EvalRequest {
        expression: "let x = ; in x".into(),
        flake_ref: None,
        attribute: None,
        pure: None,
    };
    let (status, body): (_, EvalResult) = post_json("/api/v1/eval", &req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.errors.is_empty());
    assert_eq!(body.value, serde_json::Value::Null);
}

#[tokio::test]
async fn eval_flake_stub_returns_not_implemented() {
    let (status, body): (_, EvalResult) = get_json("/api/v1/eval/flake/nixpkgs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.errors.is_empty());
}

#[tokio::test]
async fn flake_show_stub_returns_empty_object() {
    let (status, body): (_, serde_json::Value) =
        get_json("/api/v1/eval/flake/nixpkgs/show").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({}));
}

#[tokio::test]
async fn flake_metadata_stub_returns_empty() {
    let (status, body): (_, FlakeMetadata) =
        get_json("/api/v1/eval/flake/nixpkgs/metadata").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.description.is_empty());
}

#[tokio::test]
async fn search_packages_stub_returns_empty() {
    let (status, body): (_, Vec<SearchResult>) =
        get_json("/api/v1/eval/search?query=hello").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}

// ── Build ───────────────────────────────────────────────────────────

#[tokio::test]
async fn build_derivation_returns_accepted() {
    let req = BuildRequest {
        installable: "nixpkgs#hello".into(),
        system: None,
        max_jobs: None,
        keep_going: None,
    };
    let (status, body): (_, BuildStatus) = post_json("/api/v1/build", &req).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body.state, "pending");
    assert!(!body.id.is_empty());
}

#[tokio::test]
async fn get_build_status_returns_pending() {
    let (status, body): (_, BuildStatus) = get_json("/api/v1/build/build-001").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.id, "build-001");
    assert_eq!(body.state, "pending");
}

#[tokio::test]
async fn get_build_log_returns_empty() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/api/v1/build/build-001/log")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(body.is_empty());
}

#[tokio::test]
async fn cancel_build_returns_ok() {
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/build/build-001/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Daemon ──────────────────────────────────────────────────────────

#[tokio::test]
async fn daemon_status_returns_version() {
    let (status, body): (_, DaemonStatus) = get_json("/api/v1/daemon/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.version.is_empty());
    assert_eq!(body.store_dir, "/nix/store");
}

#[tokio::test]
async fn list_connections_returns_empty() {
    let (status, body): (_, Vec<DaemonConnection>) =
        get_json("/api/v1/daemon/connections").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}

// ── System ──────────────────────────────────────────────────────────

#[tokio::test]
async fn system_rebuild_returns_accepted() {
    let req = SystemRebuildRequest {
        flake: None,
        action: Some("switch".into()),
        hostname: None,
    };
    let (status, body): (_, SystemStatus) = post_json("/api/v1/system/rebuild", &req).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body.generation, 0);
}

#[tokio::test]
async fn system_status_returns_ok() {
    let (status, body): (_, SystemStatus) = get_json("/api/v1/system/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.generation, 0);
}

#[tokio::test]
async fn list_generations_returns_empty() {
    let (status, body): (_, Vec<Generation>) = get_json("/api/v1/system/generations").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}

#[tokio::test]
async fn system_rollback_returns_ok() {
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/system/rollback")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Fleet ───────────────────────────────────────────────────────────

#[tokio::test]
async fn fleet_nodes_returns_empty() {
    let (status, body): (_, Vec<FleetNode>) = get_json("/api/v1/fleet/nodes").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}

#[tokio::test]
async fn fleet_deploy_returns_accepted() {
    let req = FleetDeployRequest {
        target: "@prod".into(),
        flake: None,
        strategy: None,
    };
    let (status, body): (_, FleetDeployStatus) = post_json("/api/v1/fleet/deploy", &req).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body.target, "@prod");
}

#[tokio::test]
async fn fleet_status_returns_zeros() {
    let (status, body): (_, FleetStatus) = get_json("/api/v1/fleet/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.total_nodes, 0);
}

#[tokio::test]
async fn fleet_rollback_returns_ok() {
    let req = FleetRollbackRequest {
        target: "node1".into(),
    };
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/fleet/rollback")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Profile ─────────────────────────────────────────────────────────

#[tokio::test]
async fn list_profiles_returns_empty() {
    let (status, body): (_, Vec<Profile>) = get_json("/api/v1/profiles").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}

#[tokio::test]
async fn install_profile_returns_profile() {
    let req = ProfileInstallRequest {
        packages: vec!["hello".into()],
        profile: Some("test".into()),
    };
    let (status, body): (_, Profile) = post_json("/api/v1/profiles/install", &req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.name, "test");
    assert_eq!(body.packages, vec!["hello"]);
}

#[tokio::test]
async fn install_profile_default_name() {
    let req = ProfileInstallRequest {
        packages: vec!["curl".into()],
        profile: None,
    };
    let (status, body): (_, Profile) = post_json("/api/v1/profiles/install", &req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.name, "default");
}

#[tokio::test]
async fn rollback_profile_returns_ok() {
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/profiles/rollback")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Cache ───────────────────────────────────────────────────────────

#[tokio::test]
async fn cache_info_returns_defaults() {
    let (status, body): (_, CacheInfo) = get_json("/api/v1/cache/info").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.store_dir, "/nix/store");
    assert!(body.want_mass_query);
    assert_eq!(body.priority, 40);
}

#[tokio::test]
async fn cache_push_returns_ok() {
    let req = CachePushRequest {
        paths: vec!["/nix/store/abc".into()],
        cache_url: "https://cache.example.com".into(),
    };
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/cache/push")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn cache_sign_returns_ok() {
    let req = CacheSignRequest {
        paths: vec!["/nix/store/abc".into()],
        key_name: "my-key".into(),
    };
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/cache/sign")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── 404 for unknown routes ──────────────────────────────────────────

#[tokio::test]
async fn unknown_route_returns_404() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/api/v1/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
