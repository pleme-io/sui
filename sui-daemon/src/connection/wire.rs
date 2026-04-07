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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ── padding() helper exhaustive ────────────────────────────

    #[test]
    fn padding_zero_for_aligned_lengths() {
        for len in [0usize, 8, 16, 24, 32, 64, 128, 1024, 8192] {
            assert_eq!(padding(len), 0, "len {len} must be aligned");
        }
    }

    #[test]
    fn padding_complement_to_eight() {
        // (len % 8) -> required padding bytes.
        let cases = [
            (1usize, 7usize),
            (2, 6),
            (3, 5),
            (4, 4),
            (5, 3),
            (6, 2),
            (7, 1),
            (9, 7),
            (10, 6),
            (15, 1),
            (17, 7),
            (23, 1),
        ];
        for (len, expected) in cases {
            assert_eq!(padding(len), expected, "len {len}");
        }
    }

    // ── write_bytes alignment for every modulo class ───────────

    #[tokio::test]
    async fn write_bytes_pads_every_length_class() {
        // For each modulo class 0..=7 we expect the total buffer to be a
        // multiple of 8 (8 byte length prefix + payload + padding).
        for len in 0usize..=23 {
            let payload = vec![0xABu8; len];
            let mut buf = Vec::new();
            write_bytes(&mut buf, &payload).await.unwrap();
            assert_eq!(buf.len() % 8, 0, "buffer for len={len} must be 8-aligned");
            // length prefix + payload + padding
            let pad = (8 - (len % 8)) % 8;
            assert_eq!(buf.len(), 8 + len + pad);

            // round-trip
            let mut cursor = Cursor::new(buf);
            let got = read_bytes(&mut cursor).await.unwrap();
            assert_eq!(got, payload);
        }
    }

    #[tokio::test]
    async fn read_bytes_consumes_padding() {
        // After reading a length-7 payload (1 padding byte) we should be
        // able to immediately read another u64 from the same stream.
        let mut buf = Vec::new();
        write_bytes(&mut buf, b"1234567").await.unwrap();
        write_u64(&mut buf, 0xCAFE).await.unwrap();
        let mut cursor = Cursor::new(buf);
        let payload = read_bytes(&mut cursor).await.unwrap();
        assert_eq!(payload, b"1234567");
        let trailing = read_u64(&mut cursor).await.unwrap();
        assert_eq!(trailing, 0xCAFE);
    }

    #[tokio::test]
    async fn read_bytes_zero_length() {
        let mut buf = Vec::new();
        write_bytes(&mut buf, &[]).await.unwrap();
        // 8 byte length prefix only, zero padding for empty payload.
        assert_eq!(buf.len(), 8);
        let mut cursor = Cursor::new(buf);
        let got = read_bytes(&mut cursor).await.unwrap();
        assert!(got.is_empty());
    }

    // ── read_string error paths ────────────────────────────────

    #[tokio::test]
    async fn read_string_rejects_invalid_utf8() {
        // Write raw bytes that are not valid UTF-8.
        let mut buf = Vec::new();
        write_bytes(&mut buf, &[0xff, 0xfe, 0xfd]).await.unwrap();
        let mut cursor = Cursor::new(buf);
        let err = read_string(&mut cursor).await.unwrap_err();
        match err {
            ConnectionError::Protocol(msg) => {
                assert!(msg.contains("UTF-8"), "error should mention UTF-8: {msg}");
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_u64_eof_propagates() {
        // Empty buffer -> UnexpectedEof.
        let buf: Vec<u8> = Vec::new();
        let mut cursor = Cursor::new(buf);
        let err = read_u64(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn read_u64_partial_propagates() {
        // 4-byte buffer; reading u64 (8 bytes) should fail.
        let buf = vec![0u8; 4];
        let mut cursor = Cursor::new(buf);
        let err = read_u64(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn read_bytes_truncated_payload_eof() {
        // Length prefix says 100 bytes but buffer only has 8 (the prefix).
        let mut buf = Vec::new();
        write_u64(&mut buf, 100).await.unwrap();
        let mut cursor = Cursor::new(buf);
        let err = read_bytes(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    // ── string round-trip exhaustive ───────────────────────────

    #[tokio::test]
    async fn write_string_round_trip_various_lengths() {
        for len in 0usize..=64 {
            let s: String = std::iter::repeat('x').take(len).collect();
            let mut buf = Vec::new();
            write_string(&mut buf, &s).await.unwrap();
            assert_eq!(buf.len() % 8, 0);
            let mut cursor = Cursor::new(buf);
            let got = read_string(&mut cursor).await.unwrap();
            assert_eq!(got, s);
        }
    }

    #[tokio::test]
    async fn write_string_unicode_payloads() {
        let cases = ["", "a", "日本語", "🦀🚀✨", "mixed日本🦀"];
        for s in cases {
            let mut buf = Vec::new();
            write_string(&mut buf, s).await.unwrap();
            assert_eq!(buf.len() % 8, 0, "buffer for {s:?} must be 8-aligned");
            let mut cursor = Cursor::new(buf);
            let got = read_string(&mut cursor).await.unwrap();
            assert_eq!(got, s);
        }
    }

    // ── write_string_list edge cases ───────────────────────────

    #[tokio::test]
    async fn write_string_list_single_element() {
        let mut buf = Vec::new();
        write_string_list(&mut buf, &["one".to_string()]).await.unwrap();
        let mut cursor = Cursor::new(buf);
        let count = read_u64(&mut cursor).await.unwrap();
        assert_eq!(count, 1);
        let s = read_string(&mut cursor).await.unwrap();
        assert_eq!(s, "one");
    }

    #[tokio::test]
    async fn write_string_list_many_elements() {
        let list: Vec<String> = (0..16).map(|i| format!("item-{i}")).collect();
        let mut buf = Vec::new();
        write_string_list(&mut buf, &list).await.unwrap();
        let mut cursor = Cursor::new(buf);
        let count = read_u64(&mut cursor).await.unwrap() as usize;
        assert_eq!(count, list.len());
        for expected in &list {
            let s = read_string(&mut cursor).await.unwrap();
            assert_eq!(&s, expected);
        }
    }

    // ── write_bool encoding ────────────────────────────────────

    #[tokio::test]
    async fn write_bool_encoding() {
        // false -> 0, true -> 1.
        let mut buf = Vec::new();
        write_bool(&mut buf, false).await.unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(read_u64(&mut cursor).await.unwrap(), 0);

        let mut buf = Vec::new();
        write_bool(&mut buf, true).await.unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(read_u64(&mut cursor).await.unwrap(), 1);
    }

    // ── stderr helpers ─────────────────────────────────────────

    #[tokio::test]
    async fn write_stderr_last_writes_one_u64() {
        use sui_compat::wire::StderrMsg;
        let mut buf = Vec::new();
        write_stderr_last(&mut buf).await.unwrap();
        assert_eq!(buf.len(), 8);
        let mut cursor = Cursor::new(buf);
        assert_eq!(read_u64(&mut cursor).await.unwrap(), StderrMsg::Last as u64);
    }

    #[tokio::test]
    async fn write_stderr_error_frame_layout() {
        use sui_compat::wire::StderrMsg;
        let mut buf = Vec::new();
        write_stderr_error(&mut buf, "boom").await.unwrap();
        let mut cursor = Cursor::new(buf);
        // marker
        assert_eq!(read_u64(&mut cursor).await.unwrap(), StderrMsg::Error as u64);
        // type "Error"
        assert_eq!(read_string(&mut cursor).await.unwrap(), "Error");
        // message
        assert_eq!(read_string(&mut cursor).await.unwrap(), "boom");
        // error number
        assert_eq!(read_u64(&mut cursor).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn write_stderr_error_handles_long_messages() {
        let long_msg: String = "x".repeat(1024);
        let mut buf = Vec::new();
        write_stderr_error(&mut buf, &long_msg).await.unwrap();
        // Whole frame must remain 8-aligned.
        assert_eq!(buf.len() % 8, 0);
        let mut cursor = Cursor::new(buf);
        let _ = read_u64(&mut cursor).await.unwrap();
        let _ = read_string(&mut cursor).await.unwrap();
        let msg = read_string(&mut cursor).await.unwrap();
        assert_eq!(msg, long_msg);
    }

    #[tokio::test]
    async fn write_stderr_error_empty_message() {
        let mut buf = Vec::new();
        write_stderr_error(&mut buf, "").await.unwrap();
        assert_eq!(buf.len() % 8, 0);
        let mut cursor = Cursor::new(buf);
        let _marker = read_u64(&mut cursor).await.unwrap();
        let _ty = read_string(&mut cursor).await.unwrap();
        let msg = read_string(&mut cursor).await.unwrap();
        assert!(msg.is_empty());
        let n = read_u64(&mut cursor).await.unwrap();
        assert_eq!(n, 0);
    }

    // ── property-style: pseudo-random byte buffers round-trip ─

    #[tokio::test]
    async fn write_bytes_property_pseudo_random() {
        // LCG for deterministic byte generation. Round-trips a wide
        // variety of length / content combinations.
        let mut state: u64 = 0xDEADBEEFCAFEBABE;
        let next = |s: &mut u64| -> u8 {
            *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (*s >> 33) as u8
        };

        for _ in 0..64 {
            let len = (next(&mut state) as usize) % 130; // 0..=129
            let payload: Vec<u8> = (0..len).map(|_| next(&mut state)).collect();

            let mut buf = Vec::new();
            write_bytes(&mut buf, &payload).await.unwrap();
            assert_eq!(buf.len() % 8, 0);

            let mut cursor = Cursor::new(buf);
            let got = read_bytes(&mut cursor).await.unwrap();
            assert_eq!(got, payload);
        }
    }

    #[tokio::test]
    async fn write_u64_property_pseudo_random() {
        let mut state: u64 = 0xC0FFEE_C0FFEE_u64;
        for _ in 0..256 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut buf = Vec::new();
            write_u64(&mut buf, state).await.unwrap();
            let mut cursor = Cursor::new(buf);
            assert_eq!(read_u64(&mut cursor).await.unwrap(), state);
        }
    }

    // ── interleaved sequence: bytes / u64 / string list ────────

    #[tokio::test]
    async fn interleaved_writes_round_trip_in_order() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 0xAA).await.unwrap();
        write_string(&mut buf, "alpha").await.unwrap(); // pad 3
        write_u64(&mut buf, 0xBB).await.unwrap();
        write_string_list(&mut buf, &["x".to_string(), "yy".to_string(), "zzz".to_string()])
            .await
            .unwrap();
        write_u64(&mut buf, 0xCC).await.unwrap();

        let mut cursor = Cursor::new(buf);
        assert_eq!(read_u64(&mut cursor).await.unwrap(), 0xAA);
        assert_eq!(read_string(&mut cursor).await.unwrap(), "alpha");
        assert_eq!(read_u64(&mut cursor).await.unwrap(), 0xBB);
        let count = read_u64(&mut cursor).await.unwrap() as usize;
        assert_eq!(count, 3);
        assert_eq!(read_string(&mut cursor).await.unwrap(), "x");
        assert_eq!(read_string(&mut cursor).await.unwrap(), "yy");
        assert_eq!(read_string(&mut cursor).await.unwrap(), "zzz");
        assert_eq!(read_u64(&mut cursor).await.unwrap(), 0xCC);
    }
}
