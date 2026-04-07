//! Canonical API types shared across REST, GraphQL, and gRPC.
//!
//! Every type derives both `SimpleObject` (GraphQL) and `Serialize`/`Deserialize` (REST/gRPC).
//! Request body types derive only `Serialize`/`Deserialize` + `InputObject` where needed.

use async_graphql::{InputObject, SimpleObject};
use serde::{Deserialize, Serialize};

// ── Health ──────────────────────────────────────────────────

/// Health check response.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

impl HealthResponse {
    /// Build a healthy response with the current crate version.
    #[must_use]
    pub fn ok() -> Self {
        Self {
            status: "ok".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

// ── Store ───────────────────────────────────────────────────

/// Store path information.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct PathInfoResponse {
    pub path: String,
    pub nar_hash: String,
    pub nar_size: i64,
    pub references: Vec<String>,
    pub deriver: Option<String>,
    pub signatures: Vec<String>,
    pub registration_time: i64,
    pub content_address: Option<String>,
}

/// Garbage collection request.
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct GcRequest {
    /// Maximum bytes to free (0 = unlimited).
    pub max_freed: Option<i64>,
    /// Delete generations older than this duration (e.g., "30d").
    pub delete_older_than: Option<String>,
}

/// Garbage collection result.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct GcResult {
    pub paths_deleted: i64,
    pub bytes_freed: i64,
}

impl Default for GcResult {
    fn default() -> Self {
        Self {
            paths_deleted: 0,
            bytes_freed: 0,
        }
    }
}

/// Store verification result.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct VerifyResult {
    pub valid: i64,
    pub invalid: i64,
    pub missing: i64,
    pub errors: Vec<String>,
}

impl Default for VerifyResult {
    fn default() -> Self {
        Self {
            valid: 0,
            invalid: 0,
            missing: 0,
            errors: vec![],
        }
    }
}

/// Closure request.
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct ClosureRequest {
    pub paths: Vec<String>,
}

// ── Eval ────────────────────────────────────────────────────

/// Eval request.
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct EvalRequest {
    pub expression: String,
    pub flake_ref: Option<String>,
    pub attribute: Option<String>,
    pub pure: Option<bool>,
}

/// Eval result.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct EvalResult {
    pub value: serde_json::Value,
    pub errors: Vec<String>,
    pub drv_path: Option<String>,
    pub out_path: Option<String>,
}

impl EvalResult {
    /// Build an `EvalResult` from `sui_eval::eval` output, centralizing the
    /// success/error mapping shared by REST and GraphQL.
    #[must_use]
    pub fn from_eval(result: Result<sui_eval::Value, sui_eval::EvalError>) -> Self {
        match result {
            Ok(value) => Self {
                value: value.to_json(),
                errors: vec![],
                drv_path: None,
                out_path: None,
            },
            Err(e) => Self {
                value: serde_json::Value::Null,
                errors: vec![e.to_string()],
                drv_path: None,
                out_path: None,
            },
        }
    }

    /// Construct a stub `EvalResult` indicating a feature is not yet implemented.
    #[must_use]
    pub fn not_implemented() -> Self {
        Self {
            value: serde_json::Value::Null,
            errors: vec!["not yet implemented".to_string()],
            drv_path: None,
            out_path: None,
        }
    }
}

/// Flake metadata.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct FlakeMetadata {
    pub description: String,
    pub last_modified: i64,
    pub locked: serde_json::Value,
    pub resolved_url: Option<String>,
    pub url: Option<String>,
}

impl FlakeMetadata {
    /// Empty stub metadata (used before real flake integration).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            description: String::new(),
            last_modified: 0,
            locked: serde_json::json!({}),
            resolved_url: None,
            url: None,
        }
    }
}

/// Flake lock request.
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct FlakeLockRequest {
    pub flake_ref: Option<String>,
    pub update_inputs: Option<Vec<String>>,
}

/// Package search result.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct SearchResult {
    pub attribute: String,
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
}

/// Query parameters for flake evaluation endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlakeEvalQuery {
    /// Attribute path within the flake to evaluate.
    pub attribute: Option<String>,
}

/// Search query parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    pub query: String,
    #[serde(default = "default_flake_ref")]
    pub flake_ref: String,
}

/// Returns `"nixpkgs"` as the default flake reference for search queries.
fn default_flake_ref() -> String {
    "nixpkgs".to_string()
}

// ── Build ───────────────────────────────────────────────────

