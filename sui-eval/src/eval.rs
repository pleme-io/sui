//! Tree-walking Nix evaluator.

use crate::ast::*;
use crate::builtins;
use crate::parser::Parser;
use crate::value::*;

/// Evaluate a Nix expression string.
pub fn eval(input: &str) -> Result<Value, EvalError> {
    let expr = Parser::parse(input).map_err(|e| EvalError::ParseError(e.to_string()))?;
    let mut env = Env::new();
    builtins::register(&mut env);
    eval_expr(&expr, &env)
}

/// Evaluate an expression in an environment.
pub fn eval_expr(expr: &Expr, env: &Env) -> Result<Value, EvalError> {
    match expr {
        // Literals
        Expr::Int(n) => Ok(Value::Int(*n)),
        Expr::Float(f) => Ok(Value::Float(*f)),
        Expr::Str(s) => Ok(Value::String(s.clone())),
        Expr::Path(p) => Ok(Value::Path(p.clone())),
        Expr::SearchPath(p) => Ok(Value::Path(format!("<{p}>"))),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Null => Ok(Value::Null),

        // Variables
        Expr::Var(name) => env
            .lookup(name)
            .ok_or_else(|| EvalError::UndefinedVar(name.clone())),

        // List
        Expr::List(items) => {
            let values: Result<Vec<_>, _> = items.iter().map(|e| eval_expr(e, env)).collect();
            Ok(Value::List(values?))
        }

        // Attribute set
        Expr::AttrSet(set) => eval_attrset(set, env),

        // Select: expr.attr or expr.attr or default
        Expr::Select(expr, path, default) => {
            let mut value = eval_expr(expr, env)?;
            for attr_name in path {
                let key = eval_attr_name(attr_name, env)?;
                match value {
                    Value::Attrs(ref attrs) => {
                        if let Some(v) = attrs.get(&key) {
                            value = v.clone();
                        } else if let Some(def) = default {
                            return eval_expr(def, env);
                        } else {
                            return Err(EvalError::AttrNotFound(key));
                        }
                    }
                    _ => return Err(EvalError::TypeError(format!(
                        "cannot select from {}", value.type_name()
                    ))),
                }
            }
            Ok(value)
        }

        // Has attribute: expr ? attr
        Expr::HasAttr(expr, path) => {
            let mut value = eval_expr(expr, env)?;
            for attr_name in path {
                let key = eval_attr_name(attr_name, env)?;
                match value {
                    Value::Attrs(ref attrs) => {
                        if let Some(v) = attrs.get(&key) {
                            value = v.clone();
                        } else {
                            return Ok(Value::Bool(false));
                        }
                    }
                    _ => return Ok(Value::Bool(false)),
                }
            }
            Ok(Value::Bool(true))
        }

        // Unary operations
        Expr::UnaryOp(op, expr) => {
            let val = eval_expr(expr, env)?;
            match op {
                UnaryOp::Neg => match val {
                    Value::Int(n) => Ok(Value::Int(-n)),
                    Value::Float(f) => Ok(Value::Float(-f)),
                    _ => Err(EvalError::TypeError(format!("cannot negate {}", val.type_name()))),
                },
                UnaryOp::Not => Ok(Value::Bool(!val.as_bool()?)),
            }
        }

        // Binary operations
        Expr::BinOp(op, lhs, rhs) => eval_binop(*op, lhs, rhs, env),

        // Function application
        Expr::Apply(func_expr, arg_expr) => {
            let func = eval_expr(func_expr, env)?;
            let arg = eval_expr(arg_expr, env)?;
            apply(func, arg)
        }

        // If-then-else
        Expr::If(cond, then_expr, else_expr) => {
            if eval_expr(cond, env)?.as_bool()? {
                eval_expr(then_expr, env)
            } else {
                eval_expr(else_expr, env)
            }
        }

        // Assert
        Expr::Assert(cond, body) => {
            if !eval_expr(cond, env)?.as_bool()? {
                return Err(EvalError::AssertionFailed);
            }
            eval_expr(body, env)
        }

        // With
        Expr::With(scope_expr, body) => {
            let scope = eval_expr(scope_expr, env)?;
            let attrs = scope.as_attrs()?.clone();
            let new_env = env.child().with_scope(attrs);
            eval_expr(body, &new_env)
        }

        // Let
        Expr::Let(bindings, body) => {
            let mut new_env = env.child();
            eval_bindings(bindings, &mut new_env)?;
            eval_expr(body, &new_env)
        }

        // Lambda
        Expr::Lambda(pattern, body) => Ok(Value::Lambda(Closure {
            pattern: pattern.clone(),
            body: *body.clone(),
            env: env.clone(),
        })),
    }
}

