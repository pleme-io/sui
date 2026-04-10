//! NATS build agent — consumes build requests, builds, pushes to cache.
//!
//! This replaces the SSH-based nix-builder with a pure Rust NATS consumer.
//! The agent subscribes to the BUILD JetStream stream and processes requests:
//!
//!   1. Receive BUILD.request from NATS
//!   2. Evaluate flake ref → derivation (sui-eval)
//!   3. Build in sandbox (sui-build)
//!   4. Push outputs to cache (sui-cache)
//!   5. Publish BUILD.complete

use async_nats::jetstream;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::CliError;

#[derive(Debug, Deserialize)]
struct BuildRequest {
    build_id: String,
    flake_ref: String,
    system: String,
    attic_cache: Option<String>,
    extra_args: Vec<String>,
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
    cache_url: &str,
    cache_name: &str,
) -> Result<(), CliError> {
    info!("Starting sui build agent");
    info!(nats = %nats_url, stream = %stream_name, consumer = %consumer_name);
    info!(cache = %cache_url, cache_name = %cache_name);

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

        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        // Poll for next message with timeout
        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            async {
                use tokio_stream::StreamExt;
                messages.next().await
            },
        )
        .await;

        let msg = match msg {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => {
                warn!(error = %e, "Error receiving message");
                continue;
            }
            Ok(None) | Err(_) => {
                // Timeout or no messages — loop back
                continue;
            }
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

        // Execute the build
        let result = execute_build(&request, cache_url, cache_name).await;

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
            Ok(path) => info!(build_id = %request.build_id, store_path = %path, "Build complete"),
            Err(e) => warn!(build_id = %request.build_id, error = %e, "Build failed"),
        }
    }
}

/// Execute a single build request.
///
/// For now, this shells out to `nix build` as a subprocess. In the future,
/// this will use sui-eval → sui-build → sui-cache directly, giving us
/// full Rust-native builds with no nix dependency.
async fn execute_build(
    request: &BuildRequest,
    _cache_url: &str,
    _cache_name: &str,
) -> Result<String, String> {
    // Phase 1: Shell out to nix build (works now, uses system nix)
    // Phase 2: Replace with sui-eval → sui-build → sui-cache (full Rust)
    let mut cmd = tokio::process::Command::new("nix");
    cmd.arg("build")
        .arg(&request.flake_ref)
        .arg("--no-link")
        .arg("--print-out-paths");

    for arg in &request.extra_args {
        cmd.arg(arg);
    }

    info!(flake_ref = %request.flake_ref, "Running nix build");

    let output = cmd
        .output()
        .await
        .map_err(|e| format!("Failed to spawn nix build: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("nix build failed: {stderr}"));
    }

    let store_path = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_string();

    if store_path.is_empty() {
        return Err("nix build produced no output paths".to_string());
    }

    // Push to cache (phase 1: shell out to attic push)
    // Phase 2: use sui-cache::push_path directly
    info!(store_path = %store_path, "Pushing to cache");

    let push_result = tokio::process::Command::new("attic")
        .arg("push")
        .arg(_cache_name)
        .arg(&store_path)
        .arg("--server")
        .arg(_cache_url)
        .output()
        .await;

    match push_result {
        Ok(output) if output.status.success() => {
            info!(store_path = %store_path, "Pushed to cache");
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(error = %stderr, "Cache push failed (build still succeeded)");
        }
        Err(e) => {
            warn!(error = %e, "Cache push command failed (build still succeeded)");
        }
    }

    Ok(store_path)
}
