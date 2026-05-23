//! Typed transformation AST over ParsedNar trees.
//!
//! `StoreTransform` is the substrate's declarative authoring
//! surface for operating on NAR contents.  Operators write
//! `(defstore-transform :name … :match-kind … :pattern …)` in
//! Lisp; the Rust executor applies the transforms in declared
//! order to any ParsedNar tree.
//!
//! Three transform kinds:
//!
//! - **FileContents** — regex match-and-replace over the bytes of
//!   every file in the tree.  Used for redaction, secret-stripping,
//!   build-id rewriting.
//! - **StorePathReference** — replaces every literal
//!   `/nix/store/<from>-*` occurrence (anywhere in file contents
//!   OR symlink targets) with `/nix/store/<to>-*`.  Mirrors
//!   cppnix's referrer-graft semantics.
//! - **EntryName** — renames the top-level entries of a directory
//!   matching a regex.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;
use crate::store_ops::{NarNode, ParsedNar};

/// Typed transformation declaration.  Authored as
/// `(defstore-transform …)` in Lisp.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defstore-transform")]
pub struct StoreTransform {
    /// Stable name (used for catalog lookup + report rows).
    pub name: String,
    /// Operator-facing description.
    pub description: String,
    /// Kind discriminator.
    #[serde(rename = "matchKind")]
    pub match_kind: TransformKind,
    /// Pattern: regex for `FileContents`, prefix for
    /// `StorePathReference` and `EntryName`.
    pub pattern: String,
    /// Replacement string.
    pub replacement: String,
}

/// Discriminator for the three transform shapes.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransformKind {
    /// regex-replace over file bytes (UTF-8 lossy).
    FileContents,
    /// Replace `/nix/store/<from>-*` with `/nix/store/<to>-*`
    /// throughout file contents + symlink targets.
    StorePathReference,
    /// Rename top-level directory entries matching the regex.
    EntryName,
}

/// Outcome of applying one transform to one node.  Aggregated by
/// the executor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransformOutcome {
    /// Transform name (for report rows).
    pub transform_name: String,
    /// Number of file-content rewrites applied.
    pub file_rewrites: usize,
    /// Number of store-path-reference rewrites applied.
    pub ref_rewrites: usize,
    /// Number of entries renamed.
    pub entries_renamed: usize,
}

impl TransformOutcome {
    /// `true` if the transform produced no changes (idempotent
    /// re-apply path).
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.file_rewrites == 0
            && self.ref_rewrites == 0
            && self.entries_renamed == 0
    }
}

