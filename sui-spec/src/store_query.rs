//! Typed query AST over the typed store substrate.
//!
//! Composable predicates that match against [`StoreEntry`] +
//! [`RefIndex`] context.  Operators build queries in Rust or
//! tatara-lisp; the executor walks the inventory once and
//! returns matching entries.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::nar;
use crate::store_inventory::{RefIndex, StoreEntry};
use crate::SpecError;

/// Composable predicate over a `StoreEntry` (with optional
/// `RefIndex` context for reference-aware predicates).
#[derive(Debug, Clone)]
pub enum StorePredicate {
    /// Entry's basename (e.g. `hello-2.12`) matches this regex.
    NameMatches(String),
    /// Entry size in bytes ≥ this value.
    SizeAtLeast(u64),
    /// Entry size in bytes ≤ this value.
    SizeAtMost(u64),
    /// Entry's file_count ≥ this value.
    FileCountAtLeast(usize),
    /// Entry's NAR contents contain this literal byte sequence
    /// (case-sensitive, no regex).
    ContainsBytes(Vec<u8>),
    /// Entry's NAR file contents match this regex (UTF-8 lossy).
    ContentsMatch(String),
    /// Entry references at least one `/nix/store/<prefix>*` path
    /// where `<prefix>` matches this substring.
    HasReference(String),
    /// Logical AND.
    All(Vec<StorePredicate>),
    /// Logical OR.
    Any(Vec<StorePredicate>),
    /// Logical NOT.
    Not(Box<StorePredicate>),
}

/// Decide whether `entry` satisfies `predicate`.  `idx` is
/// optional context — predicates that don't need it (NameMatches,
/// SizeAtLeast, etc.) ignore it.
pub fn matches(
    entry: &StoreEntry,
    predicate: &StorePredicate,
    idx: Option<&RefIndex>,
) -> bool {
    match predicate {
        StorePredicate::NameMatches(pattern) => {
            regex::Regex::new(pattern).is_ok_and(|re| re.is_match(&entry.parsed.name))
        }
        StorePredicate::SizeAtLeast(n) => entry.size >= *n,
        StorePredicate::SizeAtMost(n) => entry.size <= *n,
        StorePredicate::FileCountAtLeast(n) => entry.file_count >= *n,
        StorePredicate::ContainsBytes(needle) => {
            let nar = match nar::encode(&entry.path) {
                Ok(b) => b,
                Err(_) => return false,
            };
            nar.windows(needle.len()).any(|w| w == needle)
        }
        StorePredicate::ContentsMatch(pattern) => {
            let re = match regex::bytes::Regex::new(pattern) {
                Ok(r) => r,
                Err(_) => return false,
            };
            let nar = match nar::encode(&entry.path) {
                Ok(b) => b,
                Err(_) => return false,
            };
            re.is_match(&nar)
        }
        StorePredicate::HasReference(substr) => {
            if let Some(idx) = idx {
                idx.refs_from(&entry.path).iter().any(|p| {
                    p.to_string_lossy().contains(substr)
                })
            } else {
                false
            }
        }
        StorePredicate::All(ps) => ps.iter().all(|p| matches(entry, p, idx)),
        StorePredicate::Any(ps) => ps.iter().any(|p| matches(entry, p, idx)),
        StorePredicate::Not(p) => !matches(entry, p, idx),
    }
}

// ── Named query — Lisp-authorable declarative form ────────────
//
// The full `StorePredicate` AST is too rich for the Lisp
// surface (recursive enums + closures don't translate cleanly
// to TataraDomain).  Instead we expose a flat-field form that
// captures the common cases: name regex / size bracket /
// contents regex / reference substring.  These compose with AND.

