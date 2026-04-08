use clap::{Parser, Subcommand};
use sui::{CliError, NIX_DB_PATH};
use sui_store::{LocalStore, Store};

#[derive(Parser)]
#[command(name = "sui", version, about = "Rust-native Nix replacement")]
struct Cli {
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

#[tokio::main]
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
                    println!("sui store gc — not yet implemented (Phase 5)");
                }
                StoreCommands::Verify => {
                    println!("sui store verify — not yet implemented (Phase 5)");
                }
                StoreCommands::Info => {
                    let paths = store.query_all_valid_paths().await?;
                    println!("Store dir:    /nix/store");
                    println!("Valid paths:  {}", paths.len());
                    println!("Database:     {NIX_DB_PATH}");
                }
            }
        }

        Commands::Eval { expression, json } => {
            let expr = expression
                .ok_or_else(|| CliError::MissingArgument("no expression provided".into()))?;
            let value = sui_eval::eval(&expr)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&value.to_json())?);
            } else {
                println!("{value}");
            }
        }

        Commands::Build { installable } => {
            use sui_build::{BuildClosure, LocalBuilder};
            use sui_store::{BinaryCacheStore, Substitutor};

            // Open the store and set up the build infrastructure.
            let store = sui_store::LocalStore::open_rw(NIX_DB_PATH)
                .await
                .map_err(|e| CliError::Orchestrate {
                    operation: "build",
                    message: format!("store open: {e}"),
                })?;
            let store: std::sync::Arc<dyn sui_store::Store> = std::sync::Arc::new(store);
            let caches: Vec<std::sync::Arc<BinaryCacheStore>> = vec![std::sync::Arc::new(
                BinaryCacheStore::new("https://cache.nixos.org", vec![]),
            )];
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
                println!(
                    "sui flake show {} — not yet implemented",
                    flake_ref.unwrap_or_default()
                );
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
                println!(
                    "sui flake metadata {} — not yet implemented",
                    flake_ref.unwrap_or_default()
                );
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