/// Build request.
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct BuildRequest {
    pub installable: String,
    pub system: Option<String>,
    pub max_jobs: Option<i32>,
    pub keep_going: Option<bool>,
}

/// Build status.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct BuildStatus {
    pub id: String,
    pub state: String,
    pub output_paths: Option<Vec<String>>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub log_lines: Vec<String>,
}

impl BuildStatus {
    /// A pending build stub (used before real build engine integration).
    #[must_use]
    pub fn pending_stub() -> Self {
        Self {
            id: "build-stub-0001".to_string(),
            state: "pending".to_string(),
            output_paths: None,
            started_at: None,
            completed_at: None,
            log_lines: vec![],
        }
    }
}

/// Build log query parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildLogQuery {
    #[serde(default)]
    pub follow: bool,
}

// ── Daemon ──────────────────────────────────────────────────

/// Daemon status.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct DaemonStatus {
    pub version: String,
    pub store_dir: String,
    pub active_connections: i64,
    pub trusted_users: Vec<String>,
    pub protocol_version: Option<String>,
}

impl DaemonStatus {
    /// Build the current daemon status (stub — no real connections yet).
    #[must_use]
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            store_dir: "/nix/store".to_string(),
            active_connections: 0,
            trusted_users: vec![],
            protocol_version: Some("1.0".to_string()),
        }
    }
}

/// Active daemon connection.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct DaemonConnection {
    pub id: String,
    pub user: String,
    pub trusted: bool,
    pub connected_at: Option<i64>,
}

// ── System ──────────────────────────────────────────────────

/// System rebuild request.
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct SystemRebuildRequest {
    /// Flake reference (e.g., `.#cid`).
    pub flake: Option<String>,
    /// Action: switch, boot, test, build.
    pub action: Option<String>,
    pub hostname: Option<String>,
}

/// System status.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct SystemStatus {
    pub generation: i64,
    pub config_path: String,
    pub boot_time: Option<i64>,
    pub nix_version: Option<String>,
    pub system: Option<String>,
}

impl SystemStatus {
    /// Build a stub system status (used before real system integration).
    #[must_use]
    pub fn stub() -> Self {
        Self {
            generation: 0,
            config_path: String::new(),
            boot_time: None,
            nix_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            system: None,
        }
    }
}

/// A system generation.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct Generation {
    pub number: i64,
    pub date: i64,
    pub current: bool,
    pub configuration_revision: Option<String>,
}

// ── Fleet ───────────────────────────────────────────────────

/// Fleet node.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct FleetNode {
    pub hostname: String,
    pub status: String,
    pub last_deployed: Option<i64>,
    pub current_generation: Option<i64>,
    pub system: Option<String>,
    pub flake_ref: Option<String>,
}

/// Fleet deploy request.
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct FleetDeployRequest {
    /// Node name or `@group`.
    pub target: String,
    pub flake: Option<String>,
    /// Strategy: parallel, rolling, canary.
    pub strategy: Option<String>,
}

/// Fleet deploy status.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct FleetDeployStatus {
    pub id: String,
    pub target: String,
    pub status: String,
    pub nodes: Vec<FleetNode>,
}

impl FleetDeployStatus {
    /// Build a pending deploy stub for the given target.
    #[must_use]
    pub fn pending(target: String) -> Self {
        Self {
            id: "deploy-stub-0001".to_string(),
            target,
            status: "pending".to_string(),
            nodes: vec![],
        }
    }
}

/// Fleet-wide status.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct FleetStatus {
    pub total_nodes: i64,
    pub online_nodes: i64,
    pub deploying_nodes: Option<i64>,
    pub failed_nodes: Option<i64>,
    pub nodes: Vec<FleetNode>,
}

impl FleetStatus {
    /// Build an empty fleet status (stub).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            total_nodes: 0,
            online_nodes: 0,
            deploying_nodes: Some(0),
            failed_nodes: Some(0),
            nodes: vec![],
        }
    }
}

/// Fleet rollback request.
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct FleetRollbackRequest {
    pub target: String,
}

// ── Profile ─────────────────────────────────────────────────

/// User profile.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct Profile {
    pub name: String,
    pub generation: i64,
    pub packages: Vec<String>,
    pub created_at: Option<i64>,
}

/// Profile install request.
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct ProfileInstallRequest {
    pub packages: Vec<String>,
    pub profile: Option<String>,
}

