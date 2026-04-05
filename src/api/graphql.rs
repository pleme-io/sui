//! GraphQL schema (async-graphql).
//!
//! Full query/mutation/subscription coverage matching `spec/openapi.yaml`.
//! All resolvers return stub data — real implementations come in later phases.

use async_graphql::{Context, Object, Schema, Subscription};
use axum::Router;
use tokio_stream::Stream;

use super::types::*;

pub type SuiSchema = Schema<QueryRoot, MutationRoot, SubscriptionRoot>;

// ── Queries ─────────────────────────────────────────────────

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    // ── Health ──────────────────────────────────────────

    /// Health check.
    async fn health(&self, _ctx: &Context<'_>) -> HealthResponse {
        HealthResponse {
            status: "ok".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    // ── Store ───────────────────────────────────────────

    /// List all valid store paths.
    async fn store_paths(
        &self,
        _ctx: &Context<'_>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Vec<String> {
        let _ = (limit, offset);
        vec![]
    }

    /// Query store path info.
    async fn path_info(
        &self,
        _ctx: &Context<'_>,
        path: String,
    ) -> Option<PathInfoResponse> {
        let _ = path;
        None
    }

    // ── Daemon ──────────────────────────────────────────

    /// Daemon status.
    async fn daemon_status(&self, _ctx: &Context<'_>) -> DaemonStatus {
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            store_dir: "/nix/store".to_string(),
            active_connections: 0,
            trusted_users: vec![],
            protocol_version: Some("1.0".to_string()),
        }
    }

    /// List active daemon connections.
    async fn daemon_connections(&self, _ctx: &Context<'_>) -> Vec<DaemonConnection> {
        vec![]
    }

    // ── Eval ────────────────────────────────────────────

    /// Evaluate a flake reference.
    async fn eval_flake(
        &self,
        _ctx: &Context<'_>,
        flake_ref: String,
        attribute: Option<String>,
    ) -> EvalResult {
        let _ = (flake_ref, attribute);
        EvalResult {
            value: serde_json::Value::Null,
            errors: vec!["not yet implemented".to_string()],
            drv_path: None,
            out_path: None,
        }
    }

    /// Show flake outputs.
    async fn flake_show(
        &self,
        _ctx: &Context<'_>,
        flake_ref: String,
    ) -> serde_json::Value {
        let _ = flake_ref;
        serde_json::json!({})
    }

    /// Show flake metadata.
    async fn flake_metadata(
        &self,
        _ctx: &Context<'_>,
        flake_ref: String,
    ) -> FlakeMetadata {
        let _ = flake_ref;
        FlakeMetadata {
            description: String::new(),
            last_modified: 0,
            locked: serde_json::json!({}),
            resolved_url: None,
            url: None,
        }
    }

    /// Search flake packages.
    async fn search_packages(
        &self,
        _ctx: &Context<'_>,
        query: String,
        flake_ref: Option<String>,
    ) -> Vec<SearchResult> {
        let _ = (query, flake_ref);
        vec![]
    }

    // ── Build ───────────────────────────────────────────

    /// Get build status by ID.
    async fn build_status(&self, _ctx: &Context<'_>, build_id: String) -> BuildStatus {
        BuildStatus {
            id: build_id,
            state: "pending".to_string(),
            output_paths: None,
            started_at: None,
            completed_at: None,
            log_lines: vec![],
        }
    }

    /// Get build log lines.
    async fn build_log(
        &self,
        _ctx: &Context<'_>,
        build_id: String,
    ) -> Vec<String> {
        let _ = build_id;
        vec![]
    }

    // ── System ──────────────────────────────────────────

    /// Get current system status.
    async fn system_status(&self, _ctx: &Context<'_>) -> SystemStatus {
        SystemStatus {
            generation: 0,
            config_path: String::new(),
            boot_time: None,
            nix_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            system: None,
        }
    }

    /// List system generations.
    async fn system_generations(&self, _ctx: &Context<'_>) -> Vec<Generation> {
        vec![]
    }

    // ── Fleet ───────────────────────────────────────────

    /// List fleet nodes.
    async fn fleet_nodes(&self, _ctx: &Context<'_>) -> Vec<FleetNode> {
        vec![]
    }

    /// Get fleet-wide status.
    async fn fleet_status(&self, _ctx: &Context<'_>) -> FleetStatus {
        FleetStatus {
            total_nodes: 0,
            online_nodes: 0,
            deploying_nodes: Some(0),
            failed_nodes: Some(0),
            nodes: vec![],
        }
    }

    // ── Profile ─────────────────────────────────────────

    /// List user profiles.
    async fn profiles(&self, _ctx: &Context<'_>) -> Vec<Profile> {
        vec![]
    }

    // ── Cache ───────────────────────────────────────────

    /// Get binary cache info.
    async fn cache_info(&self, _ctx: &Context<'_>) -> CacheInfo {
        CacheInfo {
            store_dir: "/nix/store".to_string(),
            want_mass_query: true,
            priority: 40,
        }
    }
}

// ── Mutations ───────────────────────────────────────────────

pub struct MutationRoot;

#[Object]
impl MutationRoot {
    // ── Store ───────────────────────────────────────────

    /// Compute the closure of store paths.
    async fn compute_closure(
        &self,
        _ctx: &Context<'_>,
        paths: Vec<String>,
    ) -> Vec<String> {
        let _ = paths;
        vec![]
    }

    /// Run garbage collection.
    async fn collect_garbage(
        &self,
        _ctx: &Context<'_>,
        request: Option<GcRequest>,
    ) -> GcResult {
        let _ = request;
        GcResult {
            paths_deleted: 0,
            bytes_freed: 0,
        }
    }

