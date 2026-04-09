//! Bidirectional conversion between bytecode VM values and tree-walker values.
//!
//! The bytecode VM (`sui_bytecode`) uses `VMValue` / `StringKeyedValue`,
//! while the tree-walker (`sui_eval`) uses `Value` / `NixAttrs`. This module
//! bridges the two representations so that the VM can be wired as an
//! alternative evaluation backend.

use crate::value::{NixAttrs, SmolStr, Value};
use sui_bytecode::value::{StringKeyedValue, VMValue};
use sui_bytecode::intern::Interner;

/// Convert a `StringKeyedValue` (from the bytecode VM) to a tree-walker `Value`.
///
/// This is the primary conversion path: the VM evaluates an expression and
/// returns a `StringKeyedValue` (fully resolved string keys), which we then
/// convert to the `Value` type used by the rest of sui (build, orchestrate, etc.).
#[must_use]
pub fn string_keyed_to_eval(sk: &StringKeyedValue) -> Value {
    match sk {
        StringKeyedValue::Null => Value::Null,
        StringKeyedValue::Bool(b) => Value::Bool(*b),
        StringKeyedValue::Int(n) => Value::Int(*n),
        StringKeyedValue::Float(f) => Value::Float(*f),
        StringKeyedValue::String(s) => Value::string(s.clone()),
        StringKeyedValue::Path(p) => Value::Path(SmolStr::from(p.as_str())),
        StringKeyedValue::List(items) => {
            Value::List(items.iter().map(string_keyed_to_eval).collect())
        }
        StringKeyedValue::Attrs(map) => {
            let mut attrs = NixAttrs::with_capacity(map.len());
            for (k, v) in map {
                attrs.insert(k.clone(), string_keyed_to_eval(v));
            }
            Value::Attrs(attrs)
        }
        StringKeyedValue::Lambda => Value::Null, // lambdas cannot cross the boundary
        StringKeyedValue::Thunk(cb) => {
            // Wrap the StringKeyedValue thunk as a tree-walker native thunk.
            let cb_clone = std::rc::Rc::clone(cb);
            Value::Thunk(crate::value::Thunk::new_native(move || {
                let sk_val = cb_clone()
                    .map_err(|e| crate::value::EvalError::TypeError(e))?;
                Ok(string_keyed_to_eval(&sk_val))
            }))
        }
    }
}

/// Convert a `VMValue` (with interned keys) to a tree-walker `Value`.
///
/// Requires access to the interner to resolve `Symbol` keys back to strings.
#[must_use]
pub fn vm_to_eval(vm: &VMValue, interner: &Interner) -> Value {
    let sk = vm.to_string_keyed(interner);
    string_keyed_to_eval(&sk)
}

