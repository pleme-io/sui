//! Daemon-mediated store writes — the multi-user-store realize pivot.
//!
//! # The problem this seals
//!
//! On a **single-user** Nix install the store (`/nix/store` + `db.sqlite`) is
//! writable by the invoking user, so sui realizes a derivation output by writing
//! directly through [`crate::LocalStore`] + a `LocalBuilder`. On a **multi-user
//! (daemon)** install — the default on macOS — the store and its SQLite database
//! are **root-owned and read-only** to the `uid 501` user sui runs as. A direct
//! store write there fails: the operator's darwin eval computes the right
//! `stylix-fonts` `.drv` (`9px3wz2j…`) then dies at `cannot read <drv>` because
//! the `LocalBuilder` cannot materialize the closure into a root-owned store.
//!
//! The fix is structural, not a retry: on a multi-user store, **route every
//! privileged store write through the running nix daemon** (worker protocol over
//! `/nix/var/nix/daemon-socket/socket`). The daemon runs as root, so it can
//! `AddToStore` the computed `.drv`s and `BuildPaths` (substitute-or-build) the
//! output. sui asks; the daemon does the privileged work; sui reads the result.
//!
//! # The two seals (see [`StoreAccess`] and [`Realized`])
//!
//! 1. **[`StoreAccess`] dispatch** — store mode is detected at *construction*.
//!    The [`StoreAccess::Direct`] arm is only reachable when the store is
//!    genuinely writable; a **direct write against a multi-user store has no code
//!    path** (the `Direct` constructor probes writability and returns `Err`
//!    otherwise, so no expressible program can build `Direct(_)` over a
//!    read-only store). Tier: *truly-unrepresentable* on the dispatch axis — the
//!    `cannot read <drv>` failure class cannot be constructed.
//!
//! 2. **[`Realized`] proof** — the realize returns a value whose sole
//!    constructor requires the realized output to be *valid at its
//!    content-addressed store path* as attested by the daemon. Wrong/unverified
//!    bytes cannot silently flow downstream. Tier: *parse-time-rejected* on the
//!    sui side, resting on the daemon's own content-addressing guarantee (a **C2
//!    external-observation ceiling** — sui observes the daemon's attestation
//!    rather than re-hashing root-owned bytes it cannot read).

use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use sui_compat::wire::{PROTOCOL_VERSION, StderrMsg, WORKER_MAGIC_1, WORKER_MAGIC_2, WorkerOp};

/// The default nix daemon socket (mirrors `sui_daemon::DEFAULT_SOCKET_PATH`,
/// duplicated here to avoid a `sui-store → sui-daemon` dependency edge).
pub const DEFAULT_DAEMON_SOCKET: &str = "/nix/var/nix/daemon-socket/socket";

/// The fallback directory `sui-eval`'s `write_derivation_to_store` uses when the
/// real `/nix/store` is not writable (`PermissionDenied`). On a multi-user store
/// every computed `.drv` lands here, keyed by its store basename.
const DRV_FALLBACK_DIR: &str = "sui-drv-cache";

// Worker-protocol stderr frame tags — the canonical values live in
// `sui_compat::wire::StderrMsg`; aliased here as plain `u64`s for match arms.
const STDERR_LAST: u64 = StderrMsg::Last as u64;
const STDERR_ERROR: u64 = StderrMsg::Error as u64;
const STDERR_WRITE: u64 = StderrMsg::Write as u64;

/// Errors from a daemon-mediated realize.
#[derive(Debug, thiserror::Error)]
pub enum DaemonRealizeError {
    /// No daemon socket was reachable (no multi-user daemon running).
    #[error("nix daemon socket unreachable at {0}")]
    Unreachable(PathBuf),
    /// A worker-protocol / I/O error talking to the daemon.
    #[error("daemon protocol error: {0}")]
    Protocol(String),
    /// The daemon reported a build/substitute failure.
    #[error("daemon build failed: {0}")]
    Build(String),
    /// The demanded output is absent even after the daemon reported success.
    #[error("realize of {drv} completed but expected output {out} is not valid in the store")]
    OutputAbsent { drv: String, out: String },
    /// A `.drv` in the closure could not be located on disk to hand to the daemon.
    #[error("derivation {0} not present on disk (neither /nix/store nor the sui-drv-cache fallback)")]
    DrvMissing(String),
    /// The realize did not complete within its wall-clock bound. A daemon
    /// `BuildPaths` is external, non-cancellable I/O (it may substitute or build
    /// an arbitrarily large closure); if it exceeds the bound — because the
    /// output store path never becomes valid (a sui↔nix path divergence: the
    /// path exists on no cache and cannot be built), or a substituter stalls —
    /// the realize is aborted with this typed error rather than blocking the
    /// evaluator forever. See [`realize_via_daemon_bounded`].
    #[error(
        "daemon realize of {drv} → {out} exceeded its {bound_secs}s bound \
         (output never became valid — likely a sui↔nix output-path divergence \
         or a stalled substituter; NOT a hang)"
    )]
    Timeout { drv: String, out: String, bound_secs: u64 },
}

// ─────────────────────────────────────────────────────────────────────────────
// Seal 1: StoreAccess dispatch — a direct write on a multi-user store is
// unrepresentable.
// ─────────────────────────────────────────────────────────────────────────────

