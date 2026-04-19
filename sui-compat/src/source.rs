//! Source-tree store-path computation — the primitive behind
//! `builtins.getFlake "path:..."`.
//!
//! CppNix, when asked for a `path:` flake ref, serializes the
//! source tree as a NAR archive (excluding `.git` by default),
//! hashes the NAR with sha256, and produces:
//!
//!   - a store path of the form `/nix/store/<hash>-source`
//!     (computed via the `fixed-output-hash` "source" branch),
//!   - a SRI-format `narHash` of the form `sha256-<base64>`.
//!
//! Both are surfaced on the flake result as `outPath` + `narHash`
//! (top level) and duplicated inside `sourceInfo`.
//!
//! This module is the single place we serialize + hash a source
//! tree.  Callers (currently just the flake evaluator in
//! sui-eval) go through one function and get both outputs
//! atomically — no chance of the hash drifting from the path.

use std::io::Cursor;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::hash::{base64_encode, HashAlgorithm, NixHash};
use crate::nar::{NarError, NarWriter};
use crate::store_path::compute_fixed_output_hash;

/// Result of serializing + hashing a source tree.
#[derive(Debug, Clone)]
pub struct SourceHash {
    /// Store path the source would be materialized under, e.g.
    /// `/nix/store/p8zn7x0860a3h5xf1dg01a3sfxs3s46i-source`.
    pub store_path: String,
    /// SRI-format NAR hash, e.g.
    /// `sha256-fpA5m7tc6t4Oe6Uku9gKvul7CrR7urWE1K+DA0nhLPI=`.
    /// This is what CppNix exposes as the `narHash` attribute on
    /// flake results.
    pub nar_hash_sri: String,
    /// Raw NAR bytes.  Callers that want to cache or upload the
    /// archive (binary cache push, store materialization) use this
    /// directly — re-serializing would be both wasteful and risks
    /// nondeterminism.
    pub nar_bytes: Vec<u8>,
}

/// NAR-serialize `dir`, hash it, and compute the CppNix source
/// store path + SRI narHash.
///
/// The `name` argument is the final `-<name>` segment of the
/// resulting store path.  For flake `path:` refs CppNix uses
/// `"source"` unconditionally.
///
/// # Errors
///
/// Returns a [`NarError`] if the path can't be serialized (e.g.
/// broken symlink, unreadable directory).
pub fn nar_hash_source_tree(dir: &Path, name: &str) -> Result<SourceHash, NarError> {
    let mut nar_bytes = Vec::new();
    {
        let mut cursor = Cursor::new(&mut nar_bytes);
        NarWriter::write_path(&mut cursor, dir)?;
    }

    // Inner sha256 of the NAR, in lowercase hex — fed to
    // `compute_fixed_output_hash` which expects hex.
    let digest = Sha256::digest(&nar_bytes);
    let digest_bytes = digest.to_vec();
    let hex: String = digest_bytes.iter().map(|b| format!("{b:02x}")).collect();

    let store_path = compute_fixed_output_hash("sha256", &hex, true, name);

    // SRI = `sha256-<base64>` over the RAW digest bytes (not the hex).
    let nar_hash = NixHash::new(HashAlgorithm::Sha256, digest_bytes.clone());
    let nar_hash_sri = nar_hash.to_sri();

    Ok(SourceHash {
        store_path,
        nar_hash_sri,
        nar_bytes,
    })
}

/// Base64 encode the SHA-256 of `bytes` without the `sha256-`
/// prefix.  Exposed for callers that already have NAR bytes in
/// hand (e.g. a cache hit).
#[must_use]
pub fn base64_sha256(bytes: &[u8]) -> String {
    base64_encode(&Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn mk_flake_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let flake_nix = dir.path().join("flake.nix");
        let mut f = std::fs::File::create(&flake_nix).unwrap();
        // Exact bytes we probed against CppNix.
        write!(f, "{{ outputs = {{ self }}: {{ value = 42; }}; }}\n").unwrap();
        dir
    }

    #[test]
    fn source_tree_produces_a_store_path_and_an_sri_hash() {
        let dir = mk_flake_dir();
        let sh = nar_hash_source_tree(dir.path(), "source").expect("nar hash");
        // Structural assertions — any NAR-hash-of-a-tree must have
        // these shapes.  The exact CppNix parity is asserted in an
        // integration test (requires nix binary).
        assert!(sh.store_path.starts_with("/nix/store/"));
        assert!(sh.store_path.ends_with("-source"));
        assert!(sh.nar_hash_sri.starts_with("sha256-"));
        assert!(!sh.nar_bytes.is_empty());
        assert!(sh.nar_bytes.starts_with(b"\r\x00\x00\x00\x00\x00\x00\x00nix-archive-1"),
            "NAR must begin with the magic header — got {:?}",
            &sh.nar_bytes[..16]);
    }
}
