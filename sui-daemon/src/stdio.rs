
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use sui_store::traits::Store;

use crate::connection::{Connection, ConnectionError, SignaturePolicy};
use crate::trust::TrustLevel;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdioBackend {
    Local,
    Forward(PathBuf),
}

pub type TrustOverride = Option<TrustLevel>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StdioError {
    #[error("{0}")]
    Refused(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected EOF from daemon socket {0}")]
    DaemonEof(PathBuf),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
}

pub fn resolve_backend(store_url: Option<&str>, state_dir: &Path, default_socket: &Path) -> Result<StdioBackend, StdioError> {
    match store_url.map(str::trim) {
        None | Some("" | "auto") => Ok(if access_rw(state_dir) {
            StdioBackend::Local
        } else if default_socket.exists() {
            StdioBackend::Forward(default_socket.to_path_buf())
        } else {
            StdioBackend::Local
        }),
        Some("daemon") => Ok(StdioBackend::Forward(default_socket.to_path_buf())),
        Some("local") => Ok(StdioBackend::Local),
        Some(url) => match url.strip_prefix("unix://") {
            Some(path) => {
                let path = path.split_once('?').map_or(path, |(p, _)| p);
                if path.is_empty() {
                    Ok(StdioBackend::Forward(default_socket.to_path_buf()))
                } else {
                    Ok(StdioBackend::Forward(PathBuf::from(path)))
                }
            }
            None => Err(StdioError::Refused(format!(
                "nix-daemon --stdio: sui serves the local store or a daemon socket (auto, local, daemon, unix://<path>), not '{url}'"
            ))),
        },
    }
}

fn access_rw(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
}

pub fn check_trust(backend: &StdioBackend, trust: TrustOverride) -> Result<(), StdioError> {
    match (backend, trust) {
        (StdioBackend::Forward(socket), Some(TrustLevel::NotTrusted)) => Err(StdioError::Refused(format!(
            "nix-daemon --stdio --force-untrusted: the store is the daemon at {}, which decides trust itself; \
             run as a user that can open the local store, or use --store local",
            socket.display()
        ))),
        _ => Ok(()),
    }
}

pub async fn serve_local<S, R, W>(
    store: Arc<S>,
    reader: R,
    writer: W,
    trust: TrustOverride,
    caches: Vec<Arc<sui_store::BinaryCacheStore>>,
    signatures: SignaturePolicy,
) -> Result<(), StdioError>
where
    S: Store + 'static,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut conn = Connection::new(store, reader, writer, trust.unwrap_or(TrustLevel::Trusted))
        .with_caches(caches)
        .with_signature_policy(signatures);
    conn.handshake().await?;
    conn.run().await?;
    Ok(())
}

pub async fn forward<R, W>(socket: &Path, mut input: R, mut output: W) -> Result<(), StdioError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let stream = tokio::net::UnixStream::connect(socket).await.map_err(|e| {
        StdioError::Refused(format!("cannot connect to the daemon at {}: {e}", socket.display()))
    })?;
    let (mut from_daemon, mut to_daemon) = stream.into_split();
    let upstream = async {
        pump(&mut input, &mut to_daemon).await?;
        to_daemon.shutdown().await.ok();
        Ok::<(), std::io::Error>(())
    };
    let downstream = async {
        pump(&mut from_daemon, &mut output).await?;
        Ok::<(), std::io::Error>(())
    };
    tokio::pin!(downstream);
    tokio::select! {
        r = upstream => {
            r?;
            downstream.await.map_err(StdioError::from)
        }
        r = &mut downstream => {
            r?;
            Err(StdioError::DaemonEof(socket.to_path_buf()))
        }
    }
}

async fn pump<R, W>(from: &mut R, to: &mut W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = from.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        to.write_all(&buf[..n]).await?;
        to.flush().await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_with_a_writable_state_dir_is_local() {
        let dir = tempfile::tempdir().unwrap();
        let got = resolve_backend(None, dir.path(), Path::new("/nonexistent/socket")).unwrap();
        assert_eq!(got, StdioBackend::Local);
    }

    #[test]
    fn auto_without_state_access_and_with_a_socket_forwards() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("socket");
        std::fs::write(&sock, b"").unwrap();
        let got = resolve_backend(Some("auto"), Path::new("/nonexistent/state"), &sock).unwrap();
        assert_eq!(got, StdioBackend::Forward(sock));
    }

    #[test]
    fn auto_with_neither_is_local() {
        let got = resolve_backend(Some(""), Path::new("/nonexistent/state"), Path::new("/nonexistent/socket")).unwrap();
        assert_eq!(got, StdioBackend::Local);
    }

    #[test]
    fn daemon_and_unix_urls_forward() {
        let d = Path::new("/nix/var/nix/daemon-socket/socket");
        assert_eq!(resolve_backend(Some("daemon"), d, d).unwrap(), StdioBackend::Forward(d.to_path_buf()));
        assert_eq!(resolve_backend(Some("unix://"), d, d).unwrap(), StdioBackend::Forward(d.to_path_buf()));
        assert_eq!(
            resolve_backend(Some("unix:///tmp/s.sock?x=1"), d, d).unwrap(),
            StdioBackend::Forward(PathBuf::from("/tmp/s.sock"))
        );
        assert_eq!(resolve_backend(Some("local"), d, d).unwrap(), StdioBackend::Local);
    }

    #[test]
    fn other_schemes_are_refused_by_name() {
        let d = Path::new("/x");
        let err = resolve_backend(Some("https://cache.nixos.org"), d, d).unwrap_err();
        assert!(err.to_string().contains("https://cache.nixos.org"), "{err}");
    }

    #[test]
    fn force_untrusted_is_refused_only_when_forwarding() {
        let fwd = StdioBackend::Forward(PathBuf::from("/s"));
        assert!(check_trust(&fwd, Some(TrustLevel::NotTrusted)).is_err());
        assert!(check_trust(&fwd, Some(TrustLevel::Trusted)).is_ok());
        assert!(check_trust(&fwd, None).is_ok());
        assert!(check_trust(&StdioBackend::Local, Some(TrustLevel::NotTrusted)).is_ok());
    }

    #[tokio::test]
    async fn forward_ferries_both_directions_and_ends_on_client_eof() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("echo.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            s.read_exact(&mut buf).await.unwrap();
            buf.reverse();
            s.write_all(&buf).await.unwrap();
            let mut rest = Vec::new();
            s.read_to_end(&mut rest).await.unwrap();
        });
        let mut out = Vec::new();
        forward(&sock, &b"hello"[..], &mut out).await.unwrap();
        assert_eq!(out, b"olleh");
    }

    #[tokio::test]
    async fn forward_reports_a_daemon_that_closes_first() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("closer.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            drop(s);
        });
        let (client, _keep) = tokio::io::duplex(64);
        let err = forward(&sock, client, Vec::new()).await.unwrap_err();
        assert!(matches!(err, StdioError::DaemonEof(_)), "{err}");
    }
}
