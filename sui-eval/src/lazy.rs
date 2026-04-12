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
}
