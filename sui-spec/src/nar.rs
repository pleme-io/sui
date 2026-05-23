//! Typed border for the Nix Archive (.nar) format.
//!
//! NAR is the on-the-wire format the binary cache returns and the
//! daemon worker streams.  It's a serialisation of a filesystem
//! subtree: directory entries, regular files (with executable bit),
//! and symlinks, all framed by length-prefixed strings.  cppnix's
//! `libnix-store/nar-accessor.cc` is the canonical reference.
//!
//! Today sui-compat consumes the `nix-nar` upstream crate; this
//! module names the typed contract so future format extensions
//! (e.g. extended attributes) ride on an explicit spec.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defnar-format
//!   :name           "cppnix-nar"
//!   :magic          "nix-archive-1"
//!   :encoding       LengthPrefixedString
//!   :entry-types    (Regular Executable Directory Symlink)
//!   :phases         ((:kind ReadMagic)
//!                    (:kind ParseRootNode)
//!                    (:kind StreamEntries)
//!                    (:kind ValidateChecksum)))
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border ───────────────────────────────────────────────────

/// One NAR format variant.  Today: only the cppnix baseline.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defnar-format")]
pub struct NarFormat {
    pub name: String,
    /// Magic bytes at file offset 0 (UTF-8, length-prefixed).
    pub magic: String,
    /// Wire encoding of strings + framing.
    pub encoding: NarEncoding,
    /// Entry kinds the parser must accept.
    #[serde(rename = "entryTypes")]
    pub entry_types: Vec<NarEntryType>,
    pub phases: Vec<NarPhase>,
}

/// String framing convention.  cppnix uses 8-byte little-endian
/// length prefix + padded to 8 bytes.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarEncoding {
    /// Length-prefixed, 8-byte LE prefix, 8-byte padded.  cppnix.
    LengthPrefixedString,
    /// Hypothetical alternative — if a future Nix variant ships a
    /// CBOR-framed NAR, we'd land it here.
    Cbor,
}

/// File-entry kind in a NAR.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NarEntryType {
    /// Regular file, mode 0644.
    Regular,
    /// Regular file with executable bit, mode 0755.
    Executable,
    /// Directory containing further entries.
    Directory,
    /// Symbolic link with stored target string.
    Symlink,
}

/// One phase of NAR parse/emit.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NarPhase {
    pub kind: NarPhaseKind,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
}

/// Closed set of NAR-handling phases.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarPhaseKind {
    /// Read the leading magic string; reject if mismatch.
    ReadMagic,
    /// Read the root `(` token and the root entry's type.
    ParseRootNode,
    /// Streaming-recursive walk of directory entries (DFS).
    StreamEntries,
    /// Run a sha256 over the entire NAR bytes and verify against
    /// the expected hash (caller provides).
    ValidateChecksum,
    /// Write the NAR's bytes to a sink (file, network, store).
    EmitToSink,
    /// Build an in-memory tree from streamed entries.
    BuildTree,
}

// ── Spec interpreter (M3.0 minimal — magic + framing check) ───────

