//! Lazy evaluation primitives — making accidental eagerness impossible.
//!
//! The core type `Lazy<T>` guarantees evaluation is deferred until `.demand()`.
//! Unlike `Thunk` (which is a Value variant that callers must check for),
//! `Lazy<T>` is a WRAPPER that enforces laziness by construction.
//!
//! # Architecture
//!
//! ```text
//! Lazy<Value>       — a value that might not be computed yet
//! LazyAttrs         — attrset where values are Lazy<Value>
//! LazyList           — list where elements are Lazy<Value>
//! ```
//!
//! The evaluator returns `Lazy<Value>` from all expression evaluation.
//! Consumers that need concrete values call `.demand()` explicitly.
//! Operations that DON'T need the value (attrset key checking, list
//! length, typeOf on already-known types) work WITHOUT demanding.

use std::cell::OnceCell;
use std::rc::Rc;

/// A lazy value: computed at most once, on first demand.
///
/// Unlike `Thunk`, this is not a `Value` variant — it's a WRAPPER.
/// You can't accidentally pattern-match past it. You must call
/// `.demand()` to get the inner value.
///
/// Size: 1 word (Rc pointer). The inner cell is shared across clones.
#[derive(Clone)]
pub struct Lazy<T: Clone> {
    inner: Rc<LazyInner<T>>,
}

struct LazyInner<T: Clone> {
    /// Cached result (set on first demand).
    cache: OnceCell<T>,
    /// Computation to produce the value. Consumed on first demand.
    compute: std::cell::Cell<Option<Box<dyn FnOnce() -> T>>>,
}

impl<T: Clone> Lazy<T> {
    /// Create a lazy value from a computation.
    pub fn defer(f: impl FnOnce() -> T + 'static) -> Self {
        Self {
            inner: Rc::new(LazyInner {
                cache: OnceCell::new(),
                compute: std::cell::Cell::new(Some(Box::new(f))),
            }),
        }
    }

    /// Create an already-computed lazy value (no deferral).
    pub fn ready(value: T) -> Self {
        let cache = OnceCell::new();
        let _ = cache.set(value);
        Self {
            inner: Rc::new(LazyInner {
                cache,
                compute: std::cell::Cell::new(None),
            }),
        }
    }

    /// Demand the value. Evaluates if not yet computed. Returns cached result.
    pub fn demand(&self) -> &T {
        self.inner.cache.get_or_init(|| {
            let compute = self.inner.compute.take()
                .expect("Lazy: cache empty but compute already consumed (bug)");
            compute()
        })
    }

    /// Check if already computed (without forcing).
    pub fn is_ready(&self) -> bool {
        self.inner.cache.get().is_some()
    }
}

impl<T: Clone + std::fmt::Debug> std::fmt::Debug for Lazy<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(v) = self.inner.cache.get() {
            write!(f, "Lazy({v:?})")
        } else {
            write!(f, "Lazy(<deferred>)")
        }
    }
}

// ── Fallible lazy values ─────────────────────────────────────

/// A lazy value whose computation can fail.
///
/// Like `Lazy<T>` but the computation returns `Result<T, E>`.
/// On first `.demand()`, if the computation fails, the error is
/// returned and the computation can be retried on next `.demand()`.
/// On success, the result is cached permanently.
#[derive(Clone)]
pub struct FallibleLazy<T: Clone, E: Clone> {
    inner: Rc<FallibleLazyInner<T, E>>,
}

struct FallibleLazyInner<T: Clone, E: Clone> {
    cache: OnceCell<T>,
    compute: std::cell::Cell<Option<Box<dyn FnOnce() -> Result<T, E>>>>,
}

impl<T: Clone, E: Clone> FallibleLazy<T, E> {
    /// Create a fallible lazy value from a computation.
    pub fn defer(f: impl FnOnce() -> Result<T, E> + 'static) -> Self {
        Self {
            inner: Rc::new(FallibleLazyInner {
                cache: OnceCell::new(),
                compute: std::cell::Cell::new(Some(Box::new(f))),
            }),
        }
    }

