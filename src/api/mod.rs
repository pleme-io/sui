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

use crate::NIX_DB_PATH;

/// Build the combined axum [`Router`] with REST and GraphQL endpoints.
///
/// The returned router is ready to be served via [`axum::serve`]. Useful
/// in tests that want the full router without binding to a TCP socket.
pub fn build_router(app_state: AppState) -> Router {
    let schema = graphql::build_schema(app_state.clone());

    Router::new()
        .merge(rest::router())
        .merge(graphql::router(schema))
        .layer(TraceLayer::new_for_http())
        .with_state(app_state)
}

/// Start the API server on the given addresses.
///
/// `rest_addr` is the bind address for REST + GraphQL (e.g. `"0.0.0.0:8080"`).
/// `_grpc_addr` is reserved for the future gRPC listener.
///
/// # Errors
///
/// Returns an I/O error if the server fails to bind or encounters a
/// runtime error.
pub async fn serve(
    rest_addr: impl AsRef<str>,
    _grpc_addr: impl AsRef<str>,
) -> std::io::Result<()> {
    let rest_addr = rest_addr.as_ref();

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

    let app = build_router(app_state);

    let listener = tokio::net::TcpListener::bind(rest_addr).await?;
    tracing::info!("sui API server listening on {rest_addr}");
    axum::serve(listener, app).await?;

    Ok(())
}
