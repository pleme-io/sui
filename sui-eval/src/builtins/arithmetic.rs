//! Arithmetic builtins: add, sub, mul, div, ceil, floor, bitAnd, bitOr, bitXor, lessThan.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_curried(builtins, "add", |a, b| {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x + y)),
            (Value::Float(x), Value::Float(y)) => Ok(Value::Float(x + y)),
            (Value::Int(x), Value::Float(y)) => Ok(Value::Float(*x as f64 + y)),
            (Value::Float(x), Value::Int(y)) => Ok(Value::Float(x + *y as f64)),
            _ => Err(EvalError::TypeError("add: expected numbers".to_string())),
        }
    });
    register_curried(builtins, "sub", |a, b| {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x - y)),
            _ => Err(EvalError::TypeError("sub: expected ints".to_string())),
        }
    });
    register_curried(builtins, "mul", |a, b| {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x * y)),
            _ => Err(EvalError::TypeError("mul: expected ints".to_string())),
        }
    });
    register_curried(builtins, "div", |a, b| {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => {
                if *y == 0 { return Err(EvalError::DivisionByZero); }
                Ok(Value::Int(x / y))
            }
            _ => Err(EvalError::TypeError("div: expected ints".to_string())),
        }
    });

    // Numeric — simple single-arg builtins
    const NUMERIC_BUILTINS: &[BuiltinSpec] = &[
        BuiltinSpec { name: "ceil",  func: |args| Ok(Value::Int(args[0].as_float()?.ceil() as i64)) },
        BuiltinSpec { name: "floor", func: |args| Ok(Value::Int(args[0].as_float()?.floor() as i64)) },
    ];
    for spec in NUMERIC_BUILTINS {
        register_builtin(builtins, spec.name, spec.func);
    }

    // lessThan (curried)
    register_curried(builtins, "lessThan", |a, b| {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(Value::Bool(x < y)),
            (Value::Float(x), Value::Float(y)) => Ok(Value::Bool(x < y)),
            (Value::Int(x), Value::Float(y)) => Ok(Value::Bool((*x as f64) < *y)),
            (Value::Float(x), Value::Int(y)) => Ok(Value::Bool(*x < (*y as f64))),
            (Value::String(x), Value::String(y)) => Ok(Value::Bool(x.chars < y.chars)),
            _ => Err(EvalError::TypeError("lessThan: expected comparable types".into())),
        }
    });

    // bitAnd, bitOr, bitXor (curried)
    register_curried(builtins, "bitAnd", |a, b| {
        Ok(Value::Int(a.as_int()? & b.as_int()?))
    });
    register_curried(builtins, "bitOr", |a, b| {
        Ok(Value::Int(a.as_int()? | b.as_int()?))
    });
    register_curried(builtins, "bitXor", |a, b| {
        Ok(Value::Int(a.as_int()? ^ b.as_int()?))
    });
}
