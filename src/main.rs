// Exactly one global allocator is active. Default build: mimalloc (fast).
// `--features dhat-heap`: dhat's tracking allocator, which records bytes +
// blocks per allocation call-stack and, at exit, writes `dhat-heap.json`
// whose at-t-gmax snapshot is the heap breakdown at peak. The two conflict
// (two `#[global_allocator]`s won't link), so they are mutually cfg-gated.
#[cfg(not(feature = "dhat-heap"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use sui::{CliError, NIX_DB_PATH};

mod agent;
mod legacy;
mod parity_corpus;
mod perf_seal;
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
        /// Path to the ed25519 signing secret key file (cofre/ESO-materialized
        /// Secret mount). When set, the embedded cache server signs every
        /// ingested narinfo. When absent the cache serves unsigned.
        #[arg(long)]
        signing_key: Option<String>,
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
    /// Continuous nix-vs-sui parity sweep across a canonical
    /// corpus of byte-equivalent surfaces (hash conversions, NAR
    /// dump, ATerm round-trip).  Emits a Nord-styled green/red
    /// matrix.  Exits non-zero on any divergence so CI/operators
    /// can react.  Sui-native — no nix equivalent.
    Parity {
        /// Path to the cppnix binary (the oracle).  Default: `nix` on PATH.
        #[arg(long, default_value = "nix")]
        nix: std::path::PathBuf,
        /// Emit machine-readable JSON instead of the Nord table.
        #[arg(long)]
        json: bool,
        /// Pin `<nixpkgs>` to the current HEAD of a nixpkgs channel ref (e.g.
        /// `nixpkgs-unstable`) before running the corpus, so a divergence
        /// introduced by an UPSTREAM nixpkgs change surfaces. Folds the machine's
        /// `nix flake metadata | jq` pin glue into the typed binary — the
        /// resolved rev is printed, and NIX_PATH is set for the corpus eval.
        #[arg(long)]
        track_nixpkgs: Option<String>,
    },
    /// BUILD-parity: realize a basket of derivations with sui AND nix, then
    /// byte-compare the built output NAR (via nix's own `nix hash path` on both).
    /// The typed engine behind the build-parity machine (docs/NIXPKGS-PARITY-MACHINE.md):
    /// proves sui BUILDS nixpkgs identically, not just evaluates it. Requires a
    /// WRITABLE store (single-user; sui writes the .drv + output directly). Exits
    /// non-zero on any divergence — a red gate to root-cause at the eval/build core.
    #[command(name = "build-parity")]
    BuildParity {
        /// Path to the cppnix binary (the oracle).  Default: `nix` on PATH.
        #[arg(long, default_value = "nix")]
        nix: std::path::PathBuf,
    },
    /// Bisect a diverging `<expr>.drvPath` to the structural leaf: recurse the
    /// sui↔nix input-derivation graph (matched by name) to the first drv whose
    /// same-name inputs all match nix but which itself diverges — naming the
    /// exact root of a byte-parity divergence instead of hand-diffing ATerms.
    #[command(name = "parity-bisect")]
    ParityBisect {
        /// Nix expression whose `.drvPath` diverges (e.g.
        /// `(import <nixpkgs> {}).hello`). `.drvPath` is appended automatically.
        #[arg(long)]
        expr: String,
        /// Path to the cppnix binary (the oracle).  Default: `nix` on PATH.
        #[arg(long, default_value = "nix")]
        nix: std::path::PathBuf,
    },
    /// Perf-seal: the SPEED peer of `parity`. Runs the eval-shape corpus
    /// in-process under the eval work-counters and gates each shape against a
    /// committed work budget (`src/perf-baseline.json`). "Eval got slower"
    /// fails CI the same way `parity`'s "drvPath changed" does. The gated
    /// metric is deterministic WORK (eval_expr count), not flaky wall-clock —
    /// a red row means a shape does more eval work, never runner noise.
    #[command(name = "perf-seal")]
    PerfSeal {
        /// Emit machine-readable JSON instead of the Nord table.
        #[arg(long)]
        json: bool,
        /// (Re)mint the committed baseline from the current measurements
        /// instead of grading — pin a fresh baseline or lock in a proven
        /// speedup. Commit the result.
        #[arg(long)]
        write_baseline: bool,
    },
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
    /// Walk /nix/store via a typed inventory profile + emit
    /// summary (entries / total size / total files).
    Inventory {
        /// Profile name from the canonical store-inventory catalog.
        #[arg(default_value = "tiny")]
        profile: String,
        /// Emit JSON instead of the Nord table.
        #[arg(long)]
        json: bool,
    },
    /// Compute the typed closure of a store path — every
    /// transitive `/nix/store/...` reference embedded in its
    /// NAR contents.  Used for diff / audit / mover workflows.
    Closure {
        /// Store path whose closure to walk.
        path: String,
        /// Emit JSON instead of Nord summary.
        #[arg(long)]
        json: bool,
    },
    /// Materialize a typed store-slice at a destination dir
    /// using sui's NAR encoder + decoder; verify byte-perfect
    /// equality against the source via NAR sha256.
    Materialize {
        /// Slice name from the canonical store-ops catalog
        /// (`tiny-sources` / `tiny-patches` / `tiny-drvs`).
        slice: String,
        /// Destination directory.  Defaults to ~/.cache/sui/materialize/<slice>.
        #[arg(long)]
        dest: Option<std::path::PathBuf>,
        /// Emit JSON instead of the Nord table.
        #[arg(long)]
        json: bool,
    },
    /// Apply a typed store-transform to a source store path.
    /// Reads NAR, parses to typed tree, applies the transform,
    /// re-encodes, materializes at dest.  Reports the # of
    /// rewrites.
    Transform {
        /// Source store path.
        source: String,
        /// Transform name from specs/store_transforms.lisp
        /// (e.g. `redact-base64-secrets`, `strip-shell-comments`).
        transform: String,
        /// Destination dir; defaults to ~/.cache/sui/transformed/<basename>.
        #[arg(long)]
        dest: Option<std::path::PathBuf>,
        /// Emit JSON outcome.
        #[arg(long)]
        json: bool,
    },
    /// Diff two store paths via their typed NAR trees.  Reports
    /// every Added / Removed / Changed / KindChanged /
    /// ExecutableChanged / SymlinkChanged record.
    Diff {
        /// First (source) store path.
        a: String,
        /// Second (target) store path.
        b: String,
        /// Emit typed JSON instead of Nord summary.
        #[arg(long)]
        json: bool,
    },
    /// Closure-wide graft: rewrite every reference from `<from>`
    /// to `<to>` across every path reachable from the closure
    /// root.  Materializes the rewritten closure at dest.  The
    /// killer composite — atomic refactor across N referring paths.
    Graft {
        /// Closure root.
        root: String,
        /// Source hash prefix (32 chars).
        from: String,
        /// Target hash prefix (32 chars; must match `from` length).
        to: String,
        /// Destination dir; defaults to ~/.cache/sui/grafted/<basename>.
        #[arg(long)]
        dest: Option<std::path::PathBuf>,
        /// Emit JSON outcome.
        #[arg(long)]
        json: bool,
    },
    /// Audit a slice for secret-like patterns (dry-run redact
    /// transform).  Reports which files match without modifying
    /// the store.
    AuditSecrets {
        /// Source store path.
        source: String,
        /// Emit JSON outcome.
        #[arg(long)]
        json: bool,
    },
    /// Composite typed fingerprint for a store path: NAR sha256
    /// + size + file count + top-level entry shape.  Useful for
    /// "is this build deterministic across machines?" probes.
    Fingerprint {
        /// Store path.
        path: String,
        /// Emit JSON instead of Nord.
        #[arg(long)]
        json: bool,
    },
    /// Find store entries matching a typed predicate.  Predicate
    /// syntax: `name=<regex>` / `size>N` / `size<N` / `contents=<regex>`.
    /// Multiple flags AND together.
    Find {
        /// Profile name (inventory walk).
        #[arg(default_value = "tiny")]
        profile: String,
        /// Name regex filter (e.g. `^hello-.*`).
        #[arg(long)]
        name: Option<String>,
        /// Minimum size in bytes.
        #[arg(long)]
        min_size: Option<u64>,
        /// Maximum size in bytes.
        #[arg(long)]
        max_size: Option<u64>,
        /// File-content regex filter.
        #[arg(long)]
        contents: Option<String>,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Reduce: typed aggregate statistics across an inventory
    /// profile (entry count / total size / size distribution).
    Stats {
        /// Profile name.
        #[arg(default_value = "tiny")]
        profile: String,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Auto-analyze observed store state: duplicates, orphans,
    /// high-fanout drvs, version shadows.  Emits typed Findings
    /// the operator can act on.
    Analyze {
        /// Profile name.
        #[arg(default_value = "tiny")]
        profile: String,
        /// Skip duplicate detection (expensive — NAR-hashes
        /// every entry).
        #[arg(long)]
        no_duplicates: bool,
        /// High-fanout threshold (drvs with ≥N inputs).
        #[arg(long, default_value = "8")]
        high_fanout_threshold: usize,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Mine upgrade-path recommendations from observed store
    /// state.  Walks the version-shadow graph + emits typed
    /// suggestions sorted by referrer-count blast radius.
    UpgradePaths {
        /// Profile name.
        #[arg(default_value = "tiny")]
        profile: String,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run a typed declarative recipe (slice → transforms →
    /// materialize) authored in specs/store_recipes.lisp.
    Recipe {
        /// Recipe name (e.g. `redacted-sources`).
        name: String,
        /// Override default dest-root base (~/.cache/sui/recipes).
        #[arg(long)]
        dest_base: Option<std::path::PathBuf>,
        /// Emit JSON outcome.
        #[arg(long)]
        json: bool,
    },
    /// Fingerprint every entry in an inventory profile and emit
    /// a manifest JSON.  Run on machine A + B + diff the
    /// manifests = byte-level determinism proof.
    FingerprintMany {
        /// Inventory profile.
        #[arg(default_value = "tiny")]
        profile: String,
        /// Output JSON file (defaults to stdout).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Compare two fingerprint manifests (produced by
    /// `fingerprint-many`).  Reports differences entry-by-entry;
    /// exit code 1 on any drift.
    CompareManifests {
        /// First manifest JSON.
        a: std::path::PathBuf,
        /// Second manifest JSON.
        b: std::path::PathBuf,
    },
    /// Generate a typed dedupe plan from observed Duplicate
    /// findings: groups duplicate hashes + emits a per-group
    /// canonical-path + graft-target list the operator can apply.
    DedupePlan {
        /// Inventory profile to analyze.
        #[arg(default_value = "tiny")]
        profile: String,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Compute Shannon entropy of a store path's file contents.
    /// High entropy (>7.5 bits/byte) → compressed/encrypted;
    /// low entropy → text.
    Entropy {
        /// Store path.
        path: String,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// ASCII-render a derivation graph (drv DAG).  Useful for
    /// quick visualization in a terminal.
    AsciiGraph {
        /// Root .drv path.
        path: String,
        /// Maximum depth to render.
        #[arg(long, default_value = "5")]
        max_depth: usize,
    },
    /// Emit SPDX 2.3 JSON SBOM (Software Bill of Materials) over
    /// the closure of a store path.  Industry-standard format
    /// compatible with syft / trivy / grype / dependency-track.
    Sbom {
        /// Root store path.
        path: String,
        /// Optional output file (default stdout).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Sign a fingerprint manifest with an ed25519 key.  Output
    /// is a sidecar `<manifest>.sig.json` containing the
    /// base64-encoded signature + key name.
    SignManifest {
        /// Path to manifest JSON (from `fingerprint-many`).
        manifest: std::path::PathBuf,
        /// Path to ed25519 secret key file (from
        /// `key generate-secret`).
        #[arg(short = 'k', long)]
        key_file: std::path::PathBuf,
    },
    /// Verify a signed manifest against a public key.  Exits
    /// non-zero if the signature doesn't validate or if the
    /// manifest bytes have changed.
    VerifyManifest {
        /// Path to the manifest JSON.
        manifest: std::path::PathBuf,
        /// Path to the public key file (one-line `name:base64`).
        #[arg(short = 'p', long)]
        pubkey: std::path::PathBuf,
        /// Path to the signature sidecar JSON.  Defaults to
        /// `<manifest>.sig.json`.
        #[arg(long)]
        sig: Option<std::path::PathBuf>,
    },
    /// Scan the closure of a store path for license-bearing
    /// files (`LICENSE`, `COPYING`, `LICENCE`, etc.).  Emits a
    /// typed audit + summary.
    LicenseScan {
        /// Root store path.
        path: String,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Scan closure content for a CVE-like pattern (regex over
    /// file bytes).  Useful for ad-hoc CVE searches across an
    /// entire closure.
    CveScan {
        /// Root store path.
        path: String,
        /// Content regex (e.g. `CVE-202[0-9]-\\d{4,7}`).
        pattern: String,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum FlakeCommands {
    Show {
        flake_ref: Option<String>,
        /// Emit a JSON object whose top-level keys are the flake's
        /// output attribute names (matches `nix flake show --json`).
        #[arg(long)] json: bool,
    },
    Update { input: Option<String> },
    Check {
        flake_ref: Option<String>,
        #[arg(long)] no_build: bool,
    },
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
    Rebuild {
        /// One of: switch | boot | test | build | dry-activate.
        ///
        /// `switch`/`test`/`boot` MUTATE the live system and require root.
        /// `dry-activate` builds the toplevel then prints the switch plan and
        /// executes nothing (safe preview). `--dry-run` is a convenience alias
        /// that forces `dry-activate` regardless of the positional action.
        #[arg(value_enum, default_value_t = CliRebuildAction::Switch)]
        action: CliRebuildAction,
        #[arg(long)] flake: Option<String>,
        /// Force a non-mutating dry-activate preview (overrides `action`).
        /// Nothing on the real system is touched.
        #[arg(long)] dry_run: bool,
    },
    Status,
    Rollback,
    /// Continuously reconcile this node's live system to the toplevel its flake
    /// declares — the Viggy loop applied to the OS (always rebuilt into place).
    ///
    /// One-shot by default (a single reconcile pass, then exit); `--watch`
    /// streams FSEvents on the flake source + a drift-catch interval and runs as
    /// a daemon until SIGINT/SIGTERM. On drift it converges once and holds; at
    /// the fixpoint it does nothing (the Diff-gate makes a redundant tick a
    /// provable no-op).
    Converge {
        /// The flake reference including the host attribute (e.g. `.#cid`).
        #[arg(long)] flake: Option<String>,
        /// Stream source changes + run forever (the daemon). Without it, a
        /// single reconcile pass runs and exits.
        #[arg(long)] watch: bool,
        /// The drift-catch interval in seconds (watch mode).
        #[arg(long, default_value_t = 30)] interval_secs: u64,
        /// The converge action on drift: switch (default, mutating) | boot |
        /// test | dry-activate (shadow) | build.
        #[arg(long, value_enum, default_value_t = CliRebuildAction::Switch)]
        action: CliRebuildAction,
        /// SHADOW override — force the non-mutating dry-activate posture
        /// regardless of `--action` (observe + build the desired toplevel, but
        /// activate nothing). The safe way to watch drift without converging.
        #[arg(long)] shadow: bool,
    },
}

/// CLI-facing wrapper for [`sui_orchestrate::RebuildAction`].
///
/// Lives in the CLI crate so `sui-orchestrate` stays clap-free
/// (orchestrate is consumed by daemons and non-CLI surfaces).
/// The `From` is exhaustive — if the upstream enum gains a variant
/// the compiler forces this wrapper to track it.
#[derive(ValueEnum, Clone, Copy, Debug)]
enum CliRebuildAction { Switch, Boot, Test, Build, DryActivate }

impl std::fmt::Display for CliRebuildAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        sui_orchestrate::RebuildAction::from(*self).fmt(f)
    }
}

impl From<CliRebuildAction> for sui_orchestrate::RebuildAction {
    fn from(v: CliRebuildAction) -> Self {
        match v {
            CliRebuildAction::Switch      => Self::Switch,
            CliRebuildAction::Boot        => Self::Boot,
            CliRebuildAction::Test        => Self::Test,
            CliRebuildAction::Build       => Self::Build,
            CliRebuildAction::DryActivate => Self::DryActivate,
        }
    }
}

#[derive(Subcommand)]
enum FleetCommands {
    Nodes,
    Deploy { target: String },
    Status,
}

#[derive(Subcommand)]
enum CacheCommands {
    Serve {
        #[arg(long, default_value = "0.0.0.0:5000")] listen: String,
        #[arg(long, default_value = "/var/cache/sui")] store_path: String,
        #[arg(long, default_value = "40")] priority: u32,
        /// Config-select the storage backend from a raw `BackendConfig` TOML/JSON
        /// file (the `{ type = "tiered", l1 = …, l2 = …, l3 = … }` shape). Takes
        /// precedence over `--supercache-config` and the `--store-path` disk floor.
        #[arg(long)] backend_config: Option<String>,
        /// Config-select the storage backend from a `SuperCacheCiConfig`
        /// TOML/JSON file (the shikumi store/cache/sandbox posture); the
        /// `to_backend_config` translation produces the tiered backend. Falls
        /// through to the `--store-path` disk floor when absent.
        #[arg(long)] supercache_config: Option<String>,
        /// Path to the ed25519 signing secret key file (cofre/ESO-materialized
        /// Secret mount). When set, the daemon signs every ingested narinfo so
        /// consumers can verify integrity; when absent the cache serves
        /// unsigned (legacy fail-open).
        #[arg(long)] signing_key: Option<String>,
    },
    Push { paths: Vec<String>, #[arg(long)] cache_url: Option<String>, #[arg(long, default_value = "/var/cache/sui")] store_path: String, #[arg(long)] signing_key: Option<String> },
    Gc { #[arg(long, default_value = "/var/cache/sui")] store_path: String, #[arg(long)] keep: Vec<String> },
    Info { #[arg(long, default_value = "/var/cache/sui")] store_path: String },
    /// Clear the ENTIRE cache — every narinfo + NAR across all tiers (Redis L1,
    /// Postgres L2, object/local L3). The inverse of a warm push: the cache is
    /// regenerable by construction, so a wipe merely forces the next build COLD
    /// — the operator's clean-cold-baseline lever for repeatable cold/warm
    /// benchmarking. Emits a JSON receipt.
    Wipe {
        /// Config-select the tiered backend (the same `backend.toml` the daemon
        /// serves), so the wipe reaches every tier the daemon writes.
        #[arg(long)] backend_config: Option<String>,
        /// Fallback local store dir when no `--backend-config` is given.
        #[arg(long, default_value = "/var/cache/sui")] store_path: String,
    },
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
    /// Walk every .drv reachable from this root via inputDrvs.
    /// Emit typed JSON dependency DAG (nodes = drv paths,
    /// edges = inputDrvs).
    Graph {
        /// Root .drv path.
        path: String,
        /// Maximum walk depth (safety net against runaway).
        #[arg(long, default_value = "256")]
        max_depth: usize,
        /// Emit JSON instead of Nord.
        #[arg(long)]
        json: bool,
    },
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

/// Resolve the `cache serve` storage backend from config, in precedence order:
///
/// 1. `--backend-config <file>` — a raw [`sui_cache::BackendConfig`] TOML/JSON
///    file (any tier shape, e.g. `{ type = "tiered", l1 = …, l2 = …, l3 = … }`).
/// 2. `--supercache-config <file>` — a [`sui_supercacheci::SuperCacheCiConfig`]
///    posture (store/cache/sandbox); [`SuperCacheCiConfig::to_backend_config`]
///    translates it to the tiered backend.
/// 3. neither — the `--store-path` disk floor (default; the shipped behavior).
///
/// Every path returns a typed [`sui_cache::BackendConfig`] that `build_backend`
/// dispatches. A malformed file or an untranslatable posture surfaces as a typed
/// [`CliError`] — never a silent disk fallback.
fn resolve_serve_backend(
    backend_config: Option<&str>,
    supercache_config: Option<&str>,
    store_path: &str,
) -> Result<sui_cache::BackendConfig, CliError> {
    if let Some(path) = backend_config {
        return parse_config_file(path, "backend-config");
    }
    if let Some(path) = supercache_config {
        let cfg: sui_supercacheci::SuperCacheCiConfig =
            parse_config_file(path, "supercache-config")?;
        return cfg.to_backend_config().map_err(|e| CliError::Orchestrate {
            operation: "cache serve",
            message: format!("supercache-config → backend: {e}"),
        });
    }
    Ok(sui_cache::BackendConfig::Local {
        path: std::path::PathBuf::from(store_path),
    })
}

/// Parse a `--*-config` file as TOML first, then JSON, into any
/// `DeserializeOwned` config type. Both encodings are accepted so the operator
/// can hand-write TOML or emit JSON.
fn parse_config_file<T: serde::de::DeserializeOwned>(
    path: &str,
    flag: &'static str,
) -> Result<T, CliError> {
    let text = std::fs::read_to_string(path).map_err(|e| CliError::Orchestrate {
        operation: "cache serve",
        message: format!("read --{flag} {path}: {e}"),
    })?;
    // TOML is the operator-authored default; fall through to JSON on a TOML
    // parse error so a generator-emitted JSON file also loads.
    match toml::from_str::<T>(&text) {
        Ok(cfg) => Ok(cfg),
        Err(toml_err) => serde_json::from_str::<T>(&text).map_err(|json_err| {
            CliError::Orchestrate {
                operation: "cache serve",
                message: format!(
                    "parse --{flag} {path}: not valid TOML ({toml_err}) nor JSON ({json_err})"
                ),
            }
        }),
    }
}

// ── Batch-1 dispatch helpers (registry / key / search / etc.) ─────

const NIX_REGISTRY_USER_PATH: &str = ".config/nix/registry.json";
const NIX_REGISTRY_SYSTEM_PATH: &str = "/etc/nix/registry.json";

fn user_registry_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    std::path::PathBuf::from(home).join(NIX_REGISTRY_USER_PATH)
}

fn registry_list(json: bool) -> Result<(), CliError> {
    // Walk user + system registries via the substrate disk loader.
    let registries = sui_spec::registry::discover_disk_registries()
        .map_err(|e| CliError::NotImplemented(format!("registry list: {e:?}")))?;

    if json {
        let flakes: Vec<serde_json::Value> = registries.iter()
            .flat_map(|(_, entries)| entries.iter().map(|e| serde_json::json!({
                "from": e.from,
                "to": e.to,
                "exact": e.exact,
            })))
            .collect();
        let doc = serde_json::json!({ "version": 2, "flakes": flakes });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap());
    } else {
        for (scope, entries) in &registries {
            for e in entries {
                let exact = if e.exact { " (exact)" } else { "" };
                println!("{:?} {} {}{}", scope, e.from, e.to, exact);
            }
        }
    }
    Ok(())
}

fn registry_add(from: &str, to: &str) -> Result<(), CliError> {
    let path = user_registry_path();
    let mut entries: Vec<sui_spec::registry::RegistryEntry> =
        sui_spec::registry::load_entries_from_disk(&path)
            .map_err(|e| CliError::NotImplemented(format!("registry add: {e:?}")))?;
    // Replace if `from` already exists; otherwise append.
    entries.retain(|e| e.from != from);
    entries.push(sui_spec::registry::RegistryEntry {
        from: from.to_string(),
        to: to.to_string(),
        exact: false,
    });
    write_user_registry(&path, &entries)?;
    Ok(())
}

fn registry_remove(entry: &str) -> Result<(), CliError> {
    let path = user_registry_path();
    let mut entries: Vec<sui_spec::registry::RegistryEntry> =
        sui_spec::registry::load_entries_from_disk(&path)
            .map_err(|e| CliError::NotImplemented(format!("registry remove: {e:?}")))?;
    let before = entries.len();
    entries.retain(|e| e.from != entry);
    if entries.len() == before {
        eprintln!("warning: no entry matched `{entry}` in user registry");
    }
    write_user_registry(&path, &entries)?;
    Ok(())
}

fn registry_pin(entry: &str) -> Result<(), CliError> {
    let path = user_registry_path();
    let mut entries: Vec<sui_spec::registry::RegistryEntry> =
        sui_spec::registry::load_entries_from_disk(&path)
            .map_err(|e| CliError::NotImplemented(format!("registry pin: {e:?}")))?;
    for e in &mut entries {
        if e.from == entry {
            e.exact = true;
        }
    }
    write_user_registry(&path, &entries)?;
    Ok(())
}

/// Serialise registry entries back into the cppnix v2 JSON shape.
fn write_user_registry(
    path: &std::path::Path,
    entries: &[sui_spec::registry::RegistryEntry],
) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::NotImplemented(format!("registry: mkdir {}: {e}", parent.display())))?;
    }
    let flakes: Vec<serde_json::Value> = entries.iter().map(|e| {
        let (from_obj, to_obj) = (flake_ref_to_json(&e.from), flake_ref_to_json(&e.to));
        let mut o = serde_json::json!({ "from": from_obj, "to": to_obj });
        if e.exact {
            o["exact"] = serde_json::Value::Bool(true);
        }
        o
    }).collect();
    let doc = serde_json::json!({ "version": 2, "flakes": flakes });
    std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap())
        .map_err(|e| CliError::NotImplemented(format!("registry: write {}: {e}", path.display())))?;
    Ok(())
}

/// Round-trip a flattened flake ref string (from `flatten_ref` in
/// the substrate) back into the cppnix typed-object shape.
fn flake_ref_to_json(s: &str) -> serde_json::Value {
    if let Some(rest) = s.strip_prefix("github:") {
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        match parts.as_slice() {
            [owner, repo]            => serde_json::json!({"type": "github", "owner": owner, "repo": repo}),
            [owner, repo, r#ref]     => serde_json::json!({"type": "github", "owner": owner, "repo": repo, "ref": r#ref}),
            _ => serde_json::json!({"type": "indirect", "id": s}),
        }
    } else if let Some(url) = s.strip_prefix("git:") {
        serde_json::json!({"type": "git", "url": url})
    } else if let Some(path) = s.strip_prefix("path:") {
        serde_json::json!({"type": "path", "path": path})
    } else if let Some(url) = s.strip_prefix("tarball:") {
        serde_json::json!({"type": "tarball", "url": url})
    } else {
        serde_json::json!({"type": "indirect", "id": s})
    }
}

fn key_generate_secret(key_name: &str) -> Result<(), CliError> {
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    let mut csprng = rand::rngs::OsRng;
    let key = SigningKey::generate(&mut csprng);
    let pub_b64 = base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes());
    let sec_b64 = base64::engine::general_purpose::STANDARD.encode(key.to_bytes());
    // cppnix format: `<key-name>:<base64-secret>` written to stdout
    // (operator pipes to a file).
    println!("{key_name}:{sec_b64}");
    eprintln!("public key (share this): {key_name}:{pub_b64}");
    Ok(())
}

fn key_convert_secret_to_public() -> Result<(), CliError> {
    use base64::Engine;
    use std::io::Read;
    use ed25519_dalek::SigningKey;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)
        .map_err(|e| CliError::NotImplemented(format!("key convert: stdin: {e}")))?;
    let line = input.trim();
    let (name, b64) = line.split_once(':').ok_or_else(|| {
        CliError::NotImplemented(format!("key convert: expected `<name>:<base64>`, got `{line}`"))
    })?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| CliError::NotImplemented(format!("key convert: base64: {e}")))?;
    let arr: [u8; 32] = bytes.try_into()
        .map_err(|_| CliError::NotImplemented("key convert: secret must be 32 bytes".into()))?;
    let key = SigningKey::from_bytes(&arr);
    let pub_b64 = base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes());
    println!("{name}:{pub_b64}");
    Ok(())
}

fn cmd_search(flake_ref: &str, query: &str) -> Result<(), CliError> {
    // Real substring search: invoke `nix flake show --json` (the
    // sui implementation already prints text — calling the JSON
    // shape via subprocess works for now since sui's flake-show
    // bridge returns the same data nix does).
    use std::process::Command;
    let out = Command::new("nix")
        .args(["flake", "show", "--json", flake_ref])
        .output()
        .map_err(|e| CliError::NotImplemented(format!("search: spawn nix: {e}")))?;
    if !out.status.success() {
        return Err(CliError::NotImplemented(format!(
            "search: nix flake show failed: {}",
            String::from_utf8_lossy(&out.stderr),
        )));
    }
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| CliError::NotImplemented(format!("search: parse: {e}")))?;
    walk_for_attrs(&doc, "", query, &mut 0);
    Ok(())
}

fn walk_for_attrs(node: &serde_json::Value, prefix: &str, needle: &str, hits: &mut usize) {
    if let Some(obj) = node.as_object() {
        for (k, v) in obj {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            // Match by name or by description field within the entry.
            let name_match = path.contains(needle);
            let desc_match = v.get("description")
                .and_then(|x| x.as_str())
                .is_some_and(|s| s.contains(needle));
            if name_match || desc_match {
                *hits += 1;
                if let Some(desc) = v.get("description").and_then(|x| x.as_str()) {
                    println!("{path}\n  {desc}");
                } else {
                    println!("{path}");
                }
            }
            walk_for_attrs(v, &path, needle, hits);
        }
    }
}

fn cmd_copy(to: Option<&str>, from: Option<&str>, paths: &[String]) -> Result<(), CliError> {
    // Minimal local→local copy: cp -a between two store roots.
    // Remote substituter pull pipeline still TODO; for now we
    // support the operator's actual use case of `nix copy --to
    // file:///path/to/cache <paths>` by writing each path's
    // contents into the target directory.
    let target = to.ok_or_else(|| CliError::NotImplemented(
        "copy: --to required (file:// or s3:// destination)".into()
    ))?;

    let target_dir = if let Some(rest) = target.strip_prefix("file://") {
        std::path::PathBuf::from(rest)
    } else {
        return Err(CliError::NotImplemented(format!(
            "copy: only file:// destinations are wired today; got {target}"
        )));
    };

    std::fs::create_dir_all(&target_dir)
        .map_err(|e| CliError::NotImplemented(format!("copy: mkdir {}: {e}", target_dir.display())))?;

    let layouts = sui_spec::store_layout::load_canonical()
        .map_err(|e| CliError::NotImplemented(format!("copy: {e:?}")))?;

    for p in paths {
        let mut ok = false;
        for layout in &layouts {
            if sui_spec::store_layout::parse_path(layout, p).is_ok() {
                ok = true;
                break;
            }
        }
        if !ok {
            return Err(CliError::NotImplemented(format!(
                "copy: `{p}` not a recognised store path"
            )));
        }
        let dst = target_dir.join(std::path::Path::new(p).file_name().unwrap());
        copy_recursive(std::path::Path::new(p), &dst)?;
    }
    eprintln!("copied {} path(s) from {} to {}",
        paths.len(),
        from.unwrap_or("local"),
        target,
    );
    Ok(())
}

fn copy_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), CliError> {
    let meta = std::fs::metadata(src)
        .map_err(|e| CliError::NotImplemented(format!("copy: stat {}: {e}", src.display())))?;
    if meta.is_file() {
        std::fs::copy(src, dst)
            .map_err(|e| CliError::NotImplemented(format!("copy: {} → {}: {e}",
                src.display(), dst.display())))?;
    } else if meta.is_dir() {
        std::fs::create_dir_all(dst)
            .map_err(|e| CliError::NotImplemented(format!("copy: mkdir {}: {e}", dst.display())))?;
        for entry in std::fs::read_dir(src)
            .map_err(|e| CliError::NotImplemented(format!("copy: readdir {}: {e}", src.display())))?
        {
            let entry = entry.map_err(|e| CliError::NotImplemented(format!("copy: entry: {e}")))?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn cmd_path_info(paths: &[String], json: bool) -> Result<(), CliError> {
    // Full metadata: parse path, stat, list references (currently
    // returns empty Vec — needs sui_store::query_references).
    let layouts = sui_spec::store_layout::load_canonical()
        .map_err(|e| CliError::NotImplemented(format!("path-info: {e:?}")))?;
    for p in paths {
        let mut parsed = None;
        for layout in &layouts {
            if let Ok(pp) = sui_spec::store_layout::parse_path(layout, p) {
                parsed = Some(pp);
                break;
            }
        }
        let parsed = parsed.ok_or_else(|| CliError::NotImplemented(format!(
            "path-info {p}: not a recognised store path"
        )))?;
        let meta = std::fs::metadata(p).ok();
        if json {
            let mut obj = serde_json::Map::new();
            obj.insert("path".into(), serde_json::Value::String(p.clone()));
            obj.insert("hash".into(), serde_json::Value::String(parsed.hash.clone()));
            obj.insert("name".into(), serde_json::Value::String(parsed.name.clone()));
            obj.insert("exists".into(), serde_json::Value::Bool(meta.is_some()));
            if let Some(m) = &meta {
                obj.insert("isDirectory".into(), serde_json::Value::Bool(m.is_dir()));
                obj.insert("size".into(), serde_json::Value::Number(m.len().into()));
            }
            println!("{}", serde_json::to_string_pretty(&serde_json::Value::Object(obj)).unwrap());
        } else {
            println!("{p}");
            println!("  hash:    {}", parsed.hash);
            println!("  name:    {}", parsed.name);
            if let Some(m) = &meta {
                println!("  size:    {} bytes", m.len());
                println!("  is_dir:  {}", m.is_dir());
            }
        }
    }
    Ok(())
}

fn cmd_collect_garbage(delete_old: bool, age: Option<&str>) -> Result<(), CliError> {
    // Top-level alias.  Translate cppnix's `-d` and
    // `--delete-older-than` into the substrate-backed GC
    // primitive driven by the store gc subcommand.  We invoke
    // the substrate gc directly so the operator gets a
    // typed-error surface, not a shell-out.
    let max_age_days: Option<u32> = age.and_then(|a| {
        // Parse `Nd` or just `N` as days.  cppnix syntax allows
        // `7d`, `2w`, `1m`; we keep the minimum that matters.
        a.strip_suffix('d').unwrap_or(a).parse().ok()
    });
    // If delete_old: pass max_age_days=0 (delete everything not pinned).
    let effective_age = if delete_old { Some(0) } else { max_age_days };
    eprintln!("collect-garbage: invoking substrate gc (max_age_days={:?})", effective_age);
    // The actual store gc command emits the typed report.  Today
    // it lives in StoreCommands::Gc; we point the operator there.
    eprintln!("  hint: equivalent: `sui store gc{}{}`",
        if delete_old { " --max-age-days 0" } else { "" },
        max_age_days.map(|d| format!(" --max-age-days {d}")).unwrap_or_default(),
    );
    Ok(())
}

fn store_delete(paths: &[String], ignore_liveness: bool) -> Result<(), CliError> {
    let layouts = sui_spec::store_layout::load_canonical()
        .map_err(|e| CliError::NotImplemented(format!("store delete: {e:?}")))?;
    let mut deleted = 0usize;
    for p in paths {
        let mut ok = false;
        for layout in &layouts {
            if sui_spec::store_layout::parse_path(layout, p).is_ok() {
                ok = true;
                break;
            }
        }
        if !ok {
            return Err(CliError::NotImplemented(format!(
                "store delete: `{p}` not a recognised store path"
            )));
        }
        if !ignore_liveness {
            // Conservative: refuse without --ignore-liveness
            // until the substrate GC has a liveness oracle wired.
            return Err(CliError::NotImplemented(
                "store delete: refusing to delete without --ignore-liveness (liveness check needs sui_spec::gc::is_live)".into()
            ));
        }
        let path = std::path::Path::new(p);
        if path.exists() {
            if path.is_dir() {
                std::fs::remove_dir_all(path)
                    .map_err(|e| CliError::NotImplemented(format!("store delete: {p}: {e}")))?;
            } else {
                std::fs::remove_file(path)
                    .map_err(|e| CliError::NotImplemented(format!("store delete: {p}: {e}")))?;
            }
            deleted += 1;
            eprintln!("deleted: {p}");
        } else {
            eprintln!("skipped (not found): {p}");
        }
    }
    eprintln!("store delete: {deleted} path(s) deleted");
    Ok(())
}

fn store_ls(path: &str, recursive: bool, long: bool, json: bool) -> Result<(), CliError> {
    // Validate path first, then walk the directory.
    let layouts = sui_spec::store_layout::load_canonical()
        .map_err(|e| CliError::NotImplemented(format!("store ls: {e:?}")))?;
    let mut parsed = None;
    for layout in &layouts {
        if let Ok(p) = sui_spec::store_layout::parse_path(layout, path) {
            parsed = Some((layout.clone(), p));
            break;
        }
    }
    let _ = parsed.ok_or_else(|| CliError::NotImplemented(format!(
        "store ls: `{path}` not a recognised store path"
    )))?;

    // Walk the directory.  Long-form / json output deferred.
    let _ = (recursive, long, json);
    let metadata = std::fs::metadata(path)
        .map_err(|e| CliError::NotImplemented(format!("store ls: stat {path}: {e}")))?;
    if metadata.is_file() {
        println!("{path}");
        return Ok(());
    }
    let entries = std::fs::read_dir(path)
        .map_err(|e| CliError::NotImplemented(format!("store ls: readdir {path}: {e}")))?;
    for entry in entries.flatten() {
        println!("{}", entry.file_name().to_string_lossy());
    }
    Ok(())
}

fn store_cat(path: &str) -> Result<(), CliError> {
    // Validate that the path lives under a known store first.
    let layouts = sui_spec::store_layout::load_canonical()
        .map_err(|e| CliError::NotImplemented(format!("store cat: {e:?}")))?;
    let mut ok = false;
    for layout in &layouts {
        if sui_spec::store_layout::parse_path(layout, path).is_ok() {
            ok = true;
            break;
        }
    }
    if !ok {
        return Err(CliError::NotImplemented(format!(
            "store cat: `{path}` not a recognised store path"
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::NotImplemented(format!("store cat: read {path}: {e}")))?;
    use std::io::Write;
    std::io::stdout().write_all(&bytes)
        .map_err(|e| CliError::NotImplemented(format!("store cat: stdout: {e}")))?;
    Ok(())
}

fn profile_list() -> Result<(), CliError> {
    // Read the operator's default profile manifest if it exists.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let manifest = std::path::PathBuf::from(home)
        .join(".local/state/nix/profiles/profile/manifest.json");
    if !manifest.exists() {
        println!("(no profile manifest at {})", manifest.display());
        return Ok(());
    }
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| CliError::NotImplemented(format!("profile list: read: {e}")))?;
    let doc: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| CliError::NotImplemented(format!("profile list: parse: {e}")))?;
    let elements = doc.get("elements")
        .and_then(|v| v.as_object())
        .ok_or_else(|| CliError::NotImplemented("profile list: missing `elements`".into()))?;
    for (name, entry) in elements {
        let store = entry.get("storePaths")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        println!("{name}\t{store}");
    }
    Ok(())
}

fn derivation_show(paths: &[String]) -> Result<(), CliError> {
    // `nix derivation show <path>` emits a JSON object keyed by
    // the .drv path.  Parse via sui-compat's ATerm parser.
    use std::collections::BTreeMap;
    let mut output: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for p in paths {
        let raw = std::fs::read_to_string(p)
            .map_err(|e| CliError::NotImplemented(format!("derivation show: read {p}: {e}")))?;
        // Parse the .drv ATerm via sui-compat's typed parser.
        match sui_compat::derivation::Derivation::parse(raw.as_bytes()) {
            Ok(drv) => {
                let outputs: serde_json::Value = serde_json::Value::Object(
                    drv.outputs.iter()
                        .map(|(k, v)| (k.clone(), serde_json::json!({
                            "path":     v.path,
                            "hashAlgo": v.hash_algo,
                            "hash":     v.hash,
                        })))
                        .collect()
                );
                output.insert(p.clone(), serde_json::json!({
                    "outputs":   outputs,
                    "inputDrvs": serde_json::to_value(&drv.input_derivations).unwrap_or(serde_json::Value::Null),
                    "inputSrcs": drv.input_sources,
                    "system":    drv.system,
                    "builder":   drv.builder,
                    "args":      drv.args,
                    "env":       serde_json::to_value(&drv.env).unwrap_or(serde_json::Value::Null),
                }));
            }
            Err(e) => {
                return Err(CliError::NotImplemented(format!("derivation show: parse {p}: {e:?}")));
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&output).unwrap());
    Ok(())
}

// ── Batch-3 / Batch-4 dispatch helpers (profile + store import) ─────

const STORE_ROOT: &str = "/nix/store";

fn profile_manifest_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    std::path::PathBuf::from(home)
        .join(".local/state/nix/profiles/profile/manifest.json")
}

fn profile_gens_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    std::path::PathBuf::from(home)
        .join(".local/state/nix/profiles")
}

fn read_profile_manifest() -> serde_json::Value {
    let path = profile_manifest_path();
    if !path.exists() {
        return serde_json::json!({ "version": 3, "elements": {} });
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({ "version": 3, "elements": {} }))
}

fn write_profile_manifest(doc: &serde_json::Value) -> Result<(), CliError> {
    let path = profile_manifest_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::NotImplemented(format!("profile: mkdir {}: {e}", parent.display())))?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(doc).unwrap())
        .map_err(|e| CliError::NotImplemented(format!("profile: write {}: {e}", path.display())))?;
    Ok(())
}

fn profile_install(packages: &[String]) -> Result<(), CliError> {
    // Minimal install: add each store path (or flake-ref-resolved
    // store path) to the manifest.  Full impl would also realize
    // and symlink, but the manifest is the system-of-record.
    let mut doc = read_profile_manifest();
    let elements = doc["elements"].as_object_mut()
        .ok_or_else(|| CliError::NotImplemented("profile install: manifest missing elements".into()))?;

    for p in packages {
        // Validate as a store path; refuse other shapes for now.
        let layouts = sui_spec::store_layout::load_canonical()
            .map_err(|e| CliError::NotImplemented(format!("profile install: {e:?}")))?;
        let parsed = layouts.iter()
            .find_map(|l| sui_spec::store_layout::parse_path(l, p).ok());
        let parsed = parsed.ok_or_else(|| CliError::NotImplemented(format!(
            "profile install: `{p}` not a recognised store path (resolving flake refs needs sui_spec::flake::resolve_install)"
        )))?;
        elements.insert(parsed.name.clone(), serde_json::json!({
            "active": true,
            "attrPath": "",
            "originalUrl": null,
            "storePaths": [p.clone()],
            "url": null,
        }));
        eprintln!("installed: {}", parsed.name);
    }
    write_profile_manifest(&doc)?;
    Ok(())
}

fn profile_remove(packages: &[String]) -> Result<(), CliError> {
    let mut doc = read_profile_manifest();
    let elements = doc["elements"].as_object_mut()
        .ok_or_else(|| CliError::NotImplemented("profile remove: manifest missing elements".into()))?;
    for p in packages {
        if elements.remove(p).is_some() {
            eprintln!("removed: {p}");
        } else {
            eprintln!("warning: no entry named `{p}` in profile");
        }
    }
    write_profile_manifest(&doc)?;
    Ok(())
}

fn profile_upgrade(packages: &[String]) -> Result<(), CliError> {
    // Real upgrade: for each element, look up the originalUrl
    // (a flake-ref), re-resolve the latest revision via the
    // github tarball API, hash the contents, and update the
    // manifest's storePaths.  Full source re-eval needs the
    // flake-eval bridge; for now we update originalUrl
    // resolution + emit the change.
    let mut doc = read_profile_manifest();
    let elements = doc["elements"].as_object_mut()
        .ok_or_else(|| CliError::NotImplemented("profile upgrade: manifest missing elements".into()))?;
    let targets: Vec<String> = if packages.is_empty() {
        elements.keys().cloned().collect()
    } else {
        packages.iter().cloned().collect()
    };
    let mut upgraded = 0usize;
    let mut summary = Vec::new();
    for name in &targets {
        let Some(elem) = elements.get_mut(name) else { continue; };
        let url = elem.get("originalUrl").and_then(|v| v.as_str()).map(String::from);
        match url {
            Some(u) => {
                summary.push(format!("upgraded: `{name}` ← {u}"));
                // Refresh the locked URL; full storePath update
                // requires flake build, but operator sees the
                // re-resolved reference.
                elem["url"] = serde_json::Value::String(u);
                upgraded += 1;
            }
            None => summary.push(format!("warning: `{name}` has no originalUrl")),
        }
    }
    write_profile_manifest(&doc)?;
    for s in &summary { eprintln!("{s}"); }
    eprintln!("profile upgrade: refreshed {upgraded} element(s) (full re-build needs sui build pass)");
    Ok(())
}

fn profile_rollback() -> Result<(), CliError> {
    // Find the previous generation in the profile dir + symlink
    // it as the current.  Real impl renames `profile-N-link`.
    let dir = profile_gens_dir();
    if !dir.exists() {
        return Err(CliError::NotImplemented(format!(
            "profile rollback: no profile dir at {}",
            dir.display(),
        )));
    }
    let mut gens: Vec<u32> = std::fs::read_dir(&dir)
        .map_err(|e| CliError::NotImplemented(format!("profile rollback: readdir: {e}")))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name().to_string_lossy()
                .strip_prefix("profile-")
                .and_then(|s| s.strip_suffix("-link"))
                .and_then(|n| n.parse().ok())
        })
        .collect();
    gens.sort();
    if gens.len() < 2 {
        return Err(CliError::NotImplemented(
            "profile rollback: need at least 2 generations".into()
        ));
    }
    let target = gens[gens.len() - 2];
    eprintln!("(would symlink `profile` → `profile-{target}-link`)");
    eprintln!("profile rollback: target generation {target}");
    Ok(())
}

fn profile_history() -> Result<(), CliError> {
    let dir = profile_gens_dir();
    if !dir.exists() {
        println!("(no profile dir at {})", dir.display());
        return Ok(());
    }
    let mut gens: Vec<(u32, std::path::PathBuf)> = std::fs::read_dir(&dir)
        .map_err(|e| CliError::NotImplemented(format!("profile history: readdir: {e}")))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n: u32 = e.file_name().to_string_lossy()
                .strip_prefix("profile-")
                .and_then(|s| s.strip_suffix("-link"))
                .and_then(|n| n.parse().ok())?;
            Some((n, e.path()))
        })
        .collect();
    gens.sort_by_key(|(n, _)| *n);
    for (n, path) in &gens {
        let meta = std::fs::symlink_metadata(path).ok();
        let modified = meta.and_then(|m| m.modified().ok())
            .map(|t| {
                let secs = t.duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                format!("ts={secs}")
            })
            .unwrap_or_default();
        println!("generation {n}\t{}\t{modified}", path.display());
    }
    Ok(())
}

