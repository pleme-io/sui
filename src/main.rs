use std::sync::Arc;

use clap::{Parser, Subcommand};
use sui::{CliError, NIX_DB_PATH};
use sui_cache::StorageBackend as _;
use sui_store::{LocalStore, Store, Substitutor};

#[derive(Parser)]
#[command(name = "sui", version, about = "Rust-native Nix replacement")]
struct Cli {
    /// Use bytecode VM for evaluation (default; kept for compatibility)
    #[arg(long, global = true)]
    vm: bool,
    /// Fall back to tree-walker instead of bytecode VM
    #[arg(long, global = true, conflicts_with = "vm")]
    no_vm: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the API server (REST + GraphQL + gRPC)
    Serve {
        /// REST/GraphQL listen address
        #[arg(long, default_value = "0.0.0.0:8080")]
        listen: String,
        /// gRPC listen address
        #[arg(long, default_value = "0.0.0.0:50051")]
        grpc_listen: String,
    },
    /// Store operations
    Store {
        #[command(subcommand)]
        command: StoreCommands,
    },
    /// Evaluate Nix expressions
    Eval {
        /// Expression to evaluate
        expression: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Maximum thunk force depth (0 = unlimited)
        #[arg(long, default_value = "0")]
        max_force_depth: usize,
    },
    /// Build derivations
    Build {
        /// Installable to build (e.g., nixpkgs#hello)
        installable: String,
    },
    /// Flake operations
    Flake {
        #[command(subcommand)]
        command: FlakeCommands,
    },
    /// Run the Nix daemon
    Daemon {
        /// Socket path
        #[arg(long, default_value = "/tmp/sui-daemon.sock")]
        socket: String,
    },
    /// System operations (rebuild, switch, rollback)
    System {
        #[command(subcommand)]
        command: SystemCommands,
    },
    /// Fleet management
    Fleet {
        #[command(subcommand)]
        command: FleetCommands,
    },
    /// Binary cache operations
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
    },
}

#[derive(Subcommand)]
enum StoreCommands {
    /// Query path info
    PathInfo {
        /// Store path or hash
        path: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// List all valid paths
    Paths {
        /// Maximum number of paths
        #[arg(long, default_value = "100")]
        limit: usize,
    },
    /// Garbage collection
    Gc,
    /// Verify store integrity
    Verify,
    /// Show store info
    Info,
}

#[derive(Subcommand)]
enum FlakeCommands {
    /// Show flake outputs
    Show { flake_ref: Option<String> },
    /// Update flake lock file (or a specific input)
    Update {
        /// Specific input to update (e.g. `nixpkgs`)
        input: Option<String>,
    },
    /// Check the flake for errors
    Check {
        /// Skip building checks
        #[arg(long)]
        no_build: bool,
    },
    /// Lock the flake inputs without updating
    Lock,
    /// Show flake metadata
    Metadata { flake_ref: Option<String> },
}

#[derive(Subcommand)]
enum SystemCommands {
    /// Rebuild and switch to new configuration
    Rebuild {
        /// Flake reference
        #[arg(long)]
        flake: Option<String>,
    },
    /// Show current system status
    Status,
    /// Rollback to previous generation
    Rollback,
}

#[derive(Subcommand)]
enum FleetCommands {
    /// List fleet nodes
    Nodes,
    /// Deploy to nodes
    Deploy {
        /// Target (node name or @group)
        target: String,
    },
    /// Show fleet status
    Status,
}

#[derive(Subcommand)]
enum CacheCommands {
    /// Start the binary cache server
    Serve {
        /// Listen address
        #[arg(long, default_value = "0.0.0.0:5000")]
        listen: String,
        /// Local storage path
        #[arg(long, default_value = "/var/cache/sui")]
        store_path: String,
        /// Cache priority (lower = preferred)
        #[arg(long, default_value = "40")]
        priority: u32,
    },
    /// Push store paths to the cache
    Push {
        /// Store paths to push
        paths: Vec<String>,
        /// Cache URL (for remote push)
        #[arg(long)]
        cache_url: Option<String>,
        /// Local storage path (for local push)
        #[arg(long, default_value = "/var/cache/sui")]
        store_path: String,
        /// Path to signing secret key
        #[arg(long)]
        signing_key: Option<String>,
    },
    /// Garbage collect the cache
    Gc {
        /// Local storage path
        #[arg(long, default_value = "/var/cache/sui")]
        store_path: String,
        /// Hashes to keep (roots)
        #[arg(long)]
        keep: Vec<String>,
    },
    /// Show cache info
    Info {
        /// Local storage path
        #[arg(long, default_value = "/var/cache/sui")]
        store_path: String,
    },
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<(), CliError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { listen, grpc_listen } => {
            tracing::info!("starting sui API server on {listen} (REST/GraphQL) and {grpc_listen} (gRPC)");
            sui::api::serve(&listen, &grpc_listen).await?;
        }