/// Typed Lisp-authorable named query.  Authored as
/// `(defstore-query :name X :description Y :name-regex ...)`.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defstore-query")]
pub struct StoreQuery {
    /// Stable name (catalog key).
    pub name: String,
    /// Operator-facing description.
    pub description: String,
    /// Name regex (empty = any name).
    #[serde(rename = "nameRegex", default)]
    pub name_regex: String,
    /// Minimum size in bytes (0 = no minimum).
    #[serde(rename = "minSize", default)]
    pub min_size: u64,
    /// Maximum size in bytes (0 = no maximum).
    #[serde(rename = "maxSize", default)]
    pub max_size: u64,
    /// Content regex (empty = any contents).
    #[serde(rename = "contentsRegex", default)]
    pub contents_regex: String,
    /// Reference substring (empty = no reference filter).
    #[serde(rename = "hasReference", default)]
    pub has_reference: String,
}

impl StoreQuery {
    /// Compile the named query to a `StorePredicate` AST that
    /// the executor consumes.  Empty fields are skipped.
    #[must_use]
    pub fn to_predicate(&self) -> StorePredicate {
        let mut clauses = Vec::new();
        if !self.name_regex.is_empty() {
            clauses.push(StorePredicate::NameMatches(self.name_regex.clone()));
        }
        if self.min_size > 0 {
            clauses.push(StorePredicate::SizeAtLeast(self.min_size));
        }
        if self.max_size > 0 {
            clauses.push(StorePredicate::SizeAtMost(self.max_size));
        }
        if !self.contents_regex.is_empty() {
            clauses.push(StorePredicate::ContentsMatch(self.contents_regex.clone()));
        }
        if !self.has_reference.is_empty() {
            clauses.push(StorePredicate::HasReference(self.has_reference.clone()));
        }
        if clauses.is_empty() {
            // Empty query matches everything (operator authored a
            // catch-all).
            StorePredicate::All(vec![])
        } else {
            StorePredicate::All(clauses)
        }
    }
}

pub const CANONICAL_STORE_QUERIES_LISP: &str =
    include_str!("../specs/store_queries.lisp");

/// Compile every authored query.
///
/// # Errors
///
/// Returns an error if the Lisp source can't be parsed.
pub fn load_canonical() -> Result<Vec<StoreQuery>, SpecError> {
    crate::loader::load_all::<StoreQuery>(CANONICAL_STORE_QUERIES_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_inventory::StoreEntry;
    use crate::store_layout::ParsedStorePath;

    fn entry(name: &str, size: u64, file_count: usize) -> StoreEntry {
        StoreEntry {
            path: std::path::PathBuf::from(format!("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-{name}")),
            parsed: ParsedStorePath {
                algorithm: None,
                hash: "a".repeat(32),
                name: name.to_string(),
                sub_path: None,
            },
            is_directory: false,
            file_count,
            size,
        }
    }

    #[test]
    fn name_matches_regex() {
        let e = entry("hello-2.12", 0, 1);
        assert!(matches(&e, &StorePredicate::NameMatches("^hello-.*".into()), None));
        assert!(!matches(&e, &StorePredicate::NameMatches("^bye".into()), None));
    }

    #[test]
    fn size_predicates_work() {
        let e = entry("x", 100, 1);
        assert!(matches(&e, &StorePredicate::SizeAtLeast(50), None));
        assert!(!matches(&e, &StorePredicate::SizeAtLeast(150), None));
        assert!(matches(&e, &StorePredicate::SizeAtMost(100), None));
    }

    #[test]
    fn and_or_not_compose() {
        let e = entry("hello-1.0", 100, 1);
        let p = StorePredicate::All(vec![
            StorePredicate::NameMatches("^hello".into()),
            StorePredicate::SizeAtLeast(50),
            StorePredicate::Not(Box::new(StorePredicate::SizeAtLeast(500))),
        ]);
        assert!(matches(&e, &p, None));
    }

    #[test]
    fn any_short_circuits_on_true() {
        let e = entry("hello", 100, 1);
        let p = StorePredicate::Any(vec![
            StorePredicate::NameMatches("^hello".into()),
            StorePredicate::SizeAtLeast(99999),  // false
        ]);
        assert!(matches(&e, &p, None));
    }
}
