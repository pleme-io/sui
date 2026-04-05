//! Triple-stack API server: REST (axum) + GraphQL (async-graphql) + gRPC (tonic).
//!
//! All three protocols share application state containing the store backend.

pub mod graphql;
pub mod rest;
pub mod state;
pub mod types;

use axum::Router;
use tower_http::trace::TraceLayer;

use state::AppState;
use sui_store::LocalStore;

const NIX_DB_PATH: &str = "/nix/var/nix/db/db.sqlite";

/// Start the API server on the given addresses.
pub async fn serve(rest_addr: &str, _grpc_addr: &str) -> anyhow::Result<()> {
    // Try to open the local Nix store; fall back to stub mode if unavailable.
    let app_state = match LocalStore::open(NIX_DB_PATH).await {
        Ok(store) => {
            tracing::info!("connected to local Nix store at {NIX_DB_PATH}");
            AppState::with_store(store)
        }
        Err(e) => {
            tracing::warn!("could not open Nix store ({e}), running in stub mode");
            AppState::stub()
        }
    };

    let schema = graphql::build_schema(app_state.clone());

    let app = Router::new()
        .merge(rest::router())
        .merge(graphql::router(schema))
        .layer(TraceLayer::new_for_http())
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(rest_addr).await?;
    tracing::info!("sui API server listening on {rest_addr}");
    axum::serve(listener, app).await?;

    Ok(())
}