        Commands::Store { command } => {
            let store = open_store().await?;
            match command {
                StoreCommands::PathInfo { path, json } => {
                    let sp = sui::parse_store_path(&path)?;
                    match store.query_path_info(&sp).await? {
                        Some(info) => {
                            if json {
                                println!("{}", serde_json::to_string_pretty(&info)?);
                            } else {
                                println!("Path:         {}", info.path);
                                println!("NarHash:      {}", info.nar_hash);
                                println!("NarSize:      {}", info.nar_size);
                                println!("References:   {}", info.references.join(" "));
                                if let Some(ref d) = info.deriver {
                                    println!("Deriver:      {d}");
                                }
                                if !info.signatures.is_empty() {
                                    println!("Signatures:   {}", info.signatures.join(" "));
                                }
                            }
                        }
                        None => {
                            return Err(CliError::PathNotValid(sp.to_absolute_path()));
                        }
                    }
                }
                StoreCommands::Paths { limit } => {
                    let paths = store.query_all_valid_paths().await?;
                    for path in paths.iter().take(limit) {
                        println!("{}", path.to_absolute_path());
                    }
                    if paths.len() > limit {
                        eprintln!("... and {} more (use --limit to show more)", paths.len() - limit);
                    }
                }
                StoreCommands::Gc => {
                    let rw_store = LocalStore::open_rw(NIX_DB_PATH)
                        .await
                        .map_err(|e| CliError::StoreOpen {
                            path: NIX_DB_PATH,
                            source: e,
                        })?;
                    let options = sui_store::GcOptions::default();
                    let result = rw_store.collect_garbage(&options).await?;
                    println!(
                        "deleted {} paths, freed {} bytes",
                        result.paths_deleted, result.bytes_freed
                    );
                }
                StoreCommands::Verify => {
                    let result = store.verify_store().await?;
                    println!(
                        "checked {} paths: {} valid, {} corrupt",
                        result.total_checked, result.valid_count, result.corrupt.len()
                    );
                    for bad in &result.corrupt {
                        eprintln!(
                            "CORRUPT: {} — expected {}, got {}",
                            bad.path, bad.expected_hash, bad.actual_hash
                        );
                    }
                    if !result.corrupt.is_empty() {
                        std::process::exit(1);
                    }
                }
                StoreCommands::Info => {
                    let paths = store.query_all_valid_paths().await?;
                    println!("Store dir:    /nix/store");
                    println!("Valid paths:  {}", paths.len());
                    println!("Database:     {NIX_DB_PATH}");
                }
            }
        }

