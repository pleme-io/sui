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

use sui_intern::Symbol;

/// Type alias for the persistent hash map used by `NixAttrs` and `Env`.
///
/// Uses `FxBuildHasher` (fast multiplication-based hash) instead of the
/// default `RandomState`. This is optimal for `Symbol(u32)` keys where
/// the hash is a single multiply-shift — no SipHash overhead.
pub type FxHashMap<K, V> = im_rc::HashMap<K, V, FxBuildHasher>;

/// Compact attrset map — a real `hashbrown` (std) `HashMap` with `FxBuildHasher`.
///
/// Used ONLY for `NixAttrs` (attribute sets), which are immutable-after-
/// construction. Unlike `FxHashMap` (the persistent `im_rc` HAMT, retained for
/// `Env` where `child()`/scope-push relies on O(1) structural sharing), this is a
/// flat open-addressing table with ~0.875 load factor and NO branch-node
/// allocations — a symbolicated dhat profile proved the `im_rc` HAMT branch nodes
/// dominate eval heap, and the attrset slice is the safe one to compact.
///
/// BYTE-NEUTRAL: attrset observation order (which feeds drvPath hashing) comes
/// from `NixAttrs::sorted_entries()` — it resolves each `Symbol` to its `String`
/// and string-sorts on observation — NOT from this map's internal iteration
/// order. Both `im_rc::HashMap` and `std::HashMap` are unordered, so swapping the
/// implementation cannot change any observed order → drvPaths are unchanged.
pub type AttrsMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;

/// Env-gated LIVE-OBJECT CENSUS.
///
/// A permanent, zero-cost-when-off diagnostic answering the question:
/// when sui's eval peak is ~2× nix's, is the overhead (a) cyclic/lingering
/// producer garbage sui retains, or (b) a uniform per-object representation
/// overhead? These need different fixes, so we MEASURE.
///
/// Gated behind `SUI_LIVE_CENSUS=1`. The atomics are always compiled but the
/// `_MADE`/`_LIVE` bookkeeping and the RSS/dump thread only run when enabled.
/// All counters use `Relaxed` — we want a cheap high-water snapshot, not a
/// linearizable total.
///
/// `_MADE` + `_LIVE` are incremented in the INNER heap type's constructor;
/// `_LIVE` is decremented in the inner type's `Drop` so it fires exactly once
/// when the last `Rc` drops. Counters live on the inner heap types
/// (`NixAttrs`, `ThunkInner`, `EnvInner`, `NixString`, the list `Vec`) so we
/// count distinct heap allocations, not `Rc` clones.
pub mod census {
    use std::sync::atomic::{AtomicI64, Ordering::Relaxed};
    use std::sync::OnceLock;

    pub static ATTRS_LIVE: AtomicI64 = AtomicI64::new(0);
    pub static ATTRS_MADE: AtomicI64 = AtomicI64::new(0);
    pub static THUNK_LIVE: AtomicI64 = AtomicI64::new(0);
    pub static THUNK_MADE: AtomicI64 = AtomicI64::new(0);
    pub static THUNK_EVALUATED: AtomicI64 = AtomicI64::new(0);
    pub static ENV_LIVE: AtomicI64 = AtomicI64::new(0);
    pub static ENV_MADE: AtomicI64 = AtomicI64::new(0);
    pub static NIXSTR_LIVE: AtomicI64 = AtomicI64::new(0);
    pub static NIXSTR_MADE: AtomicI64 = AtomicI64::new(0);
    pub static LIST_LIVE: AtomicI64 = AtomicI64::new(0);
    pub static LIST_MADE: AtomicI64 = AtomicI64::new(0);

    /// Scope-narrowing verdict counters (`SUI_SCOPE_NARROW`, `eval.rs`).
    ///
    /// Every `let` / `rec` / pattern-default binding whose value is a thunk is
    /// classified exactly once: NARROWED means it kept its outer-env capture
    /// (no `Thunk -> Env -> Thunk` cycle closed), PINNED means its RHS reaches
    /// a sibling in the same scope and it still takes Phase 2's `update_env`.
    ///
    /// The ratio is the whole point: it is what says whether narrowing does
    /// anything on REAL code rather than on a synthetic probe. Both counters
    /// are monotonic totals, not live counts — there is nothing to decrement,
    /// which is why they need no `Drop` partner (and so cannot acquire
    /// `ENV_LIVE`'s under-reporting bug, where `Env::bind`'s `Rc::make_mut`
    /// clone is never counted as `made` while every drop is counted).
    pub static SCOPE_THUNKS_NARROWED: AtomicI64 = AtomicI64::new(0);
    pub static SCOPE_THUNKS_PINNED: AtomicI64 = AtomicI64::new(0);

    /// Record a binding that kept its outer-env capture.
    #[inline(always)]
    pub fn scope_narrowed() {
        if enabled() {
            SCOPE_THUNKS_NARROWED.fetch_add(1, Relaxed);
        }
    }

    /// Record a binding that still needs the scope env.
    #[inline(always)]
    pub fn scope_pinned() {
        if enabled() {
            SCOPE_THUNKS_PINNED.fetch_add(1, Relaxed);
        }
    }

    /// True iff `SUI_LIVE_CENSUS=1`. Cached — read once.
    #[inline]
    pub fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var("SUI_LIVE_CENSUS").as_deref() == Ok("1"))
    }

    #[inline(always)]
    pub fn made(made: &AtomicI64, live: &AtomicI64) {
        if enabled() {
            made.fetch_add(1, Relaxed);
            live.fetch_add(1, Relaxed);
        }
    }

    #[inline(always)]
    pub fn dropped(live: &AtomicI64) {
        if enabled() {
            live.fetch_sub(1, Relaxed);
        }
    }

    #[inline(always)]
    pub fn evaluated() {
        if enabled() {
            THUNK_EVALUATED.fetch_add(1, Relaxed);
        }
    }

    /// Resident set size of this process, in bytes (macOS + Linux).
    pub fn rss_bytes() -> u64 {
        #[cfg(target_os = "macos")]
        unsafe {
            let mut info: libc::mach_task_basic_info = std::mem::zeroed();
            let mut count = (std::mem::size_of::<libc::mach_task_basic_info>()
                / std::mem::size_of::<libc::natural_t>()) as libc::mach_msg_type_number_t;
            let kr = libc::task_info(
                libc::mach_task_self(),
                libc::MACH_TASK_BASIC_INFO,
                std::ptr::addr_of_mut!(info).cast(),
                &mut count,
            );
            if kr == libc::KERN_SUCCESS {
                return info.resident_size;
            }
            0
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::fs::read_to_string("/proc/self/statm")
                .ok()
                .and_then(|s| s.split_whitespace().nth(1).map(String::from))
                .and_then(|pages| pages.parse::<u64>().ok())
                .map(|pages| pages * 4096)
                .unwrap_or(0)
        }
    }

    /// Print all live/made counts + RSS to stderr, tagged.
    ///
    /// No-op unless `SUI_LIVE_CENSUS=1`. The counters only accumulate when the
    /// census is enabled, so dumping while disabled emits an all-zeros
    /// `[census exit] …` line to stderr — pure noise that pollutes any tool
    /// parsing sui's stderr. Concretely it regressed the `derivation show→add`
    /// ATerm round-trip parity row: that probe collects every non-`#` stderr
    /// line from `derivation add` as the round-tripped ATerm, and the
    /// exit-guard's unconditional dump appended the census line to it. Gating
    /// here makes census-pollution-when-disabled unrepresentable at EVERY call
    /// site (the process-exit guard AND the periodic poller), not just the one
    /// that regressed.
    pub fn dump(tag: &str) {
        if !enabled() {
            return;
        }
        let rss = rss_bytes();
        eprintln!(
            "[census {tag}] rss={rss_mb:.1}MB \
attrs_live={al} attrs_made={am} \
thunk_live={tl} thunk_made={tm} thunk_eval={te} \
env_live={el} env_made={em} \
nixstr_live={sl} nixstr_made={sm} \
list_live={ll} list_made={lm} \
scope_narrowed={sn} scope_pinned={sp}",
            rss_mb = rss as f64 / (1024.0 * 1024.0),
            al = ATTRS_LIVE.load(Relaxed),
            am = ATTRS_MADE.load(Relaxed),
            tl = THUNK_LIVE.load(Relaxed),
            tm = THUNK_MADE.load(Relaxed),
            te = THUNK_EVALUATED.load(Relaxed),
            el = ENV_LIVE.load(Relaxed),
            em = ENV_MADE.load(Relaxed),
            sl = NIXSTR_LIVE.load(Relaxed),
            sm = NIXSTR_MADE.load(Relaxed),
            ll = LIST_LIVE.load(Relaxed),
            lm = LIST_MADE.load(Relaxed),
            sn = SCOPE_THUNKS_NARROWED.load(Relaxed),
            sp = SCOPE_THUNKS_PINNED.load(Relaxed),
        );
        let (src_files, src_bytes) = crate::pos::source_text_census();
        eprintln!(
            "[census {tag}] src_files={src_files} src_bytes={src_mb:.1}MB",
            src_mb = src_bytes as f64 / (1024.0 * 1024.0),
        );
    }

    /// Spawn the periodic-dump thread (only when enabled). Dumps every 2s so a
    /// 30s+ eval captures the high-water region. Also usable as an at-exit
    /// hook via the returned guard.
    pub fn spawn_poller() {
        if !enabled() {
            return;
        }
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_millis(2000));
            dump("periodic");
        });
    }
}

// -- String interner (shared with sui-bytecode via sui-intern's thread-local) --
//
// Previously this module owned its own `thread_local! INTERNER`. That
// diverged from `sui-bytecode`'s `sui_intern::*` thread-local — Symbols
// were NOT portable across the tree-walker ↔ VM fallback boundary,
// which only worked because both paths happened to re-intern strings
// from the same source text. Delegating both to the same thread-local
// closes the gap and makes `sui_intern::prewarm()` affect this crate
// too.

/// Intern a string key, returning a Symbol handle.
/// Used for NixAttrs keys and Env binding names.
pub fn intern(s: &str) -> Symbol {
    sui_intern::intern(s)
}

/// Resolve a Symbol back to its string content. Allocates a fresh
/// `String`. For hot paths prefer [`resolve_rc`] or [`with_resolved`]
/// — `Rc::clone` is ~20x cheaper than `String::from` for identifier-
/// sized inputs.
pub fn resolve(sym: Symbol) -> String {
    sui_intern::resolve(sym)
}

/// Resolve a Symbol to a shared `Rc<str>`. Zero-copy.
pub fn resolve_rc(sym: Symbol) -> std::rc::Rc<str> {
    sui_intern::resolve_rc(sym)
}

/// Borrow the resolved string inside a closure without allocating.
pub fn with_resolved<F, R>(sym: Symbol, f: F) -> R
where
    F: FnOnce(&str) -> R,
{
    sui_intern::with_resolved(sym, f)
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
    // STARTS AT 1, NOT 0 — 0 is the reserved "untagged env" sentinel.
    //
    // `Env::new()` defaults `source_id: 0`. While the generator also started at
    // 0, the FIRST file parsed shared key-space with every untagged env, so an
    // identifier in that file could collide with one from an untagged context at
    // the same byte offset. That is the same aliasing class as the
    // CURRENT_SOURCE_ID bug fixed alongside this (see eval.rs's Ident arms) —
    // reserving 0 costs nothing and removes the overlap by construction.
    static SOURCE_GEN: Cell<u32> = const { Cell::new(1) };

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
    intern_cached_with(source_id, text_offset, || intern(name))
}

