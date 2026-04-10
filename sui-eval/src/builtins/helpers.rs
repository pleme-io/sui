//! Registration helpers for builtins.
//!
//! `register_builtin` and `register_curried` are the two primitives
//! used by all sub-modules to insert builtin functions into the
//! `builtins` attribute set.

use std::rc::Rc;

use crate::value::*;

/// Descriptor for a simple single-argument builtin function.
///
/// The implementation closure receives the full `&[Value]` argument
/// slice (always length 1 after apply) and returns a `Result<Value>`.
/// Using a struct makes the builtin table scannable and self-documenting
/// without repeating the `register_builtin(…, |args| { … })` boilerplate.
pub(crate) struct BuiltinSpec {
    pub name: &'static str,
    pub func: fn(&[Value]) -> Result<Value, EvalError>,
}

pub(crate) fn register_builtin(
    attrs: &mut NixAttrs,
    name: &'static str,
    func: impl Fn(&[Value]) -> Result<Value, EvalError> + 'static,
) {
    attrs.insert(
        name.to_string(),
        Value::Builtin(Box::new(BuiltinFn {
            name,
            func: Rc::new(func),
        })),
    );
}

pub(crate) fn register_curried(
    attrs: &mut NixAttrs,
    name: &'static str,
    func: impl Fn(&Value, &Value) -> Result<Value, EvalError> + Clone + 'static,
) {
    let f = func.clone();
    attrs.insert(
        name.to_string(),
        Value::Builtin(Box::new(BuiltinFn {
            name,
            func: Rc::new(move |args| {
                let a = args[0].clone();
                let f2 = f.clone();
                Ok(Value::Builtin(Box::new(BuiltinFn {
                    name: "curried<partial>",
                    func: Rc::new(move |args2| f2(&a, &args2[0])),
                })))
            }),
        })),
    );
}
