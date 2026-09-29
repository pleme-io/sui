//! The worker-protocol client — sui talking to a running nix daemon.
//!
//! One synchronous implementation over `std::os::unix::net::UnixStream`, built on
//! [`sui_compat::wire`]'s primitives. Synchronous on purpose: its callers are the
//! evaluator thread (writing `.drv`s), async realize code (through
//! `spawn_blocking`) and the process-lifetime root session, and a blocking std
//! socket is the one shape all three can hold without being tied to a tokio
//! reactor.
//!
//! Mirrors CppNix `remote-store.cc` at protocol 1.37.

use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sui_compat::wire::{
    self, PROTOCOL_VERSION, StderrMsg, WORKER_MAGIC_1, WORKER_MAGIC_2, WorkerOp,
};

/// Errors from a worker-protocol exchange.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// The daemon socket could not be connected.
    #[error("nix daemon socket unreachable at {socket}: {source}")]
    Unreachable {
        socket: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Transport or framing failure.
    #[error("daemon protocol error: {0}")]
    Protocol(String),
    /// The daemon answered an operation with an error frame.
    #[error("daemon refused: {0}")]
    Daemon(String),
    /// nix's configuration could not be read, so the options to send are unknown.
    #[error("nix configuration: {0}")]
    Config(#[from] sui_compat::nix_conf::NixConfError),
}

/// The client settings CppNix's `RemoteStore::setOptions` sends, from nix's
/// configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientOptions {
    /// `max-jobs` (default 1; `auto` is the number of CPUs).
    pub max_jobs: u64,
    /// `max-silent-time` (default 0).
    pub max_silent_time: u64,
    /// `cores` (default 0: every core).
    pub build_cores: u64,
}

impl ClientOptions {
    /// Read the options from nix's configuration ([`sui_compat::nix_conf`]).
    ///
    /// # Errors
    /// A configuration CppNix would refuse.
    pub fn from_nix_config() -> Result<Self, WorkerError> {
        Ok(Self::from_config(&sui_compat::nix_conf::NixConfig::load()?))
    }

    /// Read the options from an already-loaded configuration. A value that does
    /// not parse falls back to CppNix's default for it.
    #[must_use]
    pub fn from_config(cfg: &sui_compat::nix_conf::NixConfig) -> Self {
        let int = |name: &str, default: u64| {
            cfg.get(name).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
        };
        let max_jobs = match cfg.get("max-jobs").map(str::trim) {
            Some("auto") => std::thread::available_parallelism().map_or(1, |n| n.get() as u64),
            _ => int("max-jobs", 1),
        };
        Self { max_jobs, max_silent_time: int("max-silent-time", 0), build_cores: int("cores", 0) }
    }

    /// The `SetOptions` fields after the op code, before the overrides map, in
    /// wire order (protocol 1.37).
    #[must_use]
    pub fn fields(&self) -> [u64; 12] {
        [
            0,                    // keepFailed
            0,                    // keepGoing
            0,                    // tryFallback
            0,                    // verbosity (lvlError)
            self.max_jobs,        // maxBuildJobs
            self.max_silent_time, // maxSilentTime
            1,                    // useBuildHook: remote builders stay available
            0,                    // build verbosity (lvlError)
            0,                    // obsolete log type
            0,                    // obsolete print build trace
            self.build_cores,     // buildCores
            1,                    // useSubstitutes
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> sui_compat::nix_conf::NixConfig {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nix.conf"), text).unwrap();
        sui_compat::nix_conf::NixConfig::load_from(&sui_compat::nix_conf::ConfigSources {
            system: dir.path().join("nix.conf"),
            user: vec![],
            nix_config: None,
        })
        .unwrap()
    }

    #[test]
    fn set_options_carries_the_configured_build_limits_and_the_build_hook() {
        let f = ClientOptions::from_config(&cfg("max-jobs = 14\ncores = 3\nmax-silent-time = 3600\n")).fields();
        assert_eq!(f[4], 14, "maxBuildJobs");
        assert_eq!(f[5], 3600, "maxSilentTime");
        assert_eq!(f[6], 1, "useBuildHook");
        assert_eq!(f[10], 3, "buildCores");
        assert_eq!(f[11], 1, "useSubstitutes");
    }

    #[test]
    fn unset_options_take_cppnix_defaults_never_zero_jobs() {
        let f = ClientOptions::from_config(&cfg("")).fields();
        assert_eq!(f[4], 1, "max-jobs defaults to 1; 0 would forbid local builds");
        let auto = ClientOptions::from_config(&cfg("max-jobs = auto\n")).fields();
        assert!(auto[4] >= 1);
    }
}

impl From<std::io::Error> for WorkerError {
    fn from(e: std::io::Error) -> Self {
        Self::Protocol(format!("io: {e}"))
    }
}

impl From<wire::WireError> for WorkerError {
    fn from(e: wire::WireError) -> Self {
        Self::Protocol(e.to_string())
    }
}

/// A connected, handshaked worker-protocol client.
pub struct WorkerConn {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
}

impl std::fmt::Debug for WorkerConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerConn").finish_non_exhaustive()
    }
}

