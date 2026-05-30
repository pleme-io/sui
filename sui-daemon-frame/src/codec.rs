//! Low-level frame I/O.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::trace;

use sui_protocol::{WireFrame, FRAME_MAGIC};

use crate::FrameError;

/// Default maximum body length the codec will accept. 64 MiB — enough
/// for a batched closure-info response, small enough to catch a runaway
/// peer before it OOMs the daemon. Callers wanting a different cap can
/// build a [`FrameCodec`] explicitly and override.
pub const MAX_FRAME_BODY_BYTES: u32 = 64 * 1024 * 1024;

/// Stateful codec wrapper. Caches the body-length cap so per-call
/// arguments stay small.
#[derive(Debug, Clone, Copy)]
pub struct FrameCodec {
    pub max_body_bytes: u32,
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self {
            max_body_bytes: MAX_FRAME_BODY_BYTES,
        }
    }
}

impl FrameCodec {
    /// Build a codec with a custom body-length cap.
    #[must_use]
    pub const fn with_cap(max_body_bytes: u32) -> Self {
        Self { max_body_bytes }
    }

    /// Read one frame from `r`. Pulls magic + length + body in three
    /// `read_exact` calls. The body is validated against the codec's
    /// length cap before any allocation — so a hostile peer can't
    /// trick us into allocating a multi-GB buffer.
    ///
    /// # Errors
    ///
    /// See [`FrameError`].
    pub async fn read_frame<R>(&self, r: &mut R) -> Result<WireFrame, FrameError>
    where
        R: AsyncRead + Unpin,
    {
        read_frame(r, self.max_body_bytes).await
    }

    /// Write one frame to `w`. Three `write_all` calls: magic, length,
    /// body. The body is freshly serialized from `frame` at every call —
    /// caller is free to reuse `frame` afterward.
    ///
    /// # Errors
    ///
    /// See [`FrameError`].
    pub async fn write_frame<W>(&self, w: &mut W, frame: &WireFrame) -> Result<(), FrameError>
    where
        W: AsyncWrite + Unpin,
    {
        write_frame(w, frame).await
    }
}

/// Free-function reader. Used directly by tests and by the stateful
/// [`FrameCodec`].
///
/// # Errors
///
/// See [`FrameError`].
pub async fn read_frame<R>(r: &mut R, max_body_bytes: u32) -> Result<WireFrame, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).await?;
    if magic != FRAME_MAGIC {
        return Err(FrameError::BadMagic {
            expected: FRAME_MAGIC,
            got: magic,
        });
    }
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let body_len = u32::from_le_bytes(len_buf);
    if body_len > max_body_bytes {
        return Err(FrameError::FrameTooLarge {
            got: body_len,
            cap: max_body_bytes,
        });
    }
    let mut body = vec![0u8; body_len as usize];
    r.read_exact(&mut body).await?;
    trace!(
        target: "sui-daemon-frame",
        body_bytes = body_len,
        "read frame"
    );
    let frame = rkyv::from_bytes::<WireFrame, rkyv::rancor::Error>(&body)
        .map_err(|e| FrameError::Decode(e.to_string()))?;
    Ok(frame)
}

/// Free-function writer.
///
/// # Errors
///
/// See [`FrameError`].
pub async fn write_frame<W>(w: &mut W, frame: &WireFrame) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let body = rkyv::to_bytes::<rkyv::rancor::Error>(frame)
        .map_err(|e| FrameError::Decode(e.to_string()))?;
    let body_len: u32 = body
        .len()
        .try_into()
        .map_err(|_| FrameError::FrameTooLarge {
            got: u32::MAX,
            cap: u32::MAX,
        })?;
    w.write_all(&FRAME_MAGIC).await?;
    w.write_all(&body_len.to_le_bytes()).await?;
    w.write_all(&body).await?;
    trace!(
        target: "sui-daemon-frame",
        body_bytes = body_len,
        "wrote frame"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use sui_protocol::LocalRequest;
    use tokio::io::duplex;

    fn ping_frame() -> WireFrame {
        WireFrame::Request {
            id: 7,
            body: LocalRequest::Ping,
        }
    }

    #[tokio::test]
    async fn roundtrip_via_duplex() {
        let (mut a, mut b) = duplex(4096);
        let codec = FrameCodec::default();

        let sent = ping_frame();
        codec.write_frame(&mut a, &sent).await.unwrap();

        let got = codec.read_frame(&mut b).await.unwrap();
        assert!(matches!(got, WireFrame::Request { id: 7, .. }));
    }

    #[tokio::test]
    async fn rejects_bad_magic() {
        // 4 bytes of garbage followed by 4 zero len bytes — never
        // matches our magic.
        let (mut a, mut b) = duplex(64);
        a.write_all(b"XXXX\0\0\0\0").await.unwrap();
        let err = read_frame(&mut b, MAX_FRAME_BODY_BYTES).await.unwrap_err();
        assert!(matches!(err, FrameError::BadMagic { .. }));
    }

    #[tokio::test]
    async fn rejects_oversized_body() {
        let (mut a, mut b) = duplex(64);
        a.write_all(&FRAME_MAGIC).await.unwrap();
        // Length larger than our 100-byte cap.
        a.write_all(&5_000u32.to_le_bytes()).await.unwrap();
        let err = read_frame(&mut b, 100).await.unwrap_err();
        assert!(matches!(err, FrameError::FrameTooLarge { .. }));
    }

    #[tokio::test]
    async fn multi_frame_stream_in_order() {
        let (mut a, mut b) = duplex(8192);
        let codec = FrameCodec::default();

        for i in 0u64..16 {
            let f = WireFrame::Request {
                id: i,
                body: LocalRequest::Ping,
            };
            codec.write_frame(&mut a, &f).await.unwrap();
        }
        for i in 0u64..16 {
            let got = codec.read_frame(&mut b).await.unwrap();
            match got {
                WireFrame::Request { id, .. } => assert_eq!(id, i),
                other => panic!("expected Request {i}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn truncated_stream_surfaces_io_error() {
        let (mut a, mut b) = duplex(64);
        // Write only magic + 4 bytes of length, then close.
        a.write_all(&FRAME_MAGIC).await.unwrap();
        a.write_all(&12u32.to_le_bytes()).await.unwrap();
        drop(a);
        let err = read_frame(&mut b, MAX_FRAME_BODY_BYTES).await.unwrap_err();
        assert!(matches!(err, FrameError::Io(_)));
    }
}
