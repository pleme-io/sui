//! `ContentMemo` — a byte-neutral, content-keyed memo of a PURE function.
//!
//! # The load-bearing invariant (why this is safe on a byte-parity path)
//!
//! The memoized value is a **pure function of its content-key**, so a cache
//! hit is **byte-identical** to a recompute. That is exactly what makes
//! memoization safe on sui's byte-parity-critical eval path: a memoized nix
//! evaluation must produce the identical `drvPath` whether the value was
//! freshly computed or served from the memo. The key IS the content address;
//! the value is fully determined by it.
//!
//! **RISK — do not violate:** never memoize a value that depends on anything
//! other than its key — wall-clock (`currentTime`), environment (`getEnv`),
//! mutable filesystem reads, or eval-order-sensitive identity. Those are not
//! pure functions of the key, so a hit would NOT equal a recompute and could
//! change a `drvPath`. If the value can vary for a fixed key, it is not a
//! `ContentMemo` candidate.
//!
//! # Why it's a primitive
//!
//! This exact shape — a thread-local `Map<ContentKey, Rc<Value>>` of a pure
//! function, cleared at a natural boundary — was hand-rolled three times
//! before this module (the sui-compat NAR-hash memo, the sui-eval
//! referenced-idents memo, the sui-eval overlay-flatten cache). This is the
//! extracted, packaged shape: declare one with [`thread_local_content_memo!`]
//! and get a memoized accessor + a clear fn for free.
//!
//! Single-threaded by construction (`Rc` + `RefCell`), matching sui's
//! single-threaded evaluator.

use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::hash::Hash;
use std::rc::Rc;

/// A single-threaded content-keyed memo. `K` is the content address; the
/// value `Rc<V>` is a **pure function of `K`** (see the module invariant).
/// A hit is a cheap `Rc::clone`.
#[derive(Debug)]
pub struct ContentMemo<K: Eq + Hash, V> {
    map: RefCell<FxHashMap<K, Rc<V>>>,
}

impl<K: Eq + Hash, V> Default for ContentMemo<K, V> {
    fn default() -> Self {
        Self {
            map: RefCell::new(FxHashMap::default()),
        }
    }
}

impl<K: Eq + Hash + Clone, V> ContentMemo<K, V> {
    /// A fresh empty memo.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the memoized value for `key`, computing + storing it via
    /// `compute` on a miss. A hit is a cheap `Rc::clone` and is byte-identical
    /// to a recompute **by the module's purity invariant** (the caller's
    /// responsibility — `compute` must be a pure function of `key`).
    pub fn get_or_compute(&self, key: K, compute: impl FnOnce() -> V) -> Rc<V> {
        if let Some(hit) = self.map.borrow().get(&key).cloned() {
            return hit;
        }
        // Compute OUTSIDE the borrow: `compute` may itself touch other memos.
        let rc = Rc::new(compute());
        self.map.borrow_mut().insert(key, rc.clone());
        rc
    }

    /// Drop all entries. Call at a natural boundary (e.g. per top-level eval)
    /// so stale keys from a prior computation cannot persist and collide.
    pub fn clear(&self) {
        self.map.borrow_mut().clear();
    }

    /// Number of memoized entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.borrow().len()
    }

    /// Whether the memo is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.borrow().is_empty()
    }
}

/// Declare a thread-local [`ContentMemo`] plus a memoized accessor and a clear
/// fn — the boilerplate that was hand-rolled at every memo site. Real-easy
/// front: one declaration replaces the `thread_local!` + `get_or_compute`
/// wrapper + `clear` wrapper trio.
///
/// ```ignore
/// use sui_intern::thread_local_content_memo;
/// use std::collections::HashSet;
///
/// thread_local_content_memo! {
///     /// referenced idents per (source-id, text-range)
///     REFERENCED_IDENTS: (u32, rnix::TextRange) => HashSet<smol_str::SmolStr>;
///     fn referenced_idents_memo;   // accessor: (key, || compute) -> Rc<V>
///     fn clear_referenced_idents;  // clear the memo (per top-level eval)
/// }
///
/// // hit-or-compute (byte-neutral: value is a pure fn of the key):
/// let set = referenced_idents_memo(key, || walk_and_collect(expr));
/// // reset at the eval boundary:
/// clear_referenced_idents();
/// ```
#[macro_export]
macro_rules! thread_local_content_memo {
    (
        $(#[$meta:meta])*
        $STATIC:ident : $K:ty => $V:ty ;
        fn $get:ident ;
        fn $clear:ident ;
    ) => {
        thread_local! {
            $(#[$meta])*
            static $STATIC: $crate::memo::ContentMemo<$K, $V> =
                $crate::memo::ContentMemo::new();
        }

        /// Memoized accessor — returns the cached `Rc` on a hit, else computes,
        /// stores, and returns. `compute` MUST be a pure function of `key`
        /// (the memo's byte-neutrality invariant).
        #[allow(dead_code)]
        fn $get(key: $K, compute: impl FnOnce() -> $V) -> ::std::rc::Rc<$V> {
            $STATIC.with(|m| m.get_or_compute(key, compute))
        }

        /// Clear the thread-local memo (call at a natural boundary).
        #[allow(dead_code)]
        fn $clear() {
            $STATIC.with(|m| m.clear());
        }
    };
}

#[cfg(test)]
mod tests {
    use super::ContentMemo;

    #[test]
    fn hit_returns_same_rc_and_does_not_recompute() {
        let memo: ContentMemo<u32, String> = ContentMemo::new();
        let mut calls = 0;
        let a = memo.get_or_compute(7, || {
            calls += 1;
            "seven".to_string()
        });
        let b = memo.get_or_compute(7, || {
            calls += 1;
            "SHOULD-NOT-RUN".to_string()
        });
        assert_eq!(calls, 1, "second get for the same key must not recompute");
        assert!(std::rc::Rc::ptr_eq(&a, &b), "same key returns the same Rc");
        assert_eq!(&*a, "seven");
    }

    #[test]
    fn distinct_keys_are_independent() {
        let memo: ContentMemo<u32, u32> = ContentMemo::new();
        let a = memo.get_or_compute(1, || 10);
        let b = memo.get_or_compute(2, || 20);
        assert_eq!((*a, *b), (10, 20));
        assert_eq!(memo.len(), 2);
        assert!(!std::rc::Rc::ptr_eq(&a, &b));
    }

    #[test]
    fn clear_drops_entries_so_recompute_happens() {
        let memo: ContentMemo<u32, u32> = ContentMemo::new();
        let mut calls = 0;
        let _ = memo.get_or_compute(1, || {
            calls += 1;
            1
        });
        assert_eq!(memo.len(), 1);
        memo.clear();
        assert!(memo.is_empty());
        let _ = memo.get_or_compute(1, || {
            calls += 1;
            1
        });
        assert_eq!(calls, 2, "after clear, the key recomputes");
    }

    // The macro declares fn items at module scope.
    crate::thread_local_content_memo! {
        /// test memo
        TEST_MEMO: u32 => String;
        fn tl_get;
        fn tl_clear;
    }

    #[test]
    fn macro_accessor_memoizes_and_clears() {
        tl_clear();
        let a = tl_get(3, || "three".to_string());
        let b = tl_get(3, || "nope".to_string());
        assert!(std::rc::Rc::ptr_eq(&a, &b));
        assert_eq!(&*a, "three");
        tl_clear();
        let c = tl_get(3, || "again".to_string());
        assert_eq!(&*c, "again", "after clear the key recomputes");
    }
}
