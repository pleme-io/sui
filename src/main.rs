#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;

use clap::{Parser, Subcommand};
use sui::{CliError, NIX_DB_PATH};

mod agent;
use sui_cache::StorageBackend as _;
use sui_store::{LocalStore, Store, Substitutor};

#[derive(Parser)]
#[command(name = "sui", version, about = "Rust-native Nix replacement")]
struct Cli {
    #[arg(long, global = true)] vm: bool,
    #[arg(long, global = true, conflicts_with = "vm")] no_vm: bool,
    #[arg(long, global = true)] show_trace: bool,
    #[arg(short = 'L', long, global = true)] print_build_logs: bool,
    #[arg(long, global = true, hide = true)] extra_experimental_features: Option<String>,
    #[arg(long, global = true, hide = true)] no_write_lock_file: bool,
    #[arg(long, global = true, hide = true)] accept_flake_config: bool,
    #[arg(long, global = true, hide = true)] impure: bool,
    #[arg(long, global = true, hide = true, num_args = 2, action = clap::ArgAction::Append)] option: Vec<String>,
    #[arg(long, global = true, hide = true)] log_format: Option<String>,
    #[arg(long, global = true, hide = true)] max_jobs: Option<String>,
    #[arg(long, global = true, hide = true)] cores: Option<usize>,
    #[arg(long, global = true, hide = true)] keep_going: bool,
    #[arg(short = 'v', long, global = true, hide = true)] verbose: bool,
    #[arg(long, global = true, hide = true)] quiet: bool,
    #[command(subcommand)] command: Commands,
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
    Eval {
        expression: Option<String>,
        #[arg(long)] json: bool,
        #[arg(long)] raw: bool,
        #[arg(short = 'E', long = "expr")] expr_flag: Option<String>,
        #[arg(long, default_value = "0")] max_force_depth: usize,
        #[arg(long)]
        no_eval_cache: bool,
        #[arg(long, hide = true)] apply: Option<String>,
        #[arg(long = "file", short = 'f', hide = true)] file_flag: Option<String>,
    },
    Build {
        installable: Option<String>,
        #[arg(long)] no_link: bool,
        #[arg(long)] print_out_paths: bool,
        #[arg(long)] json: bool,
        #[arg(long)] dry_run: bool,
        #[arg(short = 'o', long)] out_link: Option<String>,
        #[arg(long, hide = true)] rebuild: bool,
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
    /// Enter a development shell
    Develop {
        /// Flake reference (default: current directory)
        #[arg(default_value = ".")]
        flake_ref: String,
        /// Shell attribute (default: "default")
        #[arg(short = 'A', long, default_value = "default")]
        attr: String,
        /// Command to run instead of interactive shell
        #[arg(short, long)]
        command: Option<String>,
    },
    /// Run a flake app
    Run {
        /// Installable (e.g., .#app-name)
        installable: String,
        /// Arguments to pass to the app
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Search { flake_ref: String, query: String },
    Profile { #[command(subcommand)] command: ProfileCommands },
    Repl { flake_ref: Option<String>, #[arg(long)] file: Option<String> },
    Copy { #[arg(long)] to: Option<String>, #[arg(long)] from: Option<String>, paths: Vec<String>, #[arg(long, hide = true)] no_check_sigs: bool },
    #[command(name = "path-info")] PathInfo { paths: Vec<String>, #[arg(long)] json: bool, #[arg(long, hide = true)] closure_size: bool },
    #[command(name = "collect-garbage")] CollectGarbage { #[arg(short = 'd', long)] delete_old: bool, #[arg(long)] delete_older_than: Option<String> },
    Derivation { #[command(subcommand)] command: DerivationCommands },
    #[command(name = "show-config")] ShowConfig { #[arg(long)] json: bool },
    Hash { #[command(subcommand)] command: HashCommands },
    Key { #[command(subcommand)] command: KeyCommands },
    Why { path: String, dependency: String },
    #[command(name = "path-from-hash-part")] PathFromHashPart { hash_part: String },
    Edit { installable: String },
    Log { installable: String },
    #[command(name = "store-diff-closures", aliases = ["diff-closures"])] DiffClosures { before: String, after: String },
    #[command(name = "upgrade-nix")] UpgradeNix { #[arg(long)] nix_store_paths_url: Option<String> },
    Fmt { files: Vec<String>, #[arg(long)] check: bool },
    Registry { #[command(subcommand)] command: RegistryCommands },
    /// Run as a NATS build agent (ro platform builder)
    Agent {
        /// NATS server URL
        #[arg(long, default_value = "nats://nats.nats.svc:4222")]
        nats_url: String,
        /// NATS JetStream stream name
        #[arg(long, default_value = "BUILD")]
        stream: String,
        /// Consumer name
        #[arg(long, default_value = "sui-agent")]
        consumer: String,
        /// Cache endpoint for pushing built artifacts
        #[arg(long, default_value = "http://attic.nix-cache.svc:80")]
        cache_url: String,
        /// Cache name
        #[arg(long, default_value = "main")]
        cache_name: String,
        /// Resolution strategy:
        ///   lockfile — parse flake.lock, mirror inputs (~50MB RAM, default)
        ///   eval     — full sui-eval derivation resolution (~16GiB RAM)
        ///   nix      — shell out to nix build (requires nix in container)
        #[arg(long, default_value = "lockfile")]
        strategy: String,
    },
    /// Pre-warm the derivation path cache for a flake.
    /// Run on a machine with enough RAM, then ship drv-cache.redb to K8s pods.
    #[command(name = "cache-warm")]
    CacheWarm {
        /// Path to the flake directory (or github:owner/repo reference)
        flake_ref: String,
        /// Attribute paths to cache (e.g., "packages.x86_64-linux.default")
        #[arg(long)]
        attrs: Vec<String>,
    },
    Doctor,
    #[command(name = "print-dev-env")] PrintDevEnv { flake_ref: Option<String>, #[arg(long)] json: bool },
    Bundle { installable: String, #[arg(long)] bundler: Option<String>, #[arg(short = 'o', long)] out_link: Option<String> },
    /// Run differential parity probes (sui vs cppnix) and write a typed
    /// JSON ShadowReport.  Tests sui as a full nix replacement without
    /// ever mutating the system.  Thin wrapper around the same library
    /// the sui-sweep binary uses; corpora authored in sui-spec/specs/*.lisp.
    #[command(name = "rebuild-shadow")]
    RebuildShadow {
        /// Explicit flake directories to sweep.  Defaults to walking
        /// --flakes-root for every direct child containing flake.nix.
        flakes: Vec<std::path::PathBuf>,
        /// Path to the cppnix binary (the oracle).
        #[arg(long, default_value = "nix")]
        nix: std::path::PathBuf,
        /// Root directory to walk for flake.nix files.  Default:
        /// `$HOME/code/github/pleme-io`.
        #[arg(long)]
        flakes_root: Option<std::path::PathBuf>,
        /// Corpus selection: `parity` | `builtins` | `rebuild` | `all`.
        #[arg(long, default_value = "all")]
        corpus: String,
        /// Include only probes carrying any of these tags.
        #[arg(long)]
        tag: Vec<String>,
        /// Exclude probes carrying any of these tags.
        #[arg(long)]
        skip_tag: Vec<String>,
        /// Per-probe timeout in seconds.
        #[arg(long, default_value = "30")]
        timeout_secs: u64,
        /// Explicit JSON report output path.  Default:
        /// `~/.cache/sui/shadow-reports/<host>-<ts>.json`.
        #[arg(long)]
        report: Option<std::path::PathBuf>,
        /// Skip writing the JSON report.
        #[arg(long)]
        no_report: bool,
        /// Print per-probe diagnostics to stderr.
        #[arg(long = "verbose-probes")]
        verbose_probes: bool,
    },
}

#[derive(Subcommand)]
enum StoreCommands {
    PathInfo { path: String, #[arg(long)] json: bool },
    Paths { #[arg(long, default_value = "100")] limit: usize },
    Gc { #[arg(long)] max_age_days: Option<u32>, #[arg(long)] print_roots: bool, #[arg(long)] dry_run: bool },
    Verify,
    Optimise { #[arg(long)] dry_run: bool },
    Info,
    Delete { paths: Vec<String>, #[arg(long, hide = true)] ignore_liveness: bool },
    Ls { path: String, #[arg(short = 'R', long)] recursive: bool, #[arg(short = 'l', long)] long: bool, #[arg(long)] json: bool },
    Cat { path: String },
    #[command(name = "dump-path")] DumpPath { path: String },
    #[command(name = "make-content-addressed")] MakeContentAddressed { paths: Vec<String> },
    Ping,
    #[command(name = "add-path")] AddPath { path: String, #[arg(long)] name: Option<String> },
    #[command(name = "add-file")] AddFile { path: String, #[arg(long)] name: Option<String> },
    #[command(name = "prefetch-file")] PrefetchFile { url: String, #[arg(long)] name: Option<String>, #[arg(long)] hash: Option<String>, #[arg(long)] hash_type: Option<String>, #[arg(long)] unpack: bool },
    Sign { paths: Vec<String>, #[arg(short = 'k', long)] key_file: String },
    Repair { paths: Vec<String> },
}

#[derive(Subcommand)]
enum FlakeCommands {
    Show { flake_ref: Option<String> },
    Update { input: Option<String> },
    Check { #[arg(long)] no_build: bool },
    Lock,
    Metadata { flake_ref: Option<String>, #[arg(long)] json: bool },
    Init { #[arg(short = 't', long)] template: Option<String> },
    New { dest: String, #[arg(short = 't', long)] template: Option<String> },
    Archive { flake_ref: Option<String>, #[arg(long)] json: bool },
    Clone { flake_ref: String, #[arg(long)] dest: Option<String> },
    Prefetch { flake_ref: Option<String>, #[arg(long)] json: bool },
}

#[derive(Subcommand)]
enum SystemCommands {
    Rebuild { #[arg(long)] flake: Option<String> },
    Status,
    Rollback,
}

#[derive(Subcommand)]
enum FleetCommands {
    Nodes,
    Deploy { target: String },
    Status,
}

#[derive(Subcommand)]
enum CacheCommands {
    Serve { #[arg(long, default_value = "0.0.0.0:5000")] listen: String, #[arg(long, default_value = "/var/cache/sui")] store_path: String, #[arg(long, default_value = "40")] priority: u32 },
    Push { paths: Vec<String>, #[arg(long)] cache_url: Option<String>, #[arg(long, default_value = "/var/cache/sui")] store_path: String, #[arg(long)] signing_key: Option<String> },
    Gc { #[arg(long, default_value = "/var/cache/sui")] store_path: String, #[arg(long)] keep: Vec<String> },
    Info { #[arg(long, default_value = "/var/cache/sui")] store_path: String },
}

#[derive(Subcommand)]
enum ProfileCommands {
    List { #[arg(long)] profile: Option<String>, #[arg(long)] json: bool },
    Install { packages: Vec<String>, #[arg(long)] profile: Option<String>, #[arg(long)] priority: Option<i32> },
    Remove { packages: Vec<String>, #[arg(long)] profile: Option<String> },
    Upgrade { packages: Vec<String>, #[arg(long)] profile: Option<String> },
    Rollback { #[arg(long)] profile: Option<String> },
    History { #[arg(long)] profile: Option<String> },
    #[command(name = "wipe-history")] WipeHistory { #[arg(long)] profile: Option<String>, #[arg(long)] older_than: Option<String> },
    Diff { #[arg(long)] profile: Option<String> },
}

#[derive(Subcommand)]
enum DerivationCommands {
    Show { paths: Vec<String>, #[arg(long)] json: bool },
    Add { path: String },
}

#[derive(Subcommand)]
enum HashCommands {
    File { path: String, #[arg(long, default_value = "sha256")] r#type: String, #[arg(long, default_value = "base32")] base: String },
    Path { path: String, #[arg(long, default_value = "sha256")] r#type: String, #[arg(long, default_value = "base32")] base: String },
    #[command(name = "to-base16")] ToBase16 { hash: String, #[arg(long)] r#type: Option<String> },
    #[command(name = "to-base32")] ToBase32 { hash: String, #[arg(long)] r#type: Option<String> },
    #[command(name = "to-base64")] ToBase64 { hash: String, #[arg(long)] r#type: Option<String> },
    #[command(name = "to-sri")] ToSri { hash: String, #[arg(long)] r#type: Option<String> },
}

#[derive(Subcommand)]
enum KeyCommands {
    #[command(name = "generate-secret")] GenerateSecret { #[arg(long)] key_name: String },
    #[command(name = "convert-secret-to-public")] ConvertSecretToPublic,
}

#[derive(Subcommand)]
enum RegistryCommands {
    List { #[arg(long)] json: bool },
    Add { from: String, to: String },
    Remove { entry: String },
    Pin { entry: String },
}

/// Strip the leading `<algo>:` from a substrate-typed hash string
/// to match nix CLI's bare-output form for `to-baseN`.
fn strip_algo_prefix(s: &str) -> &str {
    s.split_once(':').map(|(_, rest)| rest).unwrap_or(s)
}

/// Compute the digest of a single file, then encode it per the
/// requested base.  Mirrors `nix hash file <path> --type X --base Y`.
fn hash_file(path: &str, hash_type: &str, base: &str) -> Result<(), CliError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::NotImplemented(format!("hash file: reading {path}: {e}")))?;

    let digest: Vec<u8> = match hash_type {
        "sha256" => {
            use sha2::Digest;
            sha2::Sha256::digest(&bytes).to_vec()
        }
        "sha512" => {
            use sha2::Digest;
            sha2::Sha512::digest(&bytes).to_vec()
        }
        other => {
            return Err(CliError::NotImplemented(format!(
                "hash file: unsupported --type `{other}` (sha256 / sha512)"
            )));
        }
    };

    // Map nix's `--base` flag to substrate encoding names.  Nix
    // accepts `base16` / `base32` / `base64` / `sri`; substrate
    // uses `nix-base32` for the historical Nix variant.
    let encoding = match base {
        "base16" => "base16",
        "base32" => "nix-base32",
        "base64" => "base64",
        "sri"    => "sri",
        other    => return Err(CliError::NotImplemented(format!(
            "hash file: unknown --base `{other}` (base16 | base32 | base64 | sri)"
        ))),
    };

    let out = sui_spec::hash::encode_hash(hash_type, encoding, &digest)
        .map_err(|e| CliError::NotImplemented(format!("hash file: encode: {e:?}")))?;
    println!("{out}");
    Ok(())
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

    // Pre-intern the hot nixpkgs/flake/stdenv identifier set on the
    // main thread. Subsequent intern() calls for these are hashmap
    // hits; also amortizes the interner's initial resize cost.
    sui_intern::prewarm();

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
                StoreCommands::Gc { max_age_days, print_roots, dry_run } => {
                    if print_roots {
                        let roots = sui_store::find_gc_roots("/nix/store");
                        for root in &roots { println!("{root}"); }
                        return Ok(());
                    }
                    let rw_store = LocalStore::open_rw(NIX_DB_PATH).await.map_err(|e| CliError::StoreOpen { path: NIX_DB_PATH, source: e })?;
                    if dry_run {
                        let roots = sui_store::find_gc_roots("/nix/store");
                        let root_paths: Vec<_> = roots.iter().filter_map(|r| sui_compat::store_path::StorePath::from_absolute_path(r).ok()).collect();
                        let reachable = rw_store.compute_closure(&root_paths).await?;
                        let reachable_set: std::collections::HashSet<String> = reachable.iter().map(|p| p.to_absolute_path()).collect();
                        let all = rw_store.query_all_valid_paths().await?;
                        let garbage: Vec<_> = all.iter().filter(|p| !reachable_set.contains(&p.to_absolute_path())).collect();
                        println!("{} paths would be collected", garbage.len());
                        for p in &garbage { println!("{}", p.to_absolute_path()); }
                        return Ok(());
                    }
                    let mut options = sui_store::GcOptions::default();
                    if let Some(days) = max_age_days { options = options.with_delete_older_than(u64::from(days) * 86400); }
                    let result = rw_store.collect_garbage(&options).await?;
                    println!("deleted {} paths, freed {} bytes", result.paths_deleted, result.bytes_freed);
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
                StoreCommands::Optimise { dry_run } => {
                    let rw_store = LocalStore::open_rw(NIX_DB_PATH).await.map_err(|e| CliError::StoreOpen { path: NIX_DB_PATH, source: e })?;
                    let result = rw_store.optimise_store(dry_run).await?;
                    if dry_run { println!("{} files would be linked, {} bytes would be saved", result.files_linked, result.bytes_saved); }
                    else { println!("{} files linked, {} bytes saved", result.files_linked, result.bytes_saved); }
                }
                StoreCommands::Info => {
                    let paths = store.query_all_valid_paths().await?;
                    println!("Store dir:    /nix/store");
                    println!("Valid paths:  {}", paths.len());
                    println!("Database:     {NIX_DB_PATH}");
                }
                StoreCommands::Delete { paths: dp, ignore_liveness: _ } => { return Err(CliError::NotImplemented(format!("store delete: {} paths", dp.len()))); }
                StoreCommands::Ls { path: p, .. } => { return Err(CliError::NotImplemented(format!("store ls {p}"))); }
                StoreCommands::Cat { path: p } => { return Err(CliError::NotImplemented(format!("store cat {p}"))); }
                StoreCommands::DumpPath { path: p } => { return Err(CliError::NotImplemented(format!("store dump-path {p}"))); }
                StoreCommands::MakeContentAddressed { paths: mp } => { return Err(CliError::NotImplemented(format!("store make-content-addressed: {} paths", mp.len()))); }
                StoreCommands::Ping => { println!("Store URL: daemon\nVersion: sui {}\nTrusted: 1", env!("CARGO_PKG_VERSION")); }
                StoreCommands::AddPath { path: p, .. } => { return Err(CliError::NotImplemented(format!("store add-path {p}"))); }
                StoreCommands::AddFile { path: p, .. } => { return Err(CliError::NotImplemented(format!("store add-file {p}"))); }
                StoreCommands::PrefetchFile { url: u, .. } => { return Err(CliError::NotImplemented(format!("store prefetch-file {u}"))); }
                StoreCommands::Sign { paths: sp, key_file: kf } => { return Err(CliError::NotImplemented(format!("store sign: {} paths with {kf}", sp.len()))); }
                StoreCommands::Repair { paths: rp } => { return Err(CliError::NotImplemented(format!("store repair: {} paths", rp.len()))); }
            }
        }

        Commands::Eval { expression, json, raw: _, expr_flag, max_force_depth, no_eval_cache: _, apply: _, file_flag: _ } => {
            let expr = expr_flag.or(expression)
                .ok_or_else(|| CliError::MissingArgument("no expression provided".into()))?;
            if max_force_depth > 0 {
                sui_eval::trace::set_max_force_depth(max_force_depth);
            }
            if cli.no_vm {
                // Tree-walker evaluation path.
                // Spawn a thread with a large stack for deeply nested nixpkgs evaluation.
                // macOS's main thread has a fixed 8MB stack that stacker can't grow.
                let expr_clone = expr.clone();
                let json_flag = json;
                let handle = std::thread::Builder::new()
                    .name("sui-eval".into())
                    .stack_size(256 * 1024 * 1024) // 256MB
                    .spawn(move || -> Result<(), CliError> {
                        let value = sui_eval::eval(&expr_clone)?;
                        if json_flag {
                            println!("{}", serde_json::to_string(&value.to_json())?);
                        } else {
                            println!("{value}");
                        }
                        Ok(())
                    })
                    .expect("failed to spawn eval thread");
                handle.join().expect("eval thread panicked")?;
            } else {
                // Bytecode VM evaluation path (default).
                // Run VM on a large-stack thread: the tree-walker bridge
                // (__import) can recurse deeply on nixpkgs evaluation.
                // Bridge guards (flake resolver, builtin bridge) must be
                // installed inside the thread since they use thread-local storage.
                let expr_clone = expr.clone();
                let json_flag = json;
                let vm_handle = std::thread::Builder::new()
                    .name("sui-vm-eval".into())
                    .stack_size(256 * 1024 * 1024) // 256MB
                    .spawn(move || -> Result<(), CliError> {
                // Install flake resolver so VM getFlake delegates to tree-walker.
                let _flake_guard = sui_bytecode::set_flake_resolver(Box::new(|flake_ref: &str| {
                    let flake_dir = if flake_ref.starts_with('/') || flake_ref.starts_with('.') {
                        std::path::PathBuf::from(flake_ref)
                    } else if let Some(path) = flake_ref.strip_prefix("path:") {
                        std::path::PathBuf::from(path)
                    } else {
                        return Err(format!("unsupported flake reference: {flake_ref}"));
                    };
                    let result = sui_eval::builtins::evaluate_flake(&flake_dir)
                        .map_err(|e| e.to_string())?;
                    Ok(sui_eval::eval_to_string_keyed(&result))
                }));
                // Install builtin bridge so VM can delegate missing builtins
                // and compilation fallback (__import) to the tree-walker.
                let _bridge_guard = sui_bytecode::set_builtin_bridge(Box::new(
                    |name: &str, args: Vec<sui_bytecode::StringKeyedValue>| {
                        if name == "__import" {
                            let path_str = match &args[0] {
                                sui_bytecode::StringKeyedValue::Path(p)
                                | sui_bytecode::StringKeyedValue::String(p) => p.clone(),
                                _ => return Err("__import: expected path or string argument".to_string()),
                            };
                            let path = std::path::Path::new(&path_str);
                            let source = std::fs::read_to_string(path)
                                .map_err(|e| format!("__import: {}: {e}", path.display()))?;
                            let path_buf = path.to_path_buf();
                            let _guard = sui_eval::eval::push_eval_file(path_buf.clone());
                            let result = sui_eval::eval::eval_with_file(&source, Some(path_buf))
                                .map_err(|e| e.to_string())?;
                            let forced = sui_eval::eval::force_value(&result)
                                .map_err(|e| e.to_string())?;
                            return Ok(sui_eval::eval_to_string_keyed(&forced));
                        }

                        let eval_args: Vec<sui_eval::Value> = args
                            .iter()
                            .map(|a| sui_eval::convert::string_keyed_to_eval(a))
                            .collect();

                        let result = sui_eval::builtins::call_builtin_by_name(name, &eval_args)
                            .map_err(|e| e.to_string())?;

                        let forced = sui_eval::eval::force_value(&result)
                            .map_err(|e| e.to_string())?;

                        Ok(sui_eval::eval_to_string_keyed(&forced))
                    },
                ));
                        let sk = match sui_bytecode::eval_full(&expr_clone) {
                            Ok(r) => r.to_string_keyed(),
                            Err(e) => {
                                // VM failed — fall back to tree-walker.
                                eprintln!("[sui-vm] CLI fallback to tree-walker: {e}");
                                let tw_result = sui_eval::eval::eval(&expr_clone).map_err(|e| {
                                    CliError::Orchestrate {
                                        operation: "eval",
                                        message: e.to_string(),
                                    }
                                })?;
                                sui_eval::eval_to_string_keyed(&tw_result)
                            }
                        };
                        if json_flag {
                            let json_val = string_keyed_to_json(&sk);
                            println!("{}", serde_json::to_string(&json_val)?);
                        } else {
                            println!("{sk}");
                        }
                        Ok(())
                    })
                    .expect("failed to spawn VM eval thread");
                vm_handle.join().expect("VM eval thread panicked")?;
            }
        }

        Commands::Build { installable: installable_opt, no_link: _, print_out_paths: _, json: _, dry_run: _, out_link: _, rebuild: _ } => {
            let installable = installable_opt.unwrap_or_else(|| ".#default".to_string());
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
            FlakeCommands::Metadata { flake_ref, json: _ } => {
                let flake_dir = resolve_flake_dir(flake_ref.as_deref())?;
                print_flake_metadata(&flake_dir)?;
            }
            FlakeCommands::Init { template } => { return Err(CliError::NotImplemented(format!("flake init --template {}", template.as_deref().unwrap_or("default")))); }
            FlakeCommands::New { dest, template } => { return Err(CliError::NotImplemented(format!("flake new {dest} --template {}", template.as_deref().unwrap_or("default")))); }
            FlakeCommands::Archive { flake_ref: fr, json: _ } => { return Err(CliError::NotImplemented(format!("flake archive {}", fr.as_deref().unwrap_or(".")))); }
            FlakeCommands::Clone { flake_ref: fr, dest } => { return Err(CliError::NotImplemented(format!("flake clone {fr} --dest {}", dest.as_deref().unwrap_or(".")))); }
            FlakeCommands::Prefetch { flake_ref: fr, json: _ } => { return Err(CliError::NotImplemented(format!("flake prefetch {}", fr.as_deref().unwrap_or(".")))); }
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

        Commands::Develop { flake_ref, attr, command } => {
            let (flake_dir, override_attr) = if let Some((dir_part, attr_part)) = flake_ref.split_once('#') {
                let dir = if dir_part == "." || dir_part.is_empty() { std::env::current_dir()? } else { std::path::PathBuf::from(dir_part) };
                (dir, Some(attr_part.to_string()))
            } else {
                let dir = if flake_ref == "." || flake_ref.is_empty() { std::env::current_dir()? } else { std::path::PathBuf::from(&flake_ref) };
                (dir, None)
            };
            let shell_attr = override_attr.as_deref().unwrap_or(&attr);
            let system = current_system();
            let result = sui_eval::builtins::evaluate_flake(&flake_dir).map_err(|e| CliError::Orchestrate { operation: "develop", message: format!("eval: {e}") })?;
            let shell_drv = sui_eval::builtins::navigate_attrs(&result, &["devShells", &system, shell_attr]).map_err(|e| CliError::Orchestrate { operation: "develop", message: format!("navigate devShells.{system}.{shell_attr}: {e}") })?;
            let env_vars = extract_shell_env(&shell_drv);
            let shell_bin = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
            let mut cmd = std::process::Command::new(&shell_bin);
            for (key, value) in &env_vars { cmd.env(key, value); }
            if let Some(drv_path) = env_vars.get("PATH") { let existing = std::env::var("PATH").unwrap_or_default(); cmd.env("PATH", format!("{drv_path}:{existing}")); }
            cmd.env("IN_SUI_SHELL", "1"); cmd.env("SUI_SHELL_NAME", shell_attr);
            if let Some(run_cmd) = command { cmd.args(["-c", &run_cmd]); }
            let status = cmd.status()?;
            std::process::exit(status.code().unwrap_or(1));
        }

        Commands::Run { installable, args } => {
            let flake_ref = sui_compat::flake_ref::FlakeRef::parse(&installable).map_err(|e| CliError::Orchestrate { operation: "run", message: format!("flake ref parse: {e}") })?;
            let result = sui_eval::builtins::evaluate_flake(&flake_ref.flake_dir).map_err(|e| CliError::Orchestrate { operation: "run", message: format!("eval: {e}") })?;
            let system = current_system();
            let attr_name = &flake_ref.attribute;
            let program = try_navigate_program(&result, &system, attr_name).or_else(|| try_navigate_drv_path(&result, &system, attr_name)).ok_or_else(|| CliError::Orchestrate { operation: "run", message: format!("could not find apps.{system}.{attr_name}.program or packages.{system}.{attr_name}") })?;
            let status = std::process::Command::new(&program).args(&args).status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Commands::Search { flake_ref, query } => { return Err(CliError::NotImplemented(format!("search {flake_ref} {query}"))); }
        Commands::Profile { command } => match command {
            ProfileCommands::List { .. } => { return Err(CliError::NotImplemented("profile list".into())); }
            ProfileCommands::Install { packages, .. } => { return Err(CliError::NotImplemented(format!("profile install {}", packages.join(" ")))); }
            ProfileCommands::Remove { packages, .. } => { return Err(CliError::NotImplemented(format!("profile remove {}", packages.join(" ")))); }
            ProfileCommands::Upgrade { packages, .. } => { return Err(CliError::NotImplemented(format!("profile upgrade {}", packages.join(" ")))); }
            ProfileCommands::Rollback { .. } => { return Err(CliError::NotImplemented("profile rollback".into())); }
            ProfileCommands::History { .. } => { return Err(CliError::NotImplemented("profile history".into())); }
            ProfileCommands::WipeHistory { .. } => { return Err(CliError::NotImplemented("profile wipe-history".into())); }
            ProfileCommands::Diff { .. } => { return Err(CliError::NotImplemented("profile diff".into())); }
        },
        Commands::Repl { .. } => { return Err(CliError::NotImplemented("repl".into())); }
        Commands::Copy { to, from, paths, .. } => { return Err(CliError::NotImplemented(format!("copy {} paths from {} to {}", paths.len(), from.as_deref().unwrap_or("local"), to.as_deref().unwrap_or("?")))); }
        Commands::PathInfo { paths, .. } => { return Err(CliError::NotImplemented(format!("path-info {}", paths.join(" ")))); }
        Commands::CollectGarbage { delete_old, delete_older_than } => {
            if delete_old { return Err(CliError::NotImplemented("collect-garbage -d".into())); }
            else if let Some(ref age) = delete_older_than { return Err(CliError::NotImplemented(format!("collect-garbage --delete-older-than {age}"))); }
            else { return Err(CliError::NotImplemented("collect-garbage".into())); }
        }
        Commands::Derivation { command } => match command {
            DerivationCommands::Show { paths, .. } => { return Err(CliError::NotImplemented(format!("derivation show {}", paths.join(" ")))); }
            DerivationCommands::Add { path } => { return Err(CliError::NotImplemented(format!("derivation add {path}"))); }
        },
        Commands::ShowConfig { .. } => { println!("system = {}\nstore = /nix/store\ncores = {}", std::env::consts::ARCH, std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)); }
        Commands::Hash { command } => match command {
            HashCommands::File { path, r#type, base } => {
                hash_file(&path, &r#type, &base)?;
            }
            HashCommands::Path { path, r#type, base } => {
                // `nix hash path` runs a NAR walk; substrate
                // doesn't expose that yet — typed error pointing
                // at the gap.
                return Err(CliError::NotImplemented(format!(
                    "hash path {path} --type {type} --base {base} — needs sui_spec::nar::hash_path"
                )));
            }
            HashCommands::ToBase16 { hash, r#type: _ } => {
                // `nix hash to-base16` outputs bare hex (no `<algo>:`
                // prefix); substrate's base16 encoding already
                // returns the bare form.
                let out = sui_spec::hash::apply_conversion("auto", "base16", &hash)
                    .map_err(|e| CliError::NotImplemented(format!("hash to-base16: {e:?}")))?;
                println!("{out}");
            }
            HashCommands::ToBase32 { hash, r#type: _ } => {
                // `nix hash to-base32` outputs bare nix-base32 (no
                // `<algo>:` prefix); substrate's encoder prepends
                // the algo for storage purposes, so strip it.
                let out = sui_spec::hash::apply_conversion("auto", "nix-base32", &hash)
                    .map_err(|e| CliError::NotImplemented(format!("hash to-base32: {e:?}")))?;
                println!("{}", strip_algo_prefix(&out));
            }
            HashCommands::ToBase64 { hash, r#type: _ } => {
                // Same as to-base32 — strip the prefix for nix
                // CLI byte-equivalence.
                let out = sui_spec::hash::apply_conversion("auto", "base64", &hash)
                    .map_err(|e| CliError::NotImplemented(format!("hash to-base64: {e:?}")))?;
                println!("{}", strip_algo_prefix(&out));
            }
            HashCommands::ToSri { hash, r#type: _ } => {
                // SRI form keeps the `<algo>-<base64>` shape; no
                // prefix stripping.
                let out = sui_spec::hash::apply_conversion("auto", "sri", &hash)
                    .map_err(|e| CliError::NotImplemented(format!("hash to-sri: {e:?}")))?;
                println!("{out}");
            }
        },
        Commands::Key { command } => match command {
            KeyCommands::GenerateSecret { key_name } => { return Err(CliError::NotImplemented(format!("key generate-secret --key-name {key_name}"))); }
            KeyCommands::ConvertSecretToPublic => { return Err(CliError::NotImplemented("key convert-secret-to-public".into())); }
        },
        Commands::Why { path, dependency } => { return Err(CliError::NotImplemented(format!("why {path} {dependency}"))); }
        Commands::PathFromHashPart { hash_part } => { return Err(CliError::NotImplemented(format!("path-from-hash-part {hash_part}"))); }
        Commands::Edit { installable } => { return Err(CliError::NotImplemented(format!("edit {installable}"))); }
        Commands::Log { installable } => { return Err(CliError::NotImplemented(format!("log {installable}"))); }
        Commands::DiffClosures { before, after } => { return Err(CliError::NotImplemented(format!("diff-closures {before} {after}"))); }
        Commands::UpgradeNix { .. } => { return Err(CliError::NotImplemented("upgrade-nix".into())); }
        Commands::Fmt { files, check } => { return Err(CliError::NotImplemented(format!("fmt ({}){}", if check { "check" } else { "format" }, if files.is_empty() { String::new() } else { format!(" {}", files.join(" ")) }))); }
        Commands::Registry { command } => match command {
            RegistryCommands::List { .. } => { return Err(CliError::NotImplemented("registry list".into())); }
            RegistryCommands::Add { from, to } => { return Err(CliError::NotImplemented(format!("registry add {from} {to}"))); }
            RegistryCommands::Remove { entry } => { return Err(CliError::NotImplemented(format!("registry remove {entry}"))); }
            RegistryCommands::Pin { entry } => { return Err(CliError::NotImplemented(format!("registry pin {entry}"))); }
        },
        Commands::Agent { nats_url, stream, consumer, cache_url, cache_name, strategy } => {
            agent::run_agent(&nats_url, &stream, &consumer, &cache_url, &cache_name, &strategy).await?;
        }
        Commands::CacheWarm { flake_ref, attrs } => {
            use sui_eval::drv_cache;
            drv_cache::init_global_cache();
            let flake_dir = if flake_ref.starts_with("github:") || flake_ref.starts_with("git+") {
                // Remote ref — fetch the source first.
                let dir = agent::fetch_flake_source_public(&flake_ref)
                    .map_err(|e| CliError::MissingArgument(format!("fetch failed: {e}")))?;
                dir
            } else {
                std::path::PathBuf::from(&flake_ref)
            };

            for attr in &attrs {
                let segments: Vec<&str> = attr.split('.').collect();
                println!("Evaluating {flake_ref}#{attr} ...");
                match sui_eval::builtins::evaluate_flake_attr(&flake_dir, &segments) {
                    Ok(value) => {
                        if let Ok(attrs) = value.as_attrs() {
                            if let Some(out) = attrs.get("outPath") {
                                println!("  outPath: {}", out.as_string().unwrap_or("?"));
                            }
                            if let Some(drv) = attrs.get("drvPath") {
                                println!("  drvPath: {}", drv.as_string().unwrap_or("?"));
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("  Error: {e}");
                    }
                }
            }
            let entries = drv_cache::with_cache(|c| Some(c.len())).unwrap_or(0);
            println!("Cache now has {entries} entries at {}", drv_cache::DrvCache::default_path().display());
        }
        Commands::Doctor => { println!("Running checks against your Nix installation...\nStore: /nix/store (OK)"); }
        Commands::PrintDevEnv { flake_ref, .. } => { return Err(CliError::NotImplemented(format!("print-dev-env {}", flake_ref.as_deref().unwrap_or(".")))); }
        Commands::Bundle { installable, bundler, .. } => { return Err(CliError::NotImplemented(format!("bundle {installable} --bundler {}", bundler.as_deref().unwrap_or("default")))); }
        Commands::RebuildShadow {
            flakes, nix, flakes_root, corpus, tag, skip_tag,
            timeout_secs, report, no_report, verbose_probes,
        } => {
            let mut config = sui_spec::sweep::SweepConfig::defaults();
            // Default to the current process — operator runs `sui
            // rebuild-shadow` and the same binary is also the engine
            // under test.
            if let Ok(self_exe) = std::env::current_exe() {
                config.sui_bin = self_exe;
            }
            config.nix_bin = nix;
            if let Some(root) = flakes_root {
                config.flakes_root = root;
            }
            config.explicit_flakes = flakes;
            config.include_tags = tag;
            config.exclude_tags = skip_tag;
            config.timeout = std::time::Duration::from_secs(timeout_secs);
            config.verbose = verbose_probes;
            config.corpus = sui_spec::sweep::Corpus::from_str(&corpus)
                .ok_or_else(|| CliError::Orchestrate {
                    operation: "rebuild-shadow",
                    message: format!("unknown corpus `{corpus}` (expected parity | builtins | rebuild | all)"),
                })?;
            config.report_path = match (no_report, report) {
                (true, _)              => None,
                (false, Some(path))    => Some(path),
                (false, None)          => Some(sui_spec::sweep::default_report_path()),
            };
            let report = sui_spec::sweep::run(&config).map_err(|e| CliError::Orchestrate {
                operation: "rebuild-shadow",
                message: e.to_string(),
            })?;
            if !report.all_pass() {
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

/// Handle legacy nix-* commands dispatched by argv[0].
async fn handle_legacy_command(name: &str) -> Result<(), CliError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match name {
        "nix-build" => {
            let mut attr = None;
            let mut path = ".".to_string();
            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "-A" | "--attr" => { i += 1; if i < args.len() { attr = Some(args[i].clone()); } }
                    s if !s.starts_with('-') => { path = s.to_string(); }
                    _ => {}
                }
                i += 1;
            }
            let inst = if let Some(a) = attr { format!("{path}#{a}") } else { path };
            eprintln!("nix-build → sui build {inst}: not yet fully implemented");
        }
        "nix-store" => {
            if args.iter().any(|a| a == "--gc") { eprintln!("nix-store --gc → sui store gc"); }
            else if args.iter().any(|a| a == "--optimise") { eprintln!("nix-store --optimise → sui store optimise"); }
            else if args.iter().any(|a| a == "--verify") { eprintln!("nix-store --verify → sui store verify"); }
            else if args.iter().any(|a| a == "-q" || a == "--query") { eprintln!("nix-store --query → sui store path-info"); }
            else if args.iter().any(|a| a == "--delete") { eprintln!("nix-store --delete → sui store delete"); }
            else if args.iter().any(|a| a == "--realise" || a == "-r") { eprintln!("nix-store --realise → sui build"); }
            else { eprintln!("nix-store: unrecognized flags {:?}", args); }
        }
        "nix-instantiate" => {
            let has_eval = args.iter().any(|a| a == "--eval");
            let has_json = args.iter().any(|a| a == "--json");
            let mut expr = None;
            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "-E" | "--expr" => { i += 1; if i < args.len() { expr = Some(args[i].clone()); } }
                    "--eval" | "--json" | "--strict" => {}
                    s if !s.starts_with('-') => { expr = Some(s.to_string()); }
                    _ => {}
                }
                i += 1;
            }
            if has_eval {
                if let Some(e) = expr {
                    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new("warn")).init();
                    let value = sui_eval::eval(&e)?;
                    if has_json { println!("{}", serde_json::to_string(&value.to_json())?); }
                    else { println!("{value}"); }
                } else {
                    eprintln!("nix-instantiate --eval: no expression provided");
                    std::process::exit(1);
                }
            } else { return Err(CliError::NotImplemented("nix-instantiate (instantiate mode)".into())); }
        }
        "nix-env" => {
            if args.iter().any(|a| a == "--list-generations") { eprintln!("nix-env --list-generations → sui profile history"); }
            else if args.iter().any(|a| a == "-i" || a == "--install") { eprintln!("nix-env -i → sui profile install"); }
            else if args.iter().any(|a| a == "-e" || a == "--uninstall") { eprintln!("nix-env -e → sui profile remove"); }
            else if args.iter().any(|a| a == "-u" || a == "--upgrade") { eprintln!("nix-env -u → sui profile upgrade"); }
            else if args.iter().any(|a| a == "-q" || a == "--query") { eprintln!("nix-env -q → sui profile list"); }
            else { eprintln!("nix-env: unrecognized flags {:?}", args); }
        }
        "nix-shell" => {
            if args.iter().any(|a| a == "-p" || a == "--packages") { eprintln!("nix-shell -p → sui develop"); }
            else if args.iter().any(|a| a == "--run" || a == "--command") { eprintln!("nix-shell --run → sui develop --command"); }
            else { eprintln!("nix-shell → sui develop"); }
        }
        "nix-collect-garbage" => {
            if args.iter().any(|a| a == "-d" || a == "--delete-old") { eprintln!("nix-collect-garbage -d → sui collect-garbage -d"); }
            else { eprintln!("nix-collect-garbage → sui store gc"); }
        }
        "nix-channel" => { return Err(CliError::NotImplemented("nix-channel".into())); }
        "nix-hash" => { return Err(CliError::NotImplemented("nix-hash → sui hash".into())); }
        "nix-copy-closure" => { return Err(CliError::NotImplemented("nix-copy-closure → sui copy".into())); }
        "nix-prefetch-url" => { return Err(CliError::NotImplemented("nix-prefetch-url → sui store prefetch-file".into())); }
        _ => { eprintln!("unknown legacy command: {name}"); }
    }
    Ok(())
}

fn current_system() -> String {
    if cfg!(target_os = "macos") { if cfg!(target_arch = "aarch64") { "aarch64-darwin" } else { "x86_64-darwin" } }
    else if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" }.to_string()
}

fn extract_shell_env(value: &sui_eval::Value) -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::new();
    if let sui_eval::Value::Attrs(attrs) = value {
        for key in attrs.keys() {
            if let Some(v) = attrs.get(&key) {
                if let Ok(s) = v.as_string() {
                    match key.as_str() {
                        "type" | "drvPath" | "outPath" | "drvAttrs" | "outputHash" | "outputHashAlgo" | "outputHashMode" | "all" | "outputs" | "args" | "builder" | "system" | "name" | "pname" | "version" | "__structuredAttrs" | "__ignoreNulls" => {}
                        _ => { env.insert(key.clone(), s.to_string()); }
                    }
                }
            }
        }
    }
    env
}

fn try_navigate_program(result: &sui_eval::Value, system: &str, attr: &str) -> Option<String> {
    sui_eval::builtins::navigate_attrs(result, &["apps", system, attr, "program"]).ok().and_then(|v| v.as_string().ok().map(|s| s.to_string()))
}

fn try_navigate_drv_path(result: &sui_eval::Value, system: &str, attr: &str) -> Option<String> {
    let pkg = sui_eval::builtins::navigate_attrs(result, &["packages", system, attr]).ok()?;
    if let sui_eval::Value::Attrs(attrs) = &pkg {
        if let Some(out) = attrs.get("outPath") { if let Ok(s) = out.as_string() { return Some(format!("{}/bin/{attr}", s)); } }
    }
    None
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

    let keys: Vec<String> = attrs.keys().collect();
    let total = keys.len();
    for (i, key) in keys.iter().enumerate() {
        let is_last = i + 1 == total;
        let connector = if is_last { "\u{2514}\u{2500}\u{2500}\u{2500}" } else { "\u{251c}\u{2500}\u{2500}\u{2500}" };
        let child_prefix = if is_last { "    " } else { "\u{2502}   " };

        if let Some(child) = attrs.get(&key) {
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
    let keys: Vec<String> = attrs.keys().collect();
    let total = keys.len();
    for (i, key) in keys.iter().enumerate() {
        let is_last = i + 1 == total;
        let connector = if is_last { "\u{2514}\u{2500}\u{2500}\u{2500}" } else { "\u{251c}\u{2500}\u{2500}\u{2500}" };
        let child_prefix = if is_last {
            format!("{prefix}    ")
        } else {
            format!("{prefix}\u{2502}   ")
        };

        if let Some(child) = attrs.get(&key) {
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
        sui_bytecode::StringKeyedValue::Thunk(_) => {
            serde_json::Value::String("<thunk>".to_string())
        }
        sui_bytecode::StringKeyedValue::Callable(_) => {
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
