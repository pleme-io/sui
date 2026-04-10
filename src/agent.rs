//! NATS build agent — consumes build requests, evaluates, substitutes, caches.
//!
//! Pure Rust pipeline — no nix binary dependency:
//!
//!   1. Receive BUILD.request from NATS
//!   2. Evaluate flake ref → derivation (sui-eval, pure Rust)
//!   3. Extract output store paths (hash computation only)
//!   4. Check upstream binary caches (cache.nixos.org via sui-store)
//!   5. Download NAR + narinfo for cached outputs
//!   6. Push to local cache (sui-cache, S3-backed)
//!   7. Publish BUILD.complete

use std::sync::Arc;
use std::time::Instant;

use async_nats::jetstream;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::CliError;
use sui_cache::StorageBackend;
use sui_store::BinaryCacheStore;

/// Default upstream binary caches to check for substitutes.
const DEFAULT_UPSTREAM_CACHES: &[&str] = &["https://cache.nixos.org"];

/// Trusted public keys for signature verification.
const NIXOS_CACHE_KEY: &str =
    "cache.nixos.org-1:6NCHdD59X431o0gWypbMuDG1OvMckZu32um1TadOR8=";

// ── Agent-specific error type ────────────────────────────────

/// Errors that can occur during build request processing.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("flake evaluation failed: {0}")]
    Eval(#[from] sui_eval::EvalError),

    #[error("invalid store path: {0}")]
    StorePath(#[from] sui_compat::store_path::StorePathError),

    #[error("upstream cache error: {0}")]
    BinaryCache(#[from] sui_store::StoreError),

    #[error("local cache error: {0}")]
    LocalCache(#[from] sui_cache::CacheError),

    #[error("eval thread panicked: {0}")]
    EvalPanic(String),

    #[error("result has no outPath or drvPath attribute")]
    NoOutputPath,

    #[error("not substitutable: {path} not found in any upstream cache")]
    NotSubstitutable { path: String },
}

// ── Wire types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BuildRequest {
    build_id: String,
    flake_ref: String,
    system: String,
    #[serde(default)]
    extra_args: Vec<String>,
    #[serde(default)]
    priority: i32,
}

#[derive(Debug, Serialize)]
struct BuildComplete {
    build_id: String,
    status: String,
    store_path: Option<String>,
    error: Option<String>,
}

// ── Agent entry point ────────────────────────────────────────

pub async fn run_agent(
    nats_url: &str,
    stream_name: &str,
    consumer_name: &str,
    _cache_url: &str,
    _cache_name: &str,
) -> Result<(), CliError> {
    info!("Starting sui build agent (native pipeline — no nix dependency)");
    info!(nats = %nats_url, stream = %stream_name, consumer = %consumer_name);

    // Shared storage backend for both cache server and build pipeline
    let storage: Arc<dyn StorageBackend> = Arc::new(
        sui_cache::LocalStorage::new(std::path::PathBuf::from("/var/lib/sui/cache")),
    );

    // Cache server in background (serves /nix-cache-info for health probes)
    let cache_storage = Arc::clone(&storage);
    tokio::spawn(async move {
        let config = sui_cache::CacheConfig {
            listen: "0.0.0.0:5000".to_string(),
            backend: sui_cache::BackendConfig::Local {
                path: std::path::PathBuf::from("/var/lib/sui/cache"),
            },
            signing_key: None,
            priority: 40,
            want_mass_query: true,
            store_dir: "/nix/store".to_string(),
        };
        if let Err(e) = sui_cache::serve(config, cache_storage).await {
            error!(error = %e, "Cache server exited with error");
        }
    });

    info!("Cache server started on :5000");

    // Upstream binary caches for substitution
    let upstream_caches: Vec<BinaryCacheStore> = DEFAULT_UPSTREAM_CACHES
        .iter()
        .map(|url| {
            BinaryCacheStore::builder(url)
                .trusted_keys(vec![NIXOS_CACHE_KEY.to_string()])
                .build()
        })
        .collect();

    info!(count = upstream_caches.len(), "Initialized upstream binary caches");

    // NATS connection
    let nats_client = async_nats::connect(nats_url)
        .await
        .map_err(|e| CliError::Deploy(format!("NATS connect: {e}")))?;

    let jetstream = jetstream::new(nats_client.clone());
    info!("Connected to NATS");

    let stream = jetstream
        .get_or_create_stream(jetstream::stream::Config {
            name: stream_name.to_string(),
            subjects: vec![format!("{stream_name}.>")],
            retention: jetstream::stream::RetentionPolicy::WorkQueue,
            ..Default::default()
        })
        .await
        .map_err(|e| CliError::Deploy(format!("NATS stream setup: {e}")))?;

    let consumer = stream
        .get_or_create_consumer(
            consumer_name,
            jetstream::consumer::pull::Config {
                durable_name: Some(consumer_name.to_string()),
                filter_subject: format!("{stream_name}.request"),
                ack_wait: std::time::Duration::from_secs(300),
                max_deliver: 3,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| CliError::Deploy(format!("NATS consumer setup: {e}")))?;

    info!("Listening for build requests on {stream_name}.request");

    // Message processing loop
    loop {
        let msg = poll_next_message(&consumer).await;
        let msg = match msg {
            Some(msg) => msg,
            None => continue,
        };

        let request: BuildRequest = match serde_json::from_slice(&msg.payload) {
            Ok(req) => req,
            Err(e) => {
                error!(error = %e, "Malformed build request payload");
                let _ = msg.ack().await;
                continue;
            }
        };

        info!(
            build_id = %request.build_id,
            flake_ref = %request.flake_ref,
            system = %request.system,
            priority = request.priority,
            "Processing build request"
        );

        let started = Instant::now();
        let result =
            execute_build_native(&request, &upstream_caches, storage.as_ref()).await;
        let elapsed = started.elapsed();

        // Build completion message
        let complete = match &result {
            Ok(store_path) => BuildComplete {
                build_id: request.build_id.clone(),
                status: "Complete".to_string(),
                store_path: Some(store_path.clone()),
                error: None,
            },
            Err(e) => BuildComplete {
                build_id: request.build_id.clone(),
                status: "Failed".to_string(),
                store_path: None,
                error: Some(e.to_string()),
            },
        };

        // Publish completion (log but don't fail on serialization or publish errors)
        match serde_json::to_vec(&complete) {
            Ok(payload) => {
                let subject = format!("{stream_name}.complete.{}", request.build_id);
                if let Err(e) = nats_client.publish(subject, payload.into()).await {
                    error!(error = %e, build_id = %request.build_id, "Failed to publish completion");
                }
            }
            Err(e) => {
                error!(error = %e, build_id = %request.build_id, "Failed to serialize completion");
            }
        }

        if let Err(e) = msg.ack().await {
            error!(error = %e, build_id = %request.build_id, "Failed to ack message");
        }

        match &result {
            Ok(path) => info!(
                build_id = %request.build_id,
                store_path = %path,
                elapsed_ms = elapsed.as_millis() as u64,
                "Build complete"
            ),
            Err(e) => warn!(
                build_id = %request.build_id,
                error = %e,
                elapsed_ms = elapsed.as_millis() as u64,
                "Build failed"
            ),
        }
    }
}

// ── Message polling ──────────────────────────────────────────

/// Poll the NATS consumer for the next message with a 30s timeout.
/// Returns `None` on timeout or transient errors (caller should loop).
async fn poll_next_message(
    consumer: &async_nats::jetstream::consumer::Consumer<jetstream::consumer::pull::Config>,
) -> Option<async_nats::jetstream::Message> {
    let mut messages = match consumer.fetch().max_messages(1).messages().await {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "NATS fetch error");
            return None;
        }
    };

    let msg = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        use tokio_stream::StreamExt;
        messages.next().await
    })
    .await;

    match msg {
        Ok(Some(Ok(msg))) => Some(msg),
        Ok(Some(Err(e))) => {
            warn!(error = %e, "NATS message error");
            None
        }
        Ok(None) | Err(_) => None,
    }
}

