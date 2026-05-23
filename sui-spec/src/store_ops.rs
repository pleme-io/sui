//! Typed store-operation primitives — parse, transform, query.
//!
//! This module is the substrate's first-class surface for
//! operating on Nix stores beyond what `nix-store` natively
//! supports.  Built on top of [`crate::nar`]'s encoder/decoder,
//! it provides:
//!
//! 1. [`ParsedNar`] — typed in-memory representation of a NAR
//!    archive.  Read once, walk arbitrarily, transform freely.
//! 2. [`StoreSlice`] — typed declaration of "a subset of /nix/store
//!    to operate on" with predicate-based selection.
//! 3. [`MaterializationPlan`] — typed plan for rematerializing a
//!    slice at a new store root (different prefix, drift detection,
//!    etc.).
//! 4. [`StoreTransform`] — typed AST for store-path-aware byte
//!    transforms (graft, rewrite, redact).
//!
//! Tatara-lisp authoring surface (`(defstore-operation …)` etc.)
//! lives in `specs/store_ops.lisp`; the typed border below is
//! consumed by both the Lisp interpreter and the Rust API.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed NAR tree ─────────────────────────────────────────────────

/// Parsed NAR archive — a typed tree of nodes that operators can
/// walk, query, and transform without re-decoding on each access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedNar {
    pub root: NarNode,
}

/// One node in the parsed NAR tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NarNode {
    File {
        executable: bool,
        contents: Vec<u8>,
    },
    Directory {
        /// Sorted by name — matches NAR's canonical ordering.
        entries: Vec<(String, NarNode)>,
    },
    Symlink {
        target: String,
    },
}

impl NarNode {
    /// Walk the tree top-down, calling `visitor` on every node
    /// with its relative path (`""` for the root).
    pub fn walk<F: FnMut(&str, &NarNode)>(&self, visitor: &mut F) {
        self.walk_with_prefix("", visitor);
    }

    fn walk_with_prefix<F: FnMut(&str, &NarNode)>(&self, prefix: &str, visitor: &mut F) {
        visitor(prefix, self);
        if let NarNode::Directory { entries } = self {
            for (name, child) in entries {
                let child_path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                child.walk_with_prefix(&child_path, visitor);
            }
        }
    }

    /// Count files in the tree (excluding directories + symlinks).
    #[must_use]
    pub fn file_count(&self) -> usize {
        let mut count = 0;
        self.walk(&mut |_, n| {
            if matches!(n, NarNode::File { .. }) {
                count += 1;
            }
        });
        count
    }

    /// Total bytes of file contents in the tree.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        let mut total = 0u64;
        self.walk(&mut |_, n| {
            if let NarNode::File { contents, .. } = n {
                total += contents.len() as u64;
            }
        });
        total
    }

    /// Find a node by path (relative, `/`-separated).  Returns
    /// `None` if the path doesn't exist in the tree.
    #[must_use]
    pub fn at_path(&self, path: &str) -> Option<&NarNode> {
        if path.is_empty() {
            return Some(self);
        }
        let (head, rest) = match path.split_once('/') {
            Some((h, r)) => (h, r),
            None => (path, ""),
        };
        match self {
            NarNode::Directory { entries } => {
                let (_, child) = entries.iter().find(|(n, _)| n == head)?;
                child.at_path(rest)
            }
            _ => None,
        }
    }
}

impl ParsedNar {
    /// Parse canonical NAR bytes into the typed tree.  Equivalent
    /// to [`crate::nar::decode`] but builds an in-memory tree
    /// instead of materializing to disk.
    ///
    /// # Errors
    ///
    /// Same typed errors as [`crate::nar::decode`].
    pub fn parse(bytes: &[u8]) -> Result<Self, crate::nar::NarDecodeError> {
        // Reuse the decoder framework by going through a temp dir
        // for now — symmetric materialise-then-walk.  A pure
        // streaming parser is a follow-up optimization; the
        // current shape is correct + uses the same wire format.
        let tmp = std::env::temp_dir().join(format!(
            "sui-parsed-nar-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos()).unwrap_or(0),
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        crate::nar::decode(bytes, &tmp)?;
        let root = read_node(&tmp)
            .map_err(|e| crate::nar::NarDecodeError::Io(e.to_string()))?;
        let _ = std::fs::remove_dir_all(&tmp);
        Ok(ParsedNar { root })
    }

    /// Re-encode the typed tree back to NAR bytes.  Round-trip
    /// equivalent: `ParsedNar::parse(encode(parse(b)))?.serialize() == b`.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        crate::nar::write_string_for_test(&mut buf, b"nix-archive-1");
        write_node(&mut buf, &self.root);
        buf
    }
}

fn read_node(path: &std::path::Path) -> std::io::Result<NarNode> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        return Ok(NarNode::Symlink {
            target: target.to_string_lossy().to_string(),
        });
    }
    if meta.is_file() {
        let contents = std::fs::read(path)?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        return Ok(NarNode::File { executable, contents });
    }
    // Directory
    let mut entries: Vec<_> = std::fs::read_dir(path)?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|e| e.file_name());
    let mut children = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let child = read_node(&entry.path())?;
        children.push((name, child));
    }
    Ok(NarNode::Directory { entries: children })
}