/// A writable single-user store handle. Its **only** constructor
/// ([`WritableStore::probe`]) succeeds only when `/nix/store` is genuinely
/// writable by the current process — so no value of this type can exist over a
/// read-only, daemon-owned store.
#[derive(Debug, Clone)]
pub struct WritableStore {
    store_dir: PathBuf,
}

impl WritableStore {
    /// Probe whether `store_dir` is writable by the current process. Returns
    /// `Some(WritableStore)` only when a write is actually permitted; `None`
    /// otherwise. This is the *only* way to obtain a `WritableStore`, so a
    /// `Direct` dispatch over a read-only store cannot be constructed.
    #[must_use]
    pub fn probe(store_dir: &Path) -> Option<Self> {
        // A directory is writable if we can create-and-remove a temp entry in
        // it. `access(W_OK)` lies under some sandboxes; an actual create is the
        // honest probe.
        let probe = store_dir.join(format!(".sui-write-probe-{}", std::process::id()));
        match std::fs::File::create(&probe) {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                Some(Self { store_dir: store_dir.to_path_buf() })
            }
            Err(_) => None,
        }
    }

    /// The store directory this handle is proven writable over.
    #[must_use]
    pub fn store_dir(&self) -> &Path {
        &self.store_dir
    }
}

/// A daemon-mediated store handle. Present whenever a worker-protocol daemon
/// socket is reachable; privileged writes route through it.
#[derive(Debug, Clone)]
pub struct DaemonStore {
    socket: PathBuf,
}

impl DaemonStore {
    /// Construct a daemon handle for a reachable socket. Returns `None` if no
    /// socket exists at `socket_path`.
    #[must_use]
    pub fn at(socket_path: PathBuf) -> Option<Self> {
        if socket_path.exists() {
            Some(Self { socket: socket_path })
        } else {
            None
        }
    }

    /// The daemon socket path.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

/// Typed store-access mode, chosen by [`StoreAccess::detect`] at construction.
///
/// The invariant is on the *constructors*, not a runtime branch: `Direct` holds
/// a [`WritableStore`] (only obtainable over a writable store) and `Daemon`
/// holds a [`DaemonStore`] (only obtainable when a socket is reachable). There
/// is no `Direct(WritableStore)` value over a read-only store, so the
/// direct-write-on-multi-user-store failure class is *unrepresentable*.
#[derive(Debug, Clone)]
pub enum StoreAccess {
    /// The store is writable; realize/add-path go through the local builder.
    Direct(WritableStore),
    /// The store is daemon-owned; realize/add-path go through the worker socket.
    Daemon(DaemonStore),
}

impl StoreAccess {
    /// Detect the store access mode for the standard `/nix/store` + default
    /// daemon socket, honoring `NIX_REMOTE`.
    ///
    /// Precedence matches CppNix's own resolution:
    /// 1. `NIX_REMOTE=unix://<path>` / `daemon` / empty → force the daemon path.
    /// 2. Otherwise: if `/nix/store` is writable → `Direct`; else, if a daemon
    ///    socket is reachable → `Daemon`.
    /// 3. If neither a writable store nor a reachable daemon exists, returns
    ///    `None` (no store-write path exists — the caller surfaces this rather
    ///    than silently degrading).
    #[must_use]
    pub fn detect() -> Option<Self> {
        Self::detect_with(Path::new("/nix/store"), &default_daemon_socket())
    }

    /// Detect against an explicit store dir + daemon socket (test seam).
    #[must_use]
    pub fn detect_with(store_dir: &Path, daemon_socket: &Path) -> Option<Self> {
        // NIX_REMOTE forcing the daemon path takes precedence: an operator (or
        // `NIX_REMOTE=daemon`) explicitly asking for the daemon must never be
        // silently routed to a direct write.
        match std::env::var("NIX_REMOTE") {
            Ok(v) if v.starts_with("unix://") => {
                let sock = PathBuf::from(v.trim_start_matches("unix://"));
                return DaemonStore::at(sock).map(StoreAccess::Daemon);
            }
            Ok(v) if v == "daemon" => {
                return DaemonStore::at(daemon_socket.to_path_buf()).map(StoreAccess::Daemon);
            }
            Ok(v) if !v.is_empty() => {
                // NIX_REMOTE names a non-unix remote (e.g. https://…) — not a
                // store-write path we mediate here.
                return None;
            }
            _ => {}
        }

        // No forcing: prefer a genuinely-writable store, else the daemon.
        if let Some(w) = WritableStore::probe(store_dir) {
            return Some(StoreAccess::Direct(w));
        }
        DaemonStore::at(daemon_socket.to_path_buf()).map(StoreAccess::Daemon)
    }

