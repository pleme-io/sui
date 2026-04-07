//! Nix worker protocol wire format.
//!
//! Clean-room implementation. All integers are 64-bit unsigned, little-endian.
//! Byte buffers are length-prefixed (u64 LE) with zero-padding to 8-byte alignment.

use std::io::{self, Read, Write};
use thiserror::Error;

/// Worker protocol magic: client sends this.
pub const WORKER_MAGIC_1: u64 = 0x6e697863; // "nixc"
/// Worker protocol magic: server responds with this.
pub const WORKER_MAGIC_2: u64 = 0x6478696f; // "dxio"

/// Current protocol version (major.minor packed as u64).
pub const PROTOCOL_VERSION: u64 = (1 << 8) | 37; // 1.37

#[derive(Debug, Error)]
pub enum WireError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("unexpected magic: expected {expected:#x}, got {got:#x}")]
    BadMagic { expected: u64, got: u64 },
}

/// Worker protocol operation codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[repr(u64)]
pub enum WorkerOp {
    IsValidPath = 1,
    HasSubstitutes = 3,
    QueryPathHash = 4,
    QueryReferences = 5,
    QueryReferrers = 6,
    AddToStore = 7,
    AddTextToStore = 8,
    BuildPaths = 9,
    EnsurePath = 10,
    AddTempRoot = 11,
    AddIndirectRoot = 12,
    SyncWithGC = 13,
    FindRoots = 14,
    ExportPath = 16,
    QueryDeriver = 18,
    SetOptions = 19,
    CollectGarbage = 20,
    QuerySubstitutablePathInfo = 21,
    QueryDerivationOutputs = 22,
    QueryAllValidPaths = 23,
    QueryFailedPaths = 24,
    ClearFailedPaths = 25,
    QueryPathInfo = 26,
    ImportPaths = 27,
    QueryDerivationOutputNames = 28,
    QueryPathFromHashPart = 29,
    QuerySubstitutablePathInfos = 30,
    QueryValidPaths = 31,
    QuerySubstitutablePaths = 32,
    QueryValidDerivers = 33,
    OptimiseStore = 34,
    VerifyStore = 35,
    BuildDerivation = 36,
    AddSignatures = 37,
    NarFromPath = 38,
    AddToStoreNar = 39,
    QueryMissing = 40,
    QueryDerivationOutputMap = 41,
    RegisterDrvOutput = 42,
    QueryRealisation = 43,
    AddMultipleToStore = 44,
    AddBuildLog = 45,
}

impl WorkerOp {
    /// Parse an opcode from a u64 value.
    #[must_use]
    pub fn from_u64(v: u64) -> Option<Self> {
        Self::try_from(v).ok()
    }
}

impl TryFrom<u64> for WorkerOp {
    type Error = WireError;

