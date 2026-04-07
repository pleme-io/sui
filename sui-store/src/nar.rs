//! NAR decompression — supports xz, zstd, and uncompressed NAR data.

use std::io::Read;

use crate::traits::{StoreError, StoreResult};

/// Decompress NAR data based on the compression algorithm.
///
/// Supports `"xz"`, `"zstd"`, `"none"` (or empty string for uncompressed).
/// Returns the decompressed NAR bytes.
pub fn decompress_nar(data: &[u8], compression: &str) -> StoreResult<Vec<u8>> {
    match compression {
        "xz" => {
            let mut decoder = xz2::read::XzDecoder::new(data);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed).map_err(|e| {
                StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("xz decompression failed: {e}"),
                ))
            })?;
            Ok(decompressed)
        }
        "zstd" => {
            let mut decoder = zstd::stream::read::Decoder::new(data).map_err(|e| {
                StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("zstd decoder init failed: {e}"),
                ))
            })?;
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed).map_err(|e| {
                StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("zstd decompression failed: {e}"),
                ))
            })?;
            Ok(decompressed)
        }
        "none" | "" => Ok(data.to_vec()),
        other => Err(StoreError::NotSupported(format!(
            "compression: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Create a small NAR archive in memory (using sui_compat).
    fn make_small_nar() -> Vec<u8> {
        use sui_compat::nar::{NarNode, NarWriter};
        let node = NarNode::Regular {
            executable: false,
            contents: b"hello from nar".to_vec(),
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        buf
    }

    #[test]
    fn decompress_none_returns_data_unchanged() {
        let data = b"raw nar data";
        let result = decompress_nar(data, "none").unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn decompress_empty_string_returns_data_unchanged() {
        let data = b"raw nar data";
        let result = decompress_nar(data, "").unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn decompress_unknown_compression_returns_error() {
        let result = decompress_nar(b"data", "brotli");
        assert!(result.is_err());
        match result {
            Err(StoreError::NotSupported(msg)) => {
                assert!(msg.contains("brotli"));
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    #[test]
    fn decompress_xz_roundtrip() {
        let original = make_small_nar();

        // Compress with xz.
        let mut compressed = Vec::new();
        let mut encoder = xz2::write::XzEncoder::new(&mut compressed, 6);
        encoder.write_all(&original).unwrap();
        encoder.finish().unwrap();

        // Decompress.
        let decompressed = decompress_nar(&compressed, "xz").unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn decompress_zstd_roundtrip() {
        let original = make_small_nar();

        // Compress with zstd.
        let compressed = zstd::encode_all(original.as_slice(), 3).unwrap();

        // Decompress.
        let decompressed = decompress_nar(&compressed, "zstd").unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn decompress_xz_invalid_data_returns_error() {
        let result = decompress_nar(b"not valid xz data", "xz");
        assert!(result.is_err());
    }

    #[test]
    fn decompress_zstd_invalid_data_returns_error() {
        let result = decompress_nar(b"not valid zstd data", "zstd");
        assert!(result.is_err());
    }

    #[test]
    fn decompress_xz_empty_data() {
        // Empty xz stream is invalid.
        let result = decompress_nar(b"", "xz");
        assert!(result.is_err());
    }

    #[test]
    fn decompress_zstd_empty_data() {
        // Empty zstd stream is invalid.
        let result = decompress_nar(b"", "zstd");
        assert!(result.is_err());
    }

    #[test]
    fn decompress_none_preserves_empty_data() {
        let result = decompress_nar(b"", "none").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn decompress_xz_large_roundtrip() {
        // Create a larger payload to exercise buffered decompression.
        let original: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();

        let mut compressed = Vec::new();
        let mut encoder = xz2::write::XzEncoder::new(&mut compressed, 3);
        encoder.write_all(&original).unwrap();
        encoder.finish().unwrap();

        let decompressed = decompress_nar(&compressed, "xz").unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn decompress_zstd_large_roundtrip() {
        let original: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let compressed = zstd::encode_all(original.as_slice(), 3).unwrap();
        let decompressed = decompress_nar(&compressed, "zstd").unwrap();
        assert_eq!(decompressed, original);
    }
}
