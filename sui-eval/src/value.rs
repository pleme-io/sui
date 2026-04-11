//! Nix value types and environments.
//!
//! The evaluator is single-threaded: `Env` and `NixAttrs` contain
//! `Rc<UnsafeCell<ThunkRepr>>` thunks.  All shared pointers use `Rc`
//! (not `Arc`) because the values are never sent across threads.

use std::cell::{Cell, OnceCell, RefCell, UnsafeCell};

use std::fmt;
pub use std::rc::Rc;

use rustc_hash::FxBuildHasher;
use smallvec::SmallVec;
pub use smol_str::SmolStr;

use rowan::ast::AstNode;

use sui_intern::{Interner, Symbol};

/// Type alias for the persistent hash map used by `NixAttrs` and `Env`.
///
/// Uses `FxBuildHasher` (fast multiplication-based hash) instead of the
/// default `RandomState`. This is optimal for `Symbol(u32)` keys where
/// the hash is a single multiply-shift — no SipHash overhead.
pub type FxHashMap<K, V> = im_rc::HashMap<K, V, FxBuildHasher>;

// -- Thread-local string interner --

thread_local! {
    static INTERNER: RefCell<Interner> = RefCell::new(Interner::new());
}

/// Intern a string key, returning a Symbol handle.
/// Used for NixAttrs keys and Env binding names.
pub fn intern(s: &str) -> Symbol {
    INTERNER.with(|i| i.borrow_mut().intern(s))
}

/// Resolve a Symbol back to its string content.
pub fn resolve(sym: Symbol) -> String {
    INTERNER.with(|i| i.borrow().resolve(sym).to_string())
}

// -- Identifier symbol cache --
//
// Caches the interned Symbol for each AST identifier by (source_id, text_offset).
// Avoids re-hashing identifier strings on repeated evaluations of the
// same expression (common in loops, recursion, overlay fixpoints).
//
// The source_id discriminates different parse trees (main file vs imports)
// so that identifiers at the same byte offset in different files don't
// collide in the cache.

thread_local! {
    /// Monotonically increasing counter — bumped on each `rnix::Root::parse`.
    static SOURCE_GEN: Cell<u32> = const { Cell::new(0) };

    /// Maps `(source_id, text_offset)` → interned `Symbol`.
    static IDENT_CACHE: RefCell<rustc_hash::FxHashMap<u64, Symbol>> =
        RefCell::new(rustc_hash::FxHashMap::default());
}

/// Allocate a new source ID for a freshly parsed AST tree.
///
/// Call once per `rnix::Root::parse` invocation. The returned ID is
/// used as the high 32 bits of the `IDENT_CACHE` key, ensuring that
/// identifiers from different source texts never collide.
pub fn next_source_id() -> u32 {
    SOURCE_GEN.with(|g| {
        let id = g.get();
        g.set(id.wrapping_add(1));
        id
    })
}

/// Intern a string with caching by source ID and AST text offset.
///
/// First call for a given `(source_id, text_offset)`: hash + intern
/// (same cost as [`intern`]).
/// Subsequent calls: `FxHashMap` u64 lookup (~5 ns) — no string hashing.
pub fn intern_cached(name: &str, source_id: u32, text_offset: u32) -> Symbol {
    let key = (u64::from(source_id) << 32) | u64::from(text_offset);
    IDENT_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        *cache.entry(key).or_insert_with(|| intern(name))
    })
}

/// Clear the identifier symbol cache.
///
/// Call between independent top-level evaluations to reclaim memory.
/// The cache grows unboundedly during a single evaluation pass.
pub fn clear_ident_cache() {
    IDENT_CACHE.with(|c| c.borrow_mut().clear());
}

// ── Nix string context ─────────────────────────────────────────

/// An element of a Nix string's context set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ContextElement {
    /// Store path reference (e.g., "/nix/store/abc-hello").
    Plain(SmolStr),
    /// Derivation output reference.
    Output { drv: SmolStr, output: SmolStr },
    /// Entire derivation closure.
    DrvDeep(SmolStr),
}

impl fmt::Display for ContextElement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContextElement::Plain(p) => write!(f, "{p}"),
            ContextElement::Output { drv, output } => write!(f, "{drv}!{output}"),
            ContextElement::DrvDeep(d) => write!(f, "={d}"),
        }
    }
}

/// The context attached to a Nix string: a set of store-path references that
/// the string depends on. Plain string literals have an empty context.
///
/// Uses a `Vec` with linear deduplication instead of `BTreeSet`.  Most strings
/// have 0-2 context elements where linear search is faster than tree overhead,
/// and `Vec` has the same size as `BTreeSet` (3 words) without per-node heap
/// allocations for small sets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StringContext(SmallVec<[ContextElement; 2]>);

impl StringContext {
    /// Create an empty context.
    pub fn new() -> Self {
        Self(SmallVec::new())
    }

    /// Merge another context into this one.
    pub fn merge(&mut self, other: &StringContext) {
        for elem in &other.0 {
            if !self.0.contains(elem) {
                self.0.push(elem.clone());
            }
        }
    }

    /// Add a plain store-path reference.
    pub fn add_plain(&mut self, path: impl Into<SmolStr>) {
        let elem = ContextElement::Plain(path.into());
        if !self.0.contains(&elem) {
            self.0.push(elem);
        }
    }

    /// Add a derivation output reference.
    pub fn add_output(&mut self, drv: impl Into<SmolStr>, output: impl Into<SmolStr>) {
        let elem = ContextElement::Output { drv: drv.into(), output: output.into() };
        if !self.0.contains(&elem) {
            self.0.push(elem);
        }
    }

    /// Add a derivation-deep reference.
    pub fn add_drv_deep(&mut self, drv: impl Into<SmolStr>) {
        let elem = ContextElement::DrvDeep(drv.into());
        if !self.0.contains(&elem) {
            self.0.push(elem);
        }
    }

    /// Whether this context set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Return the number of context elements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Iterate over all context elements.
    pub fn iter(&self) -> impl Iterator<Item = &ContextElement> {
        self.0.iter()
    }

    /// Insert a raw context element (deduplicating).
    pub fn insert(&mut self, elem: ContextElement) {
        if !self.0.contains(&elem) {
            self.0.push(elem);
        }
    }

    /// Return the elements as a slice.
    pub fn elements(&self) -> &[ContextElement] {
        &self.0
    }
}

/// A Nix string value with associated context (store-path references).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NixString {
    /// The character data.
    pub chars: SmolStr,
    /// The context set (empty for plain string literals).
    pub context: StringContext,
}

impl NixString {
    /// Create a context-free string.
    pub fn plain(s: impl Into<SmolStr>) -> Self {
        Self {
            chars: s.into(),
            context: StringContext::default(),
        }
    }

    /// Create a string with an explicit context.
    pub fn with_context(s: impl Into<SmolStr>, ctx: StringContext) -> Self {
        Self {
            chars: s.into(),
            context: ctx,
        }
    }

    /// Borrow the string content.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.chars
    }

    /// Whether this string carries any context (store path references).
    #[must_use]
    pub fn has_context(&self) -> bool {
        !self.context.is_empty()
    }
}

impl AsRef<str> for NixString {
    fn as_ref(&self) -> &str {
        &self.chars
    }
}

impl std::ops::Deref for NixString {
    type Target = str;

    fn deref(&self) -> &str {
        &self.chars
    }
}

impl fmt::Display for NixString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.chars)
    }
}

// ── Value enum ────────────────────────────────────────────────

/// A Nix value.
#[derive(Debug, Clone)]
#[derive(Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(Rc<NixString>),
    Path(Box<SmolStr>),
    List(Rc<Vec<Value>>),
    Attrs(Rc<NixAttrs>),
    Lambda(Box<Closure>),
    Builtin(Box<BuiltinFn>),
    /// A lazy value (thunk) with memoization and blackhole detection.
    Thunk(Thunk),
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Value>() <= 16);

/// Internal representation of a thunk's state machine.
///
/// Transitions: `Suspended` → `Blackhole` → `Evaluated` (on success),
/// or `Suspended` → `Blackhole` → `Suspended` (on failure, to allow retry).
pub enum ThunkRepr {
    /// Not yet evaluated. Holds the AST expression and captured environment.
    Suspended {
        expr: rnix::ast::Expr,
        env: Env,
    },
    /// Pending `inherit (source) name` selection. When forced,
    /// forces the shared `source_thunk` and pulls out `name`.
    ///
    /// The `source_thunk` is created once per `inherit (source) a b c`
    /// clause and shared (via `Rc` clone) across all inherited names.
    /// This means N names share one source evaluation instead of N
    /// independent evaluations — the source thunk's own memoization
    /// ensures it is evaluated at most once.
    ///
    /// This is its own variant (rather than synthesizing a Select AST
    /// node) because rnix doesn't expose a public AST builder, and
    /// we want each inherited name to defer evaluation of the source
    /// expression so that `inherit (lib.trivial) ...` at the top of
    /// trivial.nix doesn't blackhole on the still-being-constructed
    /// `lib.trivial`.
    InheritSelect {
        source_thunk: Thunk,
        name: SmolStr,
    },
    /// A lazy value backed by a Rust closure.  Used for flake input
    /// evaluation: the closure calls `evaluate_flake` on first access
    /// instead of eagerly during flake setup, matching CppNix semantics
    /// where each input's outputs function is wrapped in a thunk.
    Native(Box<dyn FnOnce() -> Result<Value, EvalError>>),
    /// Currently being evaluated -- detects infinite recursion.
    Blackhole,
    /// Already evaluated and memoized.
    Evaluated(Box<Value>),
}

/// Inner storage for a thunk: a fast-path `OnceCell` cache plus the
/// full `UnsafeCell` state machine.  Reads of already-evaluated thunks
/// hit the `OnceCell` and never touch the `UnsafeCell`, eliminating
/// all runtime overhead on the hot path (~150M+ cache hits per nixpkgs
/// eval).  The cold path (1.8M forces) uses `UnsafeCell` directly —
/// safe because the evaluator is single-threaded (`Rc`, not `Arc`) and
/// the state machine ensures no overlapping mutable access
/// (`Suspended` → `Blackhole` → `Evaluated` transitions are sequential).
struct ThunkInner {
    /// Fast-path cache for already-evaluated thunks.
    /// Set once when `Evaluated` is stored, never cleared.
    /// Reads bypass the `UnsafeCell` entirely.
    cache: OnceCell<Box<Value>>,
    /// Full state machine for the thunk lifecycle.
    repr: UnsafeCell<ThunkRepr>,
}

/// A lazy value with memoization and blackhole detection.
#[derive(Clone)]
pub struct Thunk(pub(crate) Rc<ThunkInner>);

impl Thunk {
    /// Create a thunk that will evaluate `expr` in `env` when forced.
    pub fn new_suspended(expr: rnix::ast::Expr, env: Env) -> Self {
        crate::trace::inc_thunks_created();
        Self(Rc::new(ThunkInner {
            cache: OnceCell::new(),
            repr: UnsafeCell::new(ThunkRepr::Suspended { expr, env }),
        }))
    }

    /// Create a thunk that, when forced, forces the shared
    /// `source_thunk` and pulls out the attribute named `name`.
    ///
    /// The caller creates ONE `Thunk::new_suspended(source_expr, env)`
    /// per `inherit (source)` clause and passes clones (Rc bump) to
    /// each inherited name.  This way the source is evaluated at most
    /// once regardless of how many names are inherited.
    pub fn new_inherit_select(source_thunk: Thunk, name: impl Into<SmolStr>) -> Self {
        crate::trace::inc_thunks_created();
        Self(Rc::new(ThunkInner {
            cache: OnceCell::new(),
            repr: UnsafeCell::new(ThunkRepr::InheritSelect {
                source_thunk,
                name: name.into(),
            }),
        }))
    }

    /// Create a thunk backed by a Rust closure.  When forced, the
    /// closure is called exactly once and its result is memoized.
    /// This is used for lazy flake input evaluation.
    pub fn new_native(f: impl FnOnce() -> Result<Value, EvalError> + 'static) -> Self {
        crate::trace::inc_thunks_created();
        Self(Rc::new(ThunkInner {
            cache: OnceCell::new(),
            repr: UnsafeCell::new(ThunkRepr::Native(Box::new(f))),
        }))
    }

    /// Create a thunk that is already evaluated (an optimization).
    /// Pre-populates the `OnceCell` cache so the fast path is
    /// immediately available.
    pub fn new_evaluated(value: Value) -> Self {
        crate::trace::inc_thunks_created();
        let cache = OnceCell::new();
        let _ = cache.set(Box::new(value.clone()));
        Self(Rc::new(ThunkInner {
            cache,
            repr: UnsafeCell::new(ThunkRepr::Evaluated(Box::new(value))),
        }))
    }

    /// Check whether this thunk has already been forced.
    /// Uses the `OnceCell` cache for a fast, borrow-free check.
    pub fn is_evaluated(&self) -> bool {
        self.0.cache.get().is_some()
    }

    /// Check whether this thunk is a native (Rust closure) thunk.
    ///
    /// Native thunks are used for lazy flake input evaluation and can
    /// be very expensive to force (e.g., evaluating all of nixpkgs).
    /// This lets callers skip them in eager conversion paths.
    pub fn is_native(&self) -> bool {
        // SAFETY: Single-threaded evaluator (Rc, not Arc). Read-only access,
        // no mutable reference exists at this point.
        matches!(unsafe { &*self.0.repr.get() }, ThunkRepr::Native(_))
    }

    /// Replace the environment captured in a suspended thunk.
    /// For `InheritSelect`, delegates to the shared source thunk's
    /// `update_env` (which updates the source's captured env).
    /// No-op if the thunk is already evaluated or a blackhole.
    pub fn update_env(&self, new_env: &Env) {
        // SAFETY: Single-threaded evaluator. No other reference to repr
        // exists during env replacement.
        let repr = unsafe { &mut *self.0.repr.get() };
        match repr {
            ThunkRepr::Suspended { env, .. } => {
                *env = new_env.clone();
            }
            ThunkRepr::InheritSelect { source_thunk, .. } => {
                source_thunk.update_env(new_env);
            }
            _ => {}
        }
    }

    /// Force this thunk using the given evaluator function.
    ///
    /// On first force: transitions Suspended -> Blackhole -> Evaluated.
    /// Re-entering a Blackhole signals infinite recursion.
    /// If the evaluated result is itself a thunk, it is forced transitively.
    ///
    /// Uses `stacker::maybe_grow` to ensure sufficient stack space for
    /// deeply nested thunk chains (e.g., nixpkgs overlay fixpoints).
    pub fn force(
        &self,
        evaluator: &dyn Fn(&rnix::ast::Expr, &Env) -> Result<Value, EvalError>,
    ) -> Result<Value, EvalError> {
        // Ultra-fast path: if already evaluated, return cached value
        // WITHOUT entering stacker::maybe_grow. This avoids the stack
        // check overhead on ~150M cache hits during nixpkgs evaluation.
        if let Some(cached) = self.0.cache.get() {
            crate::perf::inc(crate::perf::Counter::ThunkHit);
            return Ok((**cached).clone());
        }
        // Cold path: evaluation may recurse deeply, so use stacker.
        stacker::maybe_grow(64 * 1024, 2 * 1024 * 1024, || {
            self.force_inner(evaluator)
        })
    }

