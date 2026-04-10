//! Builtin bridge: allows the bytecode VM to call tree-walker builtins.
//!
//! The VM has native implementations for ~103 builtins, but nixpkgs needs
//! ~119+. Instead of reimplementing every builtin in the VM's value system,
//! this module provides a callback mechanism that delegates to the
//! tree-walker's builtins for any builtin the VM doesn't handle natively.
//!
//! # Architecture
//!
//! Follows the same pattern as `set_flake_resolver` in `vm.rs`:
//! a thread-local callback with an RAII guard for cleanup.
//!
//! ```text
//! sui-eval (BytecodeEvaluator)
//!   │
//!   ├─ set_builtin_bridge(callback)  ← installs bridge before VM eval
//!   │
//!   └─ VM execution
//!       │
//!       └─ CallBuiltin for "getEnv" etc.
//!           │
//!           └─ BuiltinRegistry stub → call_builtin_bridge("getEnv", args)
//!               │
//!               └─ callback converts VMValue↔Value, calls tree-walker builtin
//! ```

use std::cell::RefCell;

use crate::value::StringKeyedValue;

/// Callback type for bridging tree-walker builtins into the VM.
///
/// Takes: builtin name, args as `StringKeyedValue` (interner-free).
/// Returns: `StringKeyedValue` result or error string.
///
/// We use `StringKeyedValue` instead of `VMValue` because:
/// - It doesn't require an interner for key resolution
/// - `sui-eval` already has `string_keyed_to_eval` / `eval_to_string_keyed`
///   conversion functions
/// - The bridge callback runs in `sui-eval` context where the tree-walker
///   value types are available
pub type BuiltinBridgeFn = Box<dyn Fn(&str, Vec<StringKeyedValue>) -> Result<StringKeyedValue, String>>;

thread_local! {
    static BUILTIN_BRIDGE: RefCell<Option<BuiltinBridgeFn>> = const { RefCell::new(None) };
}

/// Install a builtin bridge callback for the current thread.
///
/// Returns an RAII guard that restores the previous bridge on drop.
/// This ensures the bridge is always properly cleaned up even when
/// evaluation errors occur.
pub fn set_builtin_bridge(bridge: BuiltinBridgeFn) -> BuiltinBridgeGuard {
    let prev = BUILTIN_BRIDGE.with(|b| b.borrow_mut().replace(bridge));
    BuiltinBridgeGuard { _prev: prev }
}

/// RAII guard that restores the previous builtin bridge on drop.
pub struct BuiltinBridgeGuard {
    _prev: Option<BuiltinBridgeFn>,
}

impl Drop for BuiltinBridgeGuard {
    fn drop(&mut self) {
        let prev = self._prev.take();
        BUILTIN_BRIDGE.with(|b| *b.borrow_mut() = prev);
    }
}

