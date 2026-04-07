//! Unix socket daemon server.
//!
//! Listens on a configurable Unix socket path (default: XDG-compliant via
//! tsunagu, or `/nix/var/nix/daemon-socket/socket` as Nix-compat fallback),
//! accepts connections, and spawns a tokio task per connection to handle the
//! Nix worker protocol.
//!
//! Uses [`tsunagu::DaemonProcess`] for PID file management and
//! [`tsunagu::HealthCheck`] for standardized health responses.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::UnixListener;
use tsunagu::{DaemonProcess, HealthCheck, SocketPath};

use sui_store::traits::Store;

use crate::connection::Connection;
use crate::trust::TrustLevel;

/// Default daemon socket path (Nix-compat legacy path).
pub const DEFAULT_SOCKET_PATH: &str = "/nix/var/nix/daemon-socket/socket";

/// XDG-compliant socket path resolved via tsunagu.
///
/// Returns the path from `tsunagu::SocketPath::for_app("sui")`, which
/// resolves to `$XDG_RUNTIME_DIR/sui/sui.sock` or `/tmp/sui/sui.sock`.
#[must_use]
pub fn xdg_socket_path() -> PathBuf {
    SocketPath::for_app("sui")
}

/// Configuration for the daemon server.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Unix socket path to listen on.
    pub socket_path: PathBuf,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: xdg_socket_path(),
        }
    }
}

impl DaemonConfig {
    /// Create a config with a custom socket path.
    pub fn with_socket_path(path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: path.into(),
        }
    }

    /// Create a config using the Nix-compatible legacy socket path.
    #[must_use]
    pub fn nix_compat() -> Self {
        Self {
            socket_path: PathBuf::from(DEFAULT_SOCKET_PATH),
        }
    }
}

/// The daemon server — listens for connections and spawns handlers.
pub struct DaemonServer<S> {
    config: DaemonConfig,
    store: Arc<S>,
    start_time: Instant,
}

impl<S> DaemonServer<S>
where
    S: Store + 'static,
{
    /// Create a new daemon server.
    pub fn new(config: DaemonConfig, store: S) -> Self {
        Self {
            config,
            store: Arc::new(store),
            start_time: Instant::now(),
        }
    }

    /// Return a tsunagu health check for the daemon.
    #[must_use]
    pub fn health(&self) -> HealthCheck {
        let uptime = self.start_time.elapsed().as_secs();
        HealthCheck::healthy("sui-daemon", env!("CARGO_PKG_VERSION")).with_uptime(uptime)
    }

    /// Run the daemon — listen for connections and serve them.
    ///
    /// Acquires a PID lock via [`tsunagu::DaemonProcess`], then binds
    /// to the configured Unix socket, removes any stale socket file
    /// first, and enters the accept loop. The PID file is automatically
    /// cleaned up on drop.
    pub async fn run(&self) -> Result<(), DaemonError> {
        // Acquire PID lock via tsunagu.
        // Derive the PID file path from the socket path so that each
        // socket gets its own lock (avoids contention during testing).
        let pid_path = self.config.socket_path.with_extension("pid");
        let daemon_process = DaemonProcess::with_paths(
            "sui",
            pid_path,
            self.config.socket_path.clone(),
        );
        daemon_process.acquire().map_err(|e| {
            DaemonError::Bind(format!("failed to acquire PID lock: {e}"))
        })?;

        let socket_path = &self.config.socket_path;

        // Ensure parent directory exists.
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                DaemonError::Bind(format!(
                    "failed to create socket directory {}: {e}",
                    parent.display()
                ))
            })?;
        }

        // Remove stale socket file if it exists.
        if socket_path.exists() {
            tokio::fs::remove_file(socket_path).await.map_err(|e| {
                DaemonError::Bind(format!(
                    "failed to remove stale socket {}: {e}",
                    socket_path.display()
                ))
            })?;
        }

        let listener = UnixListener::bind(socket_path).map_err(|e| {
            DaemonError::Bind(format!(
                "failed to bind to {}: {e}",
                socket_path.display()
            ))
        })?;

        tracing::info!(socket = %socket_path.display(), "daemon listening");

        loop {
            let (stream, _addr) = listener.accept().await.map_err(DaemonError::Accept)?;

            let store = Arc::clone(&self.store);

            tokio::spawn(async move {
                let trust = resolve_trust(&stream);
                let (reader, writer) = tokio::io::split(stream);

                let mut conn = Connection::new(store, reader, writer, trust);

                if let Err(e) = conn.handshake().await {
                    tracing::warn!("handshake failed: {e}");
                    return;
                }

                if let Err(e) = conn.run().await {
                    tracing::warn!("connection error: {e}");
                }
            });
        }

        // `daemon_process` dropped here — PID file and socket cleaned up.
    }
}

/// Resolve the trust level from Unix socket peer credentials.
fn resolve_trust(stream: &tokio::net::UnixStream) -> TrustLevel {
    use crate::trust::SystemPeerCredentials;

    // On macOS and Linux, we can get peer credentials from the socket.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        match stream.peer_cred() {
            Ok(cred) => TrustLevel::from_uid(cred.uid(), &SystemPeerCredentials),
            Err(e) => {
                tracing::warn!("failed to get peer credentials: {e}, defaulting to not-trusted");
                TrustLevel::NotTrusted
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        tracing::warn!("peer credentials not available on this platform, defaulting to not-trusted");
        TrustLevel::NotTrusted
    }
}