        Commands::Eval { expression, json, max_force_depth } => {
            let expr = expression
                .ok_or_else(|| CliError::MissingArgument("no expression provided".into()))?;
            if max_force_depth > 0 {
                sui_eval::trace::set_max_force_depth(max_force_depth);
            }
            if cli.no_vm {
                // Tree-walker evaluation path (legacy fallback).
                let value = sui_eval::eval(&expr)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&value.to_json())?);
                } else {
                    println!("{value}");
                }
            } else {
                // Bytecode VM evaluation path (default).
                let result = sui_bytecode::eval_full(&expr).map_err(|e| {
                    CliError::Orchestrate {
                        operation: "eval",
                        message: e.to_string(),
                    }
                })?;
                if json {
                    let sk = result.to_string_keyed();
                    let json_val = string_keyed_to_json(&sk);
                    println!("{}", serde_json::to_string_pretty(&json_val)?);
                } else {
                    let sk = result.to_string_keyed();
                    println!("{sk}");
                }
            }
        }

        Commands::Build { installable } => {
            use sui_build::{BuildClosure, LocalBuilder};

            // Open the store and set up the build infrastructure.
            let store = sui_store::LocalStore::open_rw(NIX_DB_PATH)
                .await
                .map_err(|e| CliError::Orchestrate {
                    operation: "build",
                    message: format!("store open: {e}"),
                })?;
            let store: std::sync::Arc<dyn sui_store::Store> = std::sync::Arc::new(store);
            let caches = sui_orchestrate::build_caches(&sui_orchestrate::get_substituters());
            let substitutor = Substitutor::new(store.clone(), caches);

            #[cfg(target_os = "macos")]
            let sandbox: Box<dyn sui_build::sandbox::Sandbox> =
                Box::new(sui_build::sandbox::DarwinSandbox::new());
            #[cfg(not(target_os = "macos"))]
            let sandbox: Box<dyn sui_build::sandbox::Sandbox> =
                Box::new(sui_build::sandbox::LinuxSandbox::new());

            let builder = LocalBuilder::new(store, sandbox);

            if std::path::Path::new(&installable).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("drv")) {
                // Direct .drv path — build it.
                let closure = BuildClosure::compute(&installable).map_err(|e| {
                    CliError::Orchestrate {
                        operation: "build",
                        message: format!("closure: {e}"),
                    }
                })?;
                let result = builder
                    .build_closure(&closure, Some(&substitutor))
                    .await
                    .map_err(|e| CliError::Orchestrate {
                        operation: "build",
                        message: e.to_string(),
                    })?;
                for output in &result.outputs {
                    println!("{}", output.to_absolute_path());
                }
            } else {
                // Parse as a flake reference, evaluate, extract drvPath, build.
                let flake_ref =
                    sui_compat::flake_ref::FlakeRef::parse(&installable).map_err(|e| {
                        CliError::Orchestrate {
                            operation: "build",
                            message: format!("flake ref parse: {e}"),
                        }
                    })?;
                let flake_result = sui_eval::builtins::evaluate_flake(
                    &flake_ref.flake_dir,
                )
                .map_err(|e| CliError::Orchestrate {
                    operation: "build",
                    message: format!("eval: {e}"),
                })?;
                let attr_segments: Vec<&str> = flake_ref.attribute.split('.').collect();
                let target = sui_eval::builtins::navigate_attrs(&flake_result, &attr_segments)
                    .map_err(|e| CliError::Orchestrate {
                        operation: "build",
                        message: format!("navigate: {e}"),
                    })?;
                // Extract drvPath from the derivation attrset.
                let drv_path = match &target {
                    sui_eval::Value::Attrs(attrs) => {
                        attrs.get("drvPath")
                            .and_then(|v| v.as_string().ok())
                            .map(std::string::ToString::to_string)
                    }
                    _ => None,
                };
                if let Some(drv_path) = drv_path {
                    let closure =
                        BuildClosure::compute(&drv_path).map_err(|e| CliError::Orchestrate {
                            operation: "build",
                            message: format!("closure: {e}"),
                        })?;
                    let result = builder
                        .build_closure(&closure, Some(&substitutor))
                        .await
                        .map_err(|e| CliError::Orchestrate {
                            operation: "build",
                            message: e.to_string(),
                        })?;
                    for output in &result.outputs {
                        println!("{}", output.to_absolute_path());
                    }
                } else {
                    // Not a derivation — just display the evaluated value.
                    println!("{target}");
                }
            }
        }

        Commands::Flake { command } => match command {
            FlakeCommands::Show { flake_ref } => {
                let flake_dir = resolve_flake_dir(flake_ref.as_deref())?;
                let outputs = sui_eval::builtins::evaluate_flake(&flake_dir)
                    .map_err(|e| CliError::Orchestrate {
                        operation: "flake show",
                        message: format!("eval: {e}"),
                    })?;
                print_flake_tree(&outputs);
            }
            FlakeCommands::Update { input } => {
                let flake_dir = std::env::current_dir()?;
                if let Some(ref name) = input {
                    sui_eval::flake_lock::update_input(&flake_dir, name).map_err(|e| {
                        CliError::Orchestrate {
                            operation: "flake update",
                            message: e.to_string(),
                        }
                    })?;
                    println!("updated input: {name}");
                } else {
                    let updated =
                        sui_eval::flake_lock::update_all_inputs(&flake_dir).map_err(|e| {
                            CliError::Orchestrate {
                                operation: "flake update",
                                message: e.to_string(),
                            }
                        })?;
                    println!(
                        "updated {} inputs: {}",
                        updated.len(),
                        updated.join(", ")
                    );
                }
            }
            FlakeCommands::Check { no_build: _ } => {
                let flake_dir = std::env::current_dir()?;
                let result =
                    sui_eval::flake_lock::check_flake(&flake_dir).map_err(|e| {
                        CliError::Orchestrate {
                            operation: "flake check",
                            message: e.to_string(),
                        }
                    })?;
                if result.valid {
                    println!("flake check passed");
                } else {
                    for err in &result.errors {
                        eprintln!("error: {err}");
                    }
                    std::process::exit(1);
                }
            }
            FlakeCommands::Lock => {
                let flake_dir = std::env::current_dir()?;
                sui_eval::flake_lock::update_all_inputs(&flake_dir).map_err(|e| {
                    CliError::Orchestrate {
                        operation: "flake lock",
                        message: e.to_string(),
                    }
                })?;
                println!("flake.lock written");
            }
            FlakeCommands::Metadata { flake_ref } => {
                let flake_dir = resolve_flake_dir(flake_ref.as_deref())?;
                print_flake_metadata(&flake_dir)?;
            }
        },

        Commands::Daemon { socket } => {
            tracing::info!("starting sui daemon on {socket}");
            let store = open_store().await?;
            let config = sui_daemon::DaemonConfig::with_socket_path(&socket);
            let server = sui_daemon::DaemonServer::new(config, store);
            server.run().await.map_err(|e| CliError::Orchestrate {
                operation: "daemon",
                message: e.to_string(),
            })?;
        }

        Commands::System { command } => {
            let sys = sui_orchestrate::SystemOrchestrator::new().map_err(|e| {
                CliError::Orchestrate {
                    operation: "platform detection",
                    message: e.to_string(),
                }
            })?;
            match command {
                SystemCommands::Rebuild { flake } => {
                    let action = sui_orchestrate::RebuildAction::Switch;
                    let flake_ref = flake.unwrap_or_else(|| ".".to_string());
                    let result = sys.rebuild_native(&flake_ref, action).await.map_err(|e| {
                        CliError::Orchestrate {
                            operation: "rebuild",
                            message: e.to_string(),
                        }
                    })?;
                    println!("rebuild {} in {:.1}s", if result.success { "succeeded" } else { "failed" }, result.duration_secs);
                    if let Some(generation) = result.generation {
                        println!("generation: {generation}");
                    }
                    if !result.success {
                        eprintln!("{}", result.log);
                    }
                }
                SystemCommands::Status => {
                    let current = sys.current_generation().await.unwrap_or(0);
                    println!("platform:   {}", sys.platform().rebuild_command());
                    println!("generation: {current}");
                }
                SystemCommands::Rollback => {
                    let result = sys.rollback().await.map_err(|e| CliError::Orchestrate {
                        operation: "rollback",
                        message: e.to_string(),
                    })?;
                    println!("rollback {} in {:.1}s",
                        if result.success { "succeeded" } else { "failed" },
                        result.duration_secs);
                }
            }
        },

        Commands::Fleet { command } => {
            let registry = sui_orchestrate::node::NodeRegistry::new();
            let orch = sui_orchestrate::FleetOrchestrator::new(registry);
            match command {
                FleetCommands::Nodes => {
                    if orch.registry().is_empty() {
                        println!("no fleet nodes configured");
                        println!("hint: add nodes to your fleet configuration");
                    } else {
                        for node in orch.registry().all() {
                            println!("{:<15} {:<10} {}", node.hostname, node.status, node.flake_ref);
                        }
                    }
                }
                FleetCommands::Deploy { target } => {
                    let mut orch = orch;
                    let result = orch
                        .deploy(&target, sui_orchestrate::DeployStrategy::Rolling, None)
                        .await
                        .map_err(|e| CliError::Deploy(e.to_string()))?;
                    println!("deployed to {} — {}/{} succeeded in {:.1}s",
                        result.target, result.succeeded, result.total_nodes, result.duration_secs);
                }
                FleetCommands::Status => {
                    let counts = orch.registry().status_counts();
                    println!("total:     {}", counts.total);
                    println!("online:    {}", counts.online);
                    println!("deploying: {}", counts.deploying);
                    println!("failed:    {}", counts.failed);
                    println!("offline:   {}", counts.offline);
                }
            }
        },

        Commands::Cache { command } => match command {
            CacheCommands::Serve { listen, store_path, priority } => {
                let config = sui_cache::CacheConfig {
                    listen,
                    backend: sui_cache::BackendConfig::Local {
                        path: std::path::PathBuf::from(&store_path),
                    },
                    priority,
                    ..sui_cache::CacheConfig::default()
                };
                let storage: Arc<dyn sui_cache::StorageBackend> =
                    Arc::new(sui_cache::LocalStorage::new(&store_path));
                sui_cache::serve(config, storage).await.map_err(|e| {
                    CliError::Orchestrate {
                        operation: "cache serve",
                        message: e.to_string(),
                    }
                })?;
            }
            CacheCommands::Push { paths, cache_url: _, store_path, signing_key } => {
                let storage: Arc<dyn sui_cache::StorageBackend> =
                    Arc::new(sui_cache::LocalStorage::new(&store_path));
                let signer = if let Some(key_path) = signing_key {
                    let key_str = std::fs::read_to_string(&key_path).map_err(|e| {
                        CliError::Orchestrate {
                            operation: "cache push",
                            message: format!("read signing key: {e}"),
                        }
                    })?;
                    sui_cache::CacheSigner::from_secret_key_string(key_str.trim()).map_err(|e| {
                        CliError::Orchestrate {
                            operation: "cache push",
                            message: format!("parse signing key: {e}"),
                        }
                    })?
                } else {
                    sui_cache::CacheSigner::generate("sui-cache".to_string())
                };

                for path in &paths {
                    let hash = path
                        .strip_prefix("/nix/store/")
                        .unwrap_or(path)
                        .split('-')
                        .next()
                        .unwrap_or(path);
                    match sui_cache::push::push_path(
                        storage.as_ref(),
                        &signer,
                        path,
                        hash,
                        &[],
                        None,
                    )
                    .await
                    {
                        Ok(result) => {
                            println!(
                                "pushed {} (nar={}, compressed={})",
                                path, result.nar_size, result.compressed_size
                            );
                        }
                        Err(e) => {
                            eprintln!("error pushing {path}: {e}");
                        }
                    }
                }
            }
            CacheCommands::Gc { store_path, keep } => {
                let storage = sui_cache::LocalStorage::new(&store_path);
                let result = sui_cache::gc::collect_garbage(&storage, &keep).await.map_err(|e| {
                    CliError::Orchestrate {
                        operation: "cache gc",
                        message: e.to_string(),
                    }
                })?;
                println!(
                    "GC: deleted {} paths, freed {} bytes",
                    result.paths_deleted, result.bytes_freed
                );
            }
            CacheCommands::Info { store_path } => {
                let storage = sui_cache::LocalStorage::new(&store_path);
                let hashes = storage.list_narinfos().await.map_err(|e| {
                    CliError::Orchestrate {
                        operation: "cache info",
                        message: e.to_string(),
                    }
                })?;
                println!("Cache dir:   {store_path}");
                println!("Paths:       {}", hashes.len());
            }
        },
    }

    Ok(())
}