impl WorkerConn {
    /// Connect to `socket`, handshake, and send `SetOptions`.
    ///
    /// `io_timeout` bounds every read and write on the socket; `None` blocks
    /// indefinitely (the daemon decides how long a build takes).
    ///
    /// # Errors
    /// [`WorkerError::Unreachable`] when the socket does not accept, otherwise
    /// any handshake failure.
    pub fn connect(socket: &Path, io_timeout: Option<Duration>) -> Result<Self, WorkerError> {
        let stream = UnixStream::connect(socket).map_err(|source| WorkerError::Unreachable {
            socket: socket.to_path_buf(),
            source,
        })?;
        stream.set_read_timeout(io_timeout)?;
        stream.set_write_timeout(io_timeout)?;
        let reader = BufReader::new(stream.try_clone()?);
        let writer = BufWriter::new(stream);
        let mut conn = Self { reader, writer };
        conn.handshake()?;
        conn.set_options()?;
        Ok(conn)
    }

    fn u64(&mut self, v: u64) -> Result<(), WorkerError> {
        Ok(wire::write_u64(&mut self.writer, v)?)
    }

    fn str(&mut self, s: &str) -> Result<(), WorkerError> {
        Ok(wire::write_string(&mut self.writer, s)?)
    }

    fn read_u64(&mut self) -> Result<u64, WorkerError> {
        Ok(wire::read_u64(&mut self.reader)?)
    }

    fn read_str(&mut self) -> Result<String, WorkerError> {
        Ok(wire::read_string(&mut self.reader)?)
    }

    /// Send op + arguments written by `args`, then drain stderr frames.
    fn call(
        &mut self,
        op: WorkerOp,
        args: impl FnOnce(&mut Self) -> Result<(), WorkerError>,
    ) -> Result<(), WorkerError> {
        self.u64(op as u64)?;
        args(self)?;
        self.writer.flush()?;
        self.drain_stderr()
    }

    fn handshake(&mut self) -> Result<(), WorkerError> {
        self.u64(WORKER_MAGIC_1)?;
        self.writer.flush()?;
        let magic2 = self.read_u64()?;
        if magic2 != WORKER_MAGIC_2 {
            return Err(WorkerError::Protocol(format!("bad server magic {magic2:#x}")));
        }
        let _server_version = self.read_u64()?;
        self.u64(PROTOCOL_VERSION)?;
        self.u64(0)?; // cpu affinity (obsolete)
        self.u64(0)?; // reserve space (obsolete)
        self.writer.flush()?;
        let _daemon_version = self.read_str()?;
        let _trusted = self.read_u64()?;
        self.drain_stderr()
    }

    /// `SetOptions`, carrying what CppNix's client sends ([`ClientOptions`]).
    /// The daemon applies these to the connection, so sending `maxBuildJobs = 0`
    /// and `useBuildHook = 0` (as sui once did) forbids every local and remote
    /// build: only substitutable paths could be realized.
    fn set_options(&mut self) -> Result<(), WorkerError> {
        let opts = ClientOptions::from_nix_config()?;
        self.call(WorkerOp::SetOptions, |c| {
            for field in opts.fields() {
                c.u64(field)?;
            }
            c.u64(0) // overrides count
        })
    }

    /// Drain stderr frames until `STDERR_LAST`. An error frame becomes
    /// [`WorkerError::Daemon`]; log and activity frames are consumed.
    fn drain_stderr(&mut self) -> Result<(), WorkerError> {
        loop {
            let msg = self.read_u64()?;
            match msg {
                m if m == StderrMsg::Last as u64 => return Ok(()),
                m if m == StderrMsg::Error as u64 => {
                    return Err(WorkerError::Daemon(self.read_error_frame()?));
                }
                m if m == StderrMsg::Write as u64 => {
                    let _ = self.read_str()?;
                }
                m if m == StderrMsg::StartActivity as u64 => {
                    let _act = self.read_u64()?;
                    let _lvl = self.read_u64()?;
                    let _typ = self.read_u64()?;
                    let _s = self.read_str()?;
                    self.read_fields()?;
                    let _parent = self.read_u64()?;
                }
                m if m == StderrMsg::StopActivity as u64 => {
                    let _act = self.read_u64()?;
                }
                m if m == StderrMsg::Result as u64 => {
                    let _act = self.read_u64()?;
                    let _typ = self.read_u64()?;
                    self.read_fields()?;
                }
                other => {
                    return Err(WorkerError::Protocol(format!(
                        "unexpected stderr frame {other:#x}"
                    )));
                }
            }
        }
    }