/// Errors from the daemon server lifecycle (binding, accepting, or store setup).
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// Failed to bind to the Unix socket (includes PID lock failures).
    #[error("bind error: {0}")]
    Bind(String),
    /// Failed to accept an incoming connection.
    #[error("accept error: {0}")]
    Accept(#[source] std::io::Error),
    /// A store-level error during server setup.
    #[error("store error: {0}")]
    Store(#[from] sui_store::traits::StoreError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use sui_compat::store_path::StorePath;
    use sui_compat::wire;
    use sui_store::traits::{PathInfo, StoreResult};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    /// Minimal mock store for server-level tests.
    struct MockStore;

    #[async_trait::async_trait]
    impl Store for MockStore {
        async fn query_path_info(
            &self,
            _path: &StorePath,
        ) -> StoreResult<Option<PathInfo>> {
            Ok(None)
        }

        async fn is_valid_path(&self, _path: &StorePath) -> StoreResult<bool> {
            Ok(false)
        }

        async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn server_accepts_connection() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");

        let config = DaemonConfig::with_socket_path(&socket_path);
        let server = DaemonServer::new(config, MockStore);

        // Spawn server in background
        let server_handle = tokio::spawn(async move {
            // Ignore the error when we drop the test
            let _ = server.run().await;
        });

        // Wait briefly for the server to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect as a client and perform handshake
        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);

        // Send WORKER_MAGIC_1
        writer
            .write_all(&wire::WORKER_MAGIC_1.to_le_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();

        // Read WORKER_MAGIC_2
        let mut buf = [0u8; 8];
        reader.read_exact(&mut buf).await.unwrap();
        let magic2 = u64::from_le_bytes(buf);
        assert_eq!(magic2, wire::WORKER_MAGIC_2);

        // Read protocol version
        reader.read_exact(&mut buf).await.unwrap();
        let version = u64::from_le_bytes(buf);
        assert_eq!(version, wire::PROTOCOL_VERSION);

        // Send client version
        let client_version = wire::PROTOCOL_VERSION;
        writer
            .write_all(&client_version.to_le_bytes())
            .await
            .unwrap();
        // CPU affinity
        writer.write_all(&0u64.to_le_bytes()).await.unwrap();
        // Reserve
        writer.write_all(&0u64.to_le_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        // Read daemon version string (length-prefixed)
        reader.read_exact(&mut buf).await.unwrap();
        let str_len = u64::from_le_bytes(buf) as usize;
        let mut str_buf = vec![0u8; str_len];
        reader.read_exact(&mut str_buf).await.unwrap();
        let daemon_version = String::from_utf8(str_buf).unwrap();
        assert!(daemon_version.starts_with("sui-daemon"));

        // Read padding for the string
        let pad = (8 - (str_len % 8)) % 8;
        if pad > 0 {
            let mut pad_buf = vec![0u8; pad];
            reader.read_exact(&mut pad_buf).await.unwrap();
        }

        // Read trust level
        reader.read_exact(&mut buf).await.unwrap();
        let trust = u64::from_le_bytes(buf);
        // Should be trusted (same UID)
        assert_eq!(trust, 1);

        // Clean up
        server_handle.abort();
    }

    #[tokio::test]
    async fn server_removes_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("stale.sock");

        // Create a fake stale socket file
        tokio::fs::write(&socket_path, b"stale").await.unwrap();
        assert!(socket_path.exists());

        let config = DaemonConfig::with_socket_path(&socket_path);
        let server = DaemonServer::new(config, MockStore);

        let handle = tokio::spawn(async move {
            let _ = server.run().await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Should be able to connect now (stale file was removed)
        let result = UnixStream::connect(&socket_path).await;
        assert!(result.is_ok());

        handle.abort();
    }

    #[test]
    fn default_config_uses_xdg_socket_path() {
        let config = DaemonConfig::default();
        // tsunagu resolves to $XDG_RUNTIME_DIR/sui/sui.sock or /tmp/sui/sui.sock
        let expected = xdg_socket_path();
        assert_eq!(config.socket_path, expected);
        assert!(config.socket_path.to_string_lossy().contains("sui"));
        assert!(config.socket_path.to_string_lossy().ends_with("sui.sock"));
    }

    #[test]
    fn nix_compat_config() {
        let config = DaemonConfig::nix_compat();
        assert_eq!(
            config.socket_path,
            Path::new("/nix/var/nix/daemon-socket/socket")
        );
    }

    #[test]
    fn custom_config() {
        let config = DaemonConfig::with_socket_path("/tmp/test.sock");
        assert_eq!(config.socket_path, Path::new("/tmp/test.sock"));
    }

    #[test]
    fn health_check_reports_healthy() {
        let config = DaemonConfig::with_socket_path("/tmp/health-test.sock");
        let server = DaemonServer::new(config, MockStore);
        let health = server.health();
        assert!(health.is_healthy());
        assert_eq!(health.service, "sui-daemon");
        assert!(health.uptime_secs.is_some());
    }

    #[test]
    fn daemon_error_display_bind() {
        let err = DaemonError::Bind("address in use".to_string());
        assert_eq!(err.to_string(), "bind error: address in use");
    }

    #[test]
    fn daemon_error_display_accept() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let err = DaemonError::Accept(io_err);
        assert!(err.to_string().contains("accept error"));
    }

    #[test]
    fn daemon_error_from_store_error() {
        let store_err =
            sui_store::traits::StoreError::Database("db down".to_string());
        let err: DaemonError = store_err.into();
        assert!(matches!(err, DaemonError::Store(_)));
        assert!(err.to_string().contains("db down"));
    }

    #[test]
    fn xdg_socket_path_ends_with_sock() {
        let path = xdg_socket_path();
        assert!(path.to_string_lossy().ends_with("sui.sock"));
    }
}