async fn open_store() -> Result<LocalStore, CliError> {
    LocalStore::open(NIX_DB_PATH)
        .await
        .map_err(|e| CliError::StoreOpen {
            path: NIX_DB_PATH,
            source: e,
        })
}

/// Resolve a flake directory from an optional CLI argument.
///
/// If `None` or `"."`, returns the current working directory.
/// Otherwise treats the argument as a path.
fn resolve_flake_dir(flake_ref: Option<&str>) -> Result<std::path::PathBuf, CliError> {
    match flake_ref {
        None | Some("") | Some(".") => Ok(std::env::current_dir()?),
        Some(path) => {
            let p = std::path::PathBuf::from(path);
            if p.is_dir() {
                Ok(p)
            } else {
                // Maybe it has a # attribute — extract dir part.
                let dir_part = path.split('#').next().unwrap_or(".");
                let d = std::path::PathBuf::from(dir_part);
                if d.is_dir() {
                    Ok(d)
                } else {
                    Ok(std::env::current_dir()?)
                }
            }
        }
    }
}

// ── flake show ──────────────────────────────────────────────────

/// Print a tree of flake outputs matching `nix flake show` format.
fn print_flake_tree(outputs: &sui_eval::Value) {
    let sui_eval::Value::Attrs(attrs) = outputs else {
        println!("(not an attrset)");
        return;
    };

    let keys: Vec<&String> = attrs.keys().collect();
    let total = keys.len();
    for (i, key) in keys.iter().enumerate() {
        let is_last = i + 1 == total;
        let connector = if is_last { "\u{2514}\u{2500}\u{2500}\u{2500}" } else { "\u{251c}\u{2500}\u{2500}\u{2500}" };
        let child_prefix = if is_last { "    " } else { "\u{2502}   " };

        if let Some(child) = attrs.get(key) {
            let child = sui_eval::eval::force_value(child).unwrap_or_else(|_| child.clone());
            let desc = classify_output(key, &child);
            if let Some(d) = desc {
                println!("{connector}{key}: {d}");
            } else {
                // It's a nested attrset — recurse.
                println!("{connector}{key}");
                if let sui_eval::Value::Attrs(ref inner) = child {
                    print_tree_inner(inner, child_prefix);
                }
            }
        }
    }
}

