//! VM-specific value representation.
//!
//! Simpler than `sui_eval::Value` — no thunks, no rnix AST references.
//! The bytecode VM handles laziness through its own mechanisms; values
//! here are always fully evaluated.
//!
//! # String Interning
//!
//! Attribute set keys use [`Symbol`] handles instead of heap-allocated
//! `String`s. This makes key comparison O(1) (integer equality) instead
//! of O(n) (byte-by-byte string comparison). The interner is shared
//! between the compiler and VM via `Rc<RefCell<Interner>>`.

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use crate::chunk::Chunk;
use crate::intern::{Interner, Symbol};

/// A value in the bytecode VM.
///
/// Intentionally simpler than the tree-walker's `Value` type: no thunks
/// (the VM manages laziness via its call stack), no rnix AST nodes.
///
/// Attribute sets use [`Symbol`] keys for O(1) comparisons. Use
/// [`VMValue::attrs_to_strings`] to convert back to `BTreeMap<String, VMValue>`
/// for external consumption.
#[derive(Clone)]
pub enum VMValue {
    /// Nix `null`.
    Null,
    /// Nix boolean.
    Bool(bool),
    /// Nix integer (64-bit signed).
    Int(i64),
    /// Nix float (64-bit IEEE 754).
    Float(f64),
    /// Nix string (context tracking deferred to Phase 2).
    String(String),
    /// Nix path literal.
    Path(String),
    /// Nix list.
    List(Vec<VMValue>),
    /// Nix attribute set with interned keys.
    Attrs(BTreeMap<Symbol, VMValue>),
    /// A closure: compiled function body + captured upvalues.
    Closure(VMClosure),
}

/// A compiled closure: the function's bytecode chunk plus captured values.
#[derive(Clone)]
pub struct VMClosure {
    /// The function's compiled bytecode.
    pub chunk: Rc<Chunk>,
    /// Captured upvalues (values from enclosing scopes).
    pub upvalues: Vec<VMValue>,
    /// Number of parameters this closure expects (1 for Nix lambdas,
    /// but pattern-match destructuring may set multiple locals).
    pub arity: u16,
    /// Name hint for error messages (e.g., the parameter name).
    pub name: Option<String>,
}

impl fmt::Debug for VMClosure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<closure arity={}", self.arity)?;
        if let Some(ref name) = self.name {
            write!(f, " name={name}")?;
        }
        write!(f, ">")
    }
}

impl VMValue {
    /// Return the Nix type name for this value.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            VMValue::Null => "null",
            VMValue::Bool(_) => "bool",
            VMValue::Int(_) => "int",
            VMValue::Float(_) => "float",
            VMValue::String(_) => "string",
            VMValue::Path(_) => "path",
            VMValue::List(_) => "list",
            VMValue::Attrs(_) => "set",
            VMValue::Closure(_) => "lambda",
        }
    }

    /// Check if this value is truthy (for conditionals).
    pub fn is_truthy(&self) -> Result<bool, crate::error::VMError> {
        match self {
            VMValue::Bool(b) => Ok(*b),
            other => Err(crate::error::VMError::TypeError {
                expected: "bool",
                got: other.type_name(),
                context: "condition".to_string(),
            }),
        }
    }

    /// Convert a `Symbol`-keyed attrset to a `String`-keyed `BTreeMap`
    /// using the provided interner. Returns `None` if not an `Attrs`.
    #[must_use]
    pub fn attrs_to_strings(&self, interner: &Interner) -> Option<BTreeMap<String, VMValue>> {
        match self {
            VMValue::Attrs(attrs) => {
                let map = attrs
                    .iter()
                    .map(|(sym, val)| (interner.resolve(*sym).to_string(), val.clone()))
                    .collect();
                Some(map)
            }
            _ => None,
        }
    }

    /// Convert this entire value tree to use string keys (for external API).
    /// Recursively resolves all `Symbol` keys in nested attrsets and lists.
    #[must_use]
    pub fn to_string_keyed(&self, interner: &Interner) -> StringKeyedValue {
        match self {
            VMValue::Null => StringKeyedValue::Null,
            VMValue::Bool(b) => StringKeyedValue::Bool(*b),
            VMValue::Int(n) => StringKeyedValue::Int(*n),
            VMValue::Float(f) => StringKeyedValue::Float(*f),
            VMValue::String(s) => StringKeyedValue::String(s.clone()),
            VMValue::Path(p) => StringKeyedValue::Path(p.clone()),
            VMValue::List(items) => {
                StringKeyedValue::List(items.iter().map(|v| v.to_string_keyed(interner)).collect())
            }
            VMValue::Attrs(attrs) => {
                let map = attrs
                    .iter()
                    .map(|(sym, val)| {
                        (interner.resolve(*sym).to_string(), val.to_string_keyed(interner))
                    })
                    .collect();
                StringKeyedValue::Attrs(map)
            }
            VMValue::Closure(_) => StringKeyedValue::Lambda,
        }
    }

    /// Format this value for display using the interner for key resolution.
    pub fn display_with(&self, interner: &Interner, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VMValue::Null => write!(f, "null"),
            VMValue::Bool(b) => write!(f, "{b}"),
            VMValue::Int(n) => write!(f, "{n}"),
            VMValue::Float(n) => {
                if n.fract() == 0.0 {
                    write!(f, "{n:.6}")
                } else {
                    write!(f, "{n}")
                }
            }
            VMValue::String(s) => write!(f, "\"{s}\""),
            VMValue::Path(p) => write!(f, "{p}"),
            VMValue::List(items) => {
                write!(f, "[ ")?;
                for item in items {
                    item.display_with(interner, f)?;
                    write!(f, " ")?;
                }
                write!(f, "]")
            }
            VMValue::Attrs(map) => {
                write!(f, "{{ ")?;
                for (sym, v) in map {
                    let key = interner.resolve(*sym);
                    write!(f, "{key} = ")?;
                    v.display_with(interner, f)?;
                    write!(f, "; ")?;
                }
                write!(f, "}}")
            }
            VMValue::Closure(_) => write!(f, "<<lambda>>"),
        }
    }

    /// Debug this value using the interner for key resolution.
    pub fn debug_with(&self, interner: &Interner, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VMValue::Null => write!(f, "null"),
            VMValue::Bool(b) => write!(f, "{b}"),
            VMValue::Int(n) => write!(f, "{n}"),
            VMValue::Float(n) => write!(f, "{n}"),
            VMValue::String(s) => write!(f, "{s:?}"),
            VMValue::Path(p) => write!(f, "{p}"),
            VMValue::List(items) => {
                write!(f, "[ ")?;
                for item in items {
                    item.debug_with(interner, f)?;
                    write!(f, " ")?;
                }
                write!(f, "]")
            }
            VMValue::Attrs(map) => {
                write!(f, "{{ ")?;
                for (sym, v) in map {
                    let key = interner.resolve(*sym);
                    write!(f, "{key} = ")?;
                    v.debug_with(interner, f)?;
                    write!(f, "; ")?;
                }
                write!(f, "}}")
            }
            VMValue::Closure(c) => write!(f, "{c:?}"),
        }
    }
}

