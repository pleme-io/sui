//! Async wire primitives for the Nix worker protocol.
//!
//! Mirrors `sui_compat::wire` but async via `tokio::io`. The Nix wire
//! format uses:
//!
//! - **Integers**: u64 little-endian (8 bytes)
//! - **Bytes**: u64 length prefix + payload + zero-padding to 8-byte boundary
//! - **Strings**: bytes-encoded UTF-8
//! - **Booleans**: u64 where `0 = false`, `1 = true`
//! - **String lists**: u64 count + N string frames

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::ConnectionError;

/// Padding needed to align `len` bytes to an 8-byte boundary.
const fn padding(len: usize) -> usize {
    (8 - (len % 8)) % 8
}

/// Pre-allocated zero buffer for padding writes (max 7 bytes needed).
const ZERO_PAD: [u8; 7] = [0; 7];

pub(crate) async fn write_u64(w: &mut (impl AsyncWrite + Unpin), v: u64) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes()).await
}

pub(crate) async fn read_u64(r: &mut (impl AsyncRead + Unpin)) -> std::io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).await?;
    Ok(u64::from_le_bytes(buf))
}

pub(crate) async fn write_bytes(
    w: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
) -> std::io::Result<()> {
    write_u64(w, data.len() as u64).await?;
    w.write_all(data).await?;
    let pad = padding(data.len());
    if pad > 0 {
        w.write_all(&ZERO_PAD[..pad]).await?;
    }
    Ok(())
}

pub(crate) async fn read_bytes(r: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
    let len = read_u64(r).await? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    let pad = padding(len);
    if pad > 0 {
        let mut pad_buf = [0u8; 7];
        r.read_exact(&mut pad_buf[..pad]).await?;
    }
    Ok(buf)
}

pub(crate) async fn write_string(
    w: &mut (impl AsyncWrite + Unpin),
    s: &str,
) -> std::io::Result<()> {
    write_bytes(w, s.as_bytes()).await
}

pub(crate) async fn read_string(
    r: &mut (impl AsyncRead + Unpin),
) -> Result<String, ConnectionError> {
    let bytes = read_bytes(r).await?;
    String::from_utf8(bytes).map_err(|e| ConnectionError::Protocol(format!("invalid UTF-8: {e}")))
}

pub(crate) async fn write_bool(
    w: &mut (impl AsyncWrite + Unpin),
    v: bool,
) -> std::io::Result<()> {
    write_u64(w, u64::from(v)).await
}

pub(crate) async fn write_string_list(
    w: &mut (impl AsyncWrite + Unpin),
    list: &[String],
) -> std::io::Result<()> {
    write_u64(w, list.len() as u64).await?;
    for s in list {
        write_string(w, s).await?;
    }
    Ok(())
}

/// Write `STDERR_LAST` to signal the end of the stderr stream.
pub(crate) async fn write_stderr_last(
    w: &mut (impl AsyncWrite + Unpin),
) -> std::io::Result<()> {
    use sui_compat::wire::StderrMsg;
    write_u64(w, StderrMsg::Last as u64).await
}

/// Write an error response via the stderr protocol.
pub(crate) async fn write_stderr_error(
    w: &mut (impl AsyncWrite + Unpin),
    msg: &str,
) -> std::io::Result<()> {
    use sui_compat::wire::StderrMsg;
    write_u64(w, StderrMsg::Error as u64).await?;
    write_string(w, "Error").await?;
    write_string(w, msg).await?;
    write_u64(w, 0).await?;
    Ok(())
}
