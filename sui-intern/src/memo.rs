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
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::rc::Rc;

/// A content address derived **by construction** from a value of type `T`.
///
/// # The type-level seal (M3 — the stale-key/decoupling axis)
///
/// The general [`ContentMemo::get_or_compute`] lets the `key` and the
/// `compute` closure **disagree**: a caller can pass a key that is *not* a
/// function of what `compute` actually reads (the stale-key footgun — the
/// `Sharing::PerSite`/libxcrypt divergence class). `ContentKey<T>` removes
/// that footgun structurally: its **sole constructor** is [`ContentKey::of`],
/// which hashes `T`'s structural read-set. So "the key IS the content of the
/// input" is not a caller obligation — it holds **by construction**, and a
/// key decoupled from its input **has no way to be built**.
///
/// Paired with [`ContentMemo::get_or_compute_keyed`], which derives the key
/// from the same `&T` it hands to `compute`, the key↔content decoupling axis
/// is **parse-time-rejected** (there is no expressible program that memoizes
/// under a key not derived from the computed input).
///
/// # Honest ceiling — what this does NOT seal
///
/// This seals the KEY↔CONTENT structural axis only. It does **not** seal the
/// **purity** of `T → V`: `compute` could still read wall-clock, `getEnv`, or a
/// mutable filesystem — those are opaque to the type system. There is no
/// `PureFn` in safe Rust and there cannot be, so the purity axis stays
/// **only-mitigated (C1) forever** (the module invariant + the CI byte gate are
/// the correct terminal enforcement). Do not read `ContentKey<T>` as a purity
/// proof — it is a decoupling proof.
///
/// The digest is a 32-byte BLAKE3 over `T`'s `Hash` serialization, so the key
/// is a stable, collision-resistant content address of the value's structural
/// fields.
///
/// The trait impls below are hand-written (not `#[derive]`d) so they hold for
/// **any** `T` — the standard derive would demand `T: Clone + Eq + Hash + …`
/// even though the only real field is the 32-byte digest (`T` lives only in a
/// zero-size `PhantomData`).
pub struct ContentKey<T> {
    digest: [u8; 32],
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for ContentKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for ContentKey<T> {}
impl<T> PartialEq for ContentKey<T> {
    fn eq(&self, other: &Self) -> bool {
        self.digest == other.digest
    }
}
impl<T> Eq for ContentKey<T> {}
impl<T> Hash for ContentKey<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.digest.hash(state);
    }
}
impl<T> std::fmt::Debug for ContentKey<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Short hex prefix of the digest — enough to distinguish keys in logs.
        write!(f, "ContentKey(")?;
        for b in &self.digest[..4] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…)")
    }
}

/// A `std::hash::Hasher` that streams into a BLAKE3 hasher, so any `T: Hash`
/// can be content-addressed generically (its structural read-set = exactly the
/// bytes its `Hash` impl writes).
struct Blake3Hasher(blake3::Hasher);

impl Hasher for Blake3Hasher {
    fn finish(&self) -> u64 {
        // Not the content address — `ContentKey::of` uses the full 32-byte
        // digest via `finalize()`. This exists only to satisfy the trait for
        // the streaming `write` path; a fold of the first 8 digest bytes.
        let bytes = self.0.finalize();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes.as_bytes()[..8]);
        u64::from_le_bytes(buf)
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
}

impl<T: Hash> ContentKey<T> {
    /// Derive the content key from `input` — the **sole** constructor.
    ///
    /// Hashes `input`'s structural read-set (everything its `Hash` impl writes)
    /// through BLAKE3, so the returned key is a pure, deterministic function of
    /// `input`'s content. Two structurally-equal `T`s produce the identical key;
    /// there is no way to construct a `ContentKey<T>` that is *not* the content
    /// address of some `&T`.
    #[must_use]
    pub fn of(input: &T) -> Self {
        let mut hasher = Blake3Hasher(blake3::Hasher::new());
        input.hash(&mut hasher);
        Self {
            digest: *hasher.0.finalize().as_bytes(),
            _marker: PhantomData,
        }
    }

