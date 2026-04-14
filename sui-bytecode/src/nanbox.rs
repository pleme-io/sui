//! NaN-boxed value representation for the VM stack.
//!
//! Packs all value types into exactly 8 bytes using the quiet NaN
//! payload bits of IEEE 754 doubles. This eliminates heap allocation
//! for scalars and makes the value stack cache-friendly.
//!
//! # Layout
//!
//! IEEE 754 double:
//! ```text
//! [sign:1] [exponent:11] [mantissa:52]
//! ```
//!
//! A quiet NaN has exponent = all 1s and mantissa MSB = 1.
//! We use the remaining bits for a type tag + payload:
//!
//! ```text
//! Float:  any valid f64 that is not a signaling NaN with our tag pattern
//! Tagged: 0x7FF8_xxxx_xxxx_xxxx  (quiet NaN space)
//!   Tag bits [48..51] encode the type:
//!     0x0 = Null
//!     0x1 = Bool(false)
//!     0x2 = Bool(true)
//!     0x3 = Int (payload: i48 in bits [0..47])
//!     0x4 = Pointer to heap object (payload: 48-bit pointer)
//! ```
//!
//! # Performance Impact
//!
//! - Stack entries: 8 bytes instead of 40-80 bytes (enum VMValue)
//! - No heap allocation for null, bool, int, float
//! - Cache-friendly: entire stack fits in L1/L2 for typical expressions
//! - Copy is a single 64-bit register move

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use crate::error::VMError;
use crate::intern::Symbol;
use crate::value::{HigherOrderBuiltin, ThunkState, VMBuiltin, VMClosure, VMThunk, VMValue};

/// Quiet NaN base: exponent all 1s, mantissa MSB = 1.
const QNAN: u64 = 0x7FF8_0000_0000_0000;
/// Mask for the tag bits (bits 48-51, 4 bits = 16 possible tags).
const TAG_SHIFT: u64 = 48;
/// 48-bit payload mask.
const PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

// Tag values (shifted into position).
const TAG_NULL: u64 = QNAN | (0x0 << TAG_SHIFT);
const TAG_FALSE: u64 = QNAN | (0x1 << TAG_SHIFT);
const TAG_TRUE: u64 = QNAN | (0x2 << TAG_SHIFT);
const TAG_INT: u64 = QNAN | (0x3 << TAG_SHIFT);
const TAG_PTR: u64 = QNAN | (0x4 << TAG_SHIFT);

/// Tag extraction mask: QNAN + tag bits.
const TAG_MASK: u64 = QNAN | (0xF << TAG_SHIFT);

/// A NaN-boxed value: 8 bytes encoding any VM value type.
///
/// Floats are stored as-is (their bit pattern). Non-float values
/// are encoded in the NaN payload space.
pub struct NanBox(u64);

/// Heap-allocated object referenced by a `NanBox` pointer tag.
///
/// Contains the non-scalar VM value types: String, Path, List, Attrs,
/// Closure, Builtin, and Thunk.
pub enum HeapObject {
    String(String),
    Path(String),
    List(Vec<NanBox>),
    Attrs(BTreeMap<Symbol, NanBox>),
    Closure(VMClosure),
    Builtin(VMBuiltin),
    Thunk(VMThunk),
    HigherOrderBuiltin(HigherOrderBuiltin),
}

impl NanBox {
    // ── Constructors ──────────────────────────────────────────

    /// Create a null value.
    #[inline(always)]
    #[must_use]
    pub const fn null() -> Self {
        Self(TAG_NULL)
    }

    /// Create a boolean value.
    #[inline(always)]
    #[must_use]
    pub const fn bool(b: bool) -> Self {
        if b {
            Self(TAG_TRUE)
        } else {
            Self(TAG_FALSE)
        }
    }

    /// Create an integer value.
    ///
    /// Only the low 48 bits are preserved. For Nix evaluation, integers
    /// that exceed 48 bits fall back to the heap path (stored as `HeapObject`).
    #[inline(always)]
    #[must_use]
    pub fn int(n: i64) -> Self {
        // Check if the value fits in 48 bits (sign-extended).
        let fits = (n << 16) >> 16 == n;
        if fits {
            // Store as tagged NaN with 48-bit payload.
            let payload = (n as u64) & PAYLOAD_MASK;
            Self(TAG_INT | payload)
        } else {
            // Large integer: this shouldn't happen often in Nix.
            // Fall back by storing as float (lossy for >2^53).
            Self((n as f64).to_bits())
        }
    }