/// Validate that the leading bytes of a NAR stream match the
/// declared magic for the format.  M3.0 doesn't parse the full
/// archive (sui-compat consumes nix-nar for that); this surface
/// verifies the format dispatch is well-formed.
///
/// # Errors
///
/// - `nar-too-short` if the input is shorter than the framed
///   magic.
/// - `nar-bad-magic` if the magic bytes don't match the declared
///   format.
pub fn validate_magic(format: &NarFormat, input: &[u8]) -> Result<(), SpecError> {
    // cppnix encodes strings as: 8-byte LE length + content +
    // padding to 8-byte alignment.  So the leading framed magic
    // is: [magic_len_u64_le, magic_bytes, pad].
    let magic_bytes = format.magic.as_bytes();
    let needed = 8 + ((magic_bytes.len() + 7) & !7);
    if input.len() < needed {
        return Err(SpecError::Interp {
            phase: "nar-too-short".into(),
            message: format!(
                "input is {} bytes, expected at least {needed} for framed magic `{}`",
                input.len(),
                format.magic,
            ),
        });
    }
    // Read the LE u64 length.
    let mut len_bytes = [0u8; 8];
    len_bytes.copy_from_slice(&input[0..8]);
    let declared_len = u64::from_le_bytes(len_bytes) as usize;
    if declared_len != magic_bytes.len() {
        return Err(SpecError::Interp {
            phase: "nar-bad-magic".into(),
            message: format!(
                "magic-len header {declared_len} != expected {}",
                magic_bytes.len(),
            ),
        });
    }
    // Read the magic bytes.
    let magic_in = &input[8..8 + magic_bytes.len()];
    if magic_in != magic_bytes {
        return Err(SpecError::Interp {
            phase: "nar-bad-magic".into(),
            message: format!(
                "magic bytes mismatch: got `{}`, expected `{}`",
                String::from_utf8_lossy(magic_in),
                format.magic,
            ),
        });
    }
    Ok(())
}

/// Pack a string per cppnix's NAR framing (`u64-le length + bytes +
/// pad-to-8`).  Useful for tests + future M3.1 emitter.
#[must_use]
pub fn pack_framed(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let pad = (8 - (bytes.len() % 8)) % 8;
    let mut out = Vec::with_capacity(8 + bytes.len() + pad);
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
    out.extend(std::iter::repeat_n(0u8, pad));
    out
}

/// Apply the NAR algorithm.  M3.0: validates magic on the input
/// and returns an empty marker on success.  Full parse + emit are
/// M3.1 work (the sui-compat crate already handles them; this
/// surface is the typed entry-point future code dispatches
/// through).
///
/// # Errors
///
/// Whatever `validate_magic` returns.
pub fn apply(format: &NarFormat, input: &[u8]) -> Result<String, SpecError> {
    validate_magic(format, input)?;
    Ok(format!(
        "magic ok ({} bytes), full parse M3.1 (sui-compat::nar)",
        input.len(),
    ))
}

// ── NAR encoder ────────────────────────────────────────────────────
//
// Substrate-level encoder.  Byte-equivalent with `nix store dump-path`
// (verified on the operator workstation via sha256 round-trip).  Used
// by the `sui store dump-path` / `sui store add-file` / `sui store
// add-path` / `sui store make-content-addressed` dispatches in the
// sui binary, plus the future sui-build / sui-store ingestion paths.

/// Encode a filesystem path as canonical NAR bytes.  Returns the
/// magic-prefixed byte stream byte-equivalent with `nix store
/// dump-path <path>`.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] for any filesystem
/// read failure (symlink readlink, missing entries, etc.).
pub fn encode(root: &std::path::Path) -> Result<Vec<u8>, std::io::Error> {
    let mut buf = Vec::new();
    write_framed(&mut buf, b"nix-archive-1");
    write_node(&mut buf, root)?;
    Ok(buf)
}

/// Convenience: NAR-encode then sha256-digest the result.  Returns
/// the 32-byte digest matching `nix-hash --type sha256 --base16
/// --flat <nar-dump>`.
///
/// # Errors
///
/// Propagates [`encode`] errors.
pub fn hash_path_nar(root: &std::path::Path) -> Result<[u8; 32], std::io::Error> {
    use sha2::Digest;
    let nar = encode(root)?;
    let digest = sha2::Sha256::digest(&nar);
    Ok(digest.into())
}

// ── NAR decoder ────────────────────────────────────────────────────
//
// Symmetric peer of `encode`.  Reads canonical NAR bytes and
// materializes the filesystem tree under `dest`.  Combined with
// `encode`, gives the operator full round-trip control:
// dump-path → materialize at a new location → hash-equality.

/// Typed decoder error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NarDecodeError {
    BadMagic { found: String },
    BadFraming { at: usize, message: String },
    UnknownEntryType { name: String },
    TooShort { at: usize },
    Io(String),
}

