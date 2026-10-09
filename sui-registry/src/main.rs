//! `porto` — the runnable OCI Distribution Spec v1.1 registry server.
//!
//! The thin runtime shell around the library:
//!
//! - `porto` (or `porto serve`) — resolve the typed [`RegistryConfig`]
//!   (`PORTO_TIER`, then `PORTO_*` overrides), load every mounted OCI image
//!   layout read-only, build the writable backend if the config has one, serve.
//! - `porto check` — resolve the config and load + verify every layout, then
//!   exit: a broken layout or a tag conflict fails here, at build or deploy
//!   time, instead of at boot on a node.
//! - `porto config-schema` — the config's JSON Schema (the input for a
//!   generated NixOS module).
//! - `porto config-show` — the resolved config as YAML.
//!
//! Every fallible step returns a typed [`PortoError`]; there is no
//! `unwrap`/`expect`/`panic!` on the happy path.

use std::sync::Arc;

use sui_registry::config::{ConfigError, RegistryConfig};
use sui_registry::server::{serve, AppState};
use sui_registry::store::{LayoutError, LayoutStore, OverlayStore, RegistryStore, SuiCacheStore};

/// A typed startup/serve failure. `Display` is total, so `main`'s error path
/// never prints a bare framework message.
#[derive(Debug, thiserror::Error)]
enum PortoError {
    /// The config could not be resolved.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A mounted layout is broken, corrupt, or conflicts with another.
    #[error("layout refused: {0}")]
    Layout(#[from] LayoutError),
    /// Building the writable storage backend failed.
    #[error("failed to build the storage backend: {0}")]
    Backend(#[from] sui_castore::StoreError),
    /// Binding the listener or serving failed.
    #[error("registry serve failed: {0}")]
    Serve(#[from] std::io::Error),
    /// Rendering a config for display failed.
    #[error("cannot render config: {0}")]
    Render(String),
    /// An unknown subcommand.
    #[error("unknown command {0:?} (expected: serve | check | config-schema | config-show)")]
    Usage(String),
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // Print the typed error's `Display` (which names the file at fault), not
    // the `Debug` dump `fn main() -> Result` would print.
    match porto().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("porto: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn porto() -> Result<(), PortoError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let command = std::env::args().nth(1);
    match command.as_deref() {
        None | Some("serve") => run(RegistryConfig::load()?).await,
        Some("check") => {
            let config = RegistryConfig::load()?;
            let layouts = LayoutStore::load(&config.layouts)?;
            let repos = layouts.list_repositories().await.unwrap_or_default();
            println!(
                "ok: {} layout(s), {} repositor{}",
                config.layouts.len(),
                repos.len(),
                if repos.len() == 1 { "y" } else { "ies" }
            );
            for repo in repos {
                println!("  {repo}");
            }
            Ok(())
        }
        Some("config-schema") => {
            let schema = serde_json::to_string_pretty(&RegistryConfig::json_schema())
                .map_err(|e| PortoError::Render(e.to_string()))?;
            println!("{schema}");
            Ok(())
        }
        Some("config-show") => {
            let yaml = serde_yaml_ng::to_string(&RegistryConfig::load()?)
                .map_err(|e| PortoError::Render(e.to_string()))?;
            print!("{yaml}");
            Ok(())
        }
        Some(other) => Err(PortoError::Usage(other.to_string())),
    }
}

async fn run(config: RegistryConfig) -> Result<(), PortoError> {
    // Config decides the writable backend; `None` (layouts and no explicit
    // backend) is a pure read-only mirror. A tiered/redis/pg arm whose
    // sui-castore feature is not compiled in is a typed error, never a disk
    // fallback.
    let writable: Option<Arc<dyn RegistryStore>> = match config.resolve_backend() {
        Some(backend_config) => {
            let backend = sui_castore::build_backend(&backend_config).await?;
            tracing::info!("porto writable backend: {backend_config:?}");
            Some(Arc::new(SuiCacheStore::new(backend)))
        }
        None => {
            tracing::info!("porto: no writable backend — every repository is read-only");
            None
        }
    };

    let store: Arc<dyn RegistryStore> = if config.layouts.is_empty() {
        // `resolve_backend` always yields a backend when no layouts are
        // mounted, so this arm always has one.
        writable.unwrap_or_else(|| Arc::new(OverlayStore::new(LayoutStore::default(), None)))
    } else {
        let layouts = LayoutStore::load(&config.layouts)?;
        for mount in &config.layouts {
            tracing::info!(
                "porto: serving {} read-only from {} (verify: {:?})",
                mount.repository,
                mount.layout.display(),
                mount.verify
            );
        }
        Arc::new(OverlayStore::new(layouts, writable))
    };

    serve(AppState::new(store, config)).await?;
    Ok(())
}
