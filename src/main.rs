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
            // Delegate to `nix build` for now — native build pipeline will
            // be wired once the substitution layer is production-ready.
            let output = tokio::process::Command::new("nix")
                .args(["build", "--print-out-paths", &installable])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await?;

            if output.status.success() {
                let paths = String::from_utf8_lossy(&output.stdout);
                for path in paths.lines() {
                    println!("{path}");
                }
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!("{stderr}");
                return Err(CliError::Orchestrate {
                    operation: "build",
                    message: "nix build failed".into(),
                });
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
                let mut args = vec!["flake"];
                if let Some(ref inp) = input {
                    args.extend(["lock", "--update-input", inp]);
                } else {
                    args.push("update");
                }
                let output = tokio::process::Command::new("nix")
                    .args(&args)
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .status()
                    .await?;
                if !output.success() {
                    return Err(CliError::Orchestrate {
                        operation: "flake update",
                        message: "nix flake update failed".into(),
                    });
                }
            }
            FlakeCommands::Check { no_build } => {
                let mut args = vec!["flake", "check"];
                if no_build {
                    args.push("--no-build");
                }
                let output = tokio::process::Command::new("nix")
                    .args(&args)
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .status()
                    .await?;
                if !output.success() {
                    return Err(CliError::Orchestrate {
                        operation: "flake check",
                        message: "nix flake check failed".into(),
                    });
                }
            }
            FlakeCommands::Lock => {
                let output = tokio::process::Command::new("nix")
                    .args(["flake", "lock"])
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .status()
                    .await?;
                if !output.success() {
                    return Err(CliError::Orchestrate {
                        operation: "flake lock",
                        message: "nix flake lock failed".into(),
                    });
                }
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
                    let result = if let Some(ref flake_ref) = flake {
                        // Try native rebuild first — this uses sui's own
                        // eval+build pipeline with nix delegation for the MVP.
                        match sys.rebuild_native(flake_ref, action).await {
                            Ok(r) => r,
                            Err(e) => {
                                tracing::warn!("native rebuild failed ({e}), falling back to delegated rebuild");
                                sys.rebuild(action, flake.as_deref()).await.map_err(|e| {
                                    CliError::Orchestrate {
                                        operation: "rebuild",
                                        message: e.to_string(),
                                    }
                                })?
                            }
                        }
                    } else {
                        sys.rebuild(action, flake.as_deref()).await.map_err(|e| {
                            CliError::Orchestrate {
                                operation: "rebuild",
                                message: e.to_string(),
                            }
                        })?
                    };
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