    fn read_fields(&mut self) -> Result<(), WorkerError> {
        let n = self.read_u64()?;
        for _ in 0..n {
            match self.read_u64()? {
                0 => {
                    let _ = self.read_u64()?;
                }
                1 => {
                    let _ = self.read_str()?;
                }
                other => {
                    return Err(WorkerError::Protocol(format!("unknown log field type {other}")));
                }
            }
        }
        Ok(())
    }

    /// The body of an error frame (proto >= 26 struct form) → its message.
    fn read_error_frame(&mut self) -> Result<String, WorkerError> {
        let _type = self.read_str()?;
        let _level = self.read_u64()?;
        let _name = self.read_str()?;
        let msg = self.read_str()?;
        let _have_pos = self.read_u64()?;
        let ntraces = self.read_u64()?;
        for _ in 0..ntraces {
            let _have_pos = self.read_u64()?;
            let _hint = self.read_str()?;
        }
        Ok(msg)
    }

    /// `IsValidPath`.
    ///
    /// # Errors
    /// Any protocol failure.
    pub fn is_valid_path(&mut self, path: &str) -> Result<bool, WorkerError> {
        self.call(WorkerOp::IsValidPath, |c| c.str(path))?;
        Ok(self.read_u64()? != 0)
    }

    /// `QueryPathInfo` → the NAR hash, or `None` when the path is not valid.
    ///
    /// # Errors
    /// Any protocol failure.
    pub fn query_nar_hash(&mut self, path: &str) -> Result<Option<String>, WorkerError> {
        self.call(WorkerOp::QueryPathInfo, |c| c.str(path))?;
        if self.read_u64()? == 0 {
            return Ok(None);
        }
        // ValidPathInfo (1.37): deriver, narHash, references, registrationTime,
        // narSize, ultimate, sigs, ca.
        let _deriver = self.read_str()?;
        let nar_hash = self.read_str()?;
        let _refs = wire::read_string_list(&mut self.reader)?;
        let _registration_time = self.read_u64()?;
        let _nar_size = self.read_u64()?;
        let _ultimate = self.read_u64()?;
        let _sigs = wire::read_string_list(&mut self.reader)?;
        let _ca = self.read_str()?;
        Ok(Some(nar_hash))
    }

    /// `AddTextToStore` — add a text store object (a `.drv`, a `toFile`) and
    /// return the store path the daemon computed for it.
    ///
    /// `name` is the object's NAME (`foo.drv`), never its basename: the daemon
    /// computes `<hash>-<name>` itself. The daemon registers a temporary GC root
    /// for the path on this connection, as CppNix's `addToStoreFromDump` does.
    ///
    /// # Errors
    /// Any protocol failure or daemon refusal.
    pub fn add_text_to_store(
        &mut self,
        name: &str,
        text: &[u8],
        references: &[String],
    ) -> Result<String, WorkerError> {
        self.call(WorkerOp::AddTextToStore, |c| {
            c.str(name)?;
            wire::write_bytes(&mut c.writer, text)?;
            Ok(wire::write_string_list(&mut c.writer, references)?)
        })?;
        self.read_str()
    }

    /// `BuildPaths [<drv>!*]` in normal mode.
    ///
    /// # Errors
    /// Any protocol failure; a build failure is [`WorkerError::Daemon`].
    pub fn build_paths(&mut self, drv_paths: &[&str]) -> Result<(), WorkerError> {
        let with_all: Vec<String> = drv_paths.iter().map(|d| format!("{d}!*")).collect();
        self.call(WorkerOp::BuildPaths, |c| {
            wire::write_string_list(&mut c.writer, &with_all)?;
            c.u64(0) // bmNormal
        })?;
        let _ok = self.read_u64()?;
        Ok(())
    }

    /// `AddTempRoot` — keep `path` alive against GC for as long as this
    /// connection stays open (CppNix's temp-root semantics).
    ///
    /// # Errors
    /// Any protocol failure.
    pub fn add_temp_root(&mut self, path: &str) -> Result<(), WorkerError> {
        self.call(WorkerOp::AddTempRoot, |c| c.str(path))?;
        let _ack = self.read_u64()?;
        Ok(())
    }

    /// `AddIndirectRoot` — register `link` (an absolute, out-of-store symlink
    /// pointing into the store) as a permanent GC root, the way CppNix's
    /// `addPermRoot` does for `./result`.
    ///
    /// # Errors
    /// Any protocol failure.
    pub fn add_indirect_root(&mut self, link: &str) -> Result<(), WorkerError> {
        self.call(WorkerOp::AddIndirectRoot, |c| c.str(link))?;
        let _ack = self.read_u64()?;
        Ok(())
    }
}
