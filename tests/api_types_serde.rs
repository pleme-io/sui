//! Serde roundtrip tests for all API types in `sui::api::types`.
//!
//! Every public request/response type is serialized to JSON and deserialized
//! back, verifying field-level equality. This catches schema drift between
//! the Rust structs and what clients actually see over the wire.

use sui::api::types::*;

// ── Helper ──────────────────────────────────────────────────────────

fn roundtrip<T>(val: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let json = serde_json::to_string(val).expect("serialize");
    serde_json::from_str(&json).expect("deserialize")
}

// ── Health ──────────────────────────────────────────────────────────

#[test]
fn health_response_roundtrip() {
    let orig = HealthResponse {
        status: "ok".into(),
        version: "0.1.0".into(),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.status, "ok");
    assert_eq!(rt.version, "0.1.0");
}

// ── Store types ─────────────────────────────────────────────────────

#[test]
fn path_info_response_roundtrip() {
    let orig = PathInfoResponse {
        path: "/nix/store/abc-hello".into(),
        nar_hash: "sha256:dead".into(),
        nar_size: 1024,
        references: vec!["/nix/store/dep".into()],
        deriver: Some("/nix/store/abc.drv".into()),
        signatures: vec!["key:sig".into()],
        registration_time: 12345,
        content_address: Some("fixed:out:r:sha256:beef".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.path, orig.path);
    assert_eq!(rt.nar_hash, orig.nar_hash);
    assert_eq!(rt.nar_size, orig.nar_size);
    assert_eq!(rt.references, orig.references);
    assert_eq!(rt.deriver, orig.deriver);
    assert_eq!(rt.signatures, orig.signatures);
    assert_eq!(rt.registration_time, orig.registration_time);
    assert_eq!(rt.content_address, orig.content_address);
}

#[test]
fn path_info_response_roundtrip_minimal() {
    let orig = PathInfoResponse {
        path: "/nix/store/min".into(),
        nar_hash: "sha256:000".into(),
        nar_size: 0,
        references: vec![],
        deriver: None,
        signatures: vec![],
        registration_time: 0,
        content_address: None,
    };
    let rt = roundtrip(&orig);
    assert!(rt.deriver.is_none());
    assert!(rt.content_address.is_none());
    assert!(rt.references.is_empty());
}

#[test]
fn gc_request_roundtrip() {
    let orig = GcRequest {
        max_freed: Some(1_000_000),
        delete_older_than: Some("30d".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.max_freed, Some(1_000_000));
    assert_eq!(rt.delete_older_than, Some("30d".into()));
}

#[test]
fn gc_request_roundtrip_none_fields() {
    let orig = GcRequest {
        max_freed: None,
        delete_older_than: None,
    };
    let rt = roundtrip(&orig);
    assert!(rt.max_freed.is_none());
    assert!(rt.delete_older_than.is_none());
}

#[test]
fn gc_result_roundtrip() {
    let orig = GcResult {
        paths_deleted: 42,
        bytes_freed: 1_000_000,
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.paths_deleted, 42);
    assert_eq!(rt.bytes_freed, 1_000_000);
}

#[test]
fn verify_result_roundtrip() {
    let orig = VerifyResult {
        valid: 100,
        invalid: 2,
        missing: 1,
        errors: vec!["corrupt nar".into()],
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.valid, 100);
    assert_eq!(rt.invalid, 2);
    assert_eq!(rt.missing, 1);
    assert_eq!(rt.errors, vec!["corrupt nar"]);
}

#[test]
fn closure_request_roundtrip() {
    let orig = ClosureRequest {
        paths: vec!["/nix/store/a".into(), "/nix/store/b".into()],
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.paths.len(), 2);
}

// ── Eval types ──────────────────────────────────────────────────────

#[test]
fn eval_request_roundtrip() {
    let orig = EvalRequest {
        expression: "1 + 1".into(),
        flake_ref: Some("nixpkgs".into()),
        attribute: Some("hello".into()),
        pure: Some(true),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.expression, "1 + 1");
    assert_eq!(rt.flake_ref, Some("nixpkgs".into()));
    assert_eq!(rt.attribute, Some("hello".into()));
    assert_eq!(rt.pure, Some(true));
}

#[test]
fn eval_result_roundtrip() {
    let orig = EvalResult {
        value: serde_json::json!(42),
        errors: vec![],
        drv_path: Some("/nix/store/x.drv".into()),
        out_path: Some("/nix/store/y-out".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.value, serde_json::json!(42));
    assert!(rt.errors.is_empty());
    assert_eq!(rt.drv_path, Some("/nix/store/x.drv".into()));
}

#[test]
fn eval_result_roundtrip_with_errors() {
    let orig = EvalResult {
        value: serde_json::Value::Null,
        errors: vec!["parse error".into(), "type error".into()],
        drv_path: None,
        out_path: None,
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.errors.len(), 2);
    assert!(rt.drv_path.is_none());
}

#[test]
fn flake_metadata_roundtrip() {
    let orig = FlakeMetadata {
        description: "A test flake".into(),
        last_modified: 1700000000,
        locked: serde_json::json!({"type": "github"}),
        resolved_url: Some("github:NixOS/nixpkgs".into()),
        url: Some("github:NixOS/nixpkgs".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.description, "A test flake");
    assert_eq!(rt.last_modified, 1700000000);
}

#[test]
fn flake_lock_request_roundtrip() {
    let orig = FlakeLockRequest {
        flake_ref: Some("nixpkgs".into()),
        update_inputs: Some(vec!["nixpkgs".into()]),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.flake_ref, Some("nixpkgs".into()));
    assert_eq!(rt.update_inputs, Some(vec!["nixpkgs".to_string()]));
}

#[test]
fn search_result_roundtrip() {
    let orig = SearchResult {
        attribute: "pkgs.hello".into(),
        name: "hello".into(),
        version: Some("2.12.1".into()),
        description: Some("A program that produces a familiar, friendly greeting".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.attribute, "pkgs.hello");
    assert_eq!(rt.name, "hello");
}

#[test]
fn search_query_roundtrip() {
    let orig = SearchQuery {
        query: "hello".into(),
        flake_ref: "nixpkgs".into(),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.query, "hello");
    assert_eq!(rt.flake_ref, "nixpkgs");
}

#[test]
fn search_query_default_flake_ref() {
    let json = r#"{"query":"hello"}"#;
    let q: SearchQuery = serde_json::from_str(json).unwrap();
    assert_eq!(q.flake_ref, "nixpkgs");
}

#[test]
fn flake_eval_query_roundtrip() {
    let orig = FlakeEvalQuery {
        attribute: Some("packages.x86_64-linux.default".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(
        rt.attribute,
        Some("packages.x86_64-linux.default".to_string())
    );
}

#[test]
fn flake_eval_query_none() {
    let orig = FlakeEvalQuery { attribute: None };
    let rt = roundtrip(&orig);
    assert!(rt.attribute.is_none());
}

// ── Build types ─────────────────────────────────────────────────────

#[test]
fn build_request_roundtrip() {
    let orig = BuildRequest {
        installable: "nixpkgs#hello".into(),
        system: Some("x86_64-linux".into()),
        max_jobs: Some(4),
        keep_going: Some(true),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.installable, "nixpkgs#hello");
    assert_eq!(rt.system, Some("x86_64-linux".into()));
    assert_eq!(rt.max_jobs, Some(4));
    assert_eq!(rt.keep_going, Some(true));
}

#[test]
fn build_status_roundtrip() {
    let orig = BuildStatus {
        id: "build-001".into(),
        state: "succeeded".into(),
        output_paths: Some(vec!["/nix/store/out".into()]),
        started_at: Some(1700000000),
        completed_at: Some(1700000100),
        log_lines: vec!["building...".into(), "done".into()],
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.id, "build-001");
    assert_eq!(rt.state, "succeeded");
    assert_eq!(rt.log_lines.len(), 2);
}

#[test]
fn build_log_query_default_follow() {
    let json = "{}";
    let q: BuildLogQuery = serde_json::from_str(json).unwrap();
    assert!(!q.follow);
}

#[test]
fn build_log_query_with_follow() {
    let json = r#"{"follow":true}"#;
    let q: BuildLogQuery = serde_json::from_str(json).unwrap();
    assert!(q.follow);
}

// ── Daemon types ────────────────────────────────────────────────────

#[test]
fn daemon_status_roundtrip() {
    let orig = DaemonStatus {
        version: "0.1.0".into(),
        store_dir: "/nix/store".into(),
        active_connections: 5,
        trusted_users: vec!["root".into(), "nixbld".into()],
        protocol_version: Some("1.0".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.active_connections, 5);
    assert_eq!(rt.trusted_users.len(), 2);
}

#[test]
fn daemon_connection_roundtrip() {
    let orig = DaemonConnection {
        id: "conn-1".into(),
        user: "root".into(),
        trusted: true,
        connected_at: Some(1700000000),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.id, "conn-1");
    assert!(rt.trusted);
}

// ── System types ────────────────────────────────────────────────────

#[test]
fn system_rebuild_request_roundtrip() {
    let orig = SystemRebuildRequest {
        flake: Some(".#myhost".into()),
        action: Some("switch".into()),
        hostname: Some("myhost".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.flake, Some(".#myhost".into()));
    assert_eq!(rt.action, Some("switch".into()));
}

#[test]
fn system_status_roundtrip() {
    let orig = SystemStatus {
        generation: 42,
        config_path: "/etc/nixos".into(),
        boot_time: Some(1700000000),
        nix_version: Some("0.1.0".into()),
        system: Some("x86_64-linux".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.generation, 42);
    assert_eq!(rt.system, Some("x86_64-linux".into()));
}

#[test]
fn generation_roundtrip() {
    let orig = Generation {
        number: 99,
        date: 1700000000,
        current: true,
        configuration_revision: Some("abc123".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.number, 99);
    assert!(rt.current);
}

// ── Fleet types ─────────────────────────────────────────────────────

#[test]
fn fleet_node_roundtrip() {
    let orig = FleetNode {
        hostname: "node1".into(),
        status: "online".into(),
        last_deployed: Some(1700000000),
        current_generation: Some(42),
        system: Some("x86_64-linux".into()),
        flake_ref: Some(".#node1".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.hostname, "node1");
    assert_eq!(rt.current_generation, Some(42));
}

#[test]
fn fleet_deploy_request_roundtrip() {
    let orig = FleetDeployRequest {
        target: "@prod".into(),
        flake: Some(".#".into()),
        strategy: Some("rolling".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.target, "@prod");
    assert_eq!(rt.strategy, Some("rolling".into()));
}

#[test]
fn fleet_deploy_status_roundtrip() {
    let orig = FleetDeployStatus {
        id: "deploy-001".into(),
        target: "@prod".into(),
        status: "succeeded".into(),
        nodes: vec![],
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.id, "deploy-001");
    assert!(rt.nodes.is_empty());
}

#[test]
fn fleet_status_roundtrip() {
    let orig = FleetStatus {
        total_nodes: 10,
        online_nodes: 8,
        deploying_nodes: Some(1),
        failed_nodes: Some(1),
        nodes: vec![],
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.total_nodes, 10);
    assert_eq!(rt.online_nodes, 8);
}

#[test]
fn fleet_rollback_request_roundtrip() {
    let orig = FleetRollbackRequest {
        target: "node1".into(),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.target, "node1");
}

// ── Profile types ───────────────────────────────────────────────────

#[test]
fn profile_roundtrip() {
    let orig = Profile {
        name: "default".into(),
        generation: 3,
        packages: vec!["hello".into(), "curl".into()],
        created_at: Some(1700000000),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.name, "default");
    assert_eq!(rt.packages.len(), 2);
}

#[test]
fn profile_install_request_roundtrip() {
    let orig = ProfileInstallRequest {
        packages: vec!["hello".into()],
        profile: Some("custom".into()),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.packages, vec!["hello"]);
    assert_eq!(rt.profile, Some("custom".into()));
}

// ── Cache types ─────────────────────────────────────────────────────

#[test]
fn cache_info_roundtrip() {
    let orig = CacheInfo {
        store_dir: "/nix/store".into(),
        want_mass_query: true,
        priority: 40,
    };
    let rt = roundtrip(&orig);
    assert!(rt.want_mass_query);
    assert_eq!(rt.priority, 40);
}

#[test]
fn cache_push_request_roundtrip() {
    let orig = CachePushRequest {
        paths: vec!["/nix/store/abc".into()],
        cache_url: "https://cache.example.com".into(),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.paths.len(), 1);
    assert_eq!(rt.cache_url, "https://cache.example.com");
}

#[test]
fn cache_sign_request_roundtrip() {
    let orig = CacheSignRequest {
        paths: vec!["/nix/store/abc".into()],
        key_name: "my-key".into(),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.key_name, "my-key");
}

// ── Pagination ──────────────────────────────────────────────────────

#[test]
fn pagination_query_defaults() {
    let json = "{}";
    let q: PaginationQuery = serde_json::from_str(json).unwrap();
    assert_eq!(q.limit, 100);
    assert_eq!(q.offset, 0);
}

#[test]
fn pagination_query_custom_values() {
    let json = r#"{"limit":50,"offset":10}"#;
    let q: PaginationQuery = serde_json::from_str(json).unwrap();
    assert_eq!(q.limit, 50);
    assert_eq!(q.offset, 10);
}

// ── Subscription event types ────────────────────────────────────────

#[test]
fn build_log_line_roundtrip() {
    let orig = BuildLogLine {
        build_id: "build-001".into(),
        line_number: 42,
        text: "building hello-2.12.1...".into(),
        timestamp: 1700000000,
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.build_id, "build-001");
    assert_eq!(rt.line_number, 42);
}

#[test]
fn system_event_roundtrip() {
    let orig = SystemEvent {
        event_type: "generation_switch".into(),
        message: "switched to generation 42".into(),
        timestamp: 1700000000,
        generation: Some(42),
    };
    let rt = roundtrip(&orig);
    assert_eq!(rt.event_type, "generation_switch");
    assert_eq!(rt.generation, Some(42));
}
