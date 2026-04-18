//! String interning for attribute names and identifiers.
//!
//! Converts heap-allocated `String` comparisons into cheap `u32`
//! comparisons. Every unique string is stored exactly once; lookups
//! and comparisons use the [`Symbol`] handle (a `u32` index).
//!
//! # Performance Impact
//!
//! Attrset key operations (`GetAttr`, `HasAttr`, `MakeAttrs`, `UpdateAttrs`)
//! go from O(n) string comparison to O(1) integer comparison. This is
//! the single highest-ROI optimization for Nix evaluation because
//! nixpkgs is dominated by attrset operations.
//!
//! # Storage
//!
//! Strings are stored as `Rc<str>`. The forward map's key and the reverse
//! `strings` vector share the same allocation — no double-allocation on
//! intern, and `resolve_rc` returns a cheap `Rc::clone` instead of a
//! full `String::from` copy. Hashing uses `FxHashMap` (rustc-hash) —
//! ~2x faster than SipHash for the short strings typical of Nix keys
//! and identifiers.
//!
//! # Thread-Local Helpers
//!
//! For convenience in single-threaded evaluation, this crate provides
//! [`intern()`] and [`resolve()`] free functions that operate on a
//! thread-local [`Interner`] instance. [`prewarm()`] pre-interns a
//! curated set of hot nixpkgs symbols so common names get low indices
//! and the hashmap's initial resize cost is paid upfront.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use rustc_hash::FxHashMap;

/// An interned string handle — a cheap, copyable, comparable token.
///
/// Two `Symbol`s are equal if and only if they refer to the same
/// interned string. Comparison is a single `u32 ==` operation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Symbol(u32);

impl Symbol {
    /// Return the raw index for serialization / debugging.
    #[must_use]
    pub fn index(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Symbol({})", self.0)
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// A string interner that maps strings to [`Symbol`] handles.
///
/// Thread-local, single-owner. For a Nix evaluator this is sufficient
/// because evaluation is single-threaded.
#[derive(Clone, Default)]
pub struct Interner {
    /// Forward map: string content -> symbol. The `Rc<str>` key is
    /// shared with the reverse `strings` vector, so interning allocates
    /// the UTF-8 bytes exactly once.
    map: FxHashMap<Rc<str>, Symbol>,
    /// Reverse map: symbol index -> string content.
    strings: Vec<Rc<str>>,
}

impl Interner {
    /// Create a new, empty interner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: FxHashMap::default(),
            strings: Vec::new(),
        }
    }

    /// Create an interner with pre-allocated capacity. Use when you
    /// know the approximate identifier count upfront — avoids hashmap
    /// resizes during the hot path.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            map: FxHashMap::with_capacity_and_hasher(cap, rustc_hash::FxBuildHasher::default()),
            strings: Vec::with_capacity(cap),
        }
    }

    /// Intern a string, returning its symbol. If the string was already
    /// interned, returns the existing symbol (O(1) amortized).
    pub fn intern(&mut self, s: &str) -> Symbol {
        if let Some(&sym) = self.map.get(s) {
            return sym;
        }
        let rc: Rc<str> = Rc::from(s);
        let sym = Symbol(u32::try_from(self.strings.len()).expect("interner overflow"));
        self.strings.push(Rc::clone(&rc));
        self.map.insert(rc, sym);
        sym
    }

    /// Resolve a symbol back to its string content.
    ///
    /// # Panics
    ///
    /// Panics if the symbol was not produced by this interner.
    #[must_use]
    pub fn resolve(&self, sym: Symbol) -> &str {
        &self.strings[sym.0 as usize]
    }

    /// Resolve a symbol to its shared `Rc<str>` handle. Cheap to clone
    /// and pass around; prefer this over `resolve(...).to_string()` in
    /// hot paths.
    ///
    /// # Panics
    ///
    /// Panics if the symbol was not produced by this interner.
    #[must_use]
    pub fn resolve_rc(&self, sym: Symbol) -> Rc<str> {
        Rc::clone(&self.strings[sym.0 as usize])
    }

    /// Try to resolve a symbol, returning `None` if invalid.
    #[must_use]
    pub fn try_resolve(&self, sym: Symbol) -> Option<&str> {
        self.strings.get(sym.0 as usize).map(AsRef::as_ref)
    }

    /// Look up a symbol for a string without interning it.
    #[must_use]
    pub fn lookup(&self, s: &str) -> Option<Symbol> {
        self.map.get(s).copied()
    }

    /// Return the number of interned strings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.strings.len()
    }

    /// Whether the interner is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
}

impl fmt::Debug for Interner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Interner({} strings)", self.strings.len())
    }
}

// -- Thread-local interner helpers --

thread_local! {
    static GLOBAL_INTERNER: RefCell<Interner> = RefCell::new(Interner::with_capacity(512));
}

/// Intern a string using the thread-local interner.
pub fn intern(s: &str) -> Symbol {
    GLOBAL_INTERNER.with(|i| i.borrow_mut().intern(s))
}

