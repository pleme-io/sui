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
}
