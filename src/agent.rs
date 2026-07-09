//! NATS build agent — consumes build requests, resolves from lockfile, caches.
//!
//! Lockfile-based pipeline — no full nixpkgs evaluation:
//!
//!   1. Receive BUILD.request from NATS
//!   2. Fetch flake source (small tarball via GitHub archive)
//!   3. Parse flake.lock → enumerate all locked inputs with narHashes
//!   4. For each input, check upstream caches (cache.nixos.org)
//!   5. Download narinfo + NAR for available inputs
//!   6. Push to local cache (sui-cache, S3-backed)
//!   7. Publish BUILD.complete
//!
//! This avoids evaluating nixpkgs (which needs 16+ GiB RAM) by working
//! directly with the lockfile's content-addressed hashes.

use std::sync::Arc;
use std::time::Instant;

use async_nats::jetstream;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::CliError;
use sui_cache::{BackendConfig, StorageBackend};
use sui_compat::flake::FlakeLock;
use sui_eval::fetcher::InputFetcher;
use sui_store::BinaryCacheStore;

/// Default upstream binary caches to check for substitutes.
const DEFAULT_UPSTREAM_CACHES: &[&str] = &["https://cache.nixos.org"];

/// Environment variable holding the JSON-encoded [`BackendConfig`] the serve
/// daemon fronts. This is the seam the `super-cache-ci` chart writes: the
/// canonical super-cache posture is `SUI_CACHE_BACKEND_CONFIG` = a
/// `{"type":"tiered","l1":{redis},"l2":{pg},"l3":{...}}` document naming the
/// in-cluster `redis.super-cache-ci.svc` / `postgres.super-cache-ci.svc`
/// endpoints. Unset ⇒ the daemon falls back to the local-disk backend (the
/// legacy single-node posture) — never a silent Redis/Pg no-op.
const BACKEND_CONFIG_ENV: &str = "SUI_CACHE_BACKEND_CONFIG";

/// The local-disk fallback path used when `SUI_CACHE_BACKEND_CONFIG` is unset.
const DEFAULT_LOCAL_CACHE_DIR: &str = "/var/lib/sui/cache";

/// Stable, low-cardinality label for a resolved [`BackendConfig`] — used as a
/// tracing field so operators see which tier the daemon actually fronts
/// without logging endpoint URLs (which may carry credentials). Returns a
/// `&'static str` (no string building), keeping the typed-emission surface
/// clean.
fn describe_backend(config: &BackendConfig) -> &'static str {
    match config {
        BackendConfig::Local { .. } => "local",
        BackendConfig::S3 { .. } => "s3",
        BackendConfig::Redis { .. } => "redis",
        BackendConfig::Pg { .. } => "pg",
        BackendConfig::Tiered { .. } => "tiered",
    }
}

/// Resolve the [`BackendConfig`] the serve daemon fronts from the environment.
///
/// Reads [`BACKEND_CONFIG_ENV`]. When set, its value MUST be a valid
/// [`BackendConfig`] JSON document (the typed border between the chart and the
/// daemon) — a parse failure is a hard, typed error, never a silent disk
/// fallback (that would mask a mis-authored config as a healthy local cache).
/// When unset, returns the local-disk backend so the single-node posture keeps
/// working unchanged.
///
/// Note this only *selects* the backend shape; a Redis/Pg/Tiered selection is
/// materialized by [`sui_cache::build_backend`], which returns a typed
/// `CacheError::NotImplemented` if the binary was built without the
/// `redis-client` / `postgres` features (the root `sui` binary enables both via
/// its default `tiered` feature).
fn resolve_backend_config() -> Result<BackendConfig, CliError> {
    parse_backend_config(std::env::var(BACKEND_CONFIG_ENV).ok().as_deref())
}

