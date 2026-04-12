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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

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
}