/// Recursively print a tree of attributes.
fn print_tree_inner(attrs: &sui_eval::value::NixAttrs, prefix: &str) {
    let keys: Vec<&String> = attrs.keys().collect();
    let total = keys.len();
    for (i, key) in keys.iter().enumerate() {
        let is_last = i + 1 == total;
        let connector = if is_last { "\u{2514}\u{2500}\u{2500}\u{2500}" } else { "\u{251c}\u{2500}\u{2500}\u{2500}" };
        let child_prefix = if is_last {
            format!("{prefix}    ")
        } else {
            format!("{prefix}\u{2502}   ")
        };

        if let Some(child) = attrs.get(key) {
            let child = sui_eval::eval::force_value(child).unwrap_or_else(|_| child.clone());
            let desc = classify_output(key, &child);
            if let Some(d) = desc {
                println!("{prefix}{connector}{key}: {d}");
            } else {
                println!("{prefix}{connector}{key}");
                if let sui_eval::Value::Attrs(ref inner) = child {
                    print_tree_inner(inner, &child_prefix);
                }
            }
        }
    }
}

/// Classify a flake output for display. Returns `None` if the value
/// should be recursed into (nested attrset), or `Some(description)`.
fn classify_output(key: &str, value: &sui_eval::Value) -> Option<String> {
    match value {
        sui_eval::Value::Lambda(_) | sui_eval::Value::Builtin(_) => {
            // Overlays and nixosModules are typically functions.
            if key.contains("overlay") || key.contains("Overlay") {
                Some("Nixpkgs overlay".to_string())
            } else if key.contains("module") || key.contains("Module") {
                Some("NixOS module".to_string())
            } else {
                Some("function".to_string())
            }
        }
        sui_eval::Value::Attrs(attrs) => {
            // Check if it's a derivation (has type = "derivation").
            if let Some(t) = attrs.get("type") {
                if let Ok(s) = t.as_string() {
                    if s == "derivation" {
                        return Some("package".to_string());
                    }
                }
            }
            // Check for well-known output names.
            match key {
                k if k.ends_with("Configurations") || k.ends_with("configurations") => {
                    // Leaf entries under *Configurations are configuration objects.
                    return None;
                }
                "darwinConfigurations" | "nixosConfigurations" => return None,
                "packages" | "devShells" | "apps" | "checks" | "legacyPackages" => return None,
                _ => {}
            }
            // If this is a derivation-like attrs (has drvPath), label it.
            if attrs.get("drvPath").is_some() {
                return Some("derivation".to_string());
            }
            // Check parent context — known types.
            None
        }
        sui_eval::Value::String(s) => Some(format!("\"{}\"", s.chars)),
        sui_eval::Value::Bool(b) => Some(format!("{b}")),
        sui_eval::Value::Int(n) => Some(format!("{n}")),
        _ => Some(value.type_name().to_string()),
    }
}