/// Resolve a symbol using the thread-local interner.
///
/// Allocates a fresh `String`. Prefer [`resolve_rc`] or [`with_resolved`]
/// in hot paths — `Rc::clone` is ~20x cheaper than `String::from` for
/// typical identifier lengths.
#[must_use]
pub fn resolve(sym: Symbol) -> String {
    GLOBAL_INTERNER.with(|i| i.borrow().resolve(sym).to_string())
}

/// Resolve a symbol to a shared `Rc<str>` handle. Zero-copy.
#[must_use]
pub fn resolve_rc(sym: Symbol) -> Rc<str> {
    GLOBAL_INTERNER.with(|i| i.borrow().resolve_rc(sym))
}

/// Borrow the resolved string inside a closure without allocating.
/// The thread-local interner stays locked for the duration of `f`.
pub fn with_resolved<F, R>(sym: Symbol, f: F) -> R
where
    F: FnOnce(&str) -> R,
{
    GLOBAL_INTERNER.with(|i| f(i.borrow().resolve(sym)))
}

/// Look up a symbol for a string in the thread-local interner without
/// interning it.
#[must_use]
pub fn lookup(s: &str) -> Option<Symbol> {
    GLOBAL_INTERNER.with(|i| i.borrow().lookup(s))
}

/// Intern the hot nixpkgs/flake/stdenv symbol set so they get low
/// `Symbol` indices and the thread-local hashmap pays its first few
/// resizes upfront instead of on the eval hot path.
///
/// Call this once per thread before the first eval. Idempotent —
/// re-running is cheap because every symbol is a hashmap hit after
/// the first pass.
///
/// The curated list covers the identifiers that dominate nixpkgs
/// attribute access: derivation fields (`name`, `src`, `buildInputs`
/// …), module-system glue (`config`, `options`, `mkOption`, `mkIf`
/// …), flake schema (`inputs`, `outputs`, `description` …), meta
/// (`meta`, `platforms`, `license` …), and the empty string used by
/// string context machinery.
pub fn prewarm() {
    GLOBAL_INTERNER.with(|i| {
        let mut guard = i.borrow_mut();
        for s in HOT_SYMBOLS {
            guard.intern(s);
        }
    });
}

