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

use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sui_compat::activity::{self, SharedSink};
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

pub type Transport = (Box<dyn Read + Send>, Box<dyn Write + Send>);

/// A connected, handshaked worker-protocol client.
pub struct WorkerConn {
    reader: BufReader<Box<dyn Read + Send>>,
    writer: BufWriter<Box<dyn Write + Send>>,
    sink: SharedSink,
    minor: u64,
    trusted: Option<bool>,
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
        let mut conn = Self::unopened(stream)?;
        conn.handshake()?;
        conn.set_options(ClientOptions::from_nix_config()?)?;
        Ok(conn)
    }

    /// Handshake over an already-connected `stream` and send `options`.
    ///
    /// # Errors
    /// Any handshake failure.
    pub fn over(stream: UnixStream, options: ClientOptions) -> Result<Self, WorkerError> {
        let mut conn = Self::unopened(stream)?;
        conn.handshake()?;
        conn.set_options(options)?;
        Ok(conn)
    }

    fn unopened(stream: UnixStream) -> Result<Self, WorkerError> {
        let reader: Box<dyn Read + Send> = Box::new(stream.try_clone()?);
        let writer: Box<dyn Write + Send> = Box::new(stream);
        Ok(Self::from_transport((reader, writer)))
    }

    fn from_transport((reader, writer): Transport) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            sink: activity::noop_sink(),
            minor: PROTOCOL_VERSION & 0xff,
            trusted: None,
        }
    }

    pub fn over_transport(transport: Transport, options: ClientOptions) -> Result<Self, WorkerError> {
        let mut conn = Self::from_transport(transport);
        conn.handshake()?;
        conn.set_options(options)?;
        Ok(conn)
    }

    #[must_use]
    pub fn minor(&self) -> u64 {
        self.minor
    }

    #[must_use]
    pub fn trusted(&self) -> Option<bool> {
        self.trusted
    }

    /// Deliver the daemon's log and activity frames to `sink` instead of
    /// dropping them.
    #[must_use]
    pub fn with_sink(mut self, sink: SharedSink) -> Self {
        self.sink = sink;
        self
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
        let server_version = self.read_u64()?;
        if server_version >> 8 != 1 || server_version & 0xff < 21 {
            return Err(WorkerError::Protocol(format!("unsupported daemon protocol {}.{}", server_version >> 8, server_version & 0xff)));
        }
        self.minor = (server_version & 0xff).min(PROTOCOL_VERSION & 0xff);
        self.u64(PROTOCOL_VERSION)?;
        self.u64(0)?;
        self.u64(0)?;
        self.writer.flush()?;
        if self.minor >= 33 {
            let _daemon_version = self.read_str()?;
        }
        if self.minor >= 35 {
            self.trusted = match self.read_u64()? {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            };
        }
        self.drain_stderr()
    }

    /// `SetOptions`, carrying what CppNix's client sends ([`ClientOptions`]).
    /// The daemon applies these to the connection, so sending `maxBuildJobs = 0`
    /// and `useBuildHook = 0` (as sui once did) forbids every local and remote
    /// build: only substitutable paths could be realized.
    fn set_options(&mut self, opts: ClientOptions) -> Result<(), WorkerError> {
        self.call(WorkerOp::SetOptions, |c| {
            for field in opts.fields() {
                c.u64(field)?;
            }
            c.u64(0) // overrides count
        })
    }

    fn drain_stderr(&mut self) -> Result<(), WorkerError> {
        drain_stderr(&mut self.reader, &self.sink, self.minor)
    }

    fn call_streaming(
        &mut self,
        op: WorkerOp,
        args: impl FnOnce(&mut BufWriter<Box<dyn Write + Send>>) -> Result<(), WorkerError> + Send,
    ) -> Result<(), WorkerError> {
        self.u64(op as u64)?;
        let minor = self.minor;
        let Self { reader, writer, sink, .. } = self;
        std::thread::scope(|s| {
            let drained = s.spawn(move || drain_stderr(reader, sink, minor));
            let wrote = args(writer).and_then(|()| Ok(writer.flush()?));
            let drained = drained.join().map_err(|_| WorkerError::Protocol("stderr reader panicked".into()))?;
            drained?;
            wrote
        })
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

    /// `AddToStore` of a NAR as a recursive sha256 content-addressed path
    /// with no references, the way CppNix copies a source path into the
    /// store. Returns the store path the daemon computed.
    ///
    /// # Errors
    /// Any protocol failure or daemon refusal.
    pub fn add_nar_to_store(&mut self, name: &str, nar: &[u8]) -> Result<String, WorkerError> {
        self.call(WorkerOp::AddToStore, |c| {
            c.str(name)?;
            c.str("fixed:r:sha256")?;
            wire::write_string_list(&mut c.writer, &[] as &[String])?;
            c.u64(0)?;
            for chunk in nar.chunks(1 << 20) {
                c.u64(chunk.len() as u64)?;
                c.writer.write_all(chunk)?;
            }
            c.u64(0)
        })?;
        let path = self.read_str()?;
        let _deriver = self.read_str()?;
        let _nar_hash = self.read_str()?;
        let _refs = wire::read_string_list(&mut self.reader)?;
        let _registration_time = self.read_u64()?;
        let _nar_size = self.read_u64()?;
        let _ultimate = self.read_u64()?;
        let _sigs = wire::read_string_list(&mut self.reader)?;
        let _ca = self.read_str()?;
        Ok(path)
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

    /// `BuildPaths` over derived paths exactly as given: a store path is
    /// ensured (substituted if missing), `<drv>!<outputs>` is built.
    pub fn build_derived(&mut self, derived: &[String]) -> Result<(), WorkerError> {
        self.call(WorkerOp::BuildPaths, |c| {
            wire::write_string_list(&mut c.writer, derived)?;
            c.u64(0)
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

    pub fn query_valid_paths(&mut self, paths: &[String], substitute: bool) -> Result<Vec<String>, WorkerError> {
        let minor = self.minor;
        self.call(WorkerOp::QueryValidPaths, |c| {
            wire::write_string_list(&mut c.writer, paths)?;
            if minor >= 27 {
                c.u64(u64::from(substitute))?;
            }
            Ok(())
        })?;
        Ok(wire::read_string_list(&mut self.reader)?)
    }

    pub fn query_path_info(&mut self, path: &str) -> Result<Option<RemotePathInfo>, WorkerError> {
        self.call(WorkerOp::QueryPathInfo, |c| c.str(path))?;
        if self.read_u64()? == 0 {
            return Ok(None);
        }
        Ok(Some(RemotePathInfo {
            path: path.to_string(),
            deriver: self.read_str()?,
            nar_hash: self.read_str()?,
            references: wire::read_string_list(&mut self.reader)?,
            registration_time: self.read_u64()?,
            nar_size: self.read_u64()?,
            ultimate: self.read_u64()? != 0,
            sigs: wire::read_string_list(&mut self.reader)?,
            ca: self.read_str()?,
        }))
    }

    pub fn add_to_store_nar(&mut self, info: &RemotePathInfo, nar: &[u8], dont_check_sigs: bool) -> Result<(), WorkerError> {
        if self.minor < 23 {
            return Err(WorkerError::Protocol(format!("AddToStoreNar needs protocol 1.23, the daemon speaks 1.{}", self.minor)));
        }
        self.call_streaming(WorkerOp::AddToStoreNar, |w| {
            write_path_info(w, info)?;
            wire::write_u64(w, 0)?;
            wire::write_u64(w, u64::from(dont_check_sigs))?;
            let mut frames = FrameWriter { inner: w, buf: Vec::with_capacity(FRAME) };
            frames.write_all(nar)?;
            frames.finish()
        })
    }

    pub fn add_multiple_to_store<I>(&mut self, count: usize, mut items: I, dont_check_sigs: bool) -> Result<(), WorkerError>
    where
        I: FnMut(usize) -> Result<(RemotePathInfo, Vec<u8>), WorkerError> + Send,
    {
        if self.minor < 32 {
            for i in 0..count {
                let (info, nar) = items(i)?;
                self.add_to_store_nar(&info, &nar, dont_check_sigs)?;
            }
            return Ok(());
        }
        self.call_streaming(WorkerOp::AddMultipleToStore, |w| {
            wire::write_u64(w, 0)?;
            wire::write_u64(w, u64::from(dont_check_sigs))?;
            let mut frames = FrameWriter { inner: w, buf: Vec::with_capacity(FRAME) };
            wire::write_u64(&mut frames, count as u64)?;
            for i in 0..count {
                let (info, nar) = items(i)?;
                write_path_info(&mut frames, &info)?;
                frames.write_all(&nar)?;
            }
            frames.finish()
        })
    }

    pub fn nar_from_path(&mut self, path: &str) -> Result<Vec<u8>, WorkerError> {
        self.call(WorkerOp::NarFromPath, |c| c.str(path))?;
        read_nar_bytes(&mut self.reader, path)
    }

    pub fn build_derivation(
        &mut self,
        drv_path: &str,
        drv: &sui_compat::derivation::Derivation,
        input_srcs: &[String],
    ) -> Result<RemoteBuildResult, WorkerError> {
        let minor = self.minor;
        self.call(WorkerOp::BuildDerivation, |c| {
            c.str(drv_path)?;
            write_basic_derivation(&mut c.writer, drv, input_srcs)?;
            c.u64(0)
        })?;
        let status = self.read_u64()?;
        let error_msg = self.read_str()?;
        if minor >= 29 {
            for _ in 0..4 {
                self.read_u64()?;
            }
        }
        if minor >= 37 {
            for _ in 0..2 {
                if self.read_u64()? != 0 {
                    self.read_u64()?;
                }
            }
        }
        if minor >= 28 {
            let n = self.read_u64()?;
            for _ in 0..n {
                self.read_str()?;
                self.read_str()?;
            }
        }
        Ok(RemoteBuildResult { status, error_msg })
    }
}

fn drain_stderr(reader: &mut impl Read, sink: &SharedSink, minor: u64) -> Result<(), WorkerError> {
    loop {
        let msg = wire::read_u64(reader)?;
        match msg {
            m if m == StderrMsg::Last as u64 => return Ok(()),
            m if m == StderrMsg::Error as u64 => return Err(WorkerError::Daemon(read_error_frame(reader, minor)?)),
            other => match activity::read_frame(reader, other)? {
                Some(event) => sink.emit(event),
                None => return Err(WorkerError::Protocol(format!("unexpected stderr frame {other:#x}"))),
            },
        }
    }
}

fn read_error_frame(reader: &mut impl Read, minor: u64) -> Result<String, WorkerError> {
    if minor < 26 {
        let msg = wire::read_string(reader)?;
        let _status = wire::read_u64(reader)?;
        return Ok(msg);
    }
    let _type = wire::read_string(reader)?;
    let _level = wire::read_u64(reader)?;
    let _name = wire::read_string(reader)?;
    let msg = wire::read_string(reader)?;
    let _have_pos = wire::read_u64(reader)?;
    let ntraces = wire::read_u64(reader)?;
    for _ in 0..ntraces {
        let _have_pos = wire::read_u64(reader)?;
        let _hint = wire::read_string(reader)?;
    }
    Ok(msg)
}

pub(crate) fn write_path_info(w: &mut impl Write, info: &RemotePathInfo) -> Result<(), WorkerError> {
    let hash = info.nar_hash.split_once(':').map_or(info.nar_hash.as_str(), |(_, h)| h);
    wire::write_string(w, &info.path)?;
    wire::write_string(w, &info.deriver)?;
    wire::write_string(w, hash)?;
    wire::write_string_list(w, &info.references)?;
    wire::write_u64(w, info.registration_time)?;
    wire::write_u64(w, info.nar_size)?;
    wire::write_u64(w, u64::from(info.ultimate))?;
    wire::write_string_list(w, &info.sigs)?;
    wire::write_string(w, &info.ca)?;
    Ok(())
}

const FRAME: usize = 1 << 20;

struct FrameWriter<'a, W: Write> {
    inner: &'a mut W,
    buf: Vec<u8>,
}

impl<W: Write> FrameWriter<'_, W> {
    fn emit(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            self.inner.write_all(&(self.buf.len() as u64).to_le_bytes())?;
            self.inner.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), WorkerError> {
        self.emit()?;
        self.inner.write_all(&0u64.to_le_bytes())?;
        Ok(())
    }
}

impl<W: Write> Write for FrameWriter<'_, W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= FRAME {
            self.emit()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn read_nar_bytes(reader: &mut impl Read, path: &str) -> Result<Vec<u8>, WorkerError> {
    let mut tee = Tee { inner: reader, seen: Vec::new() };
    sui_compat::nar::NarReader::read_complete(&mut tee).map_err(|e| WorkerError::Protocol(format!("NAR of {path}: {e}")))?;
    Ok(tee.seen)
}

pub(crate) fn write_basic_derivation(
    w: &mut impl Write,
    drv: &sui_compat::derivation::Derivation,
    input_srcs: &[String],
) -> Result<(), WorkerError> {
    wire::write_u64(w, drv.outputs.len() as u64)?;
    for (name, out) in &drv.outputs {
        wire::write_string(w, name)?;
        wire::write_string(w, &out.path)?;
        wire::write_string(w, &out.hash_algo)?;
        wire::write_string(w, &out.hash)?;
    }
    wire::write_string_list(w, input_srcs)?;
    wire::write_string(w, &drv.system)?;
    wire::write_string(w, &drv.builder)?;
    wire::write_string_list(w, &drv.args)?;
    wire::write_u64(w, drv.env.len() as u64)?;
    for (k, v) in &drv.env {
        wire::write_string(w, k)?;
        wire::write_string(w, v)?;
    }
    Ok(())
}

struct Tee<'a, R: Read> {
    inner: &'a mut R,
    seen: Vec<u8>,
}

impl<R: Read> Read for Tee<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.seen.extend_from_slice(&buf[..n]);
        Ok(n)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePathInfo {
    pub path: String,
    pub deriver: String,
    pub nar_hash: String,
    pub references: Vec<String>,
    pub registration_time: u64,
    pub nar_size: u64,
    pub ultimate: bool,
    pub sigs: Vec<String>,
    pub ca: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBuildResult {
    pub status: u64,
    pub error_msg: String,
}

impl RemoteBuildResult {
    #[must_use]
    pub fn success(&self) -> bool {
        matches!(self.status, 0 | 1 | 2 | 13)
    }
}