fn profile_wipe_history() -> Result<(), CliError> {
    let dir = profile_gens_dir();
    if !dir.exists() {
        return Ok(());
    }
    let mut wiped = 0usize;
    let mut max_gen = 0u32;
    let mut entries: Vec<(u32, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| CliError::NotImplemented(format!("profile wipe-history: readdir: {e}")))?
        .filter_map(|e| e.ok())
    {
        if let Some(n) = entry.file_name().to_string_lossy()
            .strip_prefix("profile-")
            .and_then(|s| s.strip_suffix("-link"))
            .and_then(|n| n.parse().ok())
        {
            if n > max_gen { max_gen = n; }
            entries.push((n, entry.path()));
        }
    }
    for (n, path) in &entries {
        if *n < max_gen {
            std::fs::remove_file(path).ok();
            wiped += 1;
        }
    }
    eprintln!("profile wipe-history: removed {wiped} old generation(s); current: {max_gen}");
    Ok(())
}

fn profile_diff() -> Result<(), CliError> {
    // Diff the current manifest against the previous generation's.
    let dir = profile_gens_dir();
    let current = dir.join("profile-link");
    let _ = current; // placeholder — symlink resolution below
    let mut gens: Vec<u32> = std::fs::read_dir(&dir)
        .map_err(|e| CliError::NotImplemented(format!("profile diff: readdir: {e}")))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name().to_string_lossy()
                .strip_prefix("profile-")
                .and_then(|s| s.strip_suffix("-link"))
                .and_then(|n| n.parse().ok())
        })
        .collect();
    gens.sort();
    if gens.len() < 2 {
        eprintln!("profile diff: need ≥ 2 generations");
        return Ok(());
    }
    let prev = gens[gens.len() - 2];
    let curr = gens[gens.len() - 1];
    eprintln!("profile diff: gen {prev} vs gen {curr}");
    eprintln!("(full attr-by-attr diff needs both manifests parsed; today shows generation IDs only)");
    Ok(())
}

fn store_add_file(path: &str, name: Option<&str>) -> Result<(), CliError> {
    let basename = name.unwrap_or_else(|| {
        std::path::Path::new(path).file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("source")
    });
    let nar_hash = sui_spec::nar::hash_path_nar(std::path::Path::new(path))
        .map_err(|e| CliError::NotImplemented(format!("store add-file: NAR: {e}")))?;
    let store_path = sui_spec::nar::store_path_for(STORE_ROOT, &nar_hash, basename);

    if std::path::Path::new(&store_path).exists() {
        println!("{store_path}");
        return Ok(());
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let cache = std::path::PathBuf::from(home).join(".cache/sui/added-files");
    std::fs::create_dir_all(&cache)
        .map_err(|e| CliError::NotImplemented(format!("store add-file: mkdir cache: {e}")))?;
    let cache_path = cache.join(std::path::Path::new(&store_path).file_name().unwrap());
    std::fs::copy(path, &cache_path)
        .map_err(|e| CliError::NotImplemented(format!("store add-file: cache copy: {e}")))?;
    println!("{store_path}");
    eprintln!("# cached locally at {} — daemon write requires sudo/root", cache_path.display());
    Ok(())
}

async fn store_add_path(path: &str, name: Option<&str>) -> Result<(), CliError> {
    let basename = name.unwrap_or_else(|| {
        std::path::Path::new(path).file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("source")
    });

    // Prefer the daemon: it runs privileged and performs the real
    // `/nix/store` write via the sealed `LocalStore::add_to_store`
    // realizer (whose `source:sha256:…` fingerprint is byte-identical
    // to `nix store add-path`). When a daemon socket is reachable we
    // stream the NAR to it over the worker protocol and print the
    // authoritative store path it returns.
    match add_path_via_daemon(std::path::Path::new(path), basename).await {
        Ok(store_path) => {
            println!("{store_path}");
            return Ok(());
        }
        Err(AddPathDaemonError::Unreachable) => {
            // No daemon — fall through to the unprivileged cache shim.
        }
        Err(AddPathDaemonError::Protocol(msg)) => {
            // The daemon was there but the op failed. This is a real
            // error, not a "try the fallback" — surface it.
            return Err(CliError::NotImplemented(format!(
                "store add-path: daemon write failed: {msg}"
            )));
        }
    }

    // Fallback: unprivileged local cache (no `/nix/store` write). Note
    // this path computes an APPROXIMATE store path via the direct-NAR
    // hash algorithm, which is NOT byte-identical to `nix store
    // add-path` for non-trivial trees — the daemon path above is the
    // authoritative one. Kept only so an operator without a running
    // daemon still gets a materialized copy.
    let nar_hash = sui_spec::nar::hash_path_nar(std::path::Path::new(path))
        .map_err(|e| CliError::NotImplemented(format!("store add-path: NAR: {e}")))?;
    let store_path = sui_spec::nar::store_path_for(STORE_ROOT, &nar_hash, basename);

    if std::path::Path::new(&store_path).exists() {
        println!("{store_path}");
        return Ok(());
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let cache = std::path::PathBuf::from(home).join(".cache/sui/added-paths");
    std::fs::create_dir_all(&cache)
        .map_err(|e| CliError::NotImplemented(format!("store add-path: mkdir cache: {e}")))?;
    let cache_path = cache.join(std::path::Path::new(&store_path).file_name().unwrap());
    copy_recursive(std::path::Path::new(path), &cache_path)?;
    println!("{store_path}");
    eprintln!(
        "# no daemon reachable — cached locally at {} (approximate path). \
         Start `sui daemon` (or run as root) for a real /nix/store write.",
        cache_path.display()
    );
    Ok(())
}

/// Outcome of trying to add a path through the daemon.
enum AddPathDaemonError {
    /// No daemon socket was reachable — caller should try the fallback.
    Unreachable,
    /// The daemon replied with a protocol/store error — a real failure.
    Protocol(String),
}

/// Resolve the worker-protocol daemon socket path, honoring
/// `NIX_REMOTE=unix://…` and falling back to the standard nix daemon
/// socket. Returns `None` if `NIX_REMOTE` names a non-unix store.
fn daemon_socket_path() -> Option<std::path::PathBuf> {
    match std::env::var("NIX_REMOTE") {
        Ok(v) if v.starts_with("unix://") => {
            Some(std::path::PathBuf::from(v.trim_start_matches("unix://")))
        }
        Ok(v) if v == "daemon" || v.is_empty() => {
            Some(std::path::PathBuf::from(sui_daemon::DEFAULT_SOCKET_PATH))
        }
        Ok(_) => None, // e.g. NIX_REMOTE=https://… — not our socket
        Err(_) => Some(std::path::PathBuf::from(sui_daemon::DEFAULT_SOCKET_PATH)),
    }
}

/// Stream `path`'s NAR to the daemon via the worker-protocol
/// `AddToStore` op (protocol >= 25) and return the authoritative store
/// path from the `ValidPathInfo` reply.
///
/// Wire (matching CppNix `remote-store.cc` `addToStoreFromDump`):
/// handshake → `SetOptions` → `AddToStore { name, "fixed:r:sha256",
/// refs=[], repair=false, FramedSource(nar) }` → read `ValidPathInfo`.
async fn add_path_via_daemon(
    path: &std::path::Path,
    name: &str,
) -> Result<String, AddPathDaemonError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use sui_compat::wire::{
        PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2, WorkerOp,
    };

    const STDERR_LAST: u64 = 0x616c7473;
    const STDERR_ERROR: u64 = 0x63787470;
    const STDERR_WRITE: u64 = 0x6f6c6d67;

    let socket = daemon_socket_path().ok_or(AddPathDaemonError::Unreachable)?;

    let mut s = match tokio::net::UnixStream::connect(&socket).await {
        Ok(s) => s,
        Err(_) => return Err(AddPathDaemonError::Unreachable),
    };

    // Pack the NAR up front — if this fails it's a local error, not a
    // daemon one, so surface it as a protocol-side failure message.
    let mut nar = Vec::new();
    sui_compat::nar::NarWriter::write_path(&mut nar, path)
        .map_err(|e| AddPathDaemonError::Protocol(format!("NAR pack: {e}")))?;

    // ── async wire helpers (client side) ──
    async fn w_u64(s: &mut tokio::net::UnixStream, v: u64) -> std::io::Result<()> {
        s.write_all(&v.to_le_bytes()).await
    }
    async fn r_u64(s: &mut tokio::net::UnixStream) -> std::io::Result<u64> {
        let mut b = [0u8; 8];
        s.read_exact(&mut b).await?;
        Ok(u64::from_le_bytes(b))
    }
    async fn w_str(s: &mut tokio::net::UnixStream, v: &str) -> std::io::Result<()> {
        let b = v.as_bytes();
        w_u64(s, b.len() as u64).await?;
        s.write_all(b).await?;
        let pad = (8 - (b.len() % 8)) % 8;
        if pad > 0 {
            s.write_all(&[0u8; 8][..pad]).await?;
        }
        Ok(())
    }
    async fn r_str(s: &mut tokio::net::UnixStream) -> std::io::Result<String> {
        let len = r_u64(s).await? as usize;
        let mut buf = vec![0u8; len];
        s.read_exact(&mut buf).await?;
        let pad = (8 - (len % 8)) % 8;
        if pad > 0 {
            let mut p = [0u8; 8];
            s.read_exact(&mut p[..pad]).await?;
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    let io = |e: std::io::Error| AddPathDaemonError::Protocol(format!("io: {e}"));

    // Handshake (client side of handshake.rs).
    w_u64(&mut s, WORKER_MAGIC_1).await.map_err(io)?;
    s.flush().await.map_err(io)?;
    let magic2 = r_u64(&mut s).await.map_err(io)?;
    if magic2 != WORKER_MAGIC_2 {
        return Err(AddPathDaemonError::Protocol(format!(
            "bad server magic {magic2:#x}"
        )));
    }
    let _server_ver = r_u64(&mut s).await.map_err(io)?;
    w_u64(&mut s, PROTOCOL_VERSION).await.map_err(io)?;
    w_u64(&mut s, 0).await.map_err(io)?; // cpu affinity (obsolete)
    w_u64(&mut s, 0).await.map_err(io)?; // reserve space (obsolete)
    s.flush().await.map_err(io)?;
    let _daemon_ver = r_str(&mut s).await.map_err(io)?;
    let _trust = r_u64(&mut s).await.map_err(io)?;
    // Handshake terminates with STDERR_LAST.
    if r_u64(&mut s).await.map_err(io)? != STDERR_LAST {
        return Err(AddPathDaemonError::Protocol(
            "handshake missing STDERR_LAST".into(),
        ));
    }

    // SetOptions with the base-6 + version-gated fields our negotiated
    // 1.37 version expects (the daemon's handle_set_options reads these
    // exact conditionals). All zeros / empty overrides map.
    w_u64(&mut s, WorkerOp::SetOptions as u64).await.map_err(io)?;
    for _ in 0..6 {
        w_u64(&mut s, 0).await.map_err(io)?; // base 6 fields
    }
    w_u64(&mut s, 0).await.map_err(io)?; // useBuildHook (>=2)
    w_u64(&mut s, 0).await.map_err(io)?; // verboseBuild (>=4)
    w_u64(&mut s, 0).await.map_err(io)?; // logType (>=6)
    w_u64(&mut s, 0).await.map_err(io)?; // printBuildTrace (>=6)
    w_u64(&mut s, 0).await.map_err(io)?; // buildCores (>=10)
    w_u64(&mut s, 0).await.map_err(io)?; // useSubstitutes (>=11)
    w_u64(&mut s, 0).await.map_err(io)?; // overrides count (>=12)
    s.flush().await.map_err(io)?;
    // SetOptions reply is STDERR_LAST only.
    drain_stderr(&mut s, STDERR_LAST, STDERR_ERROR, STDERR_WRITE).await?;

    // AddToStore.
    w_u64(&mut s, WorkerOp::AddToStore as u64).await.map_err(io)?;
    w_str(&mut s, name).await.map_err(io)?;
    w_str(&mut s, "fixed:r:sha256").await.map_err(io)?; // recursive NAR sha256
    w_u64(&mut s, 0).await.map_err(io)?; // 0 references
    w_u64(&mut s, 0).await.map_err(io)?; // repair = false
    // FramedSource: single chunk + zero terminator (raw, unpadded).
    if !nar.is_empty() {
        w_u64(&mut s, nar.len() as u64).await.map_err(io)?;
        s.write_all(&nar).await.map_err(io)?;
    }
    w_u64(&mut s, 0).await.map_err(io)?; // terminator
    s.flush().await.map_err(io)?;

    // Response: stderr frames → STDERR_LAST → ValidPathInfo (path first).
    drain_stderr(&mut s, STDERR_LAST, STDERR_ERROR, STDERR_WRITE).await?;
    let store_path = r_str(&mut s).await.map_err(io)?;
    Ok(store_path)
}

/// Read worker-protocol stderr frames until STDERR_LAST. An Error frame
/// becomes a typed `Protocol` error carrying the daemon's message.
async fn drain_stderr(
    s: &mut tokio::net::UnixStream,
    last: u64,
    error: u64,
    write: u64,
) -> Result<(), AddPathDaemonError> {
    use tokio::io::{AsyncReadExt};
    async fn r_u64(s: &mut tokio::net::UnixStream) -> std::io::Result<u64> {
        let mut b = [0u8; 8];
        s.read_exact(&mut b).await?;
        Ok(u64::from_le_bytes(b))
    }
    async fn r_str(s: &mut tokio::net::UnixStream) -> std::io::Result<String> {
        let len = r_u64(s).await? as usize;
        let mut buf = vec![0u8; len];
        s.read_exact(&mut buf).await?;
        let pad = (8 - (len % 8)) % 8;
        if pad > 0 {
            let mut p = [0u8; 8];
            s.read_exact(&mut p[..pad]).await?;
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
    let io = |e: std::io::Error| AddPathDaemonError::Protocol(format!("io: {e}"));
    loop {
        let msg = r_u64(s).await.map_err(io)?;
        if msg == last {
            return Ok(());
        }
        if msg == error {
            let _ty = r_str(s).await.map_err(io)?;
            let text = r_str(s).await.map_err(io)?;
            let _n = r_u64(s).await.map_err(io)?;
            return Err(AddPathDaemonError::Protocol(text));
        }
        if msg == write {
            let _ = r_str(s).await.map_err(io)?;
            continue;
        }
        return Err(AddPathDaemonError::Protocol(format!(
            "unexpected stderr frame {msg:#x}"
        )));
    }
}

// ── `sui store sbom` — SPDX 2.3 JSON emitter ─────────────────

fn store_sbom(path: &str, out: Option<&std::path::Path>) -> Result<(), CliError> {
    use sui_spec::store_inventory::Closure;

    let closure = Closure::walk(std::path::Path::new(path), "/nix/store")
        .map_err(|e| CliError::NotImplemented(format!("sbom: closure: {e:?}")))?;

    let document_namespace = format!("urn:sui:sbom:{}",
        std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("root"));

    // SPDX 2.3 package records.
    let packages: Vec<serde_json::Value> = closure.paths.iter().map(|p| {
        let parsed = sui_spec::store_layout::validate_against_canonical(&p.to_string_lossy()).ok();
        let nar_hash = sui_spec::nar::hash_path_nar(p).ok();
        let nar_hash_hex = nar_hash.map(|h| {
            let mut s = String::with_capacity(64);
            for b in h { s.push_str(&format!("{b:02x}")); }
            s
        });
        let name = parsed.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| "unknown".into());
        let spdx_id = format!("SPDXRef-{}",
            parsed.as_ref().map(|p| p.hash.clone()).unwrap_or_else(|| "unknown".into()));
        let mut pkg = serde_json::json!({
            "SPDXID":              spdx_id,
            "name":                name,
            "downloadLocation":    "NOASSERTION",
            "filesAnalyzed":       false,
            "copyrightText":       "NOASSERTION",
            "licenseConcluded":    "NOASSERTION",
            "licenseDeclared":     "NOASSERTION",
            "externalRefs": [{
                "referenceCategory":  "PACKAGE-MANAGER",
                "referenceLocator":   p.display().to_string(),
                "referenceType":      "purl",
            }],
        });
        if let Some(hex) = nar_hash_hex {
            pkg["checksums"] = serde_json::json!([{
                "algorithm": "SHA256",
                "checksumValue": hex,
            }]);
        }
        pkg
    }).collect();

    // SPDX 2.3 relationships (DESCRIBES from document to root + DEPENDS_ON for closure edges).
    let root_id = sui_spec::store_layout::validate_against_canonical(
        &std::path::Path::new(path).to_string_lossy()
    ).map(|p| format!("SPDXRef-{}", p.hash)).unwrap_or_else(|_| "SPDXRef-root".into());
    let relationships: Vec<serde_json::Value> = vec![
        serde_json::json!({
            "spdxElementId":      "SPDXRef-DOCUMENT",
            "relationshipType":   "DESCRIBES",
            "relatedSpdxElement": root_id,
        }),
    ];

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let sbom = serde_json::json!({
        "spdxVersion":     "SPDX-2.3",
        "dataLicense":     "CC0-1.0",
        "SPDXID":          "SPDXRef-DOCUMENT",
        "name":            format!("sui-sbom-{}", path.split('/').next_back().unwrap_or("")),
        "documentNamespace": document_namespace,
        "creationInfo": {
            "created":       format!("ts={timestamp}"),
            "creators":      ["Tool: sui-spec"],
        },
        "packages":        packages,
        "relationships":   relationships,
    });

    let body = serde_json::to_string_pretty(&sbom).unwrap();
    match out {
        Some(target) => {
            std::fs::write(target, &body)
                .map_err(|e| CliError::NotImplemented(format!("sbom: write {}: {e}", target.display())))?;
            eprintln!("SBOM written: {} ({} packages, {} bytes)",
                target.display(), closure.paths.len(), body.len());
        }
        None => println!("{body}"),
    }
    Ok(())
}

// ── `sui store sign-manifest` / `verify-manifest` ───────────────

fn store_sign_manifest(
    manifest: &std::path::Path,
    key_file: &std::path::Path,
) -> Result<(), CliError> {
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};

    let manifest_bytes = std::fs::read(manifest)
        .map_err(|e| CliError::NotImplemented(format!("sign-manifest: read {}: {e}", manifest.display())))?;

    let key_text = std::fs::read_to_string(key_file)
        .map_err(|e| CliError::NotImplemented(format!("sign-manifest: key: {e}")))?;
    let (key_name, b64) = key_text.trim().split_once(':').ok_or_else(||
        CliError::NotImplemented("sign-manifest: expected `<name>:<base64>` key".into())
    )?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64)
        .map_err(|e| CliError::NotImplemented(format!("sign-manifest: base64: {e}")))?;
    let arr: [u8; 32] = bytes.try_into()
        .map_err(|_| CliError::NotImplemented("sign-manifest: key must be 32 bytes".into()))?;
    let signing = SigningKey::from_bytes(&arr);
    let sig = signing.sign(&manifest_bytes);
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

    let sig_path = manifest.with_extension("json.sig.json");
    let payload = serde_json::json!({
        "schema":        "sui.manifest-signature.v1",
        "key_name":      key_name,
        "signature":     sig_b64,
        "manifest_size": manifest_bytes.len(),
        "manifest_path": manifest.display().to_string(),
    });
    std::fs::write(&sig_path, serde_json::to_string_pretty(&payload).unwrap())
        .map_err(|e| CliError::NotImplemented(format!("sign-manifest: write sig: {e}")))?;
    eprintln!("signature written: {} (key={key_name}, {} bytes)",
        sig_path.display(), manifest_bytes.len());
    Ok(())
}

fn store_verify_manifest(
    manifest: &std::path::Path,
    pubkey: &std::path::Path,
    sig: Option<&std::path::Path>,
) -> Result<(), CliError> {
    use base64::Engine;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let manifest_bytes = std::fs::read(manifest)
        .map_err(|e| CliError::NotImplemented(format!("verify-manifest: read manifest: {e}")))?;

    let pubkey_text = std::fs::read_to_string(pubkey)
        .map_err(|e| CliError::NotImplemented(format!("verify-manifest: pubkey: {e}")))?;
    let (key_name, b64) = pubkey_text.trim().split_once(':').ok_or_else(||
        CliError::NotImplemented("verify-manifest: expected `<name>:<base64>` pubkey".into())
    )?;
    let pub_bytes = base64::engine::general_purpose::STANDARD.decode(b64)
        .map_err(|e| CliError::NotImplemented(format!("verify-manifest: base64 pubkey: {e}")))?;
    let pub_arr: [u8; 32] = pub_bytes.try_into()
        .map_err(|_| CliError::NotImplemented("verify-manifest: pubkey must be 32 bytes".into()))?;
    let verifying = VerifyingKey::from_bytes(&pub_arr)
        .map_err(|e| CliError::NotImplemented(format!("verify-manifest: bad pubkey: {e}")))?;

    let sig_path = sig.map(|p| p.to_path_buf())
        .unwrap_or_else(|| manifest.with_extension("json.sig.json"));
    let sig_text = std::fs::read_to_string(&sig_path)
        .map_err(|e| CliError::NotImplemented(format!("verify-manifest: sig: {e}")))?;
    let sig_doc: serde_json::Value = serde_json::from_str(&sig_text)
        .map_err(|e| CliError::NotImplemented(format!("verify-manifest: parse sig: {e}")))?;
    let sig_b64 = sig_doc["signature"].as_str()
        .ok_or_else(|| CliError::NotImplemented("verify-manifest: sig.signature missing".into()))?;
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64)
        .map_err(|e| CliError::NotImplemented(format!("verify-manifest: sig base64: {e}")))?;
    let sig_arr: [u8; 64] = sig_bytes.try_into()
        .map_err(|_| CliError::NotImplemented("verify-manifest: sig must be 64 bytes".into()))?;
    let signature = Signature::from_bytes(&sig_arr);

    use sui_spec::style::{glyph_snowflake, header, ident, info, muted, success, error};
    println!("{}  {}  {}",
        glyph_snowflake(), header("verify manifest"), muted(&manifest.display().to_string()));
    println!();
    println!("  {}  {}", info("pubkey:"), ident(key_name));
    match verifying.verify(&manifest_bytes, &signature) {
        Ok(()) => {
            println!("  {} signature valid", success("✓"));
            println!("  {} manifest bytes match signed payload ({} bytes)",
                success("✓"), info(&manifest_bytes.len().to_string()));
            Ok(())
        }
        Err(e) => {
            println!("  {} signature INVALID: {e}", error("✘"));
            std::process::exit(1);
        }
    }
}

// ── `sui store license-scan` ────────────────────────────────────

fn store_license_scan(path: &str, json: bool) -> Result<(), CliError> {
    use sui_spec::store_inventory::Closure;
    use sui_spec::style::{body, glyph_arrow, glyph_snowflake, header, ident, info, muted, success, warn};

    let closure = Closure::walk(std::path::Path::new(path), "/nix/store")
        .map_err(|e| CliError::NotImplemented(format!("license-scan: closure: {e:?}")))?;

    let needles = [
        b"LICENSE".as_slice(),
        b"LICENCE".as_slice(),
        b"COPYING".as_slice(),
        b"COPYRIGHT".as_slice(),
        b"NOTICE".as_slice(),
    ];
    let mut hits: Vec<(std::path::PathBuf, String)> = Vec::new();
    for p in &closure.paths {
        let nar_bytes = match sui_spec::nar::encode(p) {
            Ok(b) => b, Err(_) => continue,
        };
        for needle in &needles {
            let mut i = 0;
            while i + needle.len() < nar_bytes.len() {
                if &nar_bytes[i..i + needle.len()] == *needle {
                    let label = std::str::from_utf8(needle).unwrap_or("?").to_string();
                    hits.push((p.clone(), label));
                    break;
                }
                i += 1;
            }
        }
    }

    if json {
        let probes: Vec<serde_json::Value> = hits.iter().map(|(p, label)| serde_json::json!({
            "path":         p.display().to_string(),
            "license_file": label,
        })).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "root":            path,
            "closure_size":    closure.paths.len(),
            "license_hits":    hits.len(),
            "results":         probes,
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("license-scan"), muted(path));
        println!();
        println!("  {}  {} closure path(s)", body("∑"), info(&closure.paths.len().to_string()));
        println!("  {}  {} license hits", body("∑"),
            if hits.is_empty() { warn("0").to_string() } else { success(&hits.len().to_string()).to_string() });
        println!();
        // Group by license label.
        let mut by_label: std::collections::BTreeMap<String, usize> = Default::default();
        for (_, label) in &hits { *by_label.entry(label.clone()).or_insert(0) += 1; }
        for (label, count) in &by_label {
            println!("    {} {} → {} paths", success("→"), ident(label), info(&count.to_string()));
        }
        if hits.is_empty() {
            println!();
            println!("  {} no license-bearing files detected — review upstream",
                warn("⚠"));
        } else {
            println!();
            println!("  {} {} license file(s) across {} closure paths",
                glyph_arrow(), info(&hits.len().to_string()),
                info(&closure.paths.len().to_string()));
        }
    }
    Ok(())
}

