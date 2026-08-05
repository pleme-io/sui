//! The **narhash → store-hash reverse index**: which narinfos advertise a NAR.
//!
//! # Why this exists
//!
//! The two halves of a binary cache are keyed differently and always have been:
//!
//! | Artifact | Key |
//! |---|---|
//! | narinfo | the 32-char **store-path** hash |
//! | NAR blob | the **narhash** — `nar/<filehash>.nar.xz`, taken from the narinfo's `URL:` |
//!
//! Going *forward* is easy: read the narinfo, take `URL:`. Going *backward* —
//! from a NAR to the narinfo(s) that advertise it — was not expressible at all,
//! and three backends carried a comment saying exactly that while
//! [`delete`](super::StorageBackend::delete) worked around the gap by
//! best-effort-guessing `nar/{store-hash}.{xz,zst,nar}`. That guess is wrong on
//! both sides: it deletes keys that were never this path's NAR, and it leaves
//! the real NAR behind.
//!
//! # What the missing direction costs
//!
//! **A narinfo whose advertised NAR is gone is worse than a miss.** A client
//! fetches `<hash>.narinfo` (200 OK, `URL: nar/…`), fetches that NAR, and gets
//! 404. Nix treats a missing *advertised* NAR as a hard failure, not a cache
//! miss — the same outage class as 2026-07-26, where 500s from a substituter
//! failed every build on the cluster. So nothing may remove a NAR without first
//! answering "who still advertises it?", and that question needs this index.
//!
//! Two narinfos genuinely can advertise one NAR: a NAR serializes a store path's
//! *contents*, not its name, so two store paths with byte-identical contents
//! produce one narhash and one `URL:`. Removing either path must not take the
//! NAR the other one still points at. Hence a **set** of referrers, not one.
//!
//! # The shape
//!
//! One edge per `(nar_path, store_hash)` pair — never a set-valued record.
//! Recording is then a blind write of a key that names its own content, so two
//! concurrent writers cannot lose each other's edge the way a
//! read-modify-write of a shared set would. Backends that key by string
//! ([`RedisBackend`](super::RedisBackend), [`S3Storage`](super::S3Storage),
//! [`PgStorageBackend`](super::PgStorageBackend)) store the edge under
//! [`NarRefKey`]; [`LocalStorage`](super::LocalStorage) mirrors the same shape
//! as an empty file at `<root>/nar-refs/<nar_path>/<store_hash>`.
//!
//! # Tier honesty
//!
//! - The **decision** to have an index is *parse-time-rejected*:
//!   [`StorageBackend::nar_ref_index`](super::StorageBackend::nar_ref_index) is
//!   required and has no default, so a new backend cannot inherit a silently
//!   empty one — the same mechanism, and for the same reason, as
//!   [`nar_residency`](super::StorageBackend::nar_residency).
//! - The **maintenance** is *structural but overridable*: `put_narinfo` and
//!   `delete` are provided methods that record and forget the edge, so a backend
//!   implements only the raw record verbs and cannot forget to index. A backend
//!   that overrides them can still get it wrong; that is caught by
//!   `every_production_backend_pairs_its_nar_with_its_narinfo` in CI, not by the
//!   type system. **Only-mitigated, not unrepresentable.**
//! - A store written **before** the index existed has no edges. `delete` on such
//!   a store behaves as it was always intended to (it removes its own NAR) but
//!   cannot see a co-referrer, so the strand is possible until
//!   [`reindex_nar_refs`](super::StorageBackend::reindex_nar_refs) has run once.
//!   That is a migration gap, stated, not a property of the index.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::StoreError;

/// Key-space prefix owning every reverse edge.
///
/// Deliberately a sibling of `nar/` rather than a child: a Nix client only ever
/// fetches `nix-cache-info`, `<hash>.narinfo` and the exact `URL:` a narinfo
/// advertises, so an extra top-level directory is invisible to it, while a child
/// of `nar/` would sit in the namespace a NAR key is drawn from.
pub const NAR_REF_PREFIX: &str = "nar-refs/";

/// The typed key of one reverse edge: "`hash`'s narinfo advertises `nar_path`".
///
/// A `Display` surface rather than an ad-hoc `format!` at each backend, so the
/// four key-value tiers cannot drift into four encodings of the same edge
/// (★★ TYPED EMISSION).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NarRefKey<'a> {
    /// The advertised NAR path, e.g. `nar/<filehash>.nar.xz`.
    pub nar_path: &'a str,
    /// The 32-char store-path hash of the narinfo advertising it.
    pub hash: &'a str,
}