fn eval_attrset(set: &AttrSet, env: &Env) -> Result<Value, EvalError> {
    let mut attrs = NixAttrs::new();

    if set.recursive {
        // For rec sets: two-pass. First evaluate all bindings, then they can reference each other.
        let mut rec_env = env.child();
        // First pass: evaluate and bind
        for binding in &set.bindings {
            match binding {
                Binding::AttrPath(path, expr) => {
                    if path.len() == 1 {
                        let key = eval_attr_name(&path[0], &rec_env)?;
                        let value = eval_expr(expr, &rec_env)?;
                        rec_env.bind(key.clone(), value.clone());
                        attrs.insert(key, value);
                    } else {
                        let key = eval_attr_name(&path[0], &rec_env)?;
                        let value = build_nested_attr(&path[1..], expr, &rec_env)?;
                        attrs.insert(key, value);
                    }
                }
                Binding::Inherit(from, names) => {
                    for name in names {
                        let value = if let Some(from_expr) = from {
                            let source = eval_expr(from_expr, &rec_env)?;
                            source.as_attrs()?.get(name).cloned().ok_or_else(|| {
                                EvalError::AttrNotFound(name.clone())
                            })?
                        } else {
                            rec_env
                                .lookup(name)
                                .ok_or_else(|| EvalError::UndefinedVar(name.clone()))?
                        };
                        rec_env.bind(name.clone(), value.clone());
                        attrs.insert(name.clone(), value);
                    }
                }
            }
        }
    } else {
        // Non-recursive: evaluate in parent env
        for binding in &set.bindings {
            match binding {
                Binding::AttrPath(path, expr) => {
                    if path.len() == 1 {
                        let key = eval_attr_name(&path[0], env)?;
                        let value = eval_expr(expr, env)?;
                        attrs.insert(key, value);
                    } else {
                        let key = eval_attr_name(&path[0], env)?;
                        let value = build_nested_attr(&path[1..], expr, env)?;
                        attrs.insert(key, value);
                    }
                }
                Binding::Inherit(from, names) => {
                    for name in names {
                        let value = if let Some(from_expr) = from {
                            let source = eval_expr(from_expr, env)?;
                            source.as_attrs()?.get(name).cloned().ok_or_else(|| {
                                EvalError::AttrNotFound(name.clone())
                            })?
                        } else {
                            env.lookup(name)
                                .ok_or_else(|| EvalError::UndefinedVar(name.clone()))?
                        };
                        attrs.insert(name.clone(), value);
                    }
                }
            }
        }
    }

    Ok(Value::Attrs(attrs))
}

fn build_nested_attr(path: &[AttrName], expr: &Expr, env: &Env) -> Result<Value, EvalError> {
    if path.is_empty() {
        return eval_expr(expr, env);
    }
    let key = eval_attr_name(&path[0], env)?;
    let inner = build_nested_attr(&path[1..], expr, env)?;
    let mut attrs = NixAttrs::new();
    attrs.insert(key, inner);
    Ok(Value::Attrs(attrs))
}