/// Apply a sequence of transforms to a `ParsedNar`.  Each
/// transform fires in declared order; outcomes are returned in
/// the same order.  The tree is mutated in place.
///
/// # Errors
///
/// - `transform-bad-regex` for invalid regexes in FileContents/EntryName.
/// - `transform-bad-prefix` for malformed StorePathReference patterns.
pub fn apply_all(
    tree: &mut ParsedNar,
    transforms: &[StoreTransform],
) -> Result<Vec<TransformOutcome>, SpecError> {
    let mut outcomes = Vec::with_capacity(transforms.len());
    for t in transforms {
        let outcome = apply_one(&mut tree.root, t)?;
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// Apply one transform, returning its outcome.
///
/// # Errors
///
/// Same as [`apply_all`].
pub fn apply_one(
    node: &mut NarNode,
    transform: &StoreTransform,
) -> Result<TransformOutcome, SpecError> {
    match transform.match_kind {
        TransformKind::FileContents => {
            let re = regex::bytes::Regex::new(&transform.pattern)
                .map_err(|e| SpecError::Interp {
                    phase: "transform-bad-regex".into(),
                    message: format!("{}: {e}", transform.name),
                })?;
            let replacement = transform.replacement.as_bytes();
            let mut count = 0usize;
            walk_files(node, &mut |contents| {
                let new_bytes = re.replace_all(contents, replacement).into_owned();
                if new_bytes != *contents {
                    count += 1;
                    *contents = new_bytes;
                }
            });
            Ok(TransformOutcome {
                transform_name: transform.name.clone(),
                file_rewrites: count,
                ..Default::default()
            })
        }
        TransformKind::StorePathReference => {
            // `pattern` is the 32-char base32 hash to match;
            // `replacement` is the 32-char base32 hash to swap in.
            // Both must be the same length so byte-offset math
            // stays sane.
            if transform.pattern.len() != transform.replacement.len() {
                return Err(SpecError::Interp {
                    phase: "transform-bad-prefix".into(),
                    message: format!(
                        "{}: StorePathReference requires same-length pattern + replacement (got {} vs {})",
                        transform.name,
                        transform.pattern.len(),
                        transform.replacement.len(),
                    ),
                });
            }
            let from = transform.pattern.as_bytes();
            let to = transform.replacement.as_bytes();
            let mut count = 0usize;
            walk_files(node, &mut |contents| {
                let mut new_bytes = Vec::with_capacity(contents.len());
                let mut i = 0;
                while i < contents.len() {
                    if i + from.len() <= contents.len()
                        && &contents[i..i + from.len()] == from
                    {
                        new_bytes.extend_from_slice(to);
                        count += 1;
                        i += from.len();
                    } else {
                        new_bytes.push(contents[i]);
                        i += 1;
                    }
                }
                *contents = new_bytes;
            });
            walk_symlinks(node, &mut |target| {
                if target.contains(&transform.pattern) {
                    *target = target.replace(&transform.pattern, &transform.replacement);
                    count += 1;
                }
            });
            Ok(TransformOutcome {
                transform_name: transform.name.clone(),
                ref_rewrites: count,
                ..Default::default()
            })
        }
        TransformKind::EntryName => {
            let re = regex::Regex::new(&transform.pattern)
                .map_err(|e| SpecError::Interp {
                    phase: "transform-bad-regex".into(),
                    message: format!("{}: {e}", transform.name),
                })?;
            let replacement = &transform.replacement;
            let mut count = 0usize;
            if let NarNode::Directory { entries } = node {
                for (name, _) in entries.iter_mut() {
                    let new_name = re.replace_all(name, replacement.as_str()).into_owned();
                    if new_name != *name {
                        *name = new_name;
                        count += 1;
                    }
                }
                // Re-sort to maintain canonical NAR ordering.
                entries.sort_by(|a, b| a.0.cmp(&b.0));
            }
            Ok(TransformOutcome {
                transform_name: transform.name.clone(),
                entries_renamed: count,
                ..Default::default()
            })
        }
    }
}

/// Walk every file node in the tree, calling `f(contents)` so
/// the visitor can mutate the bytes in place.
fn walk_files<F: FnMut(&mut Vec<u8>)>(node: &mut NarNode, f: &mut F) {
    match node {
        NarNode::File { contents, .. } => f(contents),
        NarNode::Directory { entries } => {
            for (_, child) in entries.iter_mut() {
                walk_files(child, f);
            }
        }
        NarNode::Symlink { .. } => {}
    }
}

/// Walk every symlink node in the tree, calling `f(target)`.
fn walk_symlinks<F: FnMut(&mut String)>(node: &mut NarNode, f: &mut F) {
    match node {
        NarNode::Symlink { target } => f(target),
        NarNode::Directory { entries } => {
            for (_, child) in entries.iter_mut() {
                walk_symlinks(child, f);
            }
        }
        NarNode::File { .. } => {}
    }
}

// ── Canonical Lisp spec ────────────────────────────────────────────

pub const CANONICAL_STORE_TRANSFORMS_LISP: &str =
    include_str!("../specs/store_transforms.lisp");

/// Compile every authored transform.
///
/// # Errors
///
/// Returns an error if the Lisp source can't be parsed.
pub fn load_canonical() -> Result<Vec<StoreTransform>, SpecError> {
    crate::loader::load_all::<StoreTransform>(CANONICAL_STORE_TRANSFORMS_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_ops::NarNode;

    fn file(s: &[u8]) -> NarNode {
        NarNode::File { executable: false, contents: s.to_vec() }
    }
    fn dir(entries: Vec<(&str, NarNode)>) -> NarNode {
        NarNode::Directory {
            entries: entries.into_iter().map(|(n, e)| (n.to_string(), e)).collect(),
        }
    }
    fn symlink(target: &str) -> NarNode {
        NarNode::Symlink { target: target.to_string() }
    }

    #[test]
    fn canonical_transforms_parse() {
        let xs = load_canonical().unwrap();
        assert!(!xs.is_empty());
    }

    #[test]
    fn file_contents_regex_rewrites() {
        let mut tree = ParsedNar { root: dir(vec![
            ("a", file(b"hello world")),
            ("b", file(b"world peace")),
        ]) };
        let t = StoreTransform {
            name: "test".into(),
            description: "world → planet".into(),
            match_kind: TransformKind::FileContents,
            pattern: "world".into(),
            replacement: "planet".into(),
        };
        let out = apply_one(&mut tree.root, &t).unwrap();
        assert_eq!(out.file_rewrites, 2);

        if let NarNode::Directory { entries } = &tree.root {
            if let NarNode::File { contents, .. } = &entries[0].1 {
                assert_eq!(contents, b"hello planet");
            }
            if let NarNode::File { contents, .. } = &entries[1].1 {
                assert_eq!(contents, b"planet peace");
            }
        }
    }

    #[test]
    fn store_path_reference_rewrites_in_files_and_symlinks() {
        let mut tree = ParsedNar { root: dir(vec![
            ("file", file(b"link to /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x")),
            ("ln",   symlink("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x")),
        ]) };
        let t = StoreTransform {
            name: "graft".into(),
            description: "".into(),
            match_kind: TransformKind::StorePathReference,
            pattern: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            replacement: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        };
        let out = apply_one(&mut tree.root, &t).unwrap();
        assert_eq!(out.ref_rewrites, 2);  // 1 in file + 1 in symlink

        if let NarNode::Directory { entries } = &tree.root {
            if let NarNode::File { contents, .. } = &entries[0].1 {
                assert!(contents.windows(32).any(|w| w == b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
            }
            if let NarNode::Symlink { target } = &entries[1].1 {
                assert_eq!(target, "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-x");
            }
        }
    }

    #[test]
    fn store_path_reference_rejects_different_length() {
        let mut tree = ParsedNar { root: file(b"") };
        let t = StoreTransform {
            name: "bad-len".into(),
            description: "".into(),
            match_kind: TransformKind::StorePathReference,
            pattern: "short".into(),
            replacement: "longer".into(),
        };
        let err = apply_one(&mut tree.root, &t).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "transform-bad-prefix"),
            _ => panic!("expected transform-bad-prefix"),
        }
    }

    #[test]
    fn entry_name_renames_top_level_only() {
        let mut tree = ParsedNar { root: dir(vec![
            ("old-x", file(b"")),
            ("old-y", file(b"")),
        ]) };
        let t = StoreTransform {
            name: "rename".into(),
            description: "".into(),
            match_kind: TransformKind::EntryName,
            pattern: "^old-".into(),
            replacement: "new-".into(),
        };
        let out = apply_one(&mut tree.root, &t).unwrap();
        assert_eq!(out.entries_renamed, 2);
        if let NarNode::Directory { entries } = &tree.root {
            assert_eq!(entries[0].0, "new-x");
            assert_eq!(entries[1].0, "new-y");
        }
    }

    #[test]
    fn idempotent_reapply_is_noop() {
        let mut tree = ParsedNar { root: file(b"hello world") };
        let t = StoreTransform {
            name: "test".into(),
            description: "".into(),
            match_kind: TransformKind::FileContents,
            pattern: "world".into(),
            replacement: "planet".into(),
        };
        let first = apply_one(&mut tree.root, &t).unwrap();
        let second = apply_one(&mut tree.root, &t).unwrap();
        assert_eq!(first.file_rewrites, 1);
        assert!(second.is_noop());
    }
}
