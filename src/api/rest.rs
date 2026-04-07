//! REST API routes (axum).
//!
//! Every endpoint from `spec/openapi.yaml` is implemented here with stub data.
//! Real implementations will be wired in later phases.

use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use sui_compat::store_path::StorePath;

use super::state::AppState;
use super::types::*;

/// Build the REST API router with all endpoint routes.
///
/// All handlers are mounted under `/api/v1/` with an additional `/health` at the root.
/// The returned router requires [`AppState`] to be provided via `.with_state()`.
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
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
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
        // Try parsing as full path or as basename
        let parsed = StorePath::from_absolute_path(&format!("/nix/store/{store_path}"))
            .or_else(|_| StorePath::from_absolute_path(&store_path));

        if let Ok(sp) = parsed {
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
    Json(GcResult {
        paths_deleted: 0,
        bytes_freed: 0,
    })
}

async fn verify_store() -> Json<VerifyResult> {
    Json(VerifyResult {
        valid: 0,
        invalid: 0,
        missing: 0,
        errors: vec![],
    })
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
    match sui_eval::eval(&req.expression) {
        Ok(value) => Json(EvalResult {
            value: value.to_json(),
            errors: vec![],
            drv_path: None,
            out_path: None,
        }),
        Err(e) => Json(EvalResult {
            value: serde_json::Value::Null,
            errors: vec![e.to_string()],
            drv_path: None,
            out_path: None,
        }),
    }
}

async fn eval_flake(
    Path(flake_ref): Path<String>,
    Query(query): Query<FlakeEvalQuery>,
) -> Json<EvalResult> {
    let _ = (flake_ref, query);
    Json(EvalResult {
        value: serde_json::Value::Null,
        errors: vec!["not yet implemented".to_string()],
        drv_path: None,
        out_path: None,
    })
}

async fn flake_show(Path(flake_ref): Path<String>) -> Json<serde_json::Value> {
    let _ = flake_ref;
    Json(serde_json::json!({}))
}

async fn flake_metadata(Path(flake_ref): Path<String>) -> Json<FlakeMetadata> {
    let _ = flake_ref;
    Json(FlakeMetadata {
        description: String::new(),
        last_modified: 0,
        locked: serde_json::json!({}),
        resolved_url: None,
        url: None,
    })
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
    (
        StatusCode::ACCEPTED,
        Json(BuildStatus {
            id: "build-stub-0001".to_string(),
            state: "pending".to_string(),
            output_paths: None,
            started_at: None,
            completed_at: None,
            log_lines: vec![],
        }),
    )
}

async fn get_build_status(Path(build_id): Path<String>) -> Json<BuildStatus> {
    Json(BuildStatus {
        id: build_id,
        state: "pending".to_string(),
        output_paths: None,
        started_at: None,
        completed_at: None,
        log_lines: vec![],
    })
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
    Json(DaemonStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        store_dir: "/nix/store".to_string(),
        active_connections: 0,
        trusted_users: vec![],
        protocol_version: Some("1.0".to_string()),
    })
}

async fn list_connections() -> Json<Vec<DaemonConnection>> {
    Json(vec![])
}

// ── System ──────────────────────────────────────────────────

async fn system_rebuild(
    Json(req): Json<SystemRebuildRequest>,
) -> (StatusCode, Json<SystemStatus>) {
    let _ = req;
    (
        StatusCode::ACCEPTED,
        Json(SystemStatus {
            generation: 0,
            config_path: String::new(),
            boot_time: None,
            nix_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            system: None,
        }),
    )
}

async fn system_status() -> Json<SystemStatus> {
    Json(SystemStatus {
        generation: 0,
        config_path: String::new(),
        boot_time: None,
        nix_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        system: None,
    })
}

async fn list_generations() -> Json<Vec<Generation>> {
    Json(vec![])
}

async fn system_rollback() -> Json<SystemStatus> {
    Json(SystemStatus {
        generation: 0,
        config_path: String::new(),
        boot_time: None,
        nix_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        system: None,
    })
}

// ── Fleet ───────────────────────────────────────────────────

async fn fleet_nodes() -> Json<Vec<FleetNode>> {
    Json(vec![])
}

async fn fleet_deploy(Json(req): Json<FleetDeployRequest>) -> (StatusCode, Json<FleetDeployStatus>) {
    let _ = &req;
    (
        StatusCode::ACCEPTED,
        Json(FleetDeployStatus {
            id: "deploy-stub-0001".to_string(),
            target: req.target,
            status: "pending".to_string(),
            nodes: vec![],
        }),
    )
}

async fn fleet_status() -> Json<FleetStatus> {
    Json(FleetStatus {
        total_nodes: 0,
        online_nodes: 0,
        deploying_nodes: Some(0),
        failed_nodes: Some(0),
        nodes: vec![],
    })
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
    Json(Profile {
        name: req.profile.unwrap_or_else(|| "default".to_string()),
        generation: 1,
        packages: req.packages,
        created_at: None,
    })
}

async fn rollback_profile() -> StatusCode {
    StatusCode::OK
}

// ── Cache ───────────────────────────────────────────────────

async fn cache_info() -> Json<CacheInfo> {
    Json(CacheInfo {
        store_dir: "/nix/store".to_string(),
        want_mass_query: true,
        priority: 40,
    })
}

async fn cache_push(Json(req): Json<CachePushRequest>) -> StatusCode {
    let _ = req;
    StatusCode::OK
}

async fn cache_sign(Json(req): Json<CacheSignRequest>) -> StatusCode {
    let _ = req;
    StatusCode::OK
}