// ── flake metadata ──────────────────────────────────────────────

/// Print flake metadata: description, path, revision, inputs.
fn print_flake_metadata(flake_dir: &std::path::Path) -> Result<(), CliError> {
    // Read description from flake.nix (simple heuristic: look for `description =`).
    let flake_nix_path = flake_dir.join("flake.nix");
    let description = if flake_nix_path.exists() {
        let content = std::fs::read_to_string(&flake_nix_path)?;
        extract_description(&content)
    } else {
        None
    };

    if let Some(desc) = &description {
        println!("Description: {desc}");
    }
    println!("Path:        {}", flake_dir.display());

    // Git revision (if available).
    if let Ok(rev) = get_git_revision(flake_dir) {
        println!("Revision:    {rev}");
    }

    // Last modified from git.
    if let Ok(date) = get_last_modified(flake_dir) {
        println!("Last modified: {date}");
    }

    // Read inputs from flake.lock.
    let lock_path = flake_dir.join("flake.lock");
    if lock_path.exists() {
        let lock_json = std::fs::read_to_string(&lock_path)?;
        let lock: sui_compat::flake::FlakeLock = serde_json::from_str(&lock_json)
            .map_err(|e| CliError::Orchestrate {
                operation: "flake metadata",
                message: format!("parse flake.lock: {e}"),
            })?;

        if let Some(root_node) = lock.nodes.get(&lock.root) {
            if !root_node.inputs.is_empty() {
                println!("Inputs:");
                let input_names: Vec<&String> = root_node.inputs.keys().collect();
                let total = input_names.len();
                for (i, name) in input_names.iter().enumerate() {
                    let is_last = i + 1 == total;
                    let connector = if is_last { "\u{2514}\u{2500}\u{2500}\u{2500}" } else { "\u{251c}\u{2500}\u{2500}\u{2500}" };

                    // Resolve the node reference.
                    let node_name = match root_node.inputs.get(*name) {
                        Some(sui_compat::flake::InputRef::Direct(n)) => n.clone(),
                        Some(sui_compat::flake::InputRef::Follows(path)) => path.join("/"),
                        None => continue,
                    };

                    if let Some(node) = lock.nodes.get(&node_name) {
                        let url = format_input_url(node);
                        println!("{connector}{name}: {url}");
                        if let Some(ref locked) = node.locked {
                            let child_prefix = if is_last { "    " } else { "\u{2502}   " };
                            if let Some(ref rev) = locked.rev {
                                let short_rev = &rev[..12.min(rev.len())];
                                println!("{child_prefix}Revision: {short_rev}...");
                            }
                        }
                    } else {
                        println!("{connector}{name}: follows {node_name}");
                    }
                }
            }
        }
    }

    Ok(())
}