impl fmt::Display for NarRefKey<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{NAR_REF_PREFIX}{}/{}", self.nar_path, self.hash)
    }
}

/// The typed key **prefix** enumerating every edge into one NAR.
///
/// The trailing `/` is load-bearing: without it `nar/ab.nar` would also scan
/// `nar/ab.nar.xz`'s edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NarRefScan<'a> {
    /// The advertised NAR path whose referrers are wanted.
    pub nar_path: &'a str,
}

impl fmt::Display for NarRefScan<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{NAR_REF_PREFIX}{}/", self.nar_path)
    }
}

/// Recover the referring store-path hash from a scanned edge key.
///
/// Returns `None` for a key that is not under `scan` — a listing that returns a
/// neighbouring key must not be silently read as a referrer.
#[must_use]
pub fn referrer_of<'k>(scan: &NarRefScan<'_>, key: &'k str) -> Option<&'k str> {
    let prefix = scan.to_string();
    let rest = key.strip_prefix(&prefix)?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(rest)
}

/// Whether a narinfo's `URL:` is a NAR path this store can safely address.
///
/// A narinfo is *input* — it arrives over `PUT /<hash>.narinfo` — and its `URL:`
/// is used both as a key and, on [`LocalStorage`](super::LocalStorage), as a
/// path joined onto the cache root. `URL: ../../etc/passwd` therefore has to be
/// refused at the boundary rather than sanitized at each use; this predicate is
/// that boundary.
///
/// Accepts a non-empty relative path of non-empty segments, none of which is
/// `.` or `..`, with no control characters and no backslash.
#[must_use]
pub fn is_addressable_nar_path(url: &str) -> bool {
    !url.is_empty()
        && !url.starts_with('/')
        && !url.contains('\\')
        && !url.chars().any(char::is_control)
        && url.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// The NAR path a stored narinfo advertises, if it advertises an addressable
/// one.
///
/// `None` covers both "no `URL:` line here" and "its `URL:` is not
/// addressable". Both mean the same thing to a caller: there is no NAR here we
/// are entitled to key, index, or delete.
///
/// # Why this reads the one field instead of calling `NarInfo::parse`
///
/// [`NarInfo::parse`](sui_compat::narinfo::NarInfo::parse) requires `FileHash`
/// and `FileSize`, and rejects the whole document when either is absent. A
/// narinfo missing them still **advertises a NAR** — a client will still fetch
/// that `URL:` and still hard-fail on a 404 — so indexing through the full
/// parse would leave exactly those narinfos out of the index. That is not a
/// harmless gap: an unindexed narinfo sharing a narhash with an indexed one gets
/// stranded the moment the indexed one is deleted. The reverse index must see
/// every narinfo that names a NAR, whatever else is wrong with it, so it reads
/// the field that matters and judges nothing else.
#[must_use]
pub fn advertised_nar_url(narinfo: &str) -> Option<String> {
    let url = advertised_url_line(narinfo)?;
    is_addressable_nar_path(url).then(|| url.to_string())
}

/// The raw `URL:` field of a narinfo, **unjudged**.
///
/// Separate from [`advertised_nar_url`] so the write boundary can tell "there is
/// no URL here" (index nothing, store it) apart from "there is a URL and it is
/// not one we will address" (refuse the write). Collapsing the two would let a
/// traversal URL through as a silently-unindexed narinfo.
#[must_use]
pub fn advertised_url_line(narinfo: &str) -> Option<&str> {
    narinfo.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "URL").then(|| value.trim())
    })
}

/// Is this narinfo text usable by a Nix client at all?
///
/// `StorePath:` is what makes a narinfo a narinfo — nix's own reader rejects
/// text without one as `corrupt: StorePath missing` — so this is the minimum
/// bar for both accepting an upload and serving a stored entry.
///
/// ── WHY THIS IS A SHARED PREDICATE, not an `is_empty()` at one call site ────
/// Measured on camelot-eks 2026-08-05: two rows in the durable tier held a
/// ZERO-LENGTH value, and the read path served them as `200` with an empty
/// body. Nix aborted the whole operation on the first one it met while asking
/// the destination which paths it already had, so two poisoned rows out of 6898
/// broke EVERY `nix copy --to` against the cache.
///
/// An unusable hit is worse than a miss: a miss makes the client build, a
/// malformed hit makes it fail — and the error names nix, not the cache. Both
/// boundaries therefore ask the same question, because fixing only the write
/// leaves existing poison fatal, and fixing only the read leaves the tier
/// accumulating garbage.
#[must_use]
pub fn is_servable_narinfo(narinfo: &str) -> bool {
    narinfo
        .lines()
        .any(|line| line.split_once(':').is_some_and(|(k, v)| k.trim() == "StorePath" && !v.trim().is_empty()))
}

