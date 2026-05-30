//! In-RAM bounded LRU cache layered on top of `sui-graph-store`.
//!
//! The daemon mmaps blobs from the [`GraphStore`] on every request; the
//! `mmap` itself is cheap (page fault), but rkyv validation + handling
//! the memory map adds 100s of microseconds per call. For the common
//! "hot blob queried repeatedly" pattern (one nix-flake-show resolving
//! the same lockfile 50 times during traversal), holding the
//! already-validated bytes in process memory is sub-microsecond.
//!
//! Eviction is plain LRU by hit recency. The cap is configurable; a
//! reasonable default is 1024 entries — enough for a developer's
//! working set, small enough not to dominate the daemon's RSS.
//!
//! [`GraphStore`]: sui_graph_store::GraphStore

use std::num::NonZeroUsize;
use std::sync::Mutex;

use bytes::Bytes;
use lru::LruCache;
use sui_graph_store::{GraphHash, GraphKind};

/// Cache key — pair `(kind, hash)`. Two different graph kinds with
/// the same hash never share the same cached entry.
pub type CacheKey = (GraphKind, GraphHash);

/// Default capacity. Sized for a developer's working set: ~1024 hot
/// graphs comfortably fits the lockfile + module-graph + AST-graph
/// triad for the rio nixos config plus a couple dozen flake.show
/// targets.
pub const DEFAULT_CAPACITY: usize = 1024;

/// Bounded LRU hot cache.
pub struct LruHotCache {
    inner: Mutex<LruCache<CacheKey, Bytes>>,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

impl LruHotCache {
    /// Build a cache with the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(NonZeroUsize::new(DEFAULT_CAPACITY).expect("non-zero default"))
    }

    /// Build a cache with a caller-chosen capacity.
    #[must_use]
    pub fn with_capacity(cap: NonZeroUsize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Look up `(kind, hash)`. Returns a cheap [`Bytes`] handle (one
    /// arc bump, no copy) on hit. Updates the LRU recency.
    pub fn get(&self, kind: GraphKind, hash: GraphHash) -> Option<Bytes> {
        let mut g = self.inner.lock().expect("cache poisoned");
        if let Some(b) = g.get(&(kind, hash)) {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(b.clone())
        } else {
            self.misses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            None
        }
    }

    /// Insert `bytes` under `(kind, hash)`. If the cap is reached the
    /// LRU victim is dropped.
    pub fn put(&self, kind: GraphKind, hash: GraphHash, bytes: Bytes) {
        let mut g = self.inner.lock().expect("cache poisoned");
        g.put((kind, hash), bytes);
    }

    /// Current entry count.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("cache poisoned").len()
    }

    /// True iff the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sum of bytes held across every entry. Walks the LRU — O(n).
    /// Called only when assembling a [`stats`] snapshot, so the cost is
    /// amortized away.
    pub fn total_bytes(&self) -> u64 {
        let g = self.inner.lock().expect("cache poisoned");
        g.iter().map(|(_, b)| b.len() as u64).sum()
    }

    /// Hit counter snapshot.
    pub fn hits(&self) -> u64 {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Miss counter snapshot.
    pub fn misses(&self) -> u64 {
        self.misses.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for LruHotCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn h(seed: &[u8]) -> GraphHash {
        GraphHash::of(seed)
    }

    #[test]
    fn put_then_get_round_trips() {
        let c = LruHotCache::new();
        c.put(GraphKind::Lockfile, h(b"a"), Bytes::from_static(b"value-a"));
        assert_eq!(
            c.get(GraphKind::Lockfile, h(b"a")).unwrap(),
            Bytes::from_static(b"value-a")
        );
    }

    #[test]
    fn lru_evicts_oldest_when_full() {
        let c = LruHotCache::with_capacity(NonZeroUsize::new(2).unwrap());
        c.put(GraphKind::Lockfile, h(b"a"), Bytes::from_static(b"A"));
        c.put(GraphKind::Lockfile, h(b"b"), Bytes::from_static(b"B"));
        // Touch a — now b is the LRU victim
        let _ = c.get(GraphKind::Lockfile, h(b"a"));
        c.put(GraphKind::Lockfile, h(b"c"), Bytes::from_static(b"C"));
        assert!(c.get(GraphKind::Lockfile, h(b"a")).is_some());
        assert!(c.get(GraphKind::Lockfile, h(b"b")).is_none());
        assert!(c.get(GraphKind::Lockfile, h(b"c")).is_some());
    }

    #[test]
    fn hit_miss_counters_increment() {
        let c = LruHotCache::new();
        c.put(GraphKind::Lockfile, h(b"x"), Bytes::from_static(b"X"));
        let _ = c.get(GraphKind::Lockfile, h(b"x"));
        let _ = c.get(GraphKind::Lockfile, h(b"x"));
        let _ = c.get(GraphKind::Lockfile, h(b"y"));
        assert_eq!(c.hits(), 2);
        assert_eq!(c.misses(), 1);
    }

    #[test]
    fn different_kinds_with_same_hash_are_distinct() {
        let c = LruHotCache::new();
        c.put(GraphKind::Lockfile, h(b"x"), Bytes::from_static(b"L"));
        c.put(GraphKind::Ast, h(b"x"), Bytes::from_static(b"A"));
        assert_eq!(
            c.get(GraphKind::Lockfile, h(b"x")).unwrap(),
            Bytes::from_static(b"L")
        );
        assert_eq!(
            c.get(GraphKind::Ast, h(b"x")).unwrap(),
            Bytes::from_static(b"A")
        );
    }

    #[test]
    fn total_bytes_sums_entry_lengths() {
        let c = LruHotCache::new();
        c.put(GraphKind::Lockfile, h(b"a"), Bytes::from_static(b"hello"));
        c.put(GraphKind::Lockfile, h(b"b"), Bytes::from_static(b"world!"));
        assert_eq!(c.total_bytes(), 11);
    }
}
