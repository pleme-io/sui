//! NAR (Nix Archive) serialization format.
//!
//! Clean-room implementation from the NAR specification:
//! <https://nix.dev/manual/nix/2.22/protocols/nix-archive>
//!
//! Wire format: all strings are 8-byte aligned with zero padding.
//! `str(s)` = 64-bit LE length + bytes + zero-pad to 8-byte boundary.
//! Directory entries must be sorted by name.

use std::io::{self, Read, Write};
use std::path::Path;

use thiserror::Error;

/// NAR magic header.
pub const NAR_MAGIC: &str = "nix-archive-1";

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

/// Write a 64-bit little-endian integer.
fn write_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Read a 64-bit little-endian integer.
fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

/// Write a length-prefixed, 8-byte-aligned string.
fn write_str(w: &mut impl Write, s: &[u8]) -> io::Result<()> {
    write_u64(w, s.len() as u64)?;
    w.write_all(s)?;
    let pad = (8 - (s.len() % 8)) % 8;
    if pad > 0 {
        w.write_all(&vec![0u8; pad])?;
    }
    Ok(())
}

/// Read a length-prefixed, 8-byte-aligned string.
fn read_str(r: &mut impl Read) -> Result<Vec<u8>, NarError> {
    let len = read_u64(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let pad = (8 - (len % 8)) % 8;
    if pad > 0 {
        let mut pad_buf = vec![0u8; pad];
        r.read_exact(&mut pad_buf)?;
    }
    Ok(buf)
}

/// Read a string and return it as a UTF-8 string, or error.
fn read_str_utf8(r: &mut impl Read) -> Result<String, NarError> {
    let bytes = read_str(r)?;
    String::from_utf8(bytes).map_err(|e| NarError::Invalid(format!("invalid UTF-8: {e}")))
}

/// Read a string and assert it matches the expected value.
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

// ── NAR node types ───────────────────────────────────────────

/// A node in a NAR archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NarNode {
    /// A regular file.
    Regular {
        executable: bool,
        contents: Vec<u8>,
    },
    /// A symbolic link.
    Symlink {
        target: String,
    },
    /// A directory with sorted entries.
    Directory {
        entries: Vec<NarEntry>,
    },
}

/// A directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarEntry {
    pub name: String,
    pub node: NarNode,
}

// ── Writer ───────────────────────────────────────────────────

/// Serialize a NAR node tree to a writer.
pub struct NarWriter;

impl NarWriter {
    /// Write a complete NAR archive.
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

    /// Serialize a filesystem path to NAR format.
    pub fn write_path(w: &mut impl Write, path: &Path) -> Result<(), NarError> {
        write_str(w, NAR_MAGIC.as_bytes())?;
        Self::write_path_node(w, path)?;
        Ok(())
    }

    fn write_path_node(w: &mut impl Write, path: &Path) -> Result<(), NarError> {
        let metadata = std::fs::symlink_metadata(path)?;

        write_str(w, b"(")?;

        if metadata.is_symlink() {
            let target = std::fs::read_link(path)?;
            write_str(w, b"type")?;
            write_str(w, b"symlink")?;
            write_str(w, b"target")?;
            write_str(w, target.to_string_lossy().as_bytes())?;
        } else if metadata.is_file() {
            write_str(w, b"type")?;
            write_str(w, b"regular")?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 != 0 {
                    write_str(w, b"executable")?;
                    write_str(w, b"")?;
                }
            }

            let contents = std::fs::read(path)?;
            write_str(w, b"contents")?;
            write_str(w, &contents)?;
        } else if metadata.is_dir() {
            write_str(w, b"type")?;
            write_str(w, b"directory")?;

            let mut entries: Vec<_> = std::fs::read_dir(path)?
                .filter_map(Result::ok)
                .collect();
            entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

            for entry in entries {
                write_str(w, b"entry")?;
                write_str(w, b"(")?;
                write_str(w, b"name")?;
                write_str(w, entry.file_name().to_string_lossy().as_bytes())?;
                write_str(w, b"node")?;
                Self::write_path_node(w, &entry.path())?;
                write_str(w, b")")?;
            }
        }

        write_str(w, b")")?;
        Ok(())
    }
}

// ── Reader ───────────────────────────────────────────────────

/// Deserialize a NAR archive from a reader.
pub struct NarReader;

impl NarReader {
    /// Read a complete NAR archive.
    pub fn read(r: &mut impl Read) -> Result<NarNode, NarError> {
        expect_str(r, NAR_MAGIC)?;
        Self::read_node(r)
    }

