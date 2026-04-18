//! Per-connection handler — handshake + opcode dispatch loop.
//!
//! Each accepted Unix socket connection gets its own [`Connection`] which
//! performs the Nix worker protocol handshake and then enters the main
//! request/response loop.
//!
//! # Module layout
//!
//! - [`wire`] — async read/write primitives for the Nix wire format
//! - [`handshake`] — magic / version / trust negotiation
//! - [`dispatch`] — opcode dispatch loop and per-operation handlers

mod dispatch;
mod handshake;
pub(crate) mod wire;

use std::sync::Arc;

use sui_store::traits::Store;

use crate::trust::TrustLevel;

// ── Protocol version thresholds ─────────────────────────────────
//
// Nix encodes protocol versions as `(major << 8) | minor`.
// These constants document the minor-version thresholds at which
// optional handshake / option fields were added or removed.
//
// TODO(scope): these belong in `sui_compat::wire` alongside `PROTOCOL_VERSION`.

/// Minimum client version that sends the (obsolete) CPU-affinity field.
const PROTOCOL_MINOR_CPU_AFFINITY: u64 = (1 << 8) | 14;
/// Minimum client version that sends the (obsolete) reserve-space field.
const PROTOCOL_MINOR_RESERVE_SPACE: u64 = (1 << 8) | 11;
/// Minimum client version that exchanges the trust-level field.
const PROTOCOL_MINOR_TRUST_EXCHANGE: u64 = (1 << 8) | 35;
/// Protocol version below which the (obsolete) `useBuildHook` field is sent
/// in `SetOptions`, and above/equal to which string-pair overrides are sent.
const PROTOCOL_MINOR_OVERRIDES: u64 = (1 << 8) | 12;