// ── Build pipeline ───────────────────────────────────────────

/// Execute a build request using the native sui pipeline.
///
/// Flow: eval → extract store paths → check upstream caches → download → cache locally.
async fn execute_build_native(
    request: &BuildRequest,
    upstream_caches: &[BinaryCacheStore],
    local_storage: &dyn StorageBackend,
) -> Result<String, AgentError> {
    let (flake_url, attr_path) = parse_flake_ref(&request.flake_ref, &request.system);

    info!(flake_url = %flake_url, attr_path = %attr_path, "Evaluating flake");

    // Step 1: Evaluate the flake expression (blocking — sui-eval is synchronous)
    let eval_started = Instant::now();
    let eval_expr = format!("(builtins.getFlake \"{flake_url}\").{attr_path}");
    let out_path = tokio::task::spawn_blocking(move || -> Result<String, AgentError> {
        let value = sui_eval::eval(&eval_expr)?;
        extract_out_path(&value)
    })
    .await
    .map_err(|e| AgentError::EvalPanic(e.to_string()))??;

    info!(
        out_path = %out_path,
        eval_ms = eval_started.elapsed().as_millis() as u64,
        "Derivation output resolved"
    );

    // Step 2: Extract hash from store path
    let store_path = sui_compat::store_path::StorePath::from_absolute_path(&out_path)?;
    let hash = store_path.hash();

    // Step 3: Check local cache first
    if local_storage.get_narinfo(&hash).await.ok().flatten().is_some() {
        info!(out_path = %out_path, "Already in local cache");
        return Ok(out_path);
    }

    // Step 4: Try upstream binary caches
    for cache in upstream_caches {
        match substitute_from_cache(cache, &hash, &out_path, local_storage).await {
            Ok(true) => {
                info!(
                    out_path = %out_path,
                    cache = %cache.base_url(),
                    "Substituted from upstream cache"
                );
                return Ok(out_path);
            }
            Ok(false) => continue,
            Err(e) => {
                warn!(
                    cache = %cache.base_url(),
                    error = %e,
                    "Upstream cache error (trying next)"
                );
                continue;
            }
        }
    }

    Err(AgentError::NotSubstitutable {
        path: out_path,
    })
}