/// A string-keyed value for external API consumption.
///
/// Produced by [`VMValue::to_string_keyed`]. Uses `BTreeMap<String, _>`
/// for attrsets so callers don't need access to the interner.
#[derive(Debug, Clone, PartialEq)]
pub enum StringKeyedValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Path(String),
    List(Vec<StringKeyedValue>),
    Attrs(BTreeMap<String, StringKeyedValue>),
    Lambda,
}

impl Eq for StringKeyedValue {}

impl fmt::Display for StringKeyedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StringKeyedValue::Null => write!(f, "null"),
            StringKeyedValue::Bool(b) => write!(f, "{b}"),
            StringKeyedValue::Int(n) => write!(f, "{n}"),
            StringKeyedValue::Float(n) => {
                if n.fract() == 0.0 {
                    write!(f, "{n:.6}")
                } else {
                    write!(f, "{n}")
                }
            }
            StringKeyedValue::String(s) => write!(f, "\"{s}\""),
            StringKeyedValue::Path(p) => write!(f, "{p}"),
            StringKeyedValue::List(items) => {
                write!(f, "[ ")?;
                for item in items {
                    write!(f, "{item} ")?;
                }
                write!(f, "]")
            }
            StringKeyedValue::Attrs(map) => {
                write!(f, "{{ ")?;
                for (k, v) in map {
                    write!(f, "{k} = {v}; ")?;
                }
                write!(f, "}}")
            }
            StringKeyedValue::Lambda => write!(f, "<<lambda>>"),
        }
    }
}

// ── Debug / Display without interner (best-effort) ────────────────

impl fmt::Debug for VMValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VMValue::Null => write!(f, "null"),
            VMValue::Bool(b) => write!(f, "{b}"),
            VMValue::Int(n) => write!(f, "{n}"),
            VMValue::Float(n) => write!(f, "{n}"),
            VMValue::String(s) => write!(f, "{s:?}"),
            VMValue::Path(p) => write!(f, "{p}"),
            VMValue::List(items) => {
                write!(f, "[ ")?;
                for item in items {
                    write!(f, "{item:?} ")?;
                }
                write!(f, "]")
            }
            VMValue::Attrs(map) => {
                write!(f, "{{ ")?;
                for (sym, v) in map {
                    write!(f, "#{} = {v:?}; ", sym.index())?;
                }
                write!(f, "}}")
            }
            VMValue::Closure(c) => write!(f, "{c:?}"),
        }
    }
}

