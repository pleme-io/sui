//! Per-connection handler — handshake + opcode dispatch loop.
//!
//! Each accepted Unix socket connection gets its own [`Connection`] which
//! performs the Nix worker protocol handshake and then enters the main
//! request/response loop.

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use sui_compat::store_path::StorePath;
use sui_compat::wire::{
    StderrMsg, WorkerOp, PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2,
};
use sui_store::traits::Store;

use crate::trust::TrustLevel;

/// Errors specific to connection handling.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad client magic: expected {expected:#x}, got {got:#x}")]
    BadMagic { expected: u64, got: u64 },
    #[error("unknown opcode: {0}")]
    UnknownOp(u64),
    #[error("store error: {0}")]
    Store(#[from] sui_store::traits::StoreError),
    #[error("protocol error: {0}")]
    Protocol(String),
}

// ── Async wire primitives ────────────────────────────────────────
//
// Mirrors `sui_compat::wire` but async via `tokio::io`.
// All integers are u64 LE. Strings are length-prefixed with 8-byte padding.

async fn write_u64(w: &mut (impl AsyncWrite + Unpin), v: u64) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes()).await
}

async fn read_u64(r: &mut (impl AsyncRead + Unpin)) -> std::io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).await?;
    Ok(u64::from_le_bytes(buf))
}

async fn write_bytes(w: &mut (impl AsyncWrite + Unpin), data: &[u8]) -> std::io::Result<()> {
    write_u64(w, data.len() as u64).await?;
    w.write_all(data).await?;
    let pad = (8 - (data.len() % 8)) % 8;
    if pad > 0 {
        w.write_all(&vec![0u8; pad]).await?;
    }
    Ok(())
}

async fn read_bytes(r: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
    let len = read_u64(r).await? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    let pad = (8 - (len % 8)) % 8;
    if pad > 0 {
        let mut pad_buf = vec![0u8; pad];
        r.read_exact(&mut pad_buf).await?;
    }
    Ok(buf)
}

async fn write_string(w: &mut (impl AsyncWrite + Unpin), s: &str) -> std::io::Result<()> {
    write_bytes(w, s.as_bytes()).await
}

async fn read_string(r: &mut (impl AsyncRead + Unpin)) -> Result<String, ConnectionError> {
    let bytes = read_bytes(r).await?;
    String::from_utf8(bytes).map_err(|e| ConnectionError::Protocol(format!("invalid UTF-8: {e}")))
}

async fn write_bool(w: &mut (impl AsyncWrite + Unpin), v: bool) -> std::io::Result<()> {
    write_u64(w, u64::from(v)).await
}

async fn write_string_list(
    w: &mut (impl AsyncWrite + Unpin),
    list: &[String],
) -> std::io::Result<()> {
    write_u64(w, list.len() as u64).await?;
    for s in list {
        write_string(w, s).await?;
    }
    Ok(())
}

// ── Stderr protocol helpers ──────────────────────────────────────

/// Write `STDERR_LAST` to signal the end of the stderr stream.
/// All successful responses are preceded by this marker.
async fn write_stderr_last(w: &mut (impl AsyncWrite + Unpin)) -> std::io::Result<()> {
    write_u64(w, StderrMsg::Last as u64).await
}

/// Write an error response via the stderr protocol.
async fn write_stderr_error(
    w: &mut (impl AsyncWrite + Unpin),
    msg: &str,
) -> std::io::Result<()> {
    write_u64(w, StderrMsg::Error as u64).await?;
    write_string(w, "Error").await?; // error type
    write_string(w, msg).await?; // error message
    write_u64(w, 0).await?; // error number / exit code
    Ok(())
}

// ── Connection ───────────────────────────────────────────────────

/// A single client connection to the daemon.
pub struct Connection<S, R, W> {
    store: Arc<S>,
    reader: R,
    writer: W,
    trust: TrustLevel,
    client_version: u64,
}

