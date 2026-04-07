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
}