fn write_node(buf: &mut Vec<u8>, node: &NarNode) {
    crate::nar::write_string_for_test(buf, b"(");
    match node {
        NarNode::File { executable, contents } => {
            crate::nar::write_string_for_test(buf, b"type");
            crate::nar::write_string_for_test(buf, b"regular");
            if *executable {
                crate::nar::write_string_for_test(buf, b"executable");
                crate::nar::write_string_for_test(buf, b"");
            }
            crate::nar::write_string_for_test(buf, b"contents");
            crate::nar::write_string_for_test(buf, contents);
        }
        NarNode::Directory { entries } => {
            crate::nar::write_string_for_test(buf, b"type");
            crate::nar::write_string_for_test(buf, b"directory");
            for (name, child) in entries {
                crate::nar::write_string_for_test(buf, b"entry");
                crate::nar::write_string_for_test(buf, b"(");
                crate::nar::write_string_for_test(buf, b"name");
                crate::nar::write_string_for_test(buf, name.as_bytes());
                crate::nar::write_string_for_test(buf, b"node");
                write_node(buf, child);
                crate::nar::write_string_for_test(buf, b")");
            }
        }
        NarNode::Symlink { target } => {
            crate::nar::write_string_for_test(buf, b"type");
            crate::nar::write_string_for_test(buf, b"symlink");
            crate::nar::write_string_for_test(buf, b"target");
            crate::nar::write_string_for_test(buf, target.as_bytes());
        }
    }
    crate::nar::write_string_for_test(buf, b")");
}

// ── Typed StoreSlice + MaterializationPlan ─────────────────────────

/// Typed declaration of "a subset of /nix/store" for operations.
/// Lisp surface: `(defstore-slice …)`.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defstore-slice")]
pub struct StoreSlice {
    /// Stable name (used in reports + tests).
    pub name: String,
    /// Source store root (typically `/nix/store`).
    #[serde(rename = "sourceRoot")]
    pub source_root: String,
    /// Selection predicate — a regex over the basename.  Matches
    /// every path whose name (e.g. `0mrdxm84...-source`) matches.
    #[serde(rename = "namePattern")]
    pub name_pattern: String,
    /// Maximum entries to include from the source.  Prevents
    /// disk-blowup when probing large stores.
    #[serde(rename = "maxEntries")]
    pub max_entries: usize,
    /// Skip entries larger than this size (bytes) — avoids
    /// rematerializing massive sources.  0 = unlimited.
    #[serde(rename = "maxSizeBytes", default)]
    pub max_size_bytes: u64,
}

/// Plan for rematerializing a slice at an alternate root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializationPlan {
    /// Source path under `source_root`.
    pub source: std::path::PathBuf,
    /// Destination path under the operator's chosen root.
    pub dest: std::path::PathBuf,
}

/// Outcome of running a [`MaterializationPlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializationOutcome {
    pub source: std::path::PathBuf,
    pub dest: std::path::PathBuf,
    pub source_nar_sha256: String,
    pub dest_nar_sha256: String,
    pub byte_equivalent: bool,
    pub source_size: u64,
    pub file_count: usize,
}