fn eval_attr_name(name: &AttrName, env: &Env) -> Result<String, EvalError> {
    match name {
        AttrName::Static(s) => Ok(s.clone()),
        AttrName::Dynamic(expr) => {
            let val = eval_expr(expr, env)?;
            Ok(val.as_string()?.to_string())
        }
    }
}

fn eval_bindings(bindings: &[Binding], env: &mut Env) -> Result<(), EvalError> {
    for binding in bindings {
        match binding {
            Binding::AttrPath(path, expr) => {
                if path.len() == 1 {
                    let key = eval_attr_name(&path[0], env)?;
                    let value = eval_expr(expr, env)?;
                    env.bind(key, value);
                }
            }
            Binding::Inherit(from, names) => {
                for name in names {
                    let value = if let Some(from_expr) = from {
                        let source = eval_expr(from_expr, env)?;
                        source.as_attrs()?.get(name).cloned().ok_or_else(|| {
                            EvalError::AttrNotFound(name.clone())
                        })?
                    } else {
                        env.lookup(name)
                            .ok_or_else(|| EvalError::UndefinedVar(name.clone()))?
                    };
                    env.bind(name.clone(), value);
                }
            }
        }
    }
    Ok(())
}

fn eval_binop(op: BinOp, lhs: &Expr, rhs: &Expr, env: &Env) -> Result<Value, EvalError> {
    // Short-circuit for && and ||
    match op {
        BinOp::And => {
            let l = eval_expr(lhs, env)?.as_bool()?;
            if !l { return Ok(Value::Bool(false)); }
            return eval_expr(rhs, env);
        }
        BinOp::Or => {
            let l = eval_expr(lhs, env)?.as_bool()?;
            if l { return Ok(Value::Bool(true)); }
            return eval_expr(rhs, env);
        }
        BinOp::Impl => {
            let l = eval_expr(lhs, env)?.as_bool()?;
            if !l { return Ok(Value::Bool(true)); }
            return eval_expr(rhs, env);
        }
        _ => {}
    }

    let l = eval_expr(lhs, env)?;
    let r = eval_expr(rhs, env)?;

    match op {
        BinOp::Add => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a + b)),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
            (Value::Int(a), Value::Float(b)) => Ok(Value::Float(*a as f64 + b)),
            (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + *b as f64)),
            (Value::String(a), Value::String(b)) => Ok(Value::String(format!("{a}{b}"))),
            (Value::Path(a), Value::String(b)) => Ok(Value::Path(format!("{a}{b}"))),
            (Value::Path(a), Value::Path(b)) => Ok(Value::Path(format!("{a}/{b}"))),
            _ => Err(EvalError::TypeError(format!(
                "cannot add {} and {}", l.type_name(), r.type_name()
            ))),
        },
        BinOp::Sub => num_op(&l, &r, |a, b| a - b, |a, b| a - b),
        BinOp::Mul => num_op(&l, &r, |a, b| a * b, |a, b| a * b),
        BinOp::Div => {
            match (&l, &r) {
                (Value::Int(_), Value::Int(0)) => Err(EvalError::DivisionByZero),
                _ => num_op(&l, &r, |a, b| a / b, |a, b| a / b),
            }
        }
        BinOp::Eq => Ok(Value::Bool(l == r)),
        BinOp::Neq => Ok(Value::Bool(l != r)),
        BinOp::Lt => compare(&l, &r, |o| o == std::cmp::Ordering::Less),
        BinOp::Le => compare(&l, &r, |o| o != std::cmp::Ordering::Greater),
        BinOp::Gt => compare(&l, &r, |o| o == std::cmp::Ordering::Greater),
        BinOp::Ge => compare(&l, &r, |o| o != std::cmp::Ordering::Less),
        BinOp::Update => {
            let la = l.as_attrs()?;
            let ra = r.as_attrs()?;
            Ok(Value::Attrs(la.update(ra)))
        }
        BinOp::Concat => {
            let mut la = l.as_list()?.to_vec();
            la.extend_from_slice(r.as_list()?);
            Ok(Value::List(la))
        }
        BinOp::And | BinOp::Or | BinOp::Impl => unreachable!("handled above"),
    }
}