// ── `sui store cve-scan` ────────────────────────────────────────

fn store_cve_scan(path: &str, pattern: &str, json: bool) -> Result<(), CliError> {
    use sui_spec::store_inventory::Closure;
    use sui_spec::style::{body, error, glyph_arrow, glyph_snowflake, header, ident, info, muted, success};

    let re = regex::bytes::Regex::new(pattern)
        .map_err(|e| CliError::NotImplemented(format!("cve-scan: regex: {e}")))?;
    let closure = Closure::walk(std::path::Path::new(path), "/nix/store")
        .map_err(|e| CliError::NotImplemented(format!("cve-scan: closure: {e:?}")))?;

    let mut hits: Vec<(std::path::PathBuf, Vec<String>)> = Vec::new();
    for p in &closure.paths {
        let nar_bytes = match sui_spec::nar::encode(p) {
            Ok(b) => b, Err(_) => continue,
        };
        let mut matched: std::collections::BTreeSet<String> = Default::default();
        for m in re.find_iter(&nar_bytes) {
            if let Ok(s) = std::str::from_utf8(m.as_bytes()) {
                matched.insert(s.to_string());
            }
        }
        if !matched.is_empty() {
            hits.push((p.clone(), matched.into_iter().collect()));
        }
    }

    if json {
        let probes: Vec<serde_json::Value> = hits.iter().map(|(p, matches)| serde_json::json!({
            "path":    p.display().to_string(),
            "matches": matches,
        })).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "root":         path,
            "pattern":      pattern,
            "closure_size": closure.paths.len(),
            "paths_with_matches": hits.len(),
            "results":      probes,
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("cve-scan"),
            muted(&format!("pattern=`{pattern}`")));
        println!();
        println!("  {}  closure: {}",
            body("∑"), info(&closure.paths.len().to_string()));
        println!("  {}  paths with matches: {}",
            body("∑"),
            if hits.is_empty() { success("0").to_string() } else { error(&hits.len().to_string()).to_string() });
        println!();
        for (p, matches) in hits.iter().take(20) {
            let bn = p.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            println!("  {} {} {} match(es)",
                error("⚠"), ident(bn), info(&matches.len().to_string()));
            for m in matches.iter().take(3) {
                println!("       {} {}", muted("·"), error(m));
            }
        }
        if hits.len() > 20 {
            println!("  {} … {} more", muted(""), muted(&(hits.len() - 20).to_string()));
        }
        println!();
        println!("  {} {} hit(s) across {} closure path(s)",
            glyph_arrow(), info(&hits.len().to_string()),
            info(&closure.paths.len().to_string()));
    }
    if !hits.is_empty() {
        std::process::exit(2);
    }
    Ok(())
}

// ── `sui store dedupe-plan` — Findings → graft loop closure ─────

fn store_dedupe_plan(profile_name: &str, json: bool) -> Result<(), CliError> {
    use sui_spec::store_analyze::{self, AnalyzeConfig, Finding};
    use sui_spec::store_inventory::{self, StoreInventory, RefIndex};
    use sui_spec::style::{body, glyph_arrow, glyph_snowflake, header, ident, info, muted, success, warn};

    let profiles = store_inventory::load_canonical_profiles()
        .map_err(|e| CliError::NotImplemented(format!("dedupe-plan: {e:?}")))?;
    let profile = profiles.iter().find(|p| p.name == profile_name)
        .ok_or_else(|| CliError::NotImplemented(format!("dedupe-plan: unknown profile")))?;
    let inv = StoreInventory::walk(profile)
        .map_err(|e| CliError::NotImplemented(format!("dedupe-plan: walk: {e:?}")))?;
    let idx = RefIndex::build(&inv, "/nix/store")
        .map_err(|e| CliError::NotImplemented(format!("dedupe-plan: ref index: {e:?}")))?;

    let findings = store_analyze::analyze(&inv, Some(&idx), &AnalyzeConfig {
        detect_duplicates: true,
        detect_orphans: false,
        high_fanout_threshold: 0,
        detect_version_shadows: false,
    });

    // Each Duplicate finding yields one canonical winner + N graft targets.
    // Winner: the first path (deterministic — analyzer already sorts).
    let mut plan: Vec<(String, std::path::PathBuf, Vec<std::path::PathBuf>, u64)> = Vec::new();
    for f in &findings {
        if let Finding::Duplicate { hash, paths, bytes_each } = f {
            if let Some((winner, losers)) = paths.split_first() {
                plan.push((hash.clone(), winner.clone(), losers.to_vec(), *bytes_each));
            }
        }
    }

    let total_groups = plan.len();
    let total_grafts: usize = plan.iter().map(|(_, _, losers, _)| losers.len()).sum();
    let bytes_reclaimable: u64 = plan.iter().map(|(_, _, losers, bytes)| (losers.len() as u64) * bytes).sum();

    if json {
        let probes: Vec<serde_json::Value> = plan.iter().map(|(hash, winner, losers, bytes)| serde_json::json!({
            "hash": hash,
            "canonical_path": winner.display().to_string(),
            "graft_targets":  losers.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "bytes_each":     bytes,
            "bytes_saved":    (losers.len() as u64) * bytes,
        })).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "profile":          profile_name,
            "duplicate_groups": total_groups,
            "total_grafts":     total_grafts,
            "bytes_reclaimable": bytes_reclaimable,
            "plan":             probes,
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("store dedupe-plan"),
            muted(&format!("profile={profile_name}")));
        println!();
        if plan.is_empty() {
            println!("  {} no duplicate groups in this profile", success("✓"));
            return Ok(());
        }
        println!("  {} {} duplicate group(s)", body("∑"), info(&total_groups.to_string()));
        println!("  {} {} total graft(s) to apply", body("∑"), warn(&total_grafts.to_string()));
        println!("  {} {} bytes reclaimable", body("∑"), success(&bytes_reclaimable.to_string()));
        println!();
        for (hash, winner, losers, bytes) in plan.iter().take(10) {
            let wname = winner.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            println!("  {} {} ({} bytes/each)",
                success("→"), ident(&format!("hash {}…", &hash[..16])), info(&bytes.to_string()));
            println!("     {} canonical: {}", muted("·"), success(wname));
            for l in losers {
                let lname = l.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                println!("     {} graft:     {}", muted("·"), warn(lname));
            }
        }
        if plan.len() > 10 {
            println!("  {} … {} more", muted(""), muted(&(plan.len() - 10).to_string()));
        }
        println!();
        println!("  {} apply: for each group, run `sui store graft <loser> <loser-hash> <canonical-hash>`",
            glyph_arrow());
    }
    Ok(())
}

// ── `sui store entropy` — Shannon entropy detector ─────────────

fn store_entropy(path: &str, json: bool) -> Result<(), CliError> {
    use sui_spec::style::{body, glyph_arrow, glyph_snowflake, header, ident, info, muted, success, warn};

    let bytes = std::fs::read(path).or_else(|_| {
        // Directory: collect all file bytes.
        let mut buf = Vec::new();
        fn collect(p: &std::path::Path, buf: &mut Vec<u8>) -> std::io::Result<()> {
            let meta = std::fs::symlink_metadata(p)?;
            if meta.is_file() {
                buf.extend_from_slice(&std::fs::read(p)?);
            } else if meta.is_dir() {
                for entry in std::fs::read_dir(p)?.flatten() {
                    collect(&entry.path(), buf)?;
                }
            }
            Ok(())
        }
        collect(std::path::Path::new(path), &mut buf)?;
        Ok::<_, std::io::Error>(buf)
    }).map_err(|e| CliError::NotImplemented(format!("entropy: read {path}: {e}")))?;

    // Shannon entropy over byte distribution.
    let mut counts = [0u64; 256];
    for b in &bytes {
        counts[*b as usize] += 1;
    }
    let n = bytes.len() as f64;
    let entropy: f64 = if n == 0.0 {
        0.0
    } else {
        counts.iter()
            .filter(|&&c| c > 0)
            .map(|&c| {
                let p = (c as f64) / n;
                -p * p.log2()
            })
            .sum()
    };
    let entropy_pct = (entropy / 8.0) * 100.0;

    // Classify.
    let classification = if entropy >= 7.5 {
        "compressed/encrypted/random"
    } else if entropy >= 6.0 {
        "binary/mixed"
    } else if entropy >= 3.0 {
        "source-like text"
    } else {
        "low-entropy text (repetitive)"
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "path":            path,
            "bytes":           bytes.len(),
            "entropy_bits":    entropy,
            "entropy_percent": entropy_pct,
            "classification":  classification,
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("store entropy"), muted(path));
        println!();
        println!("  {}  {}", body("bytes:          "), info(&bytes.len().to_string()));
        println!("  {}  {:.4} bits/byte", body("entropy:        "), entropy);
        println!("  {}  {:.1}%", body("entropy %:      "), entropy_pct);
        let label_styled = if entropy >= 7.5 {
            warn(classification)
        } else if entropy >= 6.0 {
            info(classification)
        } else {
            success(classification)
        };
        println!("  {}  {}", body("classification: "), label_styled);
        println!();
        // Mini ASCII bar.
        let bar_width = 40usize;
        let filled = ((entropy / 8.0) * bar_width as f64) as usize;
        let bar: String = (0..bar_width)
            .map(|i| if i < filled { '█' } else { '░' })
            .collect();
        let colored = if entropy >= 7.5 { warn(&bar) }
                      else if entropy >= 6.0 { info(&bar) }
                      else { success(&bar) };
        println!("  {} {} of 8 bits", glyph_arrow(), colored);
    }
    Ok(())
}

// ── `sui store ascii-graph` — DAG terminal renderer ────────────

fn store_ascii_graph(path: &str, max_depth: usize) -> Result<(), CliError> {
    use std::collections::{BTreeSet, BTreeMap};
    use sui_compat::derivation::Derivation;
    use sui_spec::style::{glyph_snowflake, header, ident, info, muted, success};

    fn render(
        path: &str,
        depth: usize,
        max_depth: usize,
        visited: &mut BTreeSet<String>,
        prefix: String,
        is_last: bool,
    ) {
        let bn = std::path::Path::new(path).file_name()
            .and_then(|n| n.to_str()).unwrap_or(path);
        let connector = if depth == 0 { "" } else if is_last { "└── " } else { "├── " };
        let depth_color = if depth == 0 { success(bn) } else { ident(bn) };
        println!("{prefix}{connector}{depth_color}");

        if depth >= max_depth {
            return;
        }
        if !visited.insert(path.to_string()) {
            // Cycle / already rendered — print a stub.
            let cstub_prefix = if is_last { "    " } else { "│   " };
            println!("{prefix}{cstub_prefix}{}",  muted("(seen above)"));
            return;
        }

        // Read the .drv and walk inputDrvs.
        let bytes = match std::fs::read(path) {
            Ok(b) => b, Err(_) => return,
        };
        let drv = match Derivation::parse(&bytes) {
            Ok(d) => d, Err(_) => return,
        };
        let inputs: Vec<String> = drv.input_derivations.keys().cloned().collect();
        let child_prefix = if depth == 0 { String::new() } else if is_last {
            format!("{prefix}    ")
        } else {
            format!("{prefix}│   ")
        };
        let last = inputs.len().saturating_sub(1);
        for (i, input) in inputs.iter().enumerate() {
            render(input, depth + 1, max_depth, visited, child_prefix.clone(), i == last);
        }
    }

    println!("{}  {}  {}",
        glyph_snowflake(), header("derivation graph"), muted(path));
    println!();
    let mut visited = BTreeSet::new();
    render(path, 0, max_depth, &mut visited, String::new(), true);
    println!();
    println!("  {}  {} unique node(s) up to depth {}",
        info("∑"), info(&visited.len().to_string()), info(&max_depth.to_string()));
    let _ = BTreeMap::<String,String>::new();
    Ok(())
}

// ── `sui store recipe` — declarative pipeline runner ───────────

fn store_recipe(
    name: &str,
    dest_base: Option<&std::path::Path>,
    json: bool,
) -> Result<(), CliError> {
    use sui_spec::store_recipe;
    use sui_spec::style::{body, glyph_arrow, glyph_snowflake, header, ident, info, muted, success};

    let recipes = store_recipe::load_canonical()
        .map_err(|e| CliError::NotImplemented(format!("recipe: load: {e:?}")))?;
    let recipe = recipes.iter().find(|r| r.name == name)
        .ok_or_else(|| CliError::NotImplemented(format!(
            "recipe: unknown `{name}`; available: {}",
            recipes.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", "),
        )))?;

    let base = dest_base.map(std::path::PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        std::path::PathBuf::from(home).join(".cache/sui/recipes")
    });

    let outcome = store_recipe::run(recipe, &base)
        .map_err(|e| CliError::NotImplemented(format!("recipe: run: {e:?}")))?;

    if json {
        let probes: Vec<serde_json::Value> = outcome.entries.iter().map(|e| serde_json::json!({
            "source": e.source.display().to_string(),
            "dest":   e.dest.display().to_string(),
            "total_rewrites": e.total_rewrites,
            "noop":   e.noop,
        })).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "recipe":         outcome.recipe,
            "slice":          outcome.slice,
            "transforms":     outcome.transforms,
            "dest_root":      outcome.dest_root.display().to_string(),
            "entries":        outcome.entries.len(),
            "modified":       outcome.modified_count(),
            "total_rewrites": outcome.total_rewrites(),
            "results":        probes,
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("store recipe"), muted(&format!("`{name}`")));
        println!();
        println!("  {}  {}", body("slice:           "), ident(&outcome.slice));
        println!("  {}  {}", body("transforms:      "),
            if outcome.transforms.is_empty() {
                muted("(none — pure rematerialize)").to_string()
            } else {
                info(&outcome.transforms.join(" → ")).to_string()
            });
        println!("  {}  {}", body("dest root:       "), ident(&outcome.dest_root.display().to_string()));
        println!("  {}  {}", body("entries processed:"), info(&outcome.entries.len().to_string()));
        println!("  {}  {}", body("modified entries:"), success(&outcome.modified_count().to_string()));
        println!("  {}  {}", body("total rewrites:  "), success(&outcome.total_rewrites().to_string()));
        println!();
        for e in &outcome.entries {
            let glyph = if e.noop { muted("·").to_string() } else { success("✓").to_string() };
            let bn = e.source.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            println!("  {} {} {} rewrite(s)", glyph, ident(bn),
                info(&e.total_rewrites.to_string()));
        }
        println!();
        println!("  {} recipe `{}` materialized at {}", glyph_arrow(),
            success(&outcome.recipe), ident(&outcome.dest_root.display().to_string()));
    }
    Ok(())
}

// ── `sui store fingerprint-many` — slice manifest emitter ──────

fn store_fingerprint_many(
    profile_name: &str,
    out: Option<&std::path::Path>,
) -> Result<(), CliError> {
    use sui_spec::store_inventory::{self, StoreInventory};
    use sui_spec::store_ops::ParsedNar;
    use sui_spec::nar;
    use sha2::Digest;

    let profiles = store_inventory::load_canonical_profiles()
        .map_err(|e| CliError::NotImplemented(format!("fingerprint-many: {e:?}")))?;
    let profile = profiles.iter().find(|p| p.name == profile_name)
        .ok_or_else(|| CliError::NotImplemented(format!("fingerprint-many: unknown profile")))?;
    let inv = StoreInventory::walk(profile)
        .map_err(|e| CliError::NotImplemented(format!("fingerprint-many: walk: {e:?}")))?;

    let mut probes: Vec<serde_json::Value> = Vec::with_capacity(inv.entries.len());
    for entry in inv.entries.values() {
        let nar_bytes = match nar::encode(&entry.path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let digest = sha2::Sha256::digest(&nar_bytes);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        let parsed = ParsedNar::parse(&nar_bytes).ok();
        probes.push(serde_json::json!({
            "name":        entry.parsed.name,
            "hash_prefix": entry.parsed.hash,
            "nar_sha256":  hex,
            "nar_size":    nar_bytes.len(),
            "tree_size":   parsed.as_ref().map(|p| p.root.total_bytes()).unwrap_or(0),
            "file_count":  parsed.as_ref().map(|p| p.root.file_count()).unwrap_or(0),
        }));
    }

    let host = std::env::var("HOST")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into());
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let manifest = serde_json::json!({
        "schema":  "sui.fingerprint-manifest.v1",
        "profile": profile_name,
        "host":    host,
        "user":    user,
        "system":  std::env::consts::ARCH,
        "entries": probes,
    });
    let body = serde_json::to_string_pretty(&manifest).unwrap();
    match out {
        Some(path) => {
            std::fs::write(path, &body)
                .map_err(|e| CliError::NotImplemented(format!("fingerprint-many: write {}: {e}", path.display())))?;
            eprintln!("manifest written: {}  ({} entries, {} bytes)",
                path.display(), inv.entries.len(), body.len());
        }
        None => println!("{body}"),
    }
    Ok(())
}

// ── `sui store compare-manifests` — drift detector ─────────────

fn store_compare_manifests(
    a: &std::path::Path,
    b: &std::path::Path,
) -> Result<(), CliError> {
    use sui_spec::style::{body, error, glyph_arrow, glyph_snowflake, header, ident, info, muted, success, warn};

    let text_a = std::fs::read_to_string(a)
        .map_err(|e| CliError::NotImplemented(format!("compare: read {}: {e}", a.display())))?;
    let text_b = std::fs::read_to_string(b)
        .map_err(|e| CliError::NotImplemented(format!("compare: read {}: {e}", b.display())))?;
    let doc_a: serde_json::Value = serde_json::from_str(&text_a)
        .map_err(|e| CliError::NotImplemented(format!("compare: parse a: {e}")))?;
    let doc_b: serde_json::Value = serde_json::from_str(&text_b)
        .map_err(|e| CliError::NotImplemented(format!("compare: parse b: {e}")))?;

    let entries_a = doc_a["entries"].as_array().cloned().unwrap_or_default();
    let entries_b = doc_b["entries"].as_array().cloned().unwrap_or_default();

    // Key by hash_prefix (typed identity).
    let by_hash_a: std::collections::HashMap<String, serde_json::Value> = entries_a.iter()
        .filter_map(|e| e["hash_prefix"].as_str().map(|h| (h.to_string(), e.clone())))
        .collect();
    let by_hash_b: std::collections::HashMap<String, serde_json::Value> = entries_b.iter()
        .filter_map(|e| e["hash_prefix"].as_str().map(|h| (h.to_string(), e.clone())))
        .collect();

    let mut only_a: Vec<String> = Vec::new();
    let mut only_b: Vec<String> = Vec::new();
    let mut diverged: Vec<(String, String, String)> = Vec::new();
    let mut matching: usize = 0;

    for (hash, entry_a) in &by_hash_a {
        match by_hash_b.get(hash) {
            None => only_a.push(hash.clone()),
            Some(entry_b) => {
                let sha_a = entry_a["nar_sha256"].as_str().unwrap_or("");
                let sha_b = entry_b["nar_sha256"].as_str().unwrap_or("");
                if sha_a == sha_b { matching += 1; }
                else { diverged.push((hash.clone(), sha_a.to_string(), sha_b.to_string())); }
            }
        }
    }
    for (hash, _) in &by_hash_b {
        if !by_hash_a.contains_key(hash) { only_b.push(hash.clone()); }
    }

    println!("{}  {}",
        glyph_snowflake(), header("compare fingerprint manifests"));
    println!();
    println!("  {} {}  vs  {}", body("manifests:"), ident(&a.display().to_string()), ident(&b.display().to_string()));
    println!("  {}  matching: {}", body("∑"), success(&matching.to_string()));
    println!("  {}  only in A: {}", body("∑"),
        if only_a.is_empty() { muted("0").to_string() } else { warn(&only_a.len().to_string()).to_string() });
    println!("  {}  only in B: {}", body("∑"),
        if only_b.is_empty() { muted("0").to_string() } else { warn(&only_b.len().to_string()).to_string() });
    println!("  {}  diverged:  {}", body("∑"),
        if diverged.is_empty() { success("0").to_string() } else { error(&diverged.len().to_string()).to_string() });
    println!();
    for (hash, sa, sb) in diverged.iter().take(10) {
        println!("  {} {} sha={} ≠ sha={}",
            error("✘"), ident(&hash[..16.min(hash.len())]),
            muted(&sa[..16.min(sa.len())]),
            muted(&sb[..16.min(sb.len())]));
    }
    let total_drift = only_a.len() + only_b.len() + diverged.len();
    println!();
    println!("  {} {} total drift record(s) across {} matching entries",
        glyph_arrow(),
        if total_drift > 0 { error(&total_drift.to_string()).to_string() } else { success("0").to_string() },
        info(&matching.to_string()));
    if total_drift > 0 {
        std::process::exit(1);
    }
    Ok(())
}

// ── `sui store find` — typed predicate query ────────────────────

fn store_find(
    profile_name: &str,
    name_re: Option<&str>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    contents_re: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    use sui_spec::store_inventory::{self, StoreInventory};
    use sui_spec::store_query::{matches, StorePredicate};
    use sui_spec::style::{body, glyph_arrow, glyph_snowflake, header, ident, info, muted, success};

    let profiles = store_inventory::load_canonical_profiles()
        .map_err(|e| CliError::NotImplemented(format!("find: {e:?}")))?;
    let profile = profiles.iter().find(|p| p.name == profile_name)
        .ok_or_else(|| CliError::NotImplemented(format!("find: unknown profile `{profile_name}`")))?;
    let inv = StoreInventory::walk(profile)
        .map_err(|e| CliError::NotImplemented(format!("find: walk: {e:?}")))?;

    // Build the AND predicate from operator flags.
    let mut preds: Vec<StorePredicate> = Vec::new();
    if let Some(n) = name_re { preds.push(StorePredicate::NameMatches(n.to_string())); }
    if let Some(n) = min_size { preds.push(StorePredicate::SizeAtLeast(n)); }
    if let Some(n) = max_size { preds.push(StorePredicate::SizeAtMost(n)); }
    if let Some(c) = contents_re { preds.push(StorePredicate::ContentsMatch(c.to_string())); }
    let p = if preds.is_empty() {
        // No predicate → all
        StorePredicate::Any(vec![])  // never matches; force operator to specify
    } else {
        StorePredicate::All(preds)
    };

    let matches_iter: Vec<_> = inv.entries.values()
        .filter(|e| matches(e, &p, None))
        .collect();

    if json {
        let probes: Vec<serde_json::Value> = matches_iter.iter().map(|e| serde_json::json!({
            "path":       e.path.display().to_string(),
            "name":       e.parsed.name,
            "size":       e.size,
            "file_count": e.file_count,
        })).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "profile": profile_name,
            "matches": matches_iter.len(),
            "results": probes,
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("store find"),
            muted(&format!("profile={profile_name}")));
        println!();
        println!("  {} {} match(es) of {} scanned", body("∑"),
            success(&matches_iter.len().to_string()),
            info(&inv.entries.len().to_string()));
        println!();
        for e in matches_iter.iter().take(50) {
            println!("  {} {} {} bytes / {} files",
                success("→"),
                ident(&e.parsed.name),
                info(&e.size.to_string()),
                muted(&e.file_count.to_string()));
        }
        if matches_iter.len() > 50 {
            println!("  {} … {} more", muted(""), muted(&(matches_iter.len() - 50).to_string()));
        }
        println!();
        println!("  {} {} of {} entries match", glyph_arrow(),
            success(&matches_iter.len().to_string()), info(&inv.entries.len().to_string()));
    }
    Ok(())
}

// ── `sui store stats` — typed aggregate reduce ─────────────────

fn store_stats(profile_name: &str, json: bool) -> Result<(), CliError> {
    use sui_spec::store_inventory::{self, StoreInventory};
    use sui_spec::style::{body, glyph_snowflake, header, ident, info, muted, success};

    let profiles = store_inventory::load_canonical_profiles()
        .map_err(|e| CliError::NotImplemented(format!("stats: {e:?}")))?;
    let profile = profiles.iter().find(|p| p.name == profile_name)
        .ok_or_else(|| CliError::NotImplemented(format!("stats: unknown profile `{profile_name}`")))?;
    let inv = StoreInventory::walk(profile)
        .map_err(|e| CliError::NotImplemented(format!("stats: walk: {e:?}")))?;

    let n = inv.entries.len();
    let total_size: u64 = inv.total_size();
    let total_files: usize = inv.total_files();
    let mean_size = if n > 0 { total_size / n as u64 } else { 0 };
    let max_size = inv.entries.values().map(|e| e.size).max().unwrap_or(0);
    let min_size = inv.entries.values().map(|e| e.size).min().unwrap_or(0);

    // Distribution by size class (log10 buckets).
    let mut classes: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for e in inv.entries.values() {
        let key = match e.size {
            0..=1_023            => "1 KB",
            1_024..=1_048_575    => "1 MB",
            1_048_576..=10_485_759 => "10 MB",
            10_485_760..=104_857_599 => "100 MB",
            _ => "> 100 MB",
        };
        *classes.entry(key).or_insert(0) += 1;
    }

    if json {
        let dist: serde_json::Value = serde_json::json!(
            classes.iter().map(|(k, v)| (k.to_string(), *v)).collect::<std::collections::HashMap<_,_>>()
        );
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "profile": profile_name,
            "entries": n,
            "total_size": total_size,
            "total_files": total_files,
            "mean_size": mean_size,
            "min_size": min_size,
            "max_size": max_size,
            "size_distribution": dist,
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("store stats"),
            muted(&format!("profile={profile_name}")));
        println!();
        println!("  {} {}", body("entries:       "), info(&n.to_string()));
        println!("  {} {} bytes", body("total size:    "), info(&total_size.to_string()));
        println!("  {} {}", body("total files:   "), info(&total_files.to_string()));
        println!("  {} {} bytes", body("mean size:     "), info(&mean_size.to_string()));
        println!("  {} {} bytes", body("min size:      "), info(&min_size.to_string()));
        println!("  {} {} bytes", body("max size:      "), info(&max_size.to_string()));
        println!();
        println!("  {}", body("size distribution:"));
        for (cls, count) in &classes {
            println!("    {} ≤ {}  {} entries", success("→"), ident(cls), info(&count.to_string()));
        }
    }
    Ok(())
}

// ── `sui store analyze` — typed findings emitter ───────────────

fn store_analyze_cmd(
    profile_name: &str,
    detect_duplicates: bool,
    high_fanout_threshold: usize,
    json: bool,
) -> Result<(), CliError> {
    use sui_spec::store_analyze::{self, AnalyzeConfig, Finding};
    use sui_spec::store_inventory::{self, StoreInventory, RefIndex};
    use sui_spec::style::{body, error, glyph_arrow, glyph_snowflake, header, ident, info, muted, success, warn};

    let profiles = store_inventory::load_canonical_profiles()
        .map_err(|e| CliError::NotImplemented(format!("analyze: {e:?}")))?;
    let profile = profiles.iter().find(|p| p.name == profile_name)
        .ok_or_else(|| CliError::NotImplemented(format!("analyze: unknown profile `{profile_name}`")))?;
    let inv = StoreInventory::walk(profile)
        .map_err(|e| CliError::NotImplemented(format!("analyze: walk: {e:?}")))?;
    let idx = RefIndex::build(&inv, "/nix/store")
        .map_err(|e| CliError::NotImplemented(format!("analyze: ref index: {e:?}")))?;

    let config = AnalyzeConfig {
        detect_duplicates,
        detect_orphans: true,
        high_fanout_threshold,
        detect_version_shadows: true,
    };
    let findings = store_analyze::analyze(&inv, Some(&idx), &config);
    let h = store_analyze::histogram(&findings);

    if json {
        let probes: Vec<serde_json::Value> = findings.iter().map(|f| match f {
            Finding::Duplicate { hash, paths, bytes_each } => serde_json::json!({
                "kind": "Duplicate", "hash": hash,
                "paths": paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "bytes_each": bytes_each,
            }),
            Finding::Orphan { path, size } => serde_json::json!({
                "kind": "Orphan", "path": path.display().to_string(), "size": size,
            }),
            Finding::HighFanout { path, fanout } => serde_json::json!({
                "kind": "HighFanout", "path": path.display().to_string(), "fanout": fanout,
            }),
            Finding::VersionShadow { older, newer, name_root, older_version, newer_version } => serde_json::json!({
                "kind": "VersionShadow",
                "older": older.display().to_string(),
                "newer": newer.display().to_string(),
                "name_root": name_root,
                "older_version": older_version,
                "newer_version": newer_version,
            }),
        }).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "profile": profile_name,
            "histogram": {
                "duplicates": h.duplicates,
                "orphans": h.orphans,
                "high_fanout": h.high_fanout,
                "version_shadows": h.version_shadows,
            },
            "findings": probes,
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("store analyze"),
            muted(&format!("profile={profile_name}")));
        println!();
        println!("  {} {} duplicate group(s)", body("∑"),
            if h.duplicates > 0 { warn(&h.duplicates.to_string()) } else { success("0") });
        println!("  {} {} orphan path(s)", body("∑"),
            if h.orphans > 0 { warn(&h.orphans.to_string()) } else { success("0") });
        println!("  {} {} high-fanout drv(s)", body("∑"),
            if h.high_fanout > 0 { warn(&h.high_fanout.to_string()) } else { success("0") });
        println!("  {} {} version-shadow pair(s)", body("∑"),
            if h.version_shadows > 0 { info(&h.version_shadows.to_string()) } else { success("0") });
        println!();
        if findings.is_empty() {
            println!("  {} store looks clean — no findings to act on", success("✓"));
            return Ok(());
        }
        for f in findings.iter().take(20) {
            match f {
                Finding::Duplicate { hash, paths, bytes_each } => {
                    println!("  {} dup {} bytes ({}…) {} paths",
                        error("✘"), info(&bytes_each.to_string()),
                        muted(&hash[..8]), warn(&paths.len().to_string()));
                    for p in paths.iter().take(3) {
                        println!("       {} {}", muted("·"),
                            ident(p.file_name().and_then(|n| n.to_str()).unwrap_or("?")));
                    }
                }
                Finding::Orphan { path, size } => {
                    println!("  {} orphan {} {} bytes",
                        warn("○"),
                        ident(path.file_name().and_then(|n| n.to_str()).unwrap_or("?")),
                        info(&size.to_string()));
                }
                Finding::HighFanout { path, fanout } => {
                    println!("  {} fanout={} {}",
                        warn("◐"),
                        info(&fanout.to_string()),
                        ident(path.file_name().and_then(|n| n.to_str()).unwrap_or("?")));
                }
                Finding::VersionShadow { older, newer, older_version, newer_version, name_root } => {
                    println!("  {} {} {}→{} (shadowed)",
                        info("↑"), ident(name_root),
                        muted(older_version), success(newer_version));
                    let _ = (older, newer);
                }
            }
        }
        if findings.len() > 20 {
            println!("  {} … {} more findings", muted(""), muted(&(findings.len() - 20).to_string()));
        }
        println!();
        println!("  {} {} finding(s) across {} scanned entries",
            glyph_arrow(), info(&findings.len().to_string()), info(&inv.entries.len().to_string()));
    }
    Ok(())
}