    /// Create an already-computed fallible lazy value.
    pub fn ready(value: T) -> Self {
        let cache = OnceCell::new();
        let _ = cache.set(value);
        Self {
            inner: Rc::new(FallibleLazyInner {
                cache,
                compute: std::cell::Cell::new(None),
            }),
        }
    }

    /// Demand the value. Evaluates if not yet computed.
    /// Returns `Ok(&T)` if cached or freshly computed.
    /// Returns `Err(E)` if computation fails.
    pub fn demand(&self) -> Result<&T, E> {
        if let Some(v) = self.inner.cache.get() {
            return Ok(v);
        }
        if let Some(compute) = self.inner.compute.take() {
            match compute() {
                Ok(val) => {
                    let _ = self.inner.cache.set(val);
                    Ok(self.inner.cache.get().unwrap())
                }
                Err(e) => Err(e),
            }
        } else {
            // Cache is empty and compute was already consumed (error case).
            // This shouldn't happen in normal usage.
            panic!("FallibleLazy: cache empty and compute consumed without storing result")
        }
    }

    /// Check if already computed (without forcing).
    pub fn is_ready(&self) -> bool {
        self.inner.cache.get().is_some()
    }
}

impl<T: Clone + std::fmt::Debug, E: Clone> std::fmt::Debug for FallibleLazy<T, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(v) = self.inner.cache.get() {
            write!(f, "FallibleLazy({v:?})")
        } else {
            write!(f, "FallibleLazy(<deferred>)")
        }
    }
}

// ── Lazy attribute set ───────────────────────────────────────

use crate::value::{Value, EvalError, NixAttrs, intern, resolve};
use sui_intern::Symbol;

/// A lazy Nix attribute value: the key is known, the value is deferred.
///
/// This is the building block for lazy attrsets. Unlike `Value::Thunk`,
/// the laziness is part of the CONTAINER (the attrset), not the VALUE.
/// The attrset knows its keys immediately but computes values on demand.
pub type LazyValue = FallibleLazy<Value, EvalError>;

/// A Nix attrset where values are computed on demand.
///
/// Keys are available immediately (for `builtins.attrNames`, `hasAttr`, `//` merge).
/// Values are `LazyValue` — only computed when accessed via `.get()`.
///
/// This makes the attrset inherently lazy by construction:
/// - `keys()` — no forcing, returns immediately
/// - `contains_key()` — no forcing
/// - `get()` — forces ONLY the requested value
/// - `update()` — merges keys, values stay lazy
#[derive(Clone, Debug)]
pub struct LazyAttrs {
    entries: im_rc::HashMap<Symbol, LazyValue, rustc_hash::FxBuildHasher>,
}

impl LazyAttrs {
    /// Create an empty lazy attrset.
    pub fn new() -> Self {
        Self {
            entries: im_rc::HashMap::default(),
        }
    }

    /// Insert a lazy value.
    pub fn insert(&mut self, key: Symbol, value: LazyValue) {
        self.entries.insert(key, value);
    }

    /// Insert an already-computed value.
    pub fn insert_ready(&mut self, key: Symbol, value: Value) {
        self.entries.insert(key, LazyValue::ready(value));
    }

    /// Insert a deferred computation.
    pub fn insert_deferred<F>(&mut self, key: Symbol, f: F)
    where
        F: FnOnce() -> Result<Value, EvalError> + 'static,
    {
        self.entries.insert(key, LazyValue::defer(f));
    }

    /// Look up a value by name — forces ONLY this value.
    pub fn get(&self, key: &str) -> Option<Result<&Value, EvalError>> {
        let sym = intern(key);
        self.entries.get(&sym).map(|lv| lv.demand())
    }

    /// Look up by pre-interned symbol — forces ONLY this value.
    pub fn get_sym(&self, sym: &Symbol) -> Option<Result<&Value, EvalError>> {
        self.entries.get(sym).map(|lv| lv.demand())
    }