    /// Inner implementation of [`Thunk::force`] — called from the
    /// `stacker` trampoline.
    fn force_inner(
        &self,
        evaluator: &dyn Fn(&rnix::ast::Expr, &Env) -> Result<Value, EvalError>,
    ) -> Result<Value, EvalError> {
        // SAFETY (all `unsafe` blocks in this method): The evaluator is
        // single-threaded (`Rc`, not `Arc`).  `ThunkInner` is `!Send`/`!Sync`.
        // The `OnceCell` fast path handles all concurrent-safe reads (150M+
        // hits).  Only the cold path (1.8M forces) touches the `UnsafeCell`.
        // The state machine guarantees no overlapping mutable access:
        // Suspended → Blackhole → Evaluated transitions are sequential.

        // Ultra-fast path: check OnceCell cache (no borrow).
        if let Some(cached) = self.0.cache.get() {
            crate::perf::inc(crate::perf::Counter::ThunkHit);
            return Ok((**cached).clone());
        }

        let thunk_id = Rc::as_ptr(&self.0) as usize;

        // Take the current repr, replacing with Blackhole.
        // SAFETY: Single-threaded evaluator. State machine ensures no
        // overlapping mutable access: Suspended->Blackhole->Evaluated.
        let repr = std::mem::replace(unsafe { &mut *self.0.repr.get() }, ThunkRepr::Blackhole);

        match repr {
            ThunkRepr::Suspended { expr, env } => {
                crate::perf::inc(crate::perf::Counter::ThunkForce);
                crate::trace::inc_thunks_forced_unique();
                // Push force frame for chain capture (always on).
                let desc: String = expr
                    .syntax()
                    .text()
                    .to_string()
                    .chars()
                    .take(60)
                    .collect();
                crate::trace::push_force(crate::trace::ForceFrame {
                    defined_in: env.eval_file().cloned(),
                    description: desc.clone(),
                    thunk_id,
                });
                // Trace mode logging.
                crate::trace::trace_force_enter(
                    env.eval_file().map(|p| p.as_path()),
                    &desc,
                );
                // Check max force depth limit.
                if let Err(msg) = crate::trace::check_force_depth() {
                    crate::trace::dump_trace_on_error();
                    crate::trace::pop_force();
                    crate::trace::trace_force_exit();
                    *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Suspended {
                        expr,
                        env,
                    };
                    return Err(EvalError::InfiniteRecursion(msg));
                }
                // Push the thunk's captured eval_file onto the thread-local
                // stack so PathRel literals and relative imports inside the
                // thunk body resolve against the file where the thunk was
                // *defined*, not where it is forced from. The RAII guard
                // pops on drop (including on error paths).
                let _file_guard = env.eval_file().cloned().map(crate::eval::push_eval_file);
                match evaluator(&expr, &env) {
                    Ok(mut value) => {
                        // Store the result FIRST, then transitively force.
                        // This order is critical: by storing before forcing
                        // inner thunks, any re-entrant access to THIS thunk
                        // during inner-thunk forcing will find Evaluated
                        // (not Blackhole), enabling self-referential fixpoints
                        // like nixpkgs `let self = f self; in self`.
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(Box::new(value.clone()));
                        // Do NOT set the OnceCell cache here — the value may
                        // still be a thunk-in-thunk that needs unwrapping below.
                        // The cache is set after full unwrapping completes.
                        // Transitively unwrap thunk-in-thunk chains, with a
                        // depth limit to catch `let x = x; in x` cycles.
                        let mut depth = 0u32;
                        while let Value::Thunk(ref inner) = value {
                            depth += 1;
                            if depth > 2000 {
                                *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(Box::new(Value::Null));
                                crate::trace::dump_trace_on_error();
                                crate::trace::pop_force();
                                crate::trace::trace_force_exit();
                                return Err(EvalError::InfiniteRecursion(
                                    "thunk chain depth exceeded".to_string(),
                                ));
                            }
                            value = inner.force(evaluator)?;
                        }
                        // Update with the fully-unwrapped value.
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(Box::new(value.clone()));
                        let _ = self.0.cache.set(Box::new(value.clone()));
                        crate::trace::pop_force();
                        crate::trace::trace_force_exit();
                        Ok(value)
                    }
                    Err(e) => {
                        // Restore suspended state so the thunk can be retried or
                        // at least not left as a permanent blackhole.
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Suspended { expr, env };
                        crate::trace::dump_trace_on_error();
                        crate::trace::pop_force();
                        crate::trace::trace_force_exit();
                        Err(e)
                    }
                }
            }
            ThunkRepr::InheritSelect { source_thunk, name } => {
                // Push force frame for inherit-select thunks.
                let desc = format!("inherit (..) {name}");
                crate::trace::push_force(crate::trace::ForceFrame {
                    defined_in: None,
                    description: desc.clone(),
                    thunk_id,
                });
                crate::trace::trace_force_enter(None, &desc);
                crate::trace::inc_thunks_forced_unique();
                // Check max force depth limit.
                if let Err(msg) = crate::trace::check_force_depth() {
                    crate::trace::dump_trace_on_error();
                    crate::trace::pop_force();
                    crate::trace::trace_force_exit();
                    // SAFETY: Single-threaded evaluator (Rc, not Arc).
                    *unsafe { &mut *self.0.repr.get() } = ThunkRepr::InheritSelect {
                        source_thunk,
                        name,
                    };
                    return Err(EvalError::InfiniteRecursion(msg));
                }
                // Force the shared source thunk (cache hit after first
                // force — all sibling inherit names share this thunk).
                // Then select the requested attribute.
                let attempt = (|| -> Result<Value, EvalError> {
                    let mut forced = source_thunk.force(evaluator)?;
                    while let Value::Thunk(inner) = forced {
                        forced = inner.force(evaluator)?;
                    }
                    let attrs = match &forced {
                        Value::Attrs(a) => a,
                        _ => {
                            return Err(EvalError::TypeError(format!(
                                "inherit (source) {name}: source is {}, not a set",
                                forced.type_name()
                            )))
                        }
                    };
                    attrs
                        .get(&name)
                        .cloned()
                        .ok_or_else(|| EvalError::AttrNotFound(name.to_string()))
                })();
                match attempt {
                    Ok(mut value) => {
                        // Store first, then transitively force (same pattern
                        // as Suspended — enables fixpoint re-entry).
                        // SAFETY: Single-threaded evaluator (Rc, not Arc).
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(Box::new(value.clone()));
                        while let Value::Thunk(inner) = value {
                            value = inner.force(evaluator)?;
                        }
                        // SAFETY: Single-threaded evaluator (Rc, not Arc).
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(Box::new(value.clone()));
                        let _ = self.0.cache.set(Box::new(value.clone()));
                        crate::trace::pop_force();
                        crate::trace::trace_force_exit();
                        Ok(value)
                    }
                    Err(e) => {
                        // SAFETY: Single-threaded evaluator (Rc, not Arc).
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::InheritSelect { source_thunk, name };
                        crate::trace::dump_trace_on_error();
                        crate::trace::pop_force();
                        crate::trace::trace_force_exit();
                        Err(e)
                    }
                }
            }
            ThunkRepr::Native(f) => {
                // Push force frame for native thunks.
                crate::trace::push_force(crate::trace::ForceFrame {
                    defined_in: None,
                    description: "<native-thunk>".into(),
                    thunk_id,
                });
                crate::trace::trace_force_enter(None, "<native-thunk>");
                crate::trace::inc_thunks_forced_unique();
                // The closure is consumed (FnOnce).  On success we
                // memoize the result.  On failure we leave Blackhole
                // — unlike Suspended thunks the closure cannot be
                // retried because it has been consumed.
                match f() {
                    Ok(mut value) => {
                        // Store BEFORE transitively forcing — same pattern as
                        // Suspended. Enables fixpoint re-entry: if inner thunks
                        // reference this native thunk, they find Evaluated (not
                        // Blackhole). This is critical for flake-parts and
                        // NixOS module system fixpoints.
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(Box::new(value.clone()));
                        while let Value::Thunk(inner) = value {
                            value = inner.force(evaluator)?;
                        }
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(Box::new(value.clone()));
                        let _ = self.0.cache.set(Box::new(value.clone()));
                        crate::trace::pop_force();
                        crate::trace::trace_force_exit();
                        Ok(value)
                    }
                    Err(e) => {
                        // Cannot restore the closure — leave an
                        // evaluated Null so subsequent forces don't
                        // hit Blackhole and confuse the user with an
                        // "infinite recursion" message.
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(Box::new(Value::Null));
                        let _ = self.0.cache.set(Box::new(Value::Null));
                        crate::trace::dump_trace_on_error();
                        crate::trace::pop_force();
                        crate::trace::trace_force_exit();
                        Err(e)
                    }
                }
            }
            ThunkRepr::Blackhole => {
                // Capture the force chain leading to this cycle.
                let chain = crate::trace::capture_cycle(thunk_id);
                crate::trace::dump_trace_on_error();
                Err(EvalError::InfiniteRecursion(chain.to_string()))
            }
            ThunkRepr::Evaluated(v) => {
                // Reached if somehow the fast-path OnceCell check above
                // missed (should not happen in single-threaded code).
                // Put back, populate cache, and return the clone.
                crate::perf::inc(crate::perf::Counter::ThunkHit);
                let cloned = (*v).clone();
                let _ = self.0.cache.set(Box::new(cloned.clone()));
                *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(v);
                Ok(cloned)
            }
        }
    }
}

impl fmt::Debug for Thunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: Single-threaded evaluator, read-only access during formatting.
        match unsafe { &*self.0.repr.get() } {
            ThunkRepr::Suspended { .. } => write!(f, "<thunk>"),
            ThunkRepr::InheritSelect { name, .. } => write!(f, "<inherit-select {name}>"),
            ThunkRepr::Native(_) => write!(f, "<native-thunk>"),
            ThunkRepr::Blackhole => write!(f, "<blackhole>"),
            ThunkRepr::Evaluated(v) => write!(f, "{v:?}"),
        }
    }
}

/// A Nix attribute set.
///
/// Uses `FxHashMap` (im_rc::HashMap with FxBuildHasher) internally for
/// O(log32 n) lookups with O(1) structural-sharing clones (the hot path
/// during overlay fixpoint evaluation). FxHash is optimal for `Symbol(u32)`
/// keys — a single multiply-shift instead of SipHash. Sorted iteration
/// (needed by `attrNames`, `attrValues`, Display, equality) collects and sorts.
#[derive(Debug, Clone, Default)]
pub struct NixAttrs(FxHashMap<Symbol, Value>);

impl NixAttrs {
    /// Create an empty attribute set.
    pub fn new() -> Self {
        Self(FxHashMap::default())
    }

    /// Create an attribute set (capacity hint ignored — `im_rc::HashMap`
    /// uses structural sharing instead of pre-allocation).
    pub fn with_capacity(_capacity: usize) -> Self {
        Self(FxHashMap::default())
    }

    /// Borrow the underlying map (read-only).
    #[must_use]
    pub fn inner(&self) -> &FxHashMap<Symbol, Value> {
        &self.0
    }

    /// Collect and sort entries by resolved key name.
    fn sorted_entries(&self) -> Vec<(String, &Value)> {
        let mut pairs: Vec<(String, &Value)> = self.0.iter()
            .map(|(sym, v)| (resolve(*sym), v))
            .collect();
        pairs.sort_by(|(a, _), (b, _)| a.cmp(b));
        pairs
    }

    /// Look up an attribute by name.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(&intern(key))
    }

    /// Look up an attribute by pre-interned [`Symbol`].
    ///
    /// Skips the `intern()` call — use when the caller already has
    /// a cached symbol (e.g. from [`intern_cached`]).
    #[must_use]
    pub fn get_sym(&self, sym: &Symbol) -> Option<&Value> {
        self.0.get(sym)
    }

    /// Insert or overwrite an attribute.
    pub fn insert(&mut self, key: String, value: Value) {
        self.0.insert(intern(&key), value);
    }

    /// Check whether an attribute exists.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(&intern(key))
    }

    /// Iterate over attribute names in sorted order.
    ///
    /// Returns resolved `String` keys (not references) because the
    /// internal storage uses `Symbol` handles.
    pub fn keys(&self) -> impl Iterator<Item = String> {
        self.sorted_entries().into_iter().map(|(k, _)| k)
    }

    /// Iterate over (name, value) pairs in sorted key order.
    ///
    /// Returns resolved `String` keys because the internal storage
    /// uses `Symbol` handles.
    pub fn iter(&self) -> impl Iterator<Item = (String, &Value)> {
        self.sorted_entries().into_iter()
    }

    /// Iterate over (name, value) pairs in arbitrary order (fast path).
    /// Use this when sorted order is NOT required.
    pub fn iter_unsorted(&self) -> impl Iterator<Item = (String, &Value)> {
        self.0.iter().map(|(sym, v)| (resolve(*sym), v))
    }

    /// Iterate over values in sorted key order.
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.sorted_entries().into_iter().map(|(_, v)| v)
    }

    /// Remove an attribute, returning its value if present.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.0.remove(&intern(key))
    }

    /// Return the number of attributes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether this attribute set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Merge two attrsets (right overrides left, like `//`).
    ///
    /// O(m log n) where m = `other.len()` thanks to `FxHashMap`'s
    /// structural sharing — the left side is cloned in O(1).
    #[must_use]
    pub fn update(&self, other: &NixAttrs) -> NixAttrs {
        let mut result = self.0.clone(); // O(1) structural sharing
        for (k, v) in other.0.iter() {
            result.insert(*k, v.clone());
        }
        NixAttrs(result)
    }
}

impl FromIterator<(String, Value)> for NixAttrs {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        NixAttrs(iter.into_iter().map(|(k, v)| (intern(&k), v)).collect())
    }
}

impl IntoIterator for NixAttrs {
    type Item = (String, Value);
    type IntoIter = Box<dyn Iterator<Item = (String, Value)>>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.0.into_iter().map(|(sym, v)| (resolve(sym), v)))
    }
}

/// A closure — lambda + captured environment.
///
/// Stores rnix AST nodes so we can re-evaluate the body in the captured env.
///
/// The environment is `Rc`-wrapped so that cloning a closure (e.g., once per
/// element in `map`/`filter`) is a refcount bump instead of a deep copy of the
/// entire binding map.
#[derive(Debug, Clone)]
pub struct Closure {
    pub param: rnix::ast::Param,
    pub body: rnix::ast::Expr,
    pub env: Env,
}

/// The function signature stored inside a [`BuiltinFn`].
pub type BuiltinFunc = dyn Fn(&[Value]) -> Result<Value, EvalError>;

/// A builtin function.
///
/// Not `Send`/`Sync` because `Value` contains rnix AST nodes (rowan `SyntaxNode`)
/// which use `NonNull` internally. The evaluator is single-threaded.
#[derive(Clone)]
pub struct BuiltinFn {
    /// Name used for display and debug printing.
    pub name: &'static str,
    /// The implementation closure.
    pub func: Rc<BuiltinFunc>,
}

impl fmt::Debug for BuiltinFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<builtin {}>", self.name)
    }
}

/// A `with` scope with optional cached forced attrset.
///
/// On first lookup, the scope value is forced and the resulting attrset
/// is cached.  Subsequent lookups skip forcing entirely.
///
/// The cache is wrapped in `Rc<RefCell<…>>` so that child environments
/// (which clone the `Vec<WithScope>`) share the same cache cell —
/// once any environment forces a scope, every related environment
/// benefits.
#[derive(Clone)]
struct WithScope {
    value: Value,
    /// Cached forced attrset.  Shared via Rc so child environments
    /// benefit from a parent having already forced the scope.
    cached: Rc<RefCell<Option<NixAttrs>>>,
}