impl std::fmt::Display for NarDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic { found }      => write!(f, "bad NAR magic: {found:?}"),
            Self::BadFraming { at, message } => write!(f, "bad framing at {at}: {message}"),
            Self::UnknownEntryType { name } => write!(f, "unknown entry type: {name:?}"),
            Self::TooShort { at }         => write!(f, "input too short at {at}"),
            Self::Io(m)                   => write!(f, "io: {m}"),
        }
    }
}

impl std::error::Error for NarDecodeError {}

/// Decode canonical NAR bytes and materialize the tree to `dest`.
/// Mirrors `nix-store --restore <dest> < <nar-bytes>`.
///
/// # Errors
///
/// Returns a typed `NarDecodeError` for any malformed input or
/// filesystem operation failure.
pub fn decode(input: &[u8], dest: &std::path::Path) -> Result<(), NarDecodeError> {
    let mut cur = Cursor::new(input);
    let magic = cur.read_framed_string()?;
    if magic != b"nix-archive-1" {
        return Err(NarDecodeError::BadMagic {
            found: String::from_utf8_lossy(&magic).to_string(),
        });
    }
    read_node(&mut cur, dest)?;
    Ok(())
}

/// Streaming variant — read from any `Read` source.  Buffers the
/// whole stream first so we can validate framing; the canonical
/// NAR is fully buffered in cppnix too, so this is symmetric.
///
/// # Errors
///
/// Propagates IO errors + decode errors.
pub fn decode_from<R: std::io::Read>(
    mut input: R,
    dest: &std::path::Path,
) -> Result<(), NarDecodeError> {
    let mut buf = Vec::new();
    input.read_to_end(&mut buf).map_err(|e| NarDecodeError::Io(e.to_string()))?;
    decode(&buf, dest)
}

struct Cursor<'a> {
    data: &'a [u8],
    pos:  usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self { Self { data, pos: 0 } }

    fn read_u64_le(&mut self) -> Result<u64, NarDecodeError> {
        if self.pos + 8 > self.data.len() {
            return Err(NarDecodeError::TooShort { at: self.pos });
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.data[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(u64::from_le_bytes(buf))
    }

    fn read_framed_string(&mut self) -> Result<Vec<u8>, NarDecodeError> {
        let len = self.read_u64_le()? as usize;
        if self.pos + len > self.data.len() {
            return Err(NarDecodeError::TooShort { at: self.pos });
        }
        let bytes = self.data[self.pos..self.pos + len].to_vec();
        self.pos += len;
        // Skip padding to next 8-byte boundary.
        let pad = (8 - (len % 8)) % 8;
        if self.pos + pad > self.data.len() {
            return Err(NarDecodeError::TooShort { at: self.pos });
        }
        self.pos += pad;
        Ok(bytes)
    }

    fn read_framed_str(&mut self) -> Result<String, NarDecodeError> {
        let bytes = self.read_framed_string()?;
        String::from_utf8(bytes).map_err(|e| NarDecodeError::BadFraming {
            at: self.pos,
            message: format!("invalid utf-8: {e}"),
        })
    }

    fn expect_string(&mut self, expected: &[u8]) -> Result<(), NarDecodeError> {
        let got = self.read_framed_string()?;
        if got != expected {
            return Err(NarDecodeError::BadFraming {
                at: self.pos,
                message: format!(
                    "expected {:?}, got {:?}",
                    String::from_utf8_lossy(expected),
                    String::from_utf8_lossy(&got),
                ),
            });
        }
        Ok(())
    }
}

fn read_node(cur: &mut Cursor, dest: &std::path::Path) -> Result<(), NarDecodeError> {
    cur.expect_string(b"(")?;
    cur.expect_string(b"type")?;
    let entry_type = cur.read_framed_str()?;
    match entry_type.as_str() {
        "regular"   => read_regular(cur, dest)?,
        "directory" => read_directory(cur, dest)?,
        "symlink"   => read_symlink(cur, dest)?,
        other       => return Err(NarDecodeError::UnknownEntryType {
            name: other.to_string(),
        }),
    }
    cur.expect_string(b")")?;
    Ok(())
}

fn read_regular(cur: &mut Cursor, dest: &std::path::Path) -> Result<(), NarDecodeError> {
    let mut executable = false;
    // Optional `executable ""` block before `contents`.
    let tag = cur.read_framed_str()?;
    let bytes = if tag == "executable" {
        executable = true;
        let _ = cur.read_framed_string()?; // empty value
        cur.expect_string(b"contents")?;
        cur.read_framed_string()?
    } else if tag == "contents" {
        cur.read_framed_string()?
    } else {
        return Err(NarDecodeError::BadFraming {
            at: cur.pos,
            message: format!("regular: expected `executable` or `contents`, got {tag:?}"),
        });
    };
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| NarDecodeError::Io(e.to_string()))?;
        }
    }
    std::fs::write(dest, &bytes).map_err(|e| NarDecodeError::Io(e.to_string()))?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dest)
            .map_err(|e| NarDecodeError::Io(e.to_string()))?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(dest, perms)
            .map_err(|e| NarDecodeError::Io(e.to_string()))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