/// Parse a flake reference into (flake_url, attribute_path).
///
/// `github:pleme-io/burst-forge#packages.x86_64-linux.default`
/// → `("github:pleme-io/burst-forge", "packages.x86_64-linux.default")`
fn parse_flake_ref(flake_ref: &str, system: &str) -> (String, String) {
    match flake_ref.split_once('#') {
        Some((url, attr)) => (url.to_string(), attr.to_string()),
        None => (
            flake_ref.to_string(),
            format!("packages.{system}.default"),
        ),
    }
}

/// Extract `outPath` from a derivation Value.
fn extract_out_path(value: &sui_eval::Value) -> Result<String, AgentError> {
    let attrs = value.as_attrs().map_err(|e| {
        AgentError::Eval(sui_eval::EvalError::TypeError(format!(
            "expected derivation attrs: {e}"
        )))
    })?;

    if let Some(v) = attrs.get("outPath") {
        return v
            .as_string()
            .map(String::from)
            .map_err(|e| {
                AgentError::Eval(sui_eval::EvalError::TypeError(format!(
                    "outPath not a string: {e}"
                )))
            });
    }

    if let Some(v) = attrs.get("drvPath") {
        return v
            .as_string()
            .map(String::from)
            .map_err(|e| {
                AgentError::Eval(sui_eval::EvalError::TypeError(format!(
                    "drvPath not a string: {e}"
                )))
            });
    }

    Err(AgentError::NoOutputPath)
}

// ── Substitution ─────────────────────────────────────────────

/// Try to substitute a store path from an upstream binary cache.
///
/// Returns `Ok(true)` if the path was found and cached locally,
/// `Ok(false)` if not found in this cache.
async fn substitute_from_cache(
    upstream: &BinaryCacheStore,
    hash: &str,
    out_path: &str,
    local_storage: &dyn StorageBackend,
) -> Result<bool, AgentError> {
    let narinfo = match upstream.fetch_narinfo(hash).await {
        Ok(Some(ni)) => ni,
        Ok(None) => return Ok(false),
        Err(e) => return Err(AgentError::BinaryCache(sui_store::StoreError::Http(e.to_string()))),
    };

    info!(
        out_path = %out_path,
        nar_size = narinfo.nar_size,
        compression = %narinfo.compression,
        "Found in upstream, downloading NAR"
    );

    let download_started = Instant::now();
    let compressed_nar = upstream
        .fetch_nar(&narinfo.url)
        .await
        .map_err(|e| AgentError::BinaryCache(sui_store::StoreError::Http(e.to_string())))?;

    info!(
        out_path = %out_path,
        compressed_bytes = compressed_nar.len(),
        download_ms = download_started.elapsed().as_millis() as u64,
        "Downloaded NAR, pushing to local cache"
    );

    // Store narinfo + NAR blob in local cache
    local_storage
        .put_nar(&narinfo.url, &compressed_nar)
        .await?;
    local_storage
        .put_narinfo(hash, &narinfo.serialize())
        .await?;

    Ok(true)
}