impl fmt::Debug for WithScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WithScope")
            .field("value", &self.value)
            .field("cached", &self.cached.borrow().is_some())
            .finish()
    }
}

/// Inner data for an evaluation environment.
///
/// Wrapped in `Rc` by [`Env`] so that cloning an `Env` is always a
/// refcount bump — never a deep copy of the binding map.
///
/// Uses a flattened `FxHashMap` for bindings: `child()` clones
/// the parent's map with O(1) structural sharing instead of building
/// a linked parent chain. Lookups are a single O(log32 n) probe
/// instead of walking a chain.
#[derive(Debug, Clone, Default)]
struct EnvInner {
    bindings: FxHashMap<Symbol, Value>,
    /// Dynamic `with` scopes, innermost last.
    with_scopes: Vec<WithScope>,
    /// Source file currently being evaluated, for relative path
    /// literals (`./foo.nix`) inside function defaults that get
    /// evaluated *after* control has left the file scope.
    eval_file: Option<std::path::PathBuf>,
}

/// Evaluation environment — flattened binding map with structural sharing.
///
/// Internally an `Rc<EnvInner>`, so cloning is always O(1) (refcount
/// bump).  `child()` clones the `FxHashMap` (O(1) structural
/// sharing) instead of building a parent chain.  `bind()` uses
/// `Rc::make_mut` for copy-on-write: if the Rc is shared, only then
/// does it clone the inner data.
#[derive(Clone, Default)]
pub struct Env(Rc<EnvInner>);

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Env {
    /// Create a root environment with no bindings.
    #[must_use]
    pub fn new() -> Self {
        Self(Rc::new(EnvInner {
            bindings: FxHashMap::default(),
            with_scopes: Vec::new(),
            eval_file: None,
        }))
    }

    /// Create a child environment that inherits from this one.
    ///
    /// O(1) — the `FxHashMap` clone is structural sharing (refcount
    /// bump on internal tree nodes), not a deep copy.
    #[must_use]
    pub fn child(&self) -> Self {
        crate::perf::inc(crate::perf::Counter::EnvClone);
        Self(Rc::new(EnvInner {
            bindings: self.0.bindings.clone(), // O(1) structural sharing
            with_scopes: self.0.with_scopes.clone(),
            // Children inherit the parent's eval file so that
            // path literals nested deep in let-chains still
            // resolve against the right directory.
            eval_file: self.0.eval_file.clone(),
        }))
    }

    /// Attach a `with` scope to this environment.
    ///
    /// The value is stored lazily and only forced when a name lookup
    /// actually needs it, matching CppNix's lazy `with` semantics.
    /// Scopes are pushed to the end of the vec (innermost last).
    #[must_use]
    pub fn with_scope(mut self, value: Value) -> Self {
        Rc::make_mut(&mut self.0).with_scopes.push(WithScope {
            value,
            cached: Rc::new(RefCell::new(None)),
        });
        self
    }

    /// Bind a name to a value in this environment's own scope.
    ///
    /// Uses copy-on-write: if the inner `Rc` is shared, clones the
    /// inner data before mutating.
    pub fn bind(&mut self, name: String, value: Value) {
        Rc::make_mut(&mut self.0).bindings.insert(intern(&name), value);
    }

    /// Get the eval_file for this environment.
    #[must_use]
    pub fn eval_file(&self) -> Option<&std::path::PathBuf> {
        self.0.eval_file.as_ref()
    }

    /// Set the eval_file for this environment.
    pub fn set_eval_file(&mut self, file: Option<std::path::PathBuf>) {
        Rc::make_mut(&mut self.0).eval_file = file;
    }

    /// Lookup matching Nix semantics:
    ///
    /// 1. Probe the flattened binding map (single O(log32 n) lookup).
    ///    Any explicit `let`/`rec`/function-arg binding wins over every
    ///    `with` scope.
    /// 2. If no lexical binding matched, iterate `with_scopes` in
    ///    reverse order (innermost first). So `with X; with Y; x`
    ///    finds `x` in Y if Y has it, otherwise in X.
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<Value> {
        crate::perf::inc(crate::perf::Counter::EnvLookup);
        // 1. Flat lexical lookup — single O(1) hash + O(log32 n) probe.
        let sym = intern(name);
        if let Some(v) = self.0.bindings.get(&sym) {
            return Some(v.clone());
        }
        // 2. With-scope lookup — iterate innermost-first (reverse order).
        for scope in self.0.with_scopes.iter().rev() {
            // Fast path: use cached forced attrset
            {
                let cache = scope.cached.borrow();
                if let Some(ref attrs) = *cache {
                    if let Some(v) = attrs.get(name) {
                        return Some(v.clone());
                    }
                    continue;
                }
            }
            // Slow path: force, cache, then check
            if let Ok(forced) = crate::eval::force_value(&scope.value) {
                if let Value::Attrs(ref attrs) = forced {
                    let result = attrs.get(name).cloned();
                    *scope.cached.borrow_mut() = Some((**attrs).clone());
                    if result.is_some() {
                        return result;
                    }
                }
            }
            // If forcing fails or it's not an attrset, try next scope
        }
        None
    }

    /// Look up a binding by pre-interned [`Symbol`].
    ///
    /// Same semantics as [`lookup`](Self::lookup) but skips the
    /// `intern()` call — for use when the caller has already cached
    /// the symbol (e.g. via [`intern_cached`]).
    #[must_use]
    pub fn lookup_sym(&self, sym: Symbol) -> Option<Value> {
        crate::perf::inc(crate::perf::Counter::EnvLookup);
        // 1. Flat lexical lookup — single O(1) hash + O(log32 n) probe.
        if let Some(v) = self.0.bindings.get(&sym) {
            return Some(v.clone());
        }
        // 2. With-scope lookup — iterate innermost-first (reverse order).
        for scope in self.0.with_scopes.iter().rev() {
            // Fast path: use cached forced attrset
            {
                let cache = scope.cached.borrow();
                if let Some(ref attrs) = *cache {
                    if let Some(v) = attrs.get_sym(&sym) {
                        return Some(v.clone());
                    }
                    continue;
                }
            }
            // Slow path: force, cache, then check
            if let Ok(forced) = crate::eval::force_value(&scope.value) {
                if let Value::Attrs(ref attrs) = forced {
                    let result = attrs.get_sym(&sym).cloned();
                    *scope.cached.borrow_mut() = Some((**attrs).clone());
                    if result.is_some() {
                        return result;
                    }
                }
            }
            // If forcing fails or it's not an attrset, try next scope
        }
        None
    }
}

/// Evaluation errors produced by the Nix evaluator.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EvalError {
    /// A variable was referenced but not bound in scope.
    #[error("undefined variable: {0}")]
    UndefinedVar(String),
    /// A type mismatch or coercion failure.
    #[error("type error: {0}")]
    TypeError(String),
    /// An attribute was selected from a set that does not contain it.
    #[error("attribute not found: {0}")]
    AttrNotFound(String),
    /// A type mismatch with structured expected/got information.
    #[error("type error: expected {expected}, got {got}")]
    TypeMismatch {
        expected: &'static str,
        got: &'static str,
    },
    /// An `assert` expression's condition evaluated to false.
    #[error("assertion failed{0}")]
    AssertionFailed(String),
    /// Integer division by zero.
    #[error("division by zero")]
    DivisionByZero,
    /// Infinite recursion detected (thunk blackhole or eval depth).
    #[error("infinite recursion ({0})")]
    InfiniteRecursion(String),
    /// An I/O error from the host filesystem.
    #[error("I/O error: {context}: {message}")]
    IoError { context: String, message: String },
    /// Explicit `throw` or `abort` from Nix code.
    #[error("{0}")]
    Throw(String),
    /// A language feature that is not yet implemented.
    #[error("not yet implemented: {0}")]
    NotImplemented(String),
    /// A syntax error in the input expression.
    #[error("parse error: {0}")]
    ParseError(String),
    /// Maximum recursion depth exceeded.
    #[error("recursion limit: {0}")]
    RecursionLimit(String),
}

impl EvalError {
    /// Convenience constructor for a `TypeError` variant.
    #[must_use]
    pub fn type_error(msg: impl Into<String>) -> Self {
        EvalError::TypeError(msg.into())
    }

    /// Convenience constructor for a `TypeMismatch` variant.
    #[must_use]
    pub fn type_mismatch(expected: &'static str, got: &'static str) -> Self {
        EvalError::TypeMismatch { expected, got }
    }

    /// Create a type error for a builtin argument type mismatch.
    #[must_use]
    pub fn builtin_type(builtin: &str, expected: &str, got: &str) -> Self {
        EvalError::TypeError(format!("{builtin}: expected {expected}, got {got}"))
    }

    /// Create a type error for a binary operator type mismatch.
    #[must_use]
    pub fn op_type(op: &str, lhs: &str, rhs: &str) -> Self {
        EvalError::TypeError(format!("cannot {op} {lhs} and {rhs}"))
    }

    /// Whether this error was caused by `throw` or `abort`.
    #[must_use]
    pub fn is_throw(&self) -> bool {
        matches!(self, EvalError::Throw(_))
    }

    /// Whether this error is an infinite recursion.
    #[must_use]
    pub fn is_infinite_recursion(&self) -> bool {
        matches!(self, EvalError::InfiniteRecursion(_))
    }
}

impl Value {
    /// Convenience constructor for a context-free string.
    #[must_use]
    pub fn string(s: impl Into<SmolStr>) -> Self {
        Value::String(Rc::new(NixString::plain(s)))
    }

    /// Convenience constructor that wraps a `Vec<Value>` in `Rc` for the
    /// `List` variant.
    #[must_use]
    pub fn list(items: Vec<Value>) -> Self {
        Value::List(Rc::new(items))
    }