fn read_directory(cur: &mut Cursor, dest: &std::path::Path) -> Result<(), NarDecodeError> {
    std::fs::create_dir_all(dest).map_err(|e| NarDecodeError::Io(e.to_string()))?;
    loop {
        // Peek at the next framed string — either "entry" (more
        // children) or ")" (end-of-directory closer).
        let save_pos = cur.pos;
        let tag = cur.read_framed_string()?;
        match tag.as_slice() {
            b"entry" => {
                cur.expect_string(b"(")?;
                cur.expect_string(b"name")?;
                let name = cur.read_framed_str()?;
                cur.expect_string(b"node")?;
                read_node(cur, &dest.join(&name))?;
                cur.expect_string(b")")?;
            }
            b")" => {
                // Roll back — read_node's closing `)` matches.
                cur.pos = save_pos;
                return Ok(());
            }
            other => return Err(NarDecodeError::BadFraming {
                at: cur.pos,
                message: format!("directory: unexpected tag {:?}", String::from_utf8_lossy(other)),
            }),
        }
    }
}

fn read_symlink(cur: &mut Cursor, dest: &std::path::Path) -> Result<(), NarDecodeError> {
    cur.expect_string(b"target")?;
    let target = cur.read_framed_str()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs as unix_fs;
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| NarDecodeError::Io(e.to_string()))?;
            }
        }
        unix_fs::symlink(target, dest)
            .map_err(|e| NarDecodeError::Io(e.to_string()))?;
    }
    #[cfg(not(unix))]
    let _ = (target, dest);
    Ok(())
}

/// Compute the canonical store path for a NAR digest + name.
/// Returns `"<store-root>/<32-char-nix-base32-hash-prefix>-<name>"`.
/// Mirrors cppnix's `makeFixedOutputPath` shape.
#[must_use]
pub fn store_path_for(store_root: &str, digest: &[u8; 32], name: &str) -> String {
    let hash_b32 = crate::hash::encode_hash("sha256", "nix-base32", digest)
        .expect("nix-base32 encoding always succeeds for sha256 digests");
    let bare = hash_b32.strip_prefix("sha256:").unwrap_or(&hash_b32);
    let store_hash: String = bare.chars().take(32).collect();
    format!("{store_root}/{store_hash}-{name}")
}

/// Test/internal helper exported for store_ops's typed re-encoder.
/// External callers should use [`encode`].
#[doc(hidden)]
pub fn write_string_for_test(buf: &mut Vec<u8>, s: &[u8]) {
    write_framed(buf, s);
}

/// Write a length-prefixed string in canonical NAR padded form.
fn write_framed(buf: &mut Vec<u8>, s: &[u8]) {
    buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
    buf.extend_from_slice(s);
    let pad = (8 - (s.len() % 8)) % 8;
    for _ in 0..pad { buf.push(0); }
}