/// Extract the `description` attribute from a flake.nix source.
fn extract_description(source: &str) -> Option<String> {
    // Look for `description = "..."` pattern.
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("description") {
            let rest = rest.trim();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim();
                if let Some(rest) = rest.strip_prefix('"') {
                    if let Some(end) = rest.find('"') {
                        return Some(rest[..end].to_string());
                    }
                }
            }
        }
    }
    None
}

/// Get the git HEAD revision of a directory.
fn get_git_revision(dir: &std::path::Path) -> Result<String, std::io::Error> {
    let head_file = dir.join(".git/HEAD");
    if !head_file.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not a git repo",
        ));
    }
    let head = std::fs::read_to_string(&head_file)?;
    let head = head.trim();
    if let Some(ref_path) = head.strip_prefix("ref: ") {
        let ref_file = dir.join(format!(".git/{ref_path}"));
        if ref_file.exists() {
            let rev = std::fs::read_to_string(&ref_file)?;
            return Ok(rev.trim().to_string());
        }
        // Could be a packed ref.
        let packed_refs = dir.join(".git/packed-refs");
        if packed_refs.exists() {
            let content = std::fs::read_to_string(&packed_refs)?;
            for line in content.lines() {
                if line.ends_with(ref_path) {
                    if let Some(rev) = line.split_whitespace().next() {
                        return Ok(rev.to_string());
                    }
                }
            }
        }
    }
    // Detached HEAD — HEAD contains the rev directly.
    if head.len() >= 40 {
        return Ok(head.to_string());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "could not resolve HEAD",
    ))
}