    /// Convert a value to JSON for API output.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(n) => serde_json::json!(n),
            Value::Float(f) => serde_json::json!(f),
            Value::String(s) => serde_json::Value::String(s.chars.to_string()),
            Value::Path(p) => serde_json::Value::String(p.to_string()),
            Value::List(items) => {
                serde_json::Value::Array(items.iter().map(|v| v.to_json()).collect())
            }
            Value::Attrs(attrs) => {
                let map: serde_json::Map<String, serde_json::Value> = attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_json()))
                    .collect();
                serde_json::Value::Object(map)
            }
            Value::Lambda(_) => serde_json::Value::String("<lambda>".to_string()),
            Value::Builtin(b) => serde_json::Value::String(format!("<builtin {}>", b.name)),
            Value::Thunk(thunk) => {
                // Force the thunk for JSON conversion.
                match thunk.force(&|expr, env| crate::eval::eval_expr(expr, env)) {
                    Ok(v) => v.to_json(),
                    Err(_) => serde_json::Value::String("<thunk:error>".to_string()),
                }
            }
        }
    }

    /// Return the Nix type name for this value (e.g. `"int"`, `"set"`).
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::String(_) => "string",
            Value::Path(_) => "path",
            Value::List(_) => "list",
            Value::Attrs(_) => "set",
            Value::Lambda(_) => "lambda",
            Value::Builtin(_) => "lambda",
            Value::Thunk(thunk) => {
                // Force and delegate.
                match thunk.force(&|expr, env| crate::eval::eval_expr(expr, env)) {
                    Ok(v) => v.type_name(),
                    Err(_) => "thunk",
                }
            }
        }
    }

    // ── Value coercion methods ──────────────────────────────────
    //
    // Naming conventions:
    //
    // • `as_*(&self)` — borrow. Returns a reference or Copy type.
    //   Primitives (`as_bool`, `as_int`) force thunks transparently
    //   because they return owned Copy values. Reference accessors
    //   (`as_string`, `as_nix_string`, `as_attrs`, `as_list`) CANNOT
    //   force thunks (the forced value is transient and we can't
    //   return a borrow into it), so they error on Thunk inputs.
    //
    // • `to_*(&self)` — clone / force. Returns an owned value and
    //   DOES force thunks. Use when the value may be a thunk and you
    //   need an owned result. Examples: `to_float`, `to_string`,
    //   `to_attrs`, `to_list`.
    //
    // • `coerce_to_path` — a Nix-specific coercion that accepts both
    //   Path and String values (many builtins accept either).

    /// Extract a bool, forcing thunks if needed.
    pub fn as_bool(&self) -> Result<bool, EvalError> {
        match self {
            Value::Bool(b) => Ok(*b),
            Value::Thunk(thunk) => {
                thunk.force(&|e, env| crate::eval::eval_expr(e, env))?.as_bool()
            }
            _ => Err(EvalError::TypeMismatch { expected: "bool", got: self.type_name() }),
        }
    }

    /// Extract an integer, forcing thunks if needed.
    pub fn as_int(&self) -> Result<i64, EvalError> {
        match self {
            Value::Int(n) => Ok(*n),
            Value::Thunk(thunk) => {
                thunk.force(&|e, env| crate::eval::eval_expr(e, env))?.as_int()
            }
            _ => Err(EvalError::TypeMismatch { expected: "int", got: self.type_name() }),
        }
    }

    /// Borrow the string content without forcing thunks.
    pub fn as_string(&self) -> Result<&str, EvalError> {
        match self {
            Value::String(s) => Ok(&s.chars),
            // Note: we cannot return a reference into a forced thunk here
            // because the forced value is transient. Callers that go through
            // force_value() in eval.rs will match on the concrete value.
            Value::Thunk(_) => Err(EvalError::TypeError(
                "thunk in as_string: force first via force_value()".into(),
            )),
            _ => Err(EvalError::TypeMismatch { expected: "string", got: self.type_name() }),
        }
    }

    /// Return a reference to the full `NixString` (with context).
    pub fn as_nix_string(&self) -> Result<&NixString, EvalError> {
        match self {
            Value::String(ns) => Ok(ns),
            Value::Thunk(_) => Err(EvalError::TypeError(
                "thunk in as_nix_string: force first via force_value()".into(),
            )),
            _ => Err(EvalError::TypeMismatch { expected: "string", got: self.type_name() }),
        }
    }

    /// Force-aware string extraction. Returns an owned String by forcing
    /// thunks if needed. Use this instead of `as_string()` when you may
    /// be operating on thunked attrset values.
    pub fn to_str(&self) -> Result<String, EvalError> {
        match self {
            Value::String(s) => Ok(s.chars.to_string()),
            Value::Thunk(thunk) => {
                let forced = thunk.force(&|e, env| crate::eval::eval_expr(e, env))?;
                forced.to_str()
            }
            _ => Err(EvalError::TypeMismatch { expected: "string", got: self.type_name() }),
        }
    }

    /// Force-aware `NixString` extraction. Returns an owned `NixString`
    /// (with context) by forcing thunks if needed.
    pub fn to_nix_string(&self) -> Result<NixString, EvalError> {
        match self {
            Value::String(s) => Ok((**s).clone()),
            Value::Thunk(thunk) => {
                let forced = thunk.force(&|e, env| crate::eval::eval_expr(e, env))?;
                forced.to_nix_string()
            }
            _ => Err(EvalError::TypeMismatch { expected: "string", got: self.type_name() }),
        }
    }

    /// Borrow the inner attrs without forcing. If the value is a
    /// thunk, the caller should have force_value'd it first; we
    /// return an error rather than silently mutating the thunk
    /// (which would require &mut self).
    ///
    /// Most call sites should use `to_attrs()` (which forces and
    /// clones) unless they're certain the value is already
    /// concrete and want to avoid the clone.
    pub fn as_attrs(&self) -> Result<&NixAttrs, EvalError> {
        match self {
            Value::Attrs(a) => Ok(a),
            Value::Thunk(_) => Err(EvalError::TypeError(
                "thunk in as_attrs: force first via force_value() or use to_attrs()".into(),
            )),
            _ => Err(EvalError::TypeMismatch { expected: "set", got: self.type_name() }),
        }
    }

    /// Borrow the list content without forcing thunks.
    pub fn as_list(&self) -> Result<&[Value], EvalError> {
        match self {
            Value::List(l) => Ok(l.as_slice()),
            Value::Thunk(_) => Err(EvalError::TypeError(
                "thunk in as_list: force first via force_value()".into(),
            )),
            _ => Err(EvalError::TypeMismatch { expected: "list", got: self.type_name() }),
        }
    }

    /// Force-aware attrs extraction. Forces the value if it is a thunk.
    pub fn to_attrs(&self) -> Result<NixAttrs, EvalError> {
        match self {
            Value::Attrs(a) => Ok((**a).clone()),
            Value::Thunk(thunk) => {
                let forced = thunk.force(&|e, env| crate::eval::eval_expr(e, env))?;
                forced.to_attrs()
            }
            _ => Err(EvalError::TypeMismatch { expected: "set", got: self.type_name() }),
        }
    }

    /// Force-aware list extraction. Forces the value if it is a thunk.
    pub fn to_list(&self) -> Result<Vec<Value>, EvalError> {
        match self {
            Value::List(l) => Ok((**l).clone()),
            Value::Thunk(thunk) => {
                let forced = thunk.force(&|e, env| crate::eval::eval_expr(e, env))?;
                forced.to_list()
            }
            _ => Err(EvalError::TypeMismatch { expected: "list", got: self.type_name() }),
        }
    }

    /// Extract a filesystem path from a `Path` or `String` value.
    ///
    /// Many builtins (`readFile`, `import`, `pathExists`, etc.) accept
    /// either `Path` or `String` arguments. This method centralises
    /// that coercion so every call-site doesn't repeat the same match.
    pub fn coerce_to_path(&self, context: &str) -> Result<String, EvalError> {
        match self {
            Value::Path(p) => Ok(p.to_string()),
            Value::String(ns) => Ok(ns.chars.to_string()),
            Value::Attrs(attrs) => {
                if let Some(out_path) = attrs.get("outPath") {
                    let forced = crate::eval::force_value(out_path)?;
                    forced.coerce_to_path(context)
                } else {
                    Err(EvalError::TypeError(format!(
                        "{context}: expected path or string, got set without outPath"
                    )))
                }
            }
            _ => Err(EvalError::TypeError(format!(
                "{context}: expected path or string, got {}",
                self.type_name()
            ))),
        }
    }

    /// Coerce a numeric value to float.
    pub fn to_float(&self) -> Result<f64, EvalError> {
        match self {
            Value::Float(f) => Ok(*f),
            Value::Int(n) => Ok(*n as f64),
            Value::Thunk(thunk) => {
                thunk.force(&|e, env| crate::eval::eval_expr(e, env))?.to_float()
            }
            _ => Err(EvalError::TypeMismatch { expected: "number", got: self.type_name() }),
        }
    }

    /// Coerce this value to a string following CppNix semantics.
    ///
    /// This is the single source of truth for string coercion used by
    /// string interpolation, `builtins.toString`, and derivation env
    /// var construction.
    ///
    /// Rules (in order):
    /// - String → its content (with context)
    /// - Path → path string (adds Plain context element)
    /// - Int → decimal representation
    /// - Float → decimal representation
    /// - Bool → "1" for true, "" for false
    /// - Null → ""
    /// - Attrs with `__toString` → call `__toString(self)` and coerce result
    /// - Attrs with `outPath` → coerce outPath recursively
    /// - List → space-joined coerced elements
    /// - Lambda/Builtin/Thunk → error
    pub fn coerce_to_string(&self) -> Result<(String, StringContext), EvalError> {
        let mut ctx = StringContext::new();
        let s = match self {
            Value::String(ns) => {
                ctx.merge(&ns.context);
                ns.chars.to_string()
            }
            Value::Path(p) => {
                ctx.add_plain((**p).clone());
                p.to_string()
            }
            Value::Int(n) => n.to_string(),
            Value::Float(f) => format!("{f}"),
            Value::Bool(true) => "1".to_string(),
            Value::Bool(false) => String::new(),
            Value::Null => String::new(),
            Value::Attrs(attrs) => {
                if let Some(to_str) = attrs.get("__toString") {
                    let result =
                        crate::eval::apply(to_str.clone(), Value::Attrs(attrs.clone()))?;
                    let forced = crate::eval::force_value(&result)?;
                    let (s, c) = forced.coerce_to_string()?;
                    ctx.merge(&c);
                    s
                } else if let Some(out_path) = attrs.get("outPath") {
                    let forced = crate::eval::force_value(out_path)?;
                    let (s, c) = forced.coerce_to_string()?;
                    ctx.merge(&c);
                    s
                } else {
                    return Err(EvalError::TypeError(
                        "cannot coerce set to string (no __toString or outPath)".into(),
                    ));
                }
            }
            Value::List(items) => {
                let mut parts = Vec::new();
                for item in items.iter() {
                    let forced = crate::eval::force_value(item)?;
                    let (s, c) = forced.coerce_to_string()?;
                    ctx.merge(&c);
                    parts.push(s);
                }
                parts.join(" ")
            }
            other => {
                return Err(EvalError::TypeError(format!(
                    "cannot coerce {} to string",
                    other.type_name()
                )));
            }
        };
        Ok((s, ctx))
    }
}

// ── Conversions from foreign value types ────────────────────

impl From<&serde_json::Value> for Value {
    fn from(json: &serde_json::Value) -> Self {
        match json {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else {
                    Value::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => Value::string(s.clone()),
            serde_json::Value::Array(arr) => {
                Value::List(Rc::new(arr.iter().map(Value::from).collect()))
            }
            serde_json::Value::Object(obj) => {
                let mut attrs = NixAttrs::new();
                for (k, v) in obj {
                    attrs.insert(k.clone(), Value::from(v));
                }
                Value::Attrs(Rc::new(attrs))
            }
        }
    }
}

impl From<&toml::Value> for Value {
    fn from(v: &toml::Value) -> Self {
        match v {
            toml::Value::String(s) => Value::string(s.clone()),
            toml::Value::Integer(n) => Value::Int(*n),
            toml::Value::Float(f) => Value::Float(*f),
            toml::Value::Boolean(b) => Value::Bool(*b),
            toml::Value::Array(arr) => {
                Value::List(Rc::new(arr.iter().map(Value::from).collect()))
            }
            toml::Value::Table(t) => {
                let mut attrs = NixAttrs::new();
                for (k, val) in t {
                    attrs.insert(k.clone(), Value::from(val));
                }
                Value::Attrs(Rc::new(attrs))
            }
            toml::Value::Datetime(dt) => Value::string(dt.to_string()),
        }
    }
}


// ── From impls for ergonomic Value construction ─────────────

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::Int(n)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

impl From<NixString> for Value {
    fn from(s: NixString) -> Self {
        Value::String(Rc::new(s))
    }
}

impl From<NixAttrs> for Value {
    fn from(attrs: NixAttrs) -> Self {
        Value::Attrs(Rc::new(attrs))
    }
}

impl From<Vec<Value>> for Value {
    fn from(list: Vec<Value>) -> Self {
        Value::List(Rc::new(list))
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        // Force thunks before comparing.
        let force = |v: &Value| -> Value {
            match v {
                Value::Thunk(t) => t
                    .force(&|e, env| crate::eval::eval_expr(e, env))
                    .unwrap_or(Value::Null),
                other => other.clone(),
            }
        };
        let l = force(self);
        let r = force(other);

        match (&l, &r) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => (*a as f64) == *b,
            (Value::String(a), Value::String(b)) => a.chars == b.chars,
            (Value::Path(a), Value::Path(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Attrs(a), Value::Attrs(b)) => a.inner() == b.inner(),
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "\"{}\"", s.chars.replace('\\', "\\\\").replace('"', "\\\"")),
            Value::Path(p) => write!(f, "{p}"),
            Value::List(items) => {
                write!(f, "[ ")?;
                for item in items.iter() {
                    write!(f, "{item} ")?;
                }
                write!(f, "]")
            }
            Value::Attrs(attrs) => {
                write!(f, "{{ ")?;
                for (k, v) in attrs.iter() {
                    write!(f, "{k} = {v}; ")?;
                }
                write!(f, "}}")
            }
            Value::Lambda(_) => write!(f, "<<lambda>>"),
            Value::Builtin(b) => write!(f, "<<builtin {}>>" , b.name),
            Value::Thunk(thunk) => {
                match thunk.force(&|e, env| crate::eval::eval_expr(e, env)) {
                    Ok(v) => write!(f, "{v}"),
                    Err(_) => write!(f, "<<thunk:error>>"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    // ── Value size assertion ──────────────────────────────

    #[test]
    fn value_is_16_bytes() {
        assert_eq!(std::mem::size_of::<Value>(), 16);
    }

    // ── Value::to_json for every variant ─────────────────

    #[test]
    fn to_json_null() {
        assert_eq!(Value::Null.to_json(), serde_json::Value::Null);
    }

    #[test]
    fn to_json_bool() {
        assert_eq!(Value::Bool(true).to_json(), serde_json::Value::Bool(true));
        assert_eq!(Value::Bool(false).to_json(), serde_json::Value::Bool(false));
    }

    #[test]
    fn to_json_int() {
        assert_eq!(Value::Int(42).to_json(), serde_json::json!(42));
    }

    #[test]
    fn to_json_float() {
        assert_eq!(Value::Float(3.14).to_json(), serde_json::json!(3.14));
    }

    #[test]
    fn to_json_string() {
        assert_eq!(
            Value::string("hello").to_json(),
            serde_json::Value::String("hello".to_string()),
        );
    }

    #[test]
    fn to_json_path() {
        assert_eq!(
            Value::Path(Box::new(SmolStr::from("/nix/store"))).to_json(),
            serde_json::Value::String("/nix/store".to_string()),
        );
    }

    #[test]
    fn to_json_list() {
        let v = Value::list(vec![Value::Int(1), Value::Bool(true)]);
        assert_eq!(v.to_json(), serde_json::json!([1, true]));
    }

    #[test]
    fn to_json_attrs() {
        let mut attrs = NixAttrs::new();
        attrs.insert("a".to_string(), Value::Int(1));
        let v = Value::Attrs(Rc::new(attrs));
        assert_eq!(v.to_json(), serde_json::json!({"a": 1}));
    }

    #[test]
    fn to_json_lambda() {
        // Build a minimal rnix lambda for testing
        let root = rnix::Root::parse("x: x");
        let expr = root.tree().expr().unwrap();
        let lambda = match expr {
            rnix::ast::Expr::Lambda(l) => l,
            _ => panic!("expected lambda"),
        };
        let closure = Closure {
            param: lambda.param().unwrap(),
            body: lambda.body().unwrap(),
            env: Env::new(),
        };
        assert_eq!(
            Value::Lambda(Box::new(closure)).to_json(),
            serde_json::Value::String("<lambda>".to_string()),
        );
    }

    #[test]
    fn to_json_builtin() {
        let b = BuiltinFn {
            name: "test",
            func: Rc::new(|_| Ok(Value::Null)),
        };
        assert_eq!(
            Value::Builtin(Box::new(b)).to_json(),
            serde_json::Value::String("<builtin test>".to_string()),
        );
    }

    // ── Value::type_name for every variant ───────────────

    #[test]
    fn type_name_null() { assert_eq!(Value::Null.type_name(), "null"); }

    #[test]
    fn type_name_bool() { assert_eq!(Value::Bool(false).type_name(), "bool"); }

    #[test]
    fn type_name_int() { assert_eq!(Value::Int(0).type_name(), "int"); }

    #[test]
    fn type_name_float() { assert_eq!(Value::Float(0.0).type_name(), "float"); }

    #[test]
    fn type_name_string() { assert_eq!(Value::string("").type_name(), "string"); }

    #[test]
    fn type_name_path() { assert_eq!(Value::Path(Box::new(SmolStr::from(""))).type_name(), "path"); }

    #[test]
    fn type_name_list() { assert_eq!(Value::list(vec![]).type_name(), "list"); }

    #[test]
    fn type_name_set() { assert_eq!(Value::Attrs(Rc::new(NixAttrs::new())).type_name(), "set"); }

    #[test]
    fn type_name_lambda() {
        let root = rnix::Root::parse("x: x");
        let expr = root.tree().expr().unwrap();
        let lambda = match expr {
            rnix::ast::Expr::Lambda(l) => l,
            _ => panic!("expected lambda"),
        };
        let closure = Closure {
            param: lambda.param().unwrap(),
            body: lambda.body().unwrap(),
            env: Env::new(),
        };
        assert_eq!(Value::Lambda(Box::new(closure)).type_name(), "lambda");
    }

    #[test]
    fn type_name_builtin() {
        let b = BuiltinFn {
            name: "t",
            func: Rc::new(|_| Ok(Value::Null)),
        };
        assert_eq!(Value::Builtin(Box::new(b)).type_name(), "lambda");
    }

    // ── as_* error on wrong type ─────────────────────────

    #[test]
    fn as_bool_error_on_non_bool() {
        assert!(Value::Int(1).as_bool().is_err());
        assert!(Value::string("true").as_bool().is_err());
    }

    #[test]
    fn as_int_error_on_non_int() {
        assert!(Value::Bool(true).as_int().is_err());
        assert!(Value::Float(1.0).as_int().is_err());
    }

    #[test]
    fn as_string_error_on_non_string() {
        assert!(Value::Int(42).as_string().is_err());
        assert!(Value::Null.as_string().is_err());
    }

    #[test]
    fn as_attrs_error_on_non_attrs() {
        assert!(Value::Int(1).as_attrs().is_err());
        assert!(Value::list(vec![]).as_attrs().is_err());
    }

    #[test]
    fn as_list_error_on_non_list() {
        assert!(Value::Int(1).as_list().is_err());
        assert!(Value::Attrs(Rc::new(NixAttrs::new())).as_list().is_err());
    }

    // ── to_float int->float coercion ─────────────────────

    #[test]
    fn to_float_coerces_int() {
        assert_eq!(Value::Int(5).to_float().unwrap(), 5.0);
        assert_eq!(Value::Float(2.5).to_float().unwrap(), 2.5);
        assert!(Value::string("x").to_float().is_err());
    }

    // ── PartialEq ────────────────────────────────────────

    #[test]
    fn partial_eq_int_float_cross() {
        assert_eq!(Value::Int(3), Value::Float(3.0));
        assert_eq!(Value::Float(3.0), Value::Int(3));
        assert_ne!(Value::Int(3), Value::Float(3.5));
    }

    #[test]
    fn partial_eq_different_types_not_equal() {
        assert_ne!(Value::Int(1), Value::string("1"));
        assert_ne!(Value::Bool(true), Value::Int(1));
        assert_ne!(Value::Null, Value::Bool(false));
        assert_ne!(Value::list(vec![]), Value::Attrs(Rc::new(NixAttrs::new())));
    }

    // ── Display for all variants ─────────────────────────

    #[test]
    fn display_null() { assert_eq!(format!("{}", Value::Null), "null"); }

    #[test]
    fn display_bool() {
        assert_eq!(format!("{}", Value::Bool(true)), "true");
        assert_eq!(format!("{}", Value::Bool(false)), "false");
    }

    #[test]
    fn display_int() { assert_eq!(format!("{}", Value::Int(42)), "42"); }

    #[test]
    fn display_float() {
        let s = format!("{}", Value::Float(3.14));
        assert!(s.contains("3.14"));
    }

    #[test]
    fn display_string() {
        assert_eq!(format!("{}", Value::string("hi")), "\"hi\"");
    }

    #[test]
    fn display_string_with_escapes() {
        let v = Value::string("a\"b\\c");
        let s = format!("{v}");
        assert!(s.contains("\\\""));
        assert!(s.contains("\\\\"));
    }

    #[test]
    fn display_path() {
        assert_eq!(format!("{}", Value::Path(Box::new(SmolStr::from("/foo")))), "/foo");
    }

    #[test]
    fn display_list() {
        let v = Value::list(vec![Value::Int(1), Value::Int(2)]);
        assert_eq!(format!("{v}"), "[ 1 2 ]");
    }

    #[test]
    fn display_attrs() {
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(1));
        let v = Value::Attrs(Rc::new(attrs));
        assert_eq!(format!("{v}"), "{ x = 1; }");
    }

    #[test]
    fn display_lambda() {
        let root = rnix::Root::parse("x: x");
        let expr = root.tree().expr().unwrap();
        let lambda = match expr {
            rnix::ast::Expr::Lambda(l) => l,
            _ => panic!("expected lambda"),
        };
        let closure = Closure {
            param: lambda.param().unwrap(),
            body: lambda.body().unwrap(),
            env: Env::new(),
        };
        assert_eq!(format!("{}", Value::Lambda(Box::new(closure))), "<<lambda>>");
    }

    #[test]
    fn display_builtin() {
        let b = BuiltinFn {
            name: "add",
            func: Rc::new(|_| Ok(Value::Null)),
        };
        assert_eq!(format!("{}", Value::Builtin(Box::new(b))), "<<builtin add>>");
    }

    // ── NixAttrs ─────────────────────────────────────────

    #[test]
    fn nixattrs_update_merging() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        a.insert("y".to_string(), Value::Int(2));
        let mut b = NixAttrs::new();
        b.insert("y".to_string(), Value::Int(99));
        b.insert("z".to_string(), Value::Int(3));
        let merged = a.update(&b);
        assert_eq!(merged.get("x"), Some(&Value::Int(1)));
        assert_eq!(merged.get("y"), Some(&Value::Int(99)));
        assert_eq!(merged.get("z"), Some(&Value::Int(3)));
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn nixattrs_contains_key() {
        let mut a = NixAttrs::new();
        a.insert("foo".to_string(), Value::Null);
        assert!(a.contains_key("foo"));
        assert!(!a.contains_key("bar"));
    }

    // ── Env ──────────────────────────────────────────────

    #[test]
    fn env_lookup_through_parent_chain() {
        let mut root = Env::new();
        root.bind("a".to_string(), Value::Int(1));
        let mut child = root.child();
        child.bind("b".to_string(), Value::Int(2));
        let grandchild = child.child();
        // grandchild can see both a and b through parent chain
        assert_eq!(grandchild.lookup("a"), Some(Value::Int(1)));
        assert_eq!(grandchild.lookup("b"), Some(Value::Int(2)));
        assert_eq!(grandchild.lookup("c"), None);
    }

    #[test]
    fn env_with_scope_lookup() {
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(42));
        let env = Env::new().with_scope(Value::Attrs(Rc::new(attrs)));
        assert_eq!(env.lookup("x"), Some(Value::Int(42)));
        assert_eq!(env.lookup("y"), None);
    }

    #[test]
    fn env_local_shadows_with_scope() {
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(1));
        let mut env = Env::new().with_scope(Value::Attrs(Rc::new(attrs)));
        env.bind("x".to_string(), Value::Int(99));
        assert_eq!(env.lookup("x"), Some(Value::Int(99)));
    }

    // ── NixString context propagation ─────────────────────

    #[test]
    fn string_context_merge_combines_elements() {
        let mut ctx_a = StringContext::new();
        ctx_a.add_plain("/nix/store/aaa".to_string());
        let mut ctx_b = StringContext::new();
        ctx_b.add_plain("/nix/store/bbb".to_string());
        ctx_a.merge(&ctx_b);
        assert_eq!(ctx_a.len(), 2);
        assert!(ctx_a.elements().contains(&ContextElement::Plain(SmolStr::from("/nix/store/aaa"))));
        assert!(ctx_a.elements().contains(&ContextElement::Plain(SmolStr::from("/nix/store/bbb"))));
    }

    #[test]
    fn string_context_merge_deduplicates() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/same".to_string());
        ctx.add_plain("/nix/store/same".to_string());
        assert_eq!(ctx.len(), 1);
    }