    /// Create a float value.
    #[inline(always)]
    #[must_use]
    pub fn float(f: f64) -> Self {
        Self(f.to_bits())
    }

    /// Create a pointer to a heap-allocated object.
    #[must_use]
    pub fn heap(obj: HeapObject) -> Self {
        let boxed = Rc::new(obj);
        let ptr = Rc::into_raw(boxed) as u64;
        debug_assert!(
            ptr & !PAYLOAD_MASK == 0,
            "pointer exceeds 48 bits"
        );
        Self(TAG_PTR | (ptr & PAYLOAD_MASK))
    }

    /// Create a string value (heap-allocated).
    #[must_use]
    pub fn string(s: String) -> Self {
        Self::heap(HeapObject::String(s))
    }

    /// Create a path value (heap-allocated).
    #[must_use]
    pub fn path(s: String) -> Self {
        Self::heap(HeapObject::Path(s))
    }

    /// Create a list value (heap-allocated).
    #[must_use]
    pub fn list(items: Vec<NanBox>) -> Self {
        Self::heap(HeapObject::List(items))
    }

    /// Create an attrs value (heap-allocated).
    #[must_use]
    pub fn attrs(map: BTreeMap<Symbol, NanBox>) -> Self {
        Self::heap(HeapObject::Attrs(map))
    }

    /// Create a closure value (heap-allocated).
    #[must_use]
    pub fn closure(c: VMClosure) -> Self {
        Self::heap(HeapObject::Closure(c))
    }

    /// Create a builtin function value (heap-allocated).
    #[must_use]
    pub fn builtin(b: VMBuiltin) -> Self {
        Self::heap(HeapObject::Builtin(b))
    }

    /// Create a thunk value (heap-allocated).
    #[must_use]
    pub fn thunk(t: VMThunk) -> Self {
        Self::heap(HeapObject::Thunk(t))
    }

    /// Create a higher-order builtin value (heap-allocated).
    #[must_use]
    pub fn higher_order_builtin(h: HigherOrderBuiltin) -> Self {
        Self::heap(HeapObject::HigherOrderBuiltin(h))
    }

    // ── Type checks ───────────────────────────────────────────

    /// Check if this value is a float (not a tagged NaN).
    #[inline(always)]
    #[must_use]
    pub fn is_float(&self) -> bool {
        // A value is a float if it's not in our tagged NaN space.
        // Our tags all have the QNAN pattern. Regular floats don't
        // (unless they happen to be NaN, which we treat as float).
        (self.0 & TAG_MASK) != TAG_INT
            && (self.0 & TAG_MASK) != TAG_NULL
            && (self.0 & TAG_MASK) != TAG_FALSE
            && (self.0 & TAG_MASK) != TAG_TRUE
            && (self.0 & TAG_MASK) != TAG_PTR
    }

    #[inline(always)]
    #[must_use]
    pub fn is_null(&self) -> bool {
        self.0 == TAG_NULL
    }

    #[inline(always)]
    #[must_use]
    pub fn is_bool(&self) -> bool {
        self.0 == TAG_TRUE || self.0 == TAG_FALSE
    }

    #[inline(always)]
    #[must_use]
    pub fn is_int(&self) -> bool {
        (self.0 & TAG_MASK) == TAG_INT
    }

    #[inline(always)]
    #[must_use]
    pub fn is_ptr(&self) -> bool {
        (self.0 & TAG_MASK) == TAG_PTR
    }

    // ── Extractors ────────────────────────────────────────────