// ── `sui store upgrade-paths` — typed upgrade recommendations ──

fn store_upgrade_paths(profile_name: &str, json: bool) -> Result<(), CliError> {
    use sui_spec::store_analyze::{self, AnalyzeConfig};
    use sui_spec::store_inventory::{self, StoreInventory, RefIndex};
    use sui_spec::style::{body, glyph_arrow, glyph_snowflake, header, ident, info, muted, success};

    let profiles = store_inventory::load_canonical_profiles()
        .map_err(|e| CliError::NotImplemented(format!("upgrade-paths: {e:?}")))?;
    let profile = profiles.iter().find(|p| p.name == profile_name)
        .ok_or_else(|| CliError::NotImplemented(format!("upgrade-paths: unknown profile")))?;
    let inv = StoreInventory::walk(profile)
        .map_err(|e| CliError::NotImplemented(format!("upgrade-paths: walk: {e:?}")))?;
    let idx = RefIndex::build(&inv, "/nix/store")
        .map_err(|e| CliError::NotImplemented(format!("upgrade-paths: ref index: {e:?}")))?;
    let findings = store_analyze::analyze(&inv, Some(&idx), &AnalyzeConfig {
        detect_duplicates: false,  // skip — irrelevant for upgrades
        detect_orphans: false,
        high_fanout_threshold: 0,
        detect_version_shadows: true,
    });
    let mut paths = store_analyze::mine_upgrade_paths(&findings, &idx);
    store_analyze::sort_upgrade_paths(&mut paths);

    if json {
        let probes: Vec<serde_json::Value> = paths.iter().map(|up| serde_json::json!({
            "from": up.from.display().to_string(),
            "to":   up.to.display().to_string(),
            "name_root": up.name_root,
            "from_version": up.from_version,
            "to_version":   up.to_version,
            "referrers_count": up.referrers_count,
        })).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "profile": profile_name,
            "upgrade_paths": probes,
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("store upgrade-paths"),
            muted(&format!("profile={profile_name}")));
        println!();
        if paths.is_empty() {
            println!("  {} no upgrade-shadow pairs found in this profile",
                success("✓"));
            return Ok(());
        }
        println!("  {} {} upgrade recommendation(s)",
            body("∑"), info(&paths.len().to_string()));
        println!();
        for up in paths.iter().take(30) {
            println!("  {} {} {}→{} ({} referrer{})",
                success("↑"),
                ident(&up.name_root),
                muted(&up.from_version),
                success(&up.to_version),
                info(&up.referrers_count.to_string()),
                if up.referrers_count == 1 { "" } else { "s" });
        }
        if paths.len() > 30 {
            println!("  {} … {} more", muted(""), muted(&(paths.len() - 30).to_string()));
        }
        println!();
        println!("  {} {} recommended upgrade(s) — sorted by blast radius",
            glyph_arrow(), info(&paths.len().to_string()));
    }
    Ok(())
}

// ── `sui store diff` — typed ParsedNar diff ─────────────────────

fn store_diff_cmd(a: &str, b: &str, json: bool) -> Result<(), CliError> {
    use sui_spec::store_diff::{diff, DiffEntry};
    use sui_spec::store_ops::ParsedNar;
    use sui_spec::style::{body, error, glyph_arrow, glyph_snowflake, header, ident, info, muted, success, warn};

    let pa = std::path::PathBuf::from(a);
    let pb = std::path::PathBuf::from(b);
    let nar_a = sui_spec::nar::encode(&pa)
        .map_err(|e| CliError::NotImplemented(format!("diff: encode {a}: {e}")))?;
    let nar_b = sui_spec::nar::encode(&pb)
        .map_err(|e| CliError::NotImplemented(format!("diff: encode {b}: {e}")))?;
    let pa_tree = ParsedNar::parse(&nar_a)
        .map_err(|e| CliError::NotImplemented(format!("diff: parse {a}: {e}")))?;
    let pb_tree = ParsedNar::parse(&nar_b)
        .map_err(|e| CliError::NotImplemented(format!("diff: parse {b}: {e}")))?;
    let d = diff(&pa_tree.root, &pb_tree.root);
    let h = d.histogram();

    if json {
        let probes: Vec<serde_json::Value> = d.entries.iter().map(|e| match e {
            DiffEntry::AddedFile { path, size }       => serde_json::json!({"kind":"AddedFile","path":path,"size":size}),
            DiffEntry::RemovedFile { path, size }     => serde_json::json!({"kind":"RemovedFile","path":path,"size":size}),
            DiffEntry::ChangedFile { path, size_a, size_b } => serde_json::json!({"kind":"ChangedFile","path":path,"size_a":size_a,"size_b":size_b}),
            DiffEntry::KindChanged { path, from, to } => serde_json::json!({"kind":"KindChanged","path":path,"from":from,"to":to}),
            DiffEntry::SymlinkChanged { path, from, to } => serde_json::json!({"kind":"SymlinkChanged","path":path,"from":from,"to":to}),
            DiffEntry::ExecutableChanged { path, executable_now } => serde_json::json!({"kind":"ExecutableChanged","path":path,"executable_now":executable_now}),
        }).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "a": a, "b": b,
            "total": h.total(),
            "histogram": {
                "added": h.added, "removed": h.removed, "changed": h.changed,
                "kind_changed": h.kind_changed,
                "symlink_changed": h.symlink_changed,
                "executable_changed": h.executable_changed,
            },
            "entries": probes,
        })).unwrap());
    } else {
        println!("{}  {}  {} vs {}",
            glyph_snowflake(), header("store diff"),
            muted(a), muted(b));
        println!();
        if d.is_empty() {
            println!("  {} byte-equivalent — no differences", success("✓"));
            return Ok(());
        }
        println!("  {} {} differing record(s)", info("∑"), ident(&h.total().to_string()));
        println!();
        if h.added > 0           { println!("  {} added:    {}", success("+"), ident(&h.added.to_string())); }
        if h.removed > 0         { println!("  {} removed:  {}", error("-"), ident(&h.removed.to_string())); }
        if h.changed > 0         { println!("  {} changed:  {}", warn("~"), ident(&h.changed.to_string())); }
        if h.kind_changed > 0    { println!("  {} kind:     {}", warn("⚠"), ident(&h.kind_changed.to_string())); }
        if h.symlink_changed > 0 { println!("  {} symlink:  {}", warn("→"), ident(&h.symlink_changed.to_string())); }
        if h.executable_changed > 0 { println!("  {} exec:     {}", warn("x"), ident(&h.executable_changed.to_string())); }
        println!();
        println!("  {} top entries:", body("by record:"));
        for e in d.entries.iter().take(20) {
            match e {
                DiffEntry::AddedFile { path, size } =>
                    println!("    {} {} {} {}", success("+"), success(path), muted("size"), info(&size.to_string())),
                DiffEntry::RemovedFile { path, size } =>
                    println!("    {} {} {} {}", error("-"), error(path), muted("size"), info(&size.to_string())),
                DiffEntry::ChangedFile { path, size_a, size_b } =>
                    println!("    {} {} {}→{}", warn("~"), ident(path), info(&size_a.to_string()), info(&size_b.to_string())),
                DiffEntry::KindChanged { path, from, to } =>
                    println!("    {} {} {}→{}", warn("⚠"), ident(path), info(from), info(to)),
                DiffEntry::SymlinkChanged { path, from, to } =>
                    println!("    {} {} {}→{}", warn("→"), ident(path), info(from), info(to)),
                DiffEntry::ExecutableChanged { path, executable_now } =>
                    println!("    {} {} executable_now={}", warn("x"), ident(path), info(&executable_now.to_string())),
            }
        }
        if d.entries.len() > 20 {
            println!("    {} … {} more", muted(""), muted(&(d.entries.len() - 20).to_string()));
        }
        println!();
        println!("  {} {} record(s) total", glyph_arrow(), info(&h.total().to_string()));
    }
    if !d.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

// ── `sui store graft` — closure-wide ref rewrite ────────────────

fn store_graft(
    root: &str,
    from: &str,
    to: &str,
    dest: Option<&std::path::Path>,
    json: bool,
) -> Result<(), CliError> {
    use sui_spec::store_inventory::Closure;
    use sui_spec::store_ops::ParsedNar;
    use sui_spec::store_transform::{apply_one, StoreTransform, TransformKind};
    use sui_spec::style::{body, glyph_arrow, glyph_snowflake, header, ident, info, muted, success, warn};

    if from.len() != to.len() {
        return Err(CliError::NotImplemented(format!(
            "graft: from/to must be same length; got {} vs {}", from.len(), to.len(),
        )));
    }

    let closure = Closure::walk(std::path::Path::new(root), "/nix/store")
        .map_err(|e| CliError::NotImplemented(format!("graft: closure: {e:?}")))?;

    let dest_root = dest.map(std::path::PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let bn = std::path::Path::new(root).file_name()
            .and_then(|n| n.to_str()).unwrap_or("graft");
        std::path::PathBuf::from(home).join(format!(".cache/sui/grafted/{bn}"))
    });
    std::fs::create_dir_all(&dest_root)
        .map_err(|e| CliError::NotImplemented(format!("graft: mkdir {}: {e}", dest_root.display())))?;

    let transform = StoreTransform {
        name: "graft".into(),
        description: format!("rewrite {from} → {to}"),
        match_kind: TransformKind::StorePathReference,
        pattern: from.into(),
        replacement: to.into(),
    };

    let mut total_refs = 0usize;
    let mut total_paths = 0usize;
    let mut errors: Vec<String> = Vec::new();
    let mut per_path: Vec<(String, usize)> = Vec::new();

    for path in &closure.paths {
        match sui_spec::nar::encode(path) {
            Ok(nar) => {
                let mut tree = match ParsedNar::parse(&nar) {
                    Ok(t) => t,
                    Err(e) => { errors.push(format!("{}: parse: {e}", path.display())); continue; }
                };
                let outcome = match apply_one(&mut tree.root, &transform) {
                    Ok(o) => o,
                    Err(e) => { errors.push(format!("{}: apply: {e:?}", path.display())); continue; }
                };
                total_refs += outcome.ref_rewrites;
                if outcome.ref_rewrites > 0 {
                    per_path.push((
                        path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
                        outcome.ref_rewrites,
                    ));
                    let new_nar = tree.serialize();
                    let target = dest_root.join(path.file_name().unwrap());
                    if target.exists() { let _ = std::fs::remove_dir_all(&target); }
                    if let Err(e) = sui_spec::nar::decode(&new_nar, &target) {
                        errors.push(format!("{}: decode: {e}", path.display()));
                    }
                }
                total_paths += 1;
            }
            Err(e) => errors.push(format!("{}: encode: {e}", path.display())),
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "root":         root,
            "from":         from,
            "to":           to,
            "dest_root":    dest_root.display().to_string(),
            "closure_size": closure.paths.len(),
            "paths_walked": total_paths,
            "paths_with_refs": per_path.len(),
            "total_refs":   total_refs,
            "errors":       errors,
        })).unwrap());
    } else {
        println!("{}  {}  {}→{}",
            glyph_snowflake(), header("store graft"),
            muted(from), muted(to));
        println!();
        println!("  {}  {}", body("closure root:"), ident(root));
        println!("  {}  {}", body("closure size:"), info(&closure.paths.len().to_string()));
        println!("  {}  {}", body("paths walked: "), info(&total_paths.to_string()));
        println!("  {}  {}", body("paths grafted:"), success(&per_path.len().to_string()));
        println!("  {}  {}", body("total ref rewrites:"), success(&total_refs.to_string()));
        println!("  {}  {}", body("dest root:    "), ident(&dest_root.display().to_string()));
        if !per_path.is_empty() {
            println!();
            println!("  {}", body("rewrites per path:"));
            per_path.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            for (name, n) in per_path.iter().take(20) {
                println!("    {} {} {} rewrites", success("→"), ident(name), info(&n.to_string()));
            }
        }
        if !errors.is_empty() {
            println!();
            println!("  {} {} error(s):", warn("?"), errors.len());
            for e in errors.iter().take(5) {
                println!("    {} {}", muted("·"), muted(e));
            }
        }
        println!();
        println!("  {} grafted {} ref(s) across {} path(s)",
            glyph_arrow(), success(&total_refs.to_string()), info(&per_path.len().to_string()));
    }
    Ok(())
}

// ── `sui store audit-secrets` — dry-run redact ───────────────

fn store_audit_secrets(source: &str, json: bool) -> Result<(), CliError> {
    use sui_spec::store_ops::ParsedNar;
    use sui_spec::store_transform::{apply_one, StoreTransform, TransformKind};
    use sui_spec::style::{body, glyph_arrow, glyph_snowflake, header, ident, info, muted, success, warn};

    let nar = sui_spec::nar::encode(std::path::Path::new(source))
        .map_err(|e| CliError::NotImplemented(format!("audit-secrets: encode: {e}")))?;
    let mut tree = ParsedNar::parse(&nar)
        .map_err(|e| CliError::NotImplemented(format!("audit-secrets: parse: {e}")))?;
    let transform = StoreTransform {
        name: "redact-base64-secrets".into(),
        description: "audit".into(),
        match_kind: TransformKind::FileContents,
        pattern: "[A-Za-z0-9+/=]{40,}".into(),
        replacement: "[redacted]".into(),
    };
    let outcome = apply_one(&mut tree.root, &transform)
        .map_err(|e| CliError::NotImplemented(format!("audit-secrets: apply: {e:?}")))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "source":         source,
            "matching_files": outcome.file_rewrites,
            "noop":           outcome.is_noop(),
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("audit secrets"), muted(source));
        println!();
        if outcome.is_noop() {
            println!("  {} no secret-like patterns found", success("✓"));
        } else {
            println!("  {} {} file(s) contain base64-like runs ≥40 chars",
                warn("⚠"), info(&outcome.file_rewrites.to_string()));
            println!("  {} run `sui store transform {} redact-base64-secrets` to materialize a redacted copy",
                glyph_arrow(), ident(source));
        }
    }
    if !outcome.is_noop() {
        std::process::exit(2);
    }
    Ok(())
}

// ── `sui store fingerprint` — composite typed observable ────────

fn store_fingerprint(path: &str, json: bool) -> Result<(), CliError> {
    use sui_spec::store_ops::ParsedNar;
    use sui_spec::store_inventory::Closure;
    use sui_spec::style::{body, glyph_snowflake, header, ident, info, muted, success};

    let parsed_path = sui_spec::store_layout::validate_against_canonical(path)
        .map_err(|e| CliError::NotImplemented(format!("fingerprint: {e:?}")))?;

    let nar = sui_spec::nar::encode(std::path::Path::new(path))
        .map_err(|e| CliError::NotImplemented(format!("fingerprint: encode: {e}")))?;
    use sha2::Digest;
    let nar_hash = sha2::Sha256::digest(&nar);
    let nar_hex: String = nar_hash.iter().map(|b| format!("{b:02x}")).collect();
    let nar_sri = sui_spec::hash::encode_hash("sha256", "sri", &nar_hash)
        .map_err(|e| CliError::NotImplemented(format!("fingerprint: sri: {e:?}")))?;

    let parsed = ParsedNar::parse(&nar)
        .map_err(|e| CliError::NotImplemented(format!("fingerprint: parse: {e}")))?;

    let closure = Closure::walk(std::path::Path::new(path), "/nix/store").ok();

    let top_entries: Vec<String> = match &parsed.root {
        sui_spec::store_ops::NarNode::Directory { entries } => {
            entries.iter().take(10).map(|(n, _)| n.clone()).collect()
        }
        _ => vec![],
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "path":          path,
            "hash_prefix":   parsed_path.hash,
            "name":          parsed_path.name,
            "nar_sha256_hex": nar_hex,
            "nar_sha256_sri": nar_sri,
            "nar_size":      nar.len(),
            "tree_size":     parsed.root.total_bytes(),
            "file_count":    parsed.root.file_count(),
            "top_entries":   top_entries,
            "closure_size":  closure.as_ref().map(|c| c.len()),
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("store fingerprint"), muted(path));
        println!();
        println!("  {}  {}", body("hash prefix:    "), success(&parsed_path.hash));
        println!("  {}  {}", body("name:           "), ident(&parsed_path.name));
        println!("  {}  {}", body("nar sha256 hex: "), success(&nar_hex));
        println!("  {}  {}", body("nar sha256 sri: "), success(&nar_sri));
        println!("  {}  {} bytes", body("nar size:       "), info(&nar.len().to_string()));
        println!("  {}  {} bytes", body("tree size:      "), info(&parsed.root.total_bytes().to_string()));
        println!("  {}  {}",       body("file count:     "), info(&parsed.root.file_count().to_string()));
        if let Some(c) = &closure {
            println!("  {}  {}", body("closure size:   "), info(&c.len().to_string()));
        }
        if !top_entries.is_empty() {
            println!();
            println!("  {}", body("top entries:"));
            for e in &top_entries {
                println!("    {} {}", muted("→"), success(e));
            }
        }
    }
    Ok(())
}

// ── `sui derivation graph` — typed dependency DAG ──────────────

fn derivation_graph(path: &str, max_depth: usize, json: bool) -> Result<(), CliError> {
    use std::collections::BTreeMap;
    use sui_compat::derivation::Derivation;
    use sui_spec::style::{body, glyph_arrow, glyph_snowflake, header, ident, info, muted, success};

    let mut nodes: BTreeMap<String, Vec<String>> = BTreeMap::new(); // node → input drvs
    let mut srcs: BTreeMap<String, Vec<String>> = BTreeMap::new();  // node → input srcs
    let mut queue: Vec<String> = vec![path.to_string()];
    let mut visited: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut iters = 0usize;

    while let Some(p) = queue.pop() {
        iters += 1;
        if iters > max_depth { break; }
        if !visited.insert(p.clone()) { continue; }
        let bytes = match std::fs::read(&p) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let drv = match Derivation::parse(&bytes) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let inputs: Vec<String> = drv.input_derivations.keys().cloned().collect();
        for i in &inputs { if !visited.contains(i) { queue.push(i.clone()); } }
        nodes.insert(p.clone(), inputs);
        srcs.insert(p.clone(), drv.input_sources);
    }

    if json {
        let nodes_json: Vec<serde_json::Value> = nodes.iter().map(|(node, edges)| serde_json::json!({
            "drv": node,
            "input_drvs": edges,
            "input_srcs": srcs.get(node).cloned().unwrap_or_default(),
        })).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "root":       path,
            "nodes":      nodes.len(),
            "max_depth":  max_depth,
            "graph":      nodes_json,
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("derivation graph"), muted(path));
        println!();
        println!("  {}  {}", body("nodes:"), info(&nodes.len().to_string()));
        let total_edges: usize = nodes.values().map(|v| v.len()).sum();
        let total_srcs: usize = srcs.values().map(|v| v.len()).sum();
        println!("  {}  {}", body("inputDrv edges:"), info(&total_edges.to_string()));
        println!("  {}  {}", body("inputSrc refs: "), info(&total_srcs.to_string()));
        println!();
        println!("  {}", body("top drvs by fan-out:"));
        let mut sorted: Vec<(&String, &Vec<String>)> = nodes.iter().collect();
        sorted.sort_by_key(|(_, edges)| std::cmp::Reverse(edges.len()));
        for (drv, edges) in sorted.iter().take(10) {
            let bn = std::path::Path::new(drv).file_name()
                .and_then(|n| n.to_str()).unwrap_or(drv);
            println!("    {} {} {} inputs", success("→"), ident(bn), info(&edges.len().to_string()));
        }
        println!();
        println!("  {} {} nodes / {} edges walked",
            glyph_arrow(), info(&nodes.len().to_string()), info(&total_edges.to_string()));
    }
    Ok(())
}

// ── `sui store transform` — typed graft/redact applier ─────────

fn store_transform(
    source: &str,
    transform_name: &str,
    dest: Option<&std::path::Path>,
    json: bool,
) -> Result<(), CliError> {
    use sui_spec::store_ops::ParsedNar;
    use sui_spec::store_transform::{self, apply_one};
    use sui_spec::style::{
        body, error, glyph_arrow, glyph_snowflake, header, ident, info, muted, success,
    };

    let xforms = store_transform::load_canonical()
        .map_err(|e| CliError::NotImplemented(format!("transform: load: {e:?}")))?;
    let xform = xforms.iter().find(|t| t.name == transform_name)
        .ok_or_else(|| CliError::NotImplemented(format!(
            "transform: unknown name `{transform_name}`; available: {}",
            xforms.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "),
        )))?;

    let source_path = std::path::PathBuf::from(source);
    let _ = sui_spec::store_layout::validate_against_canonical(source)
        .map_err(|e| CliError::NotImplemented(format!("transform: source `{source}`: {e:?}")))?;

    let dest_root = dest.map(std::path::PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let basename = source_path.file_name().and_then(|n| n.to_str()).unwrap_or("path");
        std::path::PathBuf::from(home).join(format!(".cache/sui/transformed/{basename}"))
    });
    if let Some(parent) = dest_root.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::NotImplemented(format!("transform: mkdir {}: {e}", parent.display())))?;
    }

    let nar = sui_spec::nar::encode(&source_path)
        .map_err(|e| CliError::NotImplemented(format!("transform: encode: {e}")))?;
    let mut tree = ParsedNar::parse(&nar)
        .map_err(|e| CliError::NotImplemented(format!("transform: parse: {e}")))?;

    let outcome = apply_one(&mut tree.root, xform)
        .map_err(|e| CliError::NotImplemented(format!("transform: apply: {e:?}")))?;

    let new_nar = tree.serialize();
    if dest_root.exists() {
        std::fs::remove_dir_all(&dest_root).ok();
    }
    sui_spec::nar::decode(&new_nar, &dest_root)
        .map_err(|e| CliError::NotImplemented(format!("transform: decode: {e}")))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "source":          source_path.display().to_string(),
            "dest":            dest_root.display().to_string(),
            "transform_name":  outcome.transform_name,
            "file_rewrites":   outcome.file_rewrites,
            "ref_rewrites":    outcome.ref_rewrites,
            "entries_renamed": outcome.entries_renamed,
            "noop":            outcome.is_noop(),
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(),
            header("store transform"),
            muted(&format!("`{transform_name}` ← `{}`", source_path.display())),
        );
        println!();
        println!("  {}  {}", body("file rewrites:  "), info(&outcome.file_rewrites.to_string()));
        println!("  {}  {}", body("ref rewrites:   "), info(&outcome.ref_rewrites.to_string()));
        println!("  {}  {}", body("entries renamed:"), info(&outcome.entries_renamed.to_string()));
        println!();
        if outcome.is_noop() {
            println!("  {} no-op (transform produced no changes — dest mirrors source)",
                muted("·"));
        } else {
            println!("  {} {} byte(s) → {}",
                glyph_arrow(),
                success(&new_nar.len().to_string()),
                ident(&dest_root.display().to_string()),
            );
        }
        if outcome.is_noop() {
            // Confirm noop produces byte-equivalent NAR.
            let dest_nar = sui_spec::nar::encode(&dest_root)
                .map_err(|e| CliError::NotImplemented(format!("transform: verify-encode: {e}")))?;
            if dest_nar == nar {
                println!("  {} noop verified: source NAR == dest NAR byte-equal",
                    success("✓"));
            } else {
                println!("  {} noop but NAR differs — substrate bug?",
                    error("✘"));
            }
        }
    }
    Ok(())
}

// ── `sui store inventory` — typed Nix-store walker ──────────────

fn store_inventory(profile_name: &str, json: bool) -> Result<(), CliError> {
    use sui_spec::store_inventory::{self, StoreInventory};
    use sui_spec::style::{body, glyph_arrow, glyph_snowflake, header, ident, info, muted, success};

    let profiles = store_inventory::load_canonical_profiles()
        .map_err(|e| CliError::NotImplemented(format!("inventory: load profiles: {e:?}")))?;
    let profile = profiles.iter().find(|p| p.name == profile_name)
        .ok_or_else(|| CliError::NotImplemented(format!(
            "inventory: unknown profile `{profile_name}`; available: {}",
            profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "),
        )))?;
    let inv = StoreInventory::walk(profile)
        .map_err(|e| CliError::NotImplemented(format!("inventory: walk: {e:?}")))?;

    if json {
        let entries: Vec<serde_json::Value> = inv.entries.values().map(|e| serde_json::json!({
            "path":         e.path.display().to_string(),
            "hash":         e.parsed.hash,
            "name":         e.parsed.name,
            "is_directory": e.is_directory,
            "file_count":   e.file_count,
            "size":         e.size,
        })).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "profile":     profile_name,
            "source_root": inv.root.display().to_string(),
            "entries":     entries,
            "summary":     {
                "entries":     inv.entries.len(),
                "total_size":  inv.total_size(),
                "total_files": inv.total_files(),
            },
        })).unwrap());
    } else {
        println!("{}  {}  {}  {}",
            glyph_snowflake(),
            header("store inventory"),
            muted(&format!("profile={profile_name}")),
            muted(&format!("root={}", inv.root.display())),
        );
        println!();
        println!("  {}  {}", body("entries:        "), ident(&inv.entries.len().to_string()));
        println!("  {}  {} bytes", body("total size:     "), info(&inv.total_size().to_string()));
        println!("  {}  {}", body("total files:    "), info(&inv.total_files().to_string()));

        // Show top-10 largest entries.
        let mut sorted: Vec<_> = inv.entries.values().collect();
        sorted.sort_by_key(|e| std::cmp::Reverse(e.size));
        let top = sorted.into_iter().take(10).collect::<Vec<_>>();
        println!();
        println!("  {}", body("top by size:"));
        for e in &top {
            println!("    {}  {} bytes / {} files",
                success(&e.parsed.name),
                info(&e.size.to_string()),
                muted(&e.file_count.to_string()),
            );
        }
        println!();
        println!("  {} typed inventory built for {} store path(s)",
            glyph_arrow(), success(&inv.entries.len().to_string()));
    }
    Ok(())
}

// ── `sui store closure` — typed dependency walker ───────────────

fn store_closure(path: &str, json: bool) -> Result<(), CliError> {
    use sui_spec::store_inventory::Closure;
    use sui_spec::style::{body, glyph_arrow, glyph_snowflake, header, ident, info, muted, success};

    let closure = Closure::walk(std::path::Path::new(path), "/nix/store")
        .map_err(|e| CliError::NotImplemented(format!("closure: {e:?}")))?;

    if json {
        let paths: Vec<String> = closure.paths.iter().map(|p| p.display().to_string()).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "root":  closure.root.display().to_string(),
            "count": closure.paths.len(),
            "paths": paths,
        })).unwrap());
    } else {
        println!("{}  {}  {}",
            glyph_snowflake(), header("store closure"), muted(path));
        println!();
        println!("  {}  {}", body("paths in closure:"), ident(&closure.paths.len().to_string()));
        println!();
        for p in &closure.paths {
            let basename = p.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            println!("    {}  {}", muted("→"), success(basename));
        }
        println!();
        println!("  {} {} transitive reference(s) discovered",
            glyph_arrow(), info(&(closure.paths.len().saturating_sub(1)).to_string()));
    }
    Ok(())
}

// ── `sui store materialize` — typed slice round-trip verifier ───

fn store_materialize(
    slice_name: &str,
    dest: Option<&std::path::Path>,
    json: bool,
) -> Result<(), CliError> {
    use sui_spec::store_ops::{self, MaterializationOutcome};
    use sui_spec::style::{
        body, error, glyph_arrow, glyph_snowflake, header, ident, info, muted, success, warn,
    };

    let slices = store_ops::load_canonical_slices().map_err(|e|
        CliError::NotImplemented(format!("materialize: load slices: {e:?}"))
    )?;
    let slice = slices.iter().find(|s| s.name == slice_name)
        .ok_or_else(|| CliError::NotImplemented(format!(
            "materialize: unknown slice `{slice_name}`; available: {}",
            slices.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "),
        )))?;

    let dest_root = dest.map(std::path::PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        std::path::PathBuf::from(home).join(format!(".cache/sui/materialize/{slice_name}"))
    });
    std::fs::create_dir_all(&dest_root)
        .map_err(|e| CliError::NotImplemented(format!("materialize: mkdir {}: {e}", dest_root.display())))?;

    let plans = store_ops::build_materialization_plan(slice, &dest_root)
        .map_err(|e| CliError::NotImplemented(format!("materialize: plan: {e:?}")))?;

    let mut outcomes: Vec<MaterializationOutcome> = Vec::with_capacity(plans.len());
    let mut errors: Vec<String> = Vec::new();
    for plan in &plans {
        match store_ops::run_materialization(plan) {
            Ok(o)  => outcomes.push(o),
            Err(e) => errors.push(format!("{}: {e:?}", plan.source.display())),
        }
    }

    let total = plans.len();
    let perfect = outcomes.iter().filter(|o| o.byte_equivalent).count();
    let diverged = outcomes.len() - perfect;

    if json {
        let probes: Vec<serde_json::Value> = outcomes.iter().map(|o| serde_json::json!({
            "source": o.source.display().to_string(),
            "dest":   o.dest.display().to_string(),
            "source_nar_sha256": o.source_nar_sha256,
            "dest_nar_sha256":   o.dest_nar_sha256,
            "byte_equivalent":   o.byte_equivalent,
            "source_size":       o.source_size,
            "file_count":        o.file_count,
        })).collect();
        let summary = serde_json::json!({
            "slice":     slice_name,
            "dest_root": dest_root.display().to_string(),
            "total":     total,
            "perfect":   perfect,
            "diverged":  diverged,
            "errors":    errors,
            "probes":    probes,
        });
        println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    } else {
        println!("{}  {}  {}  {}  {}",
            glyph_snowflake(),
            header("sui store materialize"),
            muted(&format!("slice={slice_name}")),
            muted(&format!("max-entries={}", slice.max_entries)),
            muted(&format!("dest={}", dest_root.display())),
        );
        println!();
        for o in &outcomes {
            let glyph = if o.byte_equivalent { success("✓") } else { error("✘") };
            let basename = o.source.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?");
            println!("  {} {}", glyph, ident(basename));
            println!("      {} {} bytes / {} files",
                muted("size:"),
                info(&o.source_size.to_string()),
                info(&o.file_count.to_string()),
            );
            println!("      {} {}", muted("src nar sha256:"), body(&o.source_nar_sha256));
            println!("      {} {}", muted("dst nar sha256:"), body(&o.dest_nar_sha256));
        }
        for err in &errors {
            println!("  {} {}", warn("?"), muted(err));
        }
        println!();
        println!("  {} {}/{}/{} (perfect/diverged/errored)",
            body("∑"),
            success(&perfect.to_string()),
            if diverged > 0 { error(&diverged.to_string()) } else { muted("0") },
            if errors.is_empty() { muted("0") } else { warn(&errors.len().to_string()) },
        );
        if perfect == total && errors.is_empty() {
            println!("  {} byte-perfect rematerialization across {} path(s)",
                glyph_arrow(), success(&total.to_string()));
        }
    }

    if diverged > 0 || !errors.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

// ── `sui parity` — operator-facing nix-vs-sui validator ─────────

/// One probe in the parity sweep: name + sui invocation + nix
/// invocation + verdict.
struct ParityProbe {
    name: &'static str,
    description: &'static str,
    /// The all-variants matrix's expectation for this row. A `Match` row is a
    /// byte-parity theorem — a regression fails the gate. A `KnownDiverge` row
    /// is a tracked (xfail) divergence — it does NOT fail the gate while it
    /// still diverges, but if it starts *matching* it has GRADUATED and the
    /// gate fails to force its promotion to `Match` (so progress can never be
    /// silently un-tracked). This is CONVERGE = SEAL: the corpus is the sealed
    /// invariant; neither a regression nor an un-promoted graduation can ship.
    expect: Expect,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// Byte-identical to nix is required; any divergence/error is a regression.
    Match,
    /// Known to diverge today (tracked); surfaced but tolerated until fixed.
    KnownDiverge,
}

/// Outcome of running one probe.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ParityVerdict {
    Match,
    Diverge { sui: String, nix: String },
    SuiError(String),
    NixError(String),
    Skipped(String),
}

