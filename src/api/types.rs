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

/// Store verification result.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct VerifyResult {
    pub valid: i64,
    pub invalid: i64,
    pub missing: i64,
    pub errors: Vec<String>,
}

/// Closure request.
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct ClosureRequest {
    pub paths: Vec<String>,
}

/// Add-to-store request (metadata — binary payload comes separately).
#[derive(Debug, Clone, Serialize, Deserialize, InputObject)]
pub struct AddToStoreRequest {
    /// Desired store path name.
    pub name: Option<String>,
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

/// Flake metadata.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct FlakeMetadata {
    pub description: String,
    pub last_modified: i64,
    pub locked: serde_json::Value,
    pub resolved_url: Option<String>,
    pub url: Option<String>,
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

/// Search query parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    pub query: String,
    #[serde(default = "default_flake_ref")]
    pub flake_ref: String,
}

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

/// Fleet-wide status.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct FleetStatus {
    pub total_nodes: i64,
    pub online_nodes: i64,
    pub deploying_nodes: Option<i64>,
    pub failed_nodes: Option<i64>,
    pub nodes: Vec<FleetNode>,
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

// ── Cache ───────────────────────────────────────────────────

/// Binary cache info.
#[derive(Debug, Clone, Serialize, Deserialize, SimpleObject)]
pub struct CacheInfo {
    pub store_dir: String,
    pub want_mass_query: bool,
    pub priority: i32,
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
}