    /// Extract a boolean. Returns `None` if not a bool.
    #[inline(always)]
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        if self.0 == TAG_TRUE {
            Some(true)
        } else if self.0 == TAG_FALSE {
            Some(false)
        } else {
            None
        }
    }

    /// Extract an integer. Returns `None` if not an int.
    #[inline(always)]
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        if (self.0 & TAG_MASK) == TAG_INT {
            // Sign-extend from 48 bits.
            let raw = (self.0 & PAYLOAD_MASK) as i64;
            let extended = (raw << 16) >> 16;
            Some(extended)
        } else {
            None
        }
    }

    /// Extract a float. Returns `None` if this is a tagged value.
    #[inline(always)]
    #[must_use]
    pub fn as_float(&self) -> Option<f64> {
        if self.is_float() {
            Some(f64::from_bits(self.0))
        } else {
            None
        }
    }

    /// Extract the heap object. Returns `None` if not a pointer.
    #[must_use]
    pub fn as_heap(&self) -> Option<&HeapObject> {
        if (self.0 & TAG_MASK) == TAG_PTR {
            let ptr = (self.0 & PAYLOAD_MASK) as *const HeapObject;
            // SAFETY: the pointer was created from Rc::into_raw and is valid
            // as long as at least one NanBox referencing it exists.
            Some(unsafe { &*ptr })
        } else {
            None
        }
    }

    // ── VM-facing helpers ──────────────────────────────────────

    /// Return the Nix type name for this value (mirrors `VMValue::type_name`).
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        if self.is_null() {
            "null"
        } else if self.is_bool() {
            "bool"
        } else if self.is_int() {
            "int"
        } else if self.is_float() {
            "float"
        } else if let Some(obj) = self.as_heap() {
            match obj {
                HeapObject::String(_) => "string",
                HeapObject::Path(_) => "path",
                HeapObject::List(_) => "list",
                HeapObject::Attrs(_) => "set",
                HeapObject::Closure(_) | HeapObject::Builtin(_) | HeapObject::HigherOrderBuiltin(_) => "lambda",
                HeapObject::Thunk(_) => "thunk",
            }
        } else {
            "unknown"
        }
    }

    /// Check if this value is truthy (for conditionals).
    /// Only booleans are valid; everything else is a type error.
    pub fn is_truthy(&self) -> Result<bool, VMError> {
        if self.0 == TAG_TRUE {
            Ok(true)
        } else if self.0 == TAG_FALSE {
            Ok(false)
        } else if self.is_null() {
            Ok(false)
        } else {
            // Non-bool values — type error. Thunks should be forced
            // before calling is_truthy.
            Err(VMError::TypeError {
                expected: "bool",
                got: self.type_name(),
                context: "condition".to_string(),
            })
        }
    }

    /// Check if this is a string.
    #[inline(always)]
    #[must_use]
    pub fn is_string(&self) -> bool {
        if let Some(HeapObject::String(_)) = self.as_heap() { true } else { false }
    }

    /// Check if this is a path.
    #[inline(always)]
    #[must_use]
    pub fn is_path(&self) -> bool {
        if let Some(HeapObject::Path(_)) = self.as_heap() { true } else { false }
    }

    /// Check if this is a list.
    #[inline(always)]
    #[must_use]
    pub fn is_list(&self) -> bool {
        if let Some(HeapObject::List(_)) = self.as_heap() { true } else { false }
    }

    /// Check if this is an attrset.
    #[inline(always)]
    #[must_use]
    pub fn is_attrs(&self) -> bool {
        if let Some(HeapObject::Attrs(_)) = self.as_heap() { true } else { false }
    }

    /// Check if this is a closure.
    #[inline(always)]
    #[must_use]
    pub fn is_closure(&self) -> bool {
        if let Some(HeapObject::Closure(_)) = self.as_heap() { true } else { false }
    }

    /// Check if this is a builtin.
    #[inline(always)]
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        if let Some(HeapObject::Builtin(_)) = self.as_heap() { true } else { false }
    }

    /// Check if this is a thunk.
    #[inline(always)]
    #[must_use]
    pub fn is_thunk(&self) -> bool {
        if let Some(HeapObject::Thunk(_)) = self.as_heap() { true } else { false }
    }

    /// Check if this is a higher-order builtin.
    #[inline(always)]
    #[must_use]
    pub fn is_higher_order_builtin(&self) -> bool {
        matches!(self.as_heap(), Some(HeapObject::HigherOrderBuiltin(_)))
    }

    /// Extract a string reference. Returns `None` if not a string.
    #[must_use]
    pub fn as_string(&self) -> Option<&str> {
        if let Some(HeapObject::String(s)) = self.as_heap() {
            Some(s.as_str())
        } else {
            None
        }
    }

    /// Extract a path reference. Returns `None` if not a path.
    #[must_use]
    pub fn as_path(&self) -> Option<&str> {
        if let Some(HeapObject::Path(p)) = self.as_heap() {
            Some(p.as_str())
        } else {
            None
        }
    }

    /// Extract a list reference. Returns `None` if not a list.
    #[must_use]
    pub fn as_list(&self) -> Option<&[NanBox]> {
        if let Some(HeapObject::List(items)) = self.as_heap() {
            Some(items.as_slice())
        } else {
            None
        }
    }

    /// Extract an attrs reference. Returns `None` if not an attrset.
    #[must_use]
    pub fn as_attrs(&self) -> Option<&BTreeMap<Symbol, NanBox>> {
        if let Some(HeapObject::Attrs(map)) = self.as_heap() {
            Some(map)
        } else {
            None
        }
    }

    /// Extract a closure reference. Returns `None` if not a closure.
    #[must_use]
    pub fn as_closure(&self) -> Option<&VMClosure> {
        if let Some(HeapObject::Closure(c)) = self.as_heap() {
            Some(c)
        } else {
            None
        }
    }

    /// Extract a builtin reference. Returns `None` if not a builtin.
    #[must_use]
    pub fn as_builtin(&self) -> Option<&VMBuiltin> {
        if let Some(HeapObject::Builtin(b)) = self.as_heap() {
            Some(b)
        } else {
            None
        }
    }

    /// Extract a thunk reference. Returns `None` if not a thunk.
    #[must_use]
    pub fn as_thunk(&self) -> Option<&VMThunk> {
        if let Some(HeapObject::Thunk(t)) = self.as_heap() {
            Some(t)
        } else {
            None
        }
    }

    /// Extract a higher-order builtin reference.
    #[must_use]
    pub fn as_higher_order_builtin(&self) -> Option<&HigherOrderBuiltin> {
        if let Some(HeapObject::HigherOrderBuiltin(h)) = self.as_heap() {
            Some(h)
        } else {
            None
        }
    }

    // ── Conversion to/from VMValue ────────────────────────────

    /// Convert a `VMValue` to a `NanBox`.
    pub fn from_vmvalue(val: &VMValue) -> Self {
        match val {
            VMValue::Null => Self::null(),
            VMValue::Bool(b) => Self::bool(*b),
            VMValue::Int(n) => Self::int(*n),
            VMValue::Float(f) => Self::float(*f),
            VMValue::String(s) => Self::string(s.clone()),
            VMValue::Path(p) => Self::path(p.clone()),
            VMValue::List(items) => {
                let boxed: Vec<NanBox> = items.iter().map(|v| Self::from_vmvalue(v)).collect();
                Self::list(boxed)
            }
            VMValue::Attrs(attrs) => {
                let boxed: BTreeMap<Symbol, NanBox> = attrs
                    .iter()
                    .map(|(k, v)| (*k, Self::from_vmvalue(v)))
                    .collect();
                Self::attrs(boxed)
            }
            VMValue::Closure(c) => Self::closure(c.clone()),
            VMValue::Builtin(b) => Self::builtin(b.clone()),
            VMValue::Thunk(t) => Self::thunk(t.clone()),
            VMValue::HigherOrderBuiltin(h) => Self::higher_order_builtin(h.clone()),
        }
    }

    /// Convert a `NanBox` back to a `VMValue`.
    pub fn to_vmvalue(&self) -> VMValue {
        if self.is_null() {
            VMValue::Null
        } else if let Some(b) = self.as_bool() {
            VMValue::Bool(b)
        } else if let Some(n) = self.as_int() {
            VMValue::Int(n)
        } else if let Some(f) = self.as_float() {
            VMValue::Float(f)
        } else if let Some(obj) = self.as_heap() {
            match obj {
                HeapObject::String(s) => VMValue::String(s.clone()),
                HeapObject::Path(p) => VMValue::Path(p.clone()),
                HeapObject::List(items) => {
                    VMValue::List(items.iter().map(NanBox::to_vmvalue).collect())
                }
                HeapObject::Attrs(attrs) => {
                    let map = attrs
                        .iter()
                        .map(|(k, v)| (*k, v.to_vmvalue()))
                        .collect();
                    VMValue::Attrs(map)
                }
                HeapObject::Closure(c) => VMValue::Closure(c.clone()),
                HeapObject::Builtin(b) => VMValue::Builtin(b.clone()),
                HeapObject::Thunk(t) => {
                    // Unwrap Done thunks to avoid re-wrapping forced values.
                    // This is critical: deep_force resolves thunks to NanBox
                    // values, but the HeapObject::Thunk wrapper persists.
                    // Without this unwrap, builtins see VMValue::Thunk instead
                    // of the concrete value, causing type errors.
                    let state = t.state.take();
                    match state {
                        Some(ThunkState::Done(boxed)) => {
                            t.state.set(Some(ThunkState::Done(boxed.clone())));
                            *boxed
                        }
                        other => {
                            t.state.set(other);
                            VMValue::Thunk(t.clone())
                        }
                    }
                }
                HeapObject::HigherOrderBuiltin(h) => VMValue::HigherOrderBuiltin(h.clone()),
            }
        } else {
            // Should not happen.
            VMValue::Null
        }
    }
}