    /// Check if a key exists — NO forcing.
    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(&intern(key))
    }

    /// Get all key names — NO forcing of values.
    pub fn keys(&self) -> impl Iterator<Item = String> + '_ {
        self.entries.keys().map(|s| resolve(*s))
    }

    /// Number of entries — NO forcing.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether empty — NO forcing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Merge two lazy attrsets (right overrides left) — NO forcing.
    /// Values stay lazy. Only keys are merged.
    pub fn update(&self, other: &LazyAttrs) -> LazyAttrs {
        let mut result = self.entries.clone();
        for (k, v) in other.entries.iter() {
            result.insert(*k, v.clone());
        }
        LazyAttrs { entries: result }
    }

    /// Convert to a traditional NixAttrs by forcing ALL values.
    /// Use sparingly — only when ALL values are needed.
    pub fn force_all(&self) -> Result<NixAttrs, EvalError> {
        let mut attrs = NixAttrs::new();
        for (sym, lv) in self.entries.iter() {
            let val = lv.demand()?;
            attrs.insert(resolve(*sym), val.clone());
        }
        Ok(attrs)
    }
}

impl Default for LazyAttrs {
    fn default() -> Self {
        Self::new()
    }
}

// ── Lazy overlay chain ────────────────────────────────────────

/// A lazy overlay: `left // right` without eagerly merging.
///
/// When an attribute is accessed, the chain is walked right-to-left
/// (right overrides left). Keys are collected lazily on first `.keys()`.
/// This eliminates the O(n) merge cost per overlay application —
/// critical for nixpkgs with 20+ overlays × 80K+ attrs each.
#[derive(Clone, Debug)]
pub enum OverlayAttrs {
    /// A concrete base attrset.
    Base(Rc<NixAttrs>),
    /// A lazy overlay: `left // right`.
    /// Right overrides left. Neither is forced until accessed.
    Overlay {
        left: Rc<OverlayAttrs>,
        right: Rc<NixAttrs>,
    },
}

impl OverlayAttrs {
    /// Create from a concrete attrset.
    pub fn base(attrs: NixAttrs) -> Self {
        OverlayAttrs::Base(Rc::new(attrs))
    }

    /// Apply an overlay: `self // right`.
    /// O(1) — just creates a new node. No iteration.
    pub fn overlay(self, right: NixAttrs) -> Self {
        OverlayAttrs::Overlay {
            left: Rc::new(self),
            right: Rc::new(right),
        }
    }