    #[test]
    fn string_context_mixed_element_types() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/foo".to_string());
        ctx.add_output("/nix/store/bar.drv".to_string(), "out".to_string());
        ctx.add_drv_deep("/nix/store/baz.drv".to_string());
        assert_eq!(ctx.len(), 3);
        assert!(!ctx.is_empty());
    }

    #[test]
    fn string_context_new_is_empty() {
        let ctx = StringContext::new();
        assert!(ctx.is_empty());
        assert_eq!(ctx.len(), 0);
    }

    #[test]
    fn string_context_merge_zero_elements() {
        let mut ctx_a = StringContext::new();
        let ctx_b = StringContext::new();
        ctx_a.merge(&ctx_b);
        assert!(ctx_a.is_empty());
    }

    #[test]
    fn string_context_merge_one_element() {
        let mut ctx = StringContext::new();
        let mut other = StringContext::new();
        other.add_plain("/nix/store/only".to_string());
        ctx.merge(&other);
        assert_eq!(ctx.len(), 1);
        assert!(ctx.elements().contains(&ContextElement::Plain(SmolStr::from("/nix/store/only"))));
    }

    #[test]
    fn string_context_merge_two_elements() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/a".to_string());
        let mut other = StringContext::new();
        other.add_plain("/nix/store/b".to_string());
        ctx.merge(&other);
        assert_eq!(ctx.len(), 2);
    }

    #[test]
    fn string_context_merge_five_elements() {
        let mut ctx = StringContext::new();
        for i in 0..5 {
            ctx.add_plain(format!("/nix/store/path-{i}"));
        }
        assert_eq!(ctx.len(), 5);
        for i in 0..5 {
            assert!(ctx.elements().contains(&ContextElement::Plain(SmolStr::from(format!("/nix/store/path-{i}").as_str()))));
        }
    }

    #[test]
    fn string_context_insert_deduplicates() {
        let mut ctx = StringContext::new();
        ctx.insert(ContextElement::Plain(SmolStr::from("/nix/store/dup")));
        ctx.insert(ContextElement::Plain(SmolStr::from("/nix/store/dup")));
        ctx.insert(ContextElement::Output { drv: SmolStr::from("/nix/store/x.drv"), output: SmolStr::from("out") });
        ctx.insert(ContextElement::Output { drv: SmolStr::from("/nix/store/x.drv"), output: SmolStr::from("out") });
        assert_eq!(ctx.len(), 2);
    }

    #[test]
    fn nix_string_plain_has_no_context() {
        let s = NixString::plain("hello");
        assert!(!s.has_context());
        assert_eq!(s.as_str(), "hello");
    }

    #[test]
    fn nix_string_with_context_reports_context() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/xyz".to_string());
        let s = NixString::with_context("hello", ctx);
        assert!(s.has_context());
        assert_eq!(s.as_str(), "hello");
    }

    #[test]
    fn nix_string_display_shows_chars_only() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/abc".to_string());
        let s = NixString::with_context("visible", ctx);
        assert_eq!(format!("{s}"), "visible");
    }

    #[test]
    fn nix_string_struct_eq_includes_context() {
        let plain = NixString::plain("hello");
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/xxx".to_string());
        let with_ctx = NixString::with_context("hello", ctx);
        // NixString's derived PartialEq compares context too
        assert_ne!(plain, with_ctx);
    }

    #[test]
    fn value_string_eq_ignores_context() {
        let plain = Value::String(Rc::new(NixString::plain("hello")));
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/xxx".to_string());
        let with_ctx = Value::String(Rc::new(NixString::with_context("hello", ctx)));
        // Value::PartialEq only compares .chars, ignoring context
        assert_eq!(plain, with_ctx);
    }

    // ── Env deeply nested with-scopes ─────────────────────

    #[test]
    fn env_nested_with_inner_wins() {
        let mut outer_attrs = NixAttrs::new();
        outer_attrs.insert("x".to_string(), Value::Int(1));
        let outer = Env::new().with_scope(Value::Attrs(Rc::new(outer_attrs)));
        let mut inner_attrs = NixAttrs::new();
        inner_attrs.insert("x".to_string(), Value::Int(2));
        let inner = outer.child().with_scope(Value::Attrs(Rc::new(inner_attrs)));
        assert_eq!(inner.lookup("x"), Some(Value::Int(2)));
    }

    #[test]
    fn env_nested_with_fallback_to_outer() {
        let mut outer_attrs = NixAttrs::new();
        outer_attrs.insert("x".to_string(), Value::Int(1));
        let outer = Env::new().with_scope(Value::Attrs(Rc::new(outer_attrs)));
        let mut inner_attrs = NixAttrs::new();
        inner_attrs.insert("y".to_string(), Value::Int(2));
        let inner = outer.child().with_scope(Value::Attrs(Rc::new(inner_attrs)));
        assert_eq!(inner.lookup("x"), Some(Value::Int(1)));
        assert_eq!(inner.lookup("y"), Some(Value::Int(2)));
    }

    #[test]
    fn env_lexical_binding_wins_over_all_with_scopes() {
        let mut outer_attrs = NixAttrs::new();
        outer_attrs.insert("x".to_string(), Value::Int(1));
        let outer = Env::new().with_scope(Value::Attrs(Rc::new(outer_attrs)));
        let mut inner_attrs = NixAttrs::new();
        inner_attrs.insert("x".to_string(), Value::Int(2));
        let mut inner = outer.child().with_scope(Value::Attrs(Rc::new(inner_attrs)));
        inner.bind("x".to_string(), Value::Int(99));
        assert_eq!(inner.lookup("x"), Some(Value::Int(99)));
    }

    #[test]
    fn env_parent_lexical_wins_over_child_with_scope() {
        let mut root = Env::new();
        root.bind("x".to_string(), Value::Int(10));
        let mut child_attrs = NixAttrs::new();
        child_attrs.insert("x".to_string(), Value::Int(20));
        let child = root.child().with_scope(Value::Attrs(Rc::new(child_attrs)));
        assert_eq!(child.lookup("x"), Some(Value::Int(10)));
    }

    #[test]
    fn env_deeply_nested_with_scopes_three_levels() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        let env1 = Env::new().with_scope(Value::Attrs(Rc::new(a)));

        let mut b = NixAttrs::new();
        b.insert("y".to_string(), Value::Int(2));
        let env2 = env1.child().with_scope(Value::Attrs(Rc::new(b)));

        let mut c = NixAttrs::new();
        c.insert("z".to_string(), Value::Int(3));
        let env3 = env2.child().with_scope(Value::Attrs(Rc::new(c)));

        assert_eq!(env3.lookup("x"), Some(Value::Int(1)));
        assert_eq!(env3.lookup("y"), Some(Value::Int(2)));
        assert_eq!(env3.lookup("z"), Some(Value::Int(3)));
        assert_eq!(env3.lookup("w"), None);
    }

    #[test]
    fn env_with_scope_does_not_pollute_bindings() {
        // With-scope values should not appear in the flat binding map.
        // They should only be found via the with-scope lookup path.
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(42));
        let env = Env::new().with_scope(Value::Attrs(Rc::new(attrs)));
        // The binding map itself should not contain "x"
        assert!(env.0.bindings.get(&intern("x")).is_none());
        // But lookup should find it via with-scope
        assert_eq!(env.lookup("x"), Some(Value::Int(42)));
    }

    #[test]
    fn env_lexical_binding_not_in_with_scopes() {
        // Lexical bindings are in the flat binding map, not in with_scopes.
        let mut env = Env::new();
        env.bind("x".to_string(), Value::Int(42));
        // with_scopes should be empty
        assert!(env.0.with_scopes.is_empty());
        // But lookup finds it via the binding map
        assert_eq!(env.lookup("x"), Some(Value::Int(42)));
    }

    #[test]
    fn env_child_inherits_eval_file() {
        let mut env = Env::new();
        env.set_eval_file(Some(std::path::PathBuf::from("/foo/bar.nix")));
        let child = env.child();
        assert_eq!(child.eval_file().cloned(), Some(std::path::PathBuf::from("/foo/bar.nix")));
    }

    #[test]
    fn env_new_has_no_parent_no_with() {
        let env = Env::new();
        assert_eq!(env.lookup("anything"), None);
        assert!(env.eval_file().is_none());
    }

    // ── Thunk state machine ───────────────────────────────

    #[test]
    fn thunk_new_suspended_is_not_evaluated() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        assert!(!thunk.is_evaluated());
    }

    #[test]
    fn thunk_new_evaluated_is_evaluated() {
        let thunk = Thunk::new_evaluated(Value::Int(42));
        assert!(thunk.is_evaluated());
    }

    #[test]
    fn thunk_force_evaluates_suspended() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let result = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Value::Int(42));
        assert!(thunk.is_evaluated());
    }

    #[test]
    fn thunk_force_memoizes_result() {
        let root = rnix::Root::parse("1 + 2");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let r1 = thunk.force(&|e, env| crate::eval::eval_expr(e, env)).unwrap();
        let r2 = thunk.force(&|e, env| crate::eval::eval_expr(e, env)).unwrap();
        assert_eq!(r1, Value::Int(3));
        assert_eq!(r2, Value::Int(3));
    }

    #[test]
    fn thunk_force_already_evaluated_returns_value() {
        let thunk = Thunk::new_evaluated(Value::Bool(true));
        let result = thunk.force(&|_, _| panic!("should not be called"));
        assert_eq!(result.unwrap(), Value::Bool(true));
    }

    #[test]
    fn thunk_blackhole_detects_infinite_recursion() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());

        // Manually set to blackhole to simulate re-entrance
        // SAFETY: Test-only, single-threaded.
        *unsafe { &mut *thunk.0.repr.get() } = ThunkRepr::Blackhole;

        let result = thunk.force(&|_, _| Ok(Value::Null));
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("infinite recursion"));
    }

    #[test]
    fn thunk_update_env_replaces_suspended_env() {
        let root = rnix::Root::parse("x");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());

        let mut new_env = Env::new();
        new_env.bind("x".to_string(), Value::Int(99));
        thunk.update_env(&new_env);

        let result = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
        assert_eq!(result.unwrap(), Value::Int(99));
    }

    #[test]
    fn thunk_update_env_noop_when_evaluated() {
        let thunk = Thunk::new_evaluated(Value::Int(1));
        let mut new_env = Env::new();
        new_env.bind("x".to_string(), Value::Int(99));
        thunk.update_env(&new_env);
        assert_eq!(
            thunk.force(&|_, _| panic!("should not be called")).unwrap(),
            Value::Int(1),
        );
    }

    #[test]
    fn thunk_debug_suspended() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        assert_eq!(format!("{thunk:?}"), "<thunk>");
    }

    #[test]
    fn thunk_debug_evaluated() {
        let thunk = Thunk::new_evaluated(Value::Int(42));
        let dbg = format!("{thunk:?}");
        assert!(dbg.contains("42"));
    }

    #[test]
    fn thunk_error_restores_suspended_state() {
        let root = rnix::Root::parse("nonexistent_var");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());

        let result = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
        assert!(result.is_err());
        // After error, thunk should be restored to Suspended, not stuck as Blackhole
        assert!(!thunk.is_evaluated());
        let dbg = format!("{thunk:?}");
        assert_eq!(dbg, "<thunk>");
    }

    #[test]
    fn thunk_inherit_select_forces_and_selects() {
        let root = rnix::Root::parse(r#"{ x = 42; }"#);
        let expr = root.tree().expr().unwrap();
        let source = Thunk::new_suspended(expr, Env::new());
        let thunk = Thunk::new_inherit_select(source, "x".to_string());
        let result = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
        assert_eq!(result.unwrap(), Value::Int(42));
        assert!(thunk.is_evaluated());
    }

    #[test]
    fn thunk_inherit_select_missing_attr_errors() {
        let root = rnix::Root::parse(r#"{ x = 42; }"#);
        let expr = root.tree().expr().unwrap();
        let source = Thunk::new_suspended(expr, Env::new());
        let thunk = Thunk::new_inherit_select(source, "y".to_string());
        let result = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
        assert!(result.is_err());
        // Thunk should restore to InheritSelect, not be stuck as Blackhole
        assert!(!thunk.is_evaluated());
    }

    #[test]
    fn thunk_inherit_select_non_attrs_source_errors() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let source = Thunk::new_suspended(expr, Env::new());
        let thunk = Thunk::new_inherit_select(source, "x".to_string());
        let result = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("not a set"));
    }

    #[test]
    fn thunk_inherit_select_shares_source_thunk() {
        // Two InheritSelect thunks share the same source thunk.
        // Forcing one should evaluate the source; the second should
        // get a cache hit on the shared source thunk.
        let root = rnix::Root::parse(r#"{ a = 1; b = 2; }"#);
        let expr = root.tree().expr().unwrap();
        let source = Thunk::new_suspended(expr, Env::new());
        let thunk_a = Thunk::new_inherit_select(source.clone(), "a".to_string());
        let thunk_b = Thunk::new_inherit_select(source.clone(), "b".to_string());
        let result_a = thunk_a.force(&|e, env| crate::eval::eval_expr(e, env));
        assert_eq!(result_a.unwrap(), Value::Int(1));
        // Source thunk should now be evaluated (memoized).
        assert!(source.is_evaluated());
        // Second force should hit the source thunk's cache.
        let result_b = thunk_b.force(&|e, env| crate::eval::eval_expr(e, env));
        assert_eq!(result_b.unwrap(), Value::Int(2));
    }

    // ── NixAttrs additional tests ─────────────────────────

    #[test]
    fn nixattrs_empty_operations() {
        let a = NixAttrs::new();
        assert!(a.is_empty());
        assert_eq!(a.len(), 0);
        assert_eq!(a.get("x"), None);
        assert!(!a.contains_key("x"));
        assert_eq!(a.keys().count(), 0);
        assert_eq!(a.iter().count(), 0);
    }

    #[test]
    fn nixattrs_update_with_empty() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        let b = NixAttrs::new();
        let merged = a.update(&b);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged.get("x"), Some(&Value::Int(1)));
    }

    #[test]
    fn nixattrs_update_empty_with_nonempty() {
        let a = NixAttrs::new();
        let mut b = NixAttrs::new();
        b.insert("x".to_string(), Value::Int(1));
        let merged = a.update(&b);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged.get("x"), Some(&Value::Int(1)));
    }

    #[test]
    fn nixattrs_keys_sorted_order() {
        let mut a = NixAttrs::new();
        a.insert("c".to_string(), Value::Int(3));
        a.insert("a".to_string(), Value::Int(1));
        a.insert("b".to_string(), Value::Int(2));
        let keys: Vec<String> = a.keys().collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    // ── Value convenience methods ─────────────────────────

    #[test]
    fn value_to_str_forces_thunks() {
        let root = rnix::Root::parse(r#""hello""#);
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert_eq!(val.to_str().unwrap(), "hello");
    }

    #[test]
    fn value_to_nix_string_forces_thunks() {
        let root = rnix::Root::parse(r#""world""#);
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        let ns = val.to_nix_string().unwrap();
        assert_eq!(ns.as_str(), "world");
        assert!(!ns.has_context());
    }

    #[test]
    fn value_to_attrs_forces_thunks() {
        let root = rnix::Root::parse("{ x = 1; }");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        let attrs = val.to_attrs().unwrap();
        assert_eq!(attrs.len(), 1);
    }

    #[test]
    fn value_to_list_forces_thunks() {
        let root = rnix::Root::parse("[1 2 3]");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        let list = val.to_list().unwrap();
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn value_to_float_on_thunk() {
        let root = rnix::Root::parse("3.14");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        let f = val.to_float().unwrap();
        assert!((f - 3.14).abs() < f64::EPSILON);
    }

    #[test]
    fn value_as_bool_on_thunk() {
        let root = rnix::Root::parse("true");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert!(val.as_bool().unwrap());
    }

    #[test]
    fn value_as_int_on_thunk() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert_eq!(val.as_int().unwrap(), 42);
    }

    #[test]
    fn value_string_constructor() {
        let v = Value::string("test");
        assert_eq!(v, Value::String(Rc::new(NixString::plain("test"))));
    }

    #[test]
    fn value_partial_eq_null_null() {
        assert_eq!(Value::Null, Value::Null);
    }

    #[test]
    fn value_partial_eq_lists_deep() {
        let a = Value::list(vec![Value::Int(1), Value::list(vec![Value::Int(2)])]);
        let b = Value::list(vec![Value::Int(1), Value::list(vec![Value::Int(2)])]);
        assert_eq!(a, b);
    }

    #[test]
    fn value_partial_eq_attrs_deep() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        let mut b = NixAttrs::new();
        b.insert("x".to_string(), Value::Int(1));
        assert_eq!(Value::Attrs(Rc::new(a)), Value::Attrs(Rc::new(b)));
    }

    // ── EvalError variants & convenience constructors ────

    #[test]
    fn eval_error_type_error_constructor() {
        let e = EvalError::type_error("oops");
        assert!(matches!(e, EvalError::TypeError(ref s) if s == "oops"));
    }

    #[test]
    fn eval_error_type_mismatch_constructor() {
        let e = EvalError::type_mismatch("int", "string");
        match e {
            EvalError::TypeMismatch { expected, got } => {
                assert_eq!(expected, "int");
                assert_eq!(got, "string");
            }
            _ => panic!("expected TypeMismatch"),
        }
    }

    #[test]
    fn eval_error_is_throw_yes_no() {
        assert!(EvalError::Throw("oops".into()).is_throw());
        assert!(!EvalError::TypeError("oops".into()).is_throw());
        assert!(!EvalError::AssertionFailed(String::new()).is_throw());
    }

    #[test]
    fn eval_error_is_infinite_recursion_yes_no() {
        assert!(EvalError::InfiniteRecursion("loop".into()).is_infinite_recursion());
        assert!(!EvalError::DivisionByZero.is_infinite_recursion());
        assert!(!EvalError::Throw("x".into()).is_infinite_recursion());
    }

    #[test]
    fn eval_error_display_undefined_var() {
        let s = format!("{}", EvalError::UndefinedVar("foo".into()));
        assert!(s.contains("undefined variable"));
        assert!(s.contains("foo"));
    }

    #[test]
    fn eval_error_display_type_error() {
        let s = format!("{}", EvalError::TypeError("bad".into()));
        assert!(s.contains("type error"));
        assert!(s.contains("bad"));
    }

    #[test]
    fn eval_error_display_attr_not_found() {
        let s = format!("{}", EvalError::AttrNotFound("x".into()));
        assert!(s.contains("attribute not found"));
        assert!(s.contains("x"));
    }

    #[test]
    fn eval_error_display_type_mismatch() {
        let s = format!(
            "{}",
            EvalError::TypeMismatch { expected: "int", got: "string" }
        );
        assert!(s.contains("expected int"));
        assert!(s.contains("got string"));
    }

    #[test]
    fn eval_error_display_assertion_failed() {
        let s = format!("{}", EvalError::AssertionFailed(String::new()));
        assert!(s.contains("assertion"));
    }

    #[test]
    fn eval_error_display_division_by_zero() {
        let s = format!("{}", EvalError::DivisionByZero);
        assert!(s.contains("division by zero"));
    }

    #[test]
    fn eval_error_display_infinite_recursion() {
        let s = format!("{}", EvalError::InfiniteRecursion("loop".into()));
        assert!(s.contains("infinite recursion"));
        assert!(s.contains("loop"));
    }

    #[test]
    fn eval_error_display_io_error() {
        let s = format!(
            "{}",
            EvalError::IoError {
                context: "ctx".into(),
                message: "no such file".into(),
            }
        );
        assert!(s.contains("I/O"));
        assert!(s.contains("ctx"));
        assert!(s.contains("no such file"));
    }

    #[test]
    fn eval_error_display_throw() {
        let s = format!("{}", EvalError::Throw("boom".into()));
        assert_eq!(s, "boom");
    }

    #[test]
    fn eval_error_display_not_implemented() {
        let s = format!("{}", EvalError::NotImplemented("frob".into()));
        assert!(s.contains("not yet implemented"));
        assert!(s.contains("frob"));
    }

    #[test]
    fn eval_error_display_parse_error() {
        let s = format!("{}", EvalError::ParseError("syntax".into()));
        assert!(s.contains("parse error"));
        assert!(s.contains("syntax"));
    }

    #[test]
    fn eval_error_display_recursion_limit() {
        let s = format!(
            "{}",
            EvalError::RecursionLimit("max depth exceeded".into())
        );
        assert!(s.contains("recursion limit"));
        assert!(s.contains("max depth exceeded"));
    }

    #[test]
    fn eval_error_partial_eq_same_variant() {
        assert_eq!(
            EvalError::UndefinedVar("x".into()),
            EvalError::UndefinedVar("x".into()),
        );
        assert_ne!(
            EvalError::UndefinedVar("x".into()),
            EvalError::UndefinedVar("y".into()),
        );
        assert_ne!(
            EvalError::UndefinedVar("x".into()),
            EvalError::AttrNotFound("x".into()),
        );
    }

    // ── ContextElement display ───────────────────────────

    #[test]
    fn context_element_display_plain() {
        let e = ContextElement::Plain("/nix/store/xyz".into());
        assert_eq!(format!("{e}"), "/nix/store/xyz");
    }

    #[test]
    fn context_element_display_output() {
        let e = ContextElement::Output {
            drv: "/nix/store/abc.drv".into(),
            output: "out".into(),
        };
        assert_eq!(format!("{e}"), "/nix/store/abc.drv!out");
    }

    #[test]
    fn context_element_display_drv_deep() {
        let e = ContextElement::DrvDeep("/nix/store/abc.drv".into());
        assert_eq!(format!("{e}"), "=/nix/store/abc.drv");
    }

    // ── StringContext additional API ─────────────────────

    #[test]
    fn string_context_iter_yields_all() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/aaa");
        ctx.add_plain("/nix/store/bbb");
        let count = ctx.iter().count();
        assert_eq!(count, 2);
    }

    #[test]
    fn string_context_len_matches_set_size() {
        let mut ctx = StringContext::new();
        assert_eq!(ctx.len(), 0);
        ctx.add_plain("/nix/store/x");
        assert_eq!(ctx.len(), 1);
        ctx.add_output("/nix/store/y.drv", "out");
        assert_eq!(ctx.len(), 2);
    }

    #[test]
    fn string_context_insert_raw_element() {
        let mut ctx = StringContext::new();
        ctx.insert(ContextElement::Plain("/nix/store/foo".into()));
        assert_eq!(ctx.len(), 1);
    }

    #[test]
    fn string_context_default_is_empty() {
        let ctx = StringContext::default();
        assert!(ctx.is_empty());
    }

    // ── NixString additional traits ──────────────────────

    #[test]
    fn nix_string_as_ref_str() {
        let s = NixString::plain("hello");
        let r: &str = s.as_ref();
        assert_eq!(r, "hello");
    }

    #[test]
    fn nix_string_deref_to_str_methods() {
        let s = NixString::plain("Hello World");
        assert_eq!(s.len(), 11);
        assert!(s.starts_with("Hello"));
        // Calling &str method via Deref proves Deref impl is wired up.
        assert_eq!(s.to_uppercase(), "HELLO WORLD");
    }

    // ── NixAttrs additional API ──────────────────────────

    #[test]
    fn nixattrs_remove_returns_value() {
        let mut a = NixAttrs::new();
        a.insert("x".into(), Value::Int(1));
        let removed = a.remove("x");
        assert_eq!(removed, Some(Value::Int(1)));
        assert!(!a.contains_key("x"));
        assert_eq!(a.remove("y"), None);
    }

    #[test]
    fn nixattrs_values_iter() {
        let mut a = NixAttrs::new();
        a.insert("a".into(), Value::Int(1));
        a.insert("b".into(), Value::Int(2));
        let mut vs: Vec<&Value> = a.values().collect();
        vs.sort_by_key(|v| match v {
            Value::Int(n) => *n,
            _ => 0,
        });
        assert_eq!(vs, vec![&Value::Int(1), &Value::Int(2)]);
    }

    #[test]
    fn nixattrs_iter_returns_sorted_pairs() {
        let mut a = NixAttrs::new();
        a.insert("zeta".into(), Value::Int(3));
        a.insert("alpha".into(), Value::Int(1));
        a.insert("mu".into(), Value::Int(2));
        let pairs: Vec<(String, &Value)> = a.iter().collect();
        assert_eq!(pairs[0].0, "alpha");
        assert_eq!(pairs[1].0, "mu");
        assert_eq!(pairs[2].0, "zeta");
    }

    #[test]
    fn nixattrs_from_iterator() {
        let pairs = vec![
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Int(2)),
        ];
        let attrs: NixAttrs = pairs.into_iter().collect();
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
        assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
    }

    #[test]
    fn nixattrs_into_iterator_yields_owned() {
        let mut a = NixAttrs::new();
        a.insert("x".into(), Value::Int(42));
        let pairs: Vec<(String, Value)> = a.into_iter().collect();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "x");
        assert_eq!(pairs[0].1, Value::Int(42));
    }

    #[test]
    fn nixattrs_default_is_empty() {
        let a = NixAttrs::default();
        assert!(a.is_empty());
    }

    // ── Value::From conversions ──────────────────────────

    #[test]
    fn value_from_bool() {
        assert_eq!(Value::from(true), Value::Bool(true));
        assert_eq!(Value::from(false), Value::Bool(false));
    }

    #[test]
    fn value_from_i64() {
        assert_eq!(Value::from(42_i64), Value::Int(42));
        assert_eq!(Value::from(-1_i64), Value::Int(-1));
    }

    #[test]
    fn value_from_f64() {
        assert_eq!(Value::from(2.5_f64), Value::Float(2.5));
    }

    #[test]
    fn value_from_nix_string() {
        let v: Value = NixString::plain("hi").into();
        assert_eq!(v, Value::string("hi"));
    }

    #[test]
    fn value_from_nix_attrs() {
        let mut a = NixAttrs::new();
        a.insert("x".into(), Value::Int(1));
        let v: Value = a.into();
        match v {
            Value::Attrs(_) => {}
            _ => panic!("expected Attrs"),
        }
    }

    #[test]
    fn value_from_vec() {
        let v: Value = vec![Value::Int(1), Value::Int(2)].into();
        assert_eq!(v, Value::list(vec![Value::Int(1), Value::Int(2)]));
    }

    #[test]
    fn value_default_is_null() {
        let v: Value = Value::default();
        assert_eq!(v, Value::Null);
    }

    // ── From<&serde_json::Value> ─────────────────────────

    #[test]
    fn value_from_json_null() {
        let v = Value::from(&serde_json::Value::Null);
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn value_from_json_bool() {
        let v = Value::from(&serde_json::Value::Bool(true));
        assert_eq!(v, Value::Bool(true));
    }

    #[test]
    fn value_from_json_int() {
        let v = Value::from(&serde_json::json!(42));
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn value_from_json_float() {
        let v = Value::from(&serde_json::json!(3.14));
        match v {
            Value::Float(f) => assert!((f - 3.14).abs() < f64::EPSILON),
            _ => panic!("expected Float"),
        }
    }

    #[test]
    fn value_from_json_string() {
        let v = Value::from(&serde_json::Value::String("hi".into()));
        assert_eq!(v, Value::string("hi"));
    }

    #[test]
    fn value_from_json_array() {
        let v = Value::from(&serde_json::json!([1, true, "x"]));
        match v {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Value::Int(1));
                assert_eq!(items[1], Value::Bool(true));
                assert_eq!(items[2], Value::string("x"));
            }
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn value_from_json_object() {
        let v = Value::from(&serde_json::json!({"a": 1, "b": "x"}));
        match v {
            Value::Attrs(attrs) => {
                assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
                assert_eq!(attrs.get("b"), Some(&Value::string("x")));
            }
            _ => panic!("expected Attrs"),
        }
    }

    #[test]
    fn value_from_json_nested() {
        let v = Value::from(&serde_json::json!({"outer": {"inner": [1, 2]}}));
        let json_back = v.to_json();
        assert_eq!(json_back, serde_json::json!({"outer": {"inner": [1, 2]}}));
    }

    // ── From<&toml::Value> ──────────────────────────────

    #[test]
    fn value_from_toml_string() {
        let t = toml::Value::String("hi".into());
        assert_eq!(Value::from(&t), Value::string("hi"));
    }

    #[test]
    fn value_from_toml_int() {
        let t = toml::Value::Integer(42);
        assert_eq!(Value::from(&t), Value::Int(42));
    }

    #[test]
    fn value_from_toml_float() {
        let t = toml::Value::Float(3.14);
        match Value::from(&t) {
            Value::Float(f) => assert!((f - 3.14).abs() < f64::EPSILON),
            _ => panic!("expected Float"),
        }
    }

    #[test]
    fn value_from_toml_bool() {
        let t = toml::Value::Boolean(true);
        assert_eq!(Value::from(&t), Value::Bool(true));
    }

    #[test]
    fn value_from_toml_array() {
        let t = toml::Value::Array(vec![
            toml::Value::Integer(1),
            toml::Value::Integer(2),
        ]);
        assert_eq!(
            Value::from(&t),
            Value::list(vec![Value::Int(1), Value::Int(2)]),
        );
    }

    #[test]
    fn value_from_toml_table() {
        let mut tbl = toml::map::Map::new();
        tbl.insert("k".into(), toml::Value::Integer(7));
        let t = toml::Value::Table(tbl);
        match Value::from(&t) {
            Value::Attrs(attrs) => {
                assert_eq!(attrs.get("k"), Some(&Value::Int(7)));
            }
            _ => panic!("expected Attrs"),
        }
    }

    #[test]
    fn value_from_toml_datetime_becomes_string() {
        // toml::Value::Datetime serializes via Display.
        let dt: toml::value::Datetime = "2024-01-01T00:00:00Z".parse().unwrap();
        let t = toml::Value::Datetime(dt);
        match Value::from(&t) {
            Value::String(_) => {}
            other => panic!("expected String, got {other:?}"),
        }
    }

    // ── Value::coerce_to_path ────────────────────────────

    #[test]
    fn coerce_to_path_from_path() {
        let v = Value::Path(Box::new("/foo".into()));
        assert_eq!(v.coerce_to_path("ctx").unwrap(), "/foo");
    }

    #[test]
    fn coerce_to_path_from_string() {
        let v = Value::string("/bar");
        assert_eq!(v.coerce_to_path("ctx").unwrap(), "/bar");
    }

    #[test]
    fn coerce_to_path_errors_on_int() {
        let v = Value::Int(1);
        let e = v.coerce_to_path("readFile").unwrap_err();
        match e {
            EvalError::TypeError(ref msg) => {
                assert!(msg.contains("readFile"));
                assert!(msg.contains("path or string"));
                assert!(msg.contains("int"));
            }
            _ => panic!("expected TypeError"),
        }
    }

    #[test]
    fn coerce_to_path_errors_on_null() {
        let v = Value::Null;
        assert!(v.coerce_to_path("ctx").is_err());
    }

    #[test]
    fn coerce_to_path_attrs_with_outpath() {
        let mut attrs = NixAttrs::new();
        attrs.insert("outPath".to_string(), Value::string("/nix/store/test"));
        let val = Value::Attrs(Rc::new(attrs));
        assert_eq!(val.coerce_to_path("test").unwrap(), "/nix/store/test");
    }

    #[test]
    fn coerce_to_path_attrs_without_outpath_fails() {
        let attrs = NixAttrs::new();
        let val = Value::Attrs(Rc::new(attrs));
        assert!(val.coerce_to_path("test").is_err());
    }

    // ── Value::coerce_to_string ─────────────────────────

    #[test]
    fn coerce_to_string_string() {
        let v = Value::string("hello");
        let (s, _ctx) = v.coerce_to_string().unwrap();
        assert_eq!(s, "hello");
    }

    #[test]
    fn coerce_to_string_path() {
        let v = Value::Path(Box::new("/foo".into()));
        let (s, ctx) = v.coerce_to_string().unwrap();
        assert_eq!(s, "/foo");
        assert!(!ctx.is_empty()); // should add a Plain context element
    }

    #[test]
    fn coerce_to_string_int() {
        let v = Value::Int(42);
        let (s, _ctx) = v.coerce_to_string().unwrap();
        assert_eq!(s, "42");
    }

    #[test]
    fn coerce_to_string_float() {
        let v = Value::Float(3.14);
        let (s, _ctx) = v.coerce_to_string().unwrap();
        assert_eq!(s, "3.14");
    }

    #[test]
    fn coerce_to_string_bool_true() {
        let (s, _ctx) = Value::Bool(true).coerce_to_string().unwrap();
        assert_eq!(s, "1");
    }

    #[test]
    fn coerce_to_string_bool_false() {
        let (s, _ctx) = Value::Bool(false).coerce_to_string().unwrap();
        assert_eq!(s, "");
    }

    #[test]
    fn coerce_to_string_null() {
        let (s, _ctx) = Value::Null.coerce_to_string().unwrap();
        assert_eq!(s, "");
    }

    #[test]
    fn coerce_to_string_attrs_with_outpath() {
        let mut attrs = NixAttrs::new();
        attrs.insert("outPath".to_string(), Value::string("/nix/store/abc"));
        let val = Value::Attrs(Rc::new(attrs));
        let (s, _ctx) = val.coerce_to_string().unwrap();
        assert_eq!(s, "/nix/store/abc");
    }

    #[test]
    fn coerce_to_string_attrs_without_outpath_or_tostring_fails() {
        let attrs = NixAttrs::new();
        let val = Value::Attrs(Rc::new(attrs));
        assert!(val.coerce_to_string().is_err());
    }

    #[test]
    fn coerce_to_string_lambda_fails() {
        let root = rnix::Root::parse("x: x");
        let expr = root.tree().expr().unwrap();
        let closure = Closure {
            param: match expr {
                rnix::ast::Expr::Lambda(ref l) => l.param().unwrap(),
                _ => panic!("expected lambda"),
            },
            body: match expr {
                rnix::ast::Expr::Lambda(ref l) => l.body().unwrap(),
                _ => panic!("expected lambda"),
            },
            env: Env::new(),
        };
        let val = Value::Lambda(Box::new(closure));
        assert!(val.coerce_to_string().is_err());
    }

    // ── BuiltinFn debug ──────────────────────────────────

    #[test]
    fn builtin_fn_debug_includes_name() {
        let b = BuiltinFn {
            name: "myFunc",
            func: Rc::new(|_| Ok(Value::Null)),
        };
        let s = format!("{b:?}");
        assert!(s.contains("myFunc"));
        assert!(s.contains("builtin"));
    }

    // ── Thunk additional tests ───────────────────────────

    #[test]
    fn thunk_force_chains_through_inner_thunks() {
        // Build a thunk whose evaluator yields another thunk.
        let inner_root = rnix::Root::parse("99");
        let inner_expr = inner_root.tree().expr().unwrap();
        let inner_thunk = Thunk::new_suspended(inner_expr, Env::new());
        let outer = Thunk::new_evaluated(Value::Thunk(inner_thunk));
        let result = outer.force(&|e, env| crate::eval::eval_expr(e, env));
        // Already-evaluated outer returns the inner thunk; the chain is
        // collapsed by the higher-level force_value, not by force() itself
        // when starting from Evaluated. So we just check we got a Thunk
        // back unchanged.
        match result.unwrap() {
            Value::Thunk(_) | Value::Int(99) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn thunk_inherit_select_debug_format() {
        let root = rnix::Root::parse("{ x = 1; }");
        let expr = root.tree().expr().unwrap();
        let source = Thunk::new_suspended(expr, Env::new());
        let thunk = Thunk::new_inherit_select(source, "x");
        let s = format!("{thunk:?}");
        assert!(s.contains("inherit-select"));
        assert!(s.contains("x"));
    }

    #[test]
    fn thunk_blackhole_debug_format() {
        let root = rnix::Root::parse("1");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        // SAFETY: Test-only, single-threaded.
        *unsafe { &mut *thunk.0.repr.get() } = ThunkRepr::Blackhole;
        assert_eq!(format!("{thunk:?}"), "<blackhole>");
    }

    // ── Value display for thunks ─────────────────────────

    #[test]
    fn value_display_thunk_evaluates() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert_eq!(format!("{val}"), "42");
    }

    #[test]
    fn value_to_json_thunk_forces() {
        let root = rnix::Root::parse(r#""world""#);
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert_eq!(val.to_json(), serde_json::Value::String("world".into()));
    }

    #[test]
    fn value_type_name_thunk_forces() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert_eq!(val.type_name(), "int");
    }

    // ── as_string / as_nix_string thunk error ────────────

    #[test]
    fn as_string_errors_on_thunk() {
        let root = rnix::Root::parse(r#""x""#);
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        let err = val.as_string().unwrap_err();
        match err {
            EvalError::TypeError(msg) => assert!(msg.contains("thunk")),
            _ => panic!("expected TypeError"),
        }
    }

    #[test]
    fn as_nix_string_errors_on_thunk() {
        let root = rnix::Root::parse(r#""x""#);
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert!(val.as_nix_string().is_err());
    }

    #[test]
    fn as_attrs_errors_on_thunk() {
        let root = rnix::Root::parse("{}");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert!(val.as_attrs().is_err());
    }

    #[test]
    fn as_list_errors_on_thunk() {
        let root = rnix::Root::parse("[]");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert!(val.as_list().is_err());
    }

    // ── as_nix_string OK on string ───────────────────────

    #[test]
    fn as_nix_string_ok_on_string() {
        let v = Value::string("hi");
        let ns = v.as_nix_string().unwrap();
        assert_eq!(ns.as_str(), "hi");
    }

    #[test]
    fn as_nix_string_errors_on_int() {
        let v = Value::Int(1);
        match v.as_nix_string() {
            Err(EvalError::TypeMismatch { expected, got }) => {
                assert_eq!(expected, "string");
                assert_eq!(got, "int");
            }
            _ => panic!("expected TypeMismatch"),
        }
    }

    // ════════════════════════════════════════════════════════════
    // 1. OnceCell Thunk Cache
    // ════════════════════════════════════════════════════════════

    #[test]
    fn oncecell_cache_populated_after_force() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        // Before forcing, cache should be empty.
        assert!(thunk.0.cache.get().is_none());
        let _ = thunk.force(&|e, env| crate::eval::eval_expr(e, env)).unwrap();
        // After forcing, cache should be populated.
        assert!(thunk.0.cache.get().is_some());
    }

    #[test]
    fn oncecell_cache_matches_force_result() {
        let root = rnix::Root::parse("1 + 2");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let forced = thunk.force(&|e, env| crate::eval::eval_expr(e, env)).unwrap();
        let cached = thunk.0.cache.get().unwrap();
        assert_eq!(**cached, forced);
    }

    #[test]
    fn oncecell_new_evaluated_prepopulates_cache() {
        let thunk = Thunk::new_evaluated(Value::Int(77));
        // Cache should be set immediately.
        let cached = thunk.0.cache.get().expect("cache should be pre-populated");
        assert_eq!(**cached, Value::Int(77));
    }

    #[test]
    fn oncecell_is_evaluated_uses_cache() {
        let thunk = Thunk::new_evaluated(Value::Bool(false));
        // is_evaluated() checks the OnceCell cache.
        assert!(thunk.is_evaluated());
        assert!(thunk.0.cache.get().is_some());
    }

    #[test]
    fn oncecell_already_evaluated_returns_cached_without_repr() {
        // Create a thunk already evaluated. Force should return
        // the cached value without touching repr (the evaluator
        // closure should never be called).
        let thunk = Thunk::new_evaluated(Value::Int(55));
        let result = thunk.force(&|_, _| panic!("evaluator should not be called"));
        assert_eq!(result.unwrap(), Value::Int(55));
    }

    // ════════════════════════════════════════════════════════════
    // 2. WithScope Memoization
    // ════════════════════════════════════════════════════════════

    #[test]
    fn with_scope_created_with_empty_cache() {
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(1));
        let env = Env::new().with_scope(Value::Attrs(Rc::new(attrs)));
        // The cached field should be None initially.
        let scope = &env.0.with_scopes[0];
        assert!(scope.cached.borrow().is_none());
    }

    #[test]
    fn with_scope_first_lookup_populates_cache() {
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(42));
        let env = Env::new().with_scope(Value::Attrs(Rc::new(attrs)));
        // Before lookup, cache is empty.
        assert!(env.0.with_scopes[0].cached.borrow().is_none());
        // Lookup forces and caches.
        let _ = env.lookup("x");
        assert!(env.0.with_scopes[0].cached.borrow().is_some());
    }

    #[test]
    fn with_scope_second_lookup_uses_cache() {
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(10));
        let env = Env::new().with_scope(Value::Attrs(Rc::new(attrs)));
        // First lookup populates cache.
        assert_eq!(env.lookup("x"), Some(Value::Int(10)));
        assert!(env.0.with_scopes[0].cached.borrow().is_some());
        // Second lookup should still work (reads from cache).
        assert_eq!(env.lookup("x"), Some(Value::Int(10)));
    }

    #[test]
    fn with_scope_child_shares_cache_via_rc() {
        let mut attrs = NixAttrs::new();
        attrs.insert("shared".to_string(), Value::Int(7));
        let parent = Env::new().with_scope(Value::Attrs(Rc::new(attrs)));
        let child = parent.child();
        // Force via parent lookup.
        let _ = parent.lookup("shared");
        // Child's with-scope cache should share the same Rc, so
        // it should also show cached.
        assert!(child.0.with_scopes[0].cached.borrow().is_some());
    }

    #[test]
    fn with_scope_innermost_checked_first() {
        let mut outer = NixAttrs::new();
        outer.insert("x".to_string(), Value::Int(1));
        outer.insert("y".to_string(), Value::Int(100));
        let mut inner = NixAttrs::new();
        inner.insert("x".to_string(), Value::Int(2));
        let env = Env::new()
            .with_scope(Value::Attrs(Rc::new(outer)))
            .with_scope(Value::Attrs(Rc::new(inner)));
        // Innermost scope has x=2, should win.
        assert_eq!(env.lookup("x"), Some(Value::Int(2)));
        // y only in outer, should fallback.
        assert_eq!(env.lookup("y"), Some(Value::Int(100)));
    }

    // ════════════════════════════════════════════════════════════
    // 3. FxHashMap for NixAttrs
    // ════════════════════════════════════════════════════════════

    #[test]
    fn fxhashmap_nixattrs_new_creates_empty() {
        let a = NixAttrs::new();
        assert!(a.is_empty());
        assert_eq!(a.len(), 0);
        // Internal map is a FxHashMap (im_rc::HashMap with FxBuildHasher).
        assert!(a.inner().is_empty());
    }

    #[test]
    fn fxhashmap_insert_get_roundtrip_with_symbol_keys() {
        let mut a = NixAttrs::new();
        a.insert("mykey".to_string(), Value::Int(42));
        assert_eq!(a.get("mykey"), Some(&Value::Int(42)));
    }

    #[test]
    fn fxhashmap_contains_key_with_interned_keys() {
        let mut a = NixAttrs::new();
        a.insert("alpha".to_string(), Value::Int(1));
        let sym = intern("alpha");
        assert!(a.inner().contains_key(&sym));
        let missing_sym = intern("beta");
        assert!(!a.inner().contains_key(&missing_sym));
    }

    #[test]
    fn fxhashmap_remove_returns_value() {
        let mut a = NixAttrs::new();
        a.insert("key".to_string(), Value::Int(99));
        let removed = a.remove("key");
        assert_eq!(removed, Some(Value::Int(99)));
        assert!(a.is_empty());
    }

    #[test]
    fn fxhashmap_keys_returns_sorted_strings() {
        let mut a = NixAttrs::new();
        a.insert("zulu".to_string(), Value::Int(1));
        a.insert("alpha".to_string(), Value::Int(2));
        a.insert("mike".to_string(), Value::Int(3));
        let keys: Vec<String> = a.keys().collect();
        assert_eq!(keys, vec!["alpha", "mike", "zulu"]);
    }

    #[test]
    fn fxhashmap_iter_returns_sorted_string_value_pairs() {
        let mut a = NixAttrs::new();
        a.insert("b".to_string(), Value::Int(2));
        a.insert("a".to_string(), Value::Int(1));
        let pairs: Vec<(String, &Value)> = a.iter().collect();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "a");
        assert_eq!(*pairs[0].1, Value::Int(1));
        assert_eq!(pairs[1].0, "b");
        assert_eq!(*pairs[1].1, Value::Int(2));
    }

    #[test]
    fn fxhashmap_update_merges_correctly() {
        let mut left = NixAttrs::new();
        left.insert("a".to_string(), Value::Int(1));
        left.insert("b".to_string(), Value::Int(2));
        let mut right = NixAttrs::new();
        right.insert("b".to_string(), Value::Int(20));
        right.insert("c".to_string(), Value::Int(3));
        let merged = left.update(&right);
        assert_eq!(merged.get("a"), Some(&Value::Int(1)));
        assert_eq!(merged.get("b"), Some(&Value::Int(20))); // right overrides
        assert_eq!(merged.get("c"), Some(&Value::Int(3)));
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn fxhashmap_from_iterator_collects_with_interning() {
        let pairs = vec![
            ("x".to_string(), Value::Int(10)),
            ("y".to_string(), Value::Int(20)),
            ("z".to_string(), Value::Int(30)),
        ];
        let attrs: NixAttrs = pairs.into_iter().collect();
        assert_eq!(attrs.len(), 3);
        assert_eq!(attrs.get("x"), Some(&Value::Int(10)));
        assert_eq!(attrs.get("y"), Some(&Value::Int(20)));
        assert_eq!(attrs.get("z"), Some(&Value::Int(30)));
        // Verify internal storage uses Symbol keys.
        let sym_x = intern("x");
        assert!(attrs.inner().contains_key(&sym_x));
    }

    // ════════════════════════════════════════════════════════════
    // 4. SmallVec StringContext
    // ════════════════════════════════════════════════════════════

    #[test]
    fn smallvec_context_empty() {
        let ctx = StringContext::new();
        assert!(ctx.is_empty());
        assert_eq!(ctx.len(), 0);
        assert_eq!(ctx.elements().len(), 0);
    }

    #[test]
    fn smallvec_context_single_element_inline() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/single");
        assert_eq!(ctx.len(), 1);
        // SmallVec<[ContextElement; 2]> stores up to 2 inline.
        assert!(!ctx.is_empty());
    }

    #[test]
    fn smallvec_context_two_elements_still_inline() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/one");
        ctx.add_output("/nix/store/two.drv", "out");
        assert_eq!(ctx.len(), 2);
    }

    #[test]
    fn smallvec_context_three_plus_spills_to_heap() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/a");
        ctx.add_plain("/nix/store/b");
        ctx.add_drv_deep("/nix/store/c.drv");
        assert_eq!(ctx.len(), 3);
        // Verify all elements are accessible.
        assert!(ctx.elements().contains(&ContextElement::Plain(SmolStr::from("/nix/store/a"))));
        assert!(ctx.elements().contains(&ContextElement::Plain(SmolStr::from("/nix/store/b"))));
        assert!(ctx.elements().contains(&ContextElement::DrvDeep(SmolStr::from("/nix/store/c.drv"))));
    }

    #[test]
    fn smallvec_context_merge_deduplicates() {
        let mut ctx1 = StringContext::new();
        ctx1.add_plain("/nix/store/dup");
        ctx1.add_output("/nix/store/x.drv", "out");
        let mut ctx2 = StringContext::new();
        ctx2.add_plain("/nix/store/dup");      // duplicate
        ctx2.add_plain("/nix/store/unique");    // new
        ctx1.merge(&ctx2);
        assert_eq!(ctx1.len(), 3); // dup not duplicated
    }

    #[test]
    fn smallvec_context_add_plain_output_drv_deep() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/plain");
        assert_eq!(ctx.len(), 1);
        assert!(ctx.elements().contains(&ContextElement::Plain(SmolStr::from("/nix/store/plain"))));

        ctx.add_output("/nix/store/out.drv", "lib");
        assert_eq!(ctx.len(), 2);
        assert!(ctx.elements().contains(&ContextElement::Output {
            drv: SmolStr::from("/nix/store/out.drv"),
            output: SmolStr::from("lib"),
        }));

        ctx.add_drv_deep("/nix/store/deep.drv");
        assert_eq!(ctx.len(), 3);
        assert!(ctx.elements().contains(&ContextElement::DrvDeep(SmolStr::from("/nix/store/deep.drv"))));
    }

    // ════════════════════════════════════════════════════════════
    // 5. Rc<Vec<Value>> for List
    // ════════════════════════════════════════════════════════════

    #[test]
    fn rc_list_constructor_wraps_in_rc() {
        let v = Value::list(vec![Value::Int(1), Value::Int(2)]);
        match &v {
            Value::List(rc) => {
                assert_eq!(rc.len(), 2);
                assert_eq!(Rc::strong_count(rc), 1);
            }
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn rc_list_clone_is_refcount_bump() {
        let v = Value::list(vec![Value::Int(10)]);
        let rc1 = match &v {
            Value::List(rc) => rc.clone(),
            _ => panic!("expected List"),
        };
        let v2 = v.clone();
        let rc2 = match &v2 {
            Value::List(rc) => rc.clone(),
            _ => panic!("expected List"),
        };
        // Both point to the same allocation.
        assert!(Rc::ptr_eq(&rc1, &rc2));
        // Strong count should be 3: rc1, rc2, and the one inside v or v2.
        // Actually: v has one, v2 has one, rc1 has one, rc2 has one = 4.
        assert!(Rc::strong_count(&rc1) >= 2);
    }

    #[test]
    fn rc_list_as_list_returns_slice() {
        let v = Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let slice = v.as_list().unwrap();
        assert_eq!(slice.len(), 3);
        assert_eq!(slice[0], Value::Int(1));
        assert_eq!(slice[1], Value::Int(2));
        assert_eq!(slice[2], Value::Int(3));
    }

    #[test]
    fn rc_list_from_vec_wraps_in_rc() {
        let items = vec![Value::Bool(true), Value::Bool(false)];
        let v: Value = items.into();
        match &v {
            Value::List(rc) => {
                assert_eq!(rc.len(), 2);
                assert_eq!(Rc::strong_count(rc), 1);
            }
            _ => panic!("expected List"),
        }
    }

    // ════════════════════════════════════════════════════════════
    // 6. String Interning
    // ════════════════════════════════════════════════════════════

    #[test]
    fn intern_same_string_returns_same_symbol() {
        let s1 = intern("hello_intern_test");
        let s2 = intern("hello_intern_test");
        assert_eq!(s1, s2);
    }

    #[test]
    fn intern_different_strings_returns_different_symbols() {
        let s1 = intern("unique_str_a_9182");
        let s2 = intern("unique_str_b_9182");
        assert_ne!(s1, s2);
    }

    #[test]
    fn resolve_roundtrips_correctly() {
        let sym = intern("roundtrip_test_str");
        let resolved = resolve(sym);
        assert_eq!(resolved, "roundtrip_test_str");
    }

    #[test]
    fn intern_cached_same_offset_returns_cached_symbol() {
        let sid = next_source_id();
        let sym1 = intern_cached("cached_ident_aa", sid, 100);
        let sym2 = intern_cached("cached_ident_aa", sid, 100);
        assert_eq!(sym1, sym2);
    }

    #[test]
    fn intern_cached_different_offset_same_string_returns_same_symbol() {
        // Even with different offsets, the same string should intern
        // to the same Symbol (interning dedup at the interner level).
        let sid = next_source_id();
        let sym1 = intern_cached("dedup_test_str_77", sid, 200);
        let sym2 = intern_cached("dedup_test_str_77", sid, 300);
        // The symbols should be equal because the interner deduplicates.
        assert_eq!(sym1, sym2);
    }

    #[test]
    fn clear_ident_cache_clears() {
        let sid = next_source_id();
        let _sym = intern_cached("to_be_cleared_99", sid, 500);
        clear_ident_cache();
        // After clearing, the cache is empty, but interning the same
        // string again should still return the same Symbol (the interner
        // itself is not cleared, just the offset cache).
        let sym2 = intern_cached("to_be_cleared_99", sid, 500);
        let resolved = resolve(sym2);
        assert_eq!(resolved, "to_be_cleared_99");
    }

    #[test]
    fn next_source_id_increments_monotonically() {
        let id1 = next_source_id();
        let id2 = next_source_id();
        let id3 = next_source_id();
        assert_eq!(id2, id1 + 1);
        assert_eq!(id3, id2 + 1);
    }

    // ════════════════════════════════════════════════════════════
    // 7. Env Operations
    // ════════════════════════════════════════════════════════════

    #[test]
    fn env_new_creates_empty_bindings() {
        let env = Env::new();
        assert!(env.0.bindings.is_empty());
        assert!(env.0.with_scopes.is_empty());
        assert!(env.eval_file().is_none());
    }

    #[test]
    fn env_bind_lookup_roundtrip() {
        let mut env = Env::new();
        env.bind("foo".to_string(), Value::Int(42));
        assert_eq!(env.lookup("foo"), Some(Value::Int(42)));
        assert_eq!(env.lookup("bar"), None);
    }

    #[test]
    fn env_child_inherits_parent_bindings_flattened() {
        let mut parent = Env::new();
        parent.bind("a".to_string(), Value::Int(1));
        parent.bind("b".to_string(), Value::Int(2));
        let child = parent.child();
        // Child sees parent's bindings.
        assert_eq!(child.lookup("a"), Some(Value::Int(1)));
        assert_eq!(child.lookup("b"), Some(Value::Int(2)));
        // Verify bindings are in child's own map (flattened).
        let sym_a = intern("a");
        assert!(child.0.bindings.contains_key(&sym_a));
    }

    #[test]
    fn env_child_inherits_with_scopes() {
        let mut attrs = NixAttrs::new();
        attrs.insert("ws".to_string(), Value::Int(10));
        let parent = Env::new().with_scope(Value::Attrs(Rc::new(attrs)));
        let child = parent.child();
        // Child should have the same with_scopes as parent.
        assert_eq!(child.0.with_scopes.len(), parent.0.with_scopes.len());
        assert_eq!(child.lookup("ws"), Some(Value::Int(10)));
    }

    #[test]
    fn env_lookup_sym_fast_path_matches_lookup() {
        let mut env = Env::new();
        env.bind("target".to_string(), Value::Int(88));
        let sym = intern("target");
        let via_lookup = env.lookup("target");
        let via_sym = env.lookup_sym(sym);
        assert_eq!(via_lookup, via_sym);
        assert_eq!(via_sym, Some(Value::Int(88)));
    }

    #[test]
    fn env_lookup_sym_with_scope_fallback() {
        let mut attrs = NixAttrs::new();
        attrs.insert("sym_ws".to_string(), Value::Int(33));
        let env = Env::new().with_scope(Value::Attrs(Rc::new(attrs)));
        let sym = intern("sym_ws");
        assert_eq!(env.lookup_sym(sym), Some(Value::Int(33)));
    }

    #[test]
    fn env_with_scope_ordering_multiple_innermost_wins() {
        let mut a1 = NixAttrs::new();
        a1.insert("x".to_string(), Value::Int(1));
        let mut a2 = NixAttrs::new();
        a2.insert("x".to_string(), Value::Int(2));
        let mut a3 = NixAttrs::new();
        a3.insert("x".to_string(), Value::Int(3));
        let env = Env::new()
            .with_scope(Value::Attrs(Rc::new(a1)))
            .with_scope(Value::Attrs(Rc::new(a2)))
            .with_scope(Value::Attrs(Rc::new(a3)));
        // Innermost (a3) should win.
        assert_eq!(env.lookup("x"), Some(Value::Int(3)));
    }

    #[test]
    fn env_lookup_sym_not_found_returns_none() {
        let env = Env::new();
        let sym = intern("nonexistent_sym_99");
        assert_eq!(env.lookup_sym(sym), None);
    }

    #[test]
    fn env_lookup_sym_lexical_wins_over_with_scope() {
        let mut attrs = NixAttrs::new();
        attrs.insert("priority".to_string(), Value::Int(1));
        let mut env = Env::new().with_scope(Value::Attrs(Rc::new(attrs)));
        env.bind("priority".to_string(), Value::Int(99));
        let sym = intern("priority");
        assert_eq!(env.lookup_sym(sym), Some(Value::Int(99)));
    }
}
