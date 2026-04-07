//! NAR (Nix Archive) format — backed by the `nix-nar` crate.
//!
//! This module wraps `nix-nar` for filesystem operations and provides
//! an in-memory `NarNode` tree for programmatic NAR construction and testing.
//! The wire-level serialization (8-byte aligned, length-prefixed strings)
//! is implemented here for cases where we need raw NAR bytes without
//! touching the filesystem.

use std::io::{self, Read, Write};
use std::path::Path;

use crate::wire;
use thiserror::Error;

/// NAR magic header.
pub const NAR_MAGIC: &str = "nix-archive-1";

/// Hard cap on a single length-prefixed string in a NAR. CppNix
/// allows arbitrarily large NARs, but a single *string* (filename,
/// type token, file contents) above this size is almost certainly
/// either a corrupted file or a fuzz-input attack on the parser.
/// Allocating multi-exabyte buffers triggers `abort()` from the
/// system allocator, which `catch_unwind` cannot contain — so we
/// reject up front instead. 4 GiB is a generous cap for any real
/// file we'd encounter in `/nix/store`.
pub const MAX_NAR_STRING: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum NarError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid NAR: {0}")]
    Invalid(String),
    #[error("unexpected token: expected {expected}, got {got}")]
    UnexpectedToken { expected: String, got: String },
}

// ── Wire primitives ──────────────────────────────────────────
//
// u64 and length-prefixed byte framing is shared with the worker
// protocol in `crate::wire`. NAR adds a max-string-length cap
// to defend against allocation bombs.

fn write_str(w: &mut impl Write, s: &[u8]) -> io::Result<()> {
    wire::write_bytes(w, s)
}

fn read_str(r: &mut impl Read) -> Result<Vec<u8>, NarError> {
    let len_u64 = wire::read_u64(r)?;
    if len_u64 > MAX_NAR_STRING {
        return Err(NarError::Invalid(format!(
            "nar string too long: {len_u64} bytes exceeds {MAX_NAR_STRING} cap"
        )));
    }
    let len = len_u64 as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let pad = (8 - (len % 8)) % 8;
    if pad > 0 {
        let mut pad_buf = vec![0u8; pad];
        r.read_exact(&mut pad_buf)?;
    }
    Ok(buf)
}

fn read_str_utf8(r: &mut impl Read) -> Result<String, NarError> {
    let bytes = read_str(r)?;
    String::from_utf8(bytes).map_err(|e| NarError::Invalid(format!("invalid UTF-8: {e}")))
}

fn expect_str(r: &mut impl Read, expected: &str) -> Result<(), NarError> {
    let got = read_str_utf8(r)?;
    if got != expected {
        return Err(NarError::UnexpectedToken {
            expected: expected.to_string(),
            got,
        });
    }
    Ok(())
}

// ── In-memory NAR node types ─────────────────────────────────

/// A node in a NAR archive tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NarNode {
    /// A regular file with optional executable permission.
    Regular {
        /// Whether the file has the executable bit set.
        executable: bool,
        /// Raw file contents.
        contents: Vec<u8>,
    },
    /// A symbolic link.
    Symlink {
        /// The symlink target path.
        target: String,
    },
    /// A directory containing named entries.
    Directory {
        /// Sorted list of directory entries.
        entries: Vec<NarEntry>,
    },
}

/// A named entry within a NAR directory node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarEntry {
    /// Entry filename (relative, no path separators).
    pub name: String,
    /// The file, symlink, or subdirectory at this entry.
    pub node: NarNode,
}

// ── Writer ───────────────────────────────────────────────────

/// Serialize a NAR node tree to a writer.
pub struct NarWriter;

impl NarWriter {
    /// Write a complete NAR archive from an in-memory tree.
    pub fn write(w: &mut impl Write, node: &NarNode) -> Result<(), NarError> {
        write_str(w, NAR_MAGIC.as_bytes())?;
        Self::write_node(w, node)?;
        Ok(())
    }