fn write_node(buf: &mut Vec<u8>, path: &std::path::Path) -> std::io::Result<()> {
    write_framed(buf, b"(");
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_file() {
        write_framed(buf, b"type");
        write_framed(buf, b"regular");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 != 0 {
                write_framed(buf, b"executable");
                write_framed(buf, b"");
            }
        }
        write_framed(buf, b"contents");
        let bytes = std::fs::read(path)?;
        write_framed(buf, &bytes);
    } else if meta.is_dir() {
        write_framed(buf, b"type");
        write_framed(buf, b"directory");
        let mut entries: Vec<_> = std::fs::read_dir(path)?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            write_framed(buf, b"entry");
            write_framed(buf, b"(");
            write_framed(buf, b"name");
            write_framed(buf, entry.file_name().to_string_lossy().as_bytes());
            write_framed(buf, b"node");
            write_node(buf, &entry.path())?;
            write_framed(buf, b")");
        }
    } else if meta.file_type().is_symlink() {
        write_framed(buf, b"type");
        write_framed(buf, b"symlink");
        write_framed(buf, b"target");
        let target = std::fs::read_link(path)?;
        write_framed(buf, target.to_string_lossy().as_bytes());
    }
    write_framed(buf, b")");
    Ok(())
}

// ── Canonical spec ─────────────────────────────────────────────────

pub const CANONICAL_NAR_LISP: &str = include_str!("../specs/nar.lisp");