fn num_op(
    l: &Value,
    r: &Value,
    int_op: impl Fn(i64, i64) -> i64,
    float_op: impl Fn(f64, f64) -> f64,
) -> Result<Value, EvalError> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => Ok(Value::Int(int_op(*a, *b))),
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(float_op(*a, *b))),
        (Value::Int(a), Value::Float(b)) => Ok(Value::Float(float_op(*a as f64, *b))),
        (Value::Float(a), Value::Int(b)) => Ok(Value::Float(float_op(*a, *b as f64))),
        _ => Err(EvalError::TypeError(format!(
            "cannot perform arithmetic on {} and {}",
            l.type_name(),
            r.type_name()
        ))),
    }
}

fn compare(
    l: &Value,
    r: &Value,
    pred: impl Fn(std::cmp::Ordering) -> bool,
) -> Result<Value, EvalError> {
    let ord = match (l, r) {
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)).unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        _ => return Err(EvalError::TypeError(format!(
            "cannot compare {} and {}", l.type_name(), r.type_name()
        ))),
    };
    Ok(Value::Bool(pred(ord)))
}

/// Apply a function to an argument.
pub fn apply(func: Value, arg: Value) -> Result<Value, EvalError> {
    match func {
        Value::Lambda(closure) => {
            let mut call_env = closure.env.child();
            bind_pattern(&closure.pattern, &arg, &mut call_env)?;
            eval_expr(&closure.body, &call_env)
        }
        Value::Builtin(b) => (b.func)(&[arg]),
        _ => Err(EvalError::TypeError(format!(
            "cannot call {}", func.type_name()
        ))),
    }
}

