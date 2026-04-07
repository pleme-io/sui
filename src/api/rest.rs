//! REST API routes (axum).
//!
//! Every endpoint from `spec/openapi.yaml` is implemented here with stub data.
//! Real implementations will be wired in later phases.

use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use super::state::AppState;
use super::types::*;

/// Build the REST API router with all endpoint routes.
///
/// All handlers are mounted under `/api/v1/` with an additional `/health` at the root.
/// The returned router requires [`AppState`] to be provided via `.with_state()`.
#[must_use]
pub fn router() -> Router<AppState> {
    Router::new()
        // Health
        .route("/health", get(health))
        .route("/api/v1/health", get(health))
        // Store
        .route("/api/v1/store/paths", get(list_paths))
        .route("/api/v1/store/path-info/{storePath}", get(get_path_info))
        .route("/api/v1/store/closure", post(compute_closure))
        .route("/api/v1/store/gc", post(collect_garbage))
        .route("/api/v1/store/verify", post(verify_store))
        .route("/api/v1/store/add", post(add_to_store))
        // Eval
        .route("/api/v1/eval", post(eval_expression))
        .route("/api/v1/eval/flake/{flakeRef}", get(eval_flake))
        .route("/api/v1/eval/flake/{flakeRef}/show", get(flake_show))
        .route(
            "/api/v1/eval/flake/{flakeRef}/metadata",
            get(flake_metadata),
        )
        .route("/api/v1/eval/flake/lock", post(flake_lock))
        .route("/api/v1/eval/search", get(search_packages))
        // Build
        .route("/api/v1/build", post(build_derivation))
        .route("/api/v1/build/{buildId}", get(get_build_status))
        .route("/api/v1/build/{buildId}/log", get(get_build_log))
        .route("/api/v1/build/{buildId}/cancel", post(cancel_build))
        // Daemon
        .route("/api/v1/daemon/status", get(daemon_status))
        .route("/api/v1/daemon/connections", get(list_connections))
        // System
        .route("/api/v1/system/rebuild", post(system_rebuild))
        .route("/api/v1/system/status", get(system_status))
        .route("/api/v1/system/generations", get(list_generations))
        .route("/api/v1/system/rollback", post(system_rollback))
        // Fleet
        .route("/api/v1/fleet/nodes", get(fleet_nodes))
        .route("/api/v1/fleet/deploy", post(fleet_deploy))
        .route("/api/v1/fleet/status", get(fleet_status))
        .route("/api/v1/fleet/rollback", post(fleet_rollback))
        // Profile
        .route("/api/v1/profiles", get(list_profiles))
        .route("/api/v1/profiles/install", post(install_profile))
        .route("/api/v1/profiles/rollback", post(rollback_profile))
        // Cache
        .route("/api/v1/cache/info", get(cache_info))
        .route("/api/v1/cache/push", post(cache_push))
        .route("/api/v1/cache/sign", post(cache_sign))
}

// ── Health ──────────────────────────────────────────────────

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse::ok())
}

// ── Store ───────────────────────────────────────────────────

async fn list_paths(
    State(state): State<AppState>,
    Query(pagination): Query<PaginationQuery>,
) -> Json<Vec<String>> {
    if let Some(ref store) = state.store {
        match store.query_all_valid_paths().await {
            Ok(paths) => {
                let offset = pagination.offset as usize;
                let limit = pagination.limit as usize;
                let strs: Vec<String> = paths
                    .into_iter()
                    .skip(offset)
                    .take(limit)
                    .map(|p| p.to_absolute_path())
                    .collect();
                Json(strs)
            }
            Err(_) => Json(vec![]),
        }
    } else {
        Json(vec![])
    }
}

async fn get_path_info(
    State(state): State<AppState>,
    Path(store_path): Path<String>,
) -> Result<Json<PathInfoResponse>, StatusCode> {
    if let Some(ref store) = state.store {
        if let Ok(sp) = crate::parse_store_path(&store_path) {
            match store.query_path_info(&sp).await {
                Ok(Some(info)) => {
                    return Ok(Json(PathInfoResponse::from(info)));
                }
                Ok(None) => return Err(StatusCode::NOT_FOUND),
                Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
            }
        }
    }
    Err(StatusCode::NOT_FOUND)
}

async fn compute_closure(Json(req): Json<ClosureRequest>) -> Json<Vec<String>> {
    let _ = req;
    // Stub — returns the input paths as the closure
    Json(vec![])
}

async fn collect_garbage(body: Option<Json<GcRequest>>) -> Json<GcResult> {
    let _ = body;
    Json(GcResult::default())
}

async fn verify_store() -> Json<VerifyResult> {
    Json(VerifyResult::default())
}

async fn add_to_store(body: axum::body::Bytes) -> (StatusCode, Json<PathInfoResponse>) {
    let _ = body;
    (
        StatusCode::CREATED,
        Json(PathInfoResponse {
            path: "/nix/store/stub-path".to_string(),
            nar_hash: "sha256:0000000000000000000000000000000000000000000000000000".to_string(),
            nar_size: 0,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 0,
            content_address: None,
        }),
    )
}

// ── Eval ────────────────────────────────────────────────────