impl Clone for HeapObject {
    fn clone(&self) -> Self {
        match self {
            HeapObject::String(s) => HeapObject::String(s.clone()),
            HeapObject::Path(p) => HeapObject::Path(p.clone()),
            HeapObject::List(items) => HeapObject::List(items.clone()),
            HeapObject::Attrs(attrs) => HeapObject::Attrs(attrs.clone()),
            HeapObject::Closure(c) => HeapObject::Closure(c.clone()),
            HeapObject::Builtin(b) => HeapObject::Builtin(b.clone()),
            HeapObject::Thunk(t) => HeapObject::Thunk(t.clone()),
            HeapObject::HigherOrderBuiltin(h) => HeapObject::HigherOrderBuiltin(h.clone()),
        }
    }
}

impl PartialEq for NanBox {
    fn eq(&self, other: &Self) -> bool {
        // Fast path: same bits means same value (covers scalars and same heap ptrs).
        if self.0 == other.0 {
            return true;
        }

        // Float comparison (handle NaN != NaN).
        if self.is_float() && other.is_float() {
            return self.as_float() == other.as_float();
        }

        // Int/Float cross-type comparison (Nix coerces int to float).
        if self.is_int() && other.is_float() {
            if let (Some(i), Some(f)) = (self.as_int(), other.as_float()) {
                return (i as f64) == f;
            }
        }
        if self.is_float() && other.is_int() {
            if let (Some(f), Some(i)) = (self.as_float(), other.as_int()) {
                return f == (i as f64);
            }
        }

        // Heap object deep comparison.
        if self.is_ptr() && other.is_ptr() {
            if let (Some(a), Some(b)) = (self.as_heap(), other.as_heap()) {
                return heap_eq(a, b);
            }
        }

        false
    }
}