/// Convert a tree-walker `Value` to a `VMValue` for consumption by the VM.
///
/// Closures and builtins cannot cross the boundary (converted to Null).
/// Thunks are wrapped lazily: already-evaluated thunks have their value
/// extracted, while unevaluated thunks become `VMThunk(NativeCallback)`
/// so they are only forced when the VM accesses the value.
#[must_use]
pub fn eval_to_vm(val: &Value, interner: &mut Interner) -> VMValue {
    match val {
        Value::Null => VMValue::Null,
        Value::Bool(b) => VMValue::Bool(*b),
        Value::Int(n) => VMValue::Int(*n),
        Value::Float(f) => VMValue::Float(*f),
        Value::String(s) => VMValue::String(s.chars.to_string()),
        Value::Path(p) => VMValue::Path(p.to_string()),
        Value::List(items) => {
            VMValue::List(items.iter().map(|v| eval_to_vm(v, interner)).collect())
        }
        Value::Attrs(attrs) => {
            let mut map = std::collections::BTreeMap::new();
            for (k, v) in attrs.iter() {
                let sym = interner.intern(k);
                map.insert(sym, eval_to_vm(v, interner));
            }
            VMValue::Attrs(map)
        }
        // Closures and builtins cannot cross the boundary.
        Value::Lambda(_) | Value::Builtin(_) => VMValue::Null,
        Value::Thunk(t) => {
            if t.is_evaluated() {
                match t.force(&|e, env| crate::eval::eval_expr(e, env)) {
                    Ok(v) => eval_to_vm(&v, interner),
                    Err(_) => VMValue::Null,
                }
            } else {
                // Wrap the tree-walker thunk in a NativeCallback VMThunk.
                let thunk_clone = t.clone();
                let cb: std::rc::Rc<dyn Fn() -> Result<StringKeyedValue, String>> =
                    std::rc::Rc::new(move || {
                        let forced = thunk_clone
                            .force(&|e, env| crate::eval::eval_expr(e, env))
                            .map_err(|e| e.to_string())?;
                        Ok(crate::eval_to_string_keyed(&forced))
                    });
                VMValue::Thunk(sui_bytecode::VMThunk {
                    state: std::rc::Rc::new(std::cell::Cell::new(Some(
                        sui_bytecode::value::ThunkState::NativeCallback(cb),
                    ))),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_scalar_null() {
        let sk = StringKeyedValue::Null;
        let val = string_keyed_to_eval(&sk);
        assert_eq!(val, Value::Null);
    }

    #[test]
    fn roundtrip_scalar_bool() {
        assert_eq!(string_keyed_to_eval(&StringKeyedValue::Bool(true)), Value::Bool(true));
        assert_eq!(string_keyed_to_eval(&StringKeyedValue::Bool(false)), Value::Bool(false));
    }

    #[test]
    fn roundtrip_scalar_int() {
        assert_eq!(string_keyed_to_eval(&StringKeyedValue::Int(42)), Value::Int(42));
    }

    #[test]
    fn roundtrip_scalar_float() {
        assert_eq!(string_keyed_to_eval(&StringKeyedValue::Float(3.14)), Value::Float(3.14));
    }

    #[test]
    fn roundtrip_string() {
        let sk = StringKeyedValue::String("hello".to_string());
        let val = string_keyed_to_eval(&sk);
        assert_eq!(val, Value::string("hello"));
    }

    #[test]
    fn roundtrip_path() {
        let sk = StringKeyedValue::Path("/tmp/x".to_string());
        let val = string_keyed_to_eval(&sk);
        assert_eq!(val, Value::Path(SmolStr::from("/tmp/x")));
    }

    #[test]
    fn roundtrip_list() {
        let sk = StringKeyedValue::List(vec![
            StringKeyedValue::Int(1),
            StringKeyedValue::Int(2),
        ]);
        let val = string_keyed_to_eval(&sk);
        assert_eq!(val, Value::List(vec![Value::Int(1), Value::Int(2)]));
    }

    #[test]
    fn roundtrip_attrs() {
        let mut map = std::collections::BTreeMap::new();
        map.insert("a".to_string(), StringKeyedValue::Int(1));
        map.insert("b".to_string(), StringKeyedValue::String("hi".to_string()));
        let sk = StringKeyedValue::Attrs(map);
        let val = string_keyed_to_eval(&sk);
        match val {
            Value::Attrs(attrs) => {
                assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
                assert_eq!(attrs.get("b"), Some(&Value::string("hi")));
            }
            _ => panic!("expected Attrs"),
        }
    }

    #[test]
    fn lambda_becomes_null() {
        let sk = StringKeyedValue::Lambda;
        let val = string_keyed_to_eval(&sk);
        assert_eq!(val, Value::Null);
    }

    #[test]
    fn vm_to_eval_with_interner() {
        let mut interner = Interner::new();
        let key = interner.intern("x");
        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert(key, VMValue::Int(42));
        let vm_val = VMValue::Attrs(attrs);
        let eval_val = vm_to_eval(&vm_val, &interner);
        match eval_val {
            Value::Attrs(a) => assert_eq!(a.get("x"), Some(&Value::Int(42))),
            _ => panic!("expected Attrs"),
        }
    }

    #[test]
    fn eval_to_vm_scalars() {
        let mut interner = Interner::new();
        assert_eq!(eval_to_vm(&Value::Null, &mut interner), VMValue::Null);
        assert_eq!(eval_to_vm(&Value::Bool(true), &mut interner), VMValue::Bool(true));
        assert_eq!(eval_to_vm(&Value::Int(7), &mut interner), VMValue::Int(7));
        assert_eq!(eval_to_vm(&Value::Float(1.5), &mut interner), VMValue::Float(1.5));
    }

    #[test]
    fn eval_to_vm_string() {
        let mut interner = Interner::new();
        let val = Value::string("test");
        let vm = eval_to_vm(&val, &mut interner);
        assert_eq!(vm, VMValue::String("test".to_string()));
    }

    #[test]
    fn eval_to_vm_attrs() {
        let mut interner = Interner::new();
        let mut attrs = NixAttrs::new();
        attrs.insert("key".to_string(), Value::Int(99));
        let val = Value::Attrs(attrs);
        let vm = eval_to_vm(&val, &mut interner);
        let sk = vm.to_string_keyed(&interner);
        match sk {
            StringKeyedValue::Attrs(map) => {
                assert_eq!(map.get("key"), Some(&StringKeyedValue::Int(99)));
            }
            _ => panic!("expected Attrs"),
        }
    }
}