    fn write_node(w: &mut impl Write, node: &NarNode) -> Result<(), NarError> {
        write_str(w, b"(")?;
        match node {
            NarNode::Regular { executable, contents } => {
                write_str(w, b"type")?;
                write_str(w, b"regular")?;
                if *executable {
                    write_str(w, b"executable")?;
                    write_str(w, b"")?;
                }
                write_str(w, b"contents")?;
                write_str(w, contents)?;
            }
            NarNode::Symlink { target } => {
                write_str(w, b"type")?;
                write_str(w, b"symlink")?;
                write_str(w, b"target")?;
                write_str(w, target.as_bytes())?;
            }
            NarNode::Directory { entries } => {
                write_str(w, b"type")?;
                write_str(w, b"directory")?;
                for entry in entries {
                    write_str(w, b"entry")?;
                    write_str(w, b"(")?;
                    write_str(w, b"name")?;
                    write_str(w, entry.name.as_bytes())?;
                    write_str(w, b"node")?;
                    Self::write_node(w, &entry.node)?;
                    write_str(w, b")")?;
                }
            }
        }
        write_str(w, b")")?;
        Ok(())
    }

    /// Serialize a filesystem path to NAR format using `nix-nar`.
    pub fn write_path(w: &mut impl Write, path: &Path) -> Result<(), NarError> {
        let encoder = nix_nar::Encoder::new(path)
            .map_err(|e| NarError::Invalid(format!("nix-nar encoder error: {e}")))?;
        let mut reader = std::io::BufReader::new(encoder);
        std::io::copy(&mut reader, w)?;
        Ok(())
    }
}

// ── Reader ───────────────────────────────────────────────────

/// Deserialize a NAR archive from a reader.
pub struct NarReader;

impl NarReader {
    /// Read a complete NAR archive into an in-memory tree.
    pub fn read_complete(r: &mut impl Read) -> Result<NarNode, NarError> {
        expect_str(r, NAR_MAGIC)?;
        Self::read_node(r)
    }

    fn read_node(r: &mut impl Read) -> Result<NarNode, NarError> {
        expect_str(r, "(")?;
        expect_str(r, "type")?;
        let node_type = read_str_utf8(r)?;

        match node_type.as_str() {
            "regular" => {
                let node = Self::read_regular(r)?;
                expect_str(r, ")")?;
                Ok(node)
            }
            "symlink" => {
                let node = Self::read_symlink(r)?;
                expect_str(r, ")")?;
                Ok(node)
            }
            "directory" => Self::read_directory(r),
            _ => Err(NarError::Invalid(format!("unknown node type: {node_type}"))),
        }
    }

    fn read_regular(r: &mut impl Read) -> Result<NarNode, NarError> {
        let mut executable = false;
        let token = read_str_utf8(r)?;
        if token == "executable" {
            executable = true;
            read_str(r)?;
            expect_str(r, "contents")?;
        } else if token != "contents" {
            return Err(NarError::UnexpectedToken {
                expected: "executable or contents".to_string(),
                got: token,
            });
        }
        let contents = read_str(r)?;
        Ok(NarNode::Regular { executable, contents })
    }

    fn read_symlink(r: &mut impl Read) -> Result<NarNode, NarError> {
        expect_str(r, "target")?;
        let target = read_str_utf8(r)?;
        Ok(NarNode::Symlink { target })
    }

    fn read_directory(r: &mut impl Read) -> Result<NarNode, NarError> {
        let mut entries = Vec::new();
        loop {
            let token = read_str_utf8(r)?;
            if token == ")" {
                return Ok(NarNode::Directory { entries });
            }
            if token != "entry" {
                return Err(NarError::UnexpectedToken {
                    expected: "entry or )".to_string(),
                    got: token,
                });
            }
            expect_str(r, "(")?;
            expect_str(r, "name")?;
            let name = read_str_utf8(r)?;
            expect_str(r, "node")?;
            let node = Self::read_node(r)?;
            expect_str(r, ")")?;
            entries.push(NarEntry { name, node });
        }
    }
}