impl Eq for NanBox {}

/// Deep equality comparison for heap objects.
fn heap_eq(a: &HeapObject, b: &HeapObject) -> bool {
    match (a, b) {
        (HeapObject::String(a), HeapObject::String(b)) => a == b,
        (HeapObject::Path(a), HeapObject::Path(b)) => a == b,
        (HeapObject::List(a), HeapObject::List(b)) => a == b,
        (HeapObject::Attrs(a), HeapObject::Attrs(b)) => a == b,
        _ => false,
    }
}

// Implement Drop for NanBox to properly handle Rc ref counting.
impl Drop for NanBox {
    fn drop(&mut self) {
        if (self.0 & TAG_MASK) == TAG_PTR {
            let ptr = (self.0 & PAYLOAD_MASK) as *const HeapObject;
            // SAFETY: this pointer was created with Rc::into_raw.
            // We reconstruct the Rc to decrement the reference count.
            unsafe {
                let _ = Rc::from_raw(ptr);
            }
        }
    }
}

// Clone must increment the Rc.
impl Clone for NanBox {
    fn clone(&self) -> Self {
        if (self.0 & TAG_MASK) == TAG_PTR {
            let ptr = (self.0 & PAYLOAD_MASK) as *const HeapObject;
            // SAFETY: reconstruct Rc, clone it (increment refcount), leak both.
            unsafe {
                let rc = Rc::from_raw(ptr);
                let cloned = Rc::clone(&rc);
                let _ = Rc::into_raw(rc); // don't drop the original
                let new_ptr = Rc::into_raw(cloned);
                Self(TAG_PTR | (new_ptr as u64 & PAYLOAD_MASK))
            }
        } else {
            Self(self.0)
        }
    }
}