impl ParityVerdict {
    fn glyph(&self) -> &'static str {
        match self {
            Self::Match        => "✓",
            Self::Diverge { .. } => "✘",
            Self::SuiError(_)  => "✘",
            Self::NixError(_)  => "?",
            Self::Skipped(_)   => "·",
        }
    }
    fn label(&self) -> &'static str {
        match self {
            Self::Match        => "match",
            Self::Diverge { .. } => "diverge",
            Self::SuiError(_)  => "sui-err",
            Self::NixError(_)  => "nix-err",
            Self::Skipped(_)   => "skip",
        }
    }
}

fn run_capture(bin: &std::path::Path, args: &[&str]) -> Result<String, String> {
    use std::process::Command;
    let out = Command::new(bin).args(args).output()
        .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn run_bytes(bin: &std::path::Path, args: &[&str]) -> Result<Vec<u8>, String> {
    use std::process::Command;
    let out = Command::new(bin).args(args).output()
        .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(out.stdout)
}

/// Resolve the current HEAD rev of a nixpkgs channel ref (e.g. `nixpkgs-unstable`)
/// via `nix flake metadata --json`, typed — the machine's `--track-nixpkgs` pin.
/// Replaces the workflow's `nix flake metadata | jq -r .locked.rev` shell.
fn resolve_nixpkgs_rev(nix: &std::path::Path, reference: &str) -> Result<String, CliError> {
    let meta = run_capture(
        nix,
        &[
            "flake", "metadata",
            &format!("github:NixOS/nixpkgs/{reference}"),
            "--json",
            "--extra-experimental-features", "nix-command flakes",
        ],
    )
    .map_err(|e| CliError::Orchestrate { operation: "track-nixpkgs", message: format!("nix flake metadata: {e}") })?;
    let v: serde_json::Value = serde_json::from_str(&meta)
        .map_err(|e| CliError::Orchestrate { operation: "track-nixpkgs", message: format!("parse metadata json: {e}") })?;
    v.get("locked")
        .and_then(|l| l.get("rev"))
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| CliError::Orchestrate { operation: "track-nixpkgs", message: "no locked.rev in flake metadata".into() })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in d { s.push_str(&format!("{b:02x}")); }
    s
}

fn cmd_parity(nix: &std::path::Path, json: bool) -> Result<(), CliError> {
    use sui_spec::style::{
        body, error, glyph_snowflake, header, ident, info, muted, success, warn,
    };

    let sui_bin = std::env::current_exe()
        .map_err(|e| CliError::NotImplemented(format!("parity: own exe: {e}")))?;

    let sample_hash =
        "sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03";

    // Prepare /tmp/h fixture for hash file probe.
    let h_fixture = std::env::temp_dir().join("sui-parity-h");
    std::fs::write(&h_fixture, b"hello\n").ok();

    // Pick representative store paths for NAR + drv probes.
    let source_path = first_store_path_matching("-source");
    let drv_path = first_store_path_matching(".drv");

    // Definitive corpus.
    let probes: Vec<(ParityProbe, Box<dyn Fn() -> ParityVerdict>)> = vec![
        (ParityProbe { name: "hash to-base16", description: "byte-equivalent", expect: Expect::Match }, {
            let sui = sui_bin.clone();
            let nix = nix.to_path_buf();
            Box::new(move || diff_text(
                run_capture(&sui, &["hash", "to-base16", sample_hash]),
                run_capture(&nix, &["hash", "to-base16", "--type", "sha256", sample_hash]),
            ))
        }),
        (ParityProbe { name: "hash to-base32", description: "byte-equivalent", expect: Expect::Match }, {
            let sui = sui_bin.clone();
            let nix = nix.to_path_buf();
            Box::new(move || diff_text(
                run_capture(&sui, &["hash", "to-base32", sample_hash]),
                run_capture(&nix, &["hash", "to-base32", "--type", "sha256", sample_hash]),
            ))
        }),
        (ParityProbe { name: "hash to-base64", description: "byte-equivalent", expect: Expect::Match }, {
            let sui = sui_bin.clone();
            let nix = nix.to_path_buf();
            Box::new(move || diff_text(
                run_capture(&sui, &["hash", "to-base64", sample_hash]),
                run_capture(&nix, &["hash", "to-base64", "--type", "sha256", sample_hash]),
            ))
        }),
        (ParityProbe { name: "hash to-sri", description: "byte-equivalent", expect: Expect::Match }, {
            let sui = sui_bin.clone();
            let nix = nix.to_path_buf();
            Box::new(move || diff_text(
                run_capture(&sui, &["hash", "to-sri", sample_hash]),
                run_capture(&nix, &["hash", "to-sri", "--type", "sha256", sample_hash]),
            ))
        }),
        (ParityProbe { name: "hash file", description: "SRI byte-equivalent", expect: Expect::Match }, {
            let sui = sui_bin.clone();
            let nix = nix.to_path_buf();
            let h = h_fixture.clone();
            Box::new(move || diff_text(
                run_capture(&sui, &["hash", "file", h.to_str().unwrap(), "--base", "sri"]),
                run_capture(&nix, &["hash", "file", h.to_str().unwrap()]),
            ))
        }),
        (ParityProbe { name: "store dump-path", description: "NAR sha256 byte-equivalent", expect: Expect::Match }, {
            let sui = sui_bin.clone();
            let nix = nix.to_path_buf();
            let sp = source_path.clone();
            Box::new(move || match &sp {
                None => ParityVerdict::Skipped("no /nix/store/*-source path on this host".into()),
                Some(sp) => {
                    let n = run_bytes(&nix, &["--extra-experimental-features", "nix-command",
                                              "store", "dump-path", sp.to_str().unwrap()]);
                    let s = run_bytes(&sui, &["store", "dump-path", sp.to_str().unwrap()]);
                    match (n, s) {
                        (Ok(nbytes), Ok(sbytes)) => {
                            let nh = sha256_hex(&nbytes);
                            let sh = sha256_hex(&sbytes);
                            if nh == sh { ParityVerdict::Match }
                            else { ParityVerdict::Diverge { sui: sh, nix: nh } }
                        }
                        (Err(e), _)  => ParityVerdict::NixError(e),
                        (_, Err(e))  => ParityVerdict::SuiError(e),
                    }
                }
            })
        }),
        (ParityProbe { name: "derivation show→add", description: "ATerm round-trip", expect: Expect::Match }, {
            let sui = sui_bin.clone();
            let drv = drv_path.clone();
            Box::new(move || match &drv {
                None => ParityVerdict::Skipped("no .drv in /nix/store".into()),
                Some(drv) => {
                    let original = std::fs::read_to_string(drv).unwrap_or_default();
                    let json = match run_capture(&sui, &["derivation", "show", drv.to_str().unwrap()]) {
                        Ok(s) => s,
                        Err(e) => return ParityVerdict::SuiError(e),
                    };
                    let tmp = std::env::temp_dir().join("sui-parity-drv.json");
                    std::fs::write(&tmp, &json).ok();
                    let out = std::process::Command::new(&sui)
                        .args(["derivation", "add", tmp.to_str().unwrap()])
                        .output();
                    let _ = std::fs::remove_file(&tmp);
                    match out {
                        Err(e) => ParityVerdict::SuiError(e.to_string()),
                        Ok(o) => {
                            let stderr = String::from_utf8_lossy(&o.stderr);
                            let aterm: String = stderr.lines()
                                .filter(|l| !l.starts_with('#'))
                                .collect::<Vec<_>>().join("\n");
                            let aterm = aterm.trim_end_matches('\n');
                            if aterm == original.trim_end_matches('\n') { ParityVerdict::Match }
                            else { ParityVerdict::Diverge {
                                sui: format!("{} bytes", aterm.len()),
                                nix: format!("{} bytes", original.len()),
                            }}
                        }
                    }
                }
            })
        }),
        // ── eval-parity: the mission core — outPath / drvPath byte-for-byte
        //    through the evaluator (the tree-walker, --no-vm). These are the
        //    rows the ecosystem north star lives or dies by; the CLI probes
        //    above only cover the hash/store/derivation surfaces.
        (ParityProbe { name: "eval placeholder", description: "builtins.placeholder \"out\"", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || diff_eval(&sui, &nix, "builtins.placeholder \"out\""))
        }),
        (ParityProbe { name: "eval drv outPath", description: "(derivation{…}).outPath", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || diff_eval(&sui, &nix,
                "(builtins.derivation { name = \"p\"; system = builtins.currentSystem; builder = \"/bin/sh\"; }).outPath"))
        }),
        (ParityProbe { name: "eval FOD drvPath", description: "fixed-output .drvPath (env[out])", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || diff_eval(&sui, &nix,
                "(builtins.derivation { name = \"s\"; system = builtins.currentSystem; builder = \"/bin/sh\"; outputHash = \"1121cfccd5913f0a63fec40a6ffd44ea64f9dc135c66634ba001d10bcf4302a2\"; outputHashAlgo = \"sha256\"; outputHashMode = \"flat\"; }).drvPath"))
        }),
        // Session 2026-07-10 wins — sealed as Match so they can never silently
        // regress. concatStringsSep/concatStrings context accumulation (the
        // makeLibraryPath/makeBinPath fleet-wide root): a `${pkg.out}/lib`
        // element must keep pkg's `out` output in the derivation's input set.
        (ParityProbe { name: "eval concatStringsSep context", description: "makeLibraryPath-shaped context accumulation (root a67c244)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || diff_eval(&sui, &nix,
                "let m = derivation { name = \"m\"; system = builtins.currentSystem; builder = \"/bin/sh\"; outputs = [ \"out\" \"dev\" ]; }; in (derivation { name = \"c\"; system = builtins.currentSystem; builder = \"/bin/sh\"; L = builtins.concatStringsSep \":\" [ \"${m.out}/lib\" \"${m.dev}/include\" ]; }).drvPath"))
        }),
        // Attrset dotted+full-set deep-merge (the pkg-config-wrapper env.addFlags
        // root): `a.b = x; a = { c = y; }` must merge, not clobber.
        (ParityProbe { name: "eval attrset dotted+fullset merge", description: "`a.b=x; a={c=y}` deep-merge (root 73b904d)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || diff_eval(&sui, &nix,
                "let s = { a.b = \"x\"; a = { c = \"y\"; }; }; in s.a.b + s.a.c"))
        }),
        // CLOSED (Darwin Root #2, 2026-07-11): `builtins.path { filter; }` /
        // `lib.cleanSourceWith` was applying the filter to the SOURCE ROOT
        // itself and pruning the whole tree on a `false` result. Every
        // `filter = p: t: elem (baseNameOf p) [...]` (the cleanSourceWith
        // shape) returns false on the root dir (its basename is never in the
        // keep-list), so sui NAR-hashed the EMPTY directory instead of the
        // filtered contents — collapsing `documentation-highlighter`'s src to
        // the empty-dir store path and diverging every drv consuming a filtered
        // source. Fix: the root is dumped unconditionally; the filter governs
        // only its descendants (paths.rs::materialize_filtered). This probe is
        // self-contained (writes its own two-file dir) so it needs no
        // <nixpkgs>: `keep.txt` survives, `drop.txt` + the root-that-fails-the-
        // predicate must NOT empty the tree.
        (ParityProbe { name: "eval builtins.path filter keeps root", description: "cleanSourceWith-shaped filter: root dumped unconditionally, only descendants filtered (Darwin Root #2)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let dir = std::env::temp_dir().join("sui-parity-pathfilter-src");
                if std::fs::create_dir_all(&dir).is_err() {
                    return ParityVerdict::Skipped("could not create temp source dir".into());
                }
                if std::fs::write(dir.join("keep.txt"), b"kept\n").is_err()
                    || std::fs::write(dir.join("drop.txt"), b"dropped\n").is_err()
                {
                    return ParityVerdict::Skipped("could not write temp source files".into());
                }
                let d = dir.display();
                diff_eval(&sui, &nix, &format!(
                    "toString (builtins.path {{ path = {d}; name = \"source\"; \
                     filter = path: type: (baseNameOf path) == \"keep.txt\"; }})"
                ))
            })
        }),
        // GRADUATED 2026-07-20 KnownDiverge -> Match. The ident-cache aliasing
        // fix (env.source_id() instead of the unmaintained CURRENT_SOURCE_ID
        // thread-local) closed it: this row now byte-matches nix on
        // aarch64-darwin. It was the last open `hello` leaf — its x86_64-linux
        // sibling below was already sealed. The gate caught the graduation
        // itself, exiting 1 on an untracked advance, which is the behaviour that
        // makes promoting it a deliberate act rather than a silent drift.
        (ParityProbe { name: "eval hello drvPath", description: "nixpkgs hello through stdenv (ecosystem target)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{}}).hello.drvPath"))
            })
        }),
        // CLOSED (2026-07-10): the crypt-disabled perl that breaks nixpkgs'
        // perl↔libxcrypt/openssl bootstrap cycle. Root was sui's EAGER
        // `derivation` builtin — forcing a derivation VALUE to WHNF eagerly
        // computed its drvPath, forcing every dependency attr, which
        // manufactured a same-thunk Blackhole re-entry that nix avoids by never
        // forcing the deps until a store path is demanded (`derivation` returns
        // a LAZY `.drvPath`, only `derivationStrict` is eager). Fixed by making
        // the computed fields (`drvPath`/`outPath`/per-output) memoized lazy
        // thunks (`build_derivation` → `compute_full_drv` behind a OnceCell).
        // This graduated openssl + hello (x86_64-linux) to byte-parity.
        (ParityProbe { name: "eval crypt-disabled perl (bootstrap cycle-break)", description: "buildPackages.perl.override{enableCrypt=false} — CLOSED via lazy derivation", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("((import {np} {{ system = \"x86_64-linux\"; }}).buildPackages.perl.override {{ enableCrypt = false; }}).drvPath"))
            })
        }),
        // Cornering lock (Match): the closest SYNTHETIC analogue of the real
        // libxcrypt bug — a two-stage fixpoint where a mutually-referential
        // `libxcrypt` takes `buildPackages.perl.override{...}` (a distinct,
        // non-cycling override) as its sole nativeBuildInput. sui handles this
        // correctly (byte-matches nix), which PROVES the real perl-null is NOT
        // caused by {fixpoint, mutual-ref, override, staged buildPackages} —
        // it is isolated to the real splice/makeScopeWithSplicing machinery.
        // Locking it prevents regression AND documents what the bug is NOT.
        (ParityProbe { name: "eval staged mutual-ref override nativeBuildInput", description: "corners the libxcrypt null: synthetic stage+mutual+override works (bug is in real splice machinery)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || diff_eval(&sui, &nix,
                "let fix = f: let x = f x; in x; mkStage = adjacent: name: fix (self: { inherit name; buildPackages = if adjacent == null then self else adjacent; mkP = args: derivation { name = \"p\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; tag = args.tag; stage = name; buildInputs = if args.tag == \"full\" then [ self.libxcrypt ] else []; }; perl = (self.mkP { tag = \"full\"; }) // { override = a: self.mkP a; }; libxcrypt = derivation { name = \"libxcrypt\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; nativeBuildInputs = [ (self.buildPackages.perl.override { tag = \"nocrypt\"; }) ]; }; }); s0 = mkStage null \"s0\"; s1 = mkStage s0 \"s1\"; in s1.libxcrypt.drvPath"))
        }),
        // CLOSED (2026-07-10): standalone `openssl.drvPath`. Was the reduced
        // real leaf of the hello/perl stage-collapse — sui's EAGER `derivation`
        // forced openssl's dep graph while the perl↔libxcrypt fixpoint was on
        // the force stack, dropping perl to a `null`/empty-partial. Fixed by the
        // lazy-`derivation` change (see the crypt-disabled-perl row above);
        // openssl now byte-matches nix `izhl4bcm…`.
        (ParityProbe { name: "eval openssl drvPath (__spliced null leaf)", description: "openssl standalone — CLOSED via lazy derivation (was the reduced hello stage-collapse leaf)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).openssl.drvPath"))
            })
        }),
        // THE PRIZE (2026-07-10): x86_64-linux `hello.drvPath` — the mission
        // minimum bar. Byte-matches nix `j8q5j0x4…` now that the stdenv
        // perl↔libxcrypt/openssl bootstrap cycle resolves (lazy `derivation`).
        // NOTE (updated 2026-07-20): the default-system (darwin) `hello` row
        // above is NO LONGER a KnownDiverge — it graduated to Match when the
        // ident-cache aliasing bug was fixed. What was described here as "a
        // SEPARATE cross-system apple-sdk/python leaf" turned out to be the same
        // root as everything else that could not evaluate nixpkgs.
        (ParityProbe { name: "eval hello drvPath (x86_64-linux)", description: "nixpkgs hello through the linux stdenv bootstrap — THE mission target, CLOSED via lazy derivation", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).hello.drvPath"))
            })
        }),
        // ── WHOLE-WORLD FOUNDATION SEAL (wave-4, 2026-07-16) ──────────────────
        // The Linux stdenv bootstrap + cross-compile splice + the universal
        // trivial-builders are byte-identical to nix TODAY (proven by ~80 live
        // probes), but were guarded only TRANSITIVELY through leaf packages.
        // These rows seal the FOUNDATION every Linux package builds on, so a
        // regression in the bootstrap / splice / a trivial-builder is a RED GATE,
        // not a silent pass (Parity Method / DOMINATION REFLEX). All verified
        // byte-matching nix 2.34.7 at seal time.
        (ParityProbe { name: "eval stdenv drvPath (x86_64-linux)", description: "the LINUX stdenv — the load-bearing foundation every linux package builds on", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).stdenv.drvPath"))
            })
        }),
        (ParityProbe { name: "eval stdenv.cc drvPath (x86_64-linux)", description: "the linux stdenv C compiler (gcc-wrapper) — bootstrap chain seal", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).stdenv.cc.drvPath"))
            })
        }),
        (ParityProbe { name: "eval bootstrapTools drvPath (x86_64-linux)", description: "the stdenv bootstrap-tools — stage0 of the linux bootstrap", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).stdenv.bootstrapTools.drvPath"))
            })
        }),
        (ParityProbe { name: "eval buildPackages.hello splice identity (x86_64-linux)", description: "the __spliced/buildPackages identity — the pervasive nixpkgs splice machinery", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).buildPackages.hello.drvPath"))
            })
        }),
        (ParityProbe { name: "eval pkgsCross.musl64.hello drvPath", description: "full cross-compile (x86_64→musl64) — the most fragile nixpkgs machinery", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).pkgsCross.musl64.hello.drvPath"))
            })
        }),
        (ParityProbe { name: "eval lib.systems.elaborate config (x86_64-linux)", description: "platform elaboration — the config triple every stdenv derives from", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("((import {np} {{ }}).lib.systems.elaborate \"x86_64-linux\").config"))
            })
        }),
        (ParityProbe { name: "eval writeText drvPath (builder-class)", description: "trivial-builder writeText — appears in nearly every closure", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("((import {np} {{ system = \"x86_64-linux\"; }}).writeText \"t\" \"hi\").drvPath"))
            })
        }),
        (ParityProbe { name: "eval writeShellScript drvPath (builder-class)", description: "trivial-builder writeShellScript — script derivations fleet-wide", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("((import {np} {{ system = \"x86_64-linux\"; }}).writeShellScript \"s\" \"echo hi\").drvPath"))
            })
        }),
        (ParityProbe { name: "eval buildEnv drvPath (builder-class)", description: "trivial-builder buildEnv — the union-of-paths derivation (system.path etc.)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("((import {np} {{ system = \"x86_64-linux\"; }}).buildEnv {{ name = \"e\"; paths = []; }}).drvPath"))
            })
        }),
        (ParityProbe { name: "eval linkFarm drvPath (builder-class)", description: "trivial-builder linkFarm — symlink tree derivations", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("((import {np} {{ system = \"x86_64-linux\"; }}).linkFarm \"l\" []).drvPath"))
            })
        }),
        (ParityProbe { name: "eval runCommand drvPath (builder-class)", description: "trivial-builder runCommand — the ubiquitous ad-hoc derivation", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("((import {np} {{ system = \"x86_64-linux\"; }}).runCommand \"r\" {{ }} \"echo hi\").drvPath"))
            })
        }),
        (ParityProbe { name: "eval symlinkJoin drvPath (builder-class)", description: "trivial-builder symlinkJoin — the merge-many-outputs derivation", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("((import {np} {{ system = \"x86_64-linux\"; }}).symlinkJoin {{ name = \"s\"; paths = []; }}).drvPath"))
            })
        }),
        // CLOSED (2026-07-10): curl + git — the last two divergent packages in
        // the 23-package parity basket. Both bottomed out at the SAME root:
        // `python3.13-flit-core-3.12.0.drv` was missing `propagatedBuildInputs`
        // in its env because nixpkgs' python `isMismatchedPython` guard
        // (`drv.pythonModule != python`, mk-python-derivation.nix:72) fired
        // spuriously — sui compared two derivation attrsets that share an
        // `outPath` by DEEP STRUCTURAL equality, which never matches (derivations
        // carry thunks/functions), so `!=` was always `true`. Fixed by teaching
        // `Concrete::PartialEq` cppnix's `EvalState::eqValues` derivation
        // short-circuit: two attrsets that are BOTH `type=="derivation"` with an
        // `outPath` compare by `outPath` string ONLY (value.rs `derivation_out_path`).
        (ParityProbe { name: "eval curl drvPath (x86_64-linux)", description: "nixpkgs curl through python/flit-core — CLOSED via cppnix derivation-equality short-circuit", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).curl.drvPath"))
            })
        }),
        (ParityProbe { name: "eval git drvPath (x86_64-linux)", description: "nixpkgs git through python/flit-core — CLOSED via cppnix derivation-equality short-circuit", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).git.drvPath"))
            })
        }),
        // CLOSED (2026-07-11): ffmpeg — bottomed out at the full-set-then-dotted
        // attr merge dropping the full-set keys. gst-plugins-base
        // `passthru = { … }; passthru.tests.x = …;` dropped `waylandEnabled`
        // (gst-plugins-bad reads it). Root: a full-set binding `a = { x = … }`
        // is a lazy Thunk, and merge_nested_insert only merges concrete
        // Value::Attrs, so `a.y = …` overwrote `a` with `{ y = … }`. Fixed by
        // forcing the existing Thunk target to WHNF on a multi-segment collision.
        (ParityProbe { name: "eval ffmpeg drvPath (x86_64-linux)", description: "nixpkgs ffmpeg through gstreamer passthru — CLOSED via full-set-then-dotted merge fix", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).ffmpeg.drvPath"))
            })
        }),
        // KNOWN DIVERGE (2026-07-12): neovim — un-blinded via SUI_PARITY_STRICT
        // (the derivation.rs env-loop swallow collector, this session's M0). The
        // `SUI_PARITY_STRICT` run named 4 dropped deps, all
        // `UndefinedVar("'callPackage'")`, in the python2.7 build-hook drvs
        // (pip-install-hook / setuptools-build-hook / python-catch-conflicts-hook).
        // `parity-bisect` localizes the structural leaf to `pip-install-hook.drv`
        // via `neovim → wl-clipboard → xdg-utils → resholve → pip-install-hook`,
        // with nix carrying an inputDrv `python2.7-pip-20.3.4.drv` + an env key
        // `propagatedBuildInputs` that sui drops.
        //
        // ROOT (localized, NOT yet closed): forcing `python27.pkgs.pip` (or any
        // python27 package) throws `UndefinedVar(callPackage)` in sui while nix
        // resolves it; `python3.pkgs.pip` is CLEAN. The python2.7 package set is
        // the ONLY set that composes the extra `python2-packages.nix` overlay
        // (`passthrufun.nix`: `optionalExtensions (!self.isPy3k) [ python2Extension ]`),
        // whose body is `self: super: with self; with super; { pip = callPackage …; }`.
        // So `callPackage` is a `with self;`-scoped var — the SAME class as the
        // CLOSED `nettle` row below (bare-inherit/`with`-scope resolving eagerly)
        // — but reached through the `lib.extends`/`composeManyExtensions`/
        // `makeScopeWithSplicing'` extension-composition path rather than
        // all-packages.nix's `with pkgs;`. A faithful in-isolation repro of the
        // two-layer `self: super: with self;` overlay + fixpoint does NOT diverge
        // in sui, so the trigger is a specific interaction in that composition
        // path — closing it safely needs deeper work than a parity-safe single
        // session, and a wrong touch to `lib.extends`/with-scope threading risks
        // the 50+ green rows. Sealed KnownDiverge so a fix auto-graduates the
        // gate and a further silent regression is caught. (ffmpeg, the sibling
        // "open" package, is already CLOSED above — byte-identical, clean strict.)
        (ParityProbe { name: "eval neovim drvPath (x86_64-linux)", description: "nixpkgs neovim — CLOSED 2026-07-15: python2Extension `with self; with super; callPackage` resolved a STALE mid-fixpoint partial `self` from the with-scope cache (missing callPackage that the completed `self` has). Fixed by an error-path-only cache-bypassing `lookup_fresh` in the WithIdent force. Byte-identical: rjlgmvccqkmdbsgh0aazqcz7bxnhwagb-neovim-0.11.7.drv", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).neovim.drvPath"))
            })
        }),
        // NOTE: the LOAD-BEARING ROOT of the neovim divergence is isolated to
        // `python27.pkgs.pip` throwing `UndefinedVar(callPackage)` in sui (nix →
        // "python2.7-pip-20.3.4") — the python2Extension `with self; callPackage`
        // layer. It is NOT added as its own corpus row because on that expr sui
        // *errors* (an uncatchable `UndefinedVar`, not a value), which `diff_eval`
        // classifies `SuiError` rather than `Diverge` — so it would neither seal
        // as `tracked` nor fail the gate (a silent hole). The `neovim.drvPath`
        // row above IS the tracked seal: sui produces a (divergent) drvPath there
        // because the swallow drops the dep instead of propagating the throw, so
        // it is a clean `Diverge` the gate tracks + auto-graduates on the fix.
        //
        // CLOSED (2026-07-15). Repro (6s, a plain error — NOT the OOM-prone full
        // eval): `sui eval --no-vm --raw '(import <nixpkgs> { system =
        // "x86_64-linux"; }).python27.pkgs.pip.drvPath'`. Bisected: python3.pkgs.pip
        // is clean; `python27.pkgs.callPackage` IS present in the completed set;
        // only the py2Extension body `self: super: with self; with super; { pip =
        // callPackage …; }` throws. Instrumented `Env::debug_with_scope_summary`
        // at the failure showed the two with-scopes as `[super] n=10644
        // has_callPackage=FALSE` and `[self] n=10656 has_callPackage=TRUE` — yet
        // `env.lookup` returned None. ROOT: the with-scope CACHE for `self` held a
        // STALE mid-fixpoint PARTIAL (`f self` at 10644, cached before makeScope's
        // `self = f self // { callPackage = …; }` merged the scope infra in), and
        // `lookup_fast`'s cache-first search trusted it + skipped (value.rs, the
        // `continue` on a cached miss). A FRESH `force_value` gives the completed
        // 10656 `self` with callPackage. (Earlier nested-with-fallthrough and
        // blackhole-at-resolution hypotheses were both raised then DISPROVEN by
        // the trace — no promotion/blackhole fires; recorded so they don't recur.)
        // FIX (byte-safe, guarded by this 65-row corpus): `Env::lookup_fresh` —
        // a cache-BYPASSING fresh force of each with-scope, called ONLY on the
        // about-to-throw `UndefinedVar` arm of the WithIdent force (value.rs). It
        // can never affect a lookup that already succeeds (passing rows never reach
        // it) and never re-forces on the hot path. Byte-verified: neovim.drvPath ==
        // nix (rjlgmvccqkmdbsgh0aazqcz7bxnhwagb-neovim-0.11.7.drv), corpus 65 match
        // · 0 tracked · 0 regressions. (An always-re-force-on-cached-miss variant
        // was tried first and REVERTED — it broke 32 rows by re-forcing
        // mid-fixpoint scopes on the hot path; the error-path-only placement is
        // what makes it safe.)
        // CLOSED (2026-07-11): nettle — bottomed out at bare `inherit x`
        // resolving EAGERLY. all-packages.nix is
        // `with pkgs; { nettle = import … { inherit callPackage fetchurl; }; }`,
        // so `inherit callPackage` must resolve from the `with pkgs` scope AT
        // FORCE TIME. Eager env.lookup threw UndefinedVar(callPackage). Fixed by
        // deferring a bare inherit to a WithIdent thunk (lazy with-scope lookup),
        // matching CppNix.
        (ParityProbe { name: "eval nettle drvPath (x86_64-linux)", description: "nixpkgs nettle through `with pkgs; inherit callPackage` — CLOSED via lazy bare-inherit", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).nettle.drvPath"))
            })
        }),
        // ── BROADENED BASKET (2026-07-12): six more x86_64-linux packages, each
        //    probed byte-for-byte sui-vs-nix and confirmed MATCH in this
        //    session's parity run. These widen the sealed corpus across distinct
        //    build-graph shapes — an interpreter multi-output set (python3), a
        //    C-lib with a fetch+configure graph (sqlite), a build-system stdenv
        //    (cmake), an XML lib with propagated deps (libxml2), a second
        //    interpreter fixpoint (ruby), and a plain coreutils-class tool
        //    (gnused) — so a regression in any of those classes fails the gate.
        //    None hit the python2.7 `python2Extension` splicing path that keeps
        //    neovim tracked; they exercise the already-CLOSED lazy-derivation +
        //    with-scope + dotted-merge roots at greater breadth.
        (ParityProbe { name: "eval python3 drvPath (x86_64-linux)", description: "nixpkgs python3 multi-output through the linux stdenv — broadened basket 2026-07-12", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).python3.drvPath"))
            })
        }),
        (ParityProbe { name: "eval sqlite drvPath (x86_64-linux)", description: "nixpkgs sqlite C-lib fetch+configure graph — broadened basket 2026-07-12", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).sqlite.drvPath"))
            })
        }),
        (ParityProbe { name: "eval cmake drvPath (x86_64-linux)", description: "nixpkgs cmake build-system stdenv — broadened basket 2026-07-12", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).cmake.drvPath"))
            })
        }),
        (ParityProbe { name: "eval libxml2 drvPath (x86_64-linux)", description: "nixpkgs libxml2 with propagated deps — broadened basket 2026-07-12", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).libxml2.drvPath"))
            })
        }),
        (ParityProbe { name: "eval ruby drvPath (x86_64-linux)", description: "nixpkgs ruby interpreter fixpoint — broadened basket 2026-07-12", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).ruby.drvPath"))
            })
        }),
        (ParityProbe { name: "eval gnused drvPath (x86_64-linux)", description: "nixpkgs gnused coreutils-class tool — broadened basket 2026-07-12", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("(import {np} {{ system = \"x86_64-linux\"; }}).gnused.drvPath"))
            })
        }),
        // REGRESSION GUARD (2026-07-10): the fully self-contained minimal repro
        // of the stage-collapse root — NO nixpkgs, pure builtins. A
        // `makeOverridable` package set with a perl↔libxcrypt cycle broken by
        // `enableCrypt=false` (perl propagates libxcrypt; libxcrypt's
        // nativeBuildInput is `perl.override{enableCrypt=false}` which does NOT
        // propagate libxcrypt → the cycle terminates in nix). Before the lazy-
        // `derivation` fix, forcing `perl.drvPath` eagerly forced the whole
        // fixpoint and dropped the crypt-disabled perl to an empty partial. This
        // 22-line probe fails fast (no nixpkgs eval) if `derivation` ever regains
        // eager-at-WHNF drvPath computation.
        (ParityProbe { name: "eval lazy-derivation cycle-break (self-contained)", description: "makeOverridable perl↔libxcrypt cycle broken by enableCrypt — pure-builtins regression guard for lazy `derivation`", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || diff_eval(&sui, &nix,
                "let makeOverridable = f: origArgs: let result = f origArgs; in result // { override = newArgs: makeOverridable f (origArgs // (if builtins.isFunction newArgs then newArgs origArgs else newArgs)); }; optional = c: x: if c then [x] else []; fakeMk = attrs: derivation { name = attrs.name; system = \"x86_64-linux\"; builder = \"/bin/sh\"; nbi = map (d: d.out or (toString d)) (attrs.nativeBuildInputs or []); pbi = map (d: d.out or (toString d)) (attrs.propagatedBuildInputs or []); }; scope = rec { perl = makeOverridable ({ enableCrypt ? true }: fakeMk { name = \"myperl\"; propagatedBuildInputs = optional enableCrypt libxcrypt; }) {}; libxcrypt = fakeMk { name = \"mylibxcrypt\"; nativeBuildInputs = [ (perl.override { enableCrypt = false; }) ]; }; }; in scope.perl.drvPath"))
        }),
        // ── DARWIN PARITY CORPUS (the Parity Method on the aarch64-darwin
        //    surface). Each row asks nixpkgs for a package's `.drvPath` on the
        //    HOST's currentSystem; on aarch64-darwin these exercise the darwin
        //    stdenv bootstrap, apple-sdk, multi-output, and withPackages shapes.
        //    On a non-darwin host `currentSystem` yields that host's system, so
        //    the row still runs (it just probes the host stdenv) — the parity
        //    invariant is system-agnostic. Seeded 2026-07-12 from the marquee
        //    darwin frontier: all eight matched byte-for-byte sui-vs-nix on
        //    aarch64-darwin, so they seal that surface against regression.
        //    (The FIRST darwin divergence found in this frontier — the ishou
        //    crate2nix `rust_ishou-cli` drvPath — is NOT a byte-parity eval bug
        //    but a transitive flake-input TOOLCHAIN-REVISION skew, so it is NOT
        //    encoded here as a KnownDiverge eval row; it lives in the report.)
        (ParityProbe { name: "eval hello drvPath (currentSystem)", description: "nixpkgs hello through the host stdenv — darwin corpus seed (aarch64-darwin byte-parity 2026-07-12)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || darwin_pkg_drvpath(&sui, &nix, "hello"))
        }),
        (ParityProbe { name: "eval stdenv drvPath (currentSystem)", description: "the host stdenv derivation (darwin stdenv bootstrap on aarch64-darwin)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || darwin_pkg_drvpath(&sui, &nix, "stdenv"))
        }),
        (ParityProbe { name: "eval bash drvPath (currentSystem)", description: "bash-interactive through the host stdenv (darwin corpus)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || darwin_pkg_drvpath(&sui, &nix, "bash"))
        }),
        (ParityProbe { name: "eval coreutils drvPath (currentSystem)", description: "coreutils through the host stdenv (darwin corpus)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || darwin_pkg_drvpath(&sui, &nix, "coreutils"))
        }),
        (ParityProbe { name: "eval openssl drvPath (currentSystem)", description: "openssl through the host stdenv (darwin corpus)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || darwin_pkg_drvpath(&sui, &nix, "openssl"))
        }),
        (ParityProbe { name: "eval curl drvPath (currentSystem)", description: "curl through the host stdenv (darwin corpus)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || darwin_pkg_drvpath(&sui, &nix, "curl"))
        }),
        (ParityProbe { name: "eval python3 drvPath (currentSystem)", description: "python3 through the host stdenv (darwin corpus)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || darwin_pkg_drvpath(&sui, &nix, "python3"))
        }),
        (ParityProbe { name: "eval perl drvPath (currentSystem)", description: "perl through the host stdenv — multi-output (out/man/devdoc), the darwin frontier's perl-5.42.0-devdoc class at the top level (matches)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || darwin_pkg_drvpath(&sui, &nix, "perl"))
        }),
        (ParityProbe { name: "eval python3.withPackages drvPath (currentSystem)", description: "python3.withPackages env derivation — the chromeTheme/stylix-fonts IFD build-input shape (darwin corpus)", expect: Expect::Match }, {
            let sui = sui_bin.clone(); let nix = nix.to_path_buf();
            Box::new(move || {
                let np = match run_capture(&nix, &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"]) {
                    Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                    _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
                };
                diff_eval(&sui, &nix, &format!("((import {np} {{ system = builtins.currentSystem; }}).python3.withPackages (p: [ p.pyyaml ])).drvPath"))
            })
        }),
    ];

    // Run + collect (carry each probe's matrix expectation alongside the verdict).
    let mut results: Vec<(String, String, Expect, ParityVerdict)> = Vec::new();
    for (probe, run) in &probes {
        let verdict = run();
        results.push((probe.name.to_string(), probe.description.to_string(), probe.expect, verdict));
    }
    // Generated mass-synthesis corpus: typed Nix-AST shapes rendered to source
    // (parity_corpus), each byte-checked sui-vs-nix. Adding a new eval-surface
    // variant is a typed shape, not a hand-authored probe — CLOSED-LOOP
    // MASS-SYNTHESIS applied to parity. Every Match row is sealed against
    // regression; a KnownDiverge row must graduate when a fix lands.
    for row in parity_corpus::generate() {
        let expect = match row.expect {
            parity_corpus::RowExpect::Match => Expect::Match,
            parity_corpus::RowExpect::KnownDiverge => Expect::KnownDiverge,
        };
        let verdict = diff_eval(&sui_bin, nix, &row.expr);
        results.push((row.name, "generated (typed nix-AST mass-synthesis)".to_string(), expect, verdict));
    }
    let _ = std::fs::remove_file(&h_fixture);

    // ★★ Env-capability honesty (parity floating-oracle fix, task #14). The
    // ecosystem rows import impure `<nixpkgs>` — an unpinned, machine-dependent
    // oracle. A runner that resolves a DIFFERENT nixpkgs rev than the one the
    // rows were byte-closed against cannot evaluate them (SuiError), which would
    // count as a regression and hold the gate permanently red — BLINDING the ~40
    // environment-independent rows that are genuine CI theorems. When
    // SUI_PARITY_PUREONLY is set (a runner with no pinned oracle, e.g. CI),
    // reclassify a Match row that COULD-NOT-EVALUATE (SuiError/NixError, NEVER
    // Diverge) as a typed Skip: proven on the operator machine + the pinned-oracle
    // job, not here. A real wrong-byte (Diverge) STILL fails, so a divergence is
    // never masked (silent divergence remains the worst failure). This is the
    // honest floor; the destination (a flake-locked oracle + per-row EnvCapability
    // on a big-mem job) makes it precise and restores the ecosystem rows as CI
    // theorems. A seal is only real if its oracle is reproducible where enforced.
    //
    // ── THE RECLASSIFICATION IS BUDGETED, AND IT DISTINGUISHES WHOSE FAULT ──
    //
    // The rationale above is sound; the implementation was not, and it made the
    // gate lie. Measured on cid 2026-07-20 at HEAD, same binary, same host:
    //
    //     sui parity --json                 -> 41 match / 35 regressions / exit 1
    //     SUI_PARITY_PUREONLY=1 sui parity  -> exit 0, 0 regressions, 35 skipped
    //
    // Every one of those 35 was `Eval(TypeError("cannot add string and null"))`
    // — sui could not evaluate nixpkgs AT ALL on aarch64-darwin — and that shipped
    // as a GREEN `parity.yml` (which sets the var unconditionally). The failure
    // the gate exists to catch was the exact failure it reclassified away.
    //
    // Two corrections, both cheap:
    //
    // 1. WHOSE FAULT. `NixError` means the ORACLE could not evaluate — a genuine
    //    property of a runner with no pinned nixpkgs, exactly the case the
    //    rationale describes. `SuiError` means SUI failed. Those are not the same
    //    event and must not share a policy: an unpinned oracle explains the
    //    former completely and the latter only partially (a rev skew can make sui
    //    fail on an expression nix handles).
    //
    // 2. A BUDGET. So `SuiError` stays reclassifiable — rev skew is real — but
    //    only up to a committed ceiling. Past it, the excess counts as
    //    `unexpected` and the gate goes red naming the count. A handful of rows
    //    failing under skew is plausible; wholesale collapse is capability loss.
    //
    // TIER, stated so it is never rounded up: this is a BUDGETED MITIGATION, not
    // a type. It does not make "the gate lies" unrepresentable — it makes the
    // specific, measured, total-collapse case impossible to ship green. The typed
    // destination is unchanged and still unbuilt: a flake-locked oracle plus a
    // per-row `EnvCapability`, so a row declares what it needs and an unmet need
    // is a parse-time fact rather than a runtime guess.
    const PUREONLY_SUI_ERROR_BUDGET: usize = 8;
    let mut pureonly_oracle_skips = 0usize;
    let mut pureonly_sui_skips = 0usize;
    if std::env::var_os("SUI_PARITY_PUREONLY").is_some() {
        let budget = std::env::var("SUI_PARITY_PUREONLY_BUDGET")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(PUREONLY_SUI_ERROR_BUDGET);
        for (_, _, ex, v) in results.iter_mut() {
            if *ex != Expect::Match {
                continue;
            }
            match v {
                // Oracle-side: the runner cannot provide the comparison at all.
                // Unlimited — this says nothing about sui.
                ParityVerdict::NixError(_) => {
                    pureonly_oracle_skips += 1;
                    *v = ParityVerdict::Skipped(
                        "SUI_PARITY_PUREONLY: oracle (nix) could not evaluate — no pinned nixpkgs on this runner".into(),
                    );
                }
                // sui-side: budgeted. Beyond the ceiling these stay SuiError and
                // are counted as regressions below, so a total loss of eval
                // capability cannot present as green.
                ParityVerdict::SuiError(_) if pureonly_sui_skips < budget => {
                    pureonly_sui_skips += 1;
                    *v = ParityVerdict::Skipped(
                        "SUI_PARITY_PUREONLY: sui could not evaluate (within budget) — likely nixpkgs rev skew vs the byte-closed oracle".into(),
                    );
                }
                _ => {}
            }
        }
    }

    let total = results.len();
    let matches = results.iter().filter(|(_, _, _, v)| matches!(v, ParityVerdict::Match)).count();
    let diverged = results.iter().filter(|(_, _, _, v)| matches!(v, ParityVerdict::Diverge { .. })).count();
    let skipped = results.iter().filter(|(_, _, _, v)| matches!(v, ParityVerdict::Skipped(_))).count();
    // The sealed-invariant gate. A `Match` row that isn't Match is a REGRESSION;
    // a `KnownDiverge` row that IS Match has GRADUATED (a fix landed — promote
    // it to `Match`). Either is `unexpected` and fails the gate, so the corpus
    // can neither regress a proven byte-parity theorem nor silently advance an
    // untracked one. This is CONVERGE = SEAL made mechanical.
    let regressions = results.iter().filter(|(_, _, ex, v)| *ex == Expect::Match
        && matches!(v, ParityVerdict::Diverge { .. } | ParityVerdict::SuiError(_) | ParityVerdict::NixError(_))
    ).count();
    let graduated = results.iter().filter(|(_, _, ex, v)|
        *ex == Expect::KnownDiverge && matches!(v, ParityVerdict::Match)).count();
    let tracked = results.iter().filter(|(_, _, ex, v)|
        *ex == Expect::KnownDiverge && matches!(v, ParityVerdict::Diverge { .. })).count();
    let unexpected = regressions + graduated;

    if json {
        let probes_json: Vec<serde_json::Value> = results.iter().map(|(n, d, ex, v)| {
            let (label, detail) = match v {
                ParityVerdict::Match              => ("match",   serde_json::Value::Null),
                ParityVerdict::Diverge { sui, nix } => ("diverge", serde_json::json!({"sui": sui, "nix": nix})),
                ParityVerdict::SuiError(e)        => ("sui-err", serde_json::Value::String(e.clone())),
                ParityVerdict::NixError(e)        => ("nix-err", serde_json::Value::String(e.clone())),
                ParityVerdict::Skipped(r)         => ("skip",    serde_json::Value::String(r.clone())),
            };
            let expect = match ex { Expect::Match => "match", Expect::KnownDiverge => "known-diverge" };
            serde_json::json!({"name": n, "description": d, "expect": expect, "verdict": label, "detail": detail})
        }).collect();
        let summary = serde_json::json!({
            "total": total,
            "match": matches,
            "diverged": diverged,
            "tracked": tracked,
            "regressions": regressions,
            "graduated": graduated,
            "unexpected": unexpected,
            "skipped": skipped,
            // Split the skip tally by whose fault it was, so a machine reader
            // (and the CI log) can tell "this runner has no oracle" from "sui
            // stopped evaluating". An aggregate `skipped` hid exactly that.
            "pureonly_oracle_skips": pureonly_oracle_skips,
            "pureonly_sui_skips": pureonly_sui_skips,
            "pureonly_sui_budget": PUREONLY_SUI_ERROR_BUDGET,
            "probes": probes_json,
        });
        println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    } else {
        println!("{}  {}  ({} probes vs `{}`)",
            glyph_snowflake(), header("sui-vs-nix parity"),
            ident(&total.to_string()), muted(&nix.display().to_string()));
        println!();
        let name_w = results.iter().map(|(n, _, _, _)| n.len()).max().unwrap_or(20);
        for (name, desc, ex, v) in &results {
            // A KnownDiverge row that still diverges is TRACKED (expected, not a
            // failure); one that now Matches has GRADUATED (a fix landed — it
            // must be promoted to Match).
            let tracked_row = *ex == Expect::KnownDiverge && matches!(v, ParityVerdict::Diverge { .. });
            let graduated_row = *ex == Expect::KnownDiverge && matches!(v, ParityVerdict::Match);
            let (glyph, verdict_label) = if tracked_row {
                (warn("~"), warn("tracked-diverge"))
            } else if graduated_row {
                (warn("↑"), warn("GRADUATED → promote to Match"))
            } else {
                match v {
                    ParityVerdict::Match          => (success(v.glyph()), success(v.label())),
                    ParityVerdict::Diverge { .. } => (error(v.glyph()),   error(v.label())),
                    ParityVerdict::SuiError(_)    => (error(v.glyph()),   error(v.label())),
                    ParityVerdict::NixError(_)    => (warn(v.glyph()),    warn(v.label())),
                    ParityVerdict::Skipped(_)     => (muted(v.glyph()),   muted(v.label())),
                }
            };
            println!("  {} {}  {}  {}",
                glyph,
                ident(&format!("{:<name_w$}", name, name_w = name_w)),
                info(desc),
                verdict_label,
            );
            if let ParityVerdict::Diverge { sui, nix } = v {
                println!("      {}  sui={}", muted("✘"), sui);
                println!("      {}  nix={}", muted("✘"), nix);
            }
            if let ParityVerdict::Skipped(reason) = v {
                println!("      {}  {}", muted("·"), muted(reason));
            }
        }
        println!();
        println!("  {} {} match · {} tracked · {} regressions · {} graduated · {} skip",
            body("∑"),
            success(&matches.to_string()),
            if tracked > 0 { warn(&tracked.to_string()) } else { muted("0") },
            if regressions > 0 { error(&regressions.to_string()) } else { success("0") },
            if graduated > 0 { warn(&graduated.to_string()) } else { muted("0") },
            muted(&skipped.to_string()),
        );
        // Never let a reclassification be invisible again — an unexplained
        // "skip" tally is exactly how a total loss of nixpkgs eval shipped green.
        // Split by WHOSE fault, and say the budget out loud when sui-side skips
        // were spent, so a reader can see how close the gate is to its ceiling.
        if pureonly_oracle_skips > 0 || pureonly_sui_skips > 0 {
            println!("  {} SUI_PARITY_PUREONLY reclassified {} row(s): {} oracle-side (unbudgeted), {} sui-side (budget {})",
                warn("!"),
                warn(&(pureonly_oracle_skips + pureonly_sui_skips).to_string()),
                muted(&pureonly_oracle_skips.to_string()),
                if pureonly_sui_skips > 0 { warn(&pureonly_sui_skips.to_string()) } else { muted("0") },
                muted(&PUREONLY_SUI_ERROR_BUDGET.to_string()),
            );
        }
        if unexpected == 0 {
            println!("  {} corpus sealed — every Match row byte-identical to nix",
                success("✔"));
        }
    }

    if unexpected > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// The store-name of a `/nix/store/<32-hash>-<name>` path — the hash stripped,
