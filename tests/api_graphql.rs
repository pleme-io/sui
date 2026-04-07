//! GraphQL schema tests for the sui API server.
//!
//! Exercises the GraphQL schema directly via `async-graphql`'s
//! `Schema::execute` without a network layer. Tests cover queries,
//! mutations, and basic introspection.

use async_graphql::Request;
use sui::api::graphql::build_schema;
use sui::api::state::AppState;

fn schema() -> sui::api::graphql::SuiSchema {
    build_schema(AppState::stub())
}

// ── Query: health ───────────────────────────────────────────────────

#[tokio::test]
async fn graphql_health_query() {
    let resp = schema()
        .execute(Request::new("{ health { status version } }"))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["health"]["status"], "ok");
    assert!(!data["health"]["version"].as_str().unwrap().is_empty());
}

// ── Query: store paths ──────────────────────────────────────────────

#[tokio::test]
async fn graphql_store_paths_stub() {
    let resp = schema()
        .execute(Request::new("{ storePaths }"))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert!(data["storePaths"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn graphql_path_info_stub() {
    let resp = schema()
        .execute(Request::new(
            r#"{ pathInfo(path: "/nix/store/abc-hello") { path narHash } }"#,
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert!(data["pathInfo"].is_null());
}

// ── Query: daemon ───────────────────────────────────────────────────

#[tokio::test]
async fn graphql_daemon_status() {
    let resp = schema()
        .execute(Request::new(
            "{ daemonStatus { version storeDir activeConnections } }",
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["daemonStatus"]["storeDir"], "/nix/store");
}

#[tokio::test]
async fn graphql_daemon_connections() {
    let resp = schema()
        .execute(Request::new("{ daemonConnections { id user trusted } }"))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert!(data["daemonConnections"].as_array().unwrap().is_empty());
}

// ── Query: eval ─────────────────────────────────────────────────────

#[tokio::test]
async fn graphql_eval_flake_stub() {
    let resp = schema()
        .execute(Request::new(
            r#"{ evalFlake(flakeRef: "nixpkgs") { value errors } }"#,
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert!(!data["evalFlake"]["errors"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn graphql_flake_show_stub() {
    let resp = schema()
        .execute(Request::new(
            r#"{ flakeShow(flakeRef: "nixpkgs") }"#,
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
}

#[tokio::test]
async fn graphql_flake_metadata_stub() {
    let resp = schema()
        .execute(Request::new(
            r#"{ flakeMetadata(flakeRef: "nixpkgs") { description lastModified } }"#,
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
}

#[tokio::test]
async fn graphql_search_packages_stub() {
    let resp = schema()
        .execute(Request::new(
            r#"{ searchPackages(query: "hello") { attribute name } }"#,
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert!(data["searchPackages"].as_array().unwrap().is_empty());
}

// ── Query: build ────────────────────────────────────────────────────

#[tokio::test]
async fn graphql_build_status_stub() {
    let resp = schema()
        .execute(Request::new(
            r#"{ buildStatus(buildId: "b1") { id state } }"#,
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["buildStatus"]["id"], "b1");
    assert_eq!(data["buildStatus"]["state"], "pending");
}

#[tokio::test]
async fn graphql_build_log_stub() {
    let resp = schema()
        .execute(Request::new(
            r#"{ buildLog(buildId: "b1") }"#,
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert!(data["buildLog"].as_array().unwrap().is_empty());
}

// ── Query: system ───────────────────────────────────────────────────

#[tokio::test]
async fn graphql_system_status() {
    let resp = schema()
        .execute(Request::new("{ systemStatus { generation configPath } }"))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["systemStatus"]["generation"], 0);
}

#[tokio::test]
async fn graphql_system_generations() {
    let resp = schema()
        .execute(Request::new(
            "{ systemGenerations { number date current } }",
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert!(data["systemGenerations"].as_array().unwrap().is_empty());
}

// ── Query: fleet ────────────────────────────────────────────────────

#[tokio::test]
async fn graphql_fleet_nodes() {
    let resp = schema()
        .execute(Request::new("{ fleetNodes { hostname status } }"))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert!(data["fleetNodes"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn graphql_fleet_status() {
    let resp = schema()
        .execute(Request::new(
            "{ fleetStatus { totalNodes onlineNodes } }",
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["fleetStatus"]["totalNodes"], 0);
}

// ── Query: profile ──────────────────────────────────────────────────

#[tokio::test]
async fn graphql_profiles() {
    let resp = schema()
        .execute(Request::new("{ profiles { name generation } }"))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert!(data["profiles"].as_array().unwrap().is_empty());
}

// ── Query: cache ────────────────────────────────────────────────────

#[tokio::test]
async fn graphql_cache_info() {
    let resp = schema()
        .execute(Request::new(
            "{ cacheInfo { storeDir wantMassQuery priority } }",
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["cacheInfo"]["storeDir"], "/nix/store");
    assert!(data["cacheInfo"]["wantMassQuery"].as_bool().unwrap());
    assert_eq!(data["cacheInfo"]["priority"], 40);
}

// ── Mutation: eval ──────────────────────────────────────────────────

#[tokio::test]
async fn graphql_mutation_eval_success() {
    let resp = schema()
        .execute(Request::new(
            r#"mutation { eval(request: { expression: "1 + 1" }) { value errors } }"#,
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["eval"]["value"], 2);
    assert!(data["eval"]["errors"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn graphql_mutation_eval_error() {
    let resp = schema()
        .execute(Request::new(
            r#"mutation { eval(request: { expression: "let x = ; in x" }) { value errors } }"#,
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert!(data["eval"]["value"].is_null());
    assert!(!data["eval"]["errors"].as_array().unwrap().is_empty());
}

// ── Mutation: build ─────────────────────────────────────────────────

#[tokio::test]
async fn graphql_mutation_build() {
    let resp = schema()
        .execute(Request::new(
            r#"mutation { build(request: { installable: "nixpkgs#hello" }) { id state } }"#,
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["build"]["state"], "pending");
}

// ── Mutation: system ────────────────────────────────────────────────

#[tokio::test]
async fn graphql_mutation_system_rebuild() {
    let resp = schema()
        .execute(Request::new(
            r#"mutation { systemRebuild(request: { action: "switch" }) { generation } }"#,
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
}

#[tokio::test]
async fn graphql_mutation_system_rollback() {
    let resp = schema()
        .execute(Request::new(
            "mutation { systemRollback { generation } }",
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
}

// ── Mutation: fleet ─────────────────────────────────────────────────

#[tokio::test]
async fn graphql_mutation_fleet_deploy() {
    let resp = schema()
        .execute(Request::new(
            r#"mutation { fleetDeploy(request: { target: "@prod" }) { id target status } }"#,
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["fleetDeploy"]["target"], "@prod");
}

// ── Introspection ───────────────────────────────────────────────────

#[tokio::test]
async fn graphql_introspection_works() {
    let resp = schema()
        .execute(Request::new(
            "{ __schema { queryType { name } mutationType { name } } }",
        ))
        .await;
    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["__schema"]["queryType"]["name"], "QueryRoot");
    assert_eq!(data["__schema"]["mutationType"]["name"], "MutationRoot");
}