fn bind_pattern(pattern: &Pattern, arg: &Value, env: &mut Env) -> Result<(), EvalError> {
    match pattern {
        Pattern::Ident(name) => {
            env.bind(name.clone(), arg.clone());
        }
        Pattern::Formals { formals, ellipsis, name } => {
            let attrs = arg.as_attrs()?;

            if let Some(n) = name {
                env.bind(n.clone(), arg.clone());
            }

            for formal in formals {
                let value = if let Some(v) = attrs.get(&formal.name) {
                    v.clone()
                } else if let Some(ref default) = formal.default {
                    eval_expr(default, env)?
                } else {
                    return Err(EvalError::TypeError(format!(
                        "missing argument '{}'", formal.name
                    )));
                };
                env.bind(formal.name.clone(), value);
            }

            if !ellipsis {
                for key in attrs.keys() {
                    if !formals.iter().any(|f| f.name == *key) {
                        return Err(EvalError::TypeError(format!(
                            "unexpected argument '{key}'"
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(input: &str) -> Value {
        eval(input).unwrap()
    }

    #[test]
    fn eval_int() { assert_eq!(ev("42"), Value::Int(42)); }

    #[test]
    fn eval_float() { assert_eq!(ev("3.14"), Value::Float(3.14)); }

    #[test]
    fn eval_string() { assert_eq!(ev(r#""hello""#), Value::String("hello".to_string())); }

    #[test]
    fn eval_bool() { assert_eq!(ev("true"), Value::Bool(true)); }

    #[test]
    fn eval_null() { assert_eq!(ev("null"), Value::Null); }

    #[test]
    fn eval_arithmetic() {
        assert_eq!(ev("1 + 2"), Value::Int(3));
        assert_eq!(ev("10 - 3"), Value::Int(7));
        assert_eq!(ev("2 * 3"), Value::Int(6));
        assert_eq!(ev("10 / 3"), Value::Int(3));
    }

    #[test]
    fn eval_precedence() {
        assert_eq!(ev("1 + 2 * 3"), Value::Int(7));
        assert_eq!(ev("(1 + 2) * 3"), Value::Int(9));
    }

    #[test]
    fn eval_comparison() {
        assert_eq!(ev("1 == 1"), Value::Bool(true));
        assert_eq!(ev("1 == 2"), Value::Bool(false));
        assert_eq!(ev("1 < 2"), Value::Bool(true));
        assert_eq!(ev("2 <= 2"), Value::Bool(true));
    }

    #[test]
    fn eval_logic() {
        assert_eq!(ev("true && false"), Value::Bool(false));
        assert_eq!(ev("true || false"), Value::Bool(true));
        assert_eq!(ev("!true"), Value::Bool(false));
    }

    #[test]
    fn eval_string_concat() {
        assert_eq!(ev(r#""hello" + " " + "world""#), Value::String("hello world".to_string()));
    }

    #[test]
    fn eval_if() {
        assert_eq!(ev("if true then 1 else 2"), Value::Int(1));
        assert_eq!(ev("if false then 1 else 2"), Value::Int(2));
    }

    #[test]
    fn eval_let() {
        assert_eq!(ev("let x = 1; in x"), Value::Int(1));
        assert_eq!(ev("let x = 1; y = 2; in x + y"), Value::Int(3));
    }

    #[test]
    fn eval_nested_let() {
        assert_eq!(ev("let a = 1; b = let c = 2; in c; in a + b"), Value::Int(3));
    }

    #[test]
    fn eval_lambda() {
        assert_eq!(ev("(x: x + 1) 41"), Value::Int(42));
    }

    #[test]
    fn eval_lambda_multi_arg() {
        assert_eq!(ev("(x: y: x + y) 1 2"), Value::Int(3));
    }

    #[test]
    fn eval_list() {
        let v = ev("[1 2 3]");
        assert_eq!(v, Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]));
    }

    #[test]
    fn eval_list_concat() {
        let v = ev("[1 2] ++ [3 4]");
        assert_eq!(v, Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)]));
    }

    #[test]
    fn eval_attrset() {
        let v = ev("{ a = 1; b = 2; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
        } else {
            panic!("expected attrset");
        }
    }

    #[test]
    fn eval_select() {
        assert_eq!(ev("{ a = 42; }.a"), Value::Int(42));
    }

    #[test]
    fn eval_select_or() {
        assert_eq!(ev("{ a = 42; }.b or 0"), Value::Int(0));
    }

    #[test]
    fn eval_has_attr() {
        assert_eq!(ev("{ a = 1; } ? a"), Value::Bool(true));
        assert_eq!(ev("{ a = 1; } ? b"), Value::Bool(false));
    }

    #[test]
    fn eval_update() {
        let v = ev("{ a = 1; b = 2; } // { b = 3; c = 4; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(3)));
            assert_eq!(attrs.get("c"), Some(&Value::Int(4)));
        } else {
            panic!("expected attrset");
        }
    }

    #[test]
    fn eval_with() {
        assert_eq!(ev("with { x = 42; }; x"), Value::Int(42));
    }

    #[test]
    fn eval_assert() {
        assert_eq!(ev("assert true; 42"), Value::Int(42));
        assert!(eval("assert false; 42").is_err());
    }

    #[test]
    fn eval_formals() {
        assert_eq!(ev("({ a, b }: a + b) { a = 1; b = 2; }"), Value::Int(3));
    }

    #[test]
    fn eval_formals_default() {
        assert_eq!(ev("({ a, b ? 10 }: a + b) { a = 1; }"), Value::Int(11));
    }

    #[test]
    fn eval_formals_ellipsis() {
        assert_eq!(ev("({ a, ... }: a) { a = 1; b = 2; }"), Value::Int(1));
    }

    #[test]
    fn eval_named_formals() {
        assert_eq!(ev("(args @ { a }: args.a) { a = 42; }"), Value::Int(42));
    }

    #[test]
    fn eval_rec_attrset() {
        assert_eq!(ev("(rec { a = 1; b = a + 1; }).b"), Value::Int(2));
    }

    #[test]
    fn eval_negation() {
        assert_eq!(ev("-42"), Value::Int(-42));
    }

    #[test]
    fn eval_float_arithmetic() {
        assert_eq!(ev("1.5 + 2.5"), Value::Float(4.0));
        assert_eq!(ev("1 + 1.5"), Value::Float(2.5));
    }

    #[test]
    fn eval_division_by_zero() {
        assert!(eval("1 / 0").is_err());
    }

    #[test]
    fn eval_builtins_available() {
        assert_eq!(ev("builtins.typeOf 42"), Value::String("int".to_string()));
        assert_eq!(ev("builtins.typeOf true"), Value::String("bool".to_string()));
    }

    #[test]
    fn eval_builtins_length() {
        assert_eq!(ev("builtins.length [1 2 3]"), Value::Int(3));
    }

    #[test]
    fn eval_builtins_head_tail() {
        assert_eq!(ev("builtins.head [1 2 3]"), Value::Int(1));
        assert_eq!(ev("builtins.length (builtins.tail [1 2 3])"), Value::Int(2));
    }

    #[test]
    fn eval_builtins_add() {
        assert_eq!(ev("builtins.add 1 2"), Value::Int(3));
    }

    #[test]
    fn eval_builtins_to_string() {
        assert_eq!(ev("builtins.toString 42"), Value::String("42".to_string()));
    }

    #[test]
    fn eval_implication() {
        assert_eq!(ev("false -> true"), Value::Bool(true));
        assert_eq!(ev("true -> false"), Value::Bool(false));
        assert_eq!(ev("true -> true"), Value::Bool(true));
    }

    // ── New tests ────────────────────────────────────────

    #[test]
    fn eval_error_undefined_variable() {
        let result = eval("nonexistent");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("undefined variable"));
    }

    #[test]
    fn eval_error_type_mismatch_arithmetic() {
        let result = eval(r#"1 + "hello""#);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("cannot add") || msg.contains("type"));
    }

    #[test]
    fn eval_error_unexpected_argument() {
        let result = eval("({ a }: a) { a = 1; b = 2; }");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("unexpected argument"));
    }

    #[test]
    fn eval_error_missing_required_argument() {
        let result = eval("({ a, b }: a + b) { a = 1; }");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("missing argument"));
    }

    #[test]
    fn eval_builtins_attr_names_sorted() {
        let v = ev("builtins.attrNames { z = 1; a = 2; m = 3; }");
        // BTreeMap keys are already sorted
        assert_eq!(
            v,
            Value::List(vec![
                Value::String("a".to_string()),
                Value::String("m".to_string()),
                Value::String("z".to_string()),
            ]),
        );
    }

    #[test]
    fn eval_builtins_attr_values() {
        let v = ev("builtins.attrValues { a = 1; b = 2; }");
        // BTreeMap iteration is sorted by key, so a=1 first, b=2 second
        assert_eq!(v, Value::List(vec![Value::Int(1), Value::Int(2)]));
    }

    #[test]
    fn eval_builtins_is_null() {
        assert_eq!(ev("builtins.isNull null"), Value::Bool(true));
        assert_eq!(ev("builtins.isNull 1"), Value::Bool(false));
    }

    #[test]
    fn eval_builtins_is_int() {
        assert_eq!(ev("builtins.isInt 42"), Value::Bool(true));
        assert_eq!(ev("builtins.isInt 3.14"), Value::Bool(false));
    }

    #[test]
    fn eval_builtins_is_bool() {
        assert_eq!(ev("builtins.isBool true"), Value::Bool(true));
        assert_eq!(ev("builtins.isBool 0"), Value::Bool(false));
    }

    #[test]
    fn eval_builtins_is_string() {
        assert_eq!(ev(r#"builtins.isString "hi""#), Value::Bool(true));
        assert_eq!(ev("builtins.isString 1"), Value::Bool(false));
    }

    #[test]
    fn eval_builtins_is_list() {
        assert_eq!(ev("builtins.isList [1 2]"), Value::Bool(true));
        assert_eq!(ev("builtins.isList {}"), Value::Bool(false));
    }

    #[test]
    fn eval_builtins_is_attrs() {
        assert_eq!(ev("builtins.isAttrs {}"), Value::Bool(true));
        assert_eq!(ev("builtins.isAttrs []"), Value::Bool(false));
    }

    #[test]
    fn eval_builtins_string_length() {
        assert_eq!(ev(r#"builtins.stringLength "hello""#), Value::Int(5));
        assert_eq!(ev(r#"builtins.stringLength """#), Value::Int(0));
    }

    #[test]
    fn eval_builtins_to_json_roundtrip() {
        // toJSON produces a JSON string; fromJSON parses it back
        assert_eq!(
            ev(r#"builtins.fromJSON (builtins.toJSON 42)"#),
            Value::Int(42),
        );
        assert_eq!(
            ev(r#"builtins.fromJSON (builtins.toJSON [1 2 3])"#),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn eval_builtins_from_json() {
        assert_eq!(
            ev(r#"builtins.fromJSON "{\"a\": 1}""#),
            {
                let mut attrs = NixAttrs::new();
                attrs.insert("a".to_string(), Value::Int(1));
                Value::Attrs(attrs)
            },
        );
        assert_eq!(ev(r#"builtins.fromJSON "null""#), Value::Null);
        assert_eq!(ev(r#"builtins.fromJSON "true""#), Value::Bool(true));
    }

    #[test]
    fn eval_nested_function_application() {
        // (f 1) 2 where f = x: y: x + y
        assert_eq!(ev("(x: y: x + y) 1 2"), Value::Int(3));
        // equivalent parenthesized form
        assert_eq!(ev("((x: y: x + y) 1) 2"), Value::Int(3));
    }

    #[test]
    fn eval_recursive_let() {
        assert_eq!(ev("let a = 1; b = a + 1; in b"), Value::Int(2));
        assert_eq!(ev("let a = 1; b = a + 1; c = b + 1; in c"), Value::Int(3));
    }

    #[test]
    fn eval_string_comparison() {
        assert_eq!(ev(r#""a" < "b""#), Value::Bool(true));
        assert_eq!(ev(r#""b" < "a""#), Value::Bool(false));
        assert_eq!(ev(r#""abc" == "abc""#), Value::Bool(true));
        assert_eq!(ev(r#""abc" != "def""#), Value::Bool(true));
    }

    #[test]
    fn eval_list_in_attrset() {
        let v = ev("{ x = [1 2 3]; }.x");
        assert_eq!(
            v,
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn eval_nested_attrset_select() {
        assert_eq!(ev("{ a = { b = 42; }; }.a.b"), Value::Int(42));
    }

    #[test]
    fn eval_let_shadows_outer() {
        assert_eq!(
            ev("let x = 1; in let x = 2; in x"),
            Value::Int(2),
        );
    }

    #[test]
    fn eval_with_provides_scope() {
        // `with` scope is available for name resolution
        assert_eq!(
            ev("with { x = 42; y = 10; }; x + y"),
            Value::Int(52),
        );
    }

    #[test]
    fn eval_list_equality() {
        assert_eq!(ev("[1 2] == [1 2]"), Value::Bool(true));
        assert_eq!(ev("[1 2] == [1 3]"), Value::Bool(false));
    }

    #[test]
    fn eval_attrset_equality() {
        assert_eq!(ev("{ a = 1; } == { a = 1; }"), Value::Bool(true));
        assert_eq!(ev("{ a = 1; } == { a = 2; }"), Value::Bool(false));
    }
}
