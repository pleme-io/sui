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

use async_nats::jetstream;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::CliError;
use sui_cache::StorageBackend;
use sui_store::BinaryCacheStore;

/// Default upstream binary caches to check for substitutes.
const DEFAULT_UPSTREAM_CACHES: &[&str] = &[
    "https://cache.nixos.org",
];

/// Trusted public keys for signature verification.
const NIXOS_CACHE_KEY: &str =
    "cache.nixos.org-1:6NCHdD59X431o0gWypbMuDG1OvMckZu32um1TadOR8=";

#[derive(Debug, Deserialize)]
struct BuildRequest {
    build_id: String,
    flake_ref: String,
    system: String,
    #[allow(dead_code)]
    attic_cache: Option<String>,
    #[allow(dead_code)]
    extra_args: Vec<String>,
    #[allow(dead_code)]
    priority: i32,
}

#[derive(Debug, Serialize)]
struct BuildComplete {
    build_id: String,
    status: String,
    store_path: Option<String>,
    error: Option<String>,
}

pub async fn run_agent(
    nats_url: &str,
    stream_name: &str,
    consumer_name: &str,
    _cache_url: &str,
    _cache_name: &str,
) -> Result<(), CliError> {
    info!("Starting sui build agent (native pipeline — no nix dependency)");
    info!(nats = %nats_url, stream = %stream_name, consumer = %consumer_name);

    // Create shared storage backend for both cache server and build pipeline
    let storage: Arc<dyn StorageBackend> = Arc::new(
        sui_cache::LocalStorage::new(std::path::PathBuf::from("/var/lib/sui/cache")),
    );

    // Start the cache server in background (serves /nix-cache-info for health checks)
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
            tracing::error!(error = %e, "Cache server failed");
        }
    });

    info!("Cache server started on :5000");

    // Initialize upstream binary caches for substitution
    let upstream_caches: Vec<BinaryCacheStore> = DEFAULT_UPSTREAM_CACHES
        .iter()
        .map(|url| {
            BinaryCacheStore::builder(url)
                .trusted_keys(vec![NIXOS_CACHE_KEY.to_string()])
                .build()
        })
        .collect();

    info!(
        count = upstream_caches.len(),
        "Initialized upstream binary caches"
    );

    // Connect to NATS
    let nats_client = async_nats::connect(nats_url)
        .await
        .map_err(|e| CliError::NotImplemented(format!("NATS connect failed: {e}")))?;

    let jetstream = jetstream::new(nats_client.clone());

    info!("Connected to NATS");

    // Get or create the BUILD stream
    let stream = jetstream
        .get_or_create_stream(jetstream::stream::Config {
            name: stream_name.to_string(),
            subjects: vec![format!("{stream_name}.>")],
            retention: jetstream::stream::RetentionPolicy::WorkQueue,
            ..Default::default()
        })
        .await
        .map_err(|e| CliError::NotImplemented(format!("Stream setup failed: {e}")))?;

    // Create a durable consumer
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
        .map_err(|e| CliError::NotImplemented(format!("Consumer setup failed: {e}")))?;

    info!("Listening for build requests on {stream_name}.request");

    // Process messages
    loop {
        let mut messages = consumer
            .fetch()
            .max_messages(1)
            .messages()
            .await
            .map_err(|e| CliError::NotImplemented(format!("Fetch failed: {e}")))?;

        // Poll for next message with timeout
        let msg = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            use tokio_stream::StreamExt;
            messages.next().await
        })
        .await;

        let msg = match msg {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => {
                warn!(error = %e, "Error receiving message");
                continue;
            }
            Ok(None) | Err(_) => continue,
        };

        // Parse the build request
        let request: BuildRequest = match serde_json::from_slice(&msg.payload) {
            Ok(req) => req,
            Err(e) => {
                error!(error = %e, "Failed to parse build request");
                let _ = msg.ack().await;
                continue;
            }
        };

        info!(
            build_id = %request.build_id,
            flake_ref = %request.flake_ref,
            system = %request.system,
            "Processing build request"
        );

        // Execute the build using native pipeline
        let result =
            execute_build_native(&request, &upstream_caches, storage.as_ref()).await;

        // Publish completion
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

        let complete_subject = format!("{stream_name}.complete.{}", request.build_id);
        let complete_payload = serde_json::to_vec(&complete).unwrap_or_default();

        if let Err(e) = nats_client
            .publish(complete_subject.clone(), complete_payload.into())
            .await
        {
            error!(error = %e, "Failed to publish build completion");
        }

        // Ack the message
        if let Err(e) = msg.ack().await {
            error!(error = %e, "Failed to ack message");
        }

        match &result {
            Ok(path) => {
                info!(build_id = %request.build_id, store_path = %path, "Build complete")
            }
            Err(e) => {
                warn!(build_id = %request.build_id, error = %e, "Build failed")
            }
        }
    }
}

