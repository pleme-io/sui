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

// ── Spec interpreter (M3 stub) ─────────────────────────────────────

/// Apply the NAR algorithm.  M3 stub.
///
/// # Errors
///
/// Always until M3.
pub fn apply(_format: &NarFormat) -> Result<String, SpecError> {
    Err(SpecError::Interp {
        phase: "nar".into(),
        message: "NAR spec interpreter not yet landed — sui-compat \
                  consumes nix-nar upstream today, M3 work lifts to \
                  this typed border".into(),
    })
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
}