impl<S, R, W> Connection<S, R, W>
where
    S: Store,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Create a new connection (pre-handshake).
    pub fn new(store: Arc<S>, reader: R, writer: W, trust: TrustLevel) -> Self {
        Self {
            store,
            reader,
            writer,
            trust,
            client_version: 0,
        }
    }

    /// Perform the Nix worker protocol handshake.
    ///
    /// Sequence:
    /// 1. Read `WORKER_MAGIC_1` from client
    /// 2. Write `WORKER_MAGIC_2` to client
    /// 3. Write `PROTOCOL_VERSION` to client
    /// 4. Read client protocol version
    /// 5. Write `0_u64` for CPU affinity (obsolete)
    /// 6. Write `0_u64` for reserve space (obsolete)
    /// 7. Write the daemon version string
    /// 8. Exchange trust level
    pub async fn handshake(&mut self) -> Result<(), ConnectionError> {
        // 1. Read client magic
        let magic = read_u64(&mut self.reader).await?;
        if magic != WORKER_MAGIC_1 {
            return Err(ConnectionError::BadMagic {
                expected: WORKER_MAGIC_1,
                got: magic,
            });
        }

        // 2. Write server magic
        write_u64(&mut self.writer, WORKER_MAGIC_2).await?;

        // 3. Write server protocol version
        write_u64(&mut self.writer, PROTOCOL_VERSION).await?;
        self.writer.flush().await?;

        // 4. Read client protocol version
        self.client_version = read_u64(&mut self.reader).await?;

        // 5. Obsolete CPU affinity (must still send zero)
        if self.client_version >= (1 << 8 | 14) {
            let _cpu_affinity = read_u64(&mut self.reader).await?;
        }

        // 6. Obsolete reserve space (must still send zero)
        if self.client_version >= (1 << 8 | 11) {
            let _reserve = read_u64(&mut self.reader).await?;
        }

        // 7. Write daemon version string
        write_string(&mut self.writer, "sui-daemon 0.1.0").await?;

        // 8. Write trust level
        if self.client_version >= (1 << 8 | 35) {
            let trust_val: u64 = match self.trust {
                TrustLevel::Trusted => 1,
                TrustLevel::NotTrusted => 2,
            };
            write_u64(&mut self.writer, trust_val).await?;
        }

        self.writer.flush().await?;

        tracing::info!(
            client_version = self.client_version,
            trust = %self.trust,
            "handshake complete"
        );

        Ok(())
    }

    /// Run the main opcode dispatch loop.
    ///
    /// Reads opcodes from the client, dispatches to the appropriate handler,
    /// and writes responses. Returns when the connection is closed or an
    /// unrecoverable error occurs.
    pub async fn run(&mut self) -> Result<(), ConnectionError> {
        loop {
            let op_raw = match read_u64(&mut self.reader).await {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    tracing::debug!("client disconnected");
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };

            let op = WorkerOp::from_u64(op_raw);

            match op {
                Some(WorkerOp::IsValidPath) => self.handle_is_valid_path().await?,
                Some(WorkerOp::QueryPathInfo) => self.handle_query_path_info().await?,
                Some(WorkerOp::QueryAllValidPaths) => self.handle_query_all_valid_paths().await?,
                Some(WorkerOp::SetOptions) => self.handle_set_options().await?,
                Some(other) => {
                    tracing::warn!(?other, "unimplemented opcode");
                    write_stderr_error(
                        &mut self.writer,
                        &format!("operation {other:?} is not yet implemented"),
                    )
                    .await?;
                    write_stderr_last(&mut self.writer).await?;
                    self.writer.flush().await?;
                }
                None => {
                    tracing::warn!(op_raw, "unknown opcode");
                    write_stderr_error(
                        &mut self.writer,
                        &format!("unknown opcode {op_raw}"),
                    )
                    .await?;
                    write_stderr_last(&mut self.writer).await?;
                    self.writer.flush().await?;
                }
            }
        }
    }

    // ── Operation handlers ───────────────────────────────────────

    /// `IsValidPath` (op 1): Read a store path, return whether it exists.
    async fn handle_is_valid_path(&mut self) -> Result<(), ConnectionError> {
        let path_str = read_string(&mut self.reader).await?;
        tracing::debug!(path = %path_str, "IsValidPath");

        let valid = match StorePath::from_absolute_path(&path_str) {
            Ok(sp) => self.store.is_valid_path(&sp).await.unwrap_or(false),
            Err(_) => false,
        };

        write_stderr_last(&mut self.writer).await?;
        write_bool(&mut self.writer, valid).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// `QueryPathInfo` (op 26): Read a store path, return its `PathInfo`.
    ///
    /// Response format:
    /// - `STDERR_LAST`
    /// - valid (bool: 1 if found, 0 if not)
    /// If found:
    /// - deriver (string, empty if none)
    /// - nar_hash (string)
    /// - references (string list)
    /// - registration_time (u64)
    /// - nar_size (u64)
    /// - ultimate (bool, always false for now)
    /// - signatures (string list)
    /// - content_address (string, empty for now)
    async fn handle_query_path_info(&mut self) -> Result<(), ConnectionError> {
        let path_str = read_string(&mut self.reader).await?;
        tracing::debug!(path = %path_str, "QueryPathInfo");

        let info = match StorePath::from_absolute_path(&path_str) {
            Ok(sp) => self.store.query_path_info(&sp).await.unwrap_or(None),
            Err(_) => None,
        };

        write_stderr_last(&mut self.writer).await?;

        match info {
            Some(pi) => {
                // Path is valid
                write_bool(&mut self.writer, true).await?;
                // Deriver
                write_string(&mut self.writer, pi.deriver.as_deref().unwrap_or("")).await?;
                // NAR hash
                write_string(&mut self.writer, &pi.nar_hash).await?;
                // References
                write_string_list(&mut self.writer, &pi.references).await?;
                // Registration time
                write_u64(&mut self.writer, pi.registration_time as u64).await?;
                // NAR size
                write_u64(&mut self.writer, pi.nar_size as u64).await?;
                // Ultimate (whether this is an "ultimate" trusted path)
                write_bool(&mut self.writer, false).await?;
                // Signatures
                write_string_list(&mut self.writer, &pi.signatures).await?;
                // Content address (empty for now)
                write_string(&mut self.writer, "").await?;
            }
            None => {
                // Path not found
                write_bool(&mut self.writer, false).await?;
            }
        }

        self.writer.flush().await?;
        Ok(())
    }

    /// `QueryAllValidPaths` (op 23): Return all valid store paths.
    async fn handle_query_all_valid_paths(&mut self) -> Result<(), ConnectionError> {
        tracing::debug!("QueryAllValidPaths");

        let paths = self
            .store
            .query_all_valid_paths()
            .await
            .unwrap_or_default();

        let path_strings: Vec<String> = paths.iter().map(|p| p.to_absolute_path()).collect();

        write_stderr_last(&mut self.writer).await?;
        write_string_list(&mut self.writer, &path_strings).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// `SetOptions` (op 19): Read and discard client options.
    ///
    /// The real Nix daemon processes ~30 option fields. We read and discard
    /// them to keep the protocol flowing, then respond with success.
    async fn handle_set_options(&mut self) -> Result<(), ConnectionError> {
        tracing::debug!("SetOptions (consuming and discarding)");

        // keepFailed
        let _keep_failed = read_u64(&mut self.reader).await?;
        // keepGoing
        let _keep_going = read_u64(&mut self.reader).await?;
        // tryFallback
        let _try_fallback = read_u64(&mut self.reader).await?;
        // verbosity
        let _verbosity = read_u64(&mut self.reader).await?;
        // maxBuildJobs
        let _max_build_jobs = read_u64(&mut self.reader).await?;
        // maxSilentTime
        let _max_silent_time = read_u64(&mut self.reader).await?;

        // Obsolete useBuildHook field (removed in protocol >= 1.12 but
        // older clients still send it).
        if self.client_version < (1 << 8 | 12) {
            let _use_build_hook = read_u64(&mut self.reader).await?;
        }

        // verboseBuild
        let _verbose_build = read_u64(&mut self.reader).await?;
        // logType (obsolete)
        let _log_type = read_u64(&mut self.reader).await?;
        // printBuildTrace (obsolete)
        let _print_build_trace = read_u64(&mut self.reader).await?;
        // buildCores
        let _build_cores = read_u64(&mut self.reader).await?;
        // useSubstitutes
        let _use_substitutes = read_u64(&mut self.reader).await?;

        // overrides (map of string->string sent as flat list)
        if self.client_version >= (1 << 8 | 12) {
            let count = read_u64(&mut self.reader).await?;
            for _ in 0..count {
                let _name = read_string(&mut self.reader).await?;
                let _value = read_string(&mut self.reader).await?;
            }
        }

        write_stderr_last(&mut self.writer).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use sui_compat::wire;
    use sui_store::traits::{PathInfo, StoreResult};

    /// A mock store for testing.
    struct MockStore {
        paths: Vec<(String, PathInfo)>,
    }

    impl MockStore {
        fn new() -> Self {
            Self { paths: Vec::new() }
        }

        fn with_path(mut self, path: &str, hash: &str) -> Self {
            self.paths.push((
                path.to_string(),
                PathInfo {
                    path: path.to_string(),
                    nar_hash: hash.to_string(),
                    nar_size: 1024,
                    references: vec![],
                    deriver: None,
                    signatures: vec![],
                    registration_time: 0,
                    content_address: None,
                },
            ));
            self
        }
    }

    #[async_trait::async_trait]
    impl Store for MockStore {
        async fn query_path_info(
            &self,
            path: &StorePath,
        ) -> StoreResult<Option<PathInfo>> {
            let abs = path.to_absolute_path();
            Ok(self.paths.iter().find(|(p, _)| p == &abs).map(|(_, i)| i.clone()))
        }

        async fn is_valid_path(&self, path: &StorePath) -> StoreResult<bool> {
            let abs = path.to_absolute_path();
            Ok(self.paths.iter().any(|(p, _)| p == &abs))
        }

        async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
            Ok(self
                .paths
                .iter()
                .filter_map(|(p, _)| StorePath::from_absolute_path(p).ok())
                .collect())
        }
    }

    /// Build a full client handshake with version exchange.
    fn build_full_client_handshake(client_version: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        // Client sends magic
        wire::write_u64(&mut buf, WORKER_MAGIC_1).unwrap();
        // Client sends its protocol version (server reads this after writing its own)
        wire::write_u64(&mut buf, client_version).unwrap();
        // CPU affinity (for version >= 1.14)
        if client_version >= (1 << 8 | 14) {
            wire::write_u64(&mut buf, 0).unwrap();
        }
        // Reserve space (for version >= 1.11)
        if client_version >= (1 << 8 | 11) {
            wire::write_u64(&mut buf, 0).unwrap();
        }
        buf
    }

    #[tokio::test]
    async fn handshake_bad_magic() {
        let store = Arc::new(MockStore::new());
        let mut input = Vec::new();
        wire::write_u64(&mut input, 0xDEADBEEF).unwrap();

        let reader = Cursor::new(input);
        let writer = Vec::new();

        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        let result = conn.handshake().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ConnectionError::BadMagic { expected, got } => {
                assert_eq!(expected, WORKER_MAGIC_1);
                assert_eq!(got, 0xDEADBEEF);
            }
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_success() {
        let store = Arc::new(MockStore::new());
        let client_version = PROTOCOL_VERSION;
        let input = build_full_client_handshake(client_version);

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();

        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.handshake().await.expect("handshake should succeed");
        assert_eq!(conn.client_version, client_version);

        // Verify server wrote the expected response
        let out = &conn.writer;
        let mut cursor = Cursor::new(out.as_slice());
        let magic2 = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(magic2, WORKER_MAGIC_2);
        let version = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(version, PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn is_valid_path_found() {
        let test_path = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-hello-2.12";
        let store = Arc::new(MockStore::new().with_path(test_path, "sha256:abc123"));

        // Build the opcode + path request
        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::IsValidPath as u64).unwrap();
        wire::write_string(&mut input, test_path).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.expect("run should succeed");

        // Parse the response
        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr_last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr_last, StderrMsg::Last as u64);
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(valid);
    }

    #[tokio::test]
    async fn is_valid_path_not_found() {
        let store = Arc::new(MockStore::new());

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::IsValidPath as u64).unwrap();
        wire::write_string(&mut input, "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-missing").unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.expect("run should succeed");

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr_last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr_last, StderrMsg::Last as u64);
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(!valid);
    }

    #[tokio::test]
    async fn query_path_info_found() {
        let test_path = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-hello-2.12";
        let store = Arc::new(MockStore::new().with_path(test_path, "sha256:abc123"));

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryPathInfo as u64).unwrap();
        wire::write_string(&mut input, test_path).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.expect("run should succeed");

        let mut cursor = Cursor::new(conn.writer.as_slice());
        // STDERR_LAST
        let stderr_last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr_last, StderrMsg::Last as u64);
        // valid = true
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(valid);
        // deriver (empty)
        let deriver = wire::read_string(&mut cursor).unwrap();
        assert_eq!(deriver, "");
        // nar_hash
        let nar_hash = wire::read_string(&mut cursor).unwrap();
        assert_eq!(nar_hash, "sha256:abc123");
        // references (empty list)
        let refs = wire::read_string_list(&mut cursor).unwrap();
        assert!(refs.is_empty());
        // registration_time
        let reg_time = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(reg_time, 0);
        // nar_size
        let nar_size = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(nar_size, 1024);
    }

    #[tokio::test]
    async fn query_path_info_not_found() {
        let store = Arc::new(MockStore::new());

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryPathInfo as u64).unwrap();
        wire::write_string(&mut input, "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-missing").unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.expect("run should succeed");

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr_last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr_last, StderrMsg::Last as u64);
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(!valid);
    }

    #[tokio::test]
    async fn query_all_valid_paths() {
        let path1 = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-hello-2.12";
        let path2 = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-world-1.0";
        let store = Arc::new(
            MockStore::new()
                .with_path(path1, "sha256:abc")
                .with_path(path2, "sha256:def"),
        );

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryAllValidPaths as u64).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.expect("run should succeed");

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr_last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr_last, StderrMsg::Last as u64);
        let paths = wire::read_string_list(&mut cursor).unwrap();
        assert_eq!(paths.len(), 2);
    }

    #[tokio::test]
    async fn unknown_opcode_returns_error() {
        let store = Arc::new(MockStore::new());

        let mut input = Vec::new();
        // Send unknown opcode 9999
        wire::write_u64(&mut input, 9999).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.expect("run should succeed (error is sent to client)");

        let mut cursor = Cursor::new(conn.writer.as_slice());
        // Should get STDERR_ERROR followed by error info, then STDERR_LAST
        let msg_type = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(msg_type, StderrMsg::Error as u64);
    }

    // ── Handshake variant tests ────────────────────────────────

    #[tokio::test]
    async fn handshake_old_client_no_cpu_affinity() {
        // Protocol 1.10: no CPU affinity, no reserve space
        let store = Arc::new(MockStore::new());
        let client_version: u64 = 1 << 8 | 10;
        let input = build_full_client_handshake(client_version);

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.handshake().await.expect("old client handshake should succeed");
        assert_eq!(conn.client_version, client_version);
    }

    #[tokio::test]
    async fn handshake_old_client_with_reserve_no_affinity() {
        // Protocol 1.11: has reserve space but no CPU affinity
        let store = Arc::new(MockStore::new());
        let client_version: u64 = 1 << 8 | 11;
        let input = build_full_client_handshake(client_version);

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.handshake().await.expect("1.11 handshake should succeed");
        assert_eq!(conn.client_version, client_version);
    }

    #[tokio::test]
    async fn handshake_old_client_no_trust_exchange() {
        // Protocol 1.34: no trust level exchange
        let store = Arc::new(MockStore::new());
        let client_version: u64 = 1 << 8 | 34;
        let input = build_full_client_handshake(client_version);

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.handshake().await.expect("1.34 handshake should succeed");

        // Verify response: magic2 + version + daemon string, but NO trust field
        let out = &conn.writer;
        let mut cursor = Cursor::new(out.as_slice());
        let magic2 = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(magic2, WORKER_MAGIC_2);
        let version = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(version, PROTOCOL_VERSION);
        let daemon_str = wire::read_string(&mut cursor).unwrap();
        assert!(daemon_str.starts_with("sui-daemon"));
        // No more bytes (no trust level for < 1.35)
        assert_eq!(cursor.position() as usize, out.len());
    }

    #[tokio::test]
    async fn handshake_not_trusted_client() {
        let store = Arc::new(MockStore::new());
        let client_version = PROTOCOL_VERSION;
        let input = build_full_client_handshake(client_version);

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::NotTrusted);
        conn.handshake().await.expect("untrusted handshake should succeed");

        // Verify trust level = 2 (NotTrusted)
        let out = &conn.writer;
        let mut cursor = Cursor::new(out.as_slice());
        let _magic2 = wire::read_u64(&mut cursor).unwrap();
        let _version = wire::read_u64(&mut cursor).unwrap();
        let _daemon_str = wire::read_string(&mut cursor).unwrap();
        let trust = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(trust, 2, "NotTrusted should be encoded as 2");
    }

    #[tokio::test]
    async fn handshake_trusted_client() {
        let store = Arc::new(MockStore::new());
        let client_version = PROTOCOL_VERSION;
        let input = build_full_client_handshake(client_version);

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.handshake().await.expect("trusted handshake should succeed");

        let out = &conn.writer;
        let mut cursor = Cursor::new(out.as_slice());
        let _magic2 = wire::read_u64(&mut cursor).unwrap();
        let _version = wire::read_u64(&mut cursor).unwrap();
        let _daemon_str = wire::read_string(&mut cursor).unwrap();
        let trust = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(trust, 1, "Trusted should be encoded as 1");
    }

    // ── Async wire primitive round-trip tests ──────────────────

    #[tokio::test]
    async fn async_write_read_u64_round_trip() {
        for val in [0u64, 1, 42, u64::MAX, WORKER_MAGIC_1, WORKER_MAGIC_2, PROTOCOL_VERSION] {
            let mut buf = Vec::new();
            write_u64(&mut buf, val).await.unwrap();
            assert_eq!(buf.len(), 8);
            let mut cursor = Cursor::new(buf);
            let read_val = read_u64(&mut cursor).await.unwrap();
            assert_eq!(read_val, val);
        }
    }

    #[tokio::test]
    async fn async_write_read_bytes_round_trip() {
        let cases: Vec<&[u8]> = vec![b"", b"a", b"hello", b"12345678", b"\x00\xff\x00"];
        for data in cases {
            let mut buf = Vec::new();
            write_bytes(&mut buf, data).await.unwrap();
            assert_eq!(buf.len() % 8, 0, "output must be 8-byte aligned");
            let mut cursor = Cursor::new(buf);
            let result = read_bytes(&mut cursor).await.unwrap();
            assert_eq!(result, data);
        }
    }

    #[tokio::test]
    async fn async_write_read_string_round_trip() {
        let cases = ["", "hello", "nix/store/path", "utf-8: 日本語"];
        for s in cases {
            let mut buf = Vec::new();
            write_string(&mut buf, s).await.unwrap();
            let mut cursor = Cursor::new(buf);
            let result = read_string(&mut cursor).await.unwrap();
            assert_eq!(result, s);
        }
    }

    #[tokio::test]
    async fn async_write_read_bool_round_trip() {
        for val in [true, false] {
            let mut buf = Vec::new();
            write_bool(&mut buf, val).await.unwrap();
            let mut cursor = Cursor::new(buf);
            let read_val = read_u64(&mut cursor).await.unwrap();
            assert_eq!(read_val != 0, val);
        }
    }

    #[tokio::test]
    async fn async_write_string_list_round_trip() {
        let list = vec!["foo".to_string(), "bar".to_string(), "baz".to_string()];
        let mut buf = Vec::new();
        write_string_list(&mut buf, &list).await.unwrap();

        let mut cursor = Cursor::new(buf.as_slice());
        let count = wire::read_u64(&mut cursor).unwrap() as usize;
        assert_eq!(count, 3);
        for expected in &list {
            let s = wire::read_string(&mut cursor).unwrap();
            assert_eq!(&s, expected);
        }
    }

    #[tokio::test]
    async fn async_write_string_list_empty() {
        let list: Vec<String> = vec![];
        let mut buf = Vec::new();
        write_string_list(&mut buf, &list).await.unwrap();
        assert_eq!(buf.len(), 8); // just the count
        let mut cursor = Cursor::new(buf.as_slice());
        let count = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(count, 0);
    }

    // ── Stderr protocol tests ───────────────────────────────────

    #[tokio::test]
    async fn stderr_last_writes_correct_marker() {
        let mut buf = Vec::new();
        write_stderr_last(&mut buf).await.unwrap();
        let mut cursor = Cursor::new(buf.as_slice());
        let val = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(val, StderrMsg::Last as u64);
    }

    #[tokio::test]
    async fn stderr_error_writes_correct_frame() {
        let mut buf = Vec::new();
        write_stderr_error(&mut buf, "test error message").await.unwrap();

        let mut cursor = Cursor::new(buf.as_slice());
        let msg_type = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(msg_type, StderrMsg::Error as u64);
        let error_type = wire::read_string(&mut cursor).unwrap();
        assert_eq!(error_type, "Error");
        let error_msg = wire::read_string(&mut cursor).unwrap();
        assert_eq!(error_msg, "test error message");
        let error_num = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(error_num, 0);
    }

    // ── Full data flow: IsValidPath true → response bytes contain true ──

    #[tokio::test]
    async fn is_valid_path_full_flow_store_returns_true() {
        let test_path = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1";
        let store = Arc::new(MockStore::new().with_path(test_path, "sha256:deadbeef"));

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::IsValidPath as u64).unwrap();
        wire::write_string(&mut input, test_path).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr_last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr_last, StderrMsg::Last as u64);
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(valid, "store contains the path, should return true");
    }

    // ── Full data flow: IsValidPath false → response bytes contain false ──

    #[tokio::test]
    async fn is_valid_path_full_flow_store_returns_false() {
        let store = Arc::new(MockStore::new()); // empty store

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::IsValidPath as u64).unwrap();
        wire::write_string(
            &mut input,
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr_last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr_last, StderrMsg::Last as u64);
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(!valid, "store is empty, should return false");
    }

    // ── Full data flow: QueryPathInfo with all fields ──────────

    /// A richer mock store that populates all PathInfo fields.
    struct RichMockStore {
        info: Option<PathInfo>,
    }

    impl RichMockStore {
        fn with_info(info: PathInfo) -> Self {
            Self { info: Some(info) }
        }
    }

    #[async_trait::async_trait]
    impl Store for RichMockStore {
        async fn query_path_info(
            &self,
            _path: &StorePath,
        ) -> StoreResult<Option<PathInfo>> {
            Ok(self.info.clone())
        }

        async fn is_valid_path(&self, _path: &StorePath) -> StoreResult<bool> {
            Ok(self.info.is_some())
        }

        async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn query_path_info_full_flow_all_fields() {
        let test_path = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1";
        let info = PathInfo {
            path: test_path.to_string(),
            nar_hash: "sha256:deadbeefcafe".to_string(),
            nar_size: 226552,
            references: vec![
                "/nix/store/3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37".to_string(),
            ],
            deriver: Some("xb4y5iklhya4blk42k1cfkb8k07dpp4n-hello-2.12.1.drv".to_string()),
            signatures: vec![
                "cache.nixos.org-1:sig123==".to_string(),
                "my-key:sig456==".to_string(),
            ],
            registration_time: 1700000000,
            content_address: None,
        };
        let store = Arc::new(RichMockStore::with_info(info));

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryPathInfo as u64).unwrap();
        wire::write_string(&mut input, test_path).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        // STDERR_LAST
        let stderr_last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr_last, StderrMsg::Last as u64);
        // valid = true
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(valid);
        // deriver
        let deriver = wire::read_string(&mut cursor).unwrap();
        assert_eq!(deriver, "xb4y5iklhya4blk42k1cfkb8k07dpp4n-hello-2.12.1.drv");
        // nar_hash
        let nar_hash = wire::read_string(&mut cursor).unwrap();
        assert_eq!(nar_hash, "sha256:deadbeefcafe");
        // references
        let refs = wire::read_string_list(&mut cursor).unwrap();
        assert_eq!(refs.len(), 1);
        assert!(refs[0].contains("glibc-2.37"));
        // registration_time
        let reg_time = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(reg_time, 1700000000);
        // nar_size
        let nar_size = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(nar_size, 226552);
        // ultimate (bool)
        let ultimate = wire::read_bool(&mut cursor).unwrap();
        assert!(!ultimate);
        // signatures
        let sigs = wire::read_string_list(&mut cursor).unwrap();
        assert_eq!(sigs.len(), 2);
        assert_eq!(sigs[0], "cache.nixos.org-1:sig123==");
        assert_eq!(sigs[1], "my-key:sig456==");
    }

    #[tokio::test]
    async fn query_path_info_full_flow_missing_returns_false() {
        let store = Arc::new(MockStore::new()); // empty

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryPathInfo as u64).unwrap();
        wire::write_string(
            &mut input,
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr_last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr_last, StderrMsg::Last as u64);
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(!valid, "path not in store, should be false");
    }
}