    fn try_from(v: u64) -> Result<Self, Self::Error> {
        match v {
            1 => Ok(Self::IsValidPath),
            3 => Ok(Self::HasSubstitutes),
            4 => Ok(Self::QueryPathHash),
            5 => Ok(Self::QueryReferences),
            6 => Ok(Self::QueryReferrers),
            7 => Ok(Self::AddToStore),
            8 => Ok(Self::AddTextToStore),
            9 => Ok(Self::BuildPaths),
            10 => Ok(Self::EnsurePath),
            11 => Ok(Self::AddTempRoot),
            12 => Ok(Self::AddIndirectRoot),
            13 => Ok(Self::SyncWithGC),
            14 => Ok(Self::FindRoots),
            16 => Ok(Self::ExportPath),
            18 => Ok(Self::QueryDeriver),
            19 => Ok(Self::SetOptions),
            20 => Ok(Self::CollectGarbage),
            21 => Ok(Self::QuerySubstitutablePathInfo),
            22 => Ok(Self::QueryDerivationOutputs),
            23 => Ok(Self::QueryAllValidPaths),
            24 => Ok(Self::QueryFailedPaths),
            25 => Ok(Self::ClearFailedPaths),
            26 => Ok(Self::QueryPathInfo),
            27 => Ok(Self::ImportPaths),
            28 => Ok(Self::QueryDerivationOutputNames),
            29 => Ok(Self::QueryPathFromHashPart),
            30 => Ok(Self::QuerySubstitutablePathInfos),
            31 => Ok(Self::QueryValidPaths),
            32 => Ok(Self::QuerySubstitutablePaths),
            33 => Ok(Self::QueryValidDerivers),
            34 => Ok(Self::OptimiseStore),
            35 => Ok(Self::VerifyStore),
            36 => Ok(Self::BuildDerivation),
            37 => Ok(Self::AddSignatures),
            38 => Ok(Self::NarFromPath),
            39 => Ok(Self::AddToStoreNar),
            40 => Ok(Self::QueryMissing),
            41 => Ok(Self::QueryDerivationOutputMap),
            42 => Ok(Self::RegisterDrvOutput),
            43 => Ok(Self::QueryRealisation),
            44 => Ok(Self::AddMultipleToStore),
            45 => Ok(Self::AddBuildLog),
            _ => Err(WireError::Protocol(format!("unknown worker op: {v}"))),
        }
    }
}

/// Stderr message types sent by the daemon during operation processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[repr(u64)]
pub enum StderrMsg {
    /// Write a string to stderr.
    Write = 0x6f6c6d67,      // "olmg"
    /// End of stderr stream — followed by the actual response.
    Last = 0x616c7473,       // "alts"
    /// Error message.
    Error = 0x63787470,      // "cxtp"
    /// Start activity.
    StartActivity = 0x53545254, // "STRT"
    /// Stop activity.
    StopActivity = 0x53544f50, // "STOP"
    /// Activity result.
    Result = 0x52534c54,     // "RSLT"
}

// ── Wire primitives ──────────────────────────────────────────

/// Write a u64 in little-endian.
pub fn write_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Read a u64 in little-endian.
pub fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

/// Zero buffer for 8-byte alignment padding (max 7 bytes needed).
const PADDING: [u8; 8] = [0u8; 8];

/// Write a length-prefixed, 8-byte-aligned byte buffer.
pub fn write_bytes(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    write_u64(w, data.len() as u64)?;
    w.write_all(data)?;
    let pad = (8 - (data.len() % 8)) % 8;
    if pad > 0 {
        w.write_all(&PADDING[..pad])?;
    }
    Ok(())
}