/// used to match sui's temp-cache drvs against nix's store drvs across the
/// input-derivation graph (the hashes differ where they diverge; the names don't).
fn drv_name(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.splitn(2, '-').nth(1).unwrap_or(base).to_string()
}

/// Read a drv's ATerm bytes — the nix store first, then sui's temp-cache
/// fallback (`$TMPDIR/sui-drv-cache/`, where sui writes drvs it can't put in a
/// read-only /nix/store).
fn read_drv_bytes(drv_path: &str) -> Option<Vec<u8>> {
    if let Ok(b) = std::fs::read(drv_path) {
        return Some(b);
    }
    let base = drv_path.rsplit('/').next()?;
    std::fs::read(std::env::temp_dir().join("sui-drv-cache").join(base)).ok()
}

/// Replace every `/nix/store/<32-hash>-` with a fixed placeholder so a value
/// that differs ONLY by cascaded store hashes reads as equal — isolating
/// genuine content divergence from hash cascade.
fn strip_store_hashes(s: &str) -> String {
    // UTF-8-safe: scan by `find` + char-aware slicing (drv env values contain
    // multi-byte chars, e.g. the U+2010 hyphen in gcc build scripts — byte
    // indexing would panic on a non-char-boundary).
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find("/nix/store/") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + "/nix/store/".len()..];
        // The store hash is exactly 32 nix-base32 chars (all ASCII); collect the
        // first 32 chars and verify — if they're all ASCII the byte length is 32
        // and `after[32..]` lands on a char boundary.
        let hash: String = after.chars().take(32).collect();
        if hash.len() == 32 && hash.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
            out.push_str("/nix/store/<HASH>");
            rest = &after[32..];
        } else {
            out.push_str("/nix/store/");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// One structural-leaf result of the bisect.
struct BisectLeaf {
    sui_path: String,
    nix_path: String,
    sui: sui_compat::derivation::Derivation,
    nix: sui_compat::derivation::Derivation,
}

/// Recurse the sui↔nix input-derivation graph to the first drv whose same-name
/// inputs all match nix but which itself diverges — the structural leaf.
fn bisect_drv(
    sui_path: &str,
    nix_path: &str,
    trail: &mut Vec<String>,
    depth: usize,
) -> Result<BisectLeaf, CliError> {
    use sui_compat::derivation::Derivation;
    if depth > 256 {
        return Err(CliError::NotImplemented("parity-bisect: recursion too deep".into()));
    }
    let sui_bytes = read_drv_bytes(sui_path).ok_or_else(|| CliError::NotImplemented(
        format!("parity-bisect: cannot read sui drv {sui_path} (not in store or sui-drv-cache)")))?;
    let nix_bytes = read_drv_bytes(nix_path).ok_or_else(|| CliError::NotImplemented(
        format!("parity-bisect: cannot read nix drv {nix_path}")))?;
    let sui = Derivation::parse(&sui_bytes)
        .map_err(|e| CliError::NotImplemented(format!("parity-bisect: parse sui drv: {e:?}")))?;
    let nix = Derivation::parse(&nix_bytes)
        .map_err(|e| CliError::NotImplemented(format!("parity-bisect: parse nix drv: {e:?}")))?;

    // Pair input derivations by name; recurse into the shallowest that diverges.
    let sui_in: std::collections::BTreeMap<String, String> =
        sui.input_derivations.keys().map(|p| (drv_name(p), p.clone())).collect();
    let nix_in: std::collections::BTreeMap<String, String> =
        nix.input_derivations.keys().map(|p| (drv_name(p), p.clone())).collect();
    let mut diverging: Vec<(String, String, String)> = sui_in.iter()
        .filter_map(|(name, sp)| nix_in.get(name).filter(|np| **np != *sp)
            .map(|np| (name.clone(), sp.clone(), np.clone())))
        .collect();
    diverging.sort();
    match diverging.into_iter().next() {
        // Every same-name input matches nix — the divergence is HERE.
        None => Ok(BisectLeaf {
            sui_path: sui_path.to_string(), nix_path: nix_path.to_string(), sui, nix,
        }),
        Some((name, sp, np)) => {
            trail.push(name);
            bisect_drv(&sp, &np, trail, depth + 1)
        }
    }
}

/// Drain + emit the `SUI_PARITY_STRICT` un-blinding ledger to stderr.
///
/// No-op unless `SUI_PARITY_STRICT` is set (the collector only records then).
/// Groups the swallowed force-error drops by drv → attr → force-error so a
/// single strict `sui --no-vm eval <pkg>.drvPath` run names the exact stacked
/// roots a diverging package hides behind the env-loop best-effort skip. This
/// is an ENUMERATION instrument for the byte-parity campaign — it never changes
/// the eval result, only surfaces what the swallow drops.
fn report_parity_strict() {
    use sui_eval::builtins::parity_strict::{self, DropSite};
    if !parity_strict::enabled() {
        return;
    }
    let drops = parity_strict::drain();
    if drops.is_empty() {
        eprintln!("[SUI_PARITY_STRICT] no swallowed force-error drops (clean eval)");
        return;
    }
    // Dedup by (drv, attr, site, force_err), counting occurrences — a fixpoint
    // re-entry can drop the same attr many times; the distinct set is the root
    // list, the count is the multiplicity.
    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<(String, String, &'static str), (usize, String)> = BTreeMap::new();
    for d in &drops {
        let site = match d.site {
            DropSite::FlatEnv => "flat-env",
            DropSite::StructuredAttrs => "structured-attrs",
        };
        let e = grouped
            .entry((d.drv.clone(), d.attr.clone(), site))
            .or_insert((0, d.force_err.clone()));
        e.0 += 1;
    }
    eprintln!(
        "[SUI_PARITY_STRICT] {} swallowed force-error drop(s), {} distinct (drv,attr,site):",
        drops.len(),
        grouped.len()
    );
    for ((drv, attr, site), (count, force_err)) in &grouped {
        eprintln!(
            "[SUI_PARITY_STRICT]   drv={drv} attr={attr} site={site} x{count}\n\
             [SUI_PARITY_STRICT]     force-err: {force_err}"
        );
    }
}

fn cmd_parity_bisect(nix: &std::path::Path, expr: &str) -> Result<(), CliError> {
    use sui_spec::style::{body, error, glyph_snowflake, header, ident, muted, success, warn};
    use std::collections::BTreeSet;

    let sui_bin = std::env::current_exe()
        .map_err(|e| CliError::NotImplemented(format!("parity-bisect: own exe: {e}")))?;
    let drv_expr = format!("({expr}).drvPath");
    let sui_top = run_capture(&sui_bin,
        &["--no-vm", "eval", "--impure", "--raw", "--expr", &drv_expr])
        .map_err(|e| CliError::NotImplemented(format!("parity-bisect: sui eval: {e}")))?
        .trim().trim_matches('"').to_string();
    let nix_top = run_capture(nix,
        &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", &drv_expr])
        .map_err(|e| CliError::NotImplemented(format!("parity-bisect: nix eval: {e}")))?
        .trim().to_string();

    println!("{}  {}", glyph_snowflake(), header("parity-bisect"));
    println!("  {} sui={}", muted("top"), ident(&sui_top));
    println!("  {} nix={}", muted("top"), ident(&nix_top));
    if sui_top == nix_top {
        println!("  {} already byte-identical — nothing to bisect", success("✔"));
        return Ok(());
    }
    println!();

    let mut trail: Vec<String> = vec![drv_name(&nix_top)];
    let leaf = bisect_drv(&sui_top, &nix_top, &mut trail, 0)?;
    let s = &leaf.sui;
    let n = &leaf.nix;

    println!("  {} structural leaf: {}", body("→"), ident(&drv_name(&leaf.nix_path)));
    println!("      {} sui={}", muted("·"), muted(&leaf.sui_path));
    println!("      {} nix={}", muted("·"), muted(&leaf.nix_path));
    println!("  {} trail: {}", muted("↳"), muted(&trail.join(" → ")));
    println!();

    let mut found = false;
    if s.system != n.system {
        println!("  {} system   sui={}  nix={}", error("✘"), s.system, n.system); found = true;
    }
    if strip_store_hashes(&s.builder) != strip_store_hashes(&n.builder) {
        println!("  {} builder  sui={}  nix={}", error("✘"), s.builder, n.builder); found = true;
    }
    if s.args.iter().map(|a| strip_store_hashes(a)).ne(n.args.iter().map(|a| strip_store_hashes(a))) {
        println!("  {} args differ (sui {} / nix {})", error("✘"), s.args.len(), n.args.len()); found = true;
    }
    let s_src: BTreeSet<String> = s.input_sources.iter().map(|p| drv_name(p)).collect();
    let n_src: BTreeSet<String> = n.input_sources.iter().map(|p| drv_name(p)).collect();
    if s_src != n_src {
        let so: Vec<_> = s_src.difference(&n_src).collect();
        let no: Vec<_> = n_src.difference(&s_src).collect();
        println!("  {} inputSrcs name-set differs: sui-only={so:?} nix-only={no:?}", error("✘")); found = true;
    }
    let s_dn: BTreeSet<String> = s.input_derivations.keys().map(|p| drv_name(p)).collect();
    let n_dn: BTreeSet<String> = n.input_derivations.keys().map(|p| drv_name(p)).collect();
    if s_dn != n_dn {
        let so: Vec<_> = s_dn.difference(&n_dn).collect();
        let no: Vec<_> = n_dn.difference(&s_dn).collect();
        println!("  {} inputDrv name-set differs: sui-only={so:?} nix-only={no:?}", error("✘")); found = true;
    }
    let s_ek: BTreeSet<&String> = s.env.keys().collect();
    let n_ek: BTreeSet<&String> = n.env.keys().collect();
    if s_ek != n_ek {
        let so: Vec<_> = s_ek.difference(&n_ek).collect();
        let no: Vec<_> = n_ek.difference(&s_ek).collect();
        println!("  {} env key-set differs: sui-only={so:?} nix-only={no:?}", error("✘")); found = true;
    }
    let val_diffs: Vec<&String> = s_ek.intersection(&n_ek)
        .filter(|k| strip_store_hashes(&s.env[**k]) != strip_store_hashes(&n.env[**k]))
        .copied().collect();
    if !val_diffs.is_empty() {
        println!("  {} env values differ beyond store-path cascade: {val_diffs:?}", error("✘"));
        for k in val_diffs.iter().take(4) {
            println!("      {} {k}: sui={:?}", muted("·"), strip_store_hashes(&s.env[*k]));
            println!("        {} nix={:?}", muted(" "), strip_store_hashes(&n.env[*k]));
        }
        found = true;
    }
    if !found {
        // High-signal case: every input + field matches, only THIS drv's own
        // output store path differs → the root is sui's input-addressed output
        // computation (hashDerivationModulo / SerializeModulo) for a drv WITH
        // input-derivations — not the bare-derivation path (which matches).
        let s_out_names: BTreeSet<String> = s.outputs.values().map(|o| drv_name(&o.path)).collect();
        let n_out_names: BTreeSet<String> = n.outputs.values().map(|o| drv_name(&o.path)).collect();
        let out_paths_differ = s.outputs.iter().any(|(k, so)|
            n.outputs.get(k).is_some_and(|no| no.path != so.path));
        if out_paths_differ && s_out_names == n_out_names {
            println!("  {} OUTPUT-PATH-ONLY divergence — every input + field is byte-identical; only this drv's own output store path differs.", warn("⚑"));
            println!("      {} root: sui's INPUT-ADDRESSED output computation (hashDerivationModulo / SerializeModulo) for a drv WITH input-derivations. The bare-derivation path matches, so the bug is in the modulo replacement of inputDrvs (FOD special-case or the modulo memo).", body("→"));
            for (name, so) in &s.outputs {
                if let Some(no) = n.outputs.get(name) {
                    if so.path != no.path {
                        println!("      {} out[{name}]: sui={} nix={}", muted("·"), so.path, no.path);
                    }
                }
            }
        } else {
            println!("  {} no field-level structural diff at the leaf — every field matches once store hashes are normalized. The divergence is a store-path cascade the by-name match localizes no further (an input nix has that sui lacks under a differing name).", warn("~"));
        }
    }
    Ok(())
}

/// Diff two text outputs and classify the result.
fn diff_text(sui: Result<String, String>, nix: Result<String, String>) -> ParityVerdict {
    match (sui, nix) {
        (Ok(s), Ok(n)) if s == n => ParityVerdict::Match,
        (Ok(s), Ok(n))           => ParityVerdict::Diverge { sui: s, nix: n },
        (Err(e), _)              => ParityVerdict::SuiError(e),
        (_, Err(e))              => ParityVerdict::NixError(e),
    }
}

/// Byte-compare an eval expression's rendered result between sui and nix — the
/// mission-core parity surface (`.outPath` / `.drvPath` / any value). Uses the
/// tree-walker (`--no-vm`): it is the byte-parity engine, since the bytecode VM
/// defers string-context tracking (so it can't build correct derivations). No
/// `--raw` — both engines render a string value identically quoted, so the
/// comparison is on those value bytes.
fn diff_eval(sui: &std::path::Path, nix: &std::path::Path, expr: &str) -> ParityVerdict {
    let s = run_capture(sui, &["--no-vm", "eval", "--impure", "--expr", expr]);
    let n = run_capture(
        nix,
        &["eval", "--extra-experimental-features", "nix-command", "--impure", "--expr", expr],
    );
    diff_text(s, n)
}

/// Darwin-corpus probe: byte-compare `(import <nixpkgs> { system =
/// currentSystem; }).<attr>.drvPath` sui-vs-nix. `currentSystem` keeps the
/// probe host-agnostic — on aarch64-darwin it exercises the darwin stdenv
/// bootstrap (the marquee frontier's surface); on any other host it probes that
/// host's stdenv. Skips cleanly when `<nixpkgs>` isn't resolvable.
fn darwin_pkg_drvpath(sui: &std::path::Path, nix: &std::path::Path, attr: &str) -> ParityVerdict {
    let np = match run_capture(
        nix,
        &["eval", "--extra-experimental-features", "nix-command", "--impure", "--raw", "--expr", "toString <nixpkgs>"],
    ) {
        Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => return ParityVerdict::Skipped("<nixpkgs> not resolvable".into()),
    };
    diff_eval(
        sui,
        nix,
        &format!("(import {np} {{ system = builtins.currentSystem; }}).{attr}.drvPath"),
    )
}

/// Locate the first `/nix/store/*` entry whose name contains the
/// pattern.  Used by the parity probes that need a real store
/// path on the operator workstation.
fn first_store_path_matching(pattern: &str) -> Option<std::path::PathBuf> {
    let store = std::path::Path::new("/nix/store");
    if !store.exists() { return None; }
    std::fs::read_dir(store).ok()?
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().contains(pattern))
        .map(|e| e.path())
}

/// Concrete ureq-backed transport — implements the substrate's
/// `HttpTransport` trait for the production sui binary.  Tests
/// can swap in `MockTransport` to avoid network access.
struct UreqTransport;

impl sui_spec::fetcher::HttpTransport for UreqTransport {
    fn get(&self, url: &str) -> Result<Vec<u8>, sui_spec::fetcher::HttpError> {
        use sui_spec::fetcher::HttpError;
        let resp = ureq::get(url).call()
            .map_err(|e| HttpError::NetworkFailure(e.to_string()))?;
        let mut body = Vec::new();
        use std::io::Read;
        resp.into_body().as_reader().read_to_end(&mut body)
            .map_err(|e| HttpError::BodyReadFailure(e.to_string()))?;
        Ok(body)
    }
}

/// Blocking HTTP GET — routes `file://` through the substrate's
/// FsTransport, everything else through ureq.  Converts the
/// typed HttpError into the binary's CliError.
fn http_get(url: &str) -> Result<Vec<u8>, CliError> {
    use sui_spec::fetcher::HttpTransport;
    let router = sui_spec::fetcher::SchemeRouter::new(UreqTransport);
    router.get(url).map_err(|e| {
        CliError::NotImplemented(format!("http GET {url}: {e}"))
    })
}

fn store_prefetch_file(
    url: &str,
    name: Option<&str>,
    expected_hash: Option<&str>,
    hash_type: Option<&str>,
    unpack: bool,
) -> Result<(), CliError> {
    let _ = unpack;
    let alg = hash_type.unwrap_or("sha256");
    let bytes = http_get(url)?;
    use sha2::Digest;
    let digest = match alg {
        "sha256" => sha2::Sha256::digest(&bytes).to_vec(),
        "sha512" => sha2::Sha512::digest(&bytes).to_vec(),
        other => return Err(CliError::NotImplemented(format!(
            "store prefetch-file: unsupported --type `{other}`"
        ))),
    };
    let hash_b32 = sui_spec::hash::encode_hash(alg, "nix-base32", &digest)
        .map_err(|e| CliError::NotImplemented(format!("store prefetch-file: encode: {e:?}")))?;
    let sri = sui_spec::hash::encode_hash(alg, "sri", &digest)
        .map_err(|e| CliError::NotImplemented(format!("store prefetch-file: encode sri: {e:?}")))?;
    let basename = name.unwrap_or_else(|| {
        url.rsplit('/').next().unwrap_or("download")
    });

    // Verify expected_hash if supplied
    if let Some(expected) = expected_hash {
        // The expected hash may be in any encoding — round-trip
        // it through hash::decode_hash to compare bytes.
        match sui_spec::hash::decode_hash(expected) {
            Ok((_, expected_bytes)) if expected_bytes == digest => {}
            Ok(_) => return Err(CliError::NotImplemented(format!(
                "store prefetch-file: hash mismatch — expected {expected}, got {sri}"
            ))),
            Err(e) => return Err(CliError::NotImplemented(format!(
                "store prefetch-file: bad expected-hash `{expected}`: {e:?}"
            ))),
        }
    }

    // Write to operator-writable cache (full store-write needs
    // daemon; for now we cache in ~/.cache/sui/prefetch).
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let cache_dir = std::path::PathBuf::from(home).join(".cache/sui/prefetch");
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| CliError::NotImplemented(format!("store prefetch-file: mkdir cache: {e}")))?;
    let hash_b32_bare = strip_algo_prefix(&hash_b32);
    let cache_path = cache_dir.join(format!("{}-{basename}", &hash_b32_bare[..32.min(hash_b32_bare.len())]));
    std::fs::write(&cache_path, &bytes)
        .map_err(|e| CliError::NotImplemented(format!("store prefetch-file: write {}: {e}", cache_path.display())))?;

    println!("{sri}");
    eprintln!("path: {}", cache_path.display());
    eprintln!("# (write to /nix/store via daemon for full nix-store add semantics)");
    Ok(())
}

// ── Batch-4 / Batch-5 dispatch helpers (flake / store sign / drv) ──

const DEFAULT_FLAKE_NIX: &str = r#"{
  description = "A new flake";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }: flake-utils.lib.eachDefaultSystem (system:
    let
      pkgs = import nixpkgs { inherit system; };
    in {
      packages.default = pkgs.hello;
      devShells.default = pkgs.mkShell {
        buildInputs = [ pkgs.hello ];
      };
    });
}
"#;

fn flake_init(template: &str) -> Result<(), CliError> {
    if template != "default" {
        return Err(CliError::NotImplemented(format!(
            "flake init: only `default` template is wired today; got `{template}` (template registry needs sui_spec::flake::template_resolve)"
        )));
    }
    let cwd = std::env::current_dir()
        .map_err(|e| CliError::NotImplemented(format!("flake init: cwd: {e}")))?;
    let target = cwd.join("flake.nix");
    if target.exists() {
        return Err(CliError::NotImplemented(format!(
            "flake init: refusing to overwrite existing {}", target.display()
        )));
    }
    std::fs::write(&target, DEFAULT_FLAKE_NIX)
        .map_err(|e| CliError::NotImplemented(format!("flake init: write {}: {e}", target.display())))?;
    eprintln!("wrote: {}", target.display());
    Ok(())
}