/// Errors specific to connection handling.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectionError {
    /// An I/O error occurred on the underlying transport.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The client sent an incorrect worker protocol magic value.
    #[error("bad client magic: expected {expected:#x}, got {got:#x}")]
    BadMagic {
        /// The expected magic value (`WORKER_MAGIC_1`).
        expected: u64,
        /// The value the client actually sent.
        got: u64,
    },
    /// An opcode with no known `WorkerOp` mapping was received.
    #[error("unknown opcode: {0}")]
    UnknownOp(u64),
    /// A store operation failed.
    #[error("store error: {0}")]
    Store(#[from] sui_store::traits::StoreError),
    /// A generic protocol-level error (e.g. invalid UTF-8 in a string frame).
    #[error("protocol error: {0}")]
    Protocol(String),
}

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
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    /// Create a new connection (pre-handshake).
    #[must_use]
    pub fn new(store: Arc<S>, reader: R, writer: W, trust: TrustLevel) -> Self {
        Self {
            store,
            reader,
            writer,
            trust,
            client_version: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::wire::{
        read_bytes, read_string, read_u64, write_bool, write_bytes, write_stderr_error,
        write_stderr_last, write_string, write_string_list, write_u64,
    };
    use super::*;
    use std::io::Cursor;
    use sui_compat::store_path::StorePath;
    use sui_compat::wire::{self, StderrMsg, WorkerOp, PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2};
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

    // ── SetOptions handler tests ──────────────────────────────

    /// Build a SetOptions payload for a given client version.
    fn build_set_options_payload(client_version: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, WorkerOp::SetOptions as u64).unwrap();
        // keepFailed, keepGoing, tryFallback, verbosity, maxBuildJobs, maxSilentTime
        for _ in 0..6 {
            wire::write_u64(&mut buf, 0).unwrap();
        }
        // useBuildHook (only sent for client < 1.12)
        if client_version < (1 << 8 | 12) {
            wire::write_u64(&mut buf, 1).unwrap();
        }
        // verboseBuild, logType, printBuildTrace, buildCores, useSubstitutes
        for _ in 0..5 {
            wire::write_u64(&mut buf, 0).unwrap();
        }
        // overrides (for client >= 1.12)
        if client_version >= (1 << 8 | 12) {
            wire::write_u64(&mut buf, 0).unwrap(); // count=0
        }
        buf
    }

    #[tokio::test]
    async fn set_options_modern_client() {
        let store = Arc::new(MockStore::new());
        let client_version = PROTOCOL_VERSION; // 1.37

        let input = build_set_options_payload(client_version);
        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = client_version;

        conn.run().await.expect("SetOptions should succeed");

        // Response should be just STDERR_LAST
        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr_last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr_last, StderrMsg::Last as u64);
    }

    #[tokio::test]
    async fn set_options_old_client_with_build_hook() {
        let store = Arc::new(MockStore::new());
        let client_version: u64 = 1 << 8 | 11; // pre-1.12: sends useBuildHook

        let input = build_set_options_payload(client_version);
        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = client_version;

        conn.run().await.expect("SetOptions with old client should succeed");

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr_last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr_last, StderrMsg::Last as u64);
    }

    #[tokio::test]
    async fn set_options_with_overrides() {
        let store = Arc::new(MockStore::new());
        let client_version = PROTOCOL_VERSION;

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::SetOptions as u64).unwrap();
        // 6 fixed fields
        for _ in 0..6 {
            wire::write_u64(&mut input, 0).unwrap();
        }
        // 5 more fields
        for _ in 0..5 {
            wire::write_u64(&mut input, 0).unwrap();
        }
        // overrides: 2 key-value pairs
        wire::write_u64(&mut input, 2).unwrap();
        wire::write_string(&mut input, "cores").unwrap();
        wire::write_string(&mut input, "4").unwrap();
        wire::write_string(&mut input, "max-jobs").unwrap();
        wire::write_string(&mut input, "8").unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = client_version;

        conn.run().await.expect("SetOptions with overrides should succeed");

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr_last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr_last, StderrMsg::Last as u64);
    }

    // ── Unimplemented opcode tests ──────────────────────────────

    #[tokio::test]
    async fn unimplemented_opcode_returns_stderr_error_then_last() {
        let store = Arc::new(MockStore::new());

        // Send a known but unimplemented opcode (AddToStore = 7)
        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::AddToStore as u64).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.expect("should handle gracefully");

        let mut cursor = Cursor::new(conn.writer.as_slice());
        // STDERR_ERROR
        let msg_type = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(msg_type, StderrMsg::Error as u64);
        let error_type = wire::read_string(&mut cursor).unwrap();
        assert_eq!(error_type, "Error");
        let error_msg = wire::read_string(&mut cursor).unwrap();
        assert!(error_msg.contains("not yet implemented"));
        let _error_num = wire::read_u64(&mut cursor).unwrap();
        // STDERR_LAST
        let last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(last, StderrMsg::Last as u64);
    }

    #[tokio::test]
    async fn unknown_opcode_error_message_contains_opcode() {
        let store = Arc::new(MockStore::new());

        let mut input = Vec::new();
        wire::write_u64(&mut input, 12345).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.expect("should handle gracefully");

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let msg_type = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(msg_type, StderrMsg::Error as u64);
        let _error_type = wire::read_string(&mut cursor).unwrap();
        let error_msg = wire::read_string(&mut cursor).unwrap();
        assert!(error_msg.contains("12345"), "error should include the opcode number");
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

    // ── Multiple sequential operations in one connection ────────

    #[tokio::test]
    async fn multiple_ops_in_sequence() {
        let test_path = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-hello-2.12";
        let store = Arc::new(MockStore::new().with_path(test_path, "sha256:abc123"));

        let mut input = Vec::new();
        // Op 1: IsValidPath (found)
        wire::write_u64(&mut input, WorkerOp::IsValidPath as u64).unwrap();
        wire::write_string(&mut input, test_path).unwrap();
        // Op 2: IsValidPath (not found)
        wire::write_u64(&mut input, WorkerOp::IsValidPath as u64).unwrap();
        wire::write_string(&mut input, "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-missing").unwrap();
        // Op 3: QueryAllValidPaths
        wire::write_u64(&mut input, WorkerOp::QueryAllValidPaths as u64).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());

        // Response 1: STDERR_LAST + true
        let stderr = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr, StderrMsg::Last as u64);
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(valid);

        // Response 2: STDERR_LAST + false
        let stderr = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr, StderrMsg::Last as u64);
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(!valid);

        // Response 3: STDERR_LAST + string list with 1 entry
        let stderr = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr, StderrMsg::Last as u64);
        let paths = wire::read_string_list(&mut cursor).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], test_path);
    }

    #[tokio::test]
    async fn mixed_ops_valid_invalid_path_info() {
        let test_path = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-hello-2.12";
        let store = Arc::new(MockStore::new().with_path(test_path, "sha256:abc"));

        let mut input = Vec::new();
        // Op 1: QueryPathInfo (found)
        wire::write_u64(&mut input, WorkerOp::QueryPathInfo as u64).unwrap();
        wire::write_string(&mut input, test_path).unwrap();
        // Op 2: IsValidPath (found)
        wire::write_u64(&mut input, WorkerOp::IsValidPath as u64).unwrap();
        wire::write_string(&mut input, test_path).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());

        // Response 1: QueryPathInfo
        let stderr = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr, StderrMsg::Last as u64);
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(valid);
        let _deriver = wire::read_string(&mut cursor).unwrap();
        let nar_hash = wire::read_string(&mut cursor).unwrap();
        assert_eq!(nar_hash, "sha256:abc");
        let _refs = wire::read_string_list(&mut cursor).unwrap();
        let _reg_time = wire::read_u64(&mut cursor).unwrap();
        let _nar_size = wire::read_u64(&mut cursor).unwrap();
        let _ultimate = wire::read_bool(&mut cursor).unwrap();
        let _sigs = wire::read_string_list(&mut cursor).unwrap();
        let _ca = wire::read_string(&mut cursor).unwrap();

        // Response 2: IsValidPath
        let stderr = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr, StderrMsg::Last as u64);
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(valid);
    }

    // ── ConnectionError variant tests ───────────────────────────

    #[test]
    fn connection_error_display_bad_magic() {
        let err = ConnectionError::BadMagic {
            expected: 0x6e697863,
            got: 0xDEADBEEF,
        };
        let msg = err.to_string();
        assert!(msg.contains("0x6e697863"), "should contain expected magic");
        assert!(msg.contains("0xdeadbeef"), "should contain actual magic");
    }

    #[test]
    fn connection_error_display_unknown_op() {
        let err = ConnectionError::UnknownOp(9999);
        assert_eq!(err.to_string(), "unknown opcode: 9999");
    }

    #[test]
    fn connection_error_display_protocol() {
        let err = ConnectionError::Protocol("invalid frame".to_string());
        assert_eq!(err.to_string(), "protocol error: invalid frame");
    }

    #[test]
    fn connection_error_display_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken");
        let err = ConnectionError::Io(io_err);
        assert!(err.to_string().contains("broken"));
    }

    #[test]
    fn connection_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let err: ConnectionError = io_err.into();
        assert!(matches!(err, ConnectionError::Io(_)));
    }

    // ── Edge case: invalid store path format ────────────────────

    #[tokio::test]
    async fn is_valid_path_invalid_store_path_format() {
        let store = Arc::new(MockStore::new());

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::IsValidPath as u64).unwrap();
        wire::write_string(&mut input, "not-a-valid-store-path").unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr, StderrMsg::Last as u64);
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(!valid, "invalid store path format should return false, not error");
    }

    #[tokio::test]
    async fn query_path_info_invalid_store_path_format() {
        let store = Arc::new(MockStore::new());

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryPathInfo as u64).unwrap();
        wire::write_string(&mut input, "/not/nix/store/path").unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr, StderrMsg::Last as u64);
        let valid = wire::read_bool(&mut cursor).unwrap();
        assert!(!valid, "invalid store path should return not-found, not error");
    }

    // ── Store error propagation tests ─────────────────────────

    struct FailingStore;

    #[async_trait::async_trait]
    impl Store for FailingStore {
        async fn query_path_info(
            &self,
            _path: &StorePath,
        ) -> StoreResult<Option<PathInfo>> {
            Err(sui_store::traits::StoreError::Database(
                "simulated failure".to_string(),
            ))
        }

        async fn is_valid_path(&self, _path: &StorePath) -> StoreResult<bool> {
            Err(sui_store::traits::StoreError::Database(
                "simulated failure".to_string(),
            ))
        }

        async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
            Err(sui_store::traits::StoreError::Database(
                "simulated failure".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn is_valid_path_propagates_store_error() {
        let store = Arc::new(FailingStore);

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::IsValidPath as u64).unwrap();
        wire::write_string(&mut input, "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-test").unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        let result = conn.run().await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConnectionError::Store(_)));
    }

    #[tokio::test]
    async fn query_path_info_propagates_store_error() {
        let store = Arc::new(FailingStore);

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryPathInfo as u64).unwrap();
        wire::write_string(&mut input, "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-test").unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        let result = conn.run().await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConnectionError::Store(_)));
    }

    #[tokio::test]
    async fn query_all_valid_paths_propagates_store_error() {
        let store = Arc::new(FailingStore);

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryAllValidPaths as u64).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        let result = conn.run().await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConnectionError::Store(_)));
    }

    // ── QueryAllValidPaths with empty store ─────────────────────

    #[tokio::test]
    async fn query_all_valid_paths_empty_store() {
        let store = Arc::new(MockStore::new());

        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryAllValidPaths as u64).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let stderr = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(stderr, StderrMsg::Last as u64);
        let paths = wire::read_string_list(&mut cursor).unwrap();
        assert!(paths.is_empty());
    }

    // ── Unimplemented WorkerOp coverage ─────────────────────────
    //
    // The daemon currently implements only a subset of opcodes; every other
    // known opcode falls into the catch-all "not yet implemented" arm in
    // dispatch.rs. These tests pin that behaviour for *every* unimplemented
    // variant so that adding a real handler is an explicit, observable
    // change (the test for that opcode will need updating).

    /// Drive `Connection::run` with a single u64 opcode and verify that the
    /// reply is the standard "not yet implemented" stderr-error frame
    /// followed by `STDERR_LAST`.
    async fn assert_op_unimplemented(op: WorkerOp) {
        let store = Arc::new(MockStore::new());
        let mut input = Vec::new();
        wire::write_u64(&mut input, op as u64).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;

        conn.run().await.expect("dispatch should not fail");

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let msg_type = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(
            msg_type,
            StderrMsg::Error as u64,
            "op {op:?} should produce STDERR_ERROR"
        );
        let error_type = wire::read_string(&mut cursor).unwrap();
        assert_eq!(error_type, "Error");
        let msg = wire::read_string(&mut cursor).unwrap();
        assert!(
            msg.contains("not yet implemented"),
            "op {op:?} message should mention not implemented, got {msg:?}"
        );
        let err_num = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(err_num, 0);
        let last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(last, StderrMsg::Last as u64);
    }

    // ── Newly-implemented read-path ops ──────────────────────────
    //
    // These six replace the earlier `unimpl_*` stubs that asserted
    // the op returned "not yet implemented". Each exercises the
    // happy-path wire format so a future refactor of the handler
    // can't silently change the response shape.

    #[tokio::test]
    async fn has_substitutes_returns_false() {
        // No substituter client yet → always false. Deliberate.
        let store = Arc::new(MockStore::new());
        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::HasSubstitutes as u64).unwrap();
        wire::write_string(&mut input, "/nix/store/whatever").unwrap();
        let reader = Cursor::new(input);
        let mut conn = Connection::new(store, reader, Vec::<u8>::new(), TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;
        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), StderrMsg::Last as u64);
        assert!(!wire::read_bool(&mut cursor).unwrap());
    }

    #[tokio::test]
    async fn unimpl_query_path_hash() {
        assert_op_unimplemented(WorkerOp::QueryPathHash).await;
    }

    #[tokio::test]
    async fn query_references_empty_for_unknown_path() {
        let store = Arc::new(MockStore::new());
        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryReferences as u64).unwrap();
        wire::write_string(&mut input, "/nix/store/missing").unwrap();
        let reader = Cursor::new(input);
        let mut conn = Connection::new(store, reader, Vec::<u8>::new(), TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;
        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), StderrMsg::Last as u64);
        let count = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(count, 0, "unknown path should produce empty reference list");
    }

    #[tokio::test]
    async fn query_referrers_empty_for_mock_store() {
        // MockStore inherits the trait default which returns
        // NotSupported. Our handler catches that and produces an
        // empty list, which is the expected graceful-degradation
        // behavior — a client querying referrers on an isolated
        // store just gets "nothing depends on this."
        let store = Arc::new(MockStore::new());
        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryReferrers as u64).unwrap();
        wire::write_string(&mut input, "/nix/store/something").unwrap();
        let reader = Cursor::new(input);
        let mut conn = Connection::new(store, reader, Vec::<u8>::new(), TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;
        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), StderrMsg::Last as u64);
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), 0);
    }

    #[tokio::test]
    async fn unimpl_add_to_store() {
        assert_op_unimplemented(WorkerOp::AddToStore).await;
    }

    #[tokio::test]
    async fn unimpl_add_text_to_store() {
        assert_op_unimplemented(WorkerOp::AddTextToStore).await;
    }

    #[tokio::test]
    async fn unimpl_build_paths() {
        assert_op_unimplemented(WorkerOp::BuildPaths).await;
    }

    #[tokio::test]
    async fn unimpl_ensure_path() {
        assert_op_unimplemented(WorkerOp::EnsurePath).await;
    }

    #[tokio::test]
    async fn add_temp_root_acks_with_one() {
        // Client sends the path to pin; we ACK with a u64 1, which
        // is what CppNix's protocol expects so the client's blocking
        // call returns.
        let store = Arc::new(MockStore::new());
        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::AddTempRoot as u64).unwrap();
        wire::write_string(&mut input, "/nix/store/anything").unwrap();
        let reader = Cursor::new(input);
        let mut conn = Connection::new(store, reader, Vec::<u8>::new(), TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;
        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), StderrMsg::Last as u64);
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), 1);
    }

    #[tokio::test]
    async fn unimpl_add_indirect_root() {
        assert_op_unimplemented(WorkerOp::AddIndirectRoot).await;
    }

    #[tokio::test]
    async fn unimpl_sync_with_gc() {
        assert_op_unimplemented(WorkerOp::SyncWithGC).await;
    }

    #[tokio::test]
    async fn unimpl_find_roots() {
        assert_op_unimplemented(WorkerOp::FindRoots).await;
    }

    #[tokio::test]
    async fn unimpl_export_path() {
        assert_op_unimplemented(WorkerOp::ExportPath).await;
    }

    #[tokio::test]
    async fn query_deriver_empty_string_for_missing_path() {
        let store = Arc::new(MockStore::new());
        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryDeriver as u64).unwrap();
        wire::write_string(&mut input, "/nix/store/missing").unwrap();
        let reader = Cursor::new(input);
        let mut conn = Connection::new(store, reader, Vec::<u8>::new(), TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;
        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), StderrMsg::Last as u64);
        let deriver = wire::read_string(&mut cursor).unwrap();
        assert_eq!(deriver, "");
    }

    #[tokio::test]
    async fn unimpl_collect_garbage() {
        assert_op_unimplemented(WorkerOp::CollectGarbage).await;
    }

    #[tokio::test]
    async fn unimpl_query_substitutable_path_info() {
        assert_op_unimplemented(WorkerOp::QuerySubstitutablePathInfo).await;
    }

    #[tokio::test]
    async fn unimpl_query_derivation_outputs() {
        assert_op_unimplemented(WorkerOp::QueryDerivationOutputs).await;
    }

    #[tokio::test]
    async fn unimpl_query_failed_paths() {
        assert_op_unimplemented(WorkerOp::QueryFailedPaths).await;
    }

    #[tokio::test]
    async fn unimpl_clear_failed_paths() {
        assert_op_unimplemented(WorkerOp::ClearFailedPaths).await;
    }

    #[tokio::test]
    async fn unimpl_import_paths() {
        assert_op_unimplemented(WorkerOp::ImportPaths).await;
    }

    #[tokio::test]
    async fn unimpl_query_derivation_output_names() {
        assert_op_unimplemented(WorkerOp::QueryDerivationOutputNames).await;
    }

    #[tokio::test]
    async fn unimpl_query_path_from_hash_part() {
        assert_op_unimplemented(WorkerOp::QueryPathFromHashPart).await;
    }

    #[tokio::test]
    async fn unimpl_query_substitutable_path_infos() {
        assert_op_unimplemented(WorkerOp::QuerySubstitutablePathInfos).await;
    }

    #[tokio::test]
    async fn query_valid_paths_filters_to_existing() {
        // Two candidates: one exists in the store, one doesn't.
        // Response should list only the existing one.
        let known = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-hello-2.12";
        let store = Arc::new(MockStore::new().with_path(known, "sha256:abc"));
        let mut input = Vec::new();
        wire::write_u64(&mut input, WorkerOp::QueryValidPaths as u64).unwrap();
        wire::write_u64(&mut input, 2).unwrap();
        wire::write_string(&mut input, known).unwrap();
        wire::write_string(&mut input, "/nix/store/00000000000000000000000000000000-missing").unwrap();
        // No trailing substitute bool — we test with an older
        // protocol version so `read_u64` doesn't try to consume
        // bytes that aren't there.
        let reader = Cursor::new(input);
        let mut conn = Connection::new(store, reader, Vec::<u8>::new(), TrustLevel::Trusted);
        conn.client_version = 26; // < PROTOCOL_MINOR_VALID_PATHS_SUBSTITUTE
        conn.run().await.unwrap();

        let mut cursor = Cursor::new(conn.writer.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), StderrMsg::Last as u64);
        let count = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(count, 1, "only the known path should come back");
        let first = wire::read_string(&mut cursor).unwrap();
        assert_eq!(first, known);
    }

    #[tokio::test]
    async fn unimpl_query_substitutable_paths() {
        assert_op_unimplemented(WorkerOp::QuerySubstitutablePaths).await;
    }

    #[tokio::test]
    async fn unimpl_query_valid_derivers() {
        assert_op_unimplemented(WorkerOp::QueryValidDerivers).await;
    }

    #[tokio::test]
    async fn unimpl_optimise_store() {
        assert_op_unimplemented(WorkerOp::OptimiseStore).await;
    }

    #[tokio::test]
    async fn unimpl_verify_store() {
        assert_op_unimplemented(WorkerOp::VerifyStore).await;
    }

    #[tokio::test]
    async fn unimpl_build_derivation() {
        assert_op_unimplemented(WorkerOp::BuildDerivation).await;
    }

    #[tokio::test]
    async fn unimpl_add_signatures() {
        assert_op_unimplemented(WorkerOp::AddSignatures).await;
    }

    #[tokio::test]
    async fn unimpl_nar_from_path() {
        assert_op_unimplemented(WorkerOp::NarFromPath).await;
    }

    #[tokio::test]
    async fn unimpl_add_to_store_nar() {
        assert_op_unimplemented(WorkerOp::AddToStoreNar).await;
    }

    #[tokio::test]
    async fn unimpl_query_missing() {
        assert_op_unimplemented(WorkerOp::QueryMissing).await;
    }

    #[tokio::test]
    async fn unimpl_query_derivation_output_map() {
        assert_op_unimplemented(WorkerOp::QueryDerivationOutputMap).await;
    }

    #[tokio::test]
    async fn unimpl_register_drv_output() {
        assert_op_unimplemented(WorkerOp::RegisterDrvOutput).await;
    }

    #[tokio::test]
    async fn unimpl_query_realisation() {
        assert_op_unimplemented(WorkerOp::QueryRealisation).await;
    }

    #[tokio::test]
    async fn unimpl_add_multiple_to_store() {
        assert_op_unimplemented(WorkerOp::AddMultipleToStore).await;
    }

    #[tokio::test]
    async fn unimpl_add_build_log() {
        assert_op_unimplemented(WorkerOp::AddBuildLog).await;
    }

    // ── SetOptions edge cases ──────────────────────────────────

    /// Build a `SetOptions` payload with a custom override count and entries.
    /// Always uses a modern client_version (>= 1.12) so the override block
    /// is included.
    fn build_set_options_with_overrides(client_version: u64, overrides: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, WorkerOp::SetOptions as u64).unwrap();
        // 6 fixed fields
        for _ in 0..6 {
            wire::write_u64(&mut buf, 0).unwrap();
        }
        // useBuildHook (only for old clients)
        if client_version < (1 << 8 | 12) {
            wire::write_u64(&mut buf, 1).unwrap();
        }
        // 5 more fields
        for _ in 0..5 {
            wire::write_u64(&mut buf, 0).unwrap();
        }
        if client_version >= (1 << 8 | 12) {
            wire::write_u64(&mut buf, overrides.len() as u64).unwrap();
            for (k, v) in overrides {
                wire::write_string(&mut buf, k).unwrap();
                wire::write_string(&mut buf, v).unwrap();
            }
        }
        buf
    }

    async fn run_set_options(payload: Vec<u8>, client_version: u64) -> Vec<u8> {
        let store = Arc::new(MockStore::new());
        let reader = Cursor::new(payload);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = client_version;
        conn.run().await.expect("SetOptions should succeed");
        conn.writer
    }

    #[tokio::test]
    async fn set_options_zero_overrides_explicit() {
        // Modern client, override count = 0.
        let payload = build_set_options_with_overrides(PROTOCOL_VERSION, &[]);
        let out = run_set_options(payload, PROTOCOL_VERSION).await;
        let mut cursor = Cursor::new(out.as_slice());
        let last = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(last, StderrMsg::Last as u64);
    }

    #[tokio::test]
    async fn set_options_many_overrides() {
        // 16 overrides exercises the loop bound and stays well below
        // any reasonable client-side cap.
        let owned: Vec<(String, String)> = (0..16)
            .map(|i| (format!("opt-{i}"), format!("value-{i}")))
            .collect();
        let pairs: Vec<(&str, &str)> = owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let payload = build_set_options_with_overrides(PROTOCOL_VERSION, &pairs);
        let out = run_set_options(payload, PROTOCOL_VERSION).await;
        let mut cursor = Cursor::new(out.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), StderrMsg::Last as u64);
    }

    #[tokio::test]
    async fn set_options_unknown_keys_accepted() {
        // The daemon currently ignores override keys, so unknown options
        // should be silently accepted.
        let payload = build_set_options_with_overrides(
            PROTOCOL_VERSION,
            &[
                ("totally-made-up-key", "yes"),
                ("another-fake-option", "1234"),
            ],
        );
        let out = run_set_options(payload, PROTOCOL_VERSION).await;
        let mut cursor = Cursor::new(out.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), StderrMsg::Last as u64);
    }

    #[tokio::test]
    async fn set_options_long_value_accepted() {
        // Stress padding logic with a long override value.
        let long_value = "v".repeat(257);
        let payload = build_set_options_with_overrides(
            PROTOCOL_VERSION,
            &[("max-jobs", long_value.as_str())],
        );
        let out = run_set_options(payload, PROTOCOL_VERSION).await;
        let mut cursor = Cursor::new(out.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), StderrMsg::Last as u64);
    }

    #[tokio::test]
    async fn set_options_empty_key_and_value() {
        let payload = build_set_options_with_overrides(PROTOCOL_VERSION, &[("", "")]);
        let out = run_set_options(payload, PROTOCOL_VERSION).await;
        let mut cursor = Cursor::new(out.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), StderrMsg::Last as u64);
    }

    #[tokio::test]
    async fn set_options_at_overrides_boundary_1_12() {
        // Exactly 1.12 is the first version that sends overrides and
        // skips useBuildHook. Verify both that the daemon parses it
        // correctly and that the response is the standard STDERR_LAST.
        let v: u64 = (1 << 8) | 12;
        let payload = build_set_options_with_overrides(v, &[("cores", "4")]);
        let out = run_set_options(payload, v).await;
        let mut cursor = Cursor::new(out.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), StderrMsg::Last as u64);
    }

    #[tokio::test]
    async fn set_options_just_below_overrides_boundary_1_11() {
        // 1.11: still sends useBuildHook, no overrides.
        let v: u64 = (1 << 8) | 11;
        let payload = build_set_options_with_overrides(v, &[]);
        let out = run_set_options(payload, v).await;
        let mut cursor = Cursor::new(out.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), StderrMsg::Last as u64);
    }

    // ── Handshake state machine ────────────────────────────────

    #[tokio::test]
    async fn handshake_then_immediate_disconnect_is_clean() {
        // After a successful handshake the client closes its side without
        // sending any opcodes. The dispatch loop should treat
        // UnexpectedEof as a clean disconnect and return Ok(()).
        let store = Arc::new(MockStore::new());
        let input = build_full_client_handshake(PROTOCOL_VERSION);
        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.handshake().await.expect("handshake");
        // No opcode bytes left to read.
        conn.run()
            .await
            .expect("clean EOF after handshake should not be an error");
    }

    #[tokio::test]
    async fn handshake_then_one_op_then_disconnect() {
        // Full lifecycle: handshake -> 1 op -> EOF.
        let test_path = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-hello-2.12";
        let store = Arc::new(MockStore::new().with_path(test_path, "sha256:abc"));

        let mut input = build_full_client_handshake(PROTOCOL_VERSION);
        wire::write_u64(&mut input, WorkerOp::IsValidPath as u64).unwrap();
        wire::write_string(&mut input, test_path).unwrap();

        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.handshake().await.expect("handshake");
        conn.run().await.expect("dispatch");

        // The writer should contain handshake response + op response.
        // We don't decode the handshake here (other tests do that)
        // — only verify there is more output than the handshake alone.
        assert!(conn.writer.len() > 16, "expected more than just magic+version");
    }

    #[tokio::test]
    async fn handshake_truncated_client_magic() {
        // Client connects but sends nothing -> read_u64 EOF -> Io error.
        let store = Arc::new(MockStore::new());
        let reader = Cursor::new(Vec::<u8>::new());
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        let err = conn.handshake().await.unwrap_err();
        assert!(matches!(err, ConnectionError::Io(_)));
    }

    #[tokio::test]
    async fn handshake_truncated_client_version() {
        // Client sends magic but never its version.
        let store = Arc::new(MockStore::new());
        let mut input = Vec::new();
        wire::write_u64(&mut input, WORKER_MAGIC_1).unwrap();
        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        let err = conn.handshake().await.unwrap_err();
        assert!(matches!(err, ConnectionError::Io(_)));
    }

    #[tokio::test]
    async fn handshake_zero_magic_is_bad_magic() {
        let store = Arc::new(MockStore::new());
        let mut input = Vec::new();
        wire::write_u64(&mut input, 0).unwrap();
        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        let err = conn.handshake().await.unwrap_err();
        match err {
            ConnectionError::BadMagic { expected, got } => {
                assert_eq!(expected, WORKER_MAGIC_1);
                assert_eq!(got, 0);
            }
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_max_magic_is_bad_magic() {
        let store = Arc::new(MockStore::new());
        let mut input = Vec::new();
        wire::write_u64(&mut input, u64::MAX).unwrap();
        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        let err = conn.handshake().await.unwrap_err();
        match err {
            ConnectionError::BadMagic { got, .. } => assert_eq!(got, u64::MAX),
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_writes_magic2_then_version_in_order() {
        // Specifically pin the byte order of the server response for
        // version >= 1.35 (magic2, version, daemon string, trust).
        let store = Arc::new(MockStore::new());
        let input = build_full_client_handshake(PROTOCOL_VERSION);
        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::NotTrusted);
        conn.handshake().await.expect("handshake");

        let mut cursor = Cursor::new(conn.writer.as_slice());
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), WORKER_MAGIC_2);
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), PROTOCOL_VERSION);
        let daemon_str = wire::read_string(&mut cursor).unwrap();
        assert!(daemon_str.starts_with("sui-daemon"));
        assert_eq!(wire::read_u64(&mut cursor).unwrap(), 2); // NotTrusted
    }

    #[tokio::test]
    async fn handshake_min_supported_protocol_version_1_10() {
        // 1.10 has neither CPU affinity nor reserve space. Should still
        // negotiate cleanly.
        let store = Arc::new(MockStore::new());
        let v: u64 = (1 << 8) | 10;
        let input = build_full_client_handshake(v);
        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.handshake().await.expect("1.10 handshake");
        assert_eq!(conn.client_version, v);
        // Trust field should NOT be sent for < 1.35.
        let mut cursor = Cursor::new(conn.writer.as_slice());
        let _magic2 = wire::read_u64(&mut cursor).unwrap();
        let _ver = wire::read_u64(&mut cursor).unwrap();
        let _daemon = wire::read_string(&mut cursor).unwrap();
        // No trailing bytes.
        assert_eq!(cursor.position() as usize, conn.writer.len());
    }

    #[tokio::test]
    async fn handshake_at_trust_exchange_boundary_1_35() {
        // First version that exchanges trust.
        let store = Arc::new(MockStore::new());
        let v: u64 = (1 << 8) | 35;
        let input = build_full_client_handshake(v);
        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.handshake().await.expect("1.35 handshake");

        let mut cursor = Cursor::new(conn.writer.as_slice());
        let _magic2 = wire::read_u64(&mut cursor).unwrap();
        let _ver = wire::read_u64(&mut cursor).unwrap();
        let _daemon = wire::read_string(&mut cursor).unwrap();
        let trust = wire::read_u64(&mut cursor).unwrap();
        assert_eq!(trust, 1);
    }

    // ── Connection lifecycle on cold reader ────────────────────

    #[tokio::test]
    async fn run_on_empty_input_is_clean_disconnect() {
        // run() before any input bytes (skipping handshake) should still
        // treat EOF on the first read_u64 as a clean disconnect, since
        // the dispatch loop is what does that translation.
        let store = Arc::new(MockStore::new());
        let reader = Cursor::new(Vec::<u8>::new());
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;
        conn.run().await.expect("EOF -> clean exit");
        assert!(conn.writer.is_empty(), "no bytes should have been written");
    }

    // ── ConnectionError remaining variants ─────────────────────

    #[test]
    fn connection_error_display_store_error() {
        let store_err = sui_store::traits::StoreError::Database("disk full".to_string());
        let err: ConnectionError = store_err.into();
        let s = err.to_string();
        assert!(s.contains("store error"), "{s}");
        assert!(s.contains("disk full"), "{s}");
    }

    #[test]
    fn connection_error_from_store_error_variant() {
        let store_err = sui_store::traits::StoreError::Database("dbfail".to_string());
        let err: ConnectionError = store_err.into();
        assert!(matches!(err, ConnectionError::Store(_)));
    }

    #[test]
    fn connection_error_protocol_constructible() {
        // Make sure the Protocol variant accepts arbitrary owned strings.
        let err = ConnectionError::Protocol(String::from("frame too short"));
        let s = err.to_string();
        assert!(s.contains("protocol error"));
        assert!(s.contains("frame too short"));
    }

    #[test]
    fn connection_error_unknown_op_zero() {
        let err = ConnectionError::UnknownOp(0);
        assert_eq!(err.to_string(), "unknown opcode: 0");
    }

    #[test]
    fn connection_error_unknown_op_max() {
        let err = ConnectionError::UnknownOp(u64::MAX);
        assert!(err.to_string().contains(&u64::MAX.to_string()));
    }

    #[test]
    fn connection_error_bad_magic_zero_inputs() {
        let err = ConnectionError::BadMagic { expected: 0, got: 0 };
        let s = err.to_string();
        assert!(s.contains("expected"));
        assert!(s.contains("got"));
    }

    // ── Multiple unknown opcodes in a row keep the loop alive ──

    #[tokio::test]
    async fn multiple_unknown_opcodes_in_sequence() {
        let store = Arc::new(MockStore::new());
        let mut input = Vec::new();
        for op in [9999u64, 8888, 7777] {
            wire::write_u64(&mut input, op).unwrap();
        }
        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.client_version = PROTOCOL_VERSION;
        conn.run().await.expect("loop should not bail");

        // Each unknown op writes STDERR_ERROR + 3 string/u64 fields + STDERR_LAST.
        // We just count the number of STDERR_ERROR markers seen.
        let mut cursor = Cursor::new(conn.writer.as_slice());
        let mut error_count = 0u32;
        while let Ok(v) = wire::read_u64(&mut cursor) {
            if v == StderrMsg::Error as u64 {
                error_count += 1;
                // skip type, message, error_num
                let _ = wire::read_string(&mut cursor).unwrap();
                let _ = wire::read_string(&mut cursor).unwrap();
                let _ = wire::read_u64(&mut cursor).unwrap();
            }
        }
        assert_eq!(error_count, 3, "should have produced 3 STDERR_ERROR frames");
    }

    // ── Handshake -> dispatch ordering: opcode bytes that look like
    //    a magic still get treated as opcode after handshake completed.

    #[tokio::test]
    async fn opcode_with_magic_value_is_unknown_after_handshake() {
        let store = Arc::new(MockStore::new());
        let mut input = build_full_client_handshake(PROTOCOL_VERSION);
        wire::write_u64(&mut input, WORKER_MAGIC_1).unwrap();
        let reader = Cursor::new(input);
        let writer: Vec<u8> = Vec::new();
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted);
        conn.handshake().await.expect("handshake");
        conn.run().await.expect("loop should not bail");

        // The dispatch response should contain a STDERR_ERROR mentioning
        // WORKER_MAGIC_1 as an unknown opcode.
        // We can't easily seek past the handshake bytes here, so just
        // check that the writer contains the magic-as-decimal somewhere
        // after the handshake header.
        let s = format!("{}", WORKER_MAGIC_1);
        let body = String::from_utf8_lossy(&conn.writer);
        assert!(body.contains(&s), "expected unknown-op message containing {s}");
    }
}