impl fmt::Display for VMValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VMValue::Null => write!(f, "null"),
            VMValue::Bool(b) => write!(f, "{b}"),
            VMValue::Int(n) => write!(f, "{n}"),
            VMValue::Float(n) => {
                // Nix always prints at least one decimal place for floats.
                if n.fract() == 0.0 {
                    write!(f, "{n:.6}")
                } else {
                    write!(f, "{n}")
                }
            }
            VMValue::String(s) => write!(f, "\"{s}\""),
            VMValue::Path(p) => write!(f, "{p}"),
            VMValue::List(items) => {
                write!(f, "[ ")?;
                for item in items {
                    write!(f, "{item} ")?;
                }
                write!(f, "]")
            }
            VMValue::Attrs(map) => {
                write!(f, "{{ ")?;
                for (sym, v) in map {
                    write!(f, "#{} = {v}; ", sym.index())?;
                }
                write!(f, "}}")
            }
            VMValue::Closure(_) => write!(f, "<<lambda>>"),
        }
    }
}

impl PartialEq for VMValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (VMValue::Null, VMValue::Null) => true,
            (VMValue::Bool(a), VMValue::Bool(b)) => a == b,
            (VMValue::Int(a), VMValue::Int(b)) => a == b,
            (VMValue::Float(a), VMValue::Float(b)) => a == b,
            (VMValue::Int(a), VMValue::Float(b)) | (VMValue::Float(b), VMValue::Int(a)) => {
                (*a as f64) == *b
            }
            (VMValue::String(a), VMValue::String(b)) => a == b,
            (VMValue::Path(a), VMValue::Path(b)) => a == b,
            (VMValue::List(a), VMValue::List(b)) => a == b,
            (VMValue::Attrs(a), VMValue::Attrs(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for VMValue {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names() {
        assert_eq!(VMValue::Null.type_name(), "null");
        assert_eq!(VMValue::Bool(true).type_name(), "bool");
        assert_eq!(VMValue::Int(0).type_name(), "int");
        assert_eq!(VMValue::Float(0.0).type_name(), "float");
        assert_eq!(VMValue::String("".to_string()).type_name(), "string");
        assert_eq!(VMValue::Path("/tmp".to_string()).type_name(), "path");
        assert_eq!(VMValue::List(vec![]).type_name(), "list");
        assert_eq!(VMValue::Attrs(BTreeMap::new()).type_name(), "set");
    }

    #[test]
    fn equality_int_float_coercion() {
        assert_eq!(VMValue::Int(1), VMValue::Float(1.0));
        assert_eq!(VMValue::Float(1.0), VMValue::Int(1));
        assert_ne!(VMValue::Int(1), VMValue::Float(1.5));
    }

    #[test]
    fn equality_same_types() {
        assert_eq!(VMValue::Null, VMValue::Null);
        assert_eq!(VMValue::Bool(true), VMValue::Bool(true));
        assert_ne!(VMValue::Bool(true), VMValue::Bool(false));
        assert_eq!(VMValue::Int(42), VMValue::Int(42));
        assert_eq!(
            VMValue::String("hello".to_string()),
            VMValue::String("hello".to_string())
        );
    }

    #[test]
    fn equality_different_types() {
        assert_ne!(VMValue::Null, VMValue::Bool(false));
        assert_ne!(VMValue::Int(0), VMValue::Bool(false));
        assert_ne!(VMValue::String("1".to_string()), VMValue::Int(1));
    }

    #[test]
    fn is_truthy_bool() {
        assert!(VMValue::Bool(true).is_truthy().unwrap());
        assert!(!VMValue::Bool(false).is_truthy().unwrap());
    }

    #[test]
    fn is_truthy_non_bool_errors() {
        assert!(VMValue::Int(1).is_truthy().is_err());
        assert!(VMValue::Null.is_truthy().is_err());
    }

    #[test]
    fn attrs_to_strings_conversion() {
        let mut interner = Interner::new();
        let key = interner.intern("hello");
        let mut attrs = BTreeMap::new();
        attrs.insert(key, VMValue::Int(42));
        let val = VMValue::Attrs(attrs);
        let string_map = val.attrs_to_strings(&interner).unwrap();
        assert_eq!(string_map.get("hello"), Some(&VMValue::Int(42)));
    }

    #[test]
    fn to_string_keyed_roundtrip() {
        let mut interner = Interner::new();
        let key = interner.intern("x");
        let mut attrs = BTreeMap::new();
        attrs.insert(key, VMValue::Int(1));
        let val = VMValue::Attrs(attrs);
        let sk = val.to_string_keyed(&interner);
        match sk {
            StringKeyedValue::Attrs(map) => {
                assert_eq!(map.get("x"), Some(&StringKeyedValue::Int(1)));
            }
            _ => panic!("expected Attrs"),
        }
    }

    #[test]
    fn symbol_keyed_attrs_equality() {
        let mut interner = Interner::new();
        let k1 = interner.intern("a");
        let k2 = interner.intern("a");
        let mut a1 = BTreeMap::new();
        a1.insert(k1, VMValue::Int(1));
        let mut a2 = BTreeMap::new();
        a2.insert(k2, VMValue::Int(1));
        assert_eq!(VMValue::Attrs(a1), VMValue::Attrs(a2));
    }
}
