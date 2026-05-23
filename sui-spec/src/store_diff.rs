//! Typed diff between two ParsedNar trees.
//!
//! Operators need a substrate-level "what changed between these
//! two NAR archives?" answer.  This module produces a typed
//! [`Diff`] AST that downstream tools render however they like
//! (JSON, terminal, IDE).
//!
//! Composes against [`crate::store_ops::ParsedNar`]; doesn't
//! touch the filesystem.

use crate::store_ops::NarNode;

/// Path-keyed diff between two NAR trees.  Paths are relative
/// to the diff root, slash-separated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub entries: Vec<DiffEntry>,
}

/// One diff record for a single path or pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffEntry {
    /// Path exists only in B.
    AddedFile { path: String, size: usize },
    /// Path exists only in A.
    RemovedFile { path: String, size: usize },
    /// Path exists in both, contents differ.
    ChangedFile { path: String, size_a: usize, size_b: usize },
    /// Path is a file in one tree and not in the other shape.
    KindChanged { path: String, from: String, to: String },
    /// Symlink target changed.
    SymlinkChanged { path: String, from: String, to: String },
    /// Executable bit changed.
    ExecutableChanged { path: String, executable_now: bool },
}

impl Diff {
    /// Number of differing records.
    #[must_use]
    pub fn len(&self) -> usize { self.entries.len() }

    /// `true` if both trees are byte-identical.
    #[must_use]
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    /// Group the diff by category (counts).
    #[must_use]
    pub fn histogram(&self) -> DiffHistogram {
        let mut h = DiffHistogram::default();
        for e in &self.entries {
            match e {
                DiffEntry::AddedFile { .. }        => h.added += 1,
                DiffEntry::RemovedFile { .. }      => h.removed += 1,
                DiffEntry::ChangedFile { .. }      => h.changed += 1,
                DiffEntry::KindChanged { .. }      => h.kind_changed += 1,
                DiffEntry::SymlinkChanged { .. }   => h.symlink_changed += 1,
                DiffEntry::ExecutableChanged { .. } => h.executable_changed += 1,
            }
        }
        h
    }
}

/// Aggregate counts.  Useful for the operator-facing summary.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiffHistogram {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub kind_changed: usize,
    pub symlink_changed: usize,
    pub executable_changed: usize,
}

impl DiffHistogram {
    /// Total differing records across all categories.
    #[must_use]
    pub fn total(&self) -> usize {
        self.added + self.removed + self.changed + self.kind_changed
            + self.symlink_changed + self.executable_changed
    }
}

/// Diff two NAR trees, producing the typed [`Diff`].
#[must_use]
pub fn diff(a: &NarNode, b: &NarNode) -> Diff {
    let mut entries = Vec::new();
    diff_nodes("", a, b, &mut entries);
    Diff { entries }
}

fn kind_label(node: &NarNode) -> &'static str {
    match node {
        NarNode::File { .. }      => "file",
        NarNode::Directory { .. } => "directory",
        NarNode::Symlink { .. }   => "symlink",
    }
}

fn diff_nodes(path: &str, a: &NarNode, b: &NarNode, out: &mut Vec<DiffEntry>) {
    match (a, b) {
        (NarNode::File { executable: ea, contents: ca },
         NarNode::File { executable: eb, contents: cb }) => {
            if ca != cb {
                out.push(DiffEntry::ChangedFile {
                    path: path.to_string(),
                    size_a: ca.len(),
                    size_b: cb.len(),
                });
            }
            if ea != eb {
                out.push(DiffEntry::ExecutableChanged {
                    path: path.to_string(),
                    executable_now: *eb,
                });
            }
        }
        (NarNode::Symlink { target: ta }, NarNode::Symlink { target: tb }) => {
            if ta != tb {
                out.push(DiffEntry::SymlinkChanged {
                    path: path.to_string(),
                    from: ta.clone(),
                    to: tb.clone(),
                });
            }
        }
        (NarNode::Directory { entries: ea }, NarNode::Directory { entries: eb }) => {
            // Walk both sorted child sets.  Entries are already
            // sorted because ParsedNar::parse + NAR encoder both
            // canonicalize.
            let mut i = 0usize;
            let mut j = 0usize;
            while i < ea.len() && j < eb.len() {
                let (na, ca) = &ea[i];
                let (nb, cb) = &eb[j];
                match na.cmp(nb) {
                    std::cmp::Ordering::Equal => {
                        let child_path = if path.is_empty() {
                            na.clone()
                        } else {
                            format!("{path}/{na}")
                        };
                        diff_nodes(&child_path, ca, cb, out);
                        i += 1;
                        j += 1;
                    }
                    std::cmp::Ordering::Less => {
                        // na is removed (in A but not B).
                        let child_path = if path.is_empty() {
                            na.clone()
                        } else {
                            format!("{path}/{na}")
                        };
                        record_subtree_only_in_one(&child_path, ca, out, /*added=*/false);
                        i += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        // nb is added (in B but not A).
                        let child_path = if path.is_empty() {
                            nb.clone()
                        } else {
                            format!("{path}/{nb}")
                        };
                        record_subtree_only_in_one(&child_path, cb, out, /*added=*/true);
                        j += 1;
                    }
                }
            }
            while i < ea.len() {
                let (na, ca) = &ea[i];
                let child_path = if path.is_empty() { na.clone() } else { format!("{path}/{na}") };
                record_subtree_only_in_one(&child_path, ca, out, /*added=*/false);
                i += 1;
            }
            while j < eb.len() {
                let (nb, cb) = &eb[j];
                let child_path = if path.is_empty() { nb.clone() } else { format!("{path}/{nb}") };
                record_subtree_only_in_one(&child_path, cb, out, /*added=*/true);
                j += 1;
            }
        }
        _ => {
            out.push(DiffEntry::KindChanged {
                path: path.to_string(),
                from: kind_label(a).to_string(),
                to: kind_label(b).to_string(),
            });
        }
    }
}