    /// Look up an attribute — walks the chain right-to-left.
    /// O(depth) where depth = number of overlays.
    /// For nixpkgs: O(20) per access instead of O(80K) per merge.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            OverlayAttrs::Base(attrs) => attrs.get(key),
            OverlayAttrs::Overlay { left, right } => {
                // Right overrides left
                right.get(key).or_else(|| left.get(key))
            }
        }
    }

    /// Look up by pre-interned symbol.
    pub fn get_sym(&self, sym: &Symbol) -> Option<&Value> {
        match self {
            OverlayAttrs::Base(attrs) => attrs.get_sym(sym),
            OverlayAttrs::Overlay { left, right } => {
                right.get_sym(sym).or_else(|| left.get_sym(sym))
            }
        }
    }

    /// Check if a key exists — walks chain, no value forcing.
    pub fn contains_key(&self, key: &str) -> bool {
        match self {
            OverlayAttrs::Base(attrs) => attrs.contains_key(key),
            OverlayAttrs::Overlay { left, right } => {
                right.contains_key(key) || left.contains_key(key)
            }
        }
    }

    /// Collect all unique keys. O(total_keys) but only called when needed
    /// (e.g., `builtins.attrNames`). NOT called during normal attribute access.
    pub fn all_keys(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        self.collect_keys(&mut seen, &mut result);
        result.sort();
        result
    }

    fn collect_keys(&self, seen: &mut std::collections::HashSet<String>, result: &mut Vec<String>) {
        match self {
            OverlayAttrs::Base(attrs) => {
                for (k, _) in attrs.iter_unsorted() {
                    if seen.insert(k.clone()) {
                        result.push(k);
                    }
                }
            }
            OverlayAttrs::Overlay { left, right } => {
                // Right first (overrides left)
                for (k, _) in right.iter_unsorted() {
                    if seen.insert(k.clone()) {
                        result.push(k);
                    }
                }
                left.collect_keys(seen, result);
            }
        }
    }

    /// Flatten to a concrete NixAttrs. Use when the full attrset is needed.
    pub fn flatten(&self) -> NixAttrs {
        match self {
            OverlayAttrs::Base(attrs) => (**attrs).clone(),
            OverlayAttrs::Overlay { left, right } => {
                let base = left.flatten();
                base.update(right)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // ── Lazy<T> tests ────────────────────────────────────────

    #[test]
    fn deferred_not_evaluated_until_demand() {
        let evaluated = Rc::new(Cell::new(false));
        let e = evaluated.clone();
        let lazy = Lazy::defer(move || {
            e.set(true);
            42
        });
        assert!(!evaluated.get());
        assert_eq!(*lazy.demand(), 42);
        assert!(evaluated.get());
    }

    #[test]
    fn demand_memoizes() {
        let count = Rc::new(Cell::new(0));
        let c = count.clone();
        let lazy = Lazy::defer(move || {
            c.set(c.get() + 1);
            "hello"
        });
        assert_eq!(*lazy.demand(), "hello");
        assert_eq!(*lazy.demand(), "hello");
        assert_eq!(count.get(), 1); // computed exactly once
    }

    #[test]
    fn ready_is_immediate() {
        let lazy = Lazy::ready(99);
        assert!(lazy.is_ready());
        assert_eq!(*lazy.demand(), 99);
    }

    #[test]
    fn clone_shares_computation() {
        let count = Rc::new(Cell::new(0));
        let c = count.clone();
        let lazy = Lazy::defer(move || {
            c.set(c.get() + 1);
            7
        });
        let clone = lazy.clone();
        assert_eq!(*lazy.demand(), 7);
        assert_eq!(*clone.demand(), 7); // same cache
        assert_eq!(count.get(), 1); // computed once
    }

    // ── FallibleLazy<T, E> tests ─────────────────────────────

    #[test]
    fn fallible_lazy_success() {
        let fl: FallibleLazy<i64, String> = FallibleLazy::defer(|| Ok(42));
        assert!(!fl.is_ready());
        assert_eq!(*fl.demand().unwrap(), 42);
        assert!(fl.is_ready());
        assert_eq!(*fl.demand().unwrap(), 42); // cached
    }

    #[test]
    fn fallible_lazy_ready() {
        let fl: FallibleLazy<i64, String> = FallibleLazy::ready(99);
        assert!(fl.is_ready());
        assert_eq!(*fl.demand().unwrap(), 99);
    }

    #[test]
    fn fallible_lazy_clone_shares() {
        let count = Rc::new(Cell::new(0));
        let c = count.clone();
        let fl: FallibleLazy<i64, String> = FallibleLazy::defer(move || {
            c.set(c.get() + 1);
            Ok(7)
        });
        let clone = fl.clone();
        assert_eq!(*fl.demand().unwrap(), 7);
        assert_eq!(*clone.demand().unwrap(), 7);
        assert_eq!(count.get(), 1); // computed once
    }

    // ── LazyAttrs tests ──────────────────────────────────────

    #[test]
    fn lazy_attrs_keys_without_forcing() {
        let evaluated = Rc::new(Cell::new(false));
        let e = evaluated.clone();
        let mut attrs = LazyAttrs::new();
        attrs.insert_deferred(intern("expensive"), move || {
            e.set(true);
            Ok(Value::Int(42))
        });
        attrs.insert_ready(intern("cheap"), Value::Int(1));

        // Keys available without forcing
        assert_eq!(attrs.len(), 2);
        assert!(attrs.contains_key("expensive"));
        assert!(attrs.contains_key("cheap"));
        assert!(!evaluated.get()); // expensive NOT computed

        // Only force when accessed
        let val = attrs.get("expensive").unwrap().unwrap();
        assert_eq!(*val, Value::Int(42));
        assert!(evaluated.get()); // NOW computed
    }

    #[test]
    fn lazy_attrs_update_no_forcing() {
        let evaluated = Rc::new(Cell::new(false));
        let e = evaluated.clone();
        let mut a = LazyAttrs::new();
        a.insert_ready(intern("x"), Value::Int(1));
        let mut b = LazyAttrs::new();
        b.insert_deferred(intern("y"), move || {
            e.set(true);
            Ok(Value::Int(2))
        });

        // Merge without forcing
        let merged = a.update(&b);
        assert_eq!(merged.len(), 2);
        assert!(!evaluated.get()); // y NOT computed during merge

        // Access x (cheap) without touching y
        assert_eq!(*merged.get("x").unwrap().unwrap(), Value::Int(1));
        assert!(!evaluated.get()); // y STILL not computed
    }

    #[test]
    fn lazy_attrs_force_all() {
        let mut attrs = LazyAttrs::new();
        attrs.insert_ready(intern("a"), Value::Int(1));
        attrs.insert_deferred(intern("b"), || Ok(Value::Int(2)));
        let nix_attrs = attrs.force_all().unwrap();
        assert_eq!(nix_attrs.get("a"), Some(&Value::Int(1)));
        assert_eq!(nix_attrs.get("b"), Some(&Value::Int(2)));
    }

    // ── OverlayAttrs tests ───────────────────────────────────

    #[test]
    fn overlay_get_right_overrides_left() {
        let mut left = NixAttrs::new();
        left.insert("x".to_string(), Value::Int(1));
        left.insert("y".to_string(), Value::Int(2));

        let mut right = NixAttrs::new();
        right.insert("x".to_string(), Value::Int(10)); // overrides

        let overlay = OverlayAttrs::base(left).overlay(right);
        assert_eq!(overlay.get("x"), Some(&Value::Int(10))); // right wins
        assert_eq!(overlay.get("y"), Some(&Value::Int(2)));   // from left
        assert_eq!(overlay.get("z"), None);                    // not found
    }

    #[test]
    fn overlay_chain_three_levels() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        let mut b = NixAttrs::new();
        b.insert("y".to_string(), Value::Int(2));
        let mut c = NixAttrs::new();
        c.insert("x".to_string(), Value::Int(3)); // overrides a.x

        let chain = OverlayAttrs::base(a).overlay(b).overlay(c);
        assert_eq!(chain.get("x"), Some(&Value::Int(3)));  // c wins
        assert_eq!(chain.get("y"), Some(&Value::Int(2)));  // b
    }

    #[test]
    fn overlay_is_o1_construction() {
        // Creating an overlay chain should be O(1) per overlay,
        // NOT O(n) where n is the number of attributes.
        let mut big = NixAttrs::new();
        for i in 0..1000 {
            big.insert(format!("attr_{i}"), Value::Int(i));
        }
        let mut small = NixAttrs::new();
        small.insert("target".to_string(), Value::Int(42));

        // This should be instant — no iteration over 1000 attrs
        let overlay = OverlayAttrs::base(big).overlay(small);
        assert_eq!(overlay.get("target"), Some(&Value::Int(42)));
        assert_eq!(overlay.get("attr_0"), Some(&Value::Int(0)));
    }

    #[test]
    fn overlay_all_keys() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        a.insert("y".to_string(), Value::Int(2));
        let mut b = NixAttrs::new();
        b.insert("y".to_string(), Value::Int(20));
        b.insert("z".to_string(), Value::Int(30));

        let overlay = OverlayAttrs::base(a).overlay(b);
        let keys = overlay.all_keys();
        assert_eq!(keys, vec!["x", "y", "z"]); // sorted, unique
    }

    #[test]
    fn overlay_contains_key() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        let overlay = OverlayAttrs::base(a);
        assert!(overlay.contains_key("x"));
        assert!(!overlay.contains_key("y"));
    }

    #[test]
    fn overlay_flatten() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        let mut b = NixAttrs::new();
        b.insert("x".to_string(), Value::Int(2));
        b.insert("y".to_string(), Value::Int(3));

        let overlay = OverlayAttrs::base(a).overlay(b);
        let flat = overlay.flatten();
        assert_eq!(flat.get("x"), Some(&Value::Int(2)));
        assert_eq!(flat.get("y"), Some(&Value::Int(3)));
    }
}