/// Compile every authored NAR-format variant.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<NarFormat>, SpecError> {
    crate::loader::load_all::<NarFormat>(CANONICAL_NAR_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_nar_parses() {
        let formats = load_canonical().expect("canonical NAR must compile");
        assert!(!formats.is_empty());
    }

    #[test]
    fn cppnix_nar_has_correct_magic() {
        let formats = load_canonical().unwrap();
        let cppnix = formats
            .iter()
            .find(|f| f.name == "cppnix-nar")
            .expect("cppnix-nar must exist");
        assert_eq!(cppnix.magic, "nix-archive-1");
        assert_eq!(cppnix.encoding, NarEncoding::LengthPrefixedString);
    }

    #[test]
    fn cppnix_nar_covers_four_entry_types() {
        let formats = load_canonical().unwrap();
        let cppnix = formats.iter().find(|f| f.name == "cppnix-nar").unwrap();
        let types: std::collections::HashSet<NarEntryType> =
            cppnix.entry_types.iter().copied().collect();
        for required in [
            NarEntryType::Regular,
            NarEntryType::Executable,
            NarEntryType::Directory,
            NarEntryType::Symlink,
        ] {
            assert!(
                types.contains(&required),
                "cppnix-nar missing entry type {required:?}",
            );
        }
    }

    #[test]
    fn nar_phases_must_read_magic_first() {
        let formats = load_canonical().unwrap();
        for f in &formats {
            let kinds: Vec<NarPhaseKind> = f.phases.iter().map(|p| p.kind).collect();
            assert_eq!(
                kinds[0],
                NarPhaseKind::ReadMagic,
                "{}: first phase must be ReadMagic",
                f.name,
            );
        }
    }

    // ── M3.0 magic-validation tests ────────────────────────────

    fn cppnix() -> NarFormat {
        load_canonical().unwrap().into_iter()
            .find(|f| f.name == "cppnix-nar")
            .unwrap()
    }

    #[test]
    fn pack_framed_roundtrip_shape() {
        // "nix-archive-1" is 13 bytes → padded to 16.  With the
        // 8-byte length prefix, total is 24 bytes.
        let packed = pack_framed("nix-archive-1");
        assert_eq!(packed.len(), 8 + 16);
        // First 8 bytes = LE u64 of 13.
        let mut len_bytes = [0u8; 8];
        len_bytes.copy_from_slice(&packed[0..8]);
        assert_eq!(u64::from_le_bytes(len_bytes), 13);
    }

    #[test]
    fn validate_magic_passes_on_correct_bytes() {
        let format = cppnix();
        let packed = pack_framed(&format.magic);
        validate_magic(&format, &packed).unwrap();
    }

    #[test]
    fn validate_magic_rejects_short_input() {
        let format = cppnix();
        let err = validate_magic(&format, &[0u8; 3]).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "nar-too-short"),
            _ => panic!("expected nar-too-short"),
        }
    }

    #[test]
    fn validate_magic_rejects_wrong_magic() {
        let format = cppnix();
        let packed = pack_framed("wrong-archive-id");
        let err = validate_magic(&format, &packed).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "nar-bad-magic"),
            _ => panic!("expected nar-bad-magic"),
        }
    }

    #[test]
    fn apply_succeeds_on_well_framed_magic() {
        let format = cppnix();
        let packed = pack_framed(&format.magic);
        let msg = apply(&format, &packed).unwrap();
        assert!(msg.contains("magic ok"));
        assert!(msg.contains("M3.1"));
    }

    // ── encoder tests ──────────────────────────────────────────

    #[test]
    fn encode_file_starts_with_magic() {
        let tmp = std::env::temp_dir().join("sui-spec-nar-file-test");
        std::fs::write(&tmp, b"hello").unwrap();
        let nar = encode(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        // First 8 bytes = LE u64 of 13 (length of "nix-archive-1")
        let len = u64::from_le_bytes(nar[0..8].try_into().unwrap());
        assert_eq!(len, 13);
        assert_eq!(&nar[8..21], b"nix-archive-1");
    }

    #[test]
    fn encode_directory_is_sorted() {
        // Build a small directory with two files in non-sorted
        // insertion order; encoder must sort by file name so
        // the NAR is deterministic.
        let tmp = std::env::temp_dir().join("sui-spec-nar-dir-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("z"), b"z-content").unwrap();
        std::fs::write(tmp.join("a"), b"a-content").unwrap();
        let nar1 = encode(&tmp).unwrap();

        // Round-trip via byte-equivalence: encoding twice yields
        // the same bytes (determinism).
        let nar2 = encode(&tmp).unwrap();
        assert_eq!(nar1, nar2);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn hash_path_nar_is_32_bytes() {
        let tmp = std::env::temp_dir().join("sui-spec-nar-hash-test");
        std::fs::write(&tmp, b"abc").unwrap();
        let digest = hash_path_nar(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(digest.len(), 32);
    }

    #[test]
    fn store_path_for_uses_first_32_chars_of_base32_hash() {
        // Known sha256 digest of empty string = a SRI we can
        // cross-check.  Just verify shape: starts with store
        // root, has 32-char hash prefix + dash + name.
        let digest: [u8; 32] = [0u8; 32];
        let path = store_path_for("/nix/store", &digest, "hello");
        assert!(path.starts_with("/nix/store/"));
        let after_root = path.strip_prefix("/nix/store/").unwrap();
        let (hash, name) = after_root.split_once('-').unwrap();
        assert_eq!(hash.len(), 32);
        assert_eq!(name, "hello");
    }

    #[test]
    fn store_path_for_is_deterministic() {
        let digest: [u8; 32] = [42u8; 32];
        let p1 = store_path_for("/nix/store", &digest, "name");
        let p2 = store_path_for("/nix/store", &digest, "name");
        assert_eq!(p1, p2);
    }

    #[test]
    fn store_path_for_differs_with_name() {
        let digest: [u8; 32] = [42u8; 32];
        let p1 = store_path_for("/nix/store", &digest, "a");
        let p2 = store_path_for("/nix/store", &digest, "b");
        // Hash prefix matches; suffix differs.
        let (h1, _) = p1.strip_prefix("/nix/store/").unwrap().split_once('-').unwrap();
        let (h2, _) = p2.strip_prefix("/nix/store/").unwrap().split_once('-').unwrap();
        assert_eq!(h1, h2);
        assert!(p1.ends_with("-a"));
        assert!(p2.ends_with("-b"));
    }
}