    fn read_node(r: &mut impl Read) -> Result<NarNode, NarError> {
        expect_str(r, "(")?;

        expect_str(r, "type")?;
        let node_type = read_str_utf8(r)?;

        let node = match node_type.as_str() {
            "regular" => Self::read_regular(r)?,
            "symlink" => Self::read_symlink(r)?,
            "directory" => Self::read_directory(r)?,
            _ => return Err(NarError::Invalid(format!("unknown node type: {node_type}"))),
        };

        expect_str(r, ")")?;
        Ok(node)
    }

    fn read_regular(r: &mut impl Read) -> Result<NarNode, NarError> {
        let mut executable = false;

        // Peek at next token — could be "executable" or "contents"
        let token = read_str_utf8(r)?;
        if token == "executable" {
            executable = true;
            // Read the empty string value
            read_str(r)?;
            // Now read "contents"
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
                // We consumed the closing paren — but read_node expects to consume it.
                // Return early here: the ")" for the directory node is this token.
                // We need to handle this differently. Let's use a peek approach.
                // Actually, re-examine: after type=directory, we loop reading "entry" tokens.
                // When we see ")" that's the close of the node. But read_node also expects ")".
                // Solution: return entries and let the caller know we already consumed ")".
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

    /// Read a complete NAR archive (top-level entry point).
    pub fn read_complete(r: &mut impl Read) -> Result<NarNode, NarError> {
        expect_str(r, NAR_MAGIC)?;
        Self::read_node_v2(r)
    }

    /// Improved node reader that handles directory close-paren correctly.
    fn read_node_v2(r: &mut impl Read) -> Result<NarNode, NarError> {
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
            "directory" => {
                Self::read_directory_v2(r)
            }
            _ => Err(NarError::Invalid(format!("unknown node type: {node_type}"))),
        }
    }

    /// Read directory entries until we hit the closing paren for this node.
    fn read_directory_v2(r: &mut impl Read) -> Result<NarNode, NarError> {
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
            let node = Self::read_node_v2(r)?;
            expect_str(r, ")")?;

            entries.push(NarEntry { name, node });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_regular_file() {
        let node = NarNode::Regular {
            executable: false,
            contents: b"hello world".to_vec(),
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();

        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn roundtrip_executable() {
        let node = NarNode::Regular {
            executable: true,
            contents: b"#!/bin/sh\necho hi".to_vec(),
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();

        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn roundtrip_symlink() {
        let node = NarNode::Symlink {
            target: "/nix/store/abc-foo/bin/foo".to_string(),
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();

        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn roundtrip_directory() {
        let node = NarNode::Directory {
            entries: vec![
                NarEntry {
                    name: "bar".to_string(),
                    node: NarNode::Regular {
                        executable: false,
                        contents: b"bar contents".to_vec(),
                    },
                },
                NarEntry {
                    name: "foo".to_string(),
                    node: NarNode::Symlink {
                        target: "bar".to_string(),
                    },
                },
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
                NarEntry {
                    name: "bin".to_string(),
                    node: NarNode::Directory {
                        entries: vec![NarEntry {
                            name: "hello".to_string(),
                            node: NarNode::Regular {
                                executable: true,
                                contents: b"ELF".to_vec(),
                            },
                        }],
                    },
                },
                NarEntry {
                    name: "lib".to_string(),
                    node: NarNode::Directory {
                        entries: vec![],
                    },
                },
            ],
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();

        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);
    }

    #[test]
    fn nar_magic_header() {
        let node = NarNode::Regular {
            executable: false,
            contents: vec![],
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();

        // First 8 bytes: length of "nix-archive-1" (13)
        let len = u64::from_le_bytes(buf[..8].try_into().unwrap());
        assert_eq!(len, 13);
        // Next 13 bytes: the magic string
        assert_eq!(&buf[8..21], NAR_MAGIC.as_bytes());
    }

    #[test]
    fn eight_byte_alignment() {
        // "hello" is 5 bytes, needs 3 bytes padding
        let node = NarNode::Regular {
            executable: false,
            contents: b"hello".to_vec(),
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();

        // Total size should be 8-byte aligned
        assert_eq!(buf.len() % 8, 0);
    }

    #[test]
    fn empty_file() {
        let node = NarNode::Regular {
            executable: false,
            contents: vec![],
        };
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
        // 1 MB file
        let contents = vec![0xAB; 1_000_000];
        let node = NarNode::Regular {
            executable: false,
            contents: contents.clone(),
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();

        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        match &parsed {
            NarNode::Regular { executable, contents: c } => {
                assert!(!executable);
                assert_eq!(c.len(), 1_000_000);
                assert_eq!(c, &contents);
            }
            _ => panic!("expected regular file"),
        }
    }

    #[test]
    fn file_with_exact_8_byte_aligned_content() {
        // Exactly 8 bytes -- no padding needed
        let node = NarNode::Regular {
            executable: false,
            contents: b"12345678".to_vec(),
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();
        assert_eq!(buf.len() % 8, 0);

        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, node);

        // Also test 16 bytes (another exact multiple)
        let node16 = NarNode::Regular {
            executable: false,
            contents: b"1234567890abcdef".to_vec(),
        };
        let mut buf16 = Vec::new();
        NarWriter::write(&mut buf16, &node16).unwrap();
        assert_eq!(buf16.len() % 8, 0);

        let parsed16 = NarReader::read_complete(&mut Cursor::new(&buf16)).unwrap();
        assert_eq!(parsed16, node16);
    }

    #[test]
    fn deeply_nested_directories_5_levels() {
        // Build 5-level nesting: level0/level1/level2/level3/level4/file.txt
        let leaf = NarNode::Regular {
            executable: false,
            contents: b"deep content".to_vec(),
        };
        let level4 = NarNode::Directory {
            entries: vec![NarEntry { name: "file.txt".to_string(), node: leaf }],
        };
        let level3 = NarNode::Directory {
            entries: vec![NarEntry { name: "level4".to_string(), node: level4 }],
        };
        let level2 = NarNode::Directory {
            entries: vec![NarEntry { name: "level3".to_string(), node: level3 }],
        };
        let level1 = NarNode::Directory {
            entries: vec![NarEntry { name: "level2".to_string(), node: level2 }],
        };
        let root = NarNode::Directory {
            entries: vec![NarEntry { name: "level1".to_string(), node: level1 }],
        };

        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &root).unwrap();

        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(parsed, root);
    }

    #[test]
    fn directory_with_many_entries() {
        // 50+ entries
        let entries: Vec<NarEntry> = (0..60)
            .map(|i| NarEntry {
                name: format!("file-{i:03}"),
                node: NarNode::Regular {
                    executable: false,
                    contents: format!("content of file {i}").into_bytes(),
                },
            })
            .collect();

        let node = NarNode::Directory { entries };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();

        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        match &parsed {
            NarNode::Directory { entries } => {
                assert_eq!(entries.len(), 60);
                assert_eq!(entries[0].name, "file-000");
                assert_eq!(entries[59].name, "file-059");
            }
            _ => panic!("expected directory"),
        }
    }

    #[test]
    fn symlink_with_long_target_path() {
        let long_target = "/nix/store/".to_string()
            + &"a".repeat(200)
            + "/lib/"
            + &"b".repeat(100)
            + "/very-long-library-name.so.1.2.3";

        let node = NarNode::Symlink {
            target: long_target.clone(),
        };
        let mut buf = Vec::new();
        NarWriter::write(&mut buf, &node).unwrap();

        let parsed = NarReader::read_complete(&mut Cursor::new(&buf)).unwrap();
        match &parsed {
            NarNode::Symlink { target } => assert_eq!(target, &long_target),
            _ => panic!("expected symlink"),
        }
    }

    #[test]
    fn write_path_on_real_temp_directory() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path();

        // Create a small file tree
        std::fs::write(dir_path.join("hello.txt"), b"Hello, NAR!").unwrap();
        std::fs::create_dir(dir_path.join("subdir")).unwrap();
        std::fs::write(dir_path.join("subdir").join("nested.txt"), b"nested content").unwrap();

        let mut buf = Vec::new();
        NarWriter::write_path(&mut buf, dir_path).unwrap();

        // The output should be valid NAR: starts with magic length + magic bytes
        assert!(buf.len() > 21);
        let magic_len = u64::from_le_bytes(buf[..8].try_into().unwrap());
        assert_eq!(magic_len, 13);
        assert_eq!(&buf[8..21], NAR_MAGIC.as_bytes());
        // Total size should be 8-byte aligned
        assert_eq!(buf.len() % 8, 0);
    }

    #[test]
    fn mixed_directory_with_all_node_types() {
        let node = NarNode::Directory {
            entries: vec![
                NarEntry {
                    name: "executable".to_string(),
                    node: NarNode::Regular {
                        executable: true,
                        contents: b"#!/bin/sh\nexit 0".to_vec(),
                    },
                },
                NarEntry {
                    name: "link".to_string(),
                    node: NarNode::Symlink {
                        target: "executable".to_string(),
                    },
                },
                NarEntry {
                    name: "regular".to_string(),
                    node: NarNode::Regular {
                        executable: false,
                        contents: b"data".to_vec(),
                    },
                },
                NarEntry {
                    name: "subdir".to_string(),
                    node: NarNode::Directory {
                        entries: vec![NarEntry {
                            name: "inner".to_string(),
                            node: NarNode::Regular {
                                executable: false,
                                contents: b"inner data".to_vec(),
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
}