impl From<ProfileInstallRequest> for Profile {
    fn from(req: ProfileInstallRequest) -> Self {
        Self {
            name: req.profile.unwrap_or_else(|| "default".to_string()),
            generation: 1,
            packages: req.packages,
            created_at: None,
        }
    }
}

// ── Cache ───────────────────────────────────────────────────

/// Binary cache info.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct CacheInfo {
    pub store_dir: String,
    pub want_mass_query: bool,
    pub priority: i32,
}

impl Default for CacheInfo {
    fn default() -> Self {
        Self {
            store_dir: "/nix/store".to_string(),
            want_mass_query: true,
            priority: 40,
        }
    }
}

/// Push-to-cache request.
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct CachePushRequest {
    pub paths: Vec<String>,
    pub cache_url: String,
}

/// Sign-paths request.
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct CacheSignRequest {
    pub paths: Vec<String>,
    pub key_name: String,
}

// ── Pagination ──────────────────────────────────────────────

/// Common pagination query parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginationQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

/// Returns `100` as the default pagination limit.
fn default_limit() -> i64 {
    100
}

// ── Subscription Events ─────────────────────────────────────

/// A single build log line (for streaming).
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct BuildLogLine {
    pub build_id: String,
    pub line_number: i64,
    pub text: String,
    pub timestamp: i64,
}

/// A system event (for streaming).
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct SystemEvent {
    pub event_type: String,
    pub message: String,
    pub timestamp: i64,
    pub generation: Option<i64>,
}

// ── From impls: core types → API types ─────────────────

impl From<sui_store::PathInfo> for PathInfoResponse {
    fn from(i: sui_store::PathInfo) -> Self {
        Self {
            path: i.path,
            nar_hash: i.nar_hash,
            nar_size: i.nar_size,
            references: i.references,
            deriver: i.deriver,
            signatures: i.signatures,
            registration_time: i.registration_time,
            content_address: i.content_address,
        }
    }
}

impl From<sui_orchestrate::Node> for FleetNode {
    fn from(n: sui_orchestrate::Node) -> Self {
        Self {
            hostname: n.hostname,
            status: n.status.to_string(),
            last_deployed: n.last_deployed,
            current_generation: n.current_generation,
            system: n.system,
            flake_ref: Some(n.flake_ref),
        }
    }
}

impl From<sui_orchestrate::fleet::DeployResult> for FleetDeployStatus {
    fn from(r: sui_orchestrate::fleet::DeployResult) -> Self {
        Self {
            id: String::new(),
            target: r.target,
            status: if r.failed == 0 { "succeeded" } else { "failed" }.to_string(),
            nodes: vec![],
        }
    }
}

impl From<sui_build::BuildResult> for BuildStatus {
    fn from(r: sui_build::BuildResult) -> Self {
        Self {
            id: String::new(),
            state: if r.success { "succeeded" } else { "failed" }.to_string(),
            output_paths: Some(r.outputs.iter().map(|p| p.to_absolute_path()).collect()),
            started_at: None,
            completed_at: None,
            log_lines: r.log.lines().map(String::from).collect(),
        }
    }
}