fn record_subtree_only_in_one(
    path: &str,
    node: &NarNode,
    out: &mut Vec<DiffEntry>,
    added: bool,
) {
    match node {
        NarNode::File { contents, .. } => {
            if added {
                out.push(DiffEntry::AddedFile { path: path.to_string(), size: contents.len() });
            } else {
                out.push(DiffEntry::RemovedFile { path: path.to_string(), size: contents.len() });
            }
        }
        NarNode::Directory { entries } => {
            for (name, child) in entries {
                let child_path = format!("{path}/{name}");
                record_subtree_only_in_one(&child_path, child, out, added);
            }
        }
        NarNode::Symlink { target } => {
            if added {
                out.push(DiffEntry::AddedFile { path: path.to_string(), size: target.len() });
            } else {
                out.push(DiffEntry::RemovedFile { path: path.to_string(), size: target.len() });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_ops::{NarNode, ParsedNar};

    fn file(s: &[u8]) -> NarNode {
        NarNode::File { executable: false, contents: s.to_vec() }
    }
    fn dir(entries: Vec<(&str, NarNode)>) -> NarNode {
        NarNode::Directory {
            entries: entries.into_iter().map(|(n, c)| (n.to_string(), c)).collect(),
        }
    }

    #[test]
    fn identical_trees_have_empty_diff() {
        let a = dir(vec![("x", file(b"hello")), ("y", file(b"world"))]);
        let b = dir(vec![("x", file(b"hello")), ("y", file(b"world"))]);
        let d = diff(&a, &b);
        assert!(d.is_empty());
    }

    #[test]
    fn changed_file_detected() {
        let a = file(b"hello");
        let b = file(b"hellp");
        let d = diff(&a, &b);
        assert_eq!(d.len(), 1);
        match &d.entries[0] {
            DiffEntry::ChangedFile { size_a, size_b, .. } => {
                assert_eq!(*size_a, 5);
                assert_eq!(*size_b, 5);
            }
            other => panic!("expected ChangedFile, got {other:?}"),
        }
    }

    #[test]
    fn added_and_removed_at_same_level() {
        let a = dir(vec![("x", file(b"x")), ("y", file(b"y"))]);
        let b = dir(vec![("y", file(b"y")), ("z", file(b"z"))]);
        let d = diff(&a, &b);
        let h = d.histogram();
        assert_eq!(h.removed, 1);  // x
        assert_eq!(h.added, 1);    // z
    }

    #[test]
    fn nested_directory_diff() {
        let a = dir(vec![("sub", dir(vec![("a", file(b"aa"))]))]);
        let b = dir(vec![("sub", dir(vec![("b", file(b"bb"))]))]);
        let d = diff(&a, &b);
        assert_eq!(d.len(), 2);  // 1 added, 1 removed
        let h = d.histogram();
        assert_eq!(h.added, 1);
        assert_eq!(h.removed, 1);
    }

    #[test]
    fn kind_change_detected() {
        let a = file(b"");
        let b = NarNode::Symlink { target: "/x".into() };
        let d = diff(&a, &b);
        assert_eq!(d.len(), 1);
        assert!(matches!(d.entries[0], DiffEntry::KindChanged { .. }));
    }

    #[test]
    fn executable_change_detected() {
        let a = NarNode::File { executable: false, contents: b"x".to_vec() };
        let b = NarNode::File { executable: true,  contents: b"x".to_vec() };
        let d = diff(&a, &b);
        assert_eq!(d.len(), 1);
        match &d.entries[0] {
            DiffEntry::ExecutableChanged { executable_now, .. } => assert!(*executable_now),
            _ => panic!(),
        }
    }

    #[test]
    fn symlink_target_change_detected() {
        let a = NarNode::Symlink { target: "/a".into() };
        let b = NarNode::Symlink { target: "/b".into() };
        let d = diff(&a, &b);
        assert_eq!(d.len(), 1);
        match &d.entries[0] {
            DiffEntry::SymlinkChanged { from, to, .. } => {
                assert_eq!(from, "/a");
                assert_eq!(to, "/b");
            }
            _ => panic!(),
        }
    }

    /// Materialize-then-parse round-trip + diff against original
    /// should be empty.  Substrate invariant.
    #[test]
    fn parsed_nar_self_diff_is_empty() {
        let tmp = std::env::temp_dir().join("sui-store-diff-self-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("x"), b"hello").unwrap();
        std::fs::write(tmp.join("y"), b"world").unwrap();
        let nar = crate::nar::encode(&tmp).unwrap();
        let parsed1 = ParsedNar::parse(&nar).unwrap();
        let parsed2 = ParsedNar::parse(&nar).unwrap();
        let d = diff(&parsed1.root, &parsed2.root);
        assert!(d.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