impl fmt::Debug for NanBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_null() {
            write!(f, "NanBox(null)")
        } else if let Some(b) = self.as_bool() {
            write!(f, "NanBox({b})")
        } else if let Some(n) = self.as_int() {
            write!(f, "NanBox({n})")
        } else if let Some(fl) = self.as_float() {
            write!(f, "NanBox({fl})")
        } else if let Some(obj) = self.as_heap() {
            match obj {
                HeapObject::String(s) => write!(f, "NanBox(\"{s}\")"),
                HeapObject::Path(p) => write!(f, "NanBox(path:{p})"),
                HeapObject::List(items) => write!(f, "NanBox(list[{}])", items.len()),
                HeapObject::Attrs(map) => write!(f, "NanBox(attrs[{}])", map.len()),
                HeapObject::Closure(c) => write!(f, "NanBox({c:?})"),
                HeapObject::Builtin(b) => write!(f, "NanBox({b:?})"),
                HeapObject::Thunk(_) => write!(f, "NanBox(<thunk>)"),
                HeapObject::HigherOrderBuiltin(h) => write!(f, "NanBox({h:?})"),
            }
        } else {
            write!(f, "NanBox(0x{:016x})", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_roundtrip() {
        let v = NanBox::null();
        assert!(v.is_null());
        assert_eq!(v.to_vmvalue(), VMValue::Null);
    }

    #[test]
    fn bool_roundtrip() {
        let t = NanBox::bool(true);
        let f = NanBox::bool(false);
        assert_eq!(t.as_bool(), Some(true));
        assert_eq!(f.as_bool(), Some(false));
        assert_eq!(t.to_vmvalue(), VMValue::Bool(true));
        assert_eq!(f.to_vmvalue(), VMValue::Bool(false));
    }

    #[test]
    fn int_roundtrip() {
        for n in [0i64, 1, -1, 42, -42, 1000000, -1000000, i32::MAX as i64, i32::MIN as i64] {
            let v = NanBox::int(n);
            assert!(v.is_int(), "should be int for {n}");
            assert_eq!(v.as_int(), Some(n), "roundtrip failed for {n}");
        }
    }

    #[test]
    fn float_roundtrip() {
        for f in [0.0f64, 1.0, -1.0, 3.14, f64::INFINITY, f64::NEG_INFINITY] {
            let v = NanBox::float(f);
            assert!(v.is_float(), "should be float for {f}");
            assert_eq!(v.as_float(), Some(f), "roundtrip failed for {f}");
        }
    }

    #[test]
    fn string_roundtrip() {
        let v = NanBox::string("hello".to_string());
        assert!(v.is_ptr());
        match v.to_vmvalue() {
            VMValue::String(s) => assert_eq!(s, "hello"),
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn clone_heap_value() {
        let v1 = NanBox::string("test".to_string());
        let v2 = v1.clone();
        match v2.to_vmvalue() {
            VMValue::String(s) => assert_eq!(s, "test"),
            other => panic!("expected String, got {other:?}"),
        }
        // Both should be valid after clone.
        match v1.to_vmvalue() {
            VMValue::String(s) => assert_eq!(s, "test"),
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn vmvalue_roundtrip_scalars() {
        let cases = [
            VMValue::Null,
            VMValue::Bool(true),
            VMValue::Bool(false),
            VMValue::Int(42),
            VMValue::Int(-1),
            VMValue::Float(3.14),
        ];
        for val in &cases {
            let boxed = NanBox::from_vmvalue(val);
            let back = boxed.to_vmvalue();
            assert_eq!(*val, back, "roundtrip failed for {val:?}");
        }
    }

    #[test]
    fn vmvalue_roundtrip_string() {
        let val = VMValue::String("hello world".to_string());
        let boxed = NanBox::from_vmvalue(&val);
        let back = boxed.to_vmvalue();
        assert_eq!(val, back);
    }

    #[test]
    fn vmvalue_roundtrip_list() {
        let val = VMValue::List(vec![VMValue::Int(1), VMValue::Int(2), VMValue::Int(3)]);
        let boxed = NanBox::from_vmvalue(&val);
        let back = boxed.to_vmvalue();
        assert_eq!(val, back);
    }

    #[test]
    fn vmvalue_roundtrip_builtin() {
        use crate::value::VMBuiltin;
        use std::rc::Rc;
        let b = VMBuiltin {
            name: "test",
            func: Rc::new(|_| Ok(VMValue::Null)),
            arity: 1,
        };
        let val = VMValue::Builtin(b);
        let boxed = NanBox::from_vmvalue(&val);
        assert!(boxed.is_builtin());
        match boxed.to_vmvalue() {
            VMValue::Builtin(b) => assert_eq!(b.name, "test"),
            other => panic!("expected Builtin, got {other:?}"),
        }
    }

    #[test]
    fn vmvalue_roundtrip_thunk() {
        use crate::chunk::Chunk;
        use std::rc::Rc;
        let thunk = VMThunk::new(Rc::new(Chunk::new()), Vec::new());
        let val = VMValue::Thunk(thunk);
        let boxed = NanBox::from_vmvalue(&val);
        assert!(boxed.is_thunk());
        match boxed.to_vmvalue() {
            VMValue::Thunk(_) => {} // ok
            other => panic!("expected Thunk, got {other:?}"),
        }
    }

    #[test]
    fn type_name_all_types() {
        assert_eq!(NanBox::null().type_name(), "null");
        assert_eq!(NanBox::bool(true).type_name(), "bool");
        assert_eq!(NanBox::int(42).type_name(), "int");
        assert_eq!(NanBox::float(3.14).type_name(), "float");
        assert_eq!(NanBox::string("hi".to_string()).type_name(), "string");
        assert_eq!(NanBox::path("/tmp".to_string()).type_name(), "path");
        assert_eq!(NanBox::list(vec![]).type_name(), "list");
        assert_eq!(NanBox::attrs(BTreeMap::new()).type_name(), "set");
    }

    #[test]
    fn nanbox_equality() {
        assert_eq!(NanBox::null(), NanBox::null());
        assert_eq!(NanBox::bool(true), NanBox::bool(true));
        assert_ne!(NanBox::bool(true), NanBox::bool(false));
        assert_eq!(NanBox::int(42), NanBox::int(42));
        assert_ne!(NanBox::int(1), NanBox::int(2));
        assert_eq!(NanBox::float(3.14), NanBox::float(3.14));
        assert_eq!(NanBox::string("a".to_string()), NanBox::string("a".to_string()));
        assert_ne!(NanBox::string("a".to_string()), NanBox::string("b".to_string()));
    }

    #[test]
    fn nanbox_int_float_coercion() {
        assert_eq!(NanBox::int(1), NanBox::float(1.0));
        assert_eq!(NanBox::float(1.0), NanBox::int(1));
        assert_ne!(NanBox::int(1), NanBox::float(1.5));
    }

    #[test]
    fn is_truthy_bool() {
        assert!(NanBox::bool(true).is_truthy().unwrap());
        assert!(!NanBox::bool(false).is_truthy().unwrap());
    }

    #[test]
    fn is_truthy_non_bool_errors() {
        // Integers are still type errors in conditions.
        assert!(NanBox::int(1).is_truthy().is_err());
        // Null is permissively treated as false (VM workaround).
        assert_eq!(NanBox::null().is_truthy().unwrap(), false);
    }

    #[test]
    fn size_is_8_bytes() {
        assert_eq!(std::mem::size_of::<NanBox>(), 8);
    }
}