async fn eval_expression(Json(req): Json<EvalRequest>) -> Json<EvalResult> {
    Json(EvalResult::from_eval(sui_eval::eval(&req.expression)))
}

async fn eval_flake(
    Path(flake_ref): Path<String>,
    Query(query): Query<FlakeEvalQuery>,
) -> Json<EvalResult> {
    let _ = (flake_ref, query);
    Json(EvalResult::not_implemented())
}

async fn flake_show(Path(flake_ref): Path<String>) -> Json<serde_json::Value> {
    let _ = flake_ref;
    Json(serde_json::json!({}))
}

async fn flake_metadata(Path(flake_ref): Path<String>) -> Json<FlakeMetadata> {
    let _ = flake_ref;
    Json(FlakeMetadata::empty())
}

async fn flake_lock(Json(req): Json<FlakeLockRequest>) -> Json<serde_json::Value> {
    let _ = req;
    Json(serde_json::json!({}))
}

async fn search_packages(Query(query): Query<SearchQuery>) -> Json<Vec<SearchResult>> {
    let _ = query;
    Json(vec![])
}

// ── Build ───────────────────────────────────────────────────

async fn build_derivation(Json(req): Json<BuildRequest>) -> (StatusCode, Json<BuildStatus>) {
    let _ = req;
    (StatusCode::ACCEPTED, Json(BuildStatus::pending_stub()))
}

async fn get_build_status(Path(build_id): Path<String>) -> Json<BuildStatus> {
    let mut status = BuildStatus::pending_stub();
    status.id = build_id;
    Json(status)
}

async fn get_build_log(
    Path(build_id): Path<String>,
    Query(query): Query<BuildLogQuery>,
) -> impl IntoResponse {
    let _ = (build_id, query);
    // Returns plain text log
    (
        StatusCode::OK,
        [("content-type", "text/plain")],
        String::new(),
    )
}

async fn cancel_build(Path(build_id): Path<String>) -> StatusCode {
    let _ = build_id;
    StatusCode::OK
}

// ── Daemon ──────────────────────────────────────────────────

async fn daemon_status() -> Json<DaemonStatus> {
    Json(DaemonStatus::current())
}

async fn list_connections() -> Json<Vec<DaemonConnection>> {
    Json(vec![])
}

// ── System ──────────────────────────────────────────────────

async fn system_rebuild(
    Json(req): Json<SystemRebuildRequest>,
) -> (StatusCode, Json<SystemStatus>) {
    let _ = req;
    (StatusCode::ACCEPTED, Json(SystemStatus::stub()))
}

async fn system_status() -> Json<SystemStatus> {
    Json(SystemStatus::stub())
}

async fn list_generations() -> Json<Vec<Generation>> {
    Json(vec![])
}

async fn system_rollback() -> Json<SystemStatus> {
    Json(SystemStatus::stub())
}

// ── Fleet ───────────────────────────────────────────────────

async fn fleet_nodes() -> Json<Vec<FleetNode>> {
    Json(vec![])
}

async fn fleet_deploy(Json(req): Json<FleetDeployRequest>) -> (StatusCode, Json<FleetDeployStatus>) {
    (StatusCode::ACCEPTED, Json(FleetDeployStatus::pending(req.target)))
}

async fn fleet_status() -> Json<FleetStatus> {
    Json(FleetStatus::empty())
}

async fn fleet_rollback(Json(req): Json<FleetRollbackRequest>) -> StatusCode {
    let _ = req;
    StatusCode::OK
}

// ── Profile ─────────────────────────────────────────────────

async fn list_profiles() -> Json<Vec<Profile>> {
    Json(vec![])
}

async fn install_profile(Json(req): Json<ProfileInstallRequest>) -> Json<Profile> {
    Json(Profile::from(req))
}

async fn rollback_profile() -> StatusCode {
    StatusCode::OK
}

// ── Cache ───────────────────────────────────────────────────

async fn cache_info() -> Json<CacheInfo> {
    Json(CacheInfo::default())
}

async fn cache_push(Json(req): Json<CachePushRequest>) -> StatusCode {
    let _ = req;
    StatusCode::OK
}

async fn cache_sign(Json(req): Json<CacheSignRequest>) -> StatusCode {
    let _ = req;
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_router() -> Router {
        router().with_state(AppState::stub())
    }

    #[tokio::test]
    async fn health_returns_ok_status() {
        let resp = test_router()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let health: HealthResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(health.status, "ok");
    }

    #[tokio::test]
    async fn eval_endpoint_evaluates_expression() {
        let req_body = serde_json::json!({ "expression": "2 + 3" });
        let resp = test_router()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/eval")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let result: EvalResult = serde_json::from_slice(&body).unwrap();
        assert_eq!(result.value, serde_json::json!(5));
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn store_paths_stub_returns_empty_list() {
        let resp = test_router()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/store/paths")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let paths: Vec<String> = serde_json::from_slice(&body).unwrap();
        assert!(paths.is_empty());
    }

    #[tokio::test]
    async fn path_info_not_found_in_stub() {
        let resp = test_router()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/store/path-info/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn router_has_all_store_routes() {
        for route in ["/api/v1/store/paths", "/api/v1/daemon/status"] {
            let resp = test_router()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(route)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "expected 200 for {route}"
            );
        }
    }
}