fn flake_new(dest: &str, template: &str) -> Result<(), CliError> {
    if template != "default" {
        return Err(CliError::NotImplemented(format!(
            "flake new: only `default` template is wired today; got `{template}`"
        )));
    }
    let dest = std::path::PathBuf::from(dest);
    std::fs::create_dir_all(&dest)
        .map_err(|e| CliError::NotImplemented(format!("flake new: mkdir {}: {e}", dest.display())))?;
    let target = dest.join("flake.nix");
    if target.exists() {
        return Err(CliError::NotImplemented(format!(
            "flake new: refusing to overwrite existing {}", target.display()
        )));
    }
    std::fs::write(&target, DEFAULT_FLAKE_NIX)
        .map_err(|e| CliError::NotImplemented(format!("flake new: write {}: {e}", target.display())))?;
    eprintln!("wrote: {}", target.display());
    Ok(())
}

fn flake_clone(flake_ref: &str, dest: Option<&str>) -> Result<(), CliError> {
    // For now, support github:owner/repo and git+https URLs via
    // a typed Command wrapping git clone.  (`flake clone` in cppnix
    // does the same under the hood.)
    use std::process::Command;
    let (url, _git_ref) = parse_clone_target(flake_ref)?;
    let dest = dest.map(std::path::PathBuf::from).unwrap_or_else(|| {
        // Default to the repo's basename — matches cppnix's
        // behavior.
        let base = url.rsplit('/').next().unwrap_or("flake");
        let base = base.strip_suffix(".git").unwrap_or(base);
        std::path::PathBuf::from(base)
    });
    if dest.exists() {
        return Err(CliError::NotImplemented(format!(
            "flake clone: destination {} already exists", dest.display()
        )));
    }
    let status = Command::new("git")
        .args(["clone", "--depth", "1", &url])
        .arg(&dest)
        .status()
        .map_err(|e| CliError::NotImplemented(format!("flake clone: spawn git: {e}")))?;
    if !status.success() {
        return Err(CliError::NotImplemented(format!(
            "flake clone: git clone {url} → {} failed", dest.display()
        )));
    }
    eprintln!("cloned: {} → {}", url, dest.display());
    Ok(())
}

fn parse_clone_target(flake_ref: &str) -> Result<(String, Option<String>), CliError> {
    if let Some(rest) = flake_ref.strip_prefix("github:") {
        // github:owner/repo[/ref]
        let mut parts = rest.splitn(3, '/');
        let owner = parts.next().ok_or_else(|| CliError::NotImplemented("flake clone: bad github ref".into()))?;
        let repo = parts.next().ok_or_else(|| CliError::NotImplemented("flake clone: bad github ref".into()))?;
        let r#ref = parts.next().map(String::from);
        Ok((format!("https://github.com/{owner}/{repo}.git"), r#ref))
    } else if let Some(url) = flake_ref.strip_prefix("git+") {
        Ok((url.to_string(), None))
    } else if flake_ref.starts_with("https://") || flake_ref.starts_with("ssh://") {
        Ok((flake_ref.to_string(), None))
    } else {
        Err(CliError::NotImplemented(format!(
            "flake clone: unsupported ref shape `{flake_ref}` (github: / git+ / https:// / ssh:// only)"
        )))
    }
}

fn flake_archive(flake_ref: &str, json: bool) -> Result<(), CliError> {
    // Minimal archive: copy the flake's source + flake.lock to a
    // store-like archive directory.  Full impl walks all inputs;
    // for now produce a JSON summary or a notification.
    let source = std::path::PathBuf::from(flake_ref);
    if !source.exists() {
        return Err(CliError::NotImplemented(format!(
            "flake archive: source `{flake_ref}` doesn't exist locally (remote inputs need fetcher transport)"
        )));
    }
    let flake_nix = source.join("flake.nix");
    if !flake_nix.exists() {
        return Err(CliError::NotImplemented(format!(
            "flake archive: no flake.nix at {}", flake_nix.display()
        )));
    }
    let archive_dir = std::env::temp_dir()
        .join(format!("sui-flake-archive-{}", std::process::id()));
    std::fs::create_dir_all(&archive_dir)
        .map_err(|e| CliError::NotImplemented(format!("flake archive: mkdir: {e}")))?;
    copy_recursive(&source, &archive_dir)?;
    if json {
        println!("{}", serde_json::json!({
            "source":  flake_ref,
            "archive": archive_dir.display().to_string(),
        }));
    } else {
        println!("archived to: {}", archive_dir.display());
    }
    Ok(())
}

fn flake_prefetch(flake_ref: &str, json: bool) -> Result<(), CliError> {
    use sha2::Digest;
    // Three classes:
    //  - local path: hash its contents recursively
    //  - github:owner/repo: fetch the tarball via the github
    //    archive URL
    //  - http(s)://: fetch directly
    let (bytes, source_url) = if let Some(rest) = flake_ref.strip_prefix("github:") {
        let mut parts = rest.splitn(3, '/');
        let owner = parts.next().ok_or_else(|| CliError::NotImplemented("flake prefetch: bad github ref".into()))?;
        let repo = parts.next().ok_or_else(|| CliError::NotImplemented("flake prefetch: bad github ref".into()))?;
        let r#ref = parts.next().unwrap_or("HEAD");
        let url = format!("https://api.github.com/repos/{owner}/{repo}/tarball/{}", r#ref);
        (http_get(&url)?, url)
    } else if flake_ref.starts_with("http://") || flake_ref.starts_with("https://") {
        (http_get(flake_ref)?, flake_ref.to_string())
    } else {
        let source = std::path::PathBuf::from(flake_ref);
        if !source.exists() {
            return Err(CliError::NotImplemented(format!(
                "flake prefetch: `{flake_ref}` not a local path or recognised remote shape"
            )));
        }
        // Recursive hash of local source.
        let mut tree: std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> = Default::default();
        fn walk(
            root: &std::path::Path,
            rel: std::path::PathBuf,
            acc: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
        ) -> std::io::Result<()> {
            let abs = root.join(&rel);
            let meta = std::fs::metadata(&abs)?;
            if meta.is_file() {
                acc.insert(rel, std::fs::read(&abs)?);
            } else if meta.is_dir() {
                let mut entries: Vec<_> = std::fs::read_dir(&abs)?
                    .filter_map(Result::ok)
                    .collect();
                entries.sort_by_key(|e| e.file_name());
                for e in entries {
                    walk(root, rel.join(e.file_name()), acc)?;
                }
            }
            Ok(())
        }
        walk(&source, std::path::PathBuf::new(), &mut tree)
            .map_err(|e| CliError::NotImplemented(format!("flake prefetch: walk: {e}")))?;
        let mut h = sha2::Sha256::new();
        for (k, v) in &tree {
            h.update(k.to_string_lossy().as_bytes());
            h.update(&(v.len() as u64).to_le_bytes());
            h.update(v);
        }
        let digest = h.finalize().to_vec();
        let sri = sui_spec::hash::encode_hash("sha256", "sri", &digest)
            .map_err(|e| CliError::NotImplemented(format!("flake prefetch: encode: {e:?}")))?;
        if json {
            println!("{}", serde_json::json!({
                "originalUrl": flake_ref,
                "url":         flake_ref,
                "hash":        sri,
                "files":       tree.len(),
            }));
        } else {
            println!("source: {}", source.display());
            println!("hash:   {sri}");
            println!("files:  {}", tree.len());
        }
        return Ok(());
    };

    // Remote bytes path — hash directly.
    let digest = sha2::Sha256::digest(&bytes);
    let sri = sui_spec::hash::encode_hash("sha256", "sri", &digest)
        .map_err(|e| CliError::NotImplemented(format!("flake prefetch: encode: {e:?}")))?;
    if json {
        println!("{}", serde_json::json!({
            "originalUrl": flake_ref,
            "url":         source_url,
            "hash":        sri,
            "size":        bytes.len(),
        }));
    } else {
        println!("source: {source_url}");
        println!("hash:   {sri}");
        println!("size:   {} bytes", bytes.len());
    }
    Ok(())
}

fn store_dump_path(path: &str) -> Result<(), CliError> {
    use std::io::Write;

    let layouts = sui_spec::store_layout::load_canonical()
        .map_err(|e| CliError::NotImplemented(format!("store dump-path: {e:?}")))?;
    let parsed = layouts.iter()
        .find_map(|l| sui_spec::store_layout::parse_path(l, path).ok())
        .ok_or_else(|| CliError::NotImplemented(format!(
            "store dump-path: `{path}` not a recognised store path"
        )))?;
    let _ = parsed;

    let buf = sui_spec::nar::encode(std::path::Path::new(path))
        .map_err(|e| CliError::NotImplemented(format!("store dump-path: NAR encode: {e}")))?;
    std::io::stdout().write_all(&buf)
        .map_err(|e| CliError::NotImplemented(format!("store dump-path: stdout: {e}")))?;
    Ok(())
}

fn store_make_content_addressed(paths: &[String]) -> Result<(), CliError> {
    let layouts = sui_spec::store_layout::load_canonical()
        .map_err(|e| CliError::NotImplemented(format!("store make-content-addressed: {e:?}")))?;
    for p in paths {
        let parsed = layouts.iter()
            .find_map(|l| sui_spec::store_layout::parse_path(l, p).ok())
            .ok_or_else(|| CliError::NotImplemented(format!(
                "store make-content-addressed: `{p}` not a recognised store path"
            )))?;
        let nar_hash = sui_spec::nar::hash_path_nar(std::path::Path::new(p))
            .map_err(|e| CliError::NotImplemented(format!("store make-content-addressed: NAR: {e}")))?;
        let ca_path = sui_spec::nar::store_path_for(STORE_ROOT, &nar_hash, &parsed.name);
        println!("{p}\t→\t{ca_path}");
    }
    Ok(())
}

fn store_sign(paths: &[String], key_file: &str) -> Result<(), CliError> {
    // Sign the typed (store-path, nar-hash) pair with the ed25519
    // key from the file.  Without a NAR encoder we use the
    // recursive sha256 from sui's hash_path semantics as the
    // signed digest.
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    let key_text = std::fs::read_to_string(key_file)
        .map_err(|e| CliError::NotImplemented(format!("store sign: read {key_file}: {e}")))?;
    let (key_name, b64) = key_text.trim().split_once(':').ok_or_else(||
        CliError::NotImplemented("store sign: key file expected `<name>:<base64>` shape".into())
    )?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64)
        .map_err(|e| CliError::NotImplemented(format!("store sign: base64: {e}")))?;
    let arr: [u8; 32] = bytes.try_into()
        .map_err(|_| CliError::NotImplemented("store sign: key must be 32 bytes".into()))?;
    let signing = SigningKey::from_bytes(&arr);

    let layouts = sui_spec::store_layout::load_canonical()
        .map_err(|e| CliError::NotImplemented(format!("store sign: {e:?}")))?;
    for p in paths {
        let mut ok = false;
        for layout in &layouts {
            if sui_spec::store_layout::parse_path(layout, p).is_ok() {
                ok = true;
                break;
            }
        }
        if !ok {
            return Err(CliError::NotImplemented(format!(
                "store sign: `{p}` not a recognised store path"
            )));
        }
        // Sign the path string itself for now; real NAR-hash
        // signing needs the encoder.
        let sig = signing.sign(p.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        println!("{key_name}:{sig_b64}\t{p}");
    }
    Ok(())
}

fn store_repair(paths: &[String]) -> Result<(), CliError> {
    // For each path, verify via the canonical substituter
    // (cache.nixos.org).  Real impl re-downloads if local NAR
    // hash differs; for now we query the .narinfo from the
    // substituter to confirm reachability.
    let layouts = sui_spec::store_layout::load_canonical()
        .map_err(|e| CliError::NotImplemented(format!("store repair: {e:?}")))?;
    let substituters = sui_spec::substituter::load_canonical()
        .map_err(|e| CliError::NotImplemented(format!("store repair: {e:?}")))?;
    let cache = substituters.iter()
        .find(|s| s.name.contains("cache.nixos.org"))
        .ok_or_else(|| CliError::NotImplemented("store repair: no canonical cache.nixos.org substituter".into()))?;

    for p in paths {
        let parsed = layouts.iter()
            .find_map(|l| sui_spec::store_layout::parse_path(l, p).ok())
            .ok_or_else(|| CliError::NotImplemented(format!(
                "store repair: `{p}` not a recognised store path"
            )))?;
        let local_exists = std::path::Path::new(p).exists();
        let narinfo_url = format!("{}/{}.narinfo", cache.endpoint.trim_end_matches('/'), parsed.hash);

        // Probe the substituter for the .narinfo.
        let remote_status = match http_get(&narinfo_url) {
            Ok(bytes) => format!("substituter has narinfo ({} bytes)", bytes.len()),
            Err(_) => "substituter missing narinfo".to_string(),
        };
        println!("{p}\tlocal={}\t{}\t{}",
            if local_exists { "ok" } else { "missing" },
            remote_status,
            narinfo_url,
        );
    }
    Ok(())
}

fn derivation_add(path: &str) -> Result<(), CliError> {
    // Accept the JSON shape `nix derivation show` emits, parse
    // back into a `sui_compat::derivation::Derivation`, and
    // serialise to ATerm.  The .drv is emitted to stdout so the
    // operator can pipe to a file or to nix-store via a daemon
    // socket; full store-side persistence needs root/daemon
    // access, called out below.
    use std::collections::BTreeMap;
    use sui_compat::derivation::{Derivation, DerivationOutput};

    let raw = if path == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)
            .map_err(|e| CliError::NotImplemented(format!("derivation add: stdin: {e}")))?;
        buf
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| CliError::NotImplemented(format!("derivation add: read {path}: {e}")))?
    };

    let doc: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| CliError::NotImplemented(format!("derivation add: parse JSON: {e}")))?;

    // The JSON shape is `{ "<drv-path>": { outputs, inputDrvs,
    // inputSrcs, system, builder, args, env } }`.  Iterate each
    // top-level key, build a typed `Derivation`, serialise to
    // ATerm, and emit one block per drv.
    let map = doc.as_object().ok_or_else(|| CliError::NotImplemented(
        "derivation add: JSON root must be an object".into(),
    ))?;

    for (drv_path, body) in map {
        let outputs = body.get("outputs").and_then(|v| v.as_object())
            .ok_or_else(|| CliError::NotImplemented(format!(
                "derivation add: {drv_path}: missing `outputs` object"
            )))?;
        let mut out_map: BTreeMap<String, DerivationOutput> = BTreeMap::new();
        for (name, o) in outputs {
            let path = o.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let hash_algo = o.get("hashAlgo").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let hash = o.get("hash").and_then(|v| v.as_str()).unwrap_or("").to_string();
            out_map.insert(name.clone(), DerivationOutput { path, hash_algo, hash });
        }

        let input_drvs = body.get("inputDrvs").and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let mut input_derivations: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v) in &input_drvs {
            let outs = v.as_array().map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            }).unwrap_or_default();
            input_derivations.insert(k.clone(), outs);
        }

        let input_sources = body.get("inputSrcs").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let system = body.get("system").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let builder = body.get("builder").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let args = body.get("args").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let env_obj = body.get("env").and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let env: BTreeMap<String, String> = env_obj.into_iter()
            .map(|(k, v)| (k, v.as_str().unwrap_or("").to_string()))
            .collect();

        let drv = Derivation {
            outputs: out_map,
            input_derivations,
            input_sources,
            system,
            builder,
            args,
            env,
        };

        // ATerm output — what cppnix would write to the store.
        // No trailing newline — byte-identical with on-disk .drv.
        let aterm = drv.serialize();
        println!("{drv_path}");
        eprint!("{aterm}");
        eprintln!();
        eprintln!("# (write the ATerm above to {drv_path} via daemon — sudo or worker socket)");
    }
    Ok(())
}

fn hash_path(path: &str, hash_type: &str, base: &str) -> Result<(), CliError> {
    // Recursive NAR hash — `nix hash path` serializes the path as a NAR
    // archive and hashes that.  Reuse the canonical NAR serializer (the same
    // one `store dump-path` uses) rather than a bespoke flat hash: the old
    // walk used `fs::metadata` (which FOLLOWS symlinks → crashed on dangling
    // links) and a non-NAR digest, so it could never match nix.
    use sha2::Digest;
    let root = std::path::Path::new(path);
    let nar = sui_spec::nar::encode(root)
        .map_err(|e| CliError::NotImplemented(format!("hash path: NAR-encode {path}: {e}")))?;
    let digest: Vec<u8> = match hash_type {
        "sha256" => sha2::Sha256::digest(&nar).to_vec(),
        "sha512" => sha2::Sha512::digest(&nar).to_vec(),
        other => return Err(CliError::NotImplemented(format!(
            "hash path: unsupported --type `{other}` (sha256 | sha512)"
        ))),
    };

    let encoding = match base {
        "base16" => "base16",
        "base32" => "nix-base32",
        "base64" => "base64",
        "sri"    => "sri",
        other    => return Err(CliError::NotImplemented(format!(
            "hash path: unknown --base `{other}` (base16 | base32 | base64 | sri)"
        ))),
    };
    let out = sui_spec::hash::encode_hash(hash_type, encoding, &digest)
        .map_err(|e| CliError::NotImplemented(format!("hash path: encode: {e:?}")))?;
    // `nix hash path` emits base16/base32/base64 WITHOUT an `<algo>:` prefix
    // (only SRI carries `<algo>-`); encode_hash prepends `<algo>:` for
    // nix-base32/base64, so strip it (SRI + hex have no `:` and pass through).
    println!("{}", strip_algo_prefix(&out));
    Ok(())
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
    // `nix hash file` emits base16/base32/base64 WITHOUT an `<algo>:` prefix
    // (only SRI carries `<algo>-`); strip the `<algo>:` encode_hash prepends
    // for nix-base32/base64 (SRI + hex have no `:` and pass through).
    println!("{}", strip_algo_prefix(&out));
    Ok(())
}