/// Parse a flake reference into (flake_url, attribute_path).
///
/// Input: `github:pleme-io/burst-forge#packages.x86_64-linux.default`
/// Output: (`github:pleme-io/burst-forge`, `packages.x86_64-linux.default`)
fn parse_flake_ref(flake_ref: &str) -> Result<(String, String), String> {
    if let Some((url, attr)) = flake_ref.split_once('#') {
        Ok((url.to_string(), attr.to_string()))
    } else {
        // No attribute path — default to packages.<system>.default
        Ok((flake_ref.to_string(), String::new()))
    }
}

/// Execute a build request using the native sui pipeline.
///
/// Flow: eval → extract store paths → check upstream caches → download → cache locally
async fn execute_build_native(
    request: &BuildRequest,
    upstream_caches: &[BinaryCacheStore],
    local_storage: &dyn StorageBackend,
) -> Result<String, String> {
    let (flake_url, attr_path) = parse_flake_ref(&request.flake_ref)?;

    // Default attribute path if none specified
    let attr_path = if attr_path.is_empty() {
        format!("packages.{}.default", request.system)
    } else {
        attr_path
    };

    info!(flake_url = %flake_url, attr_path = %attr_path, "Evaluating flake");

    // 1. Evaluate the flake expression and extract outPath
    //    (synchronous eval — run on blocking thread, return only the String)
    let eval_expr = format!(
        "(builtins.getFlake \"{flake_url}\").{attr_path}"
    );
    let out_path = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let value = sui_eval::eval(&eval_expr)
            .map_err(|e| format!("Eval failed: {e}"))?;
        extract_out_path(&value)
    })
    .await
    .map_err(|e| format!("Eval task panicked: {e}"))??;

    info!(out_path = %out_path, "Derivation output resolved");

    // 3. Extract hash from store path
    let store_path = sui_compat::store_path::StorePath::from_absolute_path(&out_path)
        .map_err(|e| format!("Invalid store path {out_path}: {e}"))?;
    let hash = store_path.hash();

    // 4. Check if already in our local cache
    if let Ok(Some(_)) = local_storage.get_narinfo(&hash).await {
        info!(out_path = %out_path, "Already in local cache");
        return Ok(out_path);
    }

    // 5. Try upstream binary caches
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
                    "Error checking upstream cache"
                );
                continue;
            }
        }
    }

    // 6. Not in any cache — future: use sui-build for local builds
    Err(format!(
        "Output {out_path} not in any upstream cache; \
         local builds via sui-build not yet wired"
    ))
}

/// Extract `outPath` from a derivation Value.
fn extract_out_path(value: &sui_eval::Value) -> Result<String, String> {
    let attrs = value
        .as_attrs()
        .map_err(|e| format!("Expected derivation attrs, got: {e}"))?;

    // Try outPath first (standard derivation output)
    if let Some(out_path_val) = attrs.get("outPath") {
        return out_path_val
            .as_string()
            .map(String::from)
            .map_err(|e| format!("outPath not a string: {e}"));
    }

    // Try drvPath as fallback
    if let Some(drv_path_val) = attrs.get("drvPath") {
        return drv_path_val
            .as_string()
            .map(String::from)
            .map_err(|e| format!("drvPath not a string: {e}"));
    }

    Err("No outPath or drvPath in evaluation result".to_string())
}

/// Try to substitute a store path from an upstream binary cache.
///
/// Returns `Ok(true)` if the path was found and cached locally,
/// `Ok(false)` if not found, or `Err` on network/storage errors.
async fn substitute_from_cache(
    upstream: &BinaryCacheStore,
    hash: &str,
    out_path: &str,
    local_storage: &dyn StorageBackend,
) -> Result<bool, String> {
    // Check if upstream has the narinfo
    let narinfo = upstream
        .fetch_narinfo(hash)
        .await
        .map_err(|e| format!("narinfo fetch: {e}"))?;

    let narinfo = match narinfo {
        Some(ni) => ni,
        None => return Ok(false),
    };

    info!(
        out_path = %out_path,
        nar_size = narinfo.nar_size,
        compression = %narinfo.compression,
        "Found in upstream cache, downloading"
    );

    // Download the compressed NAR
    let compressed_nar = upstream
        .fetch_nar(&narinfo.url)
        .await
        .map_err(|e| format!("NAR download: {e}"))?;

    info!(
        out_path = %out_path,
        compressed_bytes = compressed_nar.len(),
        "Downloaded NAR, pushing to local cache"
    );

    // Store in local cache: narinfo + NAR blob
    local_storage
        .put_nar(&narinfo.url, &compressed_nar)
        .await
        .map_err(|e| format!("put_nar: {e}"))?;

    local_storage
        .put_narinfo(hash, &narinfo.serialize())
        .await
        .map_err(|e| format!("put_narinfo: {e}"))?;

    Ok(true)
}