/// Read a length-prefixed, 8-byte-aligned byte buffer.
pub fn read_bytes(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let len = read_u64(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let pad = (8 - (len % 8)) % 8;
    if pad > 0 {
        let mut pad_buf = [0u8; 8];
        r.read_exact(&mut pad_buf[..pad])?;
    }
    Ok(buf)
}

/// Write a UTF-8 string (as length-prefixed bytes).
pub fn write_string(w: &mut impl Write, s: impl AsRef<str>) -> io::Result<()> {
    write_bytes(w, s.as_ref().as_bytes())
}

/// Read a UTF-8 string.
pub fn read_string(r: &mut impl Read) -> Result<String, WireError> {
    let bytes = read_bytes(r)?;
    String::from_utf8(bytes).map_err(|e| WireError::Protocol(format!("invalid UTF-8: {e}")))
}

/// Write a bool (as u64: 0 or 1).
pub fn write_bool(w: &mut impl Write, v: bool) -> io::Result<()> {
    write_u64(w, u64::from(v))
}

/// Read a bool (from u64: 0 or 1).
pub fn read_bool(r: &mut impl Read) -> io::Result<bool> {
    Ok(read_u64(r)? != 0)
}

/// Write a list of strings.
pub fn write_string_list(w: &mut impl Write, list: &[impl AsRef<str>]) -> io::Result<()> {
    write_u64(w, list.len() as u64)?;
    for s in list {
        write_string(w, s)?;
    }
    Ok(())
}

/// Read a list of strings.
pub fn read_string_list(r: &mut impl Read) -> Result<Vec<String>, WireError> {
    let count = read_u64(r)? as usize;
    (0..count).map(|_| read_string(r)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn u64_roundtrip() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 42).unwrap();
        assert_eq!(buf.len(), 8);
        let v = read_u64(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(v, 42);
    }

    #[test]
    fn bytes_roundtrip() {
        let data = b"hello";
        let mut buf = Vec::new();
        write_bytes(&mut buf, data).unwrap();
        // 8 (length) + 5 (data) + 3 (padding) = 16
        assert_eq!(buf.len(), 16);
        let read = read_bytes(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, data);
    }

    #[test]
    fn string_roundtrip() {
        let s = "hello world";
        let mut buf = Vec::new();
        write_string(&mut buf, s).unwrap();
        let read = read_string(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, s);
    }

    #[test]
    fn bool_roundtrip() {
        for v in [true, false] {
            let mut buf = Vec::new();
            write_bool(&mut buf, v).unwrap();
            let read = read_bool(&mut Cursor::new(&buf)).unwrap();
            assert_eq!(read, v);
        }
    }

    #[test]
    fn string_list_roundtrip() {
        let list = vec!["foo".to_string(), "bar".to_string(), "baz".to_string()];
        let mut buf = Vec::new();
        write_string_list(&mut buf, &list).unwrap();
        let read = read_string_list(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, list);
    }

    #[test]
    fn empty_string() {
        let mut buf = Vec::new();
        write_string(&mut buf, "").unwrap();
        assert_eq!(buf.len(), 8); // just the length field
        let read = read_string(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, "");
    }

    #[test]
    fn worker_op_from_u64() {
        assert_eq!(WorkerOp::from_u64(1), Some(WorkerOp::IsValidPath));
        assert_eq!(WorkerOp::from_u64(26), Some(WorkerOp::QueryPathInfo));
        assert_eq!(WorkerOp::from_u64(9999), None);
    }

    #[test]
    fn magic_constants() {
        // Verify magic bytes match the ASCII representations
        assert_eq!(&WORKER_MAGIC_1.to_le_bytes()[..4], b"cxin");
        assert_eq!(&WORKER_MAGIC_2.to_le_bytes()[..4], b"oixd");
    }

    #[test]
    fn eight_byte_alignment() {
        // "abc" is 3 bytes, needs 5 bytes padding
        let mut buf = Vec::new();
        write_bytes(&mut buf, b"abc").unwrap();
        assert_eq!(buf.len() % 8, 0);
        assert_eq!(buf.len(), 16); // 8 (len) + 3 (data) + 5 (pad)
    }

    #[test]
    fn large_string_roundtrip() {
        // 1KB+ string
        let s: String = "abcdefghij".repeat(120); // 1200 bytes
        assert!(s.len() >= 1024);
        let mut buf = Vec::new();
        write_string(&mut buf, &s).unwrap();
        assert_eq!(buf.len() % 8, 0);
        let read = read_string(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, s);
    }

    #[test]
    fn empty_list_roundtrip() {
        let list: Vec<String> = vec![];
        let mut buf = Vec::new();
        write_string_list(&mut buf, &list).unwrap();
        // Should be just the count (0) as u64
        assert_eq!(buf.len(), 8);
        let read = read_string_list(&mut Cursor::new(&buf)).unwrap();
        assert!(read.is_empty());
    }

    #[test]
    fn binary_data_with_zero_bytes() {
        let data: Vec<u8> = vec![0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x01, 0x00];
        let mut buf = Vec::new();
        write_bytes(&mut buf, &data).unwrap();
        let read = read_bytes(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, data);
    }

    #[test]
    fn protocol_version_constant() {
        // Protocol version 1.37: major=1 in high byte, minor=37 in low byte
        assert_eq!(PROTOCOL_VERSION, (1 << 8) | 37);
        assert_eq!(PROTOCOL_VERSION, 293);
        // Extract major and minor
        let major = PROTOCOL_VERSION >> 8;
        let minor = PROTOCOL_VERSION & 0xFF;
        assert_eq!(major, 1);
        assert_eq!(minor, 37);
    }

    #[test]
    fn large_u64_roundtrip() {
        let mut buf = Vec::new();
        write_u64(&mut buf, u64::MAX).unwrap();
        let v = read_u64(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(v, u64::MAX);
    }

    #[test]
    fn bytes_exact_8_byte_size() {
        // 8 bytes exactly: no padding needed
        let data = b"12345678";
        let mut buf = Vec::new();
        write_bytes(&mut buf, data).unwrap();
        // 8 (length) + 8 (data) + 0 (no padding) = 16
        assert_eq!(buf.len(), 16);
        let read = read_bytes(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, data);
    }

    #[test]
    fn worker_op_all_valid_codes() {
        let valid_ops: Vec<(u64, WorkerOp)> = vec![
            (1, WorkerOp::IsValidPath),
            (7, WorkerOp::AddToStore),
            (9, WorkerOp::BuildPaths),
            (26, WorkerOp::QueryPathInfo),
            (38, WorkerOp::NarFromPath),
            (45, WorkerOp::AddBuildLog),
        ];
        for (code, expected) in valid_ops {
            assert_eq!(WorkerOp::from_u64(code), Some(expected));
        }
    }

    #[test]
    fn worker_op_invalid_codes() {
        assert_eq!(WorkerOp::from_u64(0), None);
        assert_eq!(WorkerOp::from_u64(2), None);
        assert_eq!(WorkerOp::from_u64(15), None);
        assert_eq!(WorkerOp::from_u64(17), None);
        assert_eq!(WorkerOp::from_u64(46), None);
        assert_eq!(WorkerOp::from_u64(u64::MAX), None);
    }

    // ── Additional wire primitive tests ──────────────────

    #[test]
    fn u64_zero_roundtrip() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 0).unwrap();
        let v = read_u64(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(v, 0);
    }

    #[test]
    fn bytes_empty_roundtrip() {
        let mut buf = Vec::new();
        write_bytes(&mut buf, &[]).unwrap();
        assert_eq!(buf.len(), 8);
        let read = read_bytes(&mut Cursor::new(&buf)).unwrap();
        assert!(read.is_empty());
    }

    #[test]
    fn string_with_unicode() {
        let s = "日本語テスト 🎉";
        let mut buf = Vec::new();
        write_string(&mut buf, s).unwrap();
        assert_eq!(buf.len() % 8, 0);
        let read = read_string(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, s);
    }

    #[test]
    fn string_list_single_entry() {
        let list = vec!["only-one".to_string()];
        let mut buf = Vec::new();
        write_string_list(&mut buf, &list).unwrap();
        let read = read_string_list(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, list);
    }

    #[test]
    fn string_list_many_entries() {
        let list: Vec<String> = (0..100).map(|i| format!("item-{i}")).collect();
        let mut buf = Vec::new();
        write_string_list(&mut buf, &list).unwrap();
        let read = read_string_list(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, list);
    }

    #[test]
    fn worker_op_exhaustive_coverage() {
        let all: Vec<(u64, WorkerOp)> = vec![
            (1, WorkerOp::IsValidPath),
            (3, WorkerOp::HasSubstitutes),
            (4, WorkerOp::QueryPathHash),
            (5, WorkerOp::QueryReferences),
            (6, WorkerOp::QueryReferrers),
            (7, WorkerOp::AddToStore),
            (8, WorkerOp::AddTextToStore),
            (9, WorkerOp::BuildPaths),
            (10, WorkerOp::EnsurePath),
            (11, WorkerOp::AddTempRoot),
            (12, WorkerOp::AddIndirectRoot),
            (13, WorkerOp::SyncWithGC),
            (14, WorkerOp::FindRoots),
            (16, WorkerOp::ExportPath),
            (18, WorkerOp::QueryDeriver),
            (19, WorkerOp::SetOptions),
            (20, WorkerOp::CollectGarbage),
            (21, WorkerOp::QuerySubstitutablePathInfo),
            (22, WorkerOp::QueryDerivationOutputs),
            (23, WorkerOp::QueryAllValidPaths),
            (24, WorkerOp::QueryFailedPaths),
            (25, WorkerOp::ClearFailedPaths),
            (26, WorkerOp::QueryPathInfo),
            (27, WorkerOp::ImportPaths),
            (28, WorkerOp::QueryDerivationOutputNames),
            (29, WorkerOp::QueryPathFromHashPart),
            (30, WorkerOp::QuerySubstitutablePathInfos),
            (31, WorkerOp::QueryValidPaths),
            (32, WorkerOp::QuerySubstitutablePaths),
            (33, WorkerOp::QueryValidDerivers),
            (34, WorkerOp::OptimiseStore),
            (35, WorkerOp::VerifyStore),
            (36, WorkerOp::BuildDerivation),
            (37, WorkerOp::AddSignatures),
            (38, WorkerOp::NarFromPath),
            (39, WorkerOp::AddToStoreNar),
            (40, WorkerOp::QueryMissing),
            (41, WorkerOp::QueryDerivationOutputMap),
            (42, WorkerOp::RegisterDrvOutput),
            (43, WorkerOp::QueryRealisation),
            (44, WorkerOp::AddMultipleToStore),
            (45, WorkerOp::AddBuildLog),
        ];
        for (code, expected) in &all {
            assert_eq!(WorkerOp::from_u64(*code), Some(*expected), "failed for code {code}");
        }
        assert_eq!(all.len(), 42);
    }

    #[test]
    fn read_u64_truncated_input() {
        let result = read_u64(&mut Cursor::new(&[0u8; 4]));
        assert!(result.is_err());
    }

    #[test]
    fn read_bytes_truncated_data() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 100).unwrap();
        buf.extend_from_slice(&[0u8; 10]);
        let result = read_bytes(&mut Cursor::new(&buf));
        assert!(result.is_err());
    }

    // ── Alignment edge cases for write_bytes ─────────────

    #[test]
    fn write_bytes_alignment_for_each_residue() {
        // Lengths 0..16 — verify the output is always aligned to 8 bytes
        for len in 0..16 {
            let data = vec![0xAA_u8; len];
            let mut buf = Vec::new();
            write_bytes(&mut buf, &data).unwrap();
            assert_eq!(buf.len() % 8, 0, "len={len} not 8-byte aligned");
            // Total size = 8 (length prefix) + ceil(len/8)*8 (data + padding)
            let aligned_data_size = ((len + 7) / 8) * 8;
            assert_eq!(buf.len(), 8 + aligned_data_size);
        }
    }

    #[test]
    fn write_bytes_padding_is_zeros() {
        // 5 bytes → 3 padding bytes which must be zero
        let mut buf = Vec::new();
        write_bytes(&mut buf, b"hello").unwrap();
        // Layout: [u64 len = 5][5 data bytes][3 zero pad]
        assert_eq!(&buf[8..13], b"hello");
        assert_eq!(&buf[13..16], &[0u8, 0u8, 0u8]);
    }

    #[test]
    fn read_bytes_alignment_for_each_residue() {
        for len in 0_usize..16 {
            let data = vec![0xCC_u8; len];
            let mut buf = Vec::new();
            write_bytes(&mut buf, &data).unwrap();
            let read = read_bytes(&mut Cursor::new(&buf)).unwrap();
            assert_eq!(read, data, "roundtrip failed for len={len}");
        }
    }

    // ── read_string error path ───────────────────────────

    #[test]
    fn read_string_invalid_utf8_returns_protocol_error() {
        let mut buf = Vec::new();
        // Invalid UTF-8 sequence: 0xff is never a valid UTF-8 start byte
        write_bytes(&mut buf, &[0xFF, 0xFE, 0xFD]).unwrap();
        let result = read_string(&mut Cursor::new(&buf));
        assert!(result.is_err());
        match result {
            Err(WireError::Protocol(_)) => {}
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    // ── TryFrom<u64> for WorkerOp ────────────────────────

    #[test]
    fn worker_op_try_from_known() {
        let op: WorkerOp = WorkerOp::try_from(26).unwrap();
        assert_eq!(op, WorkerOp::QueryPathInfo);
    }

    #[test]
    fn worker_op_try_from_unknown_returns_protocol_error() {
        let result = WorkerOp::try_from(999u64);
        assert!(result.is_err());
        match result {
            Err(WireError::Protocol(s)) => assert!(s.contains("999")),
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    // ── StderrMsg constants ──────────────────────────────

    #[test]
    fn stderr_msg_constants_distinct() {
        let codes = [
            StderrMsg::Write as u64,
            StderrMsg::Last as u64,
            StderrMsg::Error as u64,
            StderrMsg::StartActivity as u64,
            StderrMsg::StopActivity as u64,
            StderrMsg::Result as u64,
        ];
        // All codes must be distinct
        for i in 0..codes.len() {
            for j in (i + 1)..codes.len() {
                assert_ne!(codes[i], codes[j]);
            }
        }
    }

    #[test]
    fn stderr_msg_eq_clone_copy() {
        let a = StderrMsg::Write;
        let b = a;
        let c = a;
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    // ── WORKER_MAGIC + PROTOCOL_VERSION constants ───────

    #[test]
    fn worker_magic_constants_distinct() {
        assert_ne!(WORKER_MAGIC_1, WORKER_MAGIC_2);
    }

    // ── read_bool truthiness ─────────────────────────────

    #[test]
    fn read_bool_nonzero_is_true() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 42).unwrap();
        let v = read_bool(&mut Cursor::new(&buf)).unwrap();
        assert!(v);
    }

    #[test]
    fn read_bool_zero_is_false() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 0).unwrap();
        let v = read_bool(&mut Cursor::new(&buf)).unwrap();
        assert!(!v);
    }

    // ── read_string_list truncation ──────────────────────

    #[test]
    fn read_string_list_truncated_count_returns_error() {
        let buf = [0u8; 4];
        let result = read_string_list(&mut Cursor::new(&buf));
        assert!(result.is_err());
    }

    #[test]
    fn read_string_list_count_exceeds_data_returns_error() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 100).unwrap(); // claim 100 strings
        let result = read_string_list(&mut Cursor::new(&buf));
        assert!(result.is_err());
    }

    // ── write_string_list with mixed-length entries ─────

    #[test]
    fn write_string_list_mixed_lengths() {
        let list = vec![
            String::new(),
            "a".to_string(),
            "ab".to_string(),
            "abcdefgh".to_string(),
            "abcdefghi".to_string(), // crosses 8-byte boundary
        ];
        let mut buf = Vec::new();
        write_string_list(&mut buf, &list).unwrap();
        let read = read_string_list(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, list);
    }

    // ── write_string accepts &str + String + &String ────

    #[test]
    fn write_string_accepts_str_types() {
        let mut buf1 = Vec::new();
        write_string(&mut buf1, "hello").unwrap();

        let mut buf2 = Vec::new();
        write_string(&mut buf2, String::from("hello")).unwrap();

        let mut buf3 = Vec::new();
        let s = String::from("hello");
        write_string(&mut buf3, &s).unwrap();

        assert_eq!(buf1, buf2);
        assert_eq!(buf2, buf3);
    }

    // ── Length-prefix at exact byte boundaries ──────────

    #[test]
    fn bytes_length_seven_padding() {
        // 7 bytes → needs 1 byte padding
        let data = b"1234567";
        let mut buf = Vec::new();
        write_bytes(&mut buf, data).unwrap();
        // 8 (len) + 7 (data) + 1 (pad) = 16
        assert_eq!(buf.len(), 16);
        let read = read_bytes(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, data);
    }

    #[test]
    fn bytes_length_one_padding() {
        // 1 byte → needs 7 bytes padding
        let data = b"x";
        let mut buf = Vec::new();
        write_bytes(&mut buf, data).unwrap();
        // 8 + 1 + 7 = 16
        assert_eq!(buf.len(), 16);
        let read = read_bytes(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, data);
    }

    #[test]
    fn bytes_length_nine_padding() {
        // 9 bytes → 7 bytes padding (mod 8 = 1)
        let data = b"123456789";
        let mut buf = Vec::new();
        write_bytes(&mut buf, data).unwrap();
        // 8 + 9 + 7 = 24
        assert_eq!(buf.len(), 24);
        let read = read_bytes(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(read, data);
    }

    // ── u64 endianness ───────────────────────────────────

    #[test]
    fn u64_written_in_little_endian() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 0x0102_0304_0506_0708).unwrap();
        // LE: low byte first
        assert_eq!(buf, vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    // ── PROTOCOL_VERSION extraction ─────────────────────

    #[test]
    fn protocol_version_major_minor_extraction() {
        let major = PROTOCOL_VERSION >> 8;
        let minor = PROTOCOL_VERSION & 0xFF;
        assert_eq!(major, 1);
        assert_eq!(minor, 37);
    }

    // ── WorkerOp from_u64 vs try_from agreement ─────────

    #[test]
    fn worker_op_from_u64_matches_try_from() {
        for code in [1, 3, 7, 26, 38, 45] {
            let from_u64 = WorkerOp::from_u64(code);
            let try_from = WorkerOp::try_from(code).ok();
            assert_eq!(from_u64, try_from);
        }
        // Both reject unknown codes
        assert_eq!(WorkerOp::from_u64(0), None);
        assert!(WorkerOp::try_from(0u64).is_err());
    }

    // ── Error display ────────────────────────────────────

    #[test]
    fn wire_error_protocol_display() {
        let err = WireError::Protocol("custom message".to_string());
        let s = format!("{err}");
        assert!(s.contains("custom message"));
    }

    #[test]
    fn wire_error_bad_magic_display() {
        let err = WireError::BadMagic { expected: 0x1234, got: 0x5678 };
        let s = format!("{err}");
        assert!(s.contains("0x1234"));
        assert!(s.contains("0x5678"));
    }

    // ── Empty bytes via wire ─────────────────────────────

    #[test]
    fn read_bytes_empty_via_zero_length() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 0).unwrap();
        let read = read_bytes(&mut Cursor::new(&buf)).unwrap();
        assert!(read.is_empty());
    }

    // ── Multiple sequential reads on one cursor ─────────

    #[test]
    fn multiple_writes_then_reads_in_order() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 11).unwrap();
        write_string(&mut buf, "first").unwrap();
        write_u64(&mut buf, 22).unwrap();
        write_string(&mut buf, "second").unwrap();
        write_bool(&mut buf, true).unwrap();

        let mut cur = Cursor::new(&buf);
        assert_eq!(read_u64(&mut cur).unwrap(), 11);
        assert_eq!(read_string(&mut cur).unwrap(), "first");
        assert_eq!(read_u64(&mut cur).unwrap(), 22);
        assert_eq!(read_string(&mut cur).unwrap(), "second");
        assert!(read_bool(&mut cur).unwrap());
    }
}