    /// Verify store integrity.
    async fn verify_store(&self, _ctx: &Context<'_>) -> VerifyResult {
        VerifyResult {
            valid: 0,
            invalid: 0,
            missing: 0,
            errors: vec![],
        }
    }

    // ── Eval ────────────────────────────────────────────

    /// Evaluate a Nix expression.
    async fn eval(&self, _ctx: &Context<'_>, request: EvalRequest) -> EvalResult {
        match sui_eval::eval(&request.expression) {
            Ok(value) => EvalResult {
                value: value.to_json(),
                errors: vec![],
                drv_path: None,
                out_path: None,
            },
            Err(e) => EvalResult {
                value: serde_json::Value::Null,
                errors: vec![e.to_string()],
                drv_path: None,
                out_path: None,
            },
        }
    }

    /// Update flake lock file.
    async fn flake_lock(
        &self,
        _ctx: &Context<'_>,
        request: FlakeLockRequest,
    ) -> serde_json::Value {
        let _ = request;
        serde_json::json!({})
    }

    // ── Build ───────────────────────────────────────────

    /// Trigger a build.
    async fn build(&self, _ctx: &Context<'_>, request: BuildRequest) -> BuildStatus {
        let _ = request;
        BuildStatus {
            id: "build-stub-0001".to_string(),
            state: "pending".to_string(),
            output_paths: None,
            started_at: None,
            completed_at: None,
            log_lines: vec![],
        }
    }

    /// Cancel a running build.
    async fn cancel_build(&self, _ctx: &Context<'_>, build_id: String) -> bool {
        let _ = build_id;
        false
    }

    // ── System ──────────────────────────────────────────

    /// Rebuild and switch system configuration.
    async fn system_rebuild(
        &self,
        _ctx: &Context<'_>,
        request: SystemRebuildRequest,
    ) -> SystemStatus {
        let _ = request;
        SystemStatus {
            generation: 0,
            config_path: String::new(),
            boot_time: None,
            nix_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            system: None,
        }
    }

    /// Rollback to previous system generation.
    async fn system_rollback(&self, _ctx: &Context<'_>) -> SystemStatus {
        SystemStatus {
            generation: 0,
            config_path: String::new(),
            boot_time: None,
            nix_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            system: None,
        }
    }

    // ── Fleet ───────────────────────────────────────────

    /// Deploy to fleet nodes.
    async fn fleet_deploy(
        &self,
        _ctx: &Context<'_>,
        request: FleetDeployRequest,
    ) -> FleetDeployStatus {
        FleetDeployStatus {
            id: "deploy-stub-0001".to_string(),
            target: request.target,
            status: "pending".to_string(),
            nodes: vec![],
        }
    }

    /// Rollback fleet deployment.
    async fn fleet_rollback(
        &self,
        _ctx: &Context<'_>,
        target: String,
    ) -> bool {
        let _ = target;
        false
    }

    // ── Profile ─────────────────────────────────────────

    /// Install packages to profile.
    async fn install_profile(
        &self,
        _ctx: &Context<'_>,
        request: ProfileInstallRequest,
    ) -> Profile {
        Profile {
            name: request.profile.unwrap_or_else(|| "default".to_string()),
            generation: 1,
            packages: request.packages,
            created_at: None,
        }
    }

    /// Rollback profile to previous generation.
    async fn rollback_profile(&self, _ctx: &Context<'_>) -> bool {
        false
    }

    // ── Cache ───────────────────────────────────────────

    /// Push store paths to binary cache.
    async fn cache_push(
        &self,
        _ctx: &Context<'_>,
        request: CachePushRequest,
    ) -> bool {
        let _ = request;
        false
    }

    /// Sign store paths.
    async fn cache_sign(
        &self,
        _ctx: &Context<'_>,
        request: CacheSignRequest,
    ) -> bool {
        let _ = request;
        false
    }
}

// ── Subscriptions ───────────────────────────────────────────

pub struct SubscriptionRoot;

#[Subscription]
impl SubscriptionRoot {
    /// Stream build log lines for a running build.
    async fn build_log_stream(
        &self,
        build_id: String,
    ) -> impl Stream<Item = BuildLogLine> {
        // Stub — emits one line then completes. Real implementation will
        // read from a broadcast channel fed by the build engine.
        tokio_stream::once(BuildLogLine {
            build_id,
            line_number: 0,
            text: "build not yet started".to_string(),
            timestamp: 0,
        })
    }

    /// Stream system events (rebuilds, generation switches, etc.).
    async fn system_events(&self) -> impl Stream<Item = SystemEvent> {
        // Stub — emits one event then completes. Real implementation will
        // read from a broadcast channel fed by the system manager.
        tokio_stream::once(SystemEvent {
            event_type: "info".to_string(),
            message: "system event stream not yet implemented".to_string(),
            timestamp: 0,
            generation: None,
        })
    }
}

// ── Schema + Router ─────────────────────────────────────────

pub fn build_schema(state: super::state::AppState) -> SuiSchema {
    Schema::build(QueryRoot, MutationRoot, SubscriptionRoot)
        .data(state)
        .finish()
}

pub fn router<S>(schema: SuiSchema) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    use async_graphql_axum::{GraphQLRequest, GraphQLResponse, GraphQLSubscription};
    use axum::routing::post;
    use axum::Extension;

    Router::new()
        .route(
            "/graphql",
            post(|schema: Extension<SuiSchema>, req: GraphQLRequest| async move {
                let resp: GraphQLResponse = schema.execute(req.into_inner()).await.into();
                resp
            }),
        )
        .route_service("/graphql/ws", GraphQLSubscription::new(schema.clone()))
        .layer(Extension(schema))
}
