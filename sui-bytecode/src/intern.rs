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

use std::collections::HashMap;
use std::fmt;

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
    /// Forward map: string content -> symbol.
    map: HashMap<String, Symbol>,
    /// Reverse map: symbol index -> string content.
    strings: Vec<String>,
}

impl Interner {
    /// Create a new, empty interner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            strings: Vec::new(),
        }
    }

    /// Intern a string, returning its symbol. If the string was already
    /// interned, returns the existing symbol (O(1) amortized).
    pub fn intern(&mut self, s: &str) -> Symbol {
        if let Some(&sym) = self.map.get(s) {
            return sym;
        }
        let sym = Symbol(self.strings.len() as u32);
        self.strings.push(s.to_string());
        self.map.insert(s.to_string(), sym);
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

    /// Try to resolve a symbol, returning `None` if invalid.
    #[must_use]
    pub fn try_resolve(&self, sym: Symbol) -> Option<&str> {
        self.strings.get(sym.0 as usize).map(String::as_str)
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
}
