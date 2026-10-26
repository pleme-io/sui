//! Find every store path a byte stream names — the one scanner every
//! integrity check shares.
//!
//! A NAR, a file or a symlink target names a store path by writing
//! `/nix/store/<32 nix32 chars>-`. [`StoreHashScanner`] finds the hash part of
//! every such occurrence in a stream fed in arbitrary chunks, so a match that
//! straddles two chunks is found exactly as if the bytes had arrived whole.
//!
//! It is a [`std::io::Write`], so a NAR can be serialized straight into it
//! (`NarWriter::write_path(&mut scanner, path)`) and a decompressor can be
//! copied into it, without ever holding the whole NAR.
//!
//! What it does **not** decide is whether a named hash is a reference. That
//! needs a universe (the paths that really exist) and, for a build, the
//! builder's candidate set; both belong to the caller. This scanner only says
//! which hashes the bytes name, and it never misses one.

use std::collections::BTreeSet;
use std::io;

use crate::store_path::{DEFAULT_STORE_DIR, STORE_PATH_HASH_LEN};

const NIX32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";
const PREFIX: &[u8] = b"/nix/store/";
/// Bytes a complete match needs: the prefix, the hash and the `-` after it.
const MATCH_LEN: usize = PREFIX.len() + STORE_PATH_HASH_LEN + 1;

/// A streaming scanner for `/nix/store/<hash>-` occurrences.
#[derive(Debug, Default, Clone)]
pub struct StoreHashScanner {
    carry: Vec<u8>,
    found: BTreeSet<String>,
    scanned: u64,
}

impl StoreHashScanner {
    /// An empty scanner.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next chunk of the stream.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.scanned += chunk.len() as u64;
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let decided = buf.len().saturating_sub(MATCH_LEN - 1);
        for start in memchr::memmem::find_iter(&buf, PREFIX) {
            if start >= decided {
                break;
            }
            let hash = &buf[start + PREFIX.len()..start + PREFIX.len() + STORE_PATH_HASH_LEN];
            if buf[start + MATCH_LEN - 1] == b'-' && hash.iter().all(|b| NIX32.contains(b)) {
                self.found.insert(String::from_utf8_lossy(hash).into_owned());
            }
        }
        self.carry = buf.split_off(decided);
    }

    /// Bytes fed so far.
    #[must_use]
    pub fn scanned(&self) -> u64 {
        self.scanned
    }

    /// The hashes found so far, sorted.
    #[must_use]
    pub fn hashes(&self) -> &BTreeSet<String> {
        &self.found
    }

    /// End the stream and return every hash it named.
    ///
    /// The carried tail is shorter than a complete match, so it can hold no
    /// further hash; finishing only releases the set.
    #[must_use]
    pub fn finish(self) -> BTreeSet<String> {
        self.found
    }
}

impl io::Write for StoreHashScanner {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.feed(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What one NAR is: its sha256, its size and the store hashes it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarDigest {
    /// sha256 of the uncompressed NAR — the narinfo's `NarHash`.
    pub sha256: [u8; 32],
    /// Bytes of the uncompressed NAR — the narinfo's `NarSize`.
    pub size: u64,
    /// Every store hash the NAR names (file contents and symlink targets).
    pub named: BTreeSet<String>,
}

/// A [`std::io::Write`] sink that digests a NAR as it streams past: one pass
/// gives the hash, the size and the names.
#[derive(Debug, Default, Clone)]
pub struct NarDigester {
    sha: sha2::Sha256,
    scanner: StoreHashScanner,
}

impl NarDigester {
    /// An empty digester.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Digest the NAR serialization of `path` (as `nix-store --dump` writes it).
    ///
    /// # Errors
    ///
    /// When the path cannot be read.
    pub fn of_path(path: &std::path::Path) -> Result<NarDigest, crate::nar::NarError> {
        let mut d = Self::new();
        crate::nar::NarWriter::write_path(&mut d, path)?;
        Ok(d.finish())
    }

    /// End the stream.
    #[must_use]
    pub fn finish(self) -> NarDigest {
        use sha2::Digest;
        NarDigest { sha256: self.sha.finalize().into(), size: self.scanner.scanned(), named: self.scanner.finish() }
    }
}

impl io::Write for NarDigester {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        use sha2::Digest;
        self.sha.update(buf);
        self.scanner.feed(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Every store hash `bytes` names.
#[must_use]
pub fn store_hashes_in(bytes: &[u8]) -> BTreeSet<String> {
    let mut s = StoreHashScanner::new();
    s.feed(bytes);
    s.finish()
}

/// The hash part of a store path given absolute (`/nix/store/<h>-n`) or as a
/// basename (`<h>-n`); `None` when it is shorter than a hash.
#[must_use]
pub fn hash_part(path: &str) -> Option<&str> {
    let base = path.strip_prefix(DEFAULT_STORE_DIR).map_or(path, |p| p.trim_start_matches('/'));
    base.get(..STORE_PATH_HASH_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
    const B: &str = "0000000000000000000000000000000a";

    #[test]
    fn finds_hashes_in_text_and_ignores_non_paths() {
        let text = format!(
            "x /nix/store/{A}-hello/bin y /nix/store/{B}-lib\0 /nix/store/not-a-hash \
             /nix/store/{A}x /nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-e /nix/store/"
        );
        let found: Vec<String> = store_hashes_in(text.as_bytes()).into_iter().collect();
        assert_eq!(found, vec![B.to_string(), A.to_string()]);
    }

    #[test]
    fn a_match_split_at_every_offset_is_still_found() {
        let text = format!("....../nix/store/{A}-rust-std/lib....");
        for cut in 0..=text.len() {
            let mut s = StoreHashScanner::new();
            s.feed(&text.as_bytes()[..cut]);
            s.feed(&text.as_bytes()[cut..]);
            assert_eq!(s.finish().len(), 1, "split at byte {cut} lost the match");
        }
        let mut s = StoreHashScanner::new();
        for b in text.as_bytes() {
            s.feed(std::slice::from_ref(b));
        }
        assert_eq!(s.finish().into_iter().collect::<Vec<_>>(), vec![A.to_string()]);
    }

    #[test]
    fn a_match_at_the_very_end_is_found() {
        let text = format!("/nix/store/{A}-");
        assert_eq!(store_hashes_in(text.as_bytes()).len(), 1);
        let short = format!("/nix/store/{A}");
        assert!(store_hashes_in(short.as_bytes()).is_empty());
    }

    #[test]
    fn a_serialized_nar_names_its_symlink_targets() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(format!("/nix/store/{A}-rust-std/lib"), dir.path().join("lib")).unwrap();
        let mut s = StoreHashScanner::new();
        crate::nar::NarWriter::write_path(&mut s, dir.path()).unwrap();
        assert_eq!(s.finish().into_iter().collect::<Vec<_>>(), vec![A.to_string()]);
    }

    #[test]
    fn hash_part_takes_absolute_or_basename() {
        assert_eq!(hash_part(&format!("/nix/store/{A}-x")), Some(A));
        assert_eq!(hash_part(&format!("{A}-x")), Some(A));
        assert_eq!(hash_part("short"), None);
    }
}