impl From<sui_orchestrate::RebuildResult> for SystemStatus {
    fn from(r: sui_orchestrate::RebuildResult) -> Self {
        Self {
            generation: r.generation.unwrap_or(0),
            config_path: String::new(),
            boot_time: None,
            nix_version: None,
            system: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_info_from_core() {
        let core = sui_store::PathInfo {
            path: "/nix/store/abc-hello".to_string(),
            nar_hash: "sha256:dead".to_string(),
            nar_size: 1024,
            references: vec!["/nix/store/dep".to_string()],
            deriver: Some("/nix/store/abc.drv".to_string()),
            signatures: vec!["key:sig".to_string()],
            registration_time: 12345,
            content_address: Some("fixed:out:r:sha256:beef".to_string()),
        };
        let api: PathInfoResponse = core.into();
        assert_eq!(api.path, "/nix/store/abc-hello");
        assert_eq!(api.content_address, Some("fixed:out:r:sha256:beef".to_string()));
    }

    #[test]
    fn fleet_node_from_core() {
        let node = sui_orchestrate::Node::new("plo", ".#plo")
            .with_system("x86_64-linux");
        let api: FleetNode = node.into();
        assert_eq!(api.hostname, "plo");
        assert_eq!(api.flake_ref, Some(".#plo".to_string()));
    }

    #[test]
    fn rebuild_result_to_system_status() {
        let result = sui_orchestrate::RebuildResult {
            success: true,
            generation: Some(42),
            action: "switch".to_string(),
            log: "ok".to_string(),
            duration_secs: 1.5,
        };
        let status: SystemStatus = result.into();
        assert_eq!(status.generation, 42);
    }

    // ── From<PathInfo> — verify ALL fields including content_address ──

    #[test]
    fn path_info_from_core_all_fields() {
        let core = sui_store::PathInfo {
            path: "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1".to_string(),
            nar_hash: "sha256:1b0ri5lsf45dknj8bfxi1syz35kmab77apxxg1yrf33la1qm3kc7".to_string(),
            nar_size: 226552,
            references: vec![
                "/nix/store/3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37-8".to_string(),
                "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1".to_string(),
            ],
            deriver: Some("xb4y5iklhya4blk42k1cfkb8k07dpp4n-hello-2.12.1.drv".to_string()),
            signatures: vec![
                "cache.nixos.org-1:sig1==".to_string(),
                "my-key:sig2==".to_string(),
            ],
            registration_time: 1700000000,
            content_address: Some("fixed:out:r:sha256:deadbeef".to_string()),
        };

        let api: PathInfoResponse = core.clone().into();

        assert_eq!(api.path, core.path);
        assert_eq!(api.nar_hash, core.nar_hash);
        assert_eq!(api.nar_size, core.nar_size);
        assert_eq!(api.references, core.references);
        assert_eq!(api.deriver, core.deriver);
        assert_eq!(api.signatures, core.signatures);
        assert_eq!(api.registration_time, core.registration_time);
        assert_eq!(api.content_address, core.content_address);
    }

    #[test]
    fn path_info_from_core_none_fields() {
        let core = sui_store::PathInfo {
            path: "/nix/store/abc-minimal".to_string(),
            nar_hash: "sha256:000".to_string(),
            nar_size: 0,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 0,
            content_address: None,
        };

        let api: PathInfoResponse = core.into();

        assert!(api.deriver.is_none());
        assert!(api.content_address.is_none());
        assert!(api.references.is_empty());
        assert!(api.signatures.is_empty());
    }

    // ── From<Node> — verify ssh_target dropped, status stringified ──

    #[test]
    fn fleet_node_from_core_all_fields() {
        let mut node = sui_orchestrate::Node::new("plo", ".#plo")
            .with_ssh("root@10.0.0.1")
            .with_groups(vec!["prod".to_string()])
            .with_system("x86_64-linux");
        node.status = sui_orchestrate::NodeStatus::Online;
        node.current_generation = Some(99);
        node.last_deployed = Some(1700000000);

        let api: FleetNode = node.into();

        assert_eq!(api.hostname, "plo");
        assert_eq!(api.status, "online");
        assert_eq!(api.last_deployed, Some(1700000000));
        assert_eq!(api.current_generation, Some(99));
        assert_eq!(api.system, Some("x86_64-linux".to_string()));
        assert_eq!(api.flake_ref, Some(".#plo".to_string()));
        // ssh_target is NOT in the API type — it's dropped during conversion
    }

    #[test]
    fn fleet_node_from_core_unknown_status() {
        let node = sui_orchestrate::Node::new("ghost", ".#ghost");
        let api: FleetNode = node.into();
        assert_eq!(api.status, "unknown");
        assert!(api.last_deployed.is_none());
        assert!(api.current_generation.is_none());
        assert!(api.system.is_none());
    }

    #[test]
    fn fleet_node_from_core_deploying_status() {
        let mut node = sui_orchestrate::Node::new("node1", ".#node1");
        node.status = sui_orchestrate::NodeStatus::Deploying;
        let api: FleetNode = node.into();
        assert_eq!(api.status, "deploying");
    }

    #[test]
    fn fleet_node_from_core_failed_status() {
        let mut node = sui_orchestrate::Node::new("node2", ".#node2");
        node.status = sui_orchestrate::NodeStatus::Failed;
        let api: FleetNode = node.into();
        assert_eq!(api.status, "failed");
    }

    // ── From<DeployResult> — verify succeeded/failed mapping ──

    #[test]
    fn deploy_result_succeeded_maps_correctly() {
        let result = sui_orchestrate::fleet::DeployResult {
            target: "@prod".to_string(),
            strategy: "rolling".to_string(),
            total_nodes: 3,
            succeeded: 3,
            failed: 0,
            results: vec![],
            duration_secs: 10.0,
        };

        let api: FleetDeployStatus = result.into();

        assert_eq!(api.target, "@prod");
        assert_eq!(api.status, "succeeded");
        assert!(api.id.is_empty()); // id is set externally
        assert!(api.nodes.is_empty());
    }

    #[test]
    fn deploy_result_failed_maps_correctly() {
        let result = sui_orchestrate::fleet::DeployResult {
            target: "node1".to_string(),
            strategy: "parallel".to_string(),
            total_nodes: 2,
            succeeded: 1,
            failed: 1,
            results: vec![],
            duration_secs: 5.0,
        };

        let api: FleetDeployStatus = result.into();

        assert_eq!(api.target, "node1");
        assert_eq!(api.status, "failed");
    }

    // ── From<BuildResult> — verify StorePath outputs become strings ──

    #[test]
    fn build_result_success_maps_correctly() {
        let output = sui_compat::store_path::StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();

        let result = sui_build::BuildResult {
            outputs: vec![output],
            log: "building...\nfinished\n".to_string(),
            success: true,
            outcome: sui_build::BuildOutcome::Success,
            duration_secs: 30.0,
        };

        let api: BuildStatus = result.into();

        assert_eq!(api.state, "succeeded");
        assert!(api.id.is_empty()); // set externally
        let paths = api.output_paths.unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(
            paths[0],
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1"
        );
        assert_eq!(api.log_lines, vec!["building...", "finished"]);
    }

    #[test]
    fn build_result_failure_maps_correctly() {
        let result = sui_build::BuildResult {
            outputs: vec![],
            log: "error: build failed\n".to_string(),
            success: false,
            outcome: sui_build::BuildOutcome::Failure {
                stderr: "build failed".to_string(),
                exit_code: 1,
            },
            duration_secs: 1.0,
        };

        let api: BuildStatus = result.into();

        assert_eq!(api.state, "failed");
        assert_eq!(api.output_paths, Some(vec![]));
        assert_eq!(api.log_lines, vec!["error: build failed"]);
    }

    // ── From<RebuildResult> — verify generation mapping ──────

    #[test]
    fn rebuild_result_with_generation() {
        let result = sui_orchestrate::RebuildResult {
            success: true,
            generation: Some(99),
            action: "switch".to_string(),
            log: "switched to generation 99\n".to_string(),
            duration_secs: 45.0,
        };

        let status: SystemStatus = result.into();

        assert_eq!(status.generation, 99);
        assert!(status.config_path.is_empty());
        assert!(status.boot_time.is_none());
        assert!(status.nix_version.is_none());
        assert!(status.system.is_none());
    }

    #[test]
    fn rebuild_result_without_generation_defaults_to_zero() {
        let result = sui_orchestrate::RebuildResult {
            success: true,
            generation: None,
            action: "build".to_string(),
            log: "built successfully\n".to_string(),
            duration_secs: 20.0,
        };

        let status: SystemStatus = result.into();

        assert_eq!(status.generation, 0);
    }

    // ── Roundtrip: construct core → From → verify every field ──

    #[test]
    fn path_info_roundtrip_all_fields() {
        let core = sui_store::PathInfo {
            path: "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-hierarchical-0.1.0.1".to_string(),
            nar_hash: "sha256:abc123".to_string(),
            nar_size: 999999,
            references: vec![
                "/nix/store/dep1".to_string(),
                "/nix/store/dep2".to_string(),
                "/nix/store/dep3".to_string(),
            ],
            deriver: Some("builder.drv".to_string()),
            signatures: vec![
                "key1:aaa==".to_string(),
                "key2:bbb==".to_string(),
            ],
            registration_time: 1234567890,
            content_address: Some("text:sha256:xyz".to_string()),
        };

        // core → API
        let api: PathInfoResponse = core.clone().into();

        // Verify every field survived the conversion
        assert_eq!(api.path, core.path);
        assert_eq!(api.nar_hash, core.nar_hash);
        assert_eq!(api.nar_size, core.nar_size);
        assert_eq!(api.references.len(), 3);
        assert_eq!(api.references, core.references);
        assert_eq!(api.deriver, core.deriver);
        assert_eq!(api.signatures.len(), 2);
        assert_eq!(api.signatures, core.signatures);
        assert_eq!(api.registration_time, core.registration_time);
        assert_eq!(api.content_address, core.content_address);

        // API → JSON → API (serde roundtrip)
        let json = serde_json::to_string(&api).unwrap();
        let reparsed: PathInfoResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(reparsed.path, api.path);
        assert_eq!(reparsed.content_address, api.content_address);
    }
}