/// The reverse index of a single [`StorageBackend`](super::StorageBackend).
///
/// Three verbs, all idempotent. Every implementation persists edges in the
/// backend's own store, so the index survives exactly as long as the data it
/// describes.
///
/// # Which way to be wrong
///
/// Over-reporting a referrer keeps a NAR that could have been reclaimed — a
/// leak. Under-reporting deletes a NAR another narinfo still advertises — an
/// outage. **Every implementation rounds toward over-reporting**, and any place
/// that cannot (a hot tier's key expiring, a fan-out where one tier is down)
/// says so at that site.
#[async_trait]
pub trait NarRefIndex: Send + Sync {
    /// Record "`hash`'s narinfo advertises `nar_path`". Idempotent.
    ///
    /// # Errors
    ///
    /// Propagates the backend's write failure.
    async fn record(&self, nar_path: &str, hash: &str) -> Result<(), StoreError>;

    /// Forget that edge. Idempotent — forgetting an absent edge is `Ok(())`.
    ///
    /// # Errors
    ///
    /// Propagates the backend's delete failure.
    async fn forget(&self, nar_path: &str, hash: &str) -> Result<(), StoreError>;

    /// Every store-path hash whose narinfo advertises `nar_path`, sorted and
    /// deduplicated.
    ///
    /// # Errors
    ///
    /// Propagates the backend's read failure. An empty vector is a real answer
    /// ("nothing advertises this NAR"), never a stand-in for a failed lookup —
    /// which is why this returns `Result<Vec<_>>` and not `Vec<_>`.
    async fn referrers(&self, nar_path: &str) -> Result<Vec<String>, StoreError>;
}

/// In-memory [`NarRefIndex`] — the reference semantics, and what every test
/// double uses.
///
/// Exported rather than test-only on purpose: a `StorageBackend` double that has
/// to hand-roll an index will hand-roll it differently, and a double whose
/// reverse index disagrees with production's is a gate that proves nothing.
#[derive(Debug, Default)]
pub struct MemNarRefIndex {
    edges: Mutex<BTreeMap<String, BTreeSet<String>>>,
}