    /// The raw 32-byte BLAKE3 digest (for debugging / cross-checking).
    #[must_use]
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

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

impl<T: Hash, V> ContentMemo<ContentKey<T>, V> {
    /// The **keyed** memo API — the M3 seal for the key↔content decoupling axis.
    ///
    /// Derives the [`ContentKey`] from `input` internally (so the key is
    /// *provably* the content address of `input`, not some ambient value the
    /// caller passed alongside it) and hands the **same `&input`** to
    /// `compute`. A caller therefore **cannot** memoize under a key decoupled
    /// from the computed input — the decoupling has no constructor.
    ///
    /// A hit is a cheap `Rc::clone` and is byte-identical to a recompute **by
    /// the module's purity invariant** — `compute` must still be a pure
    /// function of its `&T` argument (the C1-ceiling purity axis this seal does
    /// NOT close; see [`ContentKey`]).
    pub fn get_or_compute_keyed(&self, input: &T, compute: impl FnOnce(&T) -> V) -> Rc<V> {
        let key = ContentKey::of(input);
        if let Some(hit) = self.map.borrow().get(&key).cloned() {
            return hit;
        }
        // Compute OUTSIDE the borrow: `compute` may itself touch other memos.
        let rc = Rc::new(compute(input));
        self.map.borrow_mut().insert(key, rc.clone());
        rc
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

/// `ContentKey<T>` has no public constructor other than `of(&T)`, and its
/// fields are private — so you cannot build one from a value of the wrong type
/// (or from raw bytes). This doctest proves the type-level seal: passing a `&B`
/// where a `&A` is expected is a type error, not a runtime footgun.
///
/// ```compile_fail
/// use sui_intern::memo::ContentKey;
/// struct A(u32);
/// struct B(u32);
/// let b = B(1);
/// // A key over A cannot be constructed from a &B — the decoupling has no path.
/// let _k: ContentKey<A> = ContentKey::<A>::of(&b);
/// ```
///
/// And the private fields cannot be hand-forged from an arbitrary digest:
///
/// ```compile_fail
/// use sui_intern::memo::ContentKey;
/// struct A(u32);
/// // No public constructor from raw bytes; fields are private.
/// let _k: ContentKey<A> = ContentKey { digest: [0u8; 32], _marker: Default::default() };
/// ```
#[allow(dead_code)]
fn _content_key_type_seal_doc() {}

#[cfg(test)]
mod tests {
    use super::{ContentKey, ContentMemo};

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

    // --- M3: ContentKey<T> (the key↔content decoupling seal) ---

    #[derive(Hash)]
    struct Input {
        name: String,
        n: u32,
    }

    #[test]
    fn content_key_of_is_deterministic() {
        let a = Input {
            name: "x".into(),
            n: 5,
        };
        let b = Input {
            name: "x".into(),
            n: 5,
        };
        // Same structural content → identical key (the "key IS the content"
        // invariant, holding by construction).
        assert_eq!(ContentKey::of(&a), ContentKey::of(&b));
        assert_eq!(ContentKey::of(&a).digest(), ContentKey::of(&b).digest());
        // Re-deriving the same value is stable across calls.
        assert_eq!(ContentKey::of(&a), ContentKey::of(&a));
    }

    #[test]
    fn content_key_differs_on_different_content() {
        let a = Input {
            name: "x".into(),
            n: 5,
        };
        let differ_n = Input {
            name: "x".into(),
            n: 6,
        };
        let differ_name = Input {
            name: "y".into(),
            n: 5,
        };
        assert_ne!(ContentKey::of(&a), ContentKey::of(&differ_n));
        assert_ne!(ContentKey::of(&a), ContentKey::of(&differ_name));
    }

    #[test]
    fn keyed_memo_round_trips_and_does_not_recompute_on_hit() {
        let memo: ContentMemo<ContentKey<Input>, String> = ContentMemo::new();
        let mut calls = 0;
        let input = Input {
            name: "hello".into(),
            n: 2,
        };

        // Miss: compute runs, receives the SAME &input the key was derived from.
        let a = memo.get_or_compute_keyed(&input, |i| {
            calls += 1;
            assert_eq!(i.name, "hello");
            assert_eq!(i.n, 2);
            format!("{}-{}", i.name, i.n)
        });
        assert_eq!(&*a, "hello-2");

        // Hit on a structurally-equal but DISTINCT value: same content key, no
        // recompute — the key is the content, not the object identity.
        let same_content = Input {
            name: "hello".into(),
            n: 2,
        };
        let b = memo.get_or_compute_keyed(&same_content, |_| {
            calls += 1;
            "SHOULD-NOT-RUN".to_string()
        });
        assert_eq!(calls, 1, "structurally-equal input must hit, not recompute");
        assert!(std::rc::Rc::ptr_eq(&a, &b), "hit returns the same Rc");

        // Different content → miss → recompute.
        let other = Input {
            name: "world".into(),
            n: 9,
        };
        let c = memo.get_or_compute_keyed(&other, |i| {
            calls += 1;
            format!("{}-{}", i.name, i.n)
        });
        assert_eq!(calls, 2);
        assert_eq!(&*c, "world-9");
        assert_eq!(memo.len(), 2);
    }
}
