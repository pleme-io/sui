//! Arithmetic builtins: add, sub, mul, div, ceil, floor, bitAnd, bitOr, bitXor, lessThan.

use super::*;

/// Register a curried arithmetic builtin that handles Int+Float coercion.
macro_rules! register_numeric_binop {
    ($builtins:expr, $name:expr, $int_op:expr, $float_op:expr) => {
        register_curried($builtins, $name, |a, b| {
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Value::Int($int_op(*x, *y))),
                (Value::Float(x), Value::Float(y)) => Ok(Value::Float($float_op(*x, *y))),
                (Value::Int(x), Value::Float(y)) => Ok(Value::Float($float_op(*x as f64, *y))),
                (Value::Float(x), Value::Int(y)) => Ok(Value::Float($float_op(*x, *y as f64))),
                _ => Err(EvalError::builtin_type($name, "numbers", "non-numeric")),
            }
        });
    };
}

/// Register a curried bitwise builtin operating on integers.
macro_rules! register_bitwise {
    ($builtins:expr, $name:expr, $op:expr) => {
        register_curried($builtins, $name, |a, b| {
            Ok(Value::Int($op(a.as_int()?, b.as_int()?)))
        });
    };
}

pub(crate) fn register(builtins: &mut NixAttrs) {
    // Every arithmetic op flows through one macro that handles
    // Int+Int, Float+Float, and the two mixed Int+Float cases.
    // Previously sub/mul/div were int-only and diverged from
    // cppnix on mixed-type arithmetic (e.g. `builtins.div 10.0 3.0`
    // errored with "expected ints" instead of returning 3.33333…).
    register_numeric_binop!(builtins, "add", |a: i64, b: i64| a + b, |a: f64, b: f64| a + b);
    register_numeric_binop!(builtins, "sub", |a: i64, b: i64| a - b, |a: f64, b: f64| a - b);
    register_numeric_binop!(builtins, "mul", |a: i64, b: i64| a * b, |a: f64, b: f64| a * b);

    // div is its own beast: integer division must trap on /0 with
    // a typed error, whereas float division returns inf/NaN per
    // IEEE 754 (cppnix mirrors this).
    register_curried(builtins, "div", |a, b| {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => {
                if *y == 0 { return Err(EvalError::DivisionByZero); }
                Ok(Value::Int(x / y))
            }
            (Value::Float(x), Value::Float(y)) => Ok(Value::Float(x / y)),
            (Value::Int(x), Value::Float(y)) => Ok(Value::Float(*x as f64 / *y)),
            (Value::Float(x), Value::Int(y)) => Ok(Value::Float(*x / *y as f64)),
            _ => Err(EvalError::builtin_type("div", "numbers", "non-numeric")),
        }
    });

    // Numeric — simple single-arg builtins
    const NUMERIC_BUILTINS: &[BuiltinSpec] = &[
        BuiltinSpec { name: "ceil",  func: |args| Ok(Value::Int(args[0].to_float()?.ceil() as i64)) },
        BuiltinSpec { name: "floor", func: |args| Ok(Value::Int(args[0].to_float()?.floor() as i64)) },
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
    register_bitwise!(builtins, "bitAnd", |a: i64, b: i64| a & b);
    register_bitwise!(builtins, "bitOr",  |a: i64, b: i64| a | b);
    register_bitwise!(builtins, "bitXor", |a: i64, b: i64| a ^ b);
}
