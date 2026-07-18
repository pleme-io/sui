//! `porto` — the runnable OCI Distribution Spec v1.1 registry server.
//!
//! This binary is the thin runtime shell around the library: it resolves the
//! typed [`RegistryConfig`] from the environment, builds the durable
//! [`sui_cache::storage::StorageBackend`] the config selects (via sui-cache's
//! typed [`build_backend`](sui_cache::build_backend) factory — never a silent
//! hard-coded constructor), wraps it in a [`SuiCacheStore`], and serves the
//! axum `/v2/` router until shutdown.
//!
//! Every fallible step returns a typed [`PortoError`] up through `main`; there
//! is no `unwrap`/`expect`/`panic!` on the happy path — a bad config, an
//! unbuildable backend, or a bind failure is a typed non-zero exit with a
//! logged reason, never a bare panic.

use std::sync::Arc;

use sui_registry::config::RegistryConfig;
use sui_registry::server::{serve, AppState};
use sui_registry::store::SuiCacheStore;

/// A typed startup/serve failure. Each variant carries the operator-facing
/// reason; `Display` is total, so `main`'s error path never prints a bare
/// framework message.
#[derive(Debug, thiserror::Error)]
enum PortoError {
    /// Building the durable storage backend the config selected failed.
    #[error("failed to build the storage backend: {0}")]
    Backend(#[from] sui_cache::CacheError),
    /// Binding the listener or serving failed.
    #[error("registry serve failed: {0}")]
    Serve(#[from] std::io::Error),
}

#[tokio::main]
async fn main() -> Result<(), PortoError> {
    // Structured logging; `RUST_LOG` selects the filter, defaulting to `info`
    // so a fresh operator sees the "listening on …" line without extra flags.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Progressive discovery: the prescribed baseline overlaid by the env tier.
    let config = RegistryConfig::prescribed_default().from_env();

    // Config decides the backend; `build_backend` is the typed dispatch. A
    // tiered/redis/pg arm whose sui-cache feature is not compiled in returns a
    // typed `CacheError::NotImplemented` here rather than falling back to disk.
    let backend_config = config.resolve_backend();
    let backend = sui_cache::build_backend(&backend_config).await?;
    tracing::info!("porto storage backend resolved: {backend_config:?}");

    let store = Arc::new(SuiCacheStore::new(backend));
    let state = AppState::new(store, config);
    serve(state).await?;
    Ok(())
}