/// Unpack a NAR archive to a filesystem path using `nix-nar`.
pub fn unpack_nar(nar_data: &[u8], dest: &Path) -> Result<(), NarError> {
    let decoder = nix_nar::Decoder::new(std::io::Cursor::new(nar_data))
        .map_err(|e| NarError::Invalid(format!("nix-nar decoder error: {e}")))?;
    decoder
        .unpack(dest)
        .map_err(|e| NarError::Invalid(format!("nix-nar unpack error: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_regular_file() {
        let node = NarNode::Regular { executable: false, contents: b"hello world".to_vec() };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn roundtrip_executable() {
        let node = NarNode::Regular { executable: true, contents: b"#!/bin/sh\necho hi".to_vec() };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn roundtrip_symlink() {
        let node = NarNode::Symlink { target: "/nix/store/abc-foo/bin/foo".to_string() };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn roundtrip_directory() {
        let node = NarNode::Directory {
            entries: vec![
                NarEntry { name: "bar".to_string(), node: NarNode::Regular { executable: false, contents: b"bar".to_vec() } },
                NarEntry { name: "foo".to_string(), node: NarNode::Symlink { target: "bar".to_string() } },
            ],
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn roundtrip_nested_directory() {
        let node = NarNode::Directory {
            entries: vec![
                NarEntry { name: "bin".to_string(), node: NarNode::Directory {
                    entries: vec![NarEntry { name: "hello".to_string(), node: NarNode::Regular { executable: true, contents: b"ELF".to_vec() } }],
                }},
                NarEntry { name: "lib".to_string(), node: NarNode::Directory { entries: vec![] } },
            ],
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn nar_magic_header() {
        let node = NarNode::Regular { executable: false, contents: vec![] };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let len = u64::from_le_bytes(buf[..8].try_into().unwrap());
        assert_eq!(len, 13);
        assert_eq!(&buf[8..21], NAR_MAGIC.as_bytes());
    }

    #[test]
    fn eight_byte_alignment() {
        let node = NarNode::Regular { executable: false, contents: b"hello".to_vec() };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        assert_eq!(buf.len() % 8, 0);
    }

    #[test]
    fn empty_file() {
        let node = NarNode::Regular { executable: false, contents: vec![] };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn empty_directory() {
        let node = NarNode::Directory { entries: vec![] };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn very_large_file_content() {
        let contents = vec![0xAB; 1_000_000];
        let node = NarNode::Regular { executable: false, contents: contents.clone() };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn deeply_nested_5_levels() {
        let leaf = NarNode::Regular { executable: false, contents: b"deep".to_vec() };
        let mut node = leaf;
        for i in (0..5).rev() {
            node = NarNode::Directory {
                entries: vec![NarEntry { name: format!("level{i}"), node }],
            };
        }
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn directory_with_many_entries() {
        let entries: Vec<NarEntry> = (0..60).map(|i| NarEntry {
            name: format!("file-{i:03}"),
            node: NarNode::Regular { executable: false, contents: format!("content {i}").into_bytes() },
        }).collect();
        let node = NarNode::Directory { entries };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn write_path_on_real_temp_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"Hello!").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("nested.txt"), b"nested").unwrap();

        let mut buf = Vec::new();
        NarWriter::write_path(&mut buf, dir.path()).unwrap();
        assert!(buf.len() > 20);
        assert_eq!(buf.len() % 8, 0);
    }

    #[test]
    fn mixed_node_types() {
        let node = NarNode::Directory {
            entries: vec![
                NarEntry { name: "exec".to_string(), node: NarNode::Regular { executable: true, contents: b"#!/bin/sh".to_vec() } },
                NarEntry { name: "link".to_string(), node: NarNode::Symlink { target: "exec".to_string() } },
                NarEntry { name: "reg".to_string(), node: NarNode::Regular { executable: false, contents: b"data".to_vec() } },
                NarEntry { name: "sub".to_string(), node: NarNode::Directory { entries: vec![] } },
            ],
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    // ── Executable empty file ────────────────────────────

    #[test]
    fn roundtrip_executable_empty_file() {
        let node = NarNode::Regular { executable: true, contents: vec![] };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        assert_eq!(buf.len() % 8, 0);
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    // ── Large symlink target ─────────────────────────────

    #[test]
    fn roundtrip_large_symlink_target() {
        let long_target = "/nix/store/".to_string() + &"a".repeat(500) + "-long-package/lib/libfoo.so.1.2.3";
        let node = NarNode::Symlink { target: long_target };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        assert_eq!(buf.len() % 8, 0);
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    // ── Deeply nested directories (10+ levels) ──────────

    #[test]
    fn deeply_nested_10_levels() {
        let leaf = NarNode::Regular { executable: false, contents: b"leaf data".to_vec() };
        let mut node = leaf;
        for i in (0..10).rev() {
            node = NarNode::Directory {
                entries: vec![NarEntry { name: format!("d{i}"), node }],
            };
        }
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    // ── Deeply nested with multiple entries at each level ──

    #[test]
    fn deeply_nested_with_siblings() {
        let leaf_file = NarNode::Regular { executable: false, contents: b"f".to_vec() };
        let leaf_link = NarNode::Symlink { target: "f".to_string() };

        let inner = NarNode::Directory {
            entries: vec![
                NarEntry { name: "data".to_string(), node: leaf_file.clone() },
                NarEntry { name: "link".to_string(), node: leaf_link },
            ],
        };
        let mid = NarNode::Directory {
            entries: vec![
                NarEntry { name: "inner".to_string(), node: inner },
                NarEntry { name: "readme".to_string(), node: NarNode::Regular { executable: false, contents: b"README".to_vec() } },
            ],
        };
        let root = NarNode::Directory {
            entries: vec![
                NarEntry { name: "bin".to_string(), node: NarNode::Regular { executable: true, contents: b"#!/bin/sh".to_vec() } },
                NarEntry { name: "lib".to_string(), node: mid },
            ],
        };

        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &root).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, root);
    }

    // ── Binary file content with all byte values ────────

    #[test]
    fn roundtrip_binary_content_all_byte_values() {
        let contents: Vec<u8> = (0..=255).collect();
        let node = NarNode::Regular { executable: false, contents };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    // ── Symlink with special characters ─────────────────

    #[test]
    fn roundtrip_symlink_with_special_chars() {
        let node = NarNode::Symlink { target: "../foo bar/baz\ttab".to_string() };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    // ── Error: invalid magic ─────────────────────────────

    #[test]
    fn reader_rejects_bad_magic() {
        let mut buf = Vec::new();
        // Write wrong magic
        write_str(&mut buf, b"not-nar-magic").unwrap();
        let result = NarReader::read_complete(&mut Cursor::new(&buf));
        assert!(result.is_err());
    }

    #[test]
    fn reader_rejects_empty_input() {
        let result = NarReader::read_complete(&mut Cursor::new(&[]));
        assert!(result.is_err());
    }

    // ── Property tests ──────────────────────────────────

    proptest! {
        #[test]
        fn prop_regular_file_roundtrip(contents in proptest::collection::vec(any::<u8>(), 0..1000), executable in any::<bool>()) {
            let node = NarNode::Regular { executable, contents };
            let mut buf = Vec::new();
            NarWriter::write(&mut buf, &node).unwrap();
            prop_assert_eq!(buf.len() % 8, 0);
            let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
            prop_assert_eq!(parsed, node);
        }

        #[test]
        fn prop_symlink_roundtrip(target in "[a-zA-Z0-9/_.-]{1,200}") {
            let node = NarNode::Symlink { target };
            let mut buf = Vec::new();
            NarWriter::write(&mut buf, &node).unwrap();
            prop_assert_eq!(buf.len() % 8, 0);
            let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
            prop_assert_eq!(parsed, node);
        }
    }

    // ── MAX_NAR_STRING enforcement ───────────────────────

    #[test]
    fn read_str_rejects_oversized_length_prefix() {
        // Hand-craft a NAR magic + a length prefix that exceeds MAX_NAR_STRING.
        // We can't easily reach this through write_path (gigabytes of data),
        // so build the bytes by hand.
        let mut buf = Vec::new();
        // First the magic
        write_str(&mut buf, NAR_MAGIC.as_bytes()).unwrap();
        // Open paren
        write_str(&mut buf, b"(").unwrap();
        // type token
        write_str(&mut buf, b"type").unwrap();
        // Now write a u64 length prefix that exceeds MAX_NAR_STRING for the
        // node type string
        wire::write_u64(&mut buf, MAX_NAR_STRING + 1).unwrap();
        let result = NarReader::read_complete(&mut Cursor::new(&buf));
        assert!(result.is_err());
        match result {
            Err(NarError::Invalid(s)) => assert!(s.contains("too long")),
            other => panic!("expected Invalid error about size, got {other:?}"),
        }
    }

    // ── Unknown node type ────────────────────────────────

    #[test]
    fn reader_rejects_unknown_node_type() {
        let mut buf = Vec::new();
        write_str(&mut buf, NAR_MAGIC.as_bytes()).unwrap();
        write_str(&mut buf, b"(").unwrap();
        write_str(&mut buf, b"type").unwrap();
        write_str(&mut buf, b"socket").unwrap(); // not regular/symlink/directory
        let result = NarReader::read_complete(&mut Cursor::new(&buf));
        match result {
            Err(NarError::Invalid(s)) => assert!(s.contains("unknown node type")),
            other => panic!("expected Invalid for unknown type, got {other:?}"),
        }
    }

    // ── Regular file with unexpected token after type ────

    #[test]
    fn reader_rejects_regular_with_wrong_token() {
        let mut buf = Vec::new();
        write_str(&mut buf, NAR_MAGIC.as_bytes()).unwrap();
        write_str(&mut buf, b"(").unwrap();
        write_str(&mut buf, b"type").unwrap();
        write_str(&mut buf, b"regular").unwrap();
        write_str(&mut buf, b"garbage").unwrap(); // expected executable or contents
        let result = NarReader::read_complete(&mut Cursor::new(&buf));
        match result {
            Err(NarError::UnexpectedToken { expected, .. }) => {
                assert!(expected.contains("executable") || expected.contains("contents"));
            }
            other => panic!("expected UnexpectedToken, got {other:?}"),
        }
    }

    // ── Directory with token that's not entry or close ──

    #[test]
    fn reader_rejects_directory_with_wrong_token() {
        let mut buf = Vec::new();
        write_str(&mut buf, NAR_MAGIC.as_bytes()).unwrap();
        write_str(&mut buf, b"(").unwrap();
        write_str(&mut buf, b"type").unwrap();
        write_str(&mut buf, b"directory").unwrap();
        write_str(&mut buf, b"garbage").unwrap(); // expected entry or )
        let result = NarReader::read_complete(&mut Cursor::new(&buf));
        match result {
            Err(NarError::UnexpectedToken { expected, .. }) => {
                assert!(expected.contains("entry") || expected.contains(")"));
            }
            other => panic!("expected UnexpectedToken, got {other:?}"),
        }
    }

    // ── 1 KB+ symlink target ─────────────────────────────

    #[test]
    fn roundtrip_kilobyte_symlink_target() {
        let target: String = "abcdefgh".repeat(150); // 1200 bytes
        assert!(target.len() >= 1024);
        let node = NarNode::Symlink { target };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    // ── Directory with 100+ entries ──────────────────────

    #[test]
    fn directory_with_100_entries_roundtrip() {
        let entries: Vec<NarEntry> = (0..120)
            .map(|i| NarEntry {
                name: format!("file-{i:04}"),
                node: NarNode::Regular {
                    executable: false,
                    contents: format!("body-{i}").into_bytes(),
                },
            })
            .collect();
        let node = NarNode::Directory { entries };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    // ── File with all 256 byte values ────────────────────

    #[test]
    fn roundtrip_file_with_all_256_byte_values() {
        let contents: Vec<u8> = (0..=255).collect();
        assert_eq!(contents.len(), 256);
        let node = NarNode::Regular { executable: false, contents };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    // ── NarError Display ─────────────────────────────────

    #[test]
    fn nar_error_invalid_display() {
        let err = NarError::Invalid("custom".to_string());
        let s = format!("{err}");
        assert!(s.contains("custom"));
    }

    #[test]
    fn nar_error_unexpected_token_display() {
        let err = NarError::UnexpectedToken {
            expected: "foo".to_string(),
            got: "bar".to_string(),
        };
        let s = format!("{err}");
        assert!(s.contains("foo"));
        assert!(s.contains("bar"));
    }

    // ── NAR_MAGIC constant value ─────────────────────────

    #[test]
    fn nar_magic_is_nix_archive_1() {
        assert_eq!(NAR_MAGIC, "nix-archive-1");
        assert_eq!(NAR_MAGIC.len(), 13);
    }

    #[test]
    fn max_nar_string_is_4gib() {
        assert_eq!(MAX_NAR_STRING, 4 * 1024 * 1024 * 1024);
    }

    // ── unpack_nar to filesystem ─────────────────────────

    #[test]
    fn unpack_nar_roundtrip_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("hello.txt"), b"hello world").unwrap();

        let mut nar_data = Vec::new();
        NarWriter::write_path(&mut nar_data, &src).unwrap();

        let dest = dir.path().join("dest");
        unpack_nar(&nar_data, &dest).unwrap();

        let restored = std::fs::read(dest.join("hello.txt")).unwrap();
        assert_eq!(restored, b"hello world");
    }

    // ── write_path on a single file (not directory) ─────

    #[test]
    fn write_path_on_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.txt");
        std::fs::write(&path, b"plain content").unwrap();

        let mut buf = Vec::new();
        NarWriter::write_path(&mut buf, &path).unwrap();
        assert!(buf.len() >= 8);
        assert_eq!(buf.len() % 8, 0);
    }

    // ── Magic header byte-level layout ───────────────────

    #[test]
    fn magic_header_layout() {
        let node = NarNode::Regular { executable: false, contents: vec![] };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        // First 8 bytes: u64 length-prefix = 13
        let len = u64::from_le_bytes(buf[..8].try_into().unwrap());
        assert_eq!(len, NAR_MAGIC.len() as u64);
        // Next 13 bytes: magic
        assert_eq!(&buf[8..21], NAR_MAGIC.as_bytes());
        // Next 3 bytes: zero padding
        assert_eq!(&buf[21..24], &[0u8, 0u8, 0u8]);
    }

    // ── NarNode equality ─────────────────────────────────

    #[test]
    fn nar_node_equality_and_clone() {
        let n1 = NarNode::Regular { executable: false, contents: vec![1, 2, 3] };
        let n2 = n1.clone();
        assert_eq!(n1, n2);
        let n3 = NarNode::Regular { executable: true, contents: vec![1, 2, 3] };
        assert_ne!(n1, n3);
    }

    #[test]
    fn nar_entry_equality_and_clone() {
        let e1 = NarEntry {
            name: "x".to_string(),
            node: NarNode::Symlink { target: "y".to_string() },
        };
        let e2 = e1.clone();
        assert_eq!(e1, e2);
    }

    // ── Empty symlink target ─────────────────────────────

    #[test]
    fn roundtrip_empty_symlink_target() {
        let node = NarNode::Symlink { target: String::new() };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    // ── Nested directory with executable inside ─────────

    #[test]
    fn nested_directory_with_executable_inside() {
        let node = NarNode::Directory {
            entries: vec![
                NarEntry {
                    name: "bin".to_string(),
                    node: NarNode::Directory {
                        entries: vec![NarEntry {
                            name: "tool".to_string(),
                            node: NarNode::Regular {
                                executable: true,
                                contents: b"binary content".to_vec(),
                            },
                        }],
                    },
                },
            ],
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    // ── Property test for directory entries ─────────────

    proptest! {
        #[test]
        fn prop_directory_entries_roundtrip(
            count in 0_usize..=20,
        ) {
            let entries: Vec<NarEntry> = (0..count).map(|i| NarEntry {
                name: format!("e{i:03}"),
                node: NarNode::Regular {
                    executable: i % 2 == 0,
                    contents: vec![i as u8; i],
                },
            }).collect();
            let node = NarNode::Directory { entries };
            let mut buf = Vec::new();
            NarWriter::write(&mut buf, &node).unwrap();
            prop_assert_eq!(buf.len() % 8, 0);
            let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
            prop_assert_eq!(parsed, node);
        }
    }
}