impl MemNarRefIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Total edge count, across every NAR. Diagnostics and gates.
    #[must_use]
    pub fn len(&self) -> usize {
        self.edges.lock().unwrap_or_else(std::sync::PoisonError::into_inner).values().map(BTreeSet::len).sum()
    }

    /// Whether the index holds no edges at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl NarRefIndex for MemNarRefIndex {
    async fn record(&self, nar_path: &str, hash: &str) -> Result<(), StoreError> {
        self.edges
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(nar_path.to_string())
            .or_default()
            .insert(hash.to_string());
        Ok(())
    }

    async fn forget(&self, nar_path: &str, hash: &str) -> Result<(), StoreError> {
        let mut edges = self.edges.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(set) = edges.get_mut(nar_path) {
            set.remove(hash);
            if set.is_empty() {
                edges.remove(nar_path);
            }
        }
        Ok(())
    }

    async fn referrers(&self, nar_path: &str) -> Result<Vec<String>, StoreError> {
        Ok(self
            .edges
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(nar_path)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edge_key_is_scanned_by_its_own_prefix() {
        let key = NarRefKey { nar_path: "nar/abc.nar.xz", hash: "sss" }.to_string();
        assert_eq!(key, "nar-refs/nar/abc.nar.xz/sss");
        let scan = NarRefScan { nar_path: "nar/abc.nar.xz" };
        assert!(key.starts_with(&scan.to_string()));
        assert_eq!(referrer_of(&scan, &key), Some("sss"));
    }

    #[test]
    fn a_scan_prefix_does_not_reach_a_longer_neighbour() {
        // Without the trailing `/`, `nar/ab.nar` would also match
        // `nar/ab.nar.xz`'s edges and over-report a referrer onto the wrong NAR.
        let neighbour = NarRefKey { nar_path: "nar/ab.nar.xz", hash: "sss" }.to_string();
        let scan = NarRefScan { nar_path: "nar/ab.nar" };
        assert!(!neighbour.starts_with(&scan.to_string()));
        assert_eq!(referrer_of(&scan, &neighbour), None);
    }

    #[test]
    fn referrer_of_rejects_a_key_from_another_nar() {
        let scan = NarRefScan { nar_path: "nar/a.nar" };
        assert_eq!(referrer_of(&scan, "nar-refs/nar/b.nar/sss"), None);
        assert_eq!(referrer_of(&scan, "nar-refs/nar/a.nar/"), None);
        assert_eq!(referrer_of(&scan, "nar-refs/nar/a.nar/deep/sss"), None);
    }

    #[test]
    fn traversal_and_absolute_urls_are_not_addressable() {
        assert!(is_addressable_nar_path("nar/abc.nar.xz"));
        assert!(is_addressable_nar_path("nar/deep/abc.nar"));
        assert!(!is_addressable_nar_path(""));
        assert!(!is_addressable_nar_path("/etc/passwd"));
        assert!(!is_addressable_nar_path("../../etc/passwd"));
        assert!(!is_addressable_nar_path("nar/../../etc/passwd"));
        assert!(!is_addressable_nar_path("nar/./abc.nar"));
        assert!(!is_addressable_nar_path("nar//abc.nar"));
        assert!(!is_addressable_nar_path("nar\\abc.nar"));
        assert!(!is_addressable_nar_path("nar/abc\n.nar"));
    }

    #[test]
    fn an_unaddressable_url_advertises_nothing() {
        let good = "StorePath: /nix/store/abc-hello\nURL: nar/abc.nar.xz\nCompression: xz\n\
                    FileHash: sha256:aaa\nFileSize: 100\nNarHash: sha256:bbb\nNarSize: 200\n\
                    References: \n";
        assert_eq!(advertised_nar_url(good).as_deref(), Some("nar/abc.nar.xz"));

        let traversal = "StorePath: /nix/store/abc-hello\nURL: ../../etc/passwd\n\
                         Compression: xz\nFileHash: sha256:aaa\nFileSize: 100\n\
                         NarHash: sha256:bbb\nNarSize: 200\nReferences: \n";
        assert_eq!(advertised_nar_url(traversal), None);

        assert_eq!(advertised_nar_url("not a narinfo at all"), None);
    }

    /// A narinfo that [`NarInfo::parse`](sui_compat::narinfo::NarInfo::parse)
    /// **rejects** still advertises a NAR, and must still be indexed.
    ///
    /// This one has no `FileHash`/`FileSize`, so the full parser returns
    /// `MissingField` — yet a client fetching it will still request
    /// `nar/abc.nar.xz` and still hard-fail if that 404s. Indexing through the
    /// full parse would leave it out of the index, and an unindexed narinfo
    /// sharing a narhash with an indexed one is the strand this whole module
    /// exists to prevent.
    #[test]
    fn a_narinfo_the_strict_parser_rejects_still_advertises_its_nar() {
        let partial = "StorePath: /nix/store/abc-hello\nURL: nar/abc.nar.xz\n\
                       Compression: xz\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n";
        assert!(
            sui_compat::narinfo::NarInfo::parse(partial).is_err(),
            "fixture must actually be one the strict parser rejects",
        );
        assert_eq!(advertised_nar_url(partial).as_deref(), Some("nar/abc.nar.xz"));
    }

    #[tokio::test]
    async fn the_in_memory_index_is_a_set_per_nar() {
        let ix = MemNarRefIndex::new();
        assert!(ix.is_empty());

        ix.record("nar/x.nar", "aaa").await.unwrap();
        ix.record("nar/x.nar", "bbb").await.unwrap();
        // Idempotent: the same edge twice is still one edge.
        ix.record("nar/x.nar", "aaa").await.unwrap();
        assert_eq!(ix.referrers("nar/x.nar").await.unwrap(), vec!["aaa", "bbb"]);
        assert_eq!(ix.len(), 2);

        ix.forget("nar/x.nar", "aaa").await.unwrap();
        assert_eq!(ix.referrers("nar/x.nar").await.unwrap(), vec!["bbb"]);

        // Forgetting an absent edge is not an error.
        ix.forget("nar/x.nar", "aaa").await.unwrap();
        ix.forget("nar/absent.nar", "zzz").await.unwrap();

        ix.forget("nar/x.nar", "bbb").await.unwrap();
        assert!(ix.referrers("nar/x.nar").await.unwrap().is_empty());
        assert!(ix.is_empty());
    }
}