    /// Whether this access mode routes writes through the daemon.
    #[must_use]
    pub fn is_daemon(&self) -> bool {
        matches!(self, StoreAccess::Daemon(_))
    }
}

/// Resolve the daemon socket path from `NIX_REMOTE`, defaulting to the standard
/// socket.
#[must_use]
pub fn default_daemon_socket() -> PathBuf {
    match std::env::var("NIX_REMOTE") {
        Ok(v) if v.starts_with("unix://") => PathBuf::from(v.trim_start_matches("unix://")),
        _ => PathBuf::from(DEFAULT_DAEMON_SOCKET),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Seal 2: Realized — a proof the demanded output is valid at its
// content-addressed store path.
// ─────────────────────────────────────────────────────────────────────────────

/// Proof that a derivation output was realized and is **valid in the store at
/// its content-addressed path**.
///
/// The sole constructor is [`Realized::attested`], which requires the daemon to
/// have confirmed the output path is valid (`IsValidPath == true`). Because the
/// output store path is content-addressed by construction (input-addressed:
/// the path *is* the hash of the drv-modulo-inputs; fixed-output: the path is
/// the fixed content hash), a valid path at that exact address IS the proof the
/// bytes are the expected bytes. There is no way to obtain a `Realized` for an
/// absent or unverified output.
#[derive(Debug, Clone)]
pub struct Realized {
    out_path: String,
    /// The daemon-reported NAR hash of the realized output, when the daemon
    /// surfaced one via `QueryPathInfo`. `None` means validity was attested by
    /// `IsValidPath` alone (still a content-address proof — the path is the
    /// address). Recorded for downstream audit, never used to *weaken* the
    /// proof.
    nar_hash: Option<String>,
}

impl Realized {
    /// Construct the proof from a daemon validity attestation.
    ///
    /// `valid` MUST come from the daemon's own `IsValidPath`/`QueryPathInfo`
    /// answer for `out_path`. If the output is not valid, returns `Err` — a
    /// `Realized` for an unverified output is unconstructable.
    ///
    /// # Errors
    /// Returns [`DaemonRealizeError::OutputAbsent`] when the daemon reports the
    /// output is not valid in the store.
    pub fn attested(
        drv_path: &str,
        out_path: &str,
        valid: bool,
        nar_hash: Option<String>,
    ) -> Result<Self, DaemonRealizeError> {
        if !valid {
            return Err(DaemonRealizeError::OutputAbsent {
                drv: drv_path.to_string(),
                out: out_path.to_string(),
            });
        }
        Ok(Self { out_path: out_path.to_string(), nar_hash })
    }

    /// The realized (and proven-valid) output store path.
    #[must_use]
    pub fn out_path(&self) -> &str {
        &self.out_path
    }

    /// The daemon-attested NAR hash, when available.
    #[must_use]
    pub fn nar_hash(&self) -> Option<&str> {
        self.nar_hash.as_deref()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The daemon realize driver — worker-protocol client.
// ─────────────────────────────────────────────────────────────────────────────

/// Realize `drv_path`'s output `out_path` through the daemon at `store.socket()`.
///
/// Sequence (mirroring CppNix `remote-store.cc`):
/// 1. handshake + `SetOptions`.
/// 2. If `out_path` is already valid, return the proof immediately.
/// 3. `AddToStore` every `.drv` in the closure (found on disk in `/nix/store`
///    or the `sui-drv-cache` fallback) so the daemon can build them.
/// 4. `BuildPaths [<drv_path>]` — the daemon substitutes-or-builds.
/// 5. `IsValidPath out_path` → mint the [`Realized`] proof.
///
/// # Errors
/// Any protocol/build failure, or an absent output after a reported success.
pub async fn realize_via_daemon(
    store: &DaemonStore,
    drv_path: &str,
    out_path: &str,
) -> Result<Realized, DaemonRealizeError> {
    // The demanded read path may carry a subpath (`readFile "${drv}/data.txt"`).
    // The daemon speaks in store-path ROOTS — `IsValidPath`/`QueryPathInfo`
    // reject a path inside an output. Query on the root; the eval side already
    // reads the full subpath once the root is valid.
    let out_root = store_path_root(out_path);

    let mut conn = DaemonConn::connect(store.socket()).await?;
    conn.handshake().await?;
    conn.set_options().await?;

    // Fast path: already realized (a prior build, or substituted out-of-band).
    if conn.is_valid_path(out_root).await? {
        let nar = conn.query_nar_hash(out_root).await.ok().flatten();
        return Realized::attested(drv_path, out_path, true, nar);
    }

    // Feed the daemon every `.drv` in the closure it doesn't already have, so
    // `BuildPaths` can resolve the graph. sui's `.drv` bytes are byte-identical
    // to nix's (the parity guarantee), so `AddToStore` yields the exact same
    // `.drv` store path the build graph references.
    let drv_closure = collect_drv_closure(drv_path)?;
    for drv in &drv_closure {
        if !conn.is_valid_path(&drv.store_path).await? {
            conn.add_drv_to_store(&drv.store_path, &drv.disk_path).await?;
        }
    }

    // Build (substitute-or-build) the target derivation.
    conn.build_paths(&[drv_path]).await?;

    // Mint the proof from the daemon's own validity attestation on the root.
    let valid = conn.is_valid_path(out_root).await?;
    let nar = if valid { conn.query_nar_hash(out_root).await.ok().flatten() } else { None };
    Realized::attested(drv_path, out_path, valid, nar)
}

/// Realize `drv_path`'s output `out_path` through the daemon **within a
/// wall-clock bound** — the bounded-realize seal.
///
/// [`realize_via_daemon`] can block indefinitely inside `BuildPaths`: the daemon
/// substitutes-or-builds an arbitrarily large closure, and if the demanded
/// output path never becomes valid (a sui↔nix output-path divergence — the path
/// exists on no configured cache and its `.drv` cannot be built) the daemon may
/// stall waiting on a substituter or spend unbounded time trying to build it. In
/// the IFD-during-eval setting that blocks the *evaluator*, which is exactly the
/// full-cid-eval hang.
///
/// This wrapper makes the hang **unrepresentable at the realize boundary**: the
/// call either resolves (a `Realized` proof or a typed daemon error) or, on
/// elapse of `bound`, returns [`DaemonRealizeError::Timeout`]. There is no code
/// path that returns neither within `bound`.
///
/// # Tier
///
/// The bound is an **only-mitigated C5 ceiling** (non-transactional, external,
/// non-cancellable daemon I/O — a wall-clock bound is the correct terminal
/// answer, not a compile error). It composes with the *parse-time-rejected*
/// [`DaemonRealizeError::OutputAbsent`] seal: when the daemon build finishes but
/// the output is invalid, that stronger seal fires first; the timeout is the
/// floor for the case where the build never finishes at all.
///
/// # Errors
///
/// [`DaemonRealizeError::Timeout`] on elapse; otherwise any error
/// [`realize_via_daemon`] surfaces.
pub async fn realize_via_daemon_bounded(
    store: &DaemonStore,
    drv_path: &str,
    out_path: &str,
    bound: std::time::Duration,
) -> Result<Realized, DaemonRealizeError> {
    match tokio::time::timeout(bound, realize_via_daemon(store, drv_path, out_path)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(DaemonRealizeError::Timeout {
            drv: drv_path.to_string(),
            out: out_path.to_string(),
            bound_secs: bound.as_secs(),
        }),
    }
}

/// Extract the store-path ROOT (`/nix/store/<hash>-<name>`) from a possibly-
/// deeper path (`/nix/store/<hash>-<name>/sub/dir`). The daemon's path ops
/// operate on roots, never on subpaths. A path that is already a root (or is not
/// under `/nix/store`) is returned unchanged.
fn store_path_root(path: &str) -> &str {
    const PREFIX: &str = "/nix/store/";
    let Some(rest) = path.strip_prefix(PREFIX) else {
        return path;
    };
    // The root is the first component after the store prefix.
    match rest.find('/') {
        Some(slash) => &path[..PREFIX.len() + slash],
        None => path,
    }
}

/// One `.drv` in a closure and where its bytes live on disk.
struct DrvOnDisk {
    /// The canonical `/nix/store/…​.drv` path (the daemon-side name).
    store_path: String,
    /// Where the bytes actually are on disk (store path or fallback cache).
    disk_path: PathBuf,
}

/// Walk `drv_path`'s input-derivation closure, resolving each `.drv` to the
/// bytes on disk. Reads the `.drv` from `/nix/store` when present, else from the
/// `sui-drv-cache` fallback where `sui-eval` parked it on a read-only store.
///
/// Leaves-first order is not required for `BuildPaths` (the daemon resolves the
/// graph itself once every `.drv` is added), but we still add drvs bottom-up so
/// the daemon never sees a reference to an unadded input.
fn collect_drv_closure(drv_path: &str) -> Result<Vec<DrvOnDisk>, DaemonRealizeError> {
    use std::collections::BTreeSet;
    use sui_compat::derivation::Derivation;

    let fallback = std::env::temp_dir().join(DRV_FALLBACK_DIR);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut ordered: Vec<DrvOnDisk> = Vec::new();

    // Depth-first post-order so inputs precede the drvs that reference them.
    fn visit(
        drv_path: &str,
        fallback: &Path,
        seen: &mut BTreeSet<String>,
        ordered: &mut Vec<DrvOnDisk>,
    ) -> Result<(), DaemonRealizeError> {
        if !seen.insert(drv_path.to_string()) {
            return Ok(());
        }
        let disk_path = resolve_drv_on_disk(drv_path, fallback)
            .ok_or_else(|| DaemonRealizeError::DrvMissing(drv_path.to_string()))?;
        let bytes = std::fs::read(&disk_path)
            .map_err(|e| DaemonRealizeError::Protocol(format!("read {disk_path:?}: {e}")))?;
        let drv = Derivation::parse(&bytes)
            .map_err(|e| DaemonRealizeError::Protocol(format!("parse {drv_path}: {e}")))?;
        for input_drv in drv.input_derivations.keys() {
            visit(input_drv, fallback, seen, ordered)?;
        }
        ordered.push(DrvOnDisk { store_path: drv_path.to_string(), disk_path });
        Ok(())
    }

    visit(drv_path, &fallback, &mut seen, &mut ordered)?;
    Ok(ordered)
}

/// Find a `.drv`'s bytes on disk: the real store path if present, else the
/// `sui-eval` fallback cache keyed by basename.
fn resolve_drv_on_disk(drv_path: &str, fallback: &Path) -> Option<PathBuf> {
    let real = PathBuf::from(drv_path);
    if real.exists() {
        return Some(real);
    }
    let base = real.file_name()?;
    let cached = fallback.join(base);
    if cached.exists() {
        return Some(cached);
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Worker-protocol client connection.
// ─────────────────────────────────────────────────────────────────────────────

/// A connected, handshaked worker-protocol client over a Unix socket.
struct DaemonConn {
    stream: UnixStream,
}

impl DaemonConn {
    async fn connect(socket: &Path) -> Result<Self, DaemonRealizeError> {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|_| DaemonRealizeError::Unreachable(socket.to_path_buf()))?;
        Ok(Self { stream })
    }

    fn proto(e: std::io::Error) -> DaemonRealizeError {
        DaemonRealizeError::Protocol(format!("io: {e}"))
    }

    async fn w_u64(&mut self, v: u64) -> Result<(), DaemonRealizeError> {
        self.stream.write_all(&v.to_le_bytes()).await.map_err(Self::proto)
    }

    async fn r_u64(&mut self) -> Result<u64, DaemonRealizeError> {
        let mut b = [0u8; 8];
        self.stream.read_exact(&mut b).await.map_err(Self::proto)?;
        Ok(u64::from_le_bytes(b))
    }

    async fn w_str(&mut self, v: &str) -> Result<(), DaemonRealizeError> {
        self.w_bytes(v.as_bytes()).await
    }

    /// Write a length-prefixed, 8-byte-aligned byte buffer (the worker-protocol
    /// `writeString`/byte-buffer primitive). Used for both UTF-8 strings and
    /// raw binary payloads (e.g. a `.drv`'s text).
    async fn w_bytes(&mut self, b: &[u8]) -> Result<(), DaemonRealizeError> {
        self.w_u64(b.len() as u64).await?;
        self.stream.write_all(b).await.map_err(Self::proto)?;
        let pad = (8 - (b.len() % 8)) % 8;
        if pad > 0 {
            self.stream.write_all(&[0u8; 8][..pad]).await.map_err(Self::proto)?;
        }
        Ok(())
    }

    async fn r_str(&mut self) -> Result<String, DaemonRealizeError> {
        let len = self.r_u64().await? as usize;
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf).await.map_err(Self::proto)?;
        let pad = (8 - (len % 8)) % 8;
        if pad > 0 {
            let mut p = [0u8; 8];
            self.stream.read_exact(&mut p[..pad]).await.map_err(Self::proto)?;
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    async fn w_str_list(&mut self, items: &[&str]) -> Result<(), DaemonRealizeError> {
        self.w_u64(items.len() as u64).await?;
        for item in items {
            self.w_str(item).await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), DaemonRealizeError> {
        self.stream.flush().await.map_err(Self::proto)
    }

    /// Client side of the worker-protocol handshake.
    async fn handshake(&mut self) -> Result<(), DaemonRealizeError> {
        self.w_u64(WORKER_MAGIC_1).await?;
        self.flush().await?;
        let magic2 = self.r_u64().await?;
        if magic2 != WORKER_MAGIC_2 {
            return Err(DaemonRealizeError::Protocol(format!("bad server magic {magic2:#x}")));
        }
        let _server_ver = self.r_u64().await?;
        self.w_u64(PROTOCOL_VERSION).await?;
        self.w_u64(0).await?; // cpu affinity (obsolete)
        self.w_u64(0).await?; // reserve space (obsolete)
        self.flush().await?;
        let _daemon_ver = self.r_str().await?;
        let _trust = self.r_u64().await?;
        // Handshake terminates with STDERR_LAST.
        let end = self.r_u64().await?;
        if end != STDERR_LAST {
            return Err(DaemonRealizeError::Protocol(format!(
                "handshake missing STDERR_LAST (got {end:#x})"
            )));
        }
        Ok(())
    }

    /// `SetOptions` with the version-gated fields the negotiated 1.37 daemon
    /// reads. All zero / empty overrides, but with `useSubstitutes = 1` so the
    /// daemon substitutes from configured caches before building (the
    /// substitute-first posture the pivot requires).
    async fn set_options(&mut self) -> Result<(), DaemonRealizeError> {
        self.w_u64(WorkerOp::SetOptions as u64).await?;
        for _ in 0..6 {
            self.w_u64(0).await?; // base 6 fields
        }
        self.w_u64(0).await?; // useBuildHook (>=2)
        self.w_u64(0).await?; // verboseBuild (>=4)
        self.w_u64(0).await?; // logType (>=6)
        self.w_u64(0).await?; // printBuildTrace (>=6)
        self.w_u64(0).await?; // buildCores (>=10)
        self.w_u64(1).await?; // useSubstitutes (>=11) — substitute-first
        self.w_u64(0).await?; // overrides count (>=12)
        self.flush().await?;
        self.drain_stderr().await
    }

    /// `IsValidPath path` → bool.
    async fn is_valid_path(&mut self, path: &str) -> Result<bool, DaemonRealizeError> {
        self.w_u64(WorkerOp::IsValidPath as u64).await?;
        self.w_str(path).await?;
        self.flush().await?;
        self.drain_stderr().await?;
        Ok(self.r_u64().await? != 0)
    }

    /// `QueryPathInfo path` → NAR hash (best-effort; `None` if the daemon
    /// reports the path invalid or omits the info). Used only to *record* the
    /// attested hash on [`Realized`]; never to weaken the proof.
    async fn query_nar_hash(&mut self, path: &str) -> Result<Option<String>, DaemonRealizeError> {
        self.w_u64(WorkerOp::QueryPathInfo as u64).await?;
        self.w_str(path).await?;
        self.flush().await?;
        self.drain_stderr().await?;
        // Protocol >= 17: a leading `bool valid` precedes the info fields.
        let valid = self.r_u64().await?;
        if valid == 0 {
            return Ok(None);
        }
        // ValidPathInfo layout (proto 1.37): deriver, narHash, references,
        // registrationTime, narSize, ultimate, sigs, ca.
        let _deriver = self.r_str().await?;
        let nar_hash = self.r_str().await?;
        // Drain the remaining fields so the connection stays framed for reuse.
        let refs = self.r_u64().await? as usize;
        for _ in 0..refs {
            let _ = self.r_str().await?;
        }
        let _registration_time = self.r_u64().await?;
        let _nar_size = self.r_u64().await?;
        let _ultimate = self.r_u64().await?;
        let sigs = self.r_u64().await? as usize;
        for _ in 0..sigs {
            let _ = self.r_str().await?;
        }
        let _ca = self.r_str().await?;
        Ok(Some(nar_hash))
    }

    /// `AddTextToStore` a `.drv` file from `disk_path` under the store name
    /// derived from `store_path`'s basename.
    ///
    /// A `.drv` is a **text** store object (text-addressed with references to
    /// its input drvs + srcs), so it is added via `AddTextToStore` (opcode 8) —
    /// which takes the raw text bytes directly (no NAR wrapping) — NOT
    /// `AddToStore` (opcode 7, which NAR-dumps an arbitrary path). sui's
    /// serialized `.drv` bytes are byte-identical to nix's, so the daemon
    /// computes the exact same `text:sha256` store path the build graph
    /// references.
    async fn add_drv_to_store(
        &mut self,
        store_path: &str,
        disk_path: &Path,
    ) -> Result<(), DaemonRealizeError> {
        let name = Path::new(store_path)
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| DaemonRealizeError::Protocol(format!("bad drv path {store_path}")))?;

        let bytes = std::fs::read(disk_path)
            .map_err(|e| DaemonRealizeError::Protocol(format!("read {disk_path:?}: {e}")))?;

        // Recover the references (input drvs + input srcs) — the daemon needs
        // them to compute the `text:` store path and validate the closure.
        let drv = sui_compat::derivation::Derivation::parse(&bytes)
            .map_err(|e| DaemonRealizeError::Protocol(format!("parse {store_path}: {e}")))?;
        let mut refs: Vec<String> = drv.input_derivations.keys().cloned().collect();
        refs.extend(drv.input_sources.iter().cloned());
        refs.sort();
        refs.dedup();
        let ref_strs: Vec<&str> = refs.iter().map(String::as_str).collect();

        self.w_u64(WorkerOp::AddTextToStore as u64).await?;
        self.w_str(name).await?;
        self.w_bytes(&bytes).await?; // the raw `.drv` text, length-prefixed + padded
        self.w_str_list(&ref_strs).await?;
        self.flush().await?;
        self.drain_stderr().await?;
        // Reply is the resulting store path; read and discard it so the
        // connection stays framed for the next op.
        let _added_path = self.r_str().await?;
        Ok(())
    }

    /// `BuildPaths [<drv!*>...]` with `bmNormal` mode — the daemon
    /// substitutes-or-builds each derivation.
    async fn build_paths(&mut self, drv_paths: &[&str]) -> Result<(), DaemonRealizeError> {
        // Protocol >= 30 sends derivation paths with a `!outputs` selector; a
        // bare `.drv` path means "all outputs", which is what nix sends for a
        // wildcard IFD realize.
        let with_all: Vec<String> = drv_paths.iter().map(|d| format!("{d}!*")).collect();
        let refs: Vec<&str> = with_all.iter().map(String::as_str).collect();

        self.w_u64(WorkerOp::BuildPaths as u64).await?;
        self.w_str_list(&refs).await?;
        self.w_u64(0).await?; // buildMode = bmNormal (>=15)
        self.flush().await?;
        // BuildPaths streams build logs on stderr; an Error frame is a build
        // failure. On STDERR_LAST the reply is a single `1` (success ack).
        self.drain_stderr_build().await?;
        let _ok = self.r_u64().await?;
        Ok(())
    }

    /// Drain worker-protocol stderr frames until STDERR_LAST. An Error frame
    /// becomes a `Protocol` error.
    async fn drain_stderr(&mut self) -> Result<(), DaemonRealizeError> {
        loop {
            let msg = self.r_u64().await?;
            match msg {
                m if m == STDERR_LAST => return Ok(()),
                m if m == STDERR_ERROR => {
                    let text = self.read_error_frame().await?;
                    return Err(DaemonRealizeError::Protocol(text));
                }
                m if m == STDERR_WRITE => {
                    let _ = self.r_str().await?;
                }
                other => {
                    // Unknown activity/result frames (proto >= 20) carry a
                    // structured payload; treating anything unexpected on a
                    // non-build op as a hard error keeps the framing honest.
                    return Err(DaemonRealizeError::Protocol(format!(
                        "unexpected stderr frame {other:#x}"
                    )));
                }
            }
        }
    }

    /// Like [`drain_stderr`] but tolerant of the richer activity/log frame set
    /// the daemon emits during a build (proto >= 20). An Error frame is a build
    /// failure.
    async fn drain_stderr_build(&mut self) -> Result<(), DaemonRealizeError> {
        // Frame tags for the structured logger (worker-protocol.hh, proto >= 20).
        const STDERR_START_ACTIVITY: u64 = StderrMsg::StartActivity as u64;
        const STDERR_STOP_ACTIVITY: u64 = StderrMsg::StopActivity as u64;
        const STDERR_RESULT: u64 = StderrMsg::Result as u64;
        loop {
            let msg = self.r_u64().await?;
            match msg {
                m if m == STDERR_LAST => return Ok(()),
                m if m == STDERR_ERROR => {
                    let text = self.read_error_frame().await?;
                    return Err(DaemonRealizeError::Build(text));
                }
                m if m == STDERR_WRITE => {
                    let _ = self.r_str().await?;
                }
                m if m == STDERR_START_ACTIVITY => {
                    // act(u64), lvl(u64), type(u64), s(string), fields, parent(u64)
                    let _act = self.r_u64().await?;
                    let _lvl = self.r_u64().await?;
                    let _typ = self.r_u64().await?;
                    let _s = self.r_str().await?;
                    self.read_fields().await?;
                    let _parent = self.r_u64().await?;
                }
                m if m == STDERR_STOP_ACTIVITY => {
                    let _act = self.r_u64().await?;
                }
                m if m == STDERR_RESULT => {
                    let _act = self.r_u64().await?;
                    let _typ = self.r_u64().await?;
                    self.read_fields().await?;
                }
                other => {
                    return Err(DaemonRealizeError::Protocol(format!(
                        "unexpected build stderr frame {other:#x}"
                    )));
                }
            }
        }
    }

    /// Read a structured `Fields` payload (a count-prefixed list of typed
    /// fields: type tag + either a u64 or a string).
    async fn read_fields(&mut self) -> Result<(), DaemonRealizeError> {
        let n = self.r_u64().await? as usize;
        for _ in 0..n {
            let ty = self.r_u64().await?;
            match ty {
                0 => {
                    let _ = self.r_u64().await?; // int
                }
                1 => {
                    let _ = self.r_str().await?; // string
                }
                other => {
                    return Err(DaemonRealizeError::Protocol(format!(
                        "unknown log field type {other}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Read the body of an STDERR_ERROR frame → its message text.
    async fn read_error_frame(&mut self) -> Result<String, DaemonRealizeError> {
        // proto >= 26: a serialized `Error` struct; older: type + msg + havePos.
        // The daemon we negotiate (1.37) sends the struct form:
        //   type(string) level(u64) name(string) msg(string) havePos(u64)
        //   traces(list of {havePos(u64) hint(string)})
        // We only need `msg`; read the whole struct to stay framed.
        let _type = self.r_str().await?;
        let _level = self.r_u64().await?;
        let _name = self.r_str().await?;
        let msg = self.r_str().await?;
        let _have_pos = self.r_u64().await?;
        let ntraces = self.r_u64().await? as usize;
        for _ in 0..ntraces {
            let _have_pos = self.r_u64().await?;
            let _hint = self.r_str().await?;
        }
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_store_probe_rejects_readonly_dir() {
        // A root-owned read-only dir (like the multi-user /nix/store) yields
        // no WritableStore — so StoreAccess::Direct over it is unconstructable.
        let ro = Path::new("/nix/store");
        // On a multi-user store this is None; on a single-user CI store it is
        // Some. Both are correct — the invariant is that Some implies writable.
        if let Some(w) = WritableStore::probe(ro) {
            // If we got one, a probe write must genuinely have succeeded.
            let probe = w.store_dir().join(format!(".sui-write-probe-recheck-{}", std::process::id()));
            let created = std::fs::File::create(&probe).is_ok();
            let _ = std::fs::remove_file(&probe);
            assert!(created, "WritableStore handed out over a non-writable dir");
        }
    }

    #[test]
    fn writable_store_probe_accepts_tmp() {
        let tmp = std::env::temp_dir();
        assert!(
            WritableStore::probe(&tmp).is_some(),
            "temp dir must probe as writable"
        );
    }

    #[test]
    fn daemon_store_at_missing_socket_is_none() {
        let missing = PathBuf::from("/nonexistent/sui-daemon-socket-xyz");
        assert!(DaemonStore::at(missing).is_none());
    }

    #[test]
    fn store_access_detect_with_writable_prefers_direct() {
        // A writable store dir + a bogus socket → Direct (no NIX_REMOTE forcing).
        // Guarded on NIX_REMOTE being unset to keep the test hermetic.
        if std::env::var("NIX_REMOTE").is_ok() {
            return;
        }
        let tmp = std::env::temp_dir();
        let bogus_socket = PathBuf::from("/nonexistent/socket");
        let access = StoreAccess::detect_with(&tmp, &bogus_socket);
        assert!(matches!(access, Some(StoreAccess::Direct(_))));
        assert!(!access.unwrap().is_daemon());
    }

    #[test]
    fn store_access_detect_with_readonly_and_no_socket_is_none() {
        if std::env::var("NIX_REMOTE").is_ok() {
            return;
        }
        // A read-only store dir + no reachable socket → no write path at all.
        let ro = PathBuf::from("/proc/nonexistent-readonly-xyz");
        let bogus_socket = PathBuf::from("/nonexistent/socket");
        let access = StoreAccess::detect_with(&ro, &bogus_socket);
        assert!(access.is_none());
    }

    #[test]
    fn realized_requires_valid_output() {
        // The proof is unconstructable for an invalid output.
        let err = Realized::attested("/nix/store/x.drv", "/nix/store/x-out", false, None);
        assert!(matches!(err, Err(DaemonRealizeError::OutputAbsent { .. })));

        // A valid attestation mints the proof and records the nar hash.
        let ok = Realized::attested(
            "/nix/store/x.drv",
            "/nix/store/x-out",
            true,
            Some("sha256:abc".to_string()),
        )
        .unwrap();
        assert_eq!(ok.out_path(), "/nix/store/x-out");
        assert_eq!(ok.nar_hash(), Some("sha256:abc"));
    }

    #[test]
    fn store_path_root_strips_subpath() {
        assert_eq!(
            store_path_root("/nix/store/abc-name/data.txt"),
            "/nix/store/abc-name"
        );
        assert_eq!(
            store_path_root("/nix/store/abc-name/deep/sub/dir"),
            "/nix/store/abc-name"
        );
        // A bare root is returned unchanged.
        assert_eq!(store_path_root("/nix/store/abc-name"), "/nix/store/abc-name");
        // A non-store path is returned unchanged.
        assert_eq!(store_path_root("/tmp/whatever/x"), "/tmp/whatever/x");
    }

    #[tokio::test]
    async fn bounded_realize_times_out_on_a_stalled_daemon() {
        // A listener that ACCEPTS the connection but NEVER replies to the
        // handshake models the stall the bound seals: the realize future blocks
        // in `read_exact` forever. The bound must convert that into a typed
        // `Timeout`, never an unbounded hang.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("stall.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        // Hold the accepted stream so the OS keeps the connection open (silent).
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                // Park the connection open; never write a byte back.
                let _held = stream;
                std::future::pending::<()>().await;
            }
        });

        let store = DaemonStore::at(sock).expect("socket exists");
        let start = std::time::Instant::now();
        let bound = std::time::Duration::from_millis(150);
        let err = realize_via_daemon_bounded(
            &store,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv",
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-x",
            bound,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, DaemonRealizeError::Timeout { bound_secs: 0, .. }),
            "expected a Timeout (bound_secs=0 for a sub-second bound), got: {err:?}"
        );
        // The bound is honored: we returned promptly, not after a real hang.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "bounded realize must return near its bound, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn resolve_drv_prefers_store_then_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let fallback = tmp.path().join(DRV_FALLBACK_DIR);
        std::fs::create_dir_all(&fallback).unwrap();

        // A drv whose store path is absent but whose bytes are in the fallback.
        let store_path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-probe.drv";
        let base = Path::new(store_path).file_name().unwrap();
        let cached = fallback.join(base);
        std::fs::write(&cached, b"Derive([])").unwrap();

        let resolved = resolve_drv_on_disk(store_path, &fallback);
        assert_eq!(resolved.as_deref(), Some(cached.as_path()));

        // A drv absent everywhere resolves to None.
        assert!(resolve_drv_on_disk("/nix/store/zzz-missing.drv", &fallback).is_none());
    }

    /// LIVE GATE-2 PROBE — proves sui's worker-protocol client handshakes and
    /// queries a REAL nix daemon (the untested surface for a cid switch: sui
    /// sends PROTOCOL_VERSION 1.37; cid's daemon is nix 2.34.7 ≈ 1.38). The
    /// fast path (`is_valid_path` true on an already-valid output) exercises
    /// connect → handshake → set_options → is_valid_path → query_nar_hash in
    /// <1s with ZERO build. A framing drift (misparsed handshake, wrong
    /// ValidPathInfo layout, STDERR-drain desync) surfaces here as an Err.
    ///
    /// `#[ignore]` because CI has no nix daemon; run on cid:
    ///   `cargo test -p sui-store daemon_live_probe -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "live probe: requires a running nix daemon (run on cid)"]
    async fn daemon_live_probe_handshake_and_query() {
        let socket = std::path::PathBuf::from("/nix/var/nix/daemon-socket/socket");
        let Some(store) = DaemonStore::at(socket) else {
            eprintln!("SKIP daemon_live_probe: no daemon socket present");
            return;
        };
        // Discover any already-valid store OUTPUT path (a real dir, not a .drv).
        let valid = std::fs::read_dir("/nix/store")
            .expect("read /nix/store")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.is_dir() && !p.to_string_lossy().ends_with(".drv"))
            .expect("at least one valid store output path");
        let valid_s = valid.to_string_lossy().into_owned();

        // Fast path: is_valid_path==true → returns after the handshake round-trip
        // without building anything. A wrong protocol negotiation Errs here — the
        // exact GATE-2 de-risk signal.
        let realized = realize_via_daemon_bounded(
            &store,
            "/nix/store/00000000000000000000000000000000-unused.drv",
            &valid_s,
            std::time::Duration::from_secs(15),
        )
        .await
        .expect(
            "daemon handshake + set_options + is_valid_path + query_nar_hash must \
             succeed against the live nix daemon (GATE-2 protocol proof)",
        );

        assert_eq!(realized.out_path(), &valid_s);
        eprintln!(
            "DAEMON LIVE PROBE OK: handshake+set_options+is_valid_path+query_nar_hash \
             succeeded on {valid_s} (nar_hash={:?}) — sui's worker-protocol client \
             speaks the live nix daemon.",
            realized.nar_hash()
        );
    }
}
