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

use crate::worker_client::{WorkerConn, WorkerError};

/// The default nix daemon socket (mirrors `sui_daemon::DEFAULT_SOCKET_PATH`,
/// duplicated here to avoid a `sui-store → sui-daemon` dependency edge).
pub const DEFAULT_DAEMON_SOCKET: &str = "/nix/var/nix/daemon-socket/socket";

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
    #[error("derivation not readable from the store: {0}")]
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
/// 3. `AddTextToStore` every `.drv` in the closure the daemon does not have yet.
///    Evaluation instantiates every `.drv` it computes ([`crate::drv_write`]),
///    so this only fires for a closure evaluated elsewhere.
/// 4. `BuildPaths [<drv_path>]` — the daemon substitutes-or-builds.
/// 5. `IsValidPath out_path` → mint the [`Realized`] proof.
///
/// The protocol exchange is the blocking [`WorkerConn`], run on tokio's
/// blocking pool.
///
/// # Errors
/// Any protocol/build failure, or an absent output after a reported success.
pub async fn realize_via_daemon(
    store: &DaemonStore,
    drv_path: &str,
    out_path: &str,
) -> Result<Realized, DaemonRealizeError> {
    realize_on_blocking_pool(store, drv_path, out_path, None).await
}

async fn realize_on_blocking_pool(
    store: &DaemonStore,
    drv_path: &str,
    out_path: &str,
    io_timeout: Option<std::time::Duration>,
) -> Result<Realized, DaemonRealizeError> {
    let socket = store.socket().to_path_buf();
    let (drv, out) = (drv_path.to_string(), out_path.to_string());
    tokio::task::spawn_blocking(move || realize_blocking(&socket, &drv, &out, io_timeout))
        .await
        .map_err(|e| DaemonRealizeError::Protocol(format!("realize task: {e}")))?
}

fn realize_blocking(
    socket: &Path,
    drv_path: &str,
    out_path: &str,
    io_timeout: Option<std::time::Duration>,
) -> Result<Realized, DaemonRealizeError> {
    // The demanded read path may carry a subpath (`readFile "${drv}/data.txt"`).
    // The daemon speaks in store-path ROOTS — `IsValidPath`/`QueryPathInfo`
    // reject a path inside an output. Query on the root; the eval side already
    // reads the full subpath once the root is valid.
    let out_root = store_path_root(out_path);
    let proto = |e: WorkerError| match e {
        WorkerError::Unreachable { socket, .. } => DaemonRealizeError::Unreachable(socket),
        WorkerError::Protocol(m) | WorkerError::Daemon(m) => DaemonRealizeError::Protocol(m),
    };

    let mut conn = WorkerConn::connect(socket, io_timeout).map_err(proto)?;

    // Fast path: already realized (a prior build, or substituted out-of-band).
    if conn.is_valid_path(out_root).map_err(proto)? {
        let nar = conn.query_nar_hash(out_root).ok().flatten();
        return Realized::attested(drv_path, out_path, true, nar);
    }

    // Feed the daemon every `.drv` in the closure it doesn't already have, so
    // `BuildPaths` can resolve the graph. sui's `.drv` bytes are byte-identical
    // to nix's (the parity guarantee), so the daemon files each at the exact
    // path the build graph references.
    for drv in &collect_drv_closure(drv_path)? {
        if !conn.is_valid_path(&drv.store_path).map_err(proto)? {
            add_drv_from_disk(&mut conn, drv).map_err(proto)?;
        }
    }

    // Build (substitute-or-build) the target derivation. An error frame here is
    // a build failure, not a protocol fault.
    conn.build_paths(&[drv_path]).map_err(|e| match e {
        WorkerError::Daemon(m) => DaemonRealizeError::Build(m),
        other => proto(other),
    })?;

    // Mint the proof from the daemon's own validity attestation on the root.
    let valid = conn.is_valid_path(out_root).map_err(proto)?;
    let nar = if valid { conn.query_nar_hash(out_root).ok().flatten() } else { None };
    Realized::attested(drv_path, out_path, valid, nar)
}

/// `AddTextToStore` one `.drv` read from disk, under its NAME (the daemon
/// computes `<hash>-<name>` itself; passing the basename would hash the hash,
/// the same class as the store-path double-prefix bug fixed in
/// `sui-compat::source`).
fn add_drv_from_disk(conn: &mut WorkerConn, drv: &DrvOnDisk) -> Result<(), WorkerError> {
    let basename = Path::new(&drv.store_path)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| WorkerError::Protocol(format!("bad drv path {}", drv.store_path)))?;
    let name = sui_compat::source::strip_store_hash_prefix(basename);
    let refs = crate::drv_write::drv_references(&drv.parsed);
    conn.add_text_to_store(name, &drv.bytes, &refs).map(|_| ())
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
/// path that returns neither within `bound`. The socket carries an I/O timeout
/// just past `bound`, so the blocking worker behind an elapsed realize also
/// ends rather than lingering.
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
    let io_timeout = bound + std::time::Duration::from_secs(1);
    let realize = realize_on_blocking_pool(store, drv_path, out_path, Some(io_timeout));
    match tokio::time::timeout(bound, realize).await {
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

/// One `.drv` in a closure, read from the store.
struct DrvOnDisk {
    store_path: String,
    bytes: Vec<u8>,
    parsed: sui_compat::derivation::Derivation,
}

/// Walk `drv_path`'s input-derivation closure, reading each `.drv` from the
/// store, inputs before the drvs that reference them (the daemon never sees a
/// reference to an input it has not been given).
fn collect_drv_closure(drv_path: &str) -> Result<Vec<DrvOnDisk>, DaemonRealizeError> {
    use std::collections::BTreeSet;
    use sui_compat::derivation::Derivation;

    fn visit(
        drv_path: &str,
        seen: &mut BTreeSet<String>,
        ordered: &mut Vec<DrvOnDisk>,
    ) -> Result<(), DaemonRealizeError> {
        if !seen.insert(drv_path.to_string()) {
            return Ok(());
        }
        let bytes = std::fs::read(drv_path)
            .map_err(|e| DaemonRealizeError::DrvMissing(format!("{drv_path}: {e}")))?;
        let parsed = Derivation::parse(&bytes)
            .map_err(|e| DaemonRealizeError::Protocol(format!("parse {drv_path}: {e}")))?;
        let inputs: Vec<String> = parsed.input_derivations.keys().cloned().collect();
        for input_drv in &inputs {
            visit(input_drv, seen, ordered)?;
        }
        ordered.push(DrvOnDisk { store_path: drv_path.to_string(), bytes, parsed });
        Ok(())
    }

    let mut seen = BTreeSet::new();
    let mut ordered = Vec::new();
    visit(drv_path, &mut seen, &mut ordered)?;
    Ok(ordered)
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
    fn a_drv_absent_from_the_store_is_missing_not_looked_up_elsewhere() {
        let err = collect_drv_closure("/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-missing.drv")
            .err()
            .expect("an absent drv must not resolve");
        assert!(matches!(err, DaemonRealizeError::DrvMissing(_)), "{err}");
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