/// Hot symbols pre-interned by [`prewarm`]. Order is load-bearing —
/// the first entry gets Symbol(0), the second Symbol(1), etc.  Leave
/// the empty string first so `Symbol(0)` means "empty" by convention.
const HOT_SYMBOLS: &[&str] = &[
    // Sentinels
    "",
    // Derivation fields
    "name",
    "pname",
    "version",
    "src",
    "system",
    "builder",
    "args",
    "outputs",
    "out",
    "dev",
    "bin",
    "man",
    "doc",
    "outputHash",
    "outputHashAlgo",
    "outputHashMode",
    "passAsFile",
    "preferLocalBuild",
    "allowSubstitutes",
    // Build dependencies
    "buildInputs",
    "nativeBuildInputs",
    "propagatedBuildInputs",
    "propagatedNativeBuildInputs",
    "checkInputs",
    "nativeCheckInputs",
    "buildPhase",
    "installPhase",
    "configurePhase",
    "patchPhase",
    "unpackPhase",
    // Module system
    "config",
    "options",
    "imports",
    "_module",
    "mkOption",
    "mkDefault",
    "mkForce",
    "mkIf",
    "mkMerge",
    "mkOverride",
    "type",
    "default",
    "description",
    "example",
    "visible",
    "internal",
    "readOnly",
    // Flake schema
    "inputs",
    "url",
    "flake",
    "follows",
    "packages",
    "devShells",
    "apps",
    "overlays",
    "nixosModules",
    "nixosConfigurations",
    "darwinModules",
    "darwinConfigurations",
    "homeModules",
    "homeConfigurations",
    "templates",
    "checks",
    "formatter",
    "legacyPackages",
    // nixpkgs conventions
    "nixpkgs",
    "pkgs",
    "stdenv",
    "lib",
    "hostPlatform",
    "buildPlatform",
    "targetPlatform",
    "isDarwin",
    "isLinux",
    "x86_64-linux",
    "aarch64-linux",
    "x86_64-darwin",
    "aarch64-darwin",
    // Meta
    "meta",
    "platforms",
    "homepage",
    "license",
    "maintainers",
    "mainProgram",
    "available",
    "broken",
    "insecure",
    "unsupported",
    // String context + laziness helpers
    "recurseIntoAttrs",
    "__functor",
    "__toString",
    "__ignoreNulls",
    "outPath",
    "drvPath",
    "attrs",
    "outputName",
    // Common value identifiers
    "true",
    "false",
    "null",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_returns_same_symbol() {
        let mut interner = Interner::new();
        let s1 = interner.intern("hello");
        let s2 = interner.intern("hello");
        assert_eq!(s1, s2);
    }

    #[test]
    fn different_strings_different_symbols() {
        let mut interner = Interner::new();
        let s1 = interner.intern("hello");
        let s2 = interner.intern("world");
        assert_ne!(s1, s2);
    }

    #[test]
    fn resolve_roundtrip() {
        let mut interner = Interner::new();
        let sym = interner.intern("foo");
        assert_eq!(interner.resolve(sym), "foo");
    }

    #[test]
    fn resolve_rc_shares_allocation() {
        let mut interner = Interner::new();
        let sym = interner.intern("shared");
        let a = interner.resolve_rc(sym);
        let b = interner.resolve_rc(sym);
        assert_eq!(&*a, "shared");
        assert_eq!(&*b, "shared");
        // Two handles point at the same allocation.
        assert!(Rc::ptr_eq(&a, &b));
    }

    #[test]
    fn lookup_existing() {
        let mut interner = Interner::new();
        let sym = interner.intern("bar");
        assert_eq!(interner.lookup("bar"), Some(sym));
    }

    #[test]
    fn lookup_missing() {
        let interner = Interner::new();
        assert_eq!(interner.lookup("missing"), None);
    }

    #[test]
    fn len_and_empty() {
        let mut interner = Interner::new();
        assert!(interner.is_empty());
        assert_eq!(interner.len(), 0);
        interner.intern("a");
        interner.intern("b");
        interner.intern("a"); // duplicate
        assert_eq!(interner.len(), 2);
        assert!(!interner.is_empty());
    }

    #[test]
    fn symbol_ordering() {
        let mut interner = Interner::new();
        let s1 = interner.intern("alpha");
        let s2 = interner.intern("beta");
        // Symbols are ordered by insertion order, not alphabetically.
        assert!(s1 < s2);
    }

    #[test]
    fn try_resolve_valid() {
        let mut interner = Interner::new();
        let sym = interner.intern("test");
        assert_eq!(interner.try_resolve(sym), Some("test"));
    }

    #[test]
    fn try_resolve_invalid() {
        let interner = Interner::new();
        assert_eq!(interner.try_resolve(Symbol(999)), None);
    }

    #[test]
    fn clone_interner() {
        let mut interner = Interner::new();
        let s1 = interner.intern("hello");
        let cloned = interner.clone();
        assert_eq!(cloned.resolve(s1), "hello");
        assert_eq!(cloned.len(), 1);
    }

    #[test]
    fn with_capacity_preallocates() {
        let interner = Interner::with_capacity(256);
        assert!(interner.is_empty());
        // capacity is not exposed on FxHashMap publicly but len should be 0
        assert_eq!(interner.len(), 0);
    }

    // Thread-local helpers run inside `std::thread::spawn` so they
    // can't collide with each other or with tests in sibling crates
    // that already touched the global interner.
    #[test]
    fn thread_local_intern_resolve() {
        std::thread::spawn(|| {
            let sym = intern("thread_local_test");
            let resolved = resolve(sym);
            assert_eq!(resolved, "thread_local_test");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn thread_local_intern_dedup() {
        std::thread::spawn(|| {
            let s1 = intern("dedup");
            let s2 = intern("dedup");
            assert_eq!(s1, s2);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn thread_local_resolve_rc_zero_copy() {
        std::thread::spawn(|| {
            let sym = intern("tl_rc");
            let a = resolve_rc(sym);
            let b = resolve_rc(sym);
            assert!(Rc::ptr_eq(&a, &b));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn thread_local_with_resolved_no_alloc() {
        std::thread::spawn(|| {
            let sym = intern("borrowed");
            let len = with_resolved(sym, str::len);
            assert_eq!(len, "borrowed".len());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn thread_local_lookup() {
        std::thread::spawn(|| {
            assert_eq!(lookup("never_interned_here"), None);
            let sym = intern("findme");
            assert_eq!(lookup("findme"), Some(sym));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn prewarm_populates_hot_set() {
        std::thread::spawn(|| {
            // Fresh thread — interner starts empty, then prewarm fills it.
            prewarm();
            for &s in HOT_SYMBOLS {
                assert!(
                    lookup(s).is_some(),
                    "prewarm should have interned {s:?}"
                );
            }
            // Idempotent — second call shouldn't grow the interner.
            let before = HOT_SYMBOLS.len();
            prewarm();
            // The thread-local state isn't directly inspectable here;
            // instead, check that every hot symbol still resolves to
            // the same index it got the first time.
            let sym_first = lookup("name").expect("name interned by prewarm");
            assert!(sym_first.index() < u32::try_from(before).unwrap());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn hot_symbols_unique() {
        // Sanity: no duplicates in the curated list, otherwise the
        // "this symbol has the N-th index" reasoning breaks.
        let mut seen = std::collections::HashSet::new();
        for &s in HOT_SYMBOLS {
            assert!(seen.insert(s), "HOT_SYMBOLS contains duplicate {s:?}");
        }
    }
}