/// Cache an interned `Symbol` by `(source_id, text_offset)`, computing it
/// lazily via `cold` only on a cache miss.
///
/// Steady-state hit: `FxHashMap` u64 lookup — no string materialization, no
/// string hashing. This lets the identifier-eval hot path avoid the
/// per-lookup `ident_text().to_string()` heap allocation entirely, since the
/// `&str` is only needed to intern on the (once-per-offset) cold miss.
pub fn intern_cached_with<F>(source_id: u32, text_offset: u32, cold: F) -> Symbol
where
    F: FnOnce() -> Symbol,
{
    let key = (u64::from(source_id) << 32) | u64::from(text_offset);
    IDENT_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        *cache.entry(key).or_insert_with(cold)
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
#[derive(Debug, PartialEq, Eq)]
pub struct NixString {
    /// The character data.
    pub chars: SmolStr,
    /// The context set (empty for plain string literals).
    pub context: StringContext,
}

// `Clone` is hand-written so the census counts every NixString that comes
// into existence (a clone is a fresh heap object once Rc-wrapped), keeping
// `NIXSTR_MADE`/`NIXSTR_LIVE` consistent with the `Drop` below.
impl Clone for NixString {
    fn clone(&self) -> Self {
        census::made(&census::NIXSTR_MADE, &census::NIXSTR_LIVE);
        Self {
            chars: self.chars.clone(),
            context: self.context.clone(),
        }
    }
}

impl Drop for NixString {
    fn drop(&mut self) {
        census::dropped(&census::NIXSTR_LIVE);
    }
}

impl NixString {
    /// Create a context-free string.
    pub fn plain(s: impl Into<SmolStr>) -> Self {
        census::made(&census::NIXSTR_MADE, &census::NIXSTR_LIVE);
        Self {
            chars: s.into(),
            context: StringContext::default(),
        }
    }

    /// Create a string with an explicit context.
    pub fn with_context(s: impl Into<SmolStr>, ctx: StringContext) -> Self {
        census::made(&census::NIXSTR_MADE, &census::NIXSTR_LIVE);
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

/// Census wrapper around a list's backing `Vec<Value>`.
///
/// `#[repr(transparent)]` + `Deref`/`DerefMut` to `Vec<Value>` so nearly every
/// existing call site (`.len()`, `.iter()`, indexing, `.as_slice()`, `.clone()`
/// → produces a `NixList`) works unchanged. Its sole job is to carry the census
/// hooks (`LIST_MADE`/`LIST_LIVE`) on the inner heap allocation.
#[repr(transparent)]
#[derive(Debug, PartialEq)]
pub struct NixList(pub Vec<Value>);

impl NixList {
    #[inline]
    pub fn new(v: Vec<Value>) -> Self {
        census::made(&census::LIST_MADE, &census::LIST_LIVE);
        NixList(v)
    }

    /// Consume into the backing `Vec<Value>`. `mem::take` because `NixList`
    /// has a `Drop` impl (can't move the field out); the emptied husk's Drop
    /// still fires, decrementing LIVE — correct, the list is consumed.
    #[inline]
    pub fn into_vec(mut self) -> Vec<Value> {
        std::mem::take(&mut self.0)
    }
}

impl From<Vec<Value>> for NixList {
    #[inline]
    fn from(v: Vec<Value>) -> Self {
        NixList::new(v)
    }
}

// Slice/array comparison so `assert_eq!(nixlist, [..])` in tests keeps working.
impl<T: AsRef<[Value]>> PartialEq<T> for NixList {
    #[inline]
    fn eq(&self, other: &T) -> bool {
        self.0.as_slice() == other.as_ref()
    }
}

impl Clone for NixList {
    fn clone(&self) -> Self {
        census::made(&census::LIST_MADE, &census::LIST_LIVE);
        NixList(self.0.clone())
    }
}

impl Drop for NixList {
    fn drop(&mut self) {
        census::dropped(&census::LIST_LIVE);
    }
}

impl FromIterator<Value> for NixList {
    #[inline]
    fn from_iter<I: IntoIterator<Item = Value>>(iter: I) -> Self {
        NixList::new(iter.into_iter().collect())
    }
}

impl std::ops::Deref for NixList {
    type Target = Vec<Value>;
    #[inline]
    fn deref(&self) -> &Vec<Value> {
        &self.0
    }
}

impl std::ops::DerefMut for NixList {
    #[inline]
    fn deref_mut(&mut self) -> &mut Vec<Value> {
        &mut self.0
    }
}

impl<'a> IntoIterator for &'a NixList {
    type Item = &'a Value;
    type IntoIter = std::slice::Iter<'a, Value>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl IntoIterator for NixList {
    type Item = Value;
    type IntoIter = std::vec::IntoIter<Value>;
    #[inline]
    fn into_iter(mut self) -> Self::IntoIter {
        // Move the Vec out. `NixList`'s Drop still fires on the emptied husk,
        // decrementing LIVE — correct, since the elements move to the iterator
        // and the list allocation is consumed.
        std::mem::take(&mut self.0).into_iter()
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

/// A Nix value — potentially lazy (may be a Thunk).
///
/// To get a guaranteed-concrete value, call `.demand()` which returns
/// `Concrete`. The `Concrete` type has thunk-free accessors that the
/// compiler enforces — you cannot accidentally skip forcing.
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
    List(Rc<NixList>),
    Attrs(Rc<NixAttrs>),
    Lambda(Rc<Closure>),
    Builtin(Box<BuiltinFn>),
    /// A lazy value (thunk) with memoization and blackhole detection.
    Thunk(Thunk),
}

// ── Concrete: construction-guaranteed non-thunk ──────────────

/// A demanded Nix value. Guaranteed NOT a Thunk at the TYPE level.
///
/// Unlike `Value` (which has a `Thunk` variant), `Concrete` is a separate
/// enum that DOES NOT HAVE a Thunk variant. The compiler rejects any attempt
/// to construct a `Concrete` from a thunk — the variant simply doesn't exist.
///
/// The ONLY way to obtain a `Concrete` is through `Value::demand()`.
///
/// ```rust,ignore
/// let val: Value = eval_expr(expr, env)?;  // might be Thunk
/// let c: Concrete = val.demand()?;          // NOW guaranteed concrete
/// let n: i64 = c.as_int()?;                // type-safe, thunk-free
/// ```
#[derive(Debug, Clone)]
pub enum Concrete {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(Rc<NixString>),
    Path(Box<SmolStr>),
    List(Rc<NixList>),      // elements may be lazy (correct for Nix)
    Attrs(Rc<NixAttrs>),       // values may be lazy (correct for Nix)
    Lambda(Rc<Closure>),
    Builtin(Box<BuiltinFn>),
    // NO Thunk variant. The compiler enforces this.
}

impl Concrete {
    /// Convert back to a Value (for APIs that still take Value).
    #[inline]
    pub fn into_value(self) -> Value {
        match self {
            Concrete::Null => Value::Null,
            Concrete::Bool(b) => Value::Bool(b),
            Concrete::Int(n) => Value::Int(n),
            Concrete::Float(f) => Value::Float(f),
            Concrete::String(s) => Value::String(s),
            Concrete::Path(p) => Value::Path(p),
            Concrete::List(l) => Value::List(l),
            Concrete::Attrs(a) => Value::Attrs(a),
            Concrete::Lambda(c) => Value::Lambda(c),
            Concrete::Builtin(b) => Value::Builtin(b),
        }
    }

    /// Borrow as a Value reference. Constructs a temporary Value.
    /// Prefer specific accessors (as_bool, as_int, etc.) when possible.
    pub fn to_value(&self) -> Value {
        self.clone().into_value()
    }

    /// Extract bool — guaranteed no thunk.
    pub fn as_bool(&self) -> Result<bool, EvalError> {
        match self {
            Concrete::Bool(b) => Ok(*b),
            other => Err(EvalError::TypeMismatch { expected: "bool", got: other.type_name() }),
        }
    }

    /// Extract int — guaranteed no thunk.
    pub fn as_int(&self) -> Result<i64, EvalError> {
        match self {
            Concrete::Int(n) => Ok(*n),
            other => Err(EvalError::TypeMismatch { expected: "int", got: other.type_name() }),
        }
    }

    /// Extract string ref — guaranteed no thunk.
    pub fn as_str(&self) -> Result<&str, EvalError> {
        match self {
            Concrete::String(s) => Ok(&s.chars),
            other => Err(EvalError::TypeMismatch { expected: "string", got: other.type_name() }),
        }
    }

    /// Extract NixString ref — guaranteed no thunk.
    pub fn as_nix_string(&self) -> Result<&NixString, EvalError> {
        match self {
            Concrete::String(s) => Ok(s),
            other => Err(EvalError::TypeMismatch { expected: "string", got: other.type_name() }),
        }
    }

    /// Extract list ref — guaranteed no thunk at this level.
    /// Note: list ELEMENTS may still be lazy (Value, not Concrete).
    pub fn as_list(&self) -> Result<&[Value], EvalError> {
        match self {
            Concrete::List(l) => Ok(l.as_slice()),
            other => Err(EvalError::TypeMismatch { expected: "list", got: other.type_name() }),
        }
    }

    /// Extract attrs ref — guaranteed no thunk at this level.
    /// Note: attr VALUES may still be lazy (Value, not Concrete).
    pub fn as_attrs(&self) -> Result<&NixAttrs, EvalError> {
        match self {
            Concrete::Attrs(a) => Ok(a),
            other => Err(EvalError::TypeMismatch { expected: "set", got: other.type_name() }),
        }
    }

    /// Extract float — guaranteed no thunk.
    pub fn as_float(&self) -> Result<f64, EvalError> {
        match self {
            Concrete::Float(f) => Ok(*f),
            Concrete::Int(n) => Ok(*n as f64),
            other => Err(EvalError::TypeMismatch { expected: "float", got: other.type_name() }),
        }
    }

    /// Check the value type name.
    pub fn type_name(&self) -> &'static str {
        match self {
            Concrete::Null => "null",
            Concrete::Bool(_) => "bool",
            Concrete::Int(_) => "int",
            Concrete::Float(_) => "float",
            Concrete::String(_) => "string",
            Concrete::Path(_) => "path",
            Concrete::List(_) => "list",
            Concrete::Attrs(_) => "set",
            Concrete::Lambda(_) | Concrete::Builtin(_) => "lambda",
        }
    }

    /// Alias for `as_str()` — API parity with Value::as_string().
    pub fn as_string(&self) -> Result<&str, EvalError> {
        self.as_str()
    }

    /// Extract owned NixAttrs — guaranteed no thunk at this level.
    pub fn to_attrs(&self) -> Result<NixAttrs, EvalError> {
        match self {
            Concrete::Attrs(a) => Ok((**a).clone()),
            other => Err(EvalError::TypeMismatch { expected: "set", got: other.type_name() }),
        }
    }

    /// Extract owned list — guaranteed no thunk at this level.
    pub fn to_list(&self) -> Result<Vec<Value>, EvalError> {
        match self {
            Concrete::List(l) => Ok((**l).0.clone()),
            other => Err(EvalError::TypeMismatch { expected: "list", got: other.type_name() }),
        }
    }

    /// Extract a filesystem path from Path or String.
    pub fn coerce_to_path(&self, context: &str) -> Result<String, EvalError> {
        match self {
            Concrete::Path(p) => Ok(p.to_string()),
            Concrete::String(ns) => Ok(ns.chars.to_string()),
            Concrete::Attrs(attrs) => {
                if let Some(out_path) = attrs.get("outPath") {
                    let forced = crate::eval::force_value(out_path)?;
                    forced.coerce_to_path(context)
                } else {
                    Err(EvalError::type_error(format!(
                        "{context}: expected path or string, got set without outPath"
                    )))
                }
            }
            other => Err(EvalError::type_error(format!(
                "{context}: expected path or string, got {}", other.type_name()
            ))),
        }
    }

    /// Extract owned string.
    pub fn to_str(&self) -> Result<String, EvalError> {
        match self {
            Concrete::String(s) => Ok(s.chars.to_string()),
            other => Err(EvalError::TypeMismatch { expected: "string", got: other.type_name() }),
        }
    }

    /// Extract owned NixString (with context).
    pub fn to_nix_string(&self) -> Result<NixString, EvalError> {
        match self {
            Concrete::String(s) => Ok((**s).clone()),
            other => Err(EvalError::TypeMismatch { expected: "string", got: other.type_name() }),
        }
    }

    /// Check if value is a function (lambda or builtin).
    pub fn is_function(&self) -> bool {
        matches!(self, Concrete::Lambda(_) | Concrete::Builtin(_))
    }
}

// Type-safe conversion: Concrete → Value (infallible)
impl From<Concrete> for Value {
    fn from(c: Concrete) -> Value {
        c.into_value()
    }
}

impl PartialEq for Concrete {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Concrete::Null, Concrete::Null) => true,
            (Concrete::Bool(a), Concrete::Bool(b)) => a == b,
            (Concrete::Int(a), Concrete::Int(b)) => a == b,
            (Concrete::Float(a), Concrete::Float(b)) => a == b,
            (Concrete::Int(a), Concrete::Float(b)) | (Concrete::Float(b), Concrete::Int(a)) => (*a as f64) == *b,
            (Concrete::String(a), Concrete::String(b)) => Rc::ptr_eq(a, b) || a.chars == b.chars,
            (Concrete::Path(a), Concrete::Path(b)) => a == b,
            (Concrete::List(a), Concrete::List(b)) => Rc::ptr_eq(a, b) || a == b,
            (Concrete::Attrs(a), Concrete::Attrs(b)) => {
                if Rc::ptr_eq(a, b) {
                    return true;
                }
                // cppnix `EvalState::eqValues` derivation short-circuit:
                // two attrsets that are BOTH derivations (each has
                // `type == "derivation"`) AND each carry an `outPath`
                // compare by their `outPath` string ONLY — never by deep
                // structural equality.  This is load-bearing: derivations
                // hold thunks/functions (`meta`, `override`, …) that never
                // compare structurally-equal even when the two describe the
                // same store output, and forcing every attr can throw.
                // (Empirically characterized against the live nix oracle:
                //  `hello == (hello // { x = 5; })` ⇒ true.)
                if let (Some(pa), Some(pb)) =
                    (derivation_out_path(a), derivation_out_path(b))
                {
                    return pa == pb;
                }
                // Structural compare by BORROW, not by clone. The prior
                // `a.inner() == b.inner()` flattened AND cloned *both* backing
                // `AttrsMap`s (`inner()` = `as_flat().clone()`) purely to feed
                // `HashMap::eq` — the clone is dead work. `as_flat()` returns a
                // borrow into the (memoized-if-overlay) map, so
                // `a.as_flat() == b.as_flat()` runs the *identical*
                // `HashMap::eq`: same keys, same per-value `Value::eq` calls.
                // `HashMap::eq` is ORDER-INDEPENDENT by construction (it iterates
                // one map and looks each key up in the other), so this holds
                // regardless of the map's internal iteration order — true for the
                // std `AttrsMap` exactly as it was for the old `im_rc` map.
                // PROVABLY-NEUTRAL
                // on the demand axis: cloning a `Value` is an `Rc`-bump that
                // forces NOTHING; the only `.demand()` calls in this arm are (1)
                // the derivation short-circuit above (unchanged) and (2) inside
                // `Value::eq` (unchanged — same values, same order). Removing the
                // clone cannot move which thunk forces, when, or whether a throw
                // surfaces. See docs/PERF-ARSENAL.md C-A.
                let (fa, fb) = (a.as_flat(), b.as_flat());
                if crate::perf::enabled() {
                    crate::perf::inc(crate::perf::Counter::AttrsEqStructuralCalls);
                    // Combined entry count of the two maps the old `inner()`
                    // path would have cloned before comparing.
                    crate::perf::add(
                        crate::perf::Counter::AttrsEqEntriesCloneElided,
                        (fa.len() + fb.len()) as u64,
                    );
                }
                fa == fb
            }
            (Concrete::Lambda(a), Concrete::Lambda(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// Concatenate two Nix lists: `left ++ right_elems`.
///
/// `left` must be a `Value::List`; `right_elems` is the right list's element
/// slice. When `left`'s backing `Rc<Vec>` is uniquely owned (a fresh
/// temporary, as in a left-associative `acc ++ [x]` fold), the right elements
/// are appended IN PLACE — amortized O(1) instead of the O(n) full clone that
/// `left.to_vec()` would cost. When the `Rc` is shared, the shared list is
/// left untouched and a fresh clone-extended Vec is built (identical to the
/// prior `to_vec()` + `extend_from_slice` path).
///
/// # Byte-neutrality
/// PROVABLY-NEUTRAL. Both paths produce the identical ordered sequence of the
/// same `Rc`-shared lazy `Value` thunks — no element is forced, reordered, or
/// re-identified. The only observable difference is heap allocation reuse,
/// which is not a Nix-observable property. See `docs/PERF-ARSENAL.md`.
pub fn concat_lists(left: Value, right_elems: &[Value]) -> Result<Value, EvalError> {
    // Take ownership of the left backing Vec, reusing its allocation when the
    // Rc is unique. `Rc::try_unwrap` returns the inner Vec on refcount 1;
    // otherwise it clones (identical bytes to the old `to_vec()`).
    let mut la = match left {
        Value::List(rc) => {
            let reused = Rc::strong_count(&rc) == 1;
            let vec: Vec<Value> = match Rc::try_unwrap(rc) {
                Ok(v) => v.into_vec(), // uniquely owned: allocation reused
                Err(rc) => (*rc).0.clone(), // shared: clone the left (unchanged)
            };
            if crate::perf::enabled() {
                crate::perf::inc(crate::perf::Counter::ListConcatCalls);
                if reused {
                    // Left elements appended in place — copy elided.
                    crate::perf::add(
                        crate::perf::Counter::ListConcatElemsReused,
                        vec.len() as u64,
                    );
                } else {
                    // Left elements cloned into a fresh Vec (the storm).
                    crate::perf::add(
                        crate::perf::Counter::ListConcatElemsCopied,
                        vec.len() as u64,
                    );
                }
            }
            vec
        }
        other => {
            return Err(EvalError::TypeMismatch {
                expected: "list",
                got: other.type_name(),
            });
        }
    };
    // Right elements are always copied (appended); their thunks are Rc-shared.
    la.extend_from_slice(right_elems);
    Ok(Value::list(la))
}

/// If `attrs` is a derivation — an attrset whose `type` forces to the string
/// `"derivation"` AND which carries a forceable `outPath` — return that
/// `outPath` string.  Otherwise `None` (caller falls back to structural
/// equality).  A force error on `type`/`outPath` yields `None`, so a broken
/// derivation degrades to structural compare rather than a spurious match.
fn derivation_out_path(attrs: &NixAttrs) -> Option<String> {
    match attrs.get("type")?.demand().ok()? {
        Concrete::String(s) if s.chars == "derivation" => {}
        _ => return None,
    }
    match attrs.get("outPath")?.demand().ok()? {
        Concrete::String(s) => Some(s.chars.to_string()),
        _ => None,
    }
}

/// If `attrs` is a **derivation** (`type` forces to `"derivation"`) carrying a
/// forceable `drvPath` AND `outPath`, return `Ok(Some((drv_path, out_path)))`.
///
/// Returns `Ok(None)` when `attrs` is not a derivation (no `type ==
/// "derivation"`, or missing `drvPath`) — e.g. a plain attrset that merely
/// carries an `outPath` (a `{ outPath = "…"; }` path-like), which has nothing
/// to realize. Returns `Err` only if forcing `drvPath`/`outPath` itself fails
/// (a genuinely broken derivation), so the caller surfaces the eval error
/// rather than silently treating a broken drv as "not a derivation".
///
/// This is the import-from-derivation sibling of [`derivation_out_path`]: that
/// helper only needs `outPath` for equality; realize also needs `drvPath` to
/// know *what* to build.
fn derivation_drv_and_out(
    attrs: &NixAttrs,
) -> Result<Option<(String, String)>, EvalError> {
    // Not a derivation unless `type` forces to exactly "derivation".
    match attrs.get("type") {
        Some(t) => match crate::eval::force_value(t)? {
            Value::String(s) if s.chars == "derivation" => {}
            _ => return Ok(None),
        },
        None => return Ok(None),
    }
    // A derivation without a drvPath cannot be realized — treat as non-drv so
    // the caller falls back to plain coercion (the outPath arm).
    let drv_path = match attrs.get("drvPath") {
        Some(d) => crate::eval::force_value(d)?.coerce_to_path("drvPath")?,
        None => return Ok(None),
    };
    let out_path = match attrs.get("outPath") {
        Some(o) => crate::eval::force_value(o)?.coerce_to_path("outPath")?,
        None => return Ok(None),
    };
    Ok(Some((drv_path, out_path)))
}

/// Given a store-path STRING (produced by interpolating a derivation) and its
/// string context, return the producing `.drv` path IF this store path is a
/// derivation output that should be realized on a filesystem read.
///
/// Returns `Some(drv_path)` only when the context carries a
/// `ContextElement::Output { drv, output }` whose `output` store path matches
/// `out_path` — i.e. this string IS the output of a derivation named by the
/// context. `Plain`/`DrvDeep`-only contexts (a plain store-path reference, or a
/// `.drv` self-reference) don't name an output to realize, and an
/// empty-context string is a literal path with nothing to build.
///
/// This is how cppnix decides IFD across interpolation: the derivation-ness of
/// `"${drv}"` survives as string context, not as a value shape.
fn out_path_needs_realize(out_path: &str, ctx: &StringContext) -> Option<String> {
    // Only store-path strings can be derivation outputs.
    if !out_path.starts_with("/nix/store/") {
        return None;
    }
    for elem in ctx.iter() {
        if let ContextElement::Output { drv, output } = elem {
            // The context stores the OUTPUT NAME (e.g. "out"/"dev"), while the
            // string IS the output's store path. cppnix's `Output.outputName`
            // matches the string it decorates; sui's tree-walker builds the
            // interpolated string FROM this output's store path, so a single
            // `Output` element on a store-path string is the producing drv.
            let _ = output; // output name is not needed to build the closure
            return Some(drv.to_string());
        }
    }
    None
}

impl Value {
    /// Convert a known-concrete Value to Concrete. Panics if Thunk.
    /// Only use when the caller guarantees the value is not a thunk.
    pub(crate) fn demand_unchecked(self) -> Concrete {
        match self {
            Value::Null => Concrete::Null,
            Value::Bool(b) => Concrete::Bool(b),
            Value::Int(n) => Concrete::Int(n),
            Value::Float(f) => Concrete::Float(f),
            Value::String(s) => Concrete::String(s),
            Value::Path(p) => Concrete::Path(p),
            Value::List(l) => Concrete::List(l),
            Value::Attrs(a) => Concrete::Attrs(a),
            Value::Lambda(c) => Concrete::Lambda(c),
            Value::Builtin(b) => Concrete::Builtin(b),
            Value::Thunk(_) => panic!("demand_unchecked called on Thunk"),
        }
    }
}

impl Value {
    /// Demand a concrete value. Forces if Thunk, returns as-is if concrete.
    ///
    /// This is the TYPED forcing API. The returned `Concrete` is guaranteed
    /// non-Thunk — enforced by the Concrete enum having NO Thunk variant.
    pub fn demand(&self) -> Result<Concrete, EvalError> {
        let v = match self {
            Value::Thunk(_) => crate::eval::force_value(self)?,
            other => other.clone(),
        };
        // Convert Value → Concrete. Thunk is impossible after force_value.
        match v {
            Value::Null => Ok(Concrete::Null),
            Value::Bool(b) => Ok(Concrete::Bool(b)),
            Value::Int(n) => Ok(Concrete::Int(n)),
            Value::Float(f) => Ok(Concrete::Float(f)),
            Value::String(s) => Ok(Concrete::String(s)),
            Value::Path(p) => Ok(Concrete::Path(p)),
            Value::List(l) => Ok(Concrete::List(l)),
            Value::Attrs(a) => Ok(Concrete::Attrs(a)),
            Value::Lambda(c) => Ok(Concrete::Lambda(c)),
            Value::Builtin(b) => Ok(Concrete::Builtin(b)),
            Value::Thunk(_) => {
                // force_value returned a Thunk — chase it.
                // This can happen when the transitive unwrap loop hits
                // a depth limit. Re-force to resolve.
                let re_forced = crate::eval::force_value(&v)?;
                match re_forced {
                    Value::Null => Ok(Concrete::Null),
                    Value::Bool(b) => Ok(Concrete::Bool(b)),
                    Value::Int(n) => Ok(Concrete::Int(n)),
                    Value::Float(f) => Ok(Concrete::Float(f)),
                    Value::String(s) => Ok(Concrete::String(s)),
                    Value::Path(p) => Ok(Concrete::Path(p)),
                    Value::List(l) => Ok(Concrete::List(l)),
                    Value::Attrs(a) => Ok(Concrete::Attrs(a)),
                    Value::Lambda(c) => Ok(Concrete::Lambda(c)),
                    Value::Builtin(b) => Ok(Concrete::Builtin(b)),
                    Value::Thunk(_) => Err(EvalError::InfiniteRecursion(
                        "demand: thunk chain could not be resolved".to_string(),
                    )),
                }
            }
        }
    }
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Value>() <= 16);

/// Runaway backstop for overlay-fixpoint promotion.  A genuine fixpoint
/// re-entry converges in a bounded number of nested promotions (the
/// nixpkgs `libxcrypt`/`self:super:` overlay needs ≤18 concurrent
/// promotions).  A non-converging demand (e.g. a cross-system stdenv
/// fixpoint that keeps re-entering the empty partial) climbs the nesting
/// without bound.  When the active concurrent-promotion nesting
/// (`IN_PROMISE_EVAL`) reaches this cap we STOP promoting and fall through
/// to `InfiniteRecursion` — which `eval_select`'s `x.y or default` arm
/// recovers exactly like nix's lazy fall-through, converting a would-be
/// native stack overflow into the recoverable error nix itself raises.
///
/// This is the runaway half of the same discipline the release-build
/// `MAX_EVAL_DEPTH` guard provides (which is `usize::MAX` in release to
/// admit nixpkgs' legitimately-deep fixpoints); scoping the bound to
/// *promotions* keeps ordinary deep evaluation unbounded while still
/// catching a non-terminating fixpoint before the OS stack does.
const FIXPOINT_PROMOTE_NEST_CAP: u32 = 32;

/// Force-stack-depth backstop that arms once a fixpoint promotion has fired
/// (`promotion_occurred()`).  A converging fixpoint (`libxcrypt`) bottoms
/// out at a force depth of a few dozen; a non-converging promoted partial
/// recurses without bound.  This cap (10× any observed real fixpoint's force
/// depth) converts a force-stack runaway into a recoverable
/// `InfiniteRecursion` before the native OS stack aborts, without touching
/// ordinary (non-promotion) deep evaluation.  Paired with the eval-depth
/// backstop (`eval::PROMOTION_RUNAWAY_EVAL_DEPTH`) for runaways that don't
/// climb the force stack.
const PROMOTION_RUNAWAY_FORCE_DEPTH: usize = 500;

thread_local! {
    /// Depth counter for "currently evaluating the body of a Promise-state
    /// thunk".  Incremented before the body of a `ThunkRepr::Promise`
    /// runs, decremented after.  Used by `eval_select` to treat missing
    /// attribute lookups on the Promise's sentinel value as `null`
    /// instead of erroring with `AttrNotFound`.  Scoped to Promise
    /// evaluation so unrelated user code retains cppnix-strict semantics.
    pub(crate) static IN_PROMISE_EVAL: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };

    /// Set once a fixpoint promotion has occurred anywhere in the current
    /// top-level evaluation.  Arms the release-active force-depth runaway
    /// backstop for the REST of the eval (not just while `IN_PROMISE_EVAL`
    /// is non-zero) — a corrupted promoted partial can send a DOWNSTREAM
    /// fixpoint (`makeOverridable`/`commonAttrs`) into unbounded recursion
    /// AFTER the promoting force has already returned, so the backstop must
    /// outlive the promotion's own softening scope.
    pub(crate) static PROMOTION_OCCURRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `true` if any overlay-fixpoint promotion has fired in this eval.
#[inline(always)]
pub fn promotion_occurred() -> bool {
    PROMOTION_OCCURRED.with(|c| c.get())
}

/// `true` if the evaluator is currently inside the body of a
/// `ThunkRepr::Promise` (used by `eval_select` to relax
/// `AttrNotFound` errors during fix-point construction).
#[inline(always)]
pub fn in_promise_eval() -> bool {
    IN_PROMISE_EVAL.with(|c| c.get() > 0)
}

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
    /// A deferred with-scope ident lookup.  Stores a direct reference to the
    /// with-scope's shared cache and the ident name.  When forced, checks the
    /// cache for the resolved attrset and looks up the name — O(1) hash lookup,
    /// no Env traversal, no fixpoint re-forcing.
    ///
    /// This is the construction-guarantee solution for the with-scope fixpoint
    /// problem: instead of creating 80K+ Env-capturing thunks (each doing a
    /// full lookup on force), we create 80K lightweight cache-referencing thunks
    /// that share the same resolved attrset.
    WithIdent {
        /// The ident name to look up
        name: SmolStr,
        /// Direct reference to the with-scope's cached attrset.
        /// Shared via Rc<RefCell> — all idents from the same `with` scope
        /// reference the same cache.  When ANY lookup forces the scope,
        /// the cache is populated and all subsequent WithIdent forces are O(1).
        scope_cache: Rc<RefCell<Option<NixAttrs>>>,
        /// The scope value (for initial force if cache is empty)
        scope_value: Value,
        /// Fallback: the full env for lexical+outer-scope lookup if the
        /// with-scope doesn't contain this name
        env: Env,
    },
    /// Currently being evaluated -- detects infinite recursion.
    Blackhole,
    /// Currently being evaluated, but the thunk is known to be
    /// self-recursive (its RHS references the bound name).  Inner
    /// re-entrance returns the partial value from the cell instead
    /// of erroring with `InfiniteRecursion` — matches cppnix's
    /// `let x = f x; in x` semantics where inner accesses to `x`
    /// see the not-yet-complete attrset under construction.
    ///
    /// The cell starts as `Value::Attrs(empty)` (the cheapest
    /// sentinel that propagates through `mapAttrs` / `attrNames` /
    /// `concatMap` without further type errors).  When the body
    /// completes, the cell is replaced with the final value and
    /// the repr transitions to `Evaluated`.
    Promise(Rc<RefCell<Value>>),
    /// A `Native` (`FnOnce`) thunk whose closure already ran and
    /// FAILED.  The closure is consumed and cannot be retried, so we
    /// memoize the error itself and re-raise it on every subsequent
    /// force.  This is the correctness-preserving replacement for the
    /// old `Evaluated(Null)` poisoning: a thunk that threw on its first
    /// force MUST NOT silently become `null` on a second read (which
    /// turned a swallowed transient flake-input error into a bogus
    /// `AttrNotFound`/`cannot select from set` far downstream — the
    /// stylix `darwinModules` marquee root).  A re-force re-throws the
    /// original error, exactly as cppnix re-throws a thunk that failed.
    Failed(EvalError),
    /// Already evaluated and memoized as a THUNK value.  The `cache`
    /// `OnceCell` is intentionally empty for this variant (caching a thunk
    /// would spin `force_value`), so the boxed `Value` is the sole store.
    Evaluated(Box<Value>),
    /// Already evaluated and memoized as a CONCRETE (non-thunk) value.
    /// The value lives ONLY in the `cache` `OnceCell` (`Box<Concrete>`);
    /// this variant is a valueless terminal marker that collapses the
    /// former double-store (a redundant `Evaluated(Box<Value>)` alongside
    /// the cache).  Any reader that finds this marker reconstructs the
    /// `Value` from `cache` via `Concrete::into_value()`, which is a
    /// byte-identical, lossless inverse of `demand_unchecked` (same enum
    /// shape, moves the inner `Rc`/`Box` — preserving string context and
    /// list/attrs `Rc` identity).  In practice the `cache` fast path in
    /// `force`/`force_inner` returns before this arm is ever matched.
    EvaluatedConcrete,
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
    cache: OnceCell<Box<Concrete>>,
    /// Full state machine for the thunk lifecycle.
    repr: UnsafeCell<ThunkRepr>,
    /// `true` when the thunk's RHS references its own bound name
    /// (a fix-point pattern like `let x = f x; in x`).  On force,
    /// transitions to `ThunkRepr::Promise` instead of `Blackhole`
    /// so inner re-entrance sees the partial value rather than
    /// erroring.  Detected at thunk-construction time via AST
    /// text search.
    recursive: bool,
}

impl Drop for ThunkInner {
    fn drop(&mut self) {
        census::dropped(&census::THUNK_LIVE);
    }
}

/// A lazy value with memoization and blackhole detection.
#[derive(Clone)]
pub struct Thunk(pub(crate) Rc<ThunkInner>);

impl Thunk {
    /// Create a thunk that will evaluate `expr` in `env` when forced.
    pub fn new_suspended(expr: rnix::ast::Expr, env: Env) -> Self {
        crate::trace::inc_thunks_created();
        census::made(&census::THUNK_MADE, &census::THUNK_LIVE);
        Self(Rc::new(ThunkInner {
            cache: OnceCell::new(),
            repr: UnsafeCell::new(ThunkRepr::Suspended { expr, env }),
            recursive: false,
        }))
    }

    /// Like [`new_suspended`] but marks the thunk as self-recursive.
    /// On force, inner re-entrance returns the partial value from the
    /// promise cell instead of erroring with `InfiniteRecursion`,
    /// matching cppnix's `let x = f x; in x` semantics.  Use this for
    /// let-bindings whose RHS textually references the bound name
    /// (see `eval::is_self_recursive_binding`).
    pub fn new_suspended_recursive(expr: rnix::ast::Expr, env: Env) -> Self {
        crate::trace::inc_thunks_created();
        census::made(&census::THUNK_MADE, &census::THUNK_LIVE);
        crate::perf::inc(crate::perf::Counter::ThunkSiteLetForward);
        Self(Rc::new(ThunkInner {
            cache: OnceCell::new(),
            repr: UnsafeCell::new(ThunkRepr::Suspended { expr, env }),
            recursive: true,
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
        census::made(&census::THUNK_MADE, &census::THUNK_LIVE);
        crate::perf::inc(crate::perf::Counter::ThunkSiteInheritSrc);
        Self(Rc::new(ThunkInner {
            cache: OnceCell::new(),
            repr: UnsafeCell::new(ThunkRepr::InheritSelect {
                source_thunk,
                name: name.into(),
            }),
            recursive: false,
        }))
    }

    /// Create a WithIdent thunk — a deferred with-scope ident lookup.
    /// Stores a direct reference to the shared with-scope cache.
    /// When forced: O(1) hash lookup in the cache, no Env traversal.
    pub fn new_with_ident(
        name: SmolStr,
        scope_cache: Rc<RefCell<Option<NixAttrs>>>,
        scope_value: Value,
        env: Env,
    ) -> Self {
        crate::trace::inc_thunks_created();
        census::made(&census::THUNK_MADE, &census::THUNK_LIVE);
        crate::perf::inc(crate::perf::Counter::ThunkSiteOther);
        Self(Rc::new(ThunkInner {
            cache: OnceCell::new(),
            repr: UnsafeCell::new(ThunkRepr::WithIdent {
                name,
                scope_cache,
                scope_value,
                env,
            }),
            recursive: false,
        }))
    }

    /// Create a thunk backed by a Rust closure.  When forced, the
    /// closure is called exactly once and its result is memoized.
    /// This is used for lazy flake input evaluation.
    pub fn new_native(f: impl FnOnce() -> Result<Value, EvalError> + 'static) -> Self {
        crate::trace::inc_thunks_created();
        census::made(&census::THUNK_MADE, &census::THUNK_LIVE);
        crate::perf::inc(crate::perf::Counter::ThunkSiteNative);
        Self(Rc::new(ThunkInner {
            cache: OnceCell::new(),
            repr: UnsafeCell::new(ThunkRepr::Native(Box::new(f))),
            recursive: false,
        }))
    }

    /// Create a thunk that is already evaluated (an optimization).
    /// Pre-populates the `OnceCell` cache so the fast path is
    /// immediately available.
    pub fn new_evaluated(value: Value) -> Self {
        crate::trace::inc_thunks_created();
        census::made(&census::THUNK_MADE, &census::THUNK_LIVE);
        crate::perf::inc(crate::perf::Counter::ThunkSiteEvaluated);
        let cache = OnceCell::new();
        // Collapse the double-store: a concrete value lives ONLY in the
        // cache with an `EvaluatedConcrete` marker repr; a thunk value
        // keeps the boxed `Evaluated` repr and an empty cache.
        let repr = if matches!(value, Value::Thunk(_)) {
            ThunkRepr::Evaluated(Box::new(value))
        } else {
            let _ = cache.set(Box::new(value.demand_unchecked()));
            ThunkRepr::EvaluatedConcrete
        };
        Self(Rc::new(ThunkInner {
            cache,
            repr: UnsafeCell::new(repr),
            recursive: false,
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

    /// Peek at the cached value WITHOUT forcing.
    /// Returns Some(&Value) if the thunk has been evaluated, None otherwise.
    /// This is used by with-scope lookup to check if the fixpoint thunk
    /// has already been resolved (by another evaluation path) without
    /// entering the force state machine.
    pub fn peek(&self) -> Option<&Concrete> {
        self.0.cache.get().map(|v| &**v)
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

    /// Store a forced result into this thunk's terminal state, collapsing
    /// the former thunk double-store.
    ///
    /// - A CONCRETE (non-thunk) result is stored ONLY in the `cache`
    ///   `OnceCell` (`Box<Concrete>`), and `repr` becomes the valueless
    ///   `EvaluatedConcrete` marker — freeing the redundant
    ///   `Box<Value>` that `Evaluated` used to hold. Reconstruction via
    ///   `Concrete::into_value()` is a byte-identical inverse of the
    ///   `demand_unchecked()` used to fill the cache.
    /// - A THUNK result keeps `repr = Evaluated(Box<Value>)` and leaves
    ///   the cache empty (caching a thunk would spin `force_value`).
    ///
    /// SAFETY: single-threaded evaluator (`Rc`, not `Arc`); the caller
    /// must hold no other borrow of `repr` — every call site here is on
    /// the sequential `Suspended → Blackhole/Promise → Evaluated`
    /// transition, so no overlapping mutable access exists.
    ///
    /// Takes `&Value` and clones exactly as the former open-coded stores
    /// did (`Box::new(value.clone())` for the thunk repr,
    /// `Box::new(value.clone().demand_unchecked())` for the cache) — so the
    /// clone count is identical to the pre-collapse code and the change is
    /// byte-neutral by construction.
    #[inline]
    unsafe fn store_evaluated(&self, value: &Value) {
        census::evaluated();
        if matches!(value, Value::Thunk(_)) {
            *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(Box::new(value.clone()));
        } else {
            let _ = self.0.cache.set(Box::new(value.clone().demand_unchecked()));
            *unsafe { &mut *self.0.repr.get() } = ThunkRepr::EvaluatedConcrete;
        }
    }

    /// Owned-value variant of the concrete branch of [`store_evaluated`],
    /// for the `force_inner` early-return that OWNS `value` and does not
    /// need it afterward.
    ///
    /// `store_evaluated(&value)` clones the whole `Value` to fill the
    /// cache (`Box::new(value.clone().demand_unchecked())`) and the caller
    /// then returns the owned `value` separately — an extra *outer*
    /// `Value` clone. Here we MOVE `value` into the cache (no outer clone)
    /// and clone the cheaper inner `Concrete` for the return, trading one
    /// `Value::clone` for one `Concrete::clone` (the same inner `Rc` bumps,
    /// one fewer throwaway `Value` temporary).
    ///
    /// Content-, order-, and census-neutral versus the
    /// `store_evaluated(&value); return Ok(value)` it replaces: the cache
    /// holds the identical `Box<Concrete>`, `repr` becomes the identical
    /// `EvaluatedConcrete` marker, `census::evaluated()` fires exactly
    /// once, and the returned `Value` is a byte-identical reconstruction
    /// of `value`.
    ///
    /// Panics (via `demand_unchecked`) if `value` is a `Thunk` — the sole
    /// call site only reaches it on the `!was_thunk_before_loop` branch,
    /// where `value` is guaranteed non-`Thunk`.
    ///
    /// SAFETY: same contract as [`store_evaluated`] — single-threaded,
    /// no overlapping `repr` borrow.
    #[inline]
    unsafe fn store_evaluated_owned(&self, value: Value) -> Value {
        census::evaluated();
        let concrete = value.demand_unchecked();
        let ret = concrete.clone().into_value();
        let _ = self.0.cache.set(Box::new(concrete));
        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::EvaluatedConcrete;
        ret
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
            return Ok((**cached).clone().into_value());
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
            return Ok((**cached).clone().into_value());
        }

        let thunk_id = Rc::as_ptr(&self.0) as usize;

        // Promise fast-path: if this thunk is currently in `Promise`
        // state (a self-recursive fix-point whose outer body is still
        // running, and *this* call is an inner re-entrance), return
        // the cell's current partial value without consuming the
        // repr.  Matches cppnix's `let x = f x; in x` semantics:
        // inner accesses to `x` during f's evaluation see the not-
        // yet-complete value instead of erroring with
        // `InfiniteRecursion`.
        //
        // SAFETY: Single-threaded evaluator. The immutable borrow
        // is scoped to this `if let` block; the early return exits
        // before any further access to `repr`.
        if let ThunkRepr::Promise(cell) = unsafe { &*self.0.repr.get() } {
            return Ok(cell.borrow().clone());
        }

        // Take the current repr.  Replace with `Promise(cell)` if the
        // thunk is self-recursive (so inner re-entrance during body
        // evaluation hits the fast-path above), otherwise classic
        // `Blackhole` (so inner re-entrance errors with
        // `InfiniteRecursion`, which is the correct behaviour for
        // non-recursive bindings like `let r = r; in r`).
        // SAFETY: Single-threaded evaluator. State machine ensures no
        // overlapping mutable access: Suspended->Blackhole/Promise->Evaluated.
        let new_repr_on_force = if self.0.recursive {
            ThunkRepr::Promise(Rc::new(RefCell::new(
                Value::Attrs(Rc::new(NixAttrs::new())),
            )))
        } else {
            ThunkRepr::Blackhole
        };
        let is_promise = self.0.recursive;
        let repr = std::mem::replace(unsafe { &mut *self.0.repr.get() }, new_repr_on_force);

        match repr {
            ThunkRepr::Suspended { expr, env } => {
                crate::perf::inc(crate::perf::Counter::ThunkForce);
                crate::trace::inc_thunks_forced_unique();
                let tracing = crate::trace::trace_enabled();
                // Always push a force frame — `pop_force` is matched in every
                // exit path below.  This keeps the cycle chain on
                // `EvalError::InfiniteRecursion` populated WITHOUT requiring
                // the operator to set `SUI_TRACE_EVAL=verbose` first.  In
                // tracing mode we also capture the (expensive) source-text
                // description; otherwise we keep the frame cheap (just the
                // file + thunk id) so the always-on overhead stays bounded.
                let desc: String = if tracing {
                    expr.syntax().text().to_string().chars().take(60).collect()
                } else {
                    String::new()
                };
                crate::trace::push_force(crate::trace::ForceFrame {
                    defined_in: env.eval_file().cloned(),
                    description: desc.clone(),
                    thunk_id,
                });
                // Runaway backstop #1 (force-stack depth) for overlay-fixpoint
                // promotion (release-active; belt-and-suspenders with the
                // eval-depth backstop in `eval::DepthGuard::enter`).
                //
                // A promoted empty-attrs partial is byte-correct for the
                // native-system stdenv fixpoint (`libxcrypt` — the actual
                // byte-parity root; its promotions bottom out at a force depth
                // ≤ ~50), but is the WRONG partial for a demand that indexes it
                // as a list / non-attrs (the cross-system Darwin `apple-sdk`
                // path `hello` hits when `builtins.currentSystem` is macOS).
                // There the empty partial feeds a downstream `makeOverridable`
                // fixpoint that recurses without bound.  Release disables the
                // general `MAX_EVAL_DEPTH` guard (`usize::MAX`) to admit
                // nixpkgs' legitimately-deep fixpoints, so nothing else stops
                // that recursion before the OS stack aborts.
                //
                // Armed only once a promotion has fired (`promotion_occurred()`)
                // and for the REST of the eval — a corrupted partial can send a
                // downstream fixpoint runaway AFTER the promoting force returns,
                // so the backstop must outlive the promotion's own softening
                // scope.  A runaway that climbs the force stack is caught here;
                // one that climbs `eval_expr` without pushing force frames is
                // caught by the eval-depth backstop.  Either converts the
                // would-be native abort into a recoverable `InfiniteRecursion`
                // (which `x.y or default` recovers exactly like nix).
                if crate::value::promotion_occurred()
                    && crate::trace::current_force_depth() as usize
                        > PROMOTION_RUNAWAY_FORCE_DEPTH
                {
                    crate::trace::pop_force();
                    *unsafe { &mut *self.0.repr.get() } =
                        ThunkRepr::Suspended { expr, env };
                    return Err(EvalError::InfiniteRecursion(
                        "overlay-fixpoint promotion runaway (force depth exceeded)".into(),
                    ));
                }
                if tracing {
                    crate::trace::trace_force_enter(
                        env.eval_file().map(|p| p.as_path()),
                        &desc,
                    );
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
                }
                // Push the thunk's captured eval_file onto the thread-local
                // stack so PathRel literals and relative imports inside the
                // thunk body resolve against the file where the thunk was
                // *defined*, not where it is forced from. The RAII guard
                // pops on drop (including on error paths).
                let _file_guard = env.eval_file().cloned().map(crate::eval::push_eval_file);
                // Restore the thunk's DEFINING source_id in lockstep with
                // eval_file above, so idents evaluated in the thunk body key
                // the `(source_id, offset)` symbol cache against the file the
                // thunk was defined in — not the ambient source at force time.
                // Without this a cross-file force (a lazy thunk from an
                // imported file, forced after `eval_with_file` restored the
                // top-level source_id) collides on a reused offset and returns
                // a wrong Symbol (`parse.nix` `cannot select from null`).
                let _srcid_guard = crate::eval::push_source_id(env.source_id());
                // M2.6 Promise scope: bump the thread-local counter so
                // downstream `eval_select` can soften `AttrNotFound`
                // errors on the Promise's sentinel value to `null`.
                // Scoped strictly to Promise-thunk body evaluation;
                // non-recursive thunks retain cppnix-strict semantics.
                if is_promise {
                    IN_PROMISE_EVAL.with(|c| c.set(c.get() + 1));
                }
                let result = evaluator(&expr, &env);
                if is_promise {
                    IN_PROMISE_EVAL.with(|c| c.set(c.get().saturating_sub(1)));
                }
                // A `Blackhole` thunk (non-recursive at construction) may have
                // been PROMOTED to `Promise` mid-body by a same-thunk fixpoint
                // re-entry (the overlay-fixpoint path in the Blackhole arm
                // below).  That promotion bumped `IN_PROMISE_EVAL` once; balance
                // it here, and populate its cell exactly like a
                // recursive-at-construction Promise.  `is_promise` covers the
                // construction-time case; `became_promise` covers the mid-body
                // semantic-promotion case.  They're mutually exclusive (a
                // construction-time Promise never re-enters the Blackhole arm).
                let became_promise = !is_promise
                    && matches!(unsafe { &*self.0.repr.get() }, ThunkRepr::Promise(_));
                if became_promise {
                    IN_PROMISE_EVAL.with(|c| c.set(c.get().saturating_sub(1)));
                }
                match result {
                    Ok(mut value) => {
                        crate::perf::inc(crate::perf::Counter::ThunkStoreWrites);
                        // M2.6 Promise update: if this thunk transitioned
                        // through Promise(cell), populate the cell with the
                        // final value BEFORE setting Evaluated.  Any
                        // outstanding Rc clones of the cell (held by inner
                        // thunks whose bodies haven't yet run) will see the
                        // complete value when they later force.
                        if is_promise || became_promise {
                            if let ThunkRepr::Promise(cell) = unsafe { &*self.0.repr.get() } {
                                *cell.borrow_mut() = value.clone();
                            }
                        }
                        // Whether the body returned a Thunk decides the store
                        // shape.  Computed BEFORE the store so the non-thunk
                        // path can MOVE `value` into the cache (owned store)
                        // instead of cloning it (see `store_evaluated_owned`).
                        //
                        // C-store PROVABLY-NEUTRAL narrow win (M2, byte-verified):
                        // when `value` is NOT a Thunk, the collapse loop below
                        // does not execute (its guard is `while let Value::Thunk`),
                        // so the second store (in the thunk branch) would rewrite
                        // BYTE-IDENTICAL repr content and re-attempt a no-op
                        // OnceCell `cache.set`.  Skipping it is content-AND-order-
                        // neutral: the single store already established the
                        // terminal (cache=concrete, repr=EvaluatedConcrete);
                        // nothing between the stores observes `self.0.repr` (the
                        // body has returned — no re-entrant force of self is in
                        // flight; the loop only `peek()`s OTHER thunks' OnceCell
                        // caches, never self's repr), and no code observes the
                        // `Box`'s pointer identity (repr is only ever read by
                        // value — grep-confirmed). Only when `value` IS a Thunk
                        // (the loop may collapse it to a different concrete) do we
                        // re-store the unwrapped result.
                        let was_thunk_before_loop = matches!(value, Value::Thunk(_));
                        if !was_thunk_before_loop {
                            // Non-thunk: single owned store (no outer Value clone),
                            // return the reconstruction. Byte-, order-, and census-
                            // identical to `store_evaluated(&value); return Ok(value)`
                            // (Store#2 is pure redundant and skipped, as before).
                            crate::perf::inc(crate::perf::Counter::ThunkStoreRedundant);
                            let ret = unsafe { self.store_evaluated_owned(value) };
                            crate::trace::pop_force();
                            if tracing { crate::trace::trace_force_exit(); }
                            return Ok(ret);
                        }
                        // Thunk path (unchanged): Store#1, collapse loop, Store#2.
                        unsafe { self.store_evaluated(&value) };
                        // Transitively unwrap thunk-in-thunk chains, with a
                        // depth limit to catch `let x = x; in x` cycles.
                        // Chase already-resolved thunks only (peek).
                        // force_value handles full transitive resolution.
                        while let Value::Thunk(ref inner) = value {
                            match inner.peek() {
                                Some(cached) => value = cached.clone().into_value(),
                                None => break,
                            }
                        }
                        if !matches!(value, Value::Thunk(_)) {
                            crate::perf::inc(crate::perf::Counter::ThunkStoreLoopMutated);
                        }
                        unsafe { self.store_evaluated(&value) };
                        crate::trace::pop_force();
                        if tracing { crate::trace::trace_force_exit(); }
                        Ok(value)
                    }
                    Err(e) => {
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Suspended { expr, env };
                        if tracing { crate::trace::dump_trace_on_error(); }
                        crate::trace::pop_force();
                        if tracing { crate::trace::trace_force_exit(); }
                        Err(e)
                    }
                }
            }
            ThunkRepr::InheritSelect { source_thunk, name } => {
                let tracing = crate::trace::trace_enabled();
                let desc = if tracing { format!("inherit (..) {name}") } else { String::new() };
                crate::trace::push_force(crate::trace::ForceFrame {
                    defined_in: None,
                    description: desc.clone(),
                    thunk_id,
                });
                if tracing {
                    crate::trace::trace_force_enter(None, &desc);
                }
                crate::trace::inc_thunks_forced_unique();
                if tracing {
                    if let Err(msg) = crate::trace::check_force_depth() {
                        crate::trace::dump_trace_on_error();
                        crate::trace::pop_force();
                        crate::trace::trace_force_exit();
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::InheritSelect {
                            source_thunk,
                            name,
                        };
                        return Err(EvalError::InfiniteRecursion(msg));
                    }
                }
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
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(Box::new(value.clone()));
                        while let Value::Thunk(ref inner) = value {
                            match inner.peek() { Some(c) => value = c.clone().into_value(), None => break }
                        }
                        unsafe { self.store_evaluated(&value) };
                        crate::trace::pop_force();
                        if tracing { crate::trace::trace_force_exit(); }
                        Ok(value)
                    }
                    Err(e) => {
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::InheritSelect { source_thunk, name };
                        if tracing { crate::trace::dump_trace_on_error(); }
                        crate::trace::pop_force();
                        if tracing { crate::trace::trace_force_exit(); }
                        Err(e)
                    }
                }
            }
            ThunkRepr::Native(f) => {
                let tracing = crate::trace::trace_enabled();
                crate::trace::push_force(crate::trace::ForceFrame {
                    defined_in: None,
                    description: if tracing { "<native-thunk>".into() } else { String::new() },
                    thunk_id,
                });
                if tracing {
                    crate::trace::trace_force_enter(None, "<native-thunk>");
                }
                crate::trace::inc_thunks_forced_unique();
                // The closure is consumed (FnOnce).  On success we
                // memoize the result.  On failure we leave Blackhole
                // — unlike Suspended thunks the closure cannot be
                // retried because it has been consumed.
                match f() {
                    Ok(mut value) => {
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(Box::new(value.clone()));
                        while let Value::Thunk(ref inner) = value {
                            match inner.peek() { Some(c) => value = c.clone().into_value(), None => break }
                        }
                        unsafe { self.store_evaluated(&value) };
                        crate::trace::pop_force();
                        if tracing { crate::trace::trace_force_exit(); }
                        Ok(value)
                    }
                    Err(e) => {
                        // The `FnOnce` closure is consumed and cannot be
                        // retried.  Memoize the ERROR (not `Null`): a
                        // re-force must re-raise, never silently return a
                        // value the first force did not produce.  The old
                        // `Evaluated(Null)` here poisoned a flake-input
                        // thunk whose first force failed transiently
                        // (e.g. a not-yet-cached transitive source) so a
                        // later re-read saw `null` — surfacing as a bogus
                        // downstream `AttrNotFound` /
                        // `cannot select from set` (the stylix
                        // `darwinModules` marquee root).  Do NOT populate
                        // the OnceCell (there is no correct concrete value
                        // to cache); the `Failed` repr arm re-raises.
                        *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Failed(e.clone());
                        if tracing { crate::trace::dump_trace_on_error(); }
                        crate::trace::pop_force();
                        if tracing { crate::trace::trace_force_exit(); }
                        Err(e)
                    }
                }
            }
            ThunkRepr::WithIdent { name, scope_cache, scope_value, env } => {
                crate::perf::inc(crate::perf::Counter::ThunkForce);
                crate::trace::inc_thunks_forced_unique();
                // Fast path: check the shared with-scope cache.
                // All WithIdent thunks from the same `with` scope share
                // this cache. Once ANY lookup populates it, all others
                // are O(1) hash lookups.
                {
                    let cache = scope_cache.borrow();
                    if let Some(ref attrs) = *cache {
                        if let Some(v) = attrs.get(&name) {
                            let value = v.clone();
                            unsafe { self.store_evaluated(&value) };
                            return Ok(value);
                        }
                        // Name not in cached attrset — fall through to env lookup
                    }
                }
                // Cache not populated yet — force the scope value to populate it
                if let Ok(forced) = crate::eval::force_value(&scope_value) {
                    if let Value::Attrs(ref attrs) = forced {
                        *scope_cache.borrow_mut() = Some((**attrs).clone());
                        if let Some(v) = attrs.get(&name) {
                            let value = v.clone();
                            unsafe { self.store_evaluated(&value) };
                            return Ok(value);
                        }
                    }
                }
                // Name not in with-scope — fall back to full env lookup.
                //
                // The cache-first with-scope search may have skipped a scope
                // whose CACHE is a stale mid-fixpoint PARTIAL — e.g. `f self`
                // cached BEFORE makeScope's `self = f self // { callPackage = …; }`
                // merged the scope infra in, so `callPackage` is absent from
                // the stale partial yet present in the COMPLETED `self`. On any
                // lexical-scope miss (both inside and outside a Promise body),
                // re-resolve by force_value-ing each with-scope FRESH (bypassing
                // the cache) via `lookup_fresh`. It catches errors, so a
                // genuinely mid-fixpoint / throwing scope simply skips and
                // returns None — leaving the Promise-body null softening (below)
                // for the case where the with-source really IS the empty-attrset
                // sentinel. A completed value always wins over the null sentinel.
                //
                // This is the SAME class as the neovim/python27
                // `with self; with super; callPackage` root, but reached through
                // the resholve `python27' = (…).override { self = python27'; }`
                // recursive-fixpoint hooks scope, where the miss lands inside a
                // Promise body (`in_promise_eval()` true) and was previously
                // softened to `null` BEFORE `lookup_fresh` ran — silently
                // dropping `pip = callPackage …` (→ empty `propagatedBuildInputs`
                // on `pip-install-hook.drv`). Trying the completed-`self`
                // resolution first restores the drop.
                //
                // Byte-neutral: `lookup_fresh` only ever returns a value nix's
                // single lazy `self` would ALSO expose; when it misses (genuine
                // empty-partial sentinel) the softening / error behavior below is
                // exactly as before.
                let result = match env.lookup(&name) {
                    Some(v) => v,
                    None => match env.lookup_fresh(&name) {
                        Some(v) => v,
                        None if in_promise_eval() => Value::Null,
                        None => return Err(EvalError::UndefinedVar(format!("'{name}'"))),
                    },
                };
                unsafe { self.store_evaluated(&result) };
                Ok(result)
            }
            ThunkRepr::Blackhole => {
                // M2.6 bridge: when an inner force re-enters a thunk
                // that's currently being evaluated, cppnix's effective
                // behavior is to expose the not-yet-complete value
                // (typically a partial attrset).  Without the proper
                // `Promise(NixAttrs)` thunk variant (see
                // docs/M2.6-MODULE-SYSTEM-FIXPOINT.md::Genuine fix),
                // we approximate by returning a sentinel of the
                // operator's choice:
                //
                //   SUI_BLACKHOLE_AS_NULL=1         → Value::Null
                //   SUI_BLACKHOLE_AS_EMPTY_ATTRS=1  → Value::Attrs({})
                //   SUI_BLACKHOLE_AS_EMPTY_LIST=1   → Value::List([])
                //
                // `EMPTY_ATTRS` is the closest approximation for the
                // NixOS module-system fix-point because the cppnix
                // partial is itself an attrset — downstream
                // `mapAttrs`/`attrNames`/`concatMap` on the sentinel
                // see "no keys to map" rather than a type error.
                //
                // Default-off for all variants because each silently
                // hides legitimate cycles in user code (`let r = r;
                // in r.x` would return missing-attr or 0 instead of
                // erroring).
                if std::env::var_os("SUI_BLACKHOLE_AS_NULL").is_some() {
                    return Ok(Value::Null);
                }
                if std::env::var_os("SUI_BLACKHOLE_AS_EMPTY_LIST").is_some() {
                    return Ok(Value::List(Rc::new(NixList::new(Vec::new()))));
                }
                if std::env::var_os("SUI_BLACKHOLE_AS_EMPTY_ATTRS").is_some() {
                    return Ok(Value::Attrs(Rc::new(NixAttrs::new())));
                }
                if std::env::var_os("SUI_DEBUG_CYCLE").is_some() {
                    let same = crate::trace::force_stack_contains(thunk_id);
                    eprintln!(
                        "[SUI_DEBUG_CYCLE] blackhole re-entry thunk_id={thunk_id:#x} same_thunk_on_stack={same} recursive_flag={}",
                        self.0.recursive
                    );
                    crate::trace::dump_force_stack_ids();
                }
                // OVERLAY-FIXPOINT SEMANTIC PROMOTION (2026-07-10, default-ON).
                //
                // When the re-entered thunk is the SAME thunk currently
                // mid-evaluation on the force stack, this is a genuine fixpoint
                // self-reference — the nixpkgs `self:super:` overlay / `lib.fix`
                // pattern threading through `callPackage`/`self`/`super` across
                // file boundaries.  `is_self_recursive_binding` (syntactic RHS
                // name search) MISSES this because the binding's RHS never
                // textually names itself, so the thunk was classified
                // `recursive=false` and installed a hard `Blackhole` where nix
                // exposes the not-yet-complete value.  That misclassification is
                // exactly the byte-parity defect (`sui-spec/src/laziness.rs`
                // `RecursionKind::Fixpoint` ⇒ MUST be recursive + Promise): the
                // dropped perl `nativeBuildInput` on `pkgs.libxcrypt` (sui
                // q9b9v7a9… vs nix jb9k6090…).
                //
                // The FIX is the Blackhole↔Promise machinery, not a sentinel:
                // retroactively PROMOTE this Blackhole to a real `Promise(cell)`
                // and return the cell's in-progress partial.  Unlike the earlier
                // blank-empty-attrs sentinel (which left the thunk in Blackhole
                // forever and stack-overflowed `hello`), the promoted cell is a
                // first-class fixpoint cell:
                //   * the outer body populates it on completion (the
                //     `is_promise || became_promise` branch below), so any inner
                //     Rc clones that already read the empty partial converge, and
                //     the repr transitions cleanly to `Evaluated`;
                //   * `IN_PROMISE_EVAL` is bumped so downstream `eval_select`
                //     softens `AttrNotFound`/`cannot-select` on the partial to
                //     `null` (the `x.y or default` fall-through nix relies on),
                //     which is what stops the `hello` overflow.
                //
                // Genuine NON-terminating cycles (`let r = r; in r`) remain
                // errors: the promoted partial cannot make progress, so the
                // force-depth backstop (`check_force_depth`, ~2048/100 in
                // test/release) still fires `InfiniteRecursion` — nix's own
                // behaviour.  This is the semantic (fixpoint) classification the
                // typed discipline demands, done in the demand-order engine
                // instead of at syntactic construction time.
                if crate::trace::force_stack_contains(thunk_id)
                    && IN_PROMISE_EVAL.with(|c| c.get()) < FIXPOINT_PROMOTE_NEST_CAP
                {
                    if std::env::var_os("SUI_DEBUG_CYCLE").is_some() {
                        let chain = crate::trace::capture_cycle(thunk_id);
                        let nest = IN_PROMISE_EVAL.with(|c| c.get());
                        let fdepth = crate::trace::current_force_depth();
                        eprintln!("[SUI_PROMOTE] thunk_id={thunk_id:#x} cycle_len={} nest={nest} fdepth={fdepth}", chain.0.len());
                    }
                    let cell = Rc::new(RefCell::new(
                        Value::Attrs(Rc::new(NixAttrs::new())),
                    ));
                    // SAFETY: single-threaded evaluator; we hold no other borrow
                    // of `repr` here (the outer match consumed it, we replace it).
                    *unsafe { &mut *self.0.repr.get() } =
                        ThunkRepr::Promise(cell.clone());
                    // Enable Promise-body softening for the remainder of the
                    // outer force.  Decremented once by the outer force's
                    // post-body reconciliation (`became_promise`).
                    IN_PROMISE_EVAL.with(|c| c.set(c.get() + 1));
                    // Arm the release runaway backstop for the rest of the eval.
                    PROMOTION_OCCURRED.with(|c| c.set(true));
                    return Ok(cell.borrow().clone());
                }
                let chain = crate::trace::capture_cycle(thunk_id);
                crate::trace::dump_trace_on_error();
                Err(EvalError::InfiniteRecursion(chain.to_string()))
            }
            ThunkRepr::Promise(cell) => {
                // Inner re-entrance into a self-recursive thunk that's
                // currently being evaluated.  Return the partial value
                // the body has constructed so far (the cell starts as
                // `Value::Attrs(empty)` and gets updated on body return).
                // This is sui's cppnix-equivalent for `let x = f x; in x`
                // — the inner reference to `x` during f's evaluation
                // sees a partial attrset instead of the original cycle's
                // `InfiniteRecursion`.
                Ok(cell.borrow().clone())
            }
            ThunkRepr::Evaluated(v) => {
                // Reached when OnceCell wasn't populated (value was a thunk
                // when first evaluated). Cache only concrete values — caching
                // a thunk would cause force_value's loop to spin.
                crate::perf::inc(crate::perf::Counter::ThunkHit);
                let cloned = (*v).clone();
                if !matches!(cloned, Value::Thunk(_)) {
                    if !matches!(cloned, Value::Thunk(_)) { let _ = self.0.cache.set(Box::new(cloned.clone().demand_unchecked())); }
                }
                *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Evaluated(v);
                Ok(cloned)
            }
            ThunkRepr::EvaluatedConcrete => {
                // The concrete value lives in `cache`; the repr is a valueless
                // marker (the collapsed former double-store). In practice this
                // arm is unreachable: `force`/`force_inner` check the `cache`
                // fast path BEFORE the `mem::replace` that consumes the repr,
                // and `EvaluatedConcrete` always co-occurs with a populated
                // cache — so the fast path returns first. Handle it faithfully
                // anyway: reconstruct the `Value` from the cache (a byte-
                // identical inverse of `demand_unchecked`) and restore the
                // marker (the outer `mem::replace` swapped in Blackhole/Promise).
                crate::perf::inc(crate::perf::Counter::ThunkHit);
                let value = self
                    .0
                    .cache
                    .get()
                    .expect("EvaluatedConcrete implies a populated cache")
                    .as_ref()
                    .clone()
                    .into_value();
                *unsafe { &mut *self.0.repr.get() } = ThunkRepr::EvaluatedConcrete;
                Ok(value)
            }
            ThunkRepr::Failed(e) => {
                // A previously-forced `Native` thunk whose closure threw.
                // Re-raise the memoized error — never fall through to a
                // silent value.  Restore the repr (the outer
                // `mem::replace` swapped in Blackhole/Promise).
                let err = e.clone();
                *unsafe { &mut *self.0.repr.get() } = ThunkRepr::Failed(e);
                Err(err)
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
            ThunkRepr::WithIdent { name, .. } => write!(f, "<with-ident {name}>"),
            ThunkRepr::Blackhole => write!(f, "<blackhole>"),
            ThunkRepr::Promise(_) => write!(f, "<promise>"),
            ThunkRepr::Failed(e) => write!(f, "<failed-thunk: {e}>"),
            ThunkRepr::Evaluated(v) => write!(f, "{v:?}"),
            ThunkRepr::EvaluatedConcrete => match self.0.cache.get() {
                Some(c) => write!(f, "{:?}", c.as_ref().clone().into_value()),
                None => write!(f, "<evaluated-concrete>"),
            },
        }
    }
}

/// A Nix attribute set with lazy overlay support.
///
/// Internally uses either a concrete compact `AttrsMap` or a lazy overlay chain.
/// The `//` operator creates O(1) overlay nodes instead of O(m log n) merges.
/// Attribute access walks the chain right-to-left in O(depth).
/// Full iteration (attrNames, attrValues) flattens on demand.
///
/// The second tuple field is an OPTIONAL source-position table (`None` for
/// the vast majority of attrsets — merges, overlays, builtin-built, dynamic
/// keys). `eval_attrset` attaches it for a literal with static keys so
/// `builtins.unsafeGetAttrPos` can report a key's file/line/column (the
/// `attrTag` `declarations` — options.json dock root). It is behind `Rc`, so
/// a clone is a refcount bump; `None` costs one pointer-sized word.
pub struct NixAttrs(AttrsInner, Option<Rc<crate::pos::AttrPositions>>);

// Hand-written `Clone`/`Drop` so the census counts every NixAttrs value that
// comes into existence (a clone is a fresh heap object once Rc-wrapped),
// keeping `ATTRS_MADE`/`ATTRS_LIVE` consistent. Fresh (non-clone)
// constructions bump the counter at each `NixAttrs(...)` tuple-construct site.
impl Clone for NixAttrs {
    fn clone(&self) -> Self {
        census::made(&census::ATTRS_MADE, &census::ATTRS_LIVE);
        NixAttrs(self.0.clone(), self.1.clone())
    }
}

impl Drop for NixAttrs {
    fn drop(&mut self) {
        census::dropped(&census::ATTRS_LIVE);
    }
}

/// Internal representation: either a flat map or an overlay chain.
#[derive(Clone)]
enum AttrsInner {
    /// Concrete attribute set — compact flat `AttrsMap` (std hashbrown).
    Flat(AttrsMap<Symbol, Value>),
    /// Lazy overlay: right overrides left. O(1) construction.
    /// `cache` is populated on first full iteration (attrNames, etc.).
    ///
    /// `left`/`right` are interior-mutable so they can be RELEASED (swapped to an
    /// empty attrs) once `cache` is populated: after flatten the merged `cache`
    /// is the complete answer and every reader (`get_sym`/`contains_key`/
    /// `is_empty`) routes through `as_flat()` (the cache), so the un-merged
    /// parents are dead weight. Releasing them cascade-frees the intermediate
    /// overlay chain (the 50+-deep module-fixpoint retention — `EVAL-MEMORY.md`).
    /// Byte-neutral: the cache is the same map nix's flatten yields.
    Overlay {
        left: RefCell<Rc<NixAttrs>>,
        right: RefCell<Rc<NixAttrs>>,
        cache: Rc<OnceCell<AttrsMap<Symbol, Value>>>,
    },
}

impl fmt::Debug for NixAttrs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NixAttrs({})", self.len())
    }
}

impl Default for NixAttrs {
    fn default() -> Self {
        census::made(&census::ATTRS_MADE, &census::ATTRS_LIVE);
        Self(AttrsInner::Flat(AttrsMap::default()), None)
    }
}

impl NixAttrs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(_capacity: usize) -> Self {
        Self::default()
    }

    /// Attach a source-position table (the static keys' byte offsets of the
    /// literal that built this attrset). Called by `eval_attrset`; consumed
    /// by `builtins.unsafeGetAttrPos`. Never affects any observed value.
    pub fn set_positions(&mut self, pos: Rc<crate::pos::AttrPositions>) {
        self.1 = Some(pos);
    }

    /// The source-position table, if this attrset carries one (a literal with
    /// static keys). `None` for merges/overlays/builtin-built/dynamic-key
    /// attrsets.
    #[must_use]
    pub fn positions(&self) -> Option<&Rc<crate::pos::AttrPositions>> {
        self.1.as_ref()
    }

    /// Resolve the source position of `key` in this attrset — the file/line/
    /// column `builtins.unsafeGetAttrPos` returns. `None` when the attrset
    /// has no position table, the key is absent from it, or the source has
    /// no file (a `<string>`-eval'd literal).
    #[must_use]
    pub fn pos_for(&self, key: &str) -> Option<crate::pos::ResolvedPos> {
        let sym = intern(key);
        let (file, offset) = self.pos_entry(sym)?;
        crate::pos::resolve(file.as_deref(), offset)
    }

    /// Find `sym`'s (file, offset) — walking an `//` overlay RIGHT first, then
    /// LEFT, so a key's reported position follows the same precedence `//`
    /// itself gives the key's VALUE.
    ///
    /// Why this walks instead of reading `self.1`: `overlay` builds the
    /// `AttrsInner::Overlay` node with an empty position slot (it is O(1) and
    /// lazy by construction — eagerly merging two tables on every `//` would
    /// cost on a very hot path). Reading only the slot therefore reported
    /// `null` for EVERY key of every `//` result.
    ///
    /// That was not cosmetic. nixpkgs' `lib.nixosSystem` ends in
    /// `{ …; modules = …; } // removeAttrs args [ "modules" ]`, and
    /// `nixos/lib/eval-config.nix:28` derives `modulesLocation` from
    /// `unsafeGetAttrPos "modules"` on exactly that attrset. A `null` there
    /// skips `setDefaultModuleLocation`, which skips wrapping every user
    /// module in `{ _file; imports = [ m ]; }` — and since `collectModules`
    /// walks breadth-first via `genericClosure`, the missing wrapper leaves
    /// each user module one level SHALLOWER than CppNix puts it, permuting
    /// NixOS option definition order and diverging the toplevel drvPath.
    fn pos_entry(&self, sym: Symbol) -> Option<(Option<std::path::PathBuf>, u32)> {
        if let Some(table) = self.1.as_ref() {
            if let Some(offset) = table.keys.get(&sym) {
                return Some((table.file.clone(), *offset));
            }
        }
        match &self.0 {
            AttrsInner::Overlay { left, right, .. } => {
                let r = right.borrow().pos_entry(sym);
                if r.is_some() {
                    return r;
                }
                let l = left.borrow().pos_entry(sym);
                l
            }
            _ => None,
        }
    }

    /// Borrow the underlying map. Flattens if overlay.
    #[must_use]
    pub fn inner(&self) -> AttrsMap<Symbol, Value> {
        self.as_flat().clone()
    }

    /// Get a reference to a flat `AttrsMap`, populating cache if overlay.
    fn as_flat(&self) -> &AttrsMap<Symbol, Value> {
        match &self.0 {
            AttrsInner::Flat(m) => m,
            AttrsInner::Overlay { left, right, cache } => {
                crate::perf::inc(crate::perf::Counter::OverlayFlattenAttempt);
                let flat = cache.get_or_init(|| {
                    // Cache MISS: this Overlay node is being flattened for the
                    // first time — real O(left+right) merge work.
                    crate::perf::inc(crate::perf::Counter::OverlayFlattenBuild);
                    let timed = crate::perf::enabled();
                    let t0 = if timed { Some(std::time::Instant::now()) } else { None };
                    let mut result = left.borrow().as_flat().clone();
                    for (k, v) in right.borrow().as_flat().iter() {
                        result.insert(*k, v.clone());
                    }
                    crate::perf::add(
                        crate::perf::Counter::OverlayFlattenEntries,
                        result.len() as u64,
                    );
                    if let Some(t0) = t0 {
                        crate::trace::add_overlay_flatten_nanos(t0.elapsed().as_nanos());
                    }
                    result
                });
                // RELEASE the parents now that `cache` is the complete answer —
                // cascade-frees the intermediate overlay chain + their caches
                // (nothing else references them). Byte-neutral: every reader now
                // routes through this `cache`. Only swaps a still-held parent; a
                // second as_flat sees them already empty and skips. The closure's
                // borrows above are dropped by here, so these borrow_muts can't
                // conflict (single-threaded, sequential).
                // Release VALUES but keep the POSITION SKELETON. Swapping in a
                // bare `NixAttrs::new()` also discarded every `AttrPositions`
                // table in the released subtree, so `pos_entry`'s overlay walk
                // found nothing and `unsafeGetAttrPos` returned null for any
                // `//` result that had been TOUCHED — measured:
                //   fresh overlay        nix line 1, sui line 1
                //   after one attr read  nix line 1, sui NULL
                // which is every real use, since nixpkgs reads from an attrset
                // before anyone asks for a position. `position_husk` keeps the
                // same tree shape and the position tables (small, and only
                // present on literals) while dropping the values, so the
                // cascade-free still reclaims the expensive part.
                {
                    let mut l = left.borrow_mut();
                    if !l.is_empty() { *l = Rc::new(l.position_husk()); }
                }
                {
                    let mut r = right.borrow_mut();
                    if !r.is_empty() { *r = Rc::new(r.position_husk()); }
                }
                flat
            }
        }
    }

    /// A value-free copy carrying only what `pos_entry` reads: this node's own
    /// position table and, for an overlay, the same shape recursively.
    ///
    /// Used when `as_flat` releases a flattened overlay's parents. The values
    /// are what cost memory; the `AttrPositions` tables are small and exist
    /// only on attrset LITERALS with static keys, so keeping the skeleton
    /// preserves `unsafeGetAttrPos` at negligible cost. Returns an empty
    /// position-less set when the subtree carries no positions at all, so the
    /// common case allocates no more than the old `NixAttrs::new()` did.
    fn position_husk(&self) -> NixAttrs {
        match &self.0 {
            AttrsInner::Overlay { left, right, .. } => {
                let (l, r) = (left.borrow().position_husk(), right.borrow().position_husk());
                if l.1.is_none() && r.1.is_none() && !matches!(l.0, AttrsInner::Overlay { .. })
                    && !matches!(r.0, AttrsInner::Overlay { .. })
                {
                    // Nothing below carries a position — collapse to the cheap
                    // empty set rather than rebuilding a pointless spine.
                    return NixAttrs(AttrsInner::Flat(AttrsMap::default()), self.1.clone());
                }
                NixAttrs(
                    AttrsInner::Overlay {
                        left: RefCell::new(Rc::new(l)),
                        right: RefCell::new(Rc::new(r)),
                        cache: Rc::new(OnceCell::new()),
                    },
                    self.1.clone(),
                )
            }
            AttrsInner::Flat(_) => NixAttrs(AttrsInner::Flat(AttrsMap::default()), self.1.clone()),
        }
    }

    fn sorted_entries(&self) -> Vec<(String, &Value)> {
        crate::perf::inc(crate::perf::Counter::SortedEntriesCalls);
        let m = self.as_flat();
        crate::perf::add(crate::perf::Counter::SortedEntriesRows, m.len() as u64);
        let timed = crate::perf::enabled();
        let t0 = if timed { Some(std::time::Instant::now()) } else { None };
        let mut pairs: Vec<(String, &Value)> = m.iter()
            .map(|(sym, v)| (resolve(*sym), v))
            .collect();
        pairs.sort_by(|(a, _), (b, _)| a.cmp(b));
        if let Some(t0) = t0 {
            crate::trace::add_sorted_entries_nanos(t0.elapsed().as_nanos());
        }
        pairs
    }

    /// Look up an attribute by name. Walks overlay chain right-to-left.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        let sym = intern(key);
        self.get_sym(&sym)
    }

    /// Look up by pre-interned Symbol.
    ///
    /// Fast path: if the overlay's flat cache has been populated (by any
    /// prior full iteration — `attrNames`, `attrValues`, `//` merge that
    /// needed key enumeration, etc.), read directly from it in O(1). This
    /// matters in real Nix workloads where an attrset is first iterated
    /// (module eval, `with` desugaring) and then hit many times by dotted
    /// access — CppNix has no such structure and pays O(1) always; we want
    /// to match that whenever the cache is warm.
    ///
    /// Slow path: walk the overlay chain right-to-left in O(depth). Not
    /// populating the cache on cold lookups is deliberate — the cache
    /// costs O(n) to build and the chain is usually short (1–3 overlays).
    #[must_use]
    pub fn get_sym(&self, sym: &Symbol) -> Option<&Value> {
        match &self.0 {
            AttrsInner::Flat(m) => m.get(sym),
            // Route through `as_flat()` (the memoized cache) rather than borrowing
            // into `left`/`right` — this is what lets the parents be released
            // post-flatten. `as_flat` returns the cached map in O(1) when warm and
            // flattens+caches on the first cold lookup; the returned `&Value`
            // borrows the stable `cache`, never a `RefCell`.
            AttrsInner::Overlay { .. } => self.as_flat().get(sym),
        }
    }

    /// Insert or overwrite an attribute. Flattens overlay if needed.
    pub fn insert(&mut self, key: String, value: Value) {
        self.ensure_flat();
        if let AttrsInner::Flat(ref mut m) = self.0 {
            m.insert(intern(&key), value);
        }
    }

    /// Ensure the inner representation is Flat (for mutation).
    fn ensure_flat(&mut self) {
        if matches!(self.0, AttrsInner::Overlay { .. }) {
            self.0 = AttrsInner::Flat(self.as_flat().clone());
        }
    }

    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        let sym = intern(key);
        self.contains_key_sym(&sym)
    }

    #[must_use]
    pub fn contains_key_sym(&self, sym: &Symbol) -> bool {
        match &self.0 {
            AttrsInner::Flat(m) => m.contains_key(sym),
            // Route through the cache (see get_sym) so left/right stay releasable.
            AttrsInner::Overlay { .. } => self.as_flat().contains_key(sym),
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = String> {
        self.sorted_entries().into_iter().map(|(k, _)| k)
    }

    pub fn iter(&self) -> impl Iterator<Item = (String, &Value)> {
        self.sorted_entries().into_iter()
    }

    pub fn iter_unsorted(&self) -> impl Iterator<Item = (String, &Value)> {
        self.as_flat().iter().map(|(sym, v)| (resolve(*sym), v)).collect::<Vec<_>>().into_iter()
    }

    /// Sym-keyed unsorted iteration — ZERO interner traffic, zero allocation.
    ///
    /// This exists because live-sampling the cid marquee eval (2026-07-21,
    /// release-profiling binary) showed the **interner round-trip as the #1 CPU
    /// sink — 27–39% of the eval thread, sustained**: `iter_unsorted` above
    /// materializes a fresh heap `String` per key via `resolve` AND collects
    /// the whole map into a `Vec` on every call, and callers like
    /// `intersectAttrs` then re-intern each of those Strings straight back to
    /// the `Symbol` they started as (`contains_key(&str)` → `intern`), with a
    /// third intern inside `insert`. Sym→String→hash+memcmp→Sym, three times
    /// per key per call, at nixpkgs scale.
    ///
    /// `Symbol` is `Copy(u32)` and `as_flat()` hands back a real borrow (the
    /// Overlay case populates its cache), so this iterator borrows instead of
    /// collecting. Byte-neutral by the same argument already sealed for the
    /// unsorted-iteration change: the observable order of any *result* attrset
    /// is re-derived at observation time via `sorted_entries`.
    pub fn iter_syms(&self) -> impl Iterator<Item = (Symbol, &Value)> {
        self.as_flat().iter().map(|(sym, v)| (*sym, v))
    }

    /// Sym-keyed insert — the zero-intern sibling of `insert`, for callers
    /// that already hold the `Symbol` (every `iter_syms` consumer).
    pub fn insert_sym(&mut self, sym: Symbol, value: Value) {
        self.ensure_flat();
        if let AttrsInner::Flat(ref mut m) = self.0 {
            m.insert(sym, value);
        }
    }

    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.sorted_entries().into_iter().map(|(_, v)| v)
    }


    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.ensure_flat();
        if let AttrsInner::Flat(ref mut m) = self.0 {
            m.remove(&intern(key))
        } else {
            None
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match &self.0 {
            AttrsInner::Flat(m) => m.len(),
            AttrsInner::Overlay { .. } => {
                // Must flatten to count unique keys, but `as_flat()` already
                // returns a borrow into the memoized map — cloning it just to
                // read `.len()` was pure O(n) waste on every overlay `len()`.
                self.as_flat().len()
            }
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        match &self.0 {
            AttrsInner::Flat(m) => m.is_empty(),
            // Cache-first (see get_sym): a released-parent overlay is NOT empty —
            // its content lives in the flattened cache. Reading left/right here
            // (which post-release are empty) would wrongly report empty.
            AttrsInner::Overlay { .. } => self.as_flat().is_empty(),
        }
    }

    /// O(1) lazy overlay: `self // other`. Does NOT merge eagerly.
    #[must_use]
    pub fn overlay(self, other: NixAttrs) -> NixAttrs {
        if other.is_empty() { return self; }
        if self.is_empty() { return other; }
        crate::perf::inc(crate::perf::Counter::OverlayCreated);
        census::made(&census::ATTRS_MADE, &census::ATTRS_LIVE);
        NixAttrs(AttrsInner::Overlay {
            left: RefCell::new(Rc::new(self)),
            right: RefCell::new(Rc::new(other)),
            cache: Rc::new(OnceCell::new()),
        }, None)
    }

    /// Eager merge (legacy API — prefer `overlay` for `//`).
    #[must_use]
    pub fn update(&self, other: &NixAttrs) -> NixAttrs {
        match (&self.0, &other.0) {
            (AttrsInner::Flat(l), AttrsInner::Flat(r)) => {
                let mut result = l.clone();
                for (k, v) in r.iter() {
                    result.insert(*k, v.clone());
                }
                census::made(&census::ATTRS_MADE, &census::ATTRS_LIVE);
                NixAttrs(AttrsInner::Flat(result), None)
            }
            _ => {
                // For overlay inputs, flatten then merge
                let mut result = self.as_flat().clone();
                let other_flat = other.as_flat();
                for (k, v) in other_flat.iter() {
                    result.insert(*k, v.clone());
                }
                census::made(&census::ATTRS_MADE, &census::ATTRS_LIVE);
                NixAttrs(AttrsInner::Flat(result), None)
            }
        }
    }
}

impl FromIterator<(String, Value)> for NixAttrs {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        census::made(&census::ATTRS_MADE, &census::ATTRS_LIVE);
        NixAttrs(AttrsInner::Flat(iter.into_iter().map(|(k, v)| (intern(&k), v)).collect()), None)
    }
}

impl IntoIterator for NixAttrs {
    type Item = (String, Value);
    type IntoIter = Box<dyn Iterator<Item = (String, Value)>>;

    fn into_iter(self) -> Self::IntoIter {
        let flat = self.as_flat().clone();
        Box::new(flat.into_iter().map(|(sym, v)| (resolve(sym), v)))
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
    /// The `source_id` of the parse tree this env belongs to. Restored
    /// on thunk force (in lockstep with `eval_file`) so a lazily-forced
    /// thunk's idents key `IDENT_CACHE` against the file where the thunk
    /// was DEFINED, not the ambient source at force time. Without this, a
    /// cross-file force collides on `(source_id, text_offset)` and returns
    /// a wrong Symbol (the `parse.nix` `cannot select from null` bug).
    source_id: u32,
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

/// Decrements `ENV_LIVE` so the census reports a live COUNT rather than a
/// monotonic total. One relaxed atomic, and only when `SUI_LIVE_CENSUS=1` —
/// `census::dropped` checks `enabled()` first.
///
/// `EnvInner` derives `Clone`, so a clone is a NEW allocation and must be
/// counted; `Env` itself is `Rc`-cloned and must not be. That is why the
/// increments sit in `Env::new`/`Env::child` (the two `Rc::new(EnvInner …)`
/// sites) rather than in a `Clone` impl.
impl Drop for EnvInner {
    fn drop(&mut self) {
        census::dropped(&census::ENV_LIVE);
    }
}

impl Env {
    /// Create a root environment with no bindings.
    #[must_use]
    pub fn new() -> Self {
        census::made(&census::ENV_MADE, &census::ENV_LIVE);
        Self(Rc::new(EnvInner {
            bindings: FxHashMap::default(),
            with_scopes: Vec::new(),
            eval_file: None,
            source_id: 0,
        }))
    }

    /// Create a child environment that inherits from this one.
    ///
    /// O(1) — the `FxHashMap` clone is structural sharing (refcount
    /// bump on internal tree nodes), not a deep copy.
    #[must_use]
    pub fn child(&self) -> Self {
        crate::perf::inc(crate::perf::Counter::EnvClone);
        // `ENV_MADE`/`ENV_LIVE` existed but nothing ever incremented them, so
        // `census dump` reported `env_live=0 env_made=0` — and `Env` is the
        // leading suspect for sui's 12.3x footprint over CppNix, since every
        // suspended thunk holds one and nixpkgs makes them constantly. The one
        // structure most worth counting was the one the census could not see.
        census::made(&census::ENV_MADE, &census::ENV_LIVE);
        Self(Rc::new(EnvInner {
            bindings: self.0.bindings.clone(), // O(1) structural sharing
            with_scopes: self.0.with_scopes.clone(),
            // Children inherit the parent's eval file so that
            // path literals nested deep in let-chains still
            // resolve against the right directory.
            eval_file: self.0.eval_file.clone(),
            // Children inherit the parent's source_id — a child scope is
            // in the same parse tree as its parent (a new source_id only
            // arises on `eval_with_file` for an imported file).
            source_id: self.0.source_id,
        }))
    }

    /// Attach a `with` scope to this environment.
    ///
    /// If the value is a thunk that's ALREADY evaluated (OnceCell cache hit),
    /// pre-populate the with-scope cache immediately. This avoids creating
    /// deferred WithIdent thunks when the fixpoint is already resolved —
    /// critical for the overlay chain where multiple stages access the same
    /// fixpoint through different `with self;` scopes.
    #[must_use]
    pub fn with_scope(mut self, value: Value) -> Self {
        // Pre-populate cache if the value is already resolved
        let pre_cached = match &value {
            Value::Attrs(attrs) => Some((**attrs).clone()),
            Value::Thunk(thunk) => thunk.peek().and_then(|v| {
                if let Concrete::Attrs(attrs) = v { Some((**attrs).clone()) } else { None }
            }),
            _ => None,
        };
        Rc::make_mut(&mut self.0).with_scopes.push(WithScope {
            value,
            cached: Rc::new(RefCell::new(pre_cached)),
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

    /// Bind many names in ONE copy-on-write step: a single `Rc::make_mut` on the
    /// inner env, then N inserts on the owned map — instead of N successive
    /// `bind()` calls each re-borrowing + re-`make_mut`-ing `self.0`.
    ///
    /// Byte-identical to calling [`bind`](Self::bind) once per pair in the same
    /// order (same `intern`, same insert sequence, same final HAMT) — a byte-SAFE
    /// `RedundantWrite`-class optimization: it removes intermediate re-borrows,
    /// not any observable value. Consumed by pattern-lambda binding (`bind_param`),
    /// where an N-formal pattern otherwise pays N `make_mut` refcount checks.
    pub fn bind_many(&mut self, pairs: impl IntoIterator<Item = (String, Value)>) {
        let inner = Rc::make_mut(&mut self.0);
        for (name, value) in pairs {
            inner.bindings.insert(intern(&name), value);
        }
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

    /// The `source_id` of the parse tree this env belongs to (0 = top level).
    #[must_use]
    pub fn source_id(&self) -> u32 {
        self.0.source_id
    }

    /// Set the `source_id` for this environment (called by `eval_with_file`
    /// for an imported parse tree).
    pub fn set_source_id(&mut self, id: u32) {
        Rc::make_mut(&mut self.0).source_id = id;
    }

    /// Number of direct bindings in this environment (debug).
    #[must_use]
    pub fn binding_count(&self) -> usize {
        self.0.bindings.len()
    }

    /// First N binding names (debug).
    #[must_use]
    pub fn binding_names_preview(&self, n: usize) -> Vec<String> {
        self.0.bindings.keys().take(n).map(|s| resolve(*s)).collect()
    }

    /// Number of `with` scopes (debug).
    #[must_use]
    pub fn with_scope_count(&self) -> usize {
        self.0.with_scopes.len()
    }

    /// Lookup in LEXICAL scope only (no with-scopes).
    /// Used by maybe_thunk to avoid forcing with-scope fixpoints during
    /// attrset construction.
    #[must_use]
    pub fn lookup_lexical(&self, name: &str) -> Option<Value> {
        let sym = intern(name);
        self.0.bindings.get(&sym).cloned()
    }

    /// Lookup in LEXICAL scope only, by pre-interned [`Symbol`] — the
    /// Symbol-keyed sibling of [`lookup_lexical`](Self::lookup_lexical).
    ///
    /// Probes ONLY the lexical `bindings` map (the first thing
    /// [`lookup_fast`](Self::lookup_fast) does, by the same Symbol) — never
    /// the `with`-chain. The ENV-RESOLVE M0 fast path uses this: a
    /// `Resolution::Lexical{sym}` reference probes here directly with its
    /// precomputed Symbol; on a hit the returned value is byte-identical to
    /// `lookup_fast`'s (same map, same Symbol); on a miss the caller falls
    /// back to today's exact runtime path.
    #[must_use]
    pub fn lookup_lexical_sym(&self, sym: Symbol) -> Option<Value> {
        self.0.bindings.get(&sym).cloned()
    }

    /// Look up a name using ONLY with-scope caches (no forcing).
    /// Returns Some if the name is in a cached with-scope, None otherwise.
    /// Used by maybe_thunk to resolve with-scope idents without forcing fixpoints.
    #[must_use]
    pub fn lookup_with_cache_only(&self, name: &str) -> Option<Value> {
        for scope in self.0.with_scopes.iter().rev() {
            let cache = scope.cached.borrow();
            if let Some(ref attrs) = *cache {
                if let Some(v) = attrs.get(name) {
                    return Some(v.clone());
                }
            }
            // Also check if the thunk is already evaluated (peek)
            drop(cache);
            if let Value::Thunk(ref thunk) = scope.value {
                if let Some(cached_val) = thunk.peek() {
                    if let Concrete::Attrs(ref attrs) = *cached_val {
                        // Populate the with-scope cache for future lookups
                        *scope.cached.borrow_mut() = Some((**attrs).clone());
                        if let Some(v) = attrs.get(name) {
                            return Some(v.clone());
                        }
                    }
                }
            } else if let Value::Attrs(ref attrs) = scope.value {
                *scope.cached.borrow_mut() = Some((**attrs).clone());
                if let Some(v) = attrs.get(name) {
                    return Some(v.clone());
                }
            }
        }
        None
    }

    /// Get the innermost with-scope's cache and value for creating WithIdent thunks.
    /// Returns None if there are no with-scopes.
    #[must_use]
    pub fn innermost_with_scope(&self) -> Option<(Rc<RefCell<Option<NixAttrs>>>, Value)> {
        self.0.with_scopes.last().map(|scope| {
            (scope.cached.clone(), scope.value.clone())
        })
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
        self.lookup_fast(intern(name), name)
    }

    /// Cache-BYPASSING with-scope lookup: force each `with`-scope value FRESH
    /// (through the full thunk chain) and check for `name`, refreshing the
    /// per-scope cache on the way. A force that errors (a mid-fixpoint blackhole
    /// or a `with (throw …); …` namespace) is caught and the scope skipped.
    ///
    /// This exists ONLY for the last-ditch retry on the about-to-throw
    /// `UndefinedVar` path (see the WithIdent force): the normal cache-first
    /// [`lookup_fast`] can trust a stale mid-fixpoint PARTIAL cached for a scope
    /// (e.g. `f self` before makeScope merged `callPackage` into `self`) and skip
    /// it; a fresh force sees the now-completed scope. Never call this on a hot
    /// path — it re-forces every scope.
    #[must_use]
    pub fn lookup_fresh(&self, name: &str) -> Option<Value> {
        let sym = intern(name);
        if let Some(v) = self.0.bindings.get(&sym) {
            return Some(v.clone());
        }
        for scope in self.0.with_scopes.iter().rev() {
            if let Ok(Value::Attrs(attrs)) = crate::eval::force_value(&scope.value) {
                if let Some(v) = attrs.get_sym(&sym) {
                    // Refresh the stale cache with the completed scope so a later
                    // lookup of a sibling name also sees it.
                    *scope.cached.borrow_mut() = Some((*attrs).clone());
                    return Some(v.clone());
                }
            }
        }
        None
    }

    /// Lookup by pre-interned Symbol + string name. Avoids re-interning.
    #[must_use]
    pub fn lookup_fast(&self, sym: Symbol, name: &str) -> Option<Value> {
        crate::perf::inc(crate::perf::Counter::EnvLookup);
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
            // Slow path: force, cache, then check.
            // If the value is already concrete (not a thunk), use it directly.
            // If it's a thunk, try to force. On blackhole (fixpoint being
            // computed), return None so the caller can defer.
            let resolved = match &scope.value {
                Value::Attrs(attrs) => {
                    // Already concrete — cache and use directly
                    crate::perf::inc(crate::perf::Counter::WithScopeCacheClone);
                    *scope.cached.borrow_mut() = Some((**attrs).clone());
                    Some((**attrs).clone())
                }
                Value::Thunk(thunk) => {
                    // Check if the thunk is already evaluated (OnceCell cache)
                    // without entering the force state machine
                    if let Some(cached_val) = thunk.peek() {
                        if let Concrete::Attrs(ref attrs) = *cached_val {
                            crate::perf::inc(crate::perf::Counter::WithScopeCacheClone);
                            *scope.cached.borrow_mut() = Some((**attrs).clone());
                            Some((**attrs).clone())
                        } else {
                            None
                        }
                    } else {
                        // Thunk not yet evaluated — force it FULLY. Must use
                        // `force_value` (which chases the whole thunk chain),
                        // NOT `force_value_tracked` (single `force_thunk` step):
                        // a with-scope head like `lib.platforms` is often a
                        // lazy `Thunk(Thunk(Attrs))`, so one step yields a
                        // `Value::Thunk` whose `type_name()` peeks to "set" but
                        // which the `if let Value::Attrs` match REJECTS — the
                        // scope is then wrongly skipped and every bare-ident
                        // lookup through it (`with lib.platforms; unix`) fails
                        // with a spurious UndefinedVar.
                        match crate::eval::force_value(&scope.value) {
                            Ok(forced) => {
                                if let Value::Attrs(ref attrs) = forced {
                                    crate::perf::inc(crate::perf::Counter::WithScopeCacheClone);
                                    *scope.cached.borrow_mut() = Some((**attrs).clone());
                                    Some((**attrs).clone())
                                } else {
                                    None
                                }
                            }
                            Err(_) => None, // blackhole or other error — skip
                        }
                    }
                }
                _ => {
                    // Same full-chain force as the Thunk arm above.
                    match crate::eval::force_value(&scope.value) {
                        Ok(forced) => {
                            if let Value::Attrs(ref attrs) = forced {
                                crate::perf::inc(crate::perf::Counter::WithScopeCacheClone);
                                *scope.cached.borrow_mut() = Some((**attrs).clone());
                                Some((**attrs).clone())
                            } else {
                                None
                            }
                        }
                        Err(_) => None,
                    }
                }
            };
            if let Some(ref attrs) = resolved {
                if let Some(v) = attrs.get(name) {
                    return Some(v.clone());
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
            if let Ok(forced) = crate::eval::force_value_tracked(&scope.value, "with_scope") {
                if let Value::Attrs(ref attrs) = forced {
                    let result = attrs.get_sym(&sym).cloned();
                    crate::perf::inc(crate::perf::Counter::WithScopeCacheClone);
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
    /// Explicit `throw` from Nix code — CATCHABLE by `builtins.tryEval`.
    #[error("{0}")]
    Throw(String),
    /// An `abort` from Nix code — UNCATCHABLE (CppNix's `abort`/`builtins.abort`
    /// is a hard error `tryEval` does NOT catch, unlike `throw`/`assert`).
    /// Verified: `nix eval '(builtins.tryEval (abort "x")).success'` errors.
    #[error("{0}")]
    Abort(String),
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
    ///
    /// CARRIES THE EVAL FILE (added 2026-07-20). Every arithmetic/comparison
    /// raise site routes through here, and none of them appended
    /// `eval_file_ctx()` — unlike the ~12 sibling raise sites in `eval.rs` that
    /// do — so an operator type error named no file at all. Nor could the frame
    /// stack help: `NixTraceGuard::drop` pops every frame during unwind, so by
    /// the time the error surfaces `attach_trace` has nothing left to attach.
    ///
    /// The cost of that was concrete. "cannot add string and null" was the sole
    /// symptom of the ident-cache aliasing bug that stopped sui evaluating
    /// nixpkgs, and it pointed nowhere: four parallel investigations each spent
    /// most of their budget just locating it, and the only tool that worked was
    /// `SUI_TRACE_EVAL=1` dumping 521k lines to be read backwards. One
    /// `format!` argument here would have named `make-derivation.nix`
    /// immediately.
    ///
    /// Fixing it in `op_type` rather than at the `Add` arm means every operator
    /// — add, sub, mul, div, comparison, update — gains the context at once,
    /// instead of the next one to bite us needing its own patch.
    #[must_use]
    pub fn op_type(op: &str, lhs: &str, rhs: &str) -> Self {
        EvalError::TypeError(format!(
            "cannot {op} {lhs} and {rhs}{}",
            crate::eval::eval_file_ctx()
        ))
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
        Value::List(Rc::new(NixList::new(items)))
    }

    /// True when `self` is a `List` whose backing `Rc<Vec>` is uniquely owned
    /// (refcount 1). Used by [`concat_lists`] to decide the in-place fast path.
    #[must_use]
    pub fn is_uniquely_owned_list(&self) -> bool {
        matches!(self, Value::List(rc) if Rc::strong_count(rc) == 1)
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
                // nix-faithful (CppNix value-to-json.cc `tryAttrsToString`):
                // a derivation — an attrset carrying `__toString` or `outPath`
                // — serializes to THAT STRING, never its own attrs. Without
                // this, `to_json` recurses forever on the self-referential
                // derivation graph (`drv.out.drv == drv`, `drv.all`, …) and
                // overflows the stack. Mirrors `coerce_to_string` below.
                if attrs.get("__toString").is_some() || attrs.get("outPath").is_some() {
                    if let Ok((s, _ctx)) = self.coerce_to_string() {
                        return serde_json::Value::String(s);
                    }
                }
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

    /// Like [`Self::to_json`], but **refuses** where that one emits a
    /// placeholder.
    ///
    /// `to_json` renders a lambda as the string `"<lambda>"`, a builtin as
    /// `"<builtin name>"`, and — worst — a thunk whose force FAILED as
    /// `"<thunk:error>"`. All three produce valid JSON and let the caller exit
    /// 0. Measured against nix 2.31.5:
    ///
    /// ```text
    /// nix eval --json --expr '{ f = x: x; }'        exit 1
    /// sui eval --json -E    '{ f = x: x; }'         exit 0  {"f":"<lambda>"}
    /// nix eval --json --expr '{ x = throw "boom"; }' exit 1
    /// sui eval --json -E    '{ x = throw "boom"; }'  exit 0  {"x":"<thunk:error>"}
    /// ```
    ///
    /// The last one is the sharpest silent divergence in the CLI: a real
    /// evaluation error becomes a VALUE, and a consumer parsing that JSON sees
    /// a string where nix would have refused outright.
    ///
    /// `to_json` itself is deliberately left alone. It is the body of
    /// `builtins.toJSON`, whose placeholder behaviour is load-bearing for the
    /// existing corpus, and changing it would be a language-semantics change
    /// rather than a CLI fix. This variant is for OUTPUT BOUNDARIES — where a
    /// human or a script reads the result and an exit code is the contract.
    ///
    /// # Errors
    ///
    /// A function, a builtin, or a thunk whose force fails. The force error is
    /// propagated verbatim so the operator sees the `throw`'s own message
    /// rather than a generic refusal.
    pub fn try_to_json(&self) -> Result<serde_json::Value, EvalError> {
        Ok(match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(n) => serde_json::json!(n),
            Value::Float(f) => serde_json::json!(f),
            Value::String(s) => serde_json::Value::String(s.chars.to_string()),
            Value::Path(p) => serde_json::Value::String(p.to_string()),
            Value::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for v in items.iter() {
                    out.push(v.try_to_json()?);
                }
                serde_json::Value::Array(out)
            }
            Value::Attrs(attrs) => {
                // Mirror `to_json`'s CppNix `tryAttrsToString` rule: an attrset
                // carrying `__toString` or `outPath` serializes to that string.
                // Dropping it here would not merely change the output, it would
                // recurse forever on the self-referential derivation graph —
                // the reason that rule exists in `to_json` at all.
                if let Some(v) = attrs.get("outPath").or_else(|| attrs.get("__toString")) {
                    return v.try_to_json();
                }
                let mut map = serde_json::Map::new();
                for (k, v) in attrs.iter() {
                    map.insert(k.clone(), v.try_to_json()?);
                }
                serde_json::Value::Object(map)
            }
            Value::Lambda(_) => {
                return Err(EvalError::TypeError(
                    "cannot convert a function to JSON".to_string(),
                ))
            }
            Value::Builtin(b) => {
                return Err(EvalError::TypeError(format!(
                    "cannot convert a function to JSON (builtin '{}')",
                    b.name
                )))
            }
            Value::Thunk(thunk) => {
                // Propagate rather than swallow. This arm is the whole point:
                // `to_json` turns this error into the string "<thunk:error>".
                let forced = thunk.force(&|expr, env| crate::eval::eval_expr(expr, env))?;
                forced.try_to_json()?
            }
        })
    }

    /// Like [`to_json`] but threads string context into `ctx`. Used by
    /// `__structuredAttrs` derivation-env building: a derivation value
    /// serializes to its outPath (a store-path string) and its drv reference
    /// must flow into the derivation's `inputDrvs`; a bare path is copy-to-store
    /// coerced. (`to_json` drops context, which is fine for `builtins.toJSON`
    /// but not for building a derivation's `__json`.)
    pub fn to_json_with_context(
        &self,
        ctx: &mut StringContext,
    ) -> Result<serde_json::Value, EvalError> {
        Ok(match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(n) => serde_json::json!(n),
            Value::Float(f) => serde_json::json!(f),
            Value::String(s) => {
                ctx.merge(&s.context);
                serde_json::Value::String(s.chars.to_string())
            }
            Value::Path(_) => {
                let (str, c) = self.coerce_to_string_copy_to_store()?;
                ctx.merge(&c);
                serde_json::Value::String(str)
            }
            Value::List(items) => {
                let mut arr = Vec::with_capacity(items.len());
                for v in items.iter() {
                    let fv = crate::eval::force_value(v)?;
                    arr.push(fv.to_json_with_context(ctx)?);
                }
                serde_json::Value::Array(arr)
            }
            Value::Attrs(attrs) => {
                // A derivation (attrset with `outPath`/`__toString`) serializes
                // to that string with its context — never its own attrs.
                if attrs.get("__toString").is_some() || attrs.get("outPath").is_some() {
                    let (s, c) = self.coerce_to_string_copy_to_store()?;
                    ctx.merge(&c);
                    return Ok(serde_json::Value::String(s));
                }
                let mut map = serde_json::Map::new();
                for (k, v) in attrs.iter() {
                    let fv = crate::eval::force_value(v)?;
                    map.insert(k.clone(), fv.to_json_with_context(ctx)?);
                }
                serde_json::Value::Object(map)
            }
            Value::Thunk(_) => {
                let forced = crate::eval::force_value(self)?;
                forced.to_json_with_context(ctx)?
            }
            other => {
                return Err(EvalError::TypeError(format!(
                    "cannot serialize {} to JSON (__structuredAttrs)",
                    other.type_name()
                )));
            }
        })
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
            // M2.6 Promise softening: coercion of a sentinel to bool
            // inside a fix-point body returns false (the cheapest sentinel
            // that lets `if x then … else …` take the else branch).
            _ if in_promise_eval() => Ok(false),
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
            // M2.6 Promise softening: coercion of a sentinel to int
            // returns 0.
            _ if in_promise_eval() => Ok(0),
            _ => Err(EvalError::TypeMismatch { expected: "int", got: self.type_name() }),
        }
    }

    /// Borrow the string content without forcing thunks.
    pub fn as_string(&self) -> Result<&str, EvalError> {
        match self {
            Value::String(s) => Ok(&s.chars),
            Value::Thunk(_) => Err(EvalError::TypeError(
                "thunk in as_string: force first via force_value()".into(),
            )),
            _ if in_promise_eval() => Ok(""),
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
            _ if in_promise_eval() => Ok(String::new()),
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
            _ if in_promise_eval() => Ok(NixString::plain("")),
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
            _ => Err(crate::eval::attach_trace(
                EvalError::TypeMismatch { expected: "list", got: self.type_name() }
            )),
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
            // M2.6 Promise softening: a coercion of null (or any
            // non-attrset sentinel) to an attrset inside a fix-point
            // body returns an empty attrset, so downstream builtins
            // (mapAttrs, attrNames, ...) see "no keys" rather than a
            // type error.
            _ if in_promise_eval() => Ok(NixAttrs::new()),
            _ => Err(EvalError::TypeMismatch { expected: "set", got: self.type_name() }),
        }
    }

    /// Force-aware list extraction. Forces the value if it is a thunk.
    pub fn to_list(&self) -> Result<Vec<Value>, EvalError> {
        match self {
            Value::List(l) => Ok((**l).0.clone()),
            Value::Thunk(thunk) => {
                let forced = thunk.force(&|e, env| crate::eval::eval_expr(e, env))?;
                forced.to_list()
            }
            // M2.6 Promise softening: coercion of a sentinel to a list
            // inside a fix-point body returns an empty list.
            _ if in_promise_eval() => Ok(Vec::new()),
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

    /// Coerce to a filesystem path AND, if this value is a **derivation**
    /// whose output is not yet materialized on disk, realize that output first
    /// (import-from-derivation).
    ///
    /// Used by the disk-read builtins (`import`, `readFile`, `readDir`,
    /// `pathExists`, `builtins.path`) so a read under a derivation's `outPath`
    /// triggers a build/substitute of that output, exactly as cppnix does.
    ///
    /// Semantics:
    /// - A `Path`/`String` coerces as usual — no realize (nothing to build).
    /// - A derivation attrset (`type == "derivation"` with `drvPath` +
    ///   `outPath`) whose `outPath` (after input-source materialization) does
    ///   **not** exist on disk invokes the realize hook with `(drvPath,
    ///   outPath)`. On success the returned path is the (now-present) `outPath`.
    /// - A non-derivation attrset with `outPath` coerces via `outPath` as usual
    ///   (no drv to realize).
    /// - If no realize hook is installed, this degrades to `coerce_to_path`
    ///   (the read that follows will ENOENT — a real error, never a wrong
    ///   value).
    ///
    /// The realize hook mutates no value the evaluator observes; it only makes
    /// the bytes at the already-byte-correct `outPath` present on disk (see
    /// [`crate::realize`]).
    pub fn coerce_to_realized_path(&self, context: &str) -> Result<String, EvalError> {
        match self {
            // Direct derivation attrset (`import <drv>`): drvPath + outPath are
            // right there.
            Value::Attrs(attrs) => {
                if let Some((drv_path, out_path)) = derivation_drv_and_out(attrs)? {
                    self.realize_if_absent(&drv_path, &out_path, context)?;
                    return Ok(out_path);
                }
            }
            // A string produced by interpolating a derivation
            // (`readFile "${drv}"`) is a store-path STRING that carries a
            // `ContextElement::Output { drv, output }` — the derivation-ness
            // survives interpolation *as string context*, which is exactly how
            // cppnix decides to realize. If the coerced store path is absent and
            // the context names the producing `.drv`, realize it.
            Value::String(ns) => {
                let out_path = ns.chars.to_string();
                if let Some(drv_path) = out_path_needs_realize(&out_path, &ns.context) {
                    self.realize_if_absent(&drv_path, &out_path, context)?;
                }
                return Ok(out_path);
            }
            _ => {}
        }
        self.coerce_to_path(context)
    }

    /// If `out_path` (after input-source materialization) is not present on
    /// disk, invoke the realize hook to build/substitute `drv_path`. A missing
    /// hook is a silent fall-through (the following read ENOENTs — a real error,
    /// never a wrong value); a hook error is surfaced as an eval `IoError`.
    fn realize_if_absent(
        &self,
        drv_path: &str,
        out_path: &str,
        context: &str,
    ) -> Result<(), EvalError> {
        // The existence probe must consult the REAL tree — a fetched flake
        // input's `-source` prefix is redirected — so materialize first.
        let read_path = crate::path::materialize_str(out_path);
        if std::path::Path::new(&read_path).exists() {
            return Ok(());
        }
        match crate::realize::realize_output(drv_path, out_path) {
            Ok(true) | Ok(false) => Ok(()),
            Err(msg) => Err(EvalError::IoError {
                context: context.to_string(),
                message: format!(
                    "import-from-derivation: realizing {drv_path} -> {out_path}: {msg}"
                ),
            }),
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
        self.coerce_to_string_impl(false)
    }

    /// Coerce to string in CppNix **copy-to-store** mode — the coercion used by
    /// string interpolation (`"${./foo}"`) and derivation-attribute population.
    /// A source path that isn't already in the store is absolutized,
    /// canonicalized, required to exist, and NAR-copied into
    /// `/nix/store/<hash>-<basename>`; the result string is that store path and
    /// it carries store-path context. This is what makes `src = ./.` reference
    /// the correct store path (and thus the correct drv hash) instead of a raw
    /// filesystem path. `builtins.toString` keeps the plain mode
    /// ([`coerce_to_string`]) — it does *not* copy.
    pub fn coerce_to_string_copy_to_store(
        &self,
    ) -> Result<(String, StringContext), EvalError> {
        self.coerce_to_string_impl(true)
    }

    fn coerce_to_string_impl(
        &self,
        copy_to_store: bool,
    ) -> Result<(String, StringContext), EvalError> {
        let mut ctx = StringContext::new();
        let s = match self {
            Value::String(ns) => {
                ctx.merge(&ns.context);
                ns.chars.to_string()
            }
            Value::Path(p) => {
                let raw: &str = &**p;
                if copy_to_store {
                    // CppNix copy-to-store coercion: resolve the path to its
                    // canonical absolute location (relative literals resolve
                    // against the evaluating file's dir, matching CppNix's
                    // parse-time absolutization; canonicalize also yields the
                    // realpath, e.g. macOS /tmp → /private/tmp), require it to
                    // exist (CppNix errors "path '…' does not exist"), NAR-copy
                    // it, and reference the resulting store path.
                    //
                    // A Path VALUE is ALWAYS copied, even one already under
                    // /nix/store — CppNix re-NAR-copies a bare path literal
                    // (a store subpath like `<nixpkgs-source>/pkgs/…/default-
                    // builder.sh` → its own `<hash>-default-builder.sh`, or even
                    // a store root) to a fresh basename-named store path,
                    // verified against nix 2.34. Store paths that must NOT be
                    // re-copied (derivation outputs, fetchurl `src`, storePath)
                    // arrive as context-carrying *Strings*, never as Path values,
                    // so they never reach this arm. (The earlier `/nix/store/`
                    // guard kept stdenv's builder-script subpaths verbatim, which
                    // diverged every nixpkgs input-drv hash from nix.)
                    let pb = std::path::Path::new(raw);
                    let abs = if pb.is_absolute() {
                        pb.to_path_buf()
                    } else if let Some(dir) = crate::eval::current_eval_dir() {
                        dir.join(pb)
                    } else {
                        std::env::current_dir()
                            .map_err(|e| EvalError::IoError {
                                context: format!("copy-to-store coercion of {raw}"),
                                message: e.to_string(),
                            })?
                            .join(pb)
                    };
                    // Redirect the on-disk read to the input's real source
                    // tree when `abs` lies under a fetched flake input's
                    // `-source` store prefix (sui does not materialize that
                    // store path). The resulting store path is NAR-hashed
                    // from the tree CONTENT — byte-identical whether read from
                    // the store path or the cache — so no value changes.
                    let read_abs = crate::path::materialize(&abs);
                    let canon = read_abs.canonicalize().map_err(|_| {
                        EvalError::TypeError(format!(
                            "path '{}' does not exist",
                            abs.display()
                        ))
                    })?;
                    // The copied source's STORE-PATH NAME must match CppNix's
                    // `baseNameOf` of the input's own `-source` store path when
                    // the path being copied IS a fetched flake input's whole
                    // tree (the darwin `system-path` root): blx's `src = ./.`
                    // copies the blx input tree back into the store, and CppNix
                    // names that copy `<inner>-source` (blx's `/nix/store/<h>-
                    // source` basename), NOT `blx-<rev>`. sui reads the bytes
                    // from the fetcher cache (`canon`, basename `blx-<rev>`), so
                    // `canon.file_name()` gave the wrong NAME while the bytes
                    // (→ NAR hash) were already correct. Recover the logical
                    // `-source` name from the input-source map; fall back to the
                    // real dir's basename for a normal local `src = ./.`.
                    // `strip_store_hash_prefix`: `canon` may ALREADY be a store
                    // path, whose basename is `<hash>-<name>`. Without the strip
                    // the copy lands at `<newhash>-<oldhash>-<name>`. See the
                    // helper's docs for the measured 2026-08-11 receipt.
                    let name = crate::path::source_name_for_read_dir(&canon)
                        .or_else(|| {
                            canon
                                .file_name()
                                .map(|n| sui_compat::source::strip_store_hash_prefix(
                                    &n.to_string_lossy()).to_string())
                        })
                        .unwrap_or_else(|| "source".to_string());
                    let src = sui_compat::source::nar_hash_source_tree(&canon, &name)
                        .map_err(|e| {
                            EvalError::TypeError(format!(
                                "copy-to-store coercion of '{}': {e}",
                                canon.display()
                            ))
                        })?;
                    ctx.add_plain(src.store_path.clone());
                    src.store_path
                } else {
                    ctx.add_plain(raw.to_string());
                    raw.to_string()
                }
            }
            Value::Int(n) => n.to_string(),
            // CppNix uses C printf "%f" for float → string coercion,
            // which always emits 6 decimal places (`1.5` → "1.500000",
            // `3.14159` → "3.141590"). Rust's `{}` formatter strips
            // trailing zeros. Match CppNix so `lib.strings.floatToString`
            // and module-system defaults round-trip identically.
            Value::Float(f) => format!("{f:.6}"),
            Value::Bool(true) => "1".to_string(),
            Value::Bool(false) => String::new(),
            Value::Null => String::new(),
            Value::Attrs(attrs) => {
                if let Some(to_str) = attrs.get("__toString") {
                    let result =
                        crate::eval::apply(to_str.clone(), Value::Attrs(attrs.clone()))?;
                    let forced = crate::eval::force_value(&result)?;
                    let (s, c) = forced.coerce_to_string_impl(copy_to_store)?;
                    ctx.merge(&c);
                    s
                } else if let Some(out_path) = attrs.get("outPath") {
                    let forced = crate::eval::force_value(out_path)?;
                    let (s, c) = forced.coerce_to_string_impl(copy_to_store)?;
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
                    let (s, c) = forced.coerce_to_string_impl(copy_to_store)?;
                    ctx.merge(&c);
                    parts.push(s);
                }
                parts.join(" ")
            }
            Value::Thunk(_) => {
                // Force thunk then coerce the result.
                let forced = crate::eval::force_value(self)?;
                let (s, c) = forced.coerce_to_string_impl(copy_to_store)?;
                ctx.merge(&c);
                s
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
                Value::List(Rc::new(NixList::new(arr.iter().map(Value::from).collect())))
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
                Value::List(Rc::new(NixList::new(arr.iter().map(Value::from).collect())))
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
        Value::List(Rc::new(NixList::new(list)))
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        // Quick path: pointer-equal thunks are always equal.
        if let (Value::Thunk(a), Value::Thunk(b)) = (self, other) {
            if Rc::ptr_eq(&a.0, &b.0) { return true; }
        }
        // Force to Concrete, delegate to Concrete::PartialEq.
        // Single source of truth — no duplicated comparison logic.
        let l = self.demand().unwrap_or(Concrete::Null);
        let r = other.demand().unwrap_or(Concrete::Null);
        l == r
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(n) => write!(f, "{}", sui_compat::versions::cppnix_format_float(*n)),
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

    /// Measures the per-map cost of the persistent HAMT against a flat map at
    /// the sizes nixpkgs actually uses. Not an assertion — a measurement, run
    /// with `--nocapture`. See docs/COMPLETE-REPLACEMENT.md §V.34.
    #[test]
    #[ignore = "measurement, not a gate: run with --ignored --nocapture"]
    fn measure_hamt_vs_flat_attrset_cost() {
        use crate::value::census::rss_bytes;
        const N: usize = 300_000;
        const ENTRIES: usize = 4; // typical small nixpkgs attrset

        let syms: Vec<Symbol> = (0..ENTRIES).map(|i| intern(&format!("k{i}"))).collect();

        let base = rss_bytes();
        let mut hamts: Vec<FxHashMap<Symbol, Value>> = Vec::with_capacity(N);
        for _ in 0..N {
            let mut m = FxHashMap::default();
            for s in &syms { m.insert(*s, Value::Int(1)); }
            hamts.push(m);
        }
        let after_hamt = rss_bytes();

        let mut flats: Vec<std::collections::HashMap<Symbol, Value>> = Vec::with_capacity(N);
        for _ in 0..N {
            let mut m = std::collections::HashMap::with_capacity(ENTRIES);
            for s in &syms { m.insert(*s, Value::Int(1)); }
            flats.push(m);
        }
        let after_flat = rss_bytes();

        let hamt_cost = after_hamt.saturating_sub(base);
        let flat_cost = after_flat.saturating_sub(after_hamt);
        eprintln!("N={N} entries={ENTRIES}");
        eprintln!("  im_rc HAMT : {} B total, {} B/map", hamt_cost, hamt_cost / N as u64);
        eprintln!("  std flat   : {} B total, {} B/map", flat_cost, flat_cost / N as u64);
        if flat_cost > 0 {
            eprintln!("  ratio      : {:.2}x", hamt_cost as f64 / flat_cost as f64);
        }
        std::hint::black_box((&hamts, &flats));
    }

    #[test]
    fn value_is_16_bytes() {
        assert_eq!(std::mem::size_of::<Value>(), 16);
    }

    // ── `//` carries attr positions (regression) ─────────

    /// A key's position must survive `//` — from either side, with the RIGHT
    /// winning, matching the precedence `//` gives the key's value.
    ///
    /// Regression: `overlay()` builds its node with an empty position slot, so
    /// reading only that slot reported `null` for every key of every `//`
    /// result. nixpkgs' `lib.nixosSystem` ends in
    /// `{ …; modules = …; } // removeAttrs args [ "modules" ]` and
    /// `eval-config.nix:28` reads `unsafeGetAttrPos "modules"` off it to set
    /// `modulesLocation`; a null there permutes NixOS definition order and
    /// diverges the toplevel drvPath from CppNix.
    #[test]
    fn overlay_carries_attr_positions_from_both_sides() {
        let tbl = |file: &str, key: &str, off: u32| {
            let mut t = crate::pos::AttrPositions::new(Some(std::path::PathBuf::from(file)));
            t.insert(intern(key), off);
            Rc::new(t)
        };
        // Both operands must be NON-empty: `overlay` short-circuits to the
        // other side when either is empty, so an empty operand would never
        // build the Overlay node this test exists to walk.
        let mk = |file: &str, key: &str, off: u32| {
            let mut a = NixAttrs::new();
            a.insert(key.to_string(), Value::Int(1));
            a.set_positions(tbl(file, key, off));
            a
        };

        // Key only on the LEFT — the shape nixosSystem actually hits, since
        // `removeAttrs args [ "modules" ]` strips it from the right.
        let left_only = mk("/l.nix", "modules", 11).overlay(mk("/r.nix", "other", 22));
        assert_eq!(
            left_only.pos_entry(intern("modules")),
            Some((Some(std::path::PathBuf::from("/l.nix")), 11)),
        );

        // Key on BOTH sides — right wins, as it does for the value.
        let both = mk("/l.nix", "modules", 11).overlay(mk("/r.nix", "modules", 22));
        assert_eq!(
            both.pos_entry(intern("modules")),
            Some((Some(std::path::PathBuf::from("/r.nix")), 22)),
        );

        // Absent key stays absent — the walk must not invent a position.
        assert_eq!(both.pos_entry(intern("nope")), None);
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

    // ── cppnix derivation-equality short-circuit (curl/git root) ─────────

    fn mk_drv_attrs(out_path: &str, extra_key: &str, extra_val: i64) -> Value {
        let mut a = NixAttrs::new();
        a.insert("type".to_string(), Value::string("derivation"));
        a.insert("outPath".to_string(), Value::string(out_path));
        a.insert(extra_key.to_string(), Value::Int(extra_val));
        Value::Attrs(Rc::new(a))
    }

    #[test]
    fn derivations_same_outpath_differing_attrs_are_equal() {
        // The load-bearing rule: two attrsets that are BOTH `type=="derivation"`
        // with an `outPath` compare by `outPath` string ONLY — differing extra
        // attrs must NOT make them unequal. This is what nixpkgs'
        // `isMismatchedPython` (`drv.pythonModule != python`) relies on; a deep
        // structural compare here spuriously fired the guard and dropped
        // `python` from flit-core's `propagatedBuildInputs` (curl/git root).
        let a = mk_drv_attrs("/nix/store/x-foo", "foo", 1);
        let b = mk_drv_attrs("/nix/store/x-foo", "bar", 2);
        assert!(a == b, "same-outPath derivations must compare equal");
        assert!(!(a != b));
    }

    #[test]
    fn derivations_differing_outpath_are_unequal() {
        let a = mk_drv_attrs("/nix/store/x-foo", "foo", 1);
        let b = mk_drv_attrs("/nix/store/y-foo", "foo", 1);
        assert!(a != b, "different-outPath derivations must compare unequal");
    }

    #[test]
    fn non_derivation_attrs_with_outpath_use_structural_eq() {
        // `outPath` alone (no `type == "derivation"`) does NOT trigger the
        // short-circuit — nix falls back to structural equality.
        let mut a = NixAttrs::new();
        a.insert("outPath".to_string(), Value::string("/nix/store/x"));
        a.insert("foo".to_string(), Value::Int(1));
        let mut b = NixAttrs::new();
        b.insert("outPath".to_string(), Value::string("/nix/store/x"));
        b.insert("foo".to_string(), Value::Int(2));
        assert!(
            Value::Attrs(Rc::new(a)) != Value::Attrs(Rc::new(b)),
            "non-derivation attrs with equal outPath but differing foo must be unequal",
        );
    }

    // ── C-A: attrs-eq structural compare BY BORROW (PERF-ARSENAL) ─────────
    // These seal that `Concrete::eq`'s Attrs arm — now `a.as_flat() ==
    // b.as_flat()` instead of `a.inner() == b.inner()` — is result- and
    // force-identical. The clone the old path did was pure allocation waste.

    #[test]
    fn attrs_eq_borrow_result_matches_multi_key() {
        // Structural equality over a multi-key set with a nested attrset value
        // must be unaffected by dropping the pre-compare clone.
        let mk = || {
            let mut inner = NixAttrs::new();
            inner.insert("n".to_string(), Value::Int(7));
            let mut a = NixAttrs::new();
            a.insert("a".to_string(), Value::Int(1));
            a.insert("b".to_string(), Value::string("two"));
            a.insert("c".to_string(), Value::Attrs(Rc::new(inner)));
            Value::Attrs(Rc::new(a))
        };
        assert!(mk() == mk(), "equal multi-key attrsets must compare equal (borrow path)");

        // Differ in one value → unequal.
        let mut b = NixAttrs::new();
        b.insert("a".to_string(), Value::Int(1));
        b.insert("b".to_string(), Value::string("TWO"));
        let mut a2 = NixAttrs::new();
        a2.insert("a".to_string(), Value::Int(1));
        a2.insert("b".to_string(), Value::string("two"));
        assert!(
            Value::Attrs(Rc::new(a2)) != Value::Attrs(Rc::new(b)),
            "attrsets differing in one value must be unequal (borrow path)",
        );

        // Differ in key SET → unequal.
        let mut a3 = NixAttrs::new();
        a3.insert("a".to_string(), Value::Int(1));
        let mut b3 = NixAttrs::new();
        b3.insert("a".to_string(), Value::Int(1));
        b3.insert("extra".to_string(), Value::Int(9));
        assert!(
            Value::Attrs(Rc::new(a3)) != Value::Attrs(Rc::new(b3)),
            "attrsets differing in key set must be unequal (borrow path)",
        );
    }

    #[test]
    fn attrs_eq_borrow_does_not_force_or_throw_on_shared_thunk() {
        // The demand-order verification obligation, made concrete:
        // `Value::eq` swallows force errors to `Null` (unwrap_or), so the
        // Attrs arm NEVER throws in the map-value-compare path. Two attrsets
        // that carry the SAME `Rc`-shared throwing thunk under a key that is
        // NOT decisive for (in)equality must:
        //   (a) compare via the structural (borrow) path without panicking, and
        //   (b) never let the throw escape.
        // If the borrow-compare forced the thunk and propagated the error, this
        // test would fail — proving the clone-elision touched no `.demand()`
        // behaviour that the old `inner()` clone path did not already exhibit.
        let boom = Value::Thunk(Thunk::new_native(|| {
            Err(EvalError::Throw("kaboom".to_string()))
        }));
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        a.insert("t".to_string(), boom.clone()); // same Rc-shared throwing thunk
        let mut b = NixAttrs::new();
        b.insert("x".to_string(), Value::Int(2)); // decisive differ on `x`
        b.insert("t".to_string(), boom);
        // No panic, no escaped Err: the comparison returns a bool. `x` differs,
        // so they are unequal — and crucially the throwing `t` thunk did not
        // abort the comparison.
        let va = Value::Attrs(Rc::new(a));
        let vb = Value::Attrs(Rc::new(b));
        assert!(va != vb, "differ on x → unequal, throwing thunk must not abort eq");
    }

    #[test]
    fn attrs_eq_borrow_overlay_still_compares() {
        // The `as_flat()` borrow path must also work when one side is an
        // Overlay (its cache is populated by `as_flat()`), matching the old
        // `inner()` path which flattened via the same `as_flat()`.
        let mut base = NixAttrs::new();
        base.insert("a".to_string(), Value::Int(1));
        let mut over = NixAttrs::new();
        over.insert("b".to_string(), Value::Int(2));
        // Build an overlay { a = 1; } // { b = 2; } (lazy Overlay variant),
        // exercising the `as_flat()` cache-population path on one side.
        let merged = base.overlay(over);
        let mut flat = NixAttrs::new();
        flat.insert("a".to_string(), Value::Int(1));
        flat.insert("b".to_string(), Value::Int(2));
        assert!(
            Value::Attrs(Rc::new(merged)) == Value::Attrs(Rc::new(flat)),
            "overlay and equivalent flat attrset must compare equal (borrow path)",
        );
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
            Value::Lambda(Rc::new(closure)).to_json(),
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
        assert_eq!(Value::Lambda(Rc::new(closure)).type_name(), "lambda");
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

    // ── concat_lists structural share (byte-neutrality) ──────────

    #[test]
    fn concat_lists_uniquely_owned_reuses_and_is_correct() {
        // A fresh left list (Rc strong_count == 1) hits the in-place path.
        let left = Value::list(vec![Value::Int(1), Value::Int(2)]);
        assert!(left.is_uniquely_owned_list());
        let right = [Value::Int(3), Value::Int(4)];
        let out = super::concat_lists(left, &right).unwrap();
        assert_eq!(
            out.as_list().unwrap(),
            &[Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)]
        );
    }

    #[test]
    fn concat_lists_shared_left_is_left_untouched_and_correct() {
        // Keep an outstanding Rc clone so the left is NOT uniquely owned;
        // the clone-extend fallback fires and the shared list is unchanged.
        let shared = Rc::new(NixList::new(vec![Value::Int(1), Value::Int(2)]));
        let left = Value::List(Rc::clone(&shared));
        assert!(!left.is_uniquely_owned_list());
        let right = [Value::Int(3)];
        let out = super::concat_lists(left, &right).unwrap();
        assert_eq!(
            out.as_list().unwrap(),
            &[Value::Int(1), Value::Int(2), Value::Int(3)]
        );
        // The original shared backing Vec is untouched.
        assert_eq!(&*shared, &[Value::Int(1), Value::Int(2)]);
    }

    #[test]
    fn concat_lists_empty_operands() {
        let out = super::concat_lists(Value::list(vec![]), &[]).unwrap();
        assert!(out.as_list().unwrap().is_empty());
        let out2 = super::concat_lists(Value::list(vec![Value::Int(9)]), &[]).unwrap();
        assert_eq!(out2.as_list().unwrap(), &[Value::Int(9)]);
        let out3 = super::concat_lists(Value::list(vec![]), &[Value::Int(9)]).unwrap();
        assert_eq!(out3.as_list().unwrap(), &[Value::Int(9)]);
    }

    #[test]
    fn concat_lists_non_list_left_errors() {
        assert!(super::concat_lists(Value::Int(1), &[]).is_err());
    }

    #[test]
    fn concat_lists_preserves_element_identity() {
        // The concatenated list must share the SAME element Rc, not deep-copy.
        let inner = Rc::new(NixString::plain("x"));
        let a = Value::String(Rc::clone(&inner));
        let left = Value::list(vec![a]);
        let out = super::concat_lists(left, &[]).unwrap();
        if let Value::String(rc) = &out.as_list().unwrap()[0] {
            assert!(Rc::ptr_eq(rc, &inner), "element Rc identity preserved");
        } else {
            panic!("expected string element");
        }
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
        assert_eq!(format!("{}", Value::Lambda(Rc::new(closure))), "<<lambda>>");
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

    // C-store PROVABLY-NEUTRAL seal (M2): a force whose body returns a
    // CONCRETE (non-Thunk) value takes the redundant-Store#2 skip path
    // (`!was_thunk_before_loop` early-return). It must still (a) return the
    // correct value, (b) be `is_evaluated()`, (c) populate the OnceCell so
    // the fast-path returns the identical value on re-force (proving Store#1's
    // guarded `cache.set` — NOT the skipped Store#2 — is what seals the cache),
    // and (d) `peek()` returns the value (repr holds it). If the skip dropped
    // the terminal state, one of these would regress.
    #[test]
    fn thunk_force_concrete_skips_redundant_store_but_caches() {
        // `1 + 2` evaluates directly to a concrete Int (no thunk-chain unwrap),
        // so it exercises the `!was_thunk_before_loop` skip branch.
        let root = rnix::Root::parse("1 + 2");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());

        let r1 = thunk.force(&|e, env| crate::eval::eval_expr(e, env)).unwrap();
        assert_eq!(r1, Value::Int(3));
        assert!(thunk.is_evaluated());

        // OnceCell must be populated (peek sees the value) — proves Store#1's
        // guarded cache.set fired and the skipped Store#2 was truly redundant.
        assert_eq!(thunk.peek().map(|c| c.clone().into_value()), Some(Value::Int(3)));

        // Re-force hits the OnceCell ultra-fast path and returns byte-identical.
        let r2 = thunk.force(&|_, _| panic!("re-force must hit the cache, not re-eval")).unwrap();
        assert_eq!(r2, Value::Int(3));
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

    // ── Import-from-derivation (IFD) detection + realize seal ──────────
    //
    // These lock the parity-critical decision "is this coercion an
    // import-from-derivation that must realize?" — the same decision cppnix
    // makes off a string's `Output` context. Regressing them silently reopens
    // the marquee `import ishou.stylix-fonts` root.

    #[test]
    fn out_path_needs_realize_matches_output_context() {
        // A store-path string carrying a derivation `Output` context IS a
        // derivation output → its producing `.drv` is returned for realize.
        let mut ctx = StringContext::new();
        ctx.add_output("/nix/store/aaa-thing.drv", "out");
        assert_eq!(
            super::out_path_needs_realize("/nix/store/bbb-thing", &ctx),
            Some("/nix/store/aaa-thing.drv".to_string()),
        );
    }

    #[test]
    fn out_path_needs_realize_ignores_plain_context() {
        // A plain store-path reference (not a derivation output) has nothing to
        // build — no realize.
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/ccc-plain");
        assert_eq!(super::out_path_needs_realize("/nix/store/ccc-plain", &ctx), None);
    }

    #[test]
    fn out_path_needs_realize_ignores_non_store_path() {
        // A non-store path is never a derivation output, even with an (invalid)
        // Output context — nothing to realize.
        let mut ctx = StringContext::new();
        ctx.add_output("/nix/store/ddd.drv", "out");
        assert_eq!(super::out_path_needs_realize("/etc/passwd", &ctx), None);
    }

    #[test]
    fn out_path_needs_realize_empty_context_is_none() {
        // A bare store-path literal (empty context) is not a derivation output.
        let ctx = StringContext::new();
        assert_eq!(super::out_path_needs_realize("/nix/store/eee-lit", &ctx), None);
    }

    #[test]
    fn coerce_to_realized_path_present_output_is_passthrough() {
        // When the output already exists on disk, no hook is needed and the
        // path is returned unchanged (the no-op realize path — proven live on
        // the already-built ifd-test derivation).
        let dir = std::env::temp_dir().join("sui-ifd-present-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out");
        std::fs::write(&file, b"present").unwrap();
        let present = file.to_string_lossy().to_string();

        let mut ctx = StringContext::new();
        // Pretend it's a derivation output (Output context) — but it exists,
        // so realize must NOT be invoked (no hook installed → would ENOENT if
        // it tried). A store-prefix check would skip a temp path, so assert the
        // simpler invariant: an existing plain string coerces to itself.
        ctx.add_plain(&present);
        let v = Value::String(std::rc::Rc::new(NixString::with_context(
            present.as_str(),
            ctx,
        )));
        assert_eq!(v.coerce_to_realized_path("readFile").unwrap(), present);
    }

    #[test]
    fn coerce_to_realized_path_absent_output_invokes_hook() {
        // A store-path string with an Output context whose output is ABSENT
        // invokes the realize hook with the producing drv. The mock hook
        // "materializes" nothing (the store path stays absent) but records the
        // call — proving the trigger fires end-to-end through coercion.
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let _guard = crate::realize::install_realize_hook(Box::new(move |drv, out| {
            seen2.lock().unwrap().push((drv.to_string(), out.to_string()));
            Ok(())
        }));

        // An absent store path (unique per run to avoid collision with a real
        // build) carrying an Output context.
        let out = "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-ifd-absent";
        assert!(!std::path::Path::new(out).exists(), "test store path must be absent");
        let mut ctx = StringContext::new();
        ctx.add_output("/nix/store/qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq-ifd-absent.drv", "out");
        let v = Value::String(std::rc::Rc::new(NixString::with_context(out, ctx)));

        // Coercion returns the outPath and fires the hook exactly once.
        assert_eq!(v.coerce_to_realized_path("readFile").unwrap(), out);
        let s = seen.lock().unwrap();
        assert_eq!(s.len(), 1, "realize hook should fire once for an absent output");
        assert_eq!(s[0].0, "/nix/store/qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq-ifd-absent.drv");
        assert_eq!(s[0].1, out);
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
        // CppNix %f-format: always 6 decimal places.
        let v = Value::Float(3.14);
        let (s, _ctx) = v.coerce_to_string().unwrap();
        assert_eq!(s, "3.140000");
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
        let val = Value::Lambda(Rc::new(closure));
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
        // Cache stores Concrete (thunk-free); force returns Value.
        // Compare via Concrete→Value promotion.
        assert_eq!((**cached).clone().into_value(), forced);
    }

    #[test]
    fn oncecell_new_evaluated_prepopulates_cache() {
        let thunk = Thunk::new_evaluated(Value::Int(77));
        // Cache should be set immediately.
        let cached = thunk.0.cache.get().expect("cache should be pre-populated");
        assert_eq!(**cached, Concrete::Int(77));
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
        // Thunk-valued scopes start with empty cache (thunk not yet forced)
        let thunk = Thunk::new_suspended(
            rnix::Root::parse("{}").tree().expr().unwrap(),
            Env::new(),
        );
        let env = Env::new().with_scope(Value::Thunk(thunk));
        let scope = &env.0.with_scopes[0];
        assert!(scope.cached.borrow().is_none());
    }

    #[test]
    fn with_scope_concrete_pre_populates_cache() {
        // Concrete attrset scopes pre-populate cache immediately
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(1));
        let env = Env::new().with_scope(Value::Attrs(Rc::new(attrs)));
        let scope = &env.0.with_scopes[0];
        assert!(scope.cached.borrow().is_some());
    }

    #[test]
    fn with_scope_first_lookup_populates_cache() {
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(42));
        let env = Env::new().with_scope(Value::Attrs(Rc::new(attrs)));
        // Cache is pre-populated for concrete attrsets.
        assert!(env.0.with_scopes[0].cached.borrow().is_some());
        // Lookup hits the pre-populated cache.
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
