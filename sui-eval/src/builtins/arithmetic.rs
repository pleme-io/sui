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

    // lessThan (curried) — Int/Float (numeric), String (char order), and List
    // (lexicographic, element-by-element, recursing). See `nix_value_less_than`.
    register_curried(builtins, "lessThan", |a, b| {
        Ok(Value::Bool(nix_value_less_than(a, b)?))
    });

    // bitAnd, bitOr, bitXor (curried)
    register_bitwise!(builtins, "bitAnd", |a: i64, b: i64| a & b);
    register_bitwise!(builtins, "bitOr",  |a: i64, b: i64| a | b);
    register_bitwise!(builtins, "bitXor", |a: i64, b: i64| a ^ b);
}

/// Nix's `builtins.lessThan` ordering (also the `<` operator's semantics):
/// numeric for Int/Float (incl. mixed), char-order for String, and
/// **lexicographic** for List — element-by-element, recursing (nested lists
/// compare by the same rule), with a proper prefix ordered before the longer
/// list (`[1] < [1 2]`, `[] < [1]`). Equality at a position is detected the way
/// nix does — neither `a<b` nor `b<a` — so no separate `==` is needed. Forces
/// list elements pairwise as it descends, matching nix's demand order. Other /
/// mixed types are a type error, as in nix.
///
/// Scoped to `lessThan` today (the `<` binop still errors on lists — a separate,
/// un-exercised gap); kept as a free fn so the binop can adopt it when a fixture
/// demands it, without duplicating the rule.
pub(crate) fn nix_value_less_than(a: &Value, b: &Value) -> Result<bool, EvalError> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x < y),
        (Value::Float(x), Value::Float(y)) => Ok(x < y),
        (Value::Int(x), Value::Float(y)) => Ok((*x as f64) < *y),
        (Value::Float(x), Value::Int(y)) => Ok(*x < (*y as f64)),
        (Value::String(x), Value::String(y)) => Ok(x.chars < y.chars),
        (Value::List(xs), Value::List(ys)) => {
            let (xs, ys) = (&xs.0, &ys.0);
            let n = xs.len().min(ys.len());
            for i in 0..n {
                let xf = crate::eval::force_value(&xs[i])?;
                let yf = crate::eval::force_value(&ys[i])?;
                if nix_value_less_than(&xf, &yf)? {
                    return Ok(true);
                }
                if nix_value_less_than(&yf, &xf)? {
                    return Ok(false);
                }
            }
            // All shared positions equal → the shorter list is less.
            Ok(xs.len() < ys.len())
        }
        _ => Err(EvalError::TypeError("lessThan: expected comparable types".into())),
    }
}