/// Build the materialization plan from a typed slice.  Scans
/// `slice.source_root`, applies the name pattern + max entries +
/// size cap, returns a list of (source, dest) pairs.
///
/// # Errors
///
/// Returns `SpecError::Interp { phase: "slice-scan" }` if the
/// source dir can't be read.
pub fn build_materialization_plan(
    slice: &StoreSlice,
    dest_root: &std::path::Path,
) -> Result<Vec<MaterializationPlan>, SpecError> {
    let pattern = regex::Regex::new(&slice.name_pattern).map_err(|e| SpecError::Interp {
        phase: "slice-pattern".into(),
        message: format!("invalid name regex: {e}"),
    })?;
    let source_root = std::path::Path::new(&slice.source_root);
    let entries = std::fs::read_dir(source_root).map_err(|e| SpecError::Interp {
        phase: "slice-scan".into(),
        message: format!("read_dir {}: {e}", source_root.display()),
    })?;

    let mut plans: Vec<MaterializationPlan> = Vec::new();
    for entry in entries.flatten() {
        if plans.len() >= slice.max_entries {
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !pattern.is_match(&name) {
            continue;
        }
        // Size cap: estimate via metadata.len() for files,
        // skip dirs > max_size (we don't du each dir to keep it cheap).
        if slice.max_size_bytes > 0 {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() && meta.len() > slice.max_size_bytes {
                    continue;
                }
            }
        }
        plans.push(MaterializationPlan {
            source: entry.path(),
            dest: dest_root.join(&name),
        });
    }
    Ok(plans)
}

/// Run a materialization plan: NAR-encode the source, decode to
/// the destination, then compare NAR sha256.  Byte-equivalence
/// of the rematerialized tree against the source is the proof.
///
/// # Errors
///
/// Propagates encoder / decoder / hash errors.
pub fn run_materialization(
    plan: &MaterializationPlan,
) -> Result<MaterializationOutcome, SpecError> {
    use sha2::Digest;

    let source_nar = crate::nar::encode(&plan.source).map_err(|e| SpecError::Interp {
        phase: "materialize-encode".into(),
        message: format!("encode {}: {e}", plan.source.display()),
    })?;
    let source_hash = sha2::Sha256::digest(&source_nar);
    let source_hash_hex = hex_encode(&source_hash);

    if plan.dest.exists() {
        std::fs::remove_dir_all(&plan.dest).ok();
    }
    crate::nar::decode(&source_nar, &plan.dest).map_err(|e| SpecError::Interp {
        phase: "materialize-decode".into(),
        message: format!("decode → {}: {e}", plan.dest.display()),
    })?;

    let dest_nar = crate::nar::encode(&plan.dest).map_err(|e| SpecError::Interp {
        phase: "materialize-encode".into(),
        message: format!("encode {}: {e}", plan.dest.display()),
    })?;
    let dest_hash = sha2::Sha256::digest(&dest_nar);
    let dest_hash_hex = hex_encode(&dest_hash);

    let parsed = ParsedNar::parse(&source_nar).map_err(|e| SpecError::Interp {
        phase: "materialize-parse".into(),
        message: format!("parse: {e}"),
    })?;

    Ok(MaterializationOutcome {
        source: plan.source.clone(),
        dest: plan.dest.clone(),
        source_nar_sha256: source_hash_hex.clone(),
        dest_nar_sha256: dest_hash_hex.clone(),
        byte_equivalent: source_hash_hex == dest_hash_hex,
        source_size: parsed.root.total_bytes(),
        file_count: parsed.root.file_count(),
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ── Canonical Lisp spec ────────────────────────────────────────────

pub const CANONICAL_STORE_OPS_LISP: &str =
    include_str!("../specs/store_ops.lisp");

/// Compile every authored store slice declaration.
///
/// # Errors
///
/// Returns an error if the Lisp source can't be parsed.
pub fn load_canonical_slices() -> Result<Vec<StoreSlice>, SpecError> {
    crate::loader::load_all::<StoreSlice>(CANONICAL_STORE_OPS_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsed_nar_walk_counts_correctly() {
        let tmp = std::env::temp_dir().join("sui-parsed-nar-walk-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a"), b"aaaa").unwrap();
        std::fs::write(tmp.join("b"), b"bb").unwrap();
        std::fs::create_dir_all(tmp.join("d")).unwrap();
        std::fs::write(tmp.join("d/c"), b"ccc").unwrap();
        let nar = crate::nar::encode(&tmp).unwrap();
        let parsed = ParsedNar::parse(&nar).unwrap();
        assert_eq!(parsed.root.file_count(), 3);
        assert_eq!(parsed.root.total_bytes(), 4 + 2 + 3);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parsed_nar_at_path_finds_nested_file() {
        let tmp = std::env::temp_dir().join("sui-parsed-nar-at-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("sub/hello"), b"world").unwrap();
        let nar = crate::nar::encode(&tmp).unwrap();
        let parsed = ParsedNar::parse(&nar).unwrap();
        let node = parsed.root.at_path("sub/hello").unwrap();
        assert!(matches!(node, NarNode::File { .. }));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn canonical_slices_parse() {
        let slices = load_canonical_slices().unwrap();
        assert!(!slices.is_empty(), "at least one canonical slice authored");
    }
}