/// Call the builtin bridge for a named builtin.
///
/// Returns:
/// - `Ok(Some(result))` if the bridge handled the builtin
/// - `Ok(None)` if no bridge is installed (caller should error)
/// - `Err(msg)` if the bridge returned an error
pub fn call_builtin_bridge(
    name: &str,
    args: Vec<StringKeyedValue>,
) -> Result<Option<StringKeyedValue>, String> {
    BUILTIN_BRIDGE.with(|b| {
        let borrow = b.borrow();
        if let Some(ref bridge) = *borrow {
            bridge(name, args).map(Some)
        } else {
            Ok(None) // No bridge set
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_bridge_returns_none() {
        let result = call_builtin_bridge("getEnv", vec![StringKeyedValue::String("HOME".into())]);
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn bridge_handles_call() {
        let _guard = set_builtin_bridge(Box::new(|name, args| {
            assert_eq!(name, "getEnv");
            match &args[0] {
                StringKeyedValue::String(s) => {
                    Ok(StringKeyedValue::String(format!("mocked:{s}")))
                }
                _ => Err("expected string".into()),
            }
        }));

        let result = call_builtin_bridge(
            "getEnv",
            vec![StringKeyedValue::String("HOME".into())],
        );
        assert_eq!(
            result.unwrap().unwrap(),
            StringKeyedValue::String("mocked:HOME".into())
        );
    }

    #[test]
    fn bridge_error_propagates() {
        let _guard = set_builtin_bridge(Box::new(|_, _| {
            Err("bridge error".into())
        }));

        let result = call_builtin_bridge("anything", vec![]);
        assert_eq!(result.unwrap_err(), "bridge error");
    }

    #[test]
    fn guard_clears_bridge_on_drop() {
        {
            let _guard = set_builtin_bridge(Box::new(|_, _| {
                Ok(StringKeyedValue::Null)
            }));
            assert!(matches!(
                call_builtin_bridge("x", vec![]),
                Ok(Some(StringKeyedValue::Null))
            ));
        }
        // After guard drops, bridge should be cleared
        assert!(matches!(call_builtin_bridge("x", vec![]), Ok(None)));
    }

    // -- set_builtin_bridge installs callback --------------------------

    #[test]
    fn set_builtin_bridge_installs_callback() {
        let _guard = set_builtin_bridge(Box::new(|name, _| {
            Ok(StringKeyedValue::String(format!("handled:{name}")))
        }));
        let result = call_builtin_bridge("myBuiltin", vec![]);
        assert_eq!(
            result.unwrap().unwrap(),
            StringKeyedValue::String("handled:myBuiltin".into())
        );
    }

    // -- RAII guard clears callback on drop -----------------------------

    #[test]
    fn raii_guard_restores_previous_bridge() {
        // Install first bridge.
        let _outer = set_builtin_bridge(Box::new(|_, _| {
            Ok(StringKeyedValue::String("outer".into()))
        }));
        {
            // Install inner bridge (replaces outer temporarily).
            let _inner = set_builtin_bridge(Box::new(|_, _| {
                Ok(StringKeyedValue::String("inner".into()))
            }));
            let result = call_builtin_bridge("x", vec![]);
            assert_eq!(
                result.unwrap().unwrap(),
                StringKeyedValue::String("inner".into())
            );
        }
        // Inner guard dropped — outer bridge should be restored.
        let result = call_builtin_bridge("x", vec![]);
        assert_eq!(
            result.unwrap().unwrap(),
            StringKeyedValue::String("outer".into())
        );
    }

    // -- call_builtin_bridge returns None when no bridge ----------------

    #[test]
    fn call_builtin_bridge_returns_none_when_no_bridge() {
        // Ensure no bridge is installed (clean state after guard drop).
        {
            let _guard = set_builtin_bridge(Box::new(|_, _| Ok(StringKeyedValue::Null)));
        }
        let result = call_builtin_bridge("nonexistent", vec![]);
        assert!(matches!(result, Ok(None)));
    }

    // -- call_builtin_bridge returns Some when bridge set ---------------

    #[test]
    fn call_builtin_bridge_returns_some_when_bridge_set() {
        let _guard = set_builtin_bridge(Box::new(|_, _| {
            Ok(StringKeyedValue::Int(42))
        }));
        let result = call_builtin_bridge("anything", vec![]);
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
    }

    // -- bridge with simple string argument and return -----------------

    #[test]
    fn bridge_with_string_argument_and_return() {
        let _guard = set_builtin_bridge(Box::new(|name, args| {
            assert_eq!(name, "echo");
            match &args[0] {
                StringKeyedValue::String(s) => {
                    Ok(StringKeyedValue::String(format!("echo:{s}")))
                }
                _ => Err("expected string arg".into()),
            }
        }));
        let result = call_builtin_bridge(
            "echo",
            vec![StringKeyedValue::String("hello".into())],
        );
        assert_eq!(
            result.unwrap().unwrap(),
            StringKeyedValue::String("echo:hello".into())
        );
    }

    // -- bridge with attrset argument ----------------------------------

    #[test]
    fn bridge_with_attrset_argument() {
        let _guard = set_builtin_bridge(Box::new(|name, args| {
            assert_eq!(name, "inspect");
            match &args[0] {
                StringKeyedValue::Attrs(map) => {
                    let keys: Vec<&String> = map.keys().collect();
                    Ok(StringKeyedValue::Int(keys.len() as i64))
                }
                _ => Err("expected attrset".into()),
            }
        }));

        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert("a".to_string(), StringKeyedValue::Int(1));
        attrs.insert("b".to_string(), StringKeyedValue::Int(2));
        attrs.insert("c".to_string(), StringKeyedValue::Int(3));

        let result = call_builtin_bridge(
            "inspect",
            vec![StringKeyedValue::Attrs(attrs)],
        );
        assert_eq!(result.unwrap().unwrap(), StringKeyedValue::Int(3));
    }
}