/// Get the last modified date from git log.
fn get_last_modified(dir: &std::path::Path) -> Result<String, std::io::Error> {
    // Read git log for the latest commit timestamp using the reflog.
    // For simplicity, just return the mtime of flake.nix.
    let flake_nix = dir.join("flake.nix");
    if flake_nix.exists() {
        let metadata = std::fs::metadata(&flake_nix)?;
        let modified = metadata.modified()?;
        let secs = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let days = secs / 86400;
        let (y, m, d) = days_to_ymd(days);
        return Ok(format!("{y:04}-{m:02}-{d:02}"));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no flake.nix",
    ))
}

/// Format a flake input URL from node metadata.
fn format_input_url(node: &sui_compat::flake::FlakeNode) -> String {
    if let Some(ref orig) = node.original {
        let source_type = &orig.source_type;
        match (source_type.as_str(), &orig.owner, &orig.repo) {
            ("github", Some(owner), Some(repo)) => {
                let suffix = orig.git_ref.as_deref().map_or(String::new(), |r| format!("/{r}"));
                format!("github:{owner}/{repo}{suffix}")
            }
            ("gitlab", Some(owner), Some(repo)) => format!("gitlab:{owner}/{repo}"),
            ("git", _, _) if orig.url.is_some() => {
                format!("git+{}", orig.url.as_deref().unwrap_or("?"))
            }
            ("path", _, _) if orig.extra.get("path").is_some() => {
                format!("path:{}", orig.extra.get("path").and_then(|v| v.as_str()).unwrap_or("?"))
            }
            _ => format!("{source_type}:?"),
        }
    } else {
        "(unknown)".to_string()
    }
}

/// Convert a `StringKeyedValue` from the bytecode VM to `serde_json::Value`.
fn string_keyed_to_json(sk: &sui_bytecode::StringKeyedValue) -> serde_json::Value {
    match sk {
        sui_bytecode::StringKeyedValue::Null => serde_json::Value::Null,
        sui_bytecode::StringKeyedValue::Bool(b) => serde_json::Value::Bool(*b),
        sui_bytecode::StringKeyedValue::Int(n) => serde_json::json!(n),
        sui_bytecode::StringKeyedValue::Float(f) => serde_json::json!(f),
        sui_bytecode::StringKeyedValue::String(s) => serde_json::Value::String(s.clone()),
        sui_bytecode::StringKeyedValue::Path(p) => serde_json::Value::String(p.clone()),
        sui_bytecode::StringKeyedValue::List(items) => {
            serde_json::Value::Array(items.iter().map(string_keyed_to_json).collect())
        }
        sui_bytecode::StringKeyedValue::Attrs(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), string_keyed_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
        sui_bytecode::StringKeyedValue::Lambda => {
            serde_json::Value::String("<lambda>".to_string())
        }
    }
}

/// Convert days-since-epoch to (year, month, day).
fn days_to_ymd(total_days: u64) -> (u64, u64, u64) {
    let z = total_days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