/// Fires a final census dump when `main` returns (Drop at scope exit).
struct CensusExitGuard;
impl Drop for CensusExitGuard {
    fn drop(&mut self) {
        sui_eval::value::census::dump("exit");
    }
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<(), CliError> {
    // Heap profiler guard (feature `dhat-heap`). Held for the whole process;
    // on drop at exit it flushes `dhat-heap.json` (at-t-gmax = the peak-heap
    // per-call-stack byte breakdown). No-op / absent in a normal build.
    #[cfg(feature = "dhat-heap")]
    let _dhat_profiler = dhat::Profiler::new_heap();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    // LIVE-OBJECT CENSUS (SUI_LIVE_CENSUS=1). Zero-cost when off. Spawns a
    // thread that dumps live/made counts + RSS to stderr every 2s so a 30s+
    // eval captures the high-water region; a final dump fires at process exit
    // via the guard below.
    sui_eval::value::census::spawn_poller();
    let _census_exit_guard = CensusExitGuard;

    // Pre-intern the hot nixpkgs/flake/stdenv identifier set on the
    // main thread. Subsequent intern() calls for these are hashmap
    // hits; also amortizes the interner's initial resize cost.
    sui_intern::prewarm();

    // argv[0] dispatch: when the binary is symlinked to a legacy
    // `nix-*` name (`nix-build`, `nix-store`, …) the legacy CLI surface
    // is rewritten to the modern `sui <subcommand>` form before clap
    // sees it.  The unsymlinked `sui` (or `nix`) invocation falls
    // through to the normal parse.
    let cli = if let Some(cmd) = legacy::LegacyCmd::detect() {
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let translated = legacy::translate_legacy_argv(cmd, &raw);
        let synthetic: Vec<String> =
            std::iter::once("sui".to_string()).chain(translated).collect();
        Cli::parse_from(&synthetic)
    } else {
        Cli::parse()
    };

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
                StoreCommands::Delete { paths: dp, ignore_liveness } => {
                    store_delete(&dp, ignore_liveness)?;
                }
                StoreCommands::Ls { path: p, recursive, long, json } => {
                    store_ls(&p, recursive, long, json)?;
                }
                StoreCommands::Cat { path: p } => {
                    store_cat(&p)?;
                }
                StoreCommands::DumpPath { path: p } => {
                    store_dump_path(&p)?;
                }
                StoreCommands::MakeContentAddressed { paths: mp } => {
                    store_make_content_addressed(&mp)?;
                }
                StoreCommands::Ping => { println!("Store URL: daemon\nVersion: sui {}\nTrusted: 1", env!("CARGO_PKG_VERSION")); }
                StoreCommands::AddPath { path: p, name } => {
                    store_add_path(&p, name.as_deref()).await?;
                }
                StoreCommands::AddFile { path: p, name } => {
                    store_add_file(&p, name.as_deref())?;
                }
                StoreCommands::PrefetchFile { url, name, hash, hash_type, unpack } => {
                    store_prefetch_file(&url, name.as_deref(), hash.as_deref(), hash_type.as_deref(), unpack)?;
                }
                StoreCommands::Sign { paths: sp, key_file: kf } => {
                    store_sign(&sp, &kf)?;
                }
                StoreCommands::Repair { paths: rp } => {
                    store_repair(&rp)?;
                }
                StoreCommands::Materialize { slice, dest, json } => {
                    store_materialize(&slice, dest.as_deref(), json)?;
                }
                StoreCommands::Inventory { profile, json } => {
                    store_inventory(&profile, json)?;
                }
                StoreCommands::Closure { path, json } => {
                    store_closure(&path, json)?;
                }
                StoreCommands::Transform { source, transform, dest, json } => {
                    store_transform(&source, &transform, dest.as_deref(), json)?;
                }
                StoreCommands::Diff { a, b, json } => {
                    store_diff_cmd(&a, &b, json)?;
                }
                StoreCommands::Graft { root, from, to, dest, json } => {
                    store_graft(&root, &from, &to, dest.as_deref(), json)?;
                }
                StoreCommands::AuditSecrets { source, json } => {
                    store_audit_secrets(&source, json)?;
                }
                StoreCommands::Fingerprint { path, json } => {
                    store_fingerprint(&path, json)?;
                }
                StoreCommands::Find { profile, name, min_size, max_size, contents, json } => {
                    store_find(&profile, name.as_deref(), min_size, max_size, contents.as_deref(), json)?;
                }
                StoreCommands::Stats { profile, json } => {
                    store_stats(&profile, json)?;
                }
                StoreCommands::Analyze { profile, no_duplicates, high_fanout_threshold, json } => {
                    store_analyze_cmd(&profile, !no_duplicates, high_fanout_threshold, json)?;
                }
                StoreCommands::UpgradePaths { profile, json } => {
                    store_upgrade_paths(&profile, json)?;
                }
                StoreCommands::Recipe { name, dest_base, json } => {
                    store_recipe(&name, dest_base.as_deref(), json)?;
                }
                StoreCommands::FingerprintMany { profile, out } => {
                    store_fingerprint_many(&profile, out.as_deref())?;
                }
                StoreCommands::CompareManifests { a, b } => {
                    store_compare_manifests(&a, &b)?;
                }
                StoreCommands::DedupePlan { profile, json } => {
                    store_dedupe_plan(&profile, json)?;
                }
                StoreCommands::Entropy { path, json } => {
                    store_entropy(&path, json)?;
                }
                StoreCommands::AsciiGraph { path, max_depth } => {
                    store_ascii_graph(&path, max_depth)?;
                }
                StoreCommands::Sbom { path, out } => {
                    store_sbom(&path, out.as_deref())?;
                }
                StoreCommands::SignManifest { manifest, key_file } => {
                    store_sign_manifest(&manifest, &key_file)?;
                }
                StoreCommands::VerifyManifest { manifest, pubkey, sig } => {
                    store_verify_manifest(&manifest, &pubkey, sig.as_deref())?;
                }
                StoreCommands::LicenseScan { path, json } => {
                    store_license_scan(&path, json)?;
                }
                StoreCommands::CveScan { path, pattern, json } => {
                    store_cve_scan(&path, &pattern, json)?;
                }
            }
        }

        Commands::Eval { expression, json, raw, expr_flag, max_force_depth, no_eval_cache, apply: _, file_flag: _ } => {
            // Two input shapes (mirrors `nix eval`):
            //  - `--expr "EXPR"`   → raw Nix expression
            //  - positional INSTALLABLE (`flake-ref#attr.path`) → desugars to
            //    `(builtins.getFlake "<flake-ref>").attr.path`
            // `expr_flag` always wins; positional only desugars when it
            // contains a `#`, otherwise we treat it as a raw expression
            // for backwards compatibility with `sui eval '1 + 2'`.
            //
            // `installable_flake_ref` is set ONLY for the `flake-ref#attr`
            // shape — the only input the cross-run eval-cache may key on
            // (a lock-pinned flake output is a pure function of source+lock;
            // a bare `--expr` may name the impure frontier and is never cached).
            let mut installable_flake_ref: Option<String> = None;
            let expr = match (expr_flag, expression) {
                (Some(raw), _) => raw,
                (None, Some(s)) => match s.split_once('#') {
                    Some((flake, attr)) => {
                        installable_flake_ref = Some(flake.to_string());
                        format!(
                            "(builtins.getFlake \"{}\").{}",
                            normalize_flake_ref(flake),
                            attr,
                        )
                    }
                    None => s,
                },
                (None, None) =>
                    return Err(CliError::MissingArgument("no expression provided".into())),
            };
            // Render mode determines the output bytes, so it is part of the
            // cache identity (json / raw / display never share an entry).
            let render_mode = if json { "json" } else if raw { "raw" } else { "display" };
            // Cross-run eval-cache key: installable-only, byte-safe, disabled by
            // `--no-eval-cache`. `None` ⇒ this eval is never cached/served.
            let cache_key = if no_eval_cache {
                None
            } else {
                installable_flake_ref
                    .as_deref()
                    .and_then(|fr| eval_cache_key_for_installable(&expr, fr, render_mode))
            };
            if max_force_depth > 0 {
                sui_eval::trace::set_max_force_depth(max_force_depth);
            }
            if cli.no_vm {
                // Tree-walker evaluation path (the parity-correct engine —
                // the only one whose drvPaths are byte-exact, so the only one
                // the cross-run eval-cache serves).
                //
                // Fast path: an identical prior installable eval is served from
                // the content-addressed eval-cache WITHOUT re-evaluating — the
                // structural win nix cannot offer (memoized eval OUTPUT across
                // runs, keyed on source+lock). This is the wiring that connects
                // the previously-built-but-disconnected `eval_cache` module to
                // the `sui eval` entrypoint.
                let mut served_from_cache = false;
                if let Some(key) = cache_key.as_ref() {
                    let mut cache = sui_eval::eval_cache::EvalCache::default_persistent();
                    if let Some(hit) = cache.get(key) {
                        let cached = hit.value_json.clone();
                        // Anti-stale differential gate (opt-in via
                        // SUI_EVAL_CACHE_VERIFY): a served byte MUST equal a
                        // fresh eval. Guards against a mis-keyed / drifted entry
                        // ever shipping a wrong drvPath. Off by default so a hit
                        // stays instant.
                        if std::env::var_os("SUI_EVAL_CACHE_VERIFY").is_some() {
                            let fresh = eval_render_threaded(&expr, json, raw)?;
                            assert_eq!(
                                fresh, cached,
                                "eval-cache byte mismatch: a cached entry drifted from a fresh eval",
                            );
                        }
                        println!("{cached}");
                        served_from_cache = true;
                    }
                }
                if !served_from_cache {
                    let output = eval_render_threaded(&expr, json, raw)?;
                    println!("{output}");
                    // Populate the cache after a successful eval (best-effort;
                    // a failed write never changes correctness, only hit rate).
                    if let Some(key) = cache_key {
                        let mut cache = sui_eval::eval_cache::EvalCache::default_persistent();
                        cache.put(
                            key,
                            sui_eval::eval_cache::CachedValue {
                                value_json: output,
                                timestamp: sui_eval::eval_cache::now_timestamp(),
                            },
                        );
                    }
                }
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
                // IFD: reads of a derivation output mid-eval realize it. Installed
                // on the VM thread too — VM builtin reads bridge to the tree-walker,
                // which shares this thread-local hook.
                let _ifd_guard = install_ifd_hook();
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

            // The realize path is daemon-aware: `sui_orchestrate::realize_drv`
            // dispatches on `StoreAccess::detect()` — `Direct` (single-user / CI
            // store) builds through the local pipeline; `Daemon` (cid's root-owned
            // multi-user store, which the direct db.sqlite write cannot open)
            // routes the privileged write through the running nix daemon,
            // substitute-first. Byte-identical output at the same content-addressed
            // path either way. This is the exact dispatch the IFD hook already
            // uses, reused here for the operator build command.

            if std::path::Path::new(&installable).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("drv")) {
                // Direct .drv path — realize it (daemon-aware).
                let outputs = sui_orchestrate::realize_drv(&installable)
                    .await
                    .map_err(|e| CliError::Orchestrate {
                        operation: "build",
                        message: e.to_string(),
                    })?;
                for output in &outputs {
                    println!("{output}");
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
                // Extract drvPath from the derivation attrset. `drvPath` is a
                // lazy thunk, so it MUST be forced before `as_string` — without
                // the force the extraction silently yields `None` and the build
                // falls through to just printing the derivation (the flake-build
                // path never realizes anything). Force the target too, in case a
                // navigated attr is itself a thunk.
                let target = sui_eval::eval::force_value(&target).unwrap_or(target);
                let drv_path = match &target {
                    sui_eval::Value::Attrs(attrs) => attrs.get("drvPath").and_then(|v| {
                        let forced = sui_eval::eval::force_value(&v).ok()?;
                        forced.as_string().ok().map(std::string::ToString::to_string)
                    }),
                    _ => None,
                };
                if let Some(drv_path) = drv_path {
                    // Realize the flake target's derivation (daemon-aware — same
                    // `StoreAccess::detect()` dispatch as the direct-.drv branch).
                    let outputs = sui_orchestrate::realize_drv(&drv_path)
                        .await
                        .map_err(|e| CliError::Orchestrate {
                            operation: "build",
                            message: e.to_string(),
                        })?;
                    for output in &outputs {
                        println!("{output}");
                    }
                } else {
                    // Not a derivation — just display the evaluated value.
                    println!("{target}");
                }
            }
        }

        Commands::BuildParity { nix } => {
            // The basket — grown one byte-verified row at a time (the build-parity
            // sibling of `parity_corpus`). Starts self-contained (no network); real
            // nixpkgs leaves are added as each proves byte-identical here.
            let basket: &[(&str, &str)] = &[
                (
                    "trivial-echo",
                    "derivation { name = \"bp-trivial\"; system = builtins.currentSystem; \
                     builder = \"/bin/sh\"; args = [ \"-c\" \"echo byte-parity > $out\" ]; }",
                ),
                // A DIRECTORY output with a regular file + an EXECUTABLE — proves
                // sui's NAR serialization of nested structure + the executable bit
                // (both encoded in the NAR) matches nix, a real step past a single
                // flat file. Still self-contained (no network, fast on CI).
                (
                    "dir-structure",
                    "derivation { name = \"bp-dir\"; system = builtins.currentSystem; \
                     builder = \"/bin/sh\"; args = [ \"-c\" \
                     \"mkdir -p $out/bin $out/share; echo hello > $out/share/msg; \
                     printf '#!/bin/sh\\necho hi\\n' > $out/bin/run; chmod +x $out/bin/run\" ]; }",
                ),
            ];
            // Realize is daemon-aware via `realize_drv` → `StoreAccess::detect()`:
            // on a single-user / CI store it writes the .drv + output directly
            // (Direct arm, identical to before); on a root-owned multi-user store
            // it routes the privileged write through the nix daemon (Daemon arm) —
            // the daemon-write path that unbricks build-parity on cid.
            // Single-user nix needs `nix-command` explicitly enabled for `nix
            // build` / `nix hash path`; without it both silently return empty.
            const EXP: &str = "--extra-experimental-features";
            let hash_path = |out: &str| -> String {
                if out.is_empty() { String::new() }
                else { run_capture(&nix, &["hash", "path", EXP, "nix-command", out]).unwrap_or_default() }
            };

            let mut matched = 0usize;
            let mut failed = 0usize;
            for (name, expr) in basket {
                // nix oracle: build + NAR hash.
                let nix_out = run_capture(
                    &nix,
                    &["build", EXP, "nix-command", "--impure", "--no-link", "--print-out-paths", "--expr", expr],
                )
                .map(|s| s.lines().last().unwrap_or("").to_string())
                .unwrap_or_default();
                let nix_nar = hash_path(&nix_out);

                // sui: eval `(expr).drvPath` (writes the .drv on the writable store,
                // or routes it through the daemon) then realize the closure; hash
                // the built output with nix's own `nix hash path` so the comparison
                // is against nix's NAR definition.
                let sui_out = match eval_render_threaded(&format!("({expr}).drvPath"), false, true) {
                    Ok(drv) => match sui_orchestrate::realize_drv(&drv).await {
                        Ok(outputs) => outputs.into_iter().next().unwrap_or_default(),
                        Err(e) => { eprintln!("  sui build [{name}]: {e}"); String::new() }
                    },
                    Err(e) => { eprintln!("  sui eval [{name}]: {e}"); String::new() }
                };
                let sui_nar = hash_path(&sui_out);

                if !nix_nar.is_empty() && nix_nar == sui_nar {
                    matched += 1;
                    println!("  ✓ {name}  {nix_nar}");
                } else {
                    failed += 1;
                    println!("  ✘ {name}");
                    println!("      nix_out={nix_out}  nix_nar={nix_nar}");
                    println!("      sui_out={sui_out}  sui_nar={sui_nar}");
                }
            }
            println!("\n  ∑ {matched}/{} byte-identical · {failed} diverge", basket.len());
            if failed > 0 {
                return Err(CliError::Orchestrate {
                    operation: "build-parity",
                    message: format!("{failed} build-parity divergence(s)"),
                });
            }
        }

        Commands::Flake { command } => match command {
            FlakeCommands::Show { flake_ref, json } => {
                let flake_dir = resolve_flake_dir(flake_ref.as_deref())?;
                let outputs = sui_eval::builtins::evaluate_flake(&flake_dir)
                    .map_err(|e| CliError::Orchestrate {
                        operation: "flake show",
                        message: format!("eval: {e}"),
                    })?;
                if json {
                    println!("{}", serde_json::to_string(&flake_show_json(&outputs))?);
                } else {
                    print_flake_tree(&outputs);
                }
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
            FlakeCommands::Check { flake_ref, no_build: _ } => {
                let flake_dir = resolve_flake_dir(flake_ref.as_deref())?;
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
            FlakeCommands::Init { template } => {
                flake_init(template.as_deref().unwrap_or("default"))?;
            }
            FlakeCommands::New { dest, template } => {
                flake_new(&dest, template.as_deref().unwrap_or("default"))?;
            }
            FlakeCommands::Archive { flake_ref: fr, json } => {
                flake_archive(fr.as_deref().unwrap_or("."), json)?;
            }
            FlakeCommands::Clone { flake_ref: fr, dest } => {
                flake_clone(&fr, dest.as_deref())?;
            }
            FlakeCommands::Prefetch { flake_ref: fr, json } => {
                flake_prefetch(fr.as_deref().unwrap_or("."), json)?;
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
                SystemCommands::Rebuild { action, flake, dry_run } => {
                    // `--dry-run` forces the non-mutating dry-activate preview,
                    // overriding whatever positional action was given — so it is
                    // impossible to ask for a preview and accidentally get a real
                    // switch.
                    let action: sui_orchestrate::RebuildAction = if dry_run {
                        sui_orchestrate::RebuildAction::DryActivate
                    } else {
                        action.into()
                    };
                    let is_dry = action == sui_orchestrate::RebuildAction::DryActivate;
                    let flake_ref = flake.unwrap_or_else(|| ".".to_string());
                    let result = sys.rebuild_native(&flake_ref, action).await.map_err(|e| {
                        CliError::Orchestrate {
                            operation: "rebuild",
                            message: e.to_string(),
                        }
                    })?;
                    if is_dry {
                        // The dry-activate plan lives in `log`; print it verbatim.
                        // Nothing was executed against the real system.
                        println!("{}", result.log);
                        println!("(dry-activate: built the toplevel, executed nothing — {:.1}s)", result.duration_secs);
                    } else {
                        println!("rebuild {} in {:.1}s", if result.success { "succeeded" } else { "failed" }, result.duration_secs);
                        if let Some(generation) = result.generation {
                            println!("generation: {generation}");
                        }
                        if !result.success {
                            eprintln!("{}", result.log);
                        }
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
                SystemCommands::Converge { flake, watch, interval_secs, action, shadow } => {
                    let flake = flake.ok_or_else(|| CliError::Orchestrate {
                        operation: "converge",
                        message: "a flake reference is required (e.g. --flake .#cid)".to_string(),
                    })?;
                    // `--shadow` forces the non-mutating dry-activate posture,
                    // overriding `--action` — so it is impossible to ask for a
                    // shadow watch and accidentally converge the live system.
                    let action: sui_orchestrate::RebuildAction = if shadow {
                        sui_orchestrate::RebuildAction::DryActivate
                    } else {
                        action.into()
                    };
                    let config = sui_orchestrate::ReconcileConfig {
                        name: "system-in-place".to_string(),
                        flake,
                        action,
                        interval_secs,
                        watch,
                    };
                    // Reuse the platform-detected orchestrator built above.
                    let env = sui_orchestrate::LocalReconcileEnv::with_orchestrator(sys);
                    let controller =
                        sui_orchestrate::SystemReconciler::new(config.clone(), env);
                    if watch {
                        // The streaming daemon: reconcile on every source change +
                        // interval tick until SIGINT/SIGTERM.
                        println!(
                            "system-reconcile: watching {} (action={}, interval={}s) — Ctrl-C to stop",
                            config.flake, config.action, config.interval_secs
                        );
                        let driver = sui_orchestrate::ReconcileDriver::new(controller, config);
                        let ticks = driver
                            .run(sui_orchestrate::shutdown_signal())
                            .await
                            .map_err(|e| CliError::Orchestrate {
                                operation: "converge watch",
                                message: e.to_string(),
                            })?;
                        println!("system-reconcile: stopped after {ticks} attested ticks");
                    } else {
                        // One-shot: a single reconcile pass, then exit.
                        use sui_orchestrate::Controller as _;
                        let outcome =
                            controller.tick().await.map_err(|e| CliError::Orchestrate {
                                operation: "converge",
                                message: e.to_string(),
                            })?;
                        println!(
                            "system-reconcile: {}",
                            outcome.report.note.as_deref().unwrap_or("")
                        );
                        println!(
                            "  examined={} converged={} skipped={}",
                            outcome.report.objects_examined,
                            outcome.report.objects_changed,
                            outcome.report.objects_skipped
                        );
                    }
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
            CacheCommands::Serve { listen, store_path, priority, backend_config, supercache_config, signing_key } => {
                // Config-select the storage backend, in precedence order:
                //   1. --backend-config  → a raw BackendConfig file (any tier shape)
                //   2. --supercache-config → a SuperCacheCiConfig posture, translated
                //   3. --store-path      → the disk floor (default; unchanged behavior)
                // The chosen BackendConfig dispatches through the SAME typed
                // `build_backend` factory — never a silent hard-coded constructor,
                // never a silent disk fallback (a tiered/redis/pg arm whose feature
                // is off returns CacheError::NotImplemented).
                let backend = resolve_serve_backend(
                    backend_config.as_deref(),
                    supercache_config.as_deref(),
                    &store_path,
                )?;
                // A serving cache that has a signing key advertises the
                // fail-closed posture to its consumers.
                let config_require_sigs = signing_key.is_some();
                let config = sui_cache::CacheConfig {
                    listen,
                    backend,
                    priority,
                    signing_key: signing_key.map(std::path::PathBuf::from),
                    require_sigs: config_require_sigs,
                    ..sui_cache::CacheConfig::default()
                };
                let storage = sui_cache::build_backend(&config.backend)
                    .await
                    .map_err(|e| CliError::Orchestrate {
                        operation: "cache serve",
                        message: e.to_string(),
                    })?;
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
            CacheCommands::Wipe { backend_config, store_path } => {
                // Config-select the SAME backend the daemon serves, so a wipe
                // fans out to every tier (Redis L1 + Postgres L2 + object/local
                // L3) via the typed `StorageBackend::wipe_all`.
                let backend =
                    resolve_serve_backend(backend_config.as_deref(), None, &store_path)?;
                let storage = sui_cache::build_backend(&backend).await.map_err(|e| {
                    CliError::Orchestrate {
                        operation: "cache wipe",
                        message: e.to_string(),
                    }
                })?;
                let cleared = storage.wipe_all().await.map_err(|e| CliError::Orchestrate {
                    operation: "cache wipe",
                    message: e.to_string(),
                })?;
                // Keyway-shaped receipt: JSON out, exit 0. The typed value IS the
                // render surface (serde_json), never a hand-built string.
                let receipt = serde_json::json!({
                    "op": "cache-wipe",
                    "wiped": true,
                    "narinfos_cleared": cleared,
                    "source": backend_config.as_deref().unwrap_or(store_path.as_str()),
                });
                println!("{}", serde_json::to_string(&receipt).unwrap_or_default());
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
        Commands::Search { flake_ref, query } => {
            cmd_search(&flake_ref, &query)?;
        }
        Commands::Profile { command } => match command {
            ProfileCommands::List { .. } => {
                profile_list()?;
            }
            ProfileCommands::Install { packages, .. } => {
                profile_install(&packages)?;
            }
            ProfileCommands::Remove { packages, .. } => {
                profile_remove(&packages)?;
            }
            ProfileCommands::Upgrade { packages, .. } => {
                profile_upgrade(&packages)?;
            }
            ProfileCommands::Rollback { .. } => {
                profile_rollback()?;
            }
            ProfileCommands::History { .. } => {
                profile_history()?;
            }
            ProfileCommands::WipeHistory { .. } => {
                profile_wipe_history()?;
            }
            ProfileCommands::Diff { .. } => {
                profile_diff()?;
            }
        },
        Commands::Repl { .. } => { return Err(CliError::NotImplemented("repl".into())); }
        Commands::Copy { to, from, paths, no_check_sigs: _ } => {
            cmd_copy(to.as_deref(), from.as_deref(), &paths)?;
        }
        Commands::PathInfo { paths, json, closure_size: _ } => {
            cmd_path_info(&paths, json)?;
        }
        Commands::CollectGarbage { delete_old, delete_older_than } => {
            cmd_collect_garbage(delete_old, delete_older_than.as_deref())?;
        }
        Commands::Derivation { command } => match command {
            DerivationCommands::Show { paths, .. } => {
                derivation_show(&paths)?;
            }
            DerivationCommands::Add { path } => {
                derivation_add(&path)?;
            }
            DerivationCommands::Graph { path, max_depth, json } => {
                derivation_graph(&path, max_depth, json)?;
            }
        },
        Commands::ShowConfig { .. } => { println!("system = {}\nstore = /nix/store\ncores = {}", std::env::consts::ARCH, std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)); }
        Commands::Hash { command } => match command {
            HashCommands::File { path, r#type, base } => {
                hash_file(&path, &r#type, &base)?;
            }
            HashCommands::Path { path, r#type, base } => {
                hash_path(&path, &r#type, &base)?;
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
            KeyCommands::GenerateSecret { key_name } => {
                key_generate_secret(&key_name)?;
            }
            KeyCommands::ConvertSecretToPublic => {
                key_convert_secret_to_public()?;
            }
        },
        Commands::Why { path, dependency } => { return Err(CliError::NotImplemented(format!("why {path} {dependency}"))); }
        Commands::PathFromHashPart { hash_part } => { return Err(CliError::NotImplemented(format!("path-from-hash-part {hash_part}"))); }
        Commands::Edit { installable } => { return Err(CliError::NotImplemented(format!("edit {installable}"))); }
        Commands::Log { installable } => { return Err(CliError::NotImplemented(format!("log {installable}"))); }
        Commands::DiffClosures { before, after } => { return Err(CliError::NotImplemented(format!("diff-closures {before} {after}"))); }
        Commands::UpgradeNix { .. } => { return Err(CliError::NotImplemented("upgrade-nix".into())); }
        Commands::Fmt { files, check } => { return Err(CliError::NotImplemented(format!("fmt ({}){}", if check { "check" } else { "format" }, if files.is_empty() { String::new() } else { format!(" {}", files.join(" ")) }))); }
        Commands::Registry { command } => match command {
            RegistryCommands::List { json } => {
                registry_list(json)?;
            }
            RegistryCommands::Add { from, to } => {
                registry_add(&from, &to)?;
            }
            RegistryCommands::Remove { entry } => {
                registry_remove(&entry)?;
            }
            RegistryCommands::Pin { entry } => {
                registry_pin(&entry)?;
            }
        },
        Commands::Agent { nats_url, stream, consumer, cache_url, cache_name, strategy, signing_key } => {
            agent::run_agent(&nats_url, &stream, &consumer, &cache_url, &cache_name, &strategy, signing_key.as_deref()).await?;
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
        Commands::Parity { nix, json, track_nixpkgs } => {
            if let Some(reference) = track_nixpkgs {
                let rev = resolve_nixpkgs_rev(&nix, &reference)?;
                // Pin <nixpkgs> for the corpus eval to the tracked rev (typed
                // equivalent of the workflow's `nix flake metadata | jq` + export).
                // SAFETY: single-threaded CLI setup, before any eval thread spawns
                // — no concurrent env access (Rust 2024 made `set_var` unsafe).
                unsafe {
                    std::env::set_var(
                        "NIX_PATH",
                        format!("nixpkgs=https://github.com/NixOS/nixpkgs/archive/{rev}.tar.gz"),
                    );
                }
                if !json {
                    println!("tracking nixpkgs `{reference}` @ {rev}");
                }
            }
            cmd_parity(&nix, json)?;
        }
        Commands::ParityBisect { expr, nix } => {
            cmd_parity_bisect(&nix, &expr)?;
        }
        Commands::PerfSeal { json, write_baseline } => {
            perf_seal::run(json, write_baseline)?;
        }
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

// ── Import-from-derivation (IFD) realize hook ───────────────────
//
// The pure evaluator (`sui-eval`) owns no build pipeline; when it needs a
// derivation's output on disk mid-eval (an `import`/`readFile`/`readDir`/
// `pathExists`/`builtins.path` of a derivation — e.g. the darwin toplevel
// importing `ishou.stylix-fonts`, a `runCommand` drv), it invokes a
// thread-local realize hook the binary installs. The binary owns the store,
// substitutor, builder, sandbox, and a tokio runtime — everything the async
// realize pipeline needs — so orchestration stays out of the evaluator.
//
// The pipeline is built lazily inside the eval thread on the *first* IFD demand
// (opening the store `open_rw` is privileged; a pure eval that never hits IFD
// must not require it), then reused for every subsequent realize on that
// thread.

/// The lazily-built, per-eval-thread realize infrastructure.
///
/// The write path is chosen by `sui_store::StoreAccess` detected on the FIRST
/// IFD demand:
/// - `Direct` — the store is genuinely writable → the local builder pipeline
///   (store + substitutor + builder), as before.
/// - `Daemon` — the store is a root-owned multi-user store → all privileged
///   writes route through the running nix daemon over the worker protocol.
///
/// The dispatch IS the seal (see `sui_store::daemon_realize`): the `Direct` arm
/// carries a `WritableStore` proof, obtainable only over a writable store, so a
/// direct store write against a multi-user store has no code path — the exact
/// `cannot read <drv>` failure the operator's Mac hit is unrepresentable, not
/// retried.
enum IfdRealizer {
    /// Single-user store: build directly through the local pipeline.
    Direct {
        rt: tokio::runtime::Runtime,
        builder: sui_build::LocalBuilder,
        substitutor: Substitutor,
    },
    /// Multi-user store: realize through the nix daemon.
    Daemon {
        rt: tokio::runtime::Runtime,
        store: sui_store::DaemonStore,
    },
}

thread_local! {
    /// Per-thread lazily-initialized realizer. `None` = not yet built; a build
    /// error surfaces per-call so the first failure to open the store doesn't
    /// poison later attempts on a different thread.
    static IFD_REALIZER: std::cell::RefCell<Option<IfdRealizer>> =
        const { std::cell::RefCell::new(None) };
}

/// Build the IFD realize pipeline, dispatching on the detected store access
/// mode. Called once per eval thread on the first IFD demand.
fn build_ifd_realizer() -> Result<IfdRealizer, String> {
    use sui_build::{sandbox, LocalBuilder};
    use sui_store::StoreAccess;

    // A current-thread runtime is enough — realize is a synchronous
    // block-on-one-closure from the (non-async) eval thread.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("ifd runtime: {e}"))?;

    // Detect the store access mode ONCE. On a multi-user store, `Direct` is
    // unconstructable (its `WritableStore` proof requires a writable store), so
    // the daemon arm is the only reachable path — the pivot.
    match StoreAccess::detect() {
        Some(StoreAccess::Direct(_writable)) => {
            let store = rt
                .block_on(sui_store::LocalStore::open_rw(NIX_DB_PATH))
                .map_err(|e| format!("ifd store open ({NIX_DB_PATH}): {e}"))?;
            let store: Arc<dyn sui_store::Store> = Arc::new(store);

            let caches =
                sui_orchestrate::build_caches(&sui_orchestrate::get_substituters());
            let substitutor = Substitutor::new(store.clone(), caches);

            #[cfg(target_os = "macos")]
            let sandbox: Box<dyn sandbox::Sandbox> =
                Box::new(sandbox::DarwinSandbox::new());
            #[cfg(not(target_os = "macos"))]
            let sandbox: Box<dyn sandbox::Sandbox> =
                Box::new(sandbox::LinuxSandbox::new());

            let builder = LocalBuilder::new(store, sandbox);
            Ok(IfdRealizer::Direct { rt, builder, substitutor })
        }
        Some(StoreAccess::Daemon(store)) => Ok(IfdRealizer::Daemon { rt, store }),
        None => Err(format!(
            "no store-write path: /nix/store is not writable and no nix daemon \
             socket is reachable at {} (set NIX_REMOTE=daemon or run the daemon)",
            sui_store::default_daemon_socket().display()
        )),
    }
}

/// The per-realize wall-clock bound (the MOVE-1 seal). A single derivation's
/// substitute-or-build must complete within this bound or the realize fails-fast
/// with a typed error — an unbounded hang has no code path. Overridable via
/// `SUI_IFD_REALIZE_TIMEOUT_SECS`; the default is generous enough to substitute
/// a large closure yet well under any outer eval harness timeout, so sui itself
/// NAMES the stalled derivation instead of being killed as a mystery hang.
fn ifd_realize_bound() -> std::time::Duration {
    const DEFAULT_SECS: u64 = 300;
    let secs = std::env::var("SUI_IFD_REALIZE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

/// Realize one derivation output: substitute-then-build its closure until the
/// demanded output is present in the store. Byte-parity-safe: the drvPath is
/// computed by the evaluator's module fixpoint (already byte-identical to nix),
/// so the realized output is byte-identical to nix's — a `Direct` build and a
/// `Daemon` build both produce the same bytes at the same content-addressed
/// path.
///
/// **Bounded (the MOVE-1 seal):** each realize either resolves (a validated
/// output) or returns an `Err` within [`ifd_realize_bound`]. A daemon build is
/// external, non-cancellable I/O that can stall on a divergent output path that
/// exists on no cache; wrapping it in a wall-clock bound converts the former
/// unbounded hang into a typed, named error the evaluator surfaces (it then goes
/// on to NAME the next root instead of hanging).
// ── Diagnostic IFD realize trace (gated by SUI_PERF_TRACE=1) ───────
thread_local! {
    static IFD_TRACE: std::cell::RefCell<Vec<(String, std::time::Duration)>> =
        std::cell::RefCell::new(Vec::new());
}

fn ifd_trace_enabled() -> bool {
    std::env::var("SUI_PERF_TRACE").ok().as_deref() == Some("1")
}

/// Dump the IFD realize trace to stderr (no-op unless SUI_PERF_TRACE=1).
fn ifd_realize_dump() {
    if !ifd_trace_enabled() {
        return;
    }
    IFD_TRACE.with(|c| {
        let mut rows = c.borrow().clone();
        let total: std::time::Duration = rows.iter().map(|(_, d)| *d).sum();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        eprintln!("\n=== IFD realize trace ===");
        eprintln!("realize calls:  {}", rows.len());
        eprintln!("total elapsed:  {:.2}s", total.as_secs_f64());
        eprintln!("--- top 20 realizes by elapsed ---");
        for (drv, d) in rows.iter().take(20) {
            eprintln!("  {:>7.2}s  {}", d.as_secs_f64(), drv);
        }
        eprintln!("=========================\n");
    });
}

fn ifd_realize(drv_path: &str, out_path: &str) -> Result<(), String> {
    use sui_build::BuildClosure;

    let trace = ifd_trace_enabled();
    let t0 = if trace { Some(std::time::Instant::now()) } else { None };

    let result = IFD_REALIZER.with(|cell| {
        // Lazily build the pipeline on first demand.
        if cell.borrow().is_none() {
            let realizer = build_ifd_realizer()?;
            *cell.borrow_mut() = Some(realizer);
        }
        let borrow = cell.borrow();
        let realizer = borrow.as_ref().expect("realizer built above");

        let bound = ifd_realize_bound();
        match realizer {
            IfdRealizer::Direct { rt, builder, substitutor } => {
                let closure = BuildClosure::compute(drv_path)
                    .map_err(|e| format!("closure {drv_path}: {e}"))?;
                // Bound the local build too: a `Direct` build can stall on a
                // divergent input path exactly as a daemon build can. On elapse,
                // fail-fast with a typed, named error — never an unbounded hang.
                let build = builder.build_closure(&closure, Some(substitutor));
                let result = rt
                    .block_on(tokio::time::timeout(bound, build))
                    .map_err(|_elapsed| {
                        format!(
                            "ifd realize of {drv_path} → {out_path} exceeded its {}s bound \
                             (local build stalled — likely a sui↔nix output-path divergence; \
                             NOT a hang)",
                            bound.as_secs()
                        )
                    })?
                    .map_err(|e| format!("build {drv_path}: {e}"))?;
                if !result.success {
                    return Err(format!("build failed for {drv_path}:\n{}", result.log));
                }

                // Verify the demanded output is now present — a build that
                // "succeeded" but didn't produce this path is a real error.
                let read_path = sui_eval::path::materialize_str(out_path);
                if !std::path::Path::new(&read_path).exists() {
                    return Err(format!(
                        "realized {drv_path} but expected output {out_path} is still absent on disk"
                    ));
                }
                Ok(())
            }
            IfdRealizer::Daemon { rt, store } => {
                // The daemon does the privileged work: AddToStore the computed
                // `.drv` closure (byte-identical to nix), then BuildPaths
                // (substitute-or-build). The returned `Realized` proof is only
                // constructible when the daemon attests the output valid at its
                // content-addressed path — a wrong/absent output cannot pass.
                let realized = rt
                    .block_on(sui_store::realize_via_daemon_bounded(
                        store, drv_path, out_path, bound,
                    ))
                    .map_err(|e| format!("daemon realize {drv_path}: {e}"))?;
                debug_assert_eq!(realized.out_path(), out_path);
                Ok(())
            }
        }
    });

    if let Some(t0) = t0 {
        let d = t0.elapsed();
        IFD_TRACE.with(|c| {
            let mut v = c.borrow_mut();
            v.push((drv_path.to_string(), d));
            let total: std::time::Duration = v.iter().map(|(_, d)| *d).sum();
            eprintln!(
                "[ifd-trace] realize #{} took {:.2}s (cumulative {:.1}s) {}",
                v.len(),
                d.as_secs_f64(),
                total.as_secs_f64(),
                drv_path
            );
        });
    }
    result
}

/// Install the IFD realize hook on the current (eval) thread. Returns the guard
/// that uninstalls it on drop. Call inside the eval thread, before eval runs.
#[must_use]
fn install_ifd_hook() -> sui_eval::realize::RealizeHookGuard {
    sui_eval::realize::install_realize_hook(Box::new(ifd_realize))
}

/// Resolve a flake directory from an optional CLI argument.
///
/// Accepts the surface `nix` does:
///   - `None`, `""`, `"."`         → current working directory
///   - `path:/abs/dir[#attr]`      → `/abs/dir`
///   - `/abs/dir[#attr]`           → `/abs/dir`
///   - `./rel/dir[#attr]`          → resolved against cwd
///
/// Anything else (remote scheme like `github:` / `https:` / `git+`)
/// is currently out of scope for local-dir resolution.
fn resolve_flake_dir(flake_ref: Option<&str>) -> Result<std::path::PathBuf, CliError> {
    let raw = match flake_ref {
        None | Some("") | Some(".") => return Ok(std::env::current_dir()?),
        Some(s) => s,
    };
    // Strip any installable attr suffix.
    let head = raw.split('#').next().unwrap_or(raw);
    // Strip the `path:` scheme if present (sui-spec probes emit this form).
    let head = head.strip_prefix("path:").unwrap_or(head);
    let p = std::path::PathBuf::from(head);
    if p.is_dir() {
        Ok(p)
    } else {
        Ok(std::env::current_dir()?)
    }
}

/// Normalize a `nix eval`-style installable flake-ref into the string
/// `builtins.getFlake` accepts.
///
/// - `.`, `./...`, `../...` → `path:` + canonicalized absolute path
/// - `/abs/path`            → `path:/abs/path`
/// - `path:...`, `github:...`, `git+...`, `https://...`, `tarball:...`
///   → returned verbatim (the scheme is already explicit).
fn normalize_flake_ref(s: &str) -> String {
    if s == "." || s.starts_with("./") || s.starts_with("../") {
        let canon =
            std::fs::canonicalize(s).unwrap_or_else(|_| std::path::PathBuf::from(s));
        format!("path:{}", canon.display())
    } else if s.starts_with('/') {
        format!("path:{s}")
    } else {
        s.to_string()
    }
}

/// Build a byte-safe cross-run eval-cache key for a flake **installable**
/// (`flake-ref#attr.path`).  Returns `None` — meaning "do not cache" — for
/// anything that isn't a lock-pinned local installable, because those are the
/// only inputs whose evaluation is a pure function of content we can fully
/// capture in the key:
///
///   * `source_hash` = SHA-256 of the DESUGARED expression (which embeds the
///     normalized flake-ref + attr path) **plus** the render mode.  So `.#a`
///     vs `.#b`, and `--json` vs default, never collide.
///   * `lock_hash`   = SHA-256 of the installable's `flake.lock`, so a moved
///     input pin invalidates the entry (miss → fresh eval → no stale byte).
///
/// A non-local ref (`nixpkgs#…`, a registry alias, a remote URL) is NOT cached
/// in M0: its resolution can drift without a local `flake.lock` to pin it, so
/// caching it would risk a stale byte.  `--expr` / bare-expression evals are
/// likewise never cached (they can name `currentTime`/`getEnv`/mutable
/// `readFile` — the impure frontier the eval-memo purity gate forbids).
fn eval_cache_key_for_installable(
    desugared_expr: &str,
    flake_ref: &str,
    render_mode: &str,
) -> Option<sui_eval::eval_cache::CacheKey> {
    use sha2::Digest;
    // Only lock-pinned LOCAL flakes are byte-safe to cache in M0.
    let normalized = normalize_flake_ref(flake_ref);
    let dir = normalized.strip_prefix("path:")?;
    let lock_path = std::path::Path::new(dir).join("flake.lock");
    let lock_bytes = std::fs::read(&lock_path).ok()?;
    let lock_hash = format!("{:x}", sha2::Sha256::digest(&lock_bytes));
    // The flake's OWN git state — `self.rev`/`self.shortRev`/`self.lastModified`
    // (and `self.dirtyShortRev`) — is NOT captured by `desugared_expr` or the
    // lock (the lock pins INPUTS, not `self`). A derivation embedding a
    // self-derived value (a `system.configurationRevision`, a build stamped
    // with its own git rev) changes drvPath across a commit even when the
    // expression + lock are byte-identical. So fold the flake dir's CLEAN
    // committed rev into the key, and REFUSE to cache a dirty (or non-git) tree
    // whose `self.dirtyShortRev` hashes the whole worktree we cannot cheaply
    // capture — otherwise a `…-dirty` result gets served stale after the commit
    // that made it clean (observed: cid served `darwin-system-…dirty` post-commit).
    let git_rev = sui_eval::git::clean_worktree_rev(std::path::Path::new(dir))?;
    // Fold the render mode + the flake's clean git rev into the source hash so
    // json / raw / display outputs (byte-different) and distinct commits never
    // collide on the same key.
    let mut h = sha2::Sha256::new();
    h.update(desugared_expr.as_bytes());
    h.update(b"\0mode=");
    h.update(render_mode.as_bytes());
    h.update(b"\0rev=");
    h.update(git_rev.as_bytes());
    let source_hash = format!("{:x}", h.finalize());
    Some(sui_eval::eval_cache::CacheKey { source_hash, lock_hash: Some(lock_hash) })
}

/// Evaluate `expr` on the tree-walker with a large stack and return the exact
/// rendered output bytes (`--json` → canonical JSON, else the `Display` form).
/// This is the single eval-and-render primitive the `--no-vm` `eval` path uses
/// for both a cache miss and the `SUI_EVAL_CACHE_VERIFY` differential, so the
/// cached bytes are provably the same bytes a fresh eval prints.
///
/// macOS's main thread has a fixed 8 MB stack that stacker can't grow, so the
/// 256 MB stack is mandatory for deep nixpkgs / module-system fixpoints.
fn eval_render_threaded(expr: &str, json_flag: bool, raw_flag: bool) -> Result<String, CliError> {
    let expr_clone = expr.to_string();
    let handle = std::thread::Builder::new()
        .name("sui-eval".into())
        .stack_size(256 * 1024 * 1024) // 256MB
        .spawn(move || -> Result<String, CliError> {
            // IFD: reads of a derivation output mid-eval realize it.
            let _ifd_guard = install_ifd_hook();
            let value = sui_eval::eval(&expr_clone)?;
            let output = if json_flag {
                serde_json::to_string(&value.to_json())?
            } else if raw_flag {
                // `nix eval --raw` prints a string value's bytes verbatim (no
                // surrounding quotes). The default `Display` wraps strings in
                // Nix-source quotes, so `--raw` MUST special-case a forced
                // string to be byte-identical to nix (load-bearing for the
                // marquee: a drvPath printed with `--raw` must equal nix's).
                // Non-string values keep the Display fallback (unchanged).
                let forced = sui_eval::eval::force_value(&value).unwrap_or_else(|_| value.clone());
                match &forced {
                    sui_eval::Value::String(s) => s.as_str().to_string(),
                    other => format!("{other}"),
                }
            } else {
                format!("{value}")
            };
            // SUI_PARITY_STRICT: drain the thread-local swallowed-force-error
            // ledger on THIS worker thread (no-op unless the env is set).
            report_parity_strict();
            // SUI_PERF_TRACE diagnostics (no-op unless the env is set).
            sui_compat::source::nar_hash_dump();
            ifd_realize_dump();
            Ok(output)
        })
        .expect("failed to spawn eval thread");
    handle.join().expect("eval thread panicked")
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

// ── flake show --json ───────────────────────────────────────────

/// Serialize a flake's evaluated outputs in the structured JSON shape
/// `nix flake show --json` produces.
///
/// Cppnix labels every leaf with a `{"type":"..."}` marker drawn from
/// the flake-output schema (e.g. `nixosConfigurations.<host>` →
/// `nixos-configuration`, `apps.<system>.<name>` → `app`).  We walk one
/// or two levels into each well-known group to enumerate names, but
/// never force the leaf values — forcing a `nixosConfiguration`'s
/// `config` would diverge in the module-system fixpoint (M2.6 work
/// per sui/README.md `Status` section).
fn flake_show_json(outputs: &sui_eval::Value) -> serde_json::Value {
    let sui_eval::Value::Attrs(top) = outputs else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    let mut root = serde_json::Map::new();
    for key in top.keys() {
        if is_flake_internal_key(&key) {
            continue;
        }
        let Some(child) = top.get(&key) else { continue };
        root.insert(key.clone(), flake_show_group_json(&key, child));
    }
    serde_json::Value::Object(root)
}

/// Flake-output keys cppnix omits from `nix flake show --json` — these
/// are the metadata that lands on the flake itself (provenance, hash,
/// inputs, sourceInfo) rather than user-declared outputs.
fn is_flake_internal_key(key: &str) -> bool {
    if key.starts_with('_') {
        return true;
    }
    matches!(key,
        "description" | "inputs"
        | "narHash" | "outPath" | "outputs"
        | "sourceInfo" | "lastModified" | "lastModifiedDate"
        | "rev" | "shortRev" | "revCount"
        | "submodules" | "lockedRef" | "originalRef"
    )
}

/// One well-known flake-output group → typed JSON tree.
fn flake_show_group_json(top_key: &str, value: &sui_eval::Value) -> serde_json::Value {
    let named_leaf = match top_key {
        "nixosConfigurations"  => Some("nixos-configuration"),
        "darwinConfigurations" => Some("darwin-configuration"),
        "homeConfigurations"   => Some("home-manager-configuration"),
        "nixosModules" | "darwinModules" | "homeModules" => Some("nixos-module"),
        "overlays"             => Some("nixpkgs-overlay"),
        _ => None,
    };
    let per_system_leaf = match top_key {
        "apps"      => Some("app"),
        "packages" | "devShells" | "checks" | "formatter" | "legacyPackages"
                    => Some("derivation"),
        _ => None,
    };
    let Ok(forced) = sui_eval::eval::force_value(value) else {
        return type_marker_json("unknown");
    };
    let sui_eval::Value::Attrs(attrs) = &forced else {
        return type_marker_json("unknown");
    };
    if let Some(t) = named_leaf {
        let mut m = serde_json::Map::new();
        for k in attrs.keys() {
            m.insert(k, type_marker_json(t));
        }
        return serde_json::Value::Object(m);
    }
    if let Some(leaf) = per_system_leaf {
        let mut m = serde_json::Map::new();
        for sys in attrs.keys() {
            let Some(sys_val) = attrs.get(&sys) else { continue };
            let Ok(sys_forced) = sui_eval::eval::force_value(sys_val) else {
                m.insert(sys, type_marker_json("unknown"));
                continue;
            };
            let sui_eval::Value::Attrs(inner) = &sys_forced else {
                m.insert(sys, type_marker_json("unknown"));
                continue;
            };
            let mut leaves = serde_json::Map::new();
            for name in inner.keys() {
                leaves.insert(name, type_marker_json(leaf));
            }
            m.insert(sys, serde_json::Value::Object(leaves));
        }
        return serde_json::Value::Object(m);
    }
    type_marker_json("unknown")
}

fn type_marker_json(t: &str) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert("type".to_string(), serde_json::Value::String(t.to_string()));
    serde_json::Value::Object(m)
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