/// Pure core of [`resolve_backend_config`] — decides the backend from an
/// optional raw config string, with no environment access (so it is
/// deterministically testable). `None` / empty ⇒ local-disk fallback; a
/// non-empty value MUST parse as [`BackendConfig`] JSON or it is a typed error.
fn parse_backend_config(raw: Option<&str>) -> Result<BackendConfig, CliError> {
    match raw {
        Some(raw) if !raw.trim().is_empty() => {
            let cfg: BackendConfig = serde_json::from_str(raw).map_err(|e| {
                CliError::Deploy(format!(
                    "{BACKEND_CONFIG_ENV} is not a valid BackendConfig JSON document: {e}"
                ))
            })?;
            Ok(cfg)
        }
        _ => Ok(BackendConfig::Local {
            path: std::path::PathBuf::from(DEFAULT_LOCAL_CACHE_DIR),
        }),
    }
}

/// Trusted public keys for signature verification.
const NIXOS_CACHE_KEY: &str =
    "cache.nixos.org-1:6NCHdD59X431o0gWypbMuDG1OvMckZu32um1TadOR8=";

// ── Agent-specific error type ────────────────────────────────

/// Errors that can occur during build request processing.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("flake evaluation failed: {0}")]
    Eval(#[from] sui_eval::EvalError),

    #[error("flake source fetch failed: {0}")]
    Fetch(String),

    #[error("flake.lock parse failed: {0}")]
    LockParse(String),

    #[error("upstream cache error: {0}")]
    BinaryCache(#[from] sui_store::StoreError),

    #[error("local cache error: {0}")]
    LocalCache(#[from] sui_cache::CacheError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
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
    /// Number of inputs mirrored to local cache.
    cached_inputs: u32,
    /// Number of inputs not found in any upstream cache.
    missed_inputs: u32,
    error: Option<String>,
}

// ── Agent entry point ────────────────────────────────────────

/// Resolution strategy for the build agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Parse flake.lock, mirror inputs (~50MB RAM). Default.
    Lockfile,
    /// Full sui-eval derivation resolution (~16GiB RAM).
    Eval,
    /// Shell out to `nix build` (requires nix in container).
    Nix,
}

impl Strategy {
    fn from_str(s: &str) -> Self {
        match s {
            "eval" => Self::Eval,
            "nix" => Self::Nix,
            _ => Self::Lockfile,
        }
    }
}

pub async fn run_agent(
    nats_url: &str,
    stream_name: &str,
    consumer_name: &str,
    _cache_url: &str,
    _cache_name: &str,
    strategy_str: &str,
    signing_key: Option<&str>,
) -> Result<(), CliError> {
    let strategy = Strategy::from_str(strategy_str);
    info!(
        strategy = ?strategy,
        "Starting sui build agent"
    );
    info!(nats = %nats_url, stream = %stream_name, consumer = %consumer_name);

    // Resolve the storage backend the daemon fronts from config (the seam the
    // super-cache-ci chart writes). Materialize it ONCE via the typed
    // config-select factory and share it between the cache server and the build
    // pipeline — a Redis/Pg/Tiered selection in a binary without the transport
    // features fails loudly here (typed NotImplemented), never a silent disk
    // fallback.
    let backend_config = resolve_backend_config()?;
    info!(backend = %describe_backend(&backend_config), "Resolved cache backend from config");
    let storage: Arc<dyn StorageBackend> = sui_cache::build_backend(&backend_config)
        .await
        .map_err(|e| CliError::Deploy(format!("cache backend init failed: {e}")))?;

    // Cache server in background (serves /nix-cache-info for health probes
    // and the binary-cache protocol). When a signing key is provided it is
    // sourced from a file path — a cofre/ESO-materialized Secret mount in
    // production — and every ingested narinfo is signed. The served config
    // names the SAME resolved backend so nix-cache-info and the serving tier
    // agree with the build pipeline's tier.
    let cache_storage = Arc::clone(&storage);
    let signing_key_path = signing_key.map(std::path::PathBuf::from);
    let require_sigs = signing_key_path.is_some();
    let served_backend = backend_config.clone();
    tokio::spawn(async move {
        let config = sui_cache::CacheConfig {
            listen: "0.0.0.0:5000".to_string(),
            backend: served_backend,
            signing_key: signing_key_path,
            priority: 40,
            want_mass_query: true,
            store_dir: "/nix/store".to_string(),
            require_sigs,
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
        let result = match strategy {
            Strategy::Lockfile => {
                resolve_from_lockfile(&request, &upstream_caches, storage.as_ref()).await
            }
            Strategy::Eval => {
                resolve_with_eval(&request, &upstream_caches, storage.as_ref()).await
            }
            Strategy::Nix => {
                resolve_with_nix(&request).await
            }
        };
        let elapsed = started.elapsed();

        // Build completion message
        let complete = match &result {
            Ok((cached, missed)) => BuildComplete {
                build_id: request.build_id.clone(),
                status: "Complete".to_string(),
                cached_inputs: *cached,
                missed_inputs: *missed,
                error: None,
            },
            Err(e) => BuildComplete {
                build_id: request.build_id.clone(),
                status: "Failed".to_string(),
                cached_inputs: 0,
                missed_inputs: 0,
                error: Some(e.to_string()),
            },
        };

        // Publish completion
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
            Ok((cached, missed)) => info!(
                build_id = %request.build_id,
                cached_inputs = cached,
                missed_inputs = missed,
                elapsed_ms = elapsed.as_millis() as u64,
                "Lockfile resolved"
            ),
            Err(e) => warn!(
                build_id = %request.build_id,
                error = %e,
                elapsed_ms = elapsed.as_millis() as u64,
                "Resolution failed"
            ),
        }
    }
}

// ── Message polling ──────────────────────────────────────────

/// Poll the NATS consumer for the next message with a 30s timeout.
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

// ── Lockfile-based resolution ────────────────────────────────

/// Resolve a flake's inputs from its lockfile and mirror them to the local cache.
///
/// Returns `(cached_count, missed_count)`.
async fn resolve_from_lockfile(
    request: &BuildRequest,
    upstream_caches: &[BinaryCacheStore],
    local_storage: &dyn StorageBackend,
) -> Result<(u32, u32), AgentError> {
    let (flake_url, _attr_path) = parse_flake_ref(&request.flake_ref);

    // Step 1: Fetch the flake source (small tarball, not nixpkgs)
    info!(flake_url = %flake_url, "Fetching flake source");
    let fetch_started = Instant::now();
    let repo_dir = tokio::task::spawn_blocking(move || fetch_flake_source(&flake_url))
        .await
        .map_err(|e| AgentError::Fetch(format!("task panicked: {e}")))?
        .map_err(|e| AgentError::Fetch(e.to_string()))?;
    info!(
        fetch_ms = fetch_started.elapsed().as_millis() as u64,
        "Flake source fetched"
    );

    // Step 2: Parse flake.lock
    let lock_path = repo_dir.join("flake.lock");
    if !lock_path.exists() {
        return Err(AgentError::LockParse("no flake.lock found".to_string()));
    }
    let lock_json = std::fs::read_to_string(&lock_path)?;
    let lock = FlakeLock::parse(&lock_json)
        .map_err(|e| AgentError::LockParse(e.to_string()))?;

    // Step 3: Enumerate locked inputs with narHashes
    let locked_inputs: Vec<_> = lock
        .nodes
        .iter()
        .filter(|(name, _)| name.as_str() != lock.root)
        .filter_map(|(name, node)| {
            node.locked.as_ref().map(|locked| (name.clone(), locked.clone()))
        })
        .collect();

    info!(
        input_count = locked_inputs.len(),
        "Parsed flake.lock, resolving inputs"
    );

    // Step 4: For each input, check upstream caches and mirror
    let mut cached = 0u32;
    let mut missed = 0u32;

    for (input_name, locked) in &locked_inputs {
        let nar_hash = match &locked.nar_hash {
            Some(h) => h,
            None => {
                warn!(input = %input_name, "No narHash, skipping");
                missed += 1;
                continue;
            }
        };

        // Convert narHash (e.g. "sha256-xxxxx") to the 32-char nix base32 hash
        // that cache.nixos.org uses for narinfo lookups.
        // The narHash in flake.lock is the hash of the SOURCE, not of a built output.
        // We construct the expected store path and check if it exists in any cache.
        let source_desc = describe_input(locked);
        let store_hash = nar_hash_to_store_hash(nar_hash);
        let store_hash = match store_hash {
            Some(h) => h,
            None => {
                warn!(
                    input = %input_name,
                    nar_hash = %nar_hash,
                    "Could not derive store hash from narHash"
                );
                missed += 1;
                continue;
            }
        };

        // Check local cache first
        if local_storage.get_narinfo(&store_hash).await.ok().flatten().is_some() {
            info!(input = %input_name, source = %source_desc, "Already in local cache");
            cached += 1;
            continue;
        }

        // Try upstream caches
        let mut found = false;
        for cache in upstream_caches {
            match cache.fetch_narinfo(&store_hash).await {
                Ok(Some(narinfo)) => {
                    info!(
                        input = %input_name,
                        source = %source_desc,
                        nar_size = narinfo.nar_size,
                        "Found in upstream, downloading"
                    );

                    match cache.fetch_nar(&narinfo.url).await {
                        Ok(nar_data) => {
                            // Mirror to local cache
                            if let Err(e) = local_storage
                                .put_nar(&narinfo.url, &nar_data)
                                .await
                            {
                                warn!(input = %input_name, error = %e, "Failed to cache NAR");
                                continue;
                            }
                            if let Err(e) = local_storage
                                .put_narinfo(&store_hash, &narinfo.serialize())
                                .await
                            {
                                warn!(input = %input_name, error = %e, "Failed to cache narinfo");
                                continue;
                            }
                            info!(
                                input = %input_name,
                                compressed_bytes = nar_data.len(),
                                "Mirrored to local cache"
                            );
                            cached += 1;
                            found = true;
                            break;
                        }
                        Err(e) => {
                            warn!(
                                input = %input_name,
                                cache = %cache.base_url(),
                                error = %e,
                                "NAR download failed"
                            );
                        }
                    }
                }
                Ok(None) => continue,
                Err(e) => {
                    warn!(
                        input = %input_name,
                        cache = %cache.base_url(),
                        error = %e,
                        "Upstream cache error"
                    );
                }
            }
        }

        if !found {
            info!(
                input = %input_name,
                source = %source_desc,
                "Not in any upstream cache"
            );
            missed += 1;
        }
    }

    Ok((cached, missed))
}

// ── Helpers ──────────────────────────────────────────────────

/// Parse a flake reference into (flake_url, attribute_path).
fn parse_flake_ref(flake_ref: &str) -> (String, String) {
    match flake_ref.split_once('#') {
        Some((url, attr)) => (url.to_string(), attr.to_string()),
        None => (flake_ref.to_string(), String::new()),
    }
}

/// Public wrapper for use from CLI commands (cache-warm).
pub fn fetch_flake_source_public(flake_url: &str) -> Result<std::path::PathBuf, sui_eval::fetcher::FetchError> {
    fetch_flake_source(flake_url)
}

/// Fetch a flake's source directory (clone via tarball).
fn fetch_flake_source(flake_url: &str) -> Result<std::path::PathBuf, sui_eval::fetcher::FetchError> {
    // Parse github:owner/repo or github:owner/repo/ref
    let stripped = flake_url.strip_prefix("github:").ok_or_else(|| {
        sui_eval::fetcher::FetchError::UnsupportedType(format!(
            "only github: flake refs supported, got: {flake_url}"
        ))
    })?;

    let parts: Vec<&str> = stripped.splitn(3, '/').collect();
    if parts.len() < 2 {
        return Err(sui_eval::fetcher::FetchError::MissingField("owner/repo"));
    }
    let owner = parts[0];
    let repo = parts[1];
    let git_ref = parts.get(2).copied().unwrap_or("HEAD");

    let fetcher = InputFetcher::new();
    let locked = sui_compat::flake::LockedInput {
        source_type: "github".to_string(),
        owner: Some(owner.to_string()),
        repo: Some(repo.to_string()),
        rev: Some(git_ref.to_string()),
        nar_hash: None,
        last_modified: None,
        path: None,
        url: None,
        git_ref: None,
        dir: None,
        host: None,
        extra: std::collections::BTreeMap::new(),
    };
    fetcher.fetch(&locked)
}

/// Convert a flake.lock narHash (e.g. "sha256-ABCdef...==") to the 32-char
/// nix base32 hash used for cache.nixos.org narinfo lookups.
///
/// Returns `None` if the hash format is unrecognized.
fn nar_hash_to_store_hash(nar_hash: &str) -> Option<String> {
    // flake.lock uses SRI format: "sha256-<base64>"
    let b64 = nar_hash.strip_prefix("sha256-")?;
    let bytes = base64_decode(b64)?;
    if bytes.len() != 32 {
        return None;
    }
    // Convert to nix base32 (the 32-char hash in /nix/store/HASH-name)
    Some(sui_compat::store_path::nix_base32_encode(&bytes))
}

/// Decode base64 (standard, with or without padding).
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .ok()
        .or_else(|| {
            base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(input)
                .ok()
        })
}

// ── Strategy: eval (full sui-eval derivation resolution) ─────

/// Eval strategy with derivation path caching.
///
/// On cache hit: instant lookup (~0 RAM). On miss: full sui-eval (needs RAM).
/// Use `sui cache warm` to pre-populate the cache on a capable machine.
async fn resolve_with_eval(
    request: &BuildRequest,
    upstream_caches: &[BinaryCacheStore],
    local_storage: &dyn StorageBackend,
) -> Result<(u32, u32), AgentError> {
    let (flake_url, attr_path) = parse_flake_ref(&request.flake_ref);
    let attr_path = if attr_path.is_empty() {
        format!("packages.{}.default", request.system)
    } else {
        attr_path
    };

    info!(flake_url = %flake_url, attr_path = %attr_path, "Evaluating flake (eval strategy)");

    // Fetch the flake source to get a local directory for evaluate_flake_attr.
    let flake_url_owned = flake_url.clone();
    let attr_path_owned = attr_path.clone();
    let out_path = tokio::task::spawn_blocking(move || -> Result<String, AgentError> {
        // Initialize the drv cache on this thread (thread-local).
        sui_eval::drv_cache::init_global_cache();

        // Fetch the flake source directory.
        let flake_dir = fetch_flake_source(&flake_url_owned)
            .map_err(|e| AgentError::Fetch(e.to_string()))?;

        // Evaluate with cache check.
        let segments: Vec<&str> = attr_path_owned.split('.').collect();
        let value = sui_eval::builtins::evaluate_flake_attr(&flake_dir, &segments)
            .map_err(AgentError::Eval)?;

        // Extract outPath.
        let attrs = value.as_attrs().map_err(|e| {
            AgentError::Eval(sui_eval::EvalError::TypeError(format!(
                "expected derivation attrs: {e}"
            )))
        })?;
        attrs
            .get("outPath")
            .ok_or_else(|| AgentError::Fetch("no outPath in result".to_string()))?
            .as_string()
            .map(String::from)
            .map_err(|e| AgentError::Eval(sui_eval::EvalError::TypeError(format!("outPath: {e}"))))
    })
    .await
    .map_err(|e| AgentError::Fetch(format!("eval panicked: {e}")))?
    .map_err(|e| AgentError::Fetch(e.to_string()))?;

    info!(out_path = %out_path, "Derivation resolved");

    let store_path = sui_compat::store_path::StorePath::from_absolute_path(&out_path)
        .map_err(|e| AgentError::Fetch(e.to_string()))?;
    let hash = store_path.hash();

    if local_storage.get_narinfo(&hash).await.ok().flatten().is_some() {
        return Ok((1, 0));
    }

    for cache in upstream_caches {
        if let Ok(Some(narinfo)) = cache.fetch_narinfo(&hash).await {
            if let Ok(nar) = cache.fetch_nar(&narinfo.url).await {
                local_storage.put_nar(&narinfo.url, &nar).await?;
                local_storage.put_narinfo(&hash, &narinfo.serialize()).await?;
                return Ok((1, 0));
            }
        }
    }

    Ok((0, 1))
}

// ── Strategy: nix (legacy shell-out) ─────────────────────────

/// Legacy strategy: shell out to `nix build`. Requires nix binary in container.
async fn resolve_with_nix(request: &BuildRequest) -> Result<(u32, u32), AgentError> {
    let output = tokio::process::Command::new("nix")
        .args(["build", &request.flake_ref, "--no-link", "--print-out-paths"])
        .args(&request.extra_args)
        .output()
        .await
        .map_err(|e| AgentError::Fetch(format!("nix build spawn: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AgentError::Fetch(format!("nix build: {stderr}")));
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(AgentError::Fetch("nix build: no output paths".to_string()));
    }

    info!(store_path = %path, "nix build complete");
    Ok((1, 0))
}

// ── Helpers ──────────────────────────────────────────────────

/// Human-readable description of a locked input.
fn describe_input(locked: &sui_compat::flake::LockedInput) -> String {
    match locked.source_type.as_str() {
        "github" => {
            let owner = locked.owner.as_deref().unwrap_or("?");
            let repo = locked.repo.as_deref().unwrap_or("?");
            let rev = locked
                .rev
                .as_deref()
                .map(|r| &r[..r.len().min(12)])
                .unwrap_or("?");
            format!("github:{owner}/{repo}@{rev}")
        }
        "git" => {
            let url = locked.url.as_deref().unwrap_or("?");
            format!("git:{url}")
        }
        other => format!("{other}:?"),
    }
}

// ── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_config_falls_back_to_local() {
        let cfg = parse_backend_config(None).expect("None resolves");
        match cfg {
            BackendConfig::Local { path } => {
                assert_eq!(path, std::path::PathBuf::from(DEFAULT_LOCAL_CACHE_DIR));
            }
            other => panic!("expected local fallback, got {other:?}"),
        }
    }

    #[test]
    fn blank_config_falls_back_to_local() {
        // A whitespace-only value is treated as unset — the chart never leaves
        // a half-populated env behind, but be defensive.
        let cfg = parse_backend_config(Some("   ")).expect("blank resolves");
        assert!(matches!(cfg, BackendConfig::Local { .. }));
    }

    #[test]
    fn tiered_super_cache_config_parses() {
        // The exact document shape the super-cache-ci chart emits.
        let raw = r#"{
            "type": "tiered",
            "l1": { "type": "redis", "url": "redis://redis.super-cache-ci.svc:6379" },
            "l2": { "type": "pg", "url": "postgres://sui@postgres.super-cache-ci.svc:5432/sui", "max_conns": 8 },
            "l3": { "type": "s3", "bucket": "sui-super-cache", "region": "us-east-1" },
            "write_policy": "write-through"
        }"#;
        let cfg = parse_backend_config(Some(raw)).expect("tiered doc parses");
        match cfg {
            BackendConfig::Tiered { l1, l2, l3, .. } => {
                assert!(matches!(*l1, BackendConfig::Redis { .. }));
                assert!(matches!(*l2, BackendConfig::Pg { .. }));
                assert!(matches!(*l3, BackendConfig::S3 { .. }));
            }
            other => panic!("expected tiered, got {other:?}"),
        }
    }

    #[test]
    fn malformed_config_is_a_typed_error_not_a_silent_local() {
        // A mis-authored config must fail loudly, never masquerade as a healthy
        // local cache (that would hide the operator's mistake).
        let err = parse_backend_config(Some("{not json")).expect_err("must error");
        let msg = err.to_string();
        assert!(msg.contains(BACKEND_CONFIG_ENV), "error names the env var: {msg}");
    }

    #[test]
    fn describe_backend_labels_every_arm() {
        assert_eq!(
            describe_backend(&BackendConfig::Local { path: "/x".into() }),
            "local"
        );
        assert_eq!(
            describe_backend(&BackendConfig::Redis {
                url: "redis://r:6379".into(),
                ttl_secs: None,
            }),
            "redis"
        );
        assert_eq!(
            describe_backend(&BackendConfig::Pg {
                url: "postgres://p:5432/s".into(),
                max_conns: 8,
            }),
            "pg"
        );
        assert_eq!(
            describe_backend(&BackendConfig::Tiered {
                l1: Box::new(BackendConfig::Redis { url: "redis://r:6379".into(), ttl_secs: None }),
                l2: Box::new(BackendConfig::Pg { url: "postgres://p:5432/s".into(), max_conns: 8 }),
                l3: Box::new(BackendConfig::Local { path: "/x".into() }),
                write_policy: Default::default(),
            }),
            "tiered"
        );
    }
}
