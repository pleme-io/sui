//! Tree-walking Nix evaluator using rnix's typed AST.

use std::cell::Cell;

use rnix::ast::{self, AstToken, HasEntry, InterpolPart};
use rowan::ast::AstNode;

use crate::builtins;
use crate::value::*;

thread_local! { static EVAL_DEPTH: Cell<usize> = const { Cell::new(0) }; }
const MAX_EVAL_DEPTH: usize = 10_000;

/// RAII guard that decrements the eval depth counter on drop.
struct DepthGuard;

impl DepthGuard {
    fn enter() -> Result<Self, EvalError> {
        EVAL_DEPTH.with(|d| {
            let depth = d.get();
            if depth > MAX_EVAL_DEPTH {
                return Err(EvalError::TypeError(
                    "infinite recursion (eval depth exceeded)".into(),
                ));
            }
            d.set(depth + 1);
            Ok(DepthGuard)
        })
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        EVAL_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Evaluate a Nix expression string.
pub fn eval(input: &str) -> Result<Value, EvalError> {
    let parse = rnix::Root::parse(input);
    if !parse.errors().is_empty() {
        let msgs: Vec<String> = parse.errors().iter().map(|e| e.to_string()).collect();
        return Err(EvalError::ParseError(msgs.join("; ")));
    }
    let root = parse.tree();
    let expr = root.expr().ok_or_else(|| EvalError::ParseError("empty expression".to_string()))?;
    let mut env = Env::new();
    builtins::register(&mut env);
    eval_expr(&expr, &env)
}

/// Evaluate an rnix expression in an environment.
pub fn eval_expr(expr: &ast::Expr, env: &Env) -> Result<Value, EvalError> {
    let _guard = DepthGuard::enter()?;
    match expr {
        ast::Expr::Literal(lit) => eval_literal(&lit),

        ast::Expr::Str(s) => eval_str(s, env),

        ast::Expr::PathAbs(p) => {
            let text = p.syntax().text().to_string();
            Ok(Value::Path(text))
        }
        ast::Expr::PathRel(p) => {
            let text = p.syntax().text().to_string();
            Ok(Value::Path(text))
        }
        ast::Expr::PathHome(p) => {
            let text = p.syntax().text().to_string();
            Ok(Value::Path(text))
        }
        ast::Expr::PathSearch(p) => {
            let text = p.syntax().text().to_string();
            Ok(Value::Path(text))
        }

        ast::Expr::Ident(ident) => {
            let name = ident_text(ident);
            match name.as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                "null" => Ok(Value::Null),
                _ => env
                    .lookup(&name)
                    .ok_or_else(|| EvalError::UndefinedVar(name)),
            }
        }

        ast::Expr::List(list) => {
            let values: Result<Vec<_>, _> = list.items().map(|e| eval_expr(&e, env)).collect();
            Ok(Value::List(values?))
        }

        ast::Expr::AttrSet(set) => eval_attrset(set, env),

        ast::Expr::Select(sel) => {
            let base_expr = sel.expr().ok_or_else(|| {
                EvalError::ParseError("select missing expression".to_string())
            })?;
            let mut value = eval_expr(&base_expr, env)?;
            let attrpath = sel.attrpath().ok_or_else(|| {
                EvalError::ParseError("select missing attrpath".to_string())
            })?;
            for attr in attrpath.attrs() {
                let key = eval_attr(&attr, env)?;
                match value {
                    Value::Attrs(ref attrs) => {
                        if let Some(v) = attrs.get(&key) {
                            value = v.clone();
                        } else if let Some(def) = sel.default_expr() {
                            return eval_expr(&def, env);
                        } else {
                            return Err(EvalError::AttrNotFound(key));
                        }
                    }
                    _ => {
                        return Err(EvalError::TypeError(format!(
                            "cannot select from {}",
                            value.type_name()
                        )));
                    }
                }
            }
            Ok(value)
        }

        ast::Expr::HasAttr(ha) => {
            let base_expr = ha.expr().ok_or_else(|| {
                EvalError::ParseError("hasattr missing expression".to_string())
            })?;
            let mut value = eval_expr(&base_expr, env)?;
            let attrpath = ha.attrpath().ok_or_else(|| {
                EvalError::ParseError("hasattr missing attrpath".to_string())
            })?;
            for attr in attrpath.attrs() {
                let key = eval_attr(&attr, env)?;
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

        ast::Expr::UnaryOp(op) => {
            let inner = op
                .expr()
                .ok_or_else(|| EvalError::ParseError("unary op missing expr".to_string()))?;
            let val = eval_expr(&inner, env)?;
            let kind = op
                .operator()
                .ok_or_else(|| EvalError::ParseError("unary op missing operator".to_string()))?;
            match kind {
                ast::UnaryOpKind::Negate => match val {
                    Value::Int(n) => Ok(Value::Int(-n)),
                    Value::Float(f) => Ok(Value::Float(-f)),
                    _ => Err(EvalError::TypeError(format!(
                        "cannot negate {}",
                        val.type_name()
                    ))),
                },
                ast::UnaryOpKind::Invert => Ok(Value::Bool(!val.as_bool()?)),
            }
        }

        ast::Expr::BinOp(binop) => {
            let lhs_expr = binop
                .lhs()
                .ok_or_else(|| EvalError::ParseError("binop missing lhs".to_string()))?;
            let rhs_expr = binop
                .rhs()
                .ok_or_else(|| EvalError::ParseError("binop missing rhs".to_string()))?;
            let kind = binop
                .operator()
                .ok_or_else(|| EvalError::ParseError("binop missing operator".to_string()))?;
            eval_binop(kind, &lhs_expr, &rhs_expr, env)
        }

        ast::Expr::Apply(app) => {
            let func_expr = app
                .lambda()
                .ok_or_else(|| EvalError::ParseError("apply missing function".to_string()))?;
            let arg_expr = app
                .argument()
                .ok_or_else(|| EvalError::ParseError("apply missing argument".to_string()))?;
            let func = eval_expr(&func_expr, env)?;
            let arg = eval_expr(&arg_expr, env)?;
            apply(func, arg)
        }

        ast::Expr::IfElse(ie) => {
            let cond = ie
                .condition()
                .ok_or_else(|| EvalError::ParseError("if missing condition".to_string()))?;
            let body = ie
                .body()
                .ok_or_else(|| EvalError::ParseError("if missing then body".to_string()))?;
            let else_body = ie
                .else_body()
                .ok_or_else(|| EvalError::ParseError("if missing else body".to_string()))?;
            if eval_expr(&cond, env)?.as_bool()? {
                eval_expr(&body, env)
            } else {
                eval_expr(&else_body, env)
            }
        }

        ast::Expr::Assert(assert) => {
            let cond = assert
                .condition()
                .ok_or_else(|| EvalError::ParseError("assert missing condition".to_string()))?;
            let body = assert
                .body()
                .ok_or_else(|| EvalError::ParseError("assert missing body".to_string()))?;
            if !eval_expr(&cond, env)?.as_bool()? {
                return Err(EvalError::AssertionFailed);
            }
            eval_expr(&body, env)
        }

        ast::Expr::With(with) => {
            let ns = with
                .namespace()
                .ok_or_else(|| EvalError::ParseError("with missing namespace".to_string()))?;
            let body = with
                .body()
                .ok_or_else(|| EvalError::ParseError("with missing body".to_string()))?;
            let scope = eval_expr(&ns, env)?;
            let attrs = scope.as_attrs()?.clone();
            let new_env = env.child().with_scope(attrs);
            eval_expr(&body, &new_env)
        }

        ast::Expr::LetIn(letin) => {
            let mut new_env = env.child();
            eval_entries(letin, &mut new_env)?;
            let body = letin
                .body()
                .ok_or_else(|| EvalError::ParseError("let missing body".to_string()))?;
            eval_expr(&body, &new_env)
        }

        ast::Expr::Lambda(lam) => {
            let param = lam
                .param()
                .ok_or_else(|| EvalError::ParseError("lambda missing param".to_string()))?;
            let body = lam
                .body()
                .ok_or_else(|| EvalError::ParseError("lambda missing body".to_string()))?;
            Ok(Value::Lambda(Closure {
                param,
                body,
                env: env.clone(),
            }))
        }

        ast::Expr::Paren(p) => {
            let inner = p
                .expr()
                .ok_or_else(|| EvalError::ParseError("paren missing expr".to_string()))?;
            eval_expr(&inner, env)
        }

        ast::Expr::Root(r) => {
            let inner = r
                .expr()
                .ok_or_else(|| EvalError::ParseError("root missing expr".to_string()))?;
            eval_expr(&inner, env)
        }

        ast::Expr::LegacyLet(ll) => {
            let mut new_env = env.child();
            eval_entries(ll, &mut new_env)?;
            // legacy let returns the `body` attr from its bindings
            new_env
                .lookup("body")
                .ok_or_else(|| EvalError::AttrNotFound("body".to_string()))
        }

        ast::Expr::CurPos(_) => Err(EvalError::NotImplemented("__curPos".to_string())),
        ast::Expr::Error(_) => Err(EvalError::ParseError("parse error node".to_string())),
    }
}

fn eval_literal(lit: &ast::Literal) -> Result<Value, EvalError> {
    use ast::LiteralKind;
    match lit.kind() {
        LiteralKind::Integer(tok) => {
            let n = tok
                .value()
                .map_err(|e| EvalError::ParseError(format!("invalid integer: {e}")))?;
            Ok(Value::Int(n))
        }
        LiteralKind::Float(tok) => {
            let f = tok
                .value()
                .map_err(|e| EvalError::ParseError(format!("invalid float: {e}")))?;
            Ok(Value::Float(f))
        }
        LiteralKind::Uri(tok) => Ok(Value::String(tok.syntax().text().to_string())),
    }
}

fn eval_str(s: &ast::Str, env: &Env) -> Result<Value, EvalError> {
    let mut result = String::new();
    for part in s.normalized_parts() {
        match part {
            InterpolPart::Literal(text) => result.push_str(&text),
            InterpolPart::Interpolation(interpol) => {
                let expr = interpol.expr().ok_or_else(|| {
                    EvalError::ParseError("interpolation missing expr".to_string())
                })?;
                let val = eval_expr(&expr, env)?;
                match val {
                    Value::String(s) => result.push_str(&s),
                    Value::Int(n) => result.push_str(&n.to_string()),
                    Value::Float(f) => result.push_str(&format!("{f}")),
                    Value::Bool(true) => result.push('1'),
                    Value::Bool(false) => {}
                    Value::Null => {}
                    Value::Path(p) => result.push_str(&p),
                    Value::Attrs(ref attrs) => {
                        // __toString protocol: if the attrset has __toString,
                        // call it with `self` to produce a string.
                        if let Some(to_str) = attrs.get("__toString") {
                            let s = apply(to_str.clone(), val.clone())?;
                            match s {
                                Value::String(s) => result.push_str(&s),
                                _ => {
                                    return Err(EvalError::TypeError(
                                        "__toString must return a string".to_string(),
                                    ));
                                }
                            }
                        } else {
                            return Err(EvalError::TypeError(format!(
                                "cannot coerce {} to string in interpolation",
                                val.type_name()
                            )));
                        }
                    }
                    _ => {
                        return Err(EvalError::TypeError(format!(
                            "cannot coerce {} to string in interpolation",
                            val.type_name()
                        )));
                    }
                }
            }
        }
    }
    Ok(Value::String(result))
}

fn eval_attr(attr: &ast::Attr, env: &Env) -> Result<String, EvalError> {
    match attr {
        ast::Attr::Ident(ident) => Ok(ident_text(ident)),
        ast::Attr::Dynamic(dyn_) => {
            let expr = dyn_
                .expr()
                .ok_or_else(|| EvalError::ParseError("dynamic attr missing expr".to_string()))?;
            let val = eval_expr(&expr, env)?;
            Ok(val.as_string()?.to_string())
        }
        ast::Attr::Str(s) => {
            let val = eval_str(s, env)?;
            Ok(val.as_string()?.to_string())
        }
    }
}

/// Get the text of an rnix Ident node.
fn ident_text(ident: &ast::Ident) -> String {
    // Ident's ident_token() returns TOKEN_IDENT, but `or` keyword gets
    // a TOKEN_OR token instead. Use syntax().text() which always works.
    ident.syntax().text().to_string()
}

fn eval_attrset(set: &ast::AttrSet, env: &Env) -> Result<Value, EvalError> {
    let mut attrs = NixAttrs::new();
    let is_rec = set.rec_token().is_some();

    if is_rec {
        let mut rec_env = env.child();
        for entry in set.entries() {
            match entry {
                ast::Entry::AttrpathValue(apv) => {
                    let attrpath = apv.attrpath().ok_or_else(|| {
                        EvalError::ParseError("binding missing attrpath".to_string())
                    })?;
                    let value_expr = apv.value().ok_or_else(|| {
                        EvalError::ParseError("binding missing value".to_string())
                    })?;
                    let path_keys: Vec<String> = attrpath
                        .attrs()
                        .map(|a| eval_attr(&a, &rec_env))
                        .collect::<Result<_, _>>()?;
                    if path_keys.len() == 1 {
                        let key = path_keys.into_iter().next().unwrap();
                        let value = eval_expr(&value_expr, &rec_env)?;
                        rec_env.bind(key.clone(), value.clone());
                        attrs.insert(key, value);
                    } else {
                        let key = path_keys[0].clone();
                        let value = build_nested_attr(&path_keys[1..], &value_expr, &rec_env)?;
                        attrs.insert(key, value);
                    }
                }
                ast::Entry::Inherit(inherit) => {
                    eval_inherit(&inherit, &rec_env, &mut attrs, Some(&mut rec_env.clone()))?;
                }
            }
        }
    } else {
        for entry in set.entries() {
            match entry {
                ast::Entry::AttrpathValue(apv) => {
                    let attrpath = apv.attrpath().ok_or_else(|| {
                        EvalError::ParseError("binding missing attrpath".to_string())
                    })?;
                    let value_expr = apv.value().ok_or_else(|| {
                        EvalError::ParseError("binding missing value".to_string())
                    })?;
                    let path_keys: Vec<String> = attrpath
                        .attrs()
                        .map(|a| eval_attr(&a, env))
                        .collect::<Result<_, _>>()?;
                    if path_keys.len() == 1 {
                        let key = path_keys.into_iter().next().unwrap();
                        let value = eval_expr(&value_expr, env)?;
                        attrs.insert(key, value);
                    } else {
                        let key = path_keys[0].clone();
                        let value = build_nested_attr(&path_keys[1..], &value_expr, env)?;
                        attrs.insert(key, value);
                    }
                }
                ast::Entry::Inherit(inherit) => {
                    eval_inherit(&inherit, env, &mut attrs, None)?;
                }
            }
        }
    }

    Ok(Value::Attrs(attrs))
}

fn eval_inherit(
    inherit: &ast::Inherit,
    env: &Env,
    attrs: &mut NixAttrs,
    bind_env: Option<&mut Env>,
) -> Result<(), EvalError> {
    if let Some(from) = inherit.from() {
        // inherit (expr) a b c;
        let source_expr = from
            .expr()
            .ok_or_else(|| EvalError::ParseError("inherit from missing expr".to_string()))?;
        let source = eval_expr(&source_expr, env)?;
        let source_attrs = source.as_attrs()?;
        for attr in inherit.attrs() {
            let name = eval_attr(&attr, env)?;
            let value = source_attrs
                .get(&name)
                .cloned()
                .ok_or_else(|| EvalError::AttrNotFound(name.clone()))?;
            if let Some(ref mut be) = bind_env.as_deref() {
                let _ = be; // we handle binding below
            }
            attrs.insert(name, value);
        }
    } else {
        // inherit a b c;
        for attr in inherit.attrs() {
            let name = eval_attr(&attr, env)?;
            let value = env
                .lookup(&name)
                .ok_or_else(|| EvalError::UndefinedVar(name.clone()))?;
            attrs.insert(name, value);
        }
    }
    Ok(())
}

fn build_nested_attr(
    path: &[String],
    expr: &ast::Expr,
    env: &Env,
) -> Result<Value, EvalError> {
    if path.is_empty() {
        return eval_expr(expr, env);
    }
    let key = path[0].clone();
    let inner = build_nested_attr(&path[1..], expr, env)?;
    let mut attrs = NixAttrs::new();
    attrs.insert(key, inner);
    Ok(Value::Attrs(attrs))
}

/// Evaluate entries from any HasEntry node (LetIn, AttrSet, LegacyLet).
fn eval_entries<N: HasEntry + AstNode>(node: &N, env: &mut Env) -> Result<(), EvalError> {
    for entry in node.entries() {
        match entry {
            ast::Entry::AttrpathValue(apv) => {
                let attrpath = apv.attrpath().ok_or_else(|| {
                    EvalError::ParseError("binding missing attrpath".to_string())
                })?;
                let value_expr = apv.value().ok_or_else(|| {
                    EvalError::ParseError("binding missing value".to_string())
                })?;
                let path_keys: Vec<String> = attrpath
                    .attrs()
                    .map(|a| eval_attr(&a, env))
                    .collect::<Result<_, _>>()?;
                if path_keys.len() == 1 {
                    let key = path_keys.into_iter().next().unwrap();
                    let value = eval_expr(&value_expr, env)?;
                    env.bind(key, value);
                }
                // Multi-key paths in let are not standard; skip for now.
            }
            ast::Entry::Inherit(inherit) => {
                if let Some(from) = inherit.from() {
                    let source_expr = from.expr().ok_or_else(|| {
                        EvalError::ParseError("inherit from missing expr".to_string())
                    })?;
                    let source = eval_expr(&source_expr, env)?;
                    let source_attrs = source.as_attrs()?;
                    for attr in inherit.attrs() {
                        let name = eval_attr(&attr, env)?;
                        let value = source_attrs
                            .get(&name)
                            .cloned()
                            .ok_or_else(|| EvalError::AttrNotFound(name.clone()))?;
                        env.bind(name, value);
                    }
                } else {
                    for attr in inherit.attrs() {
                        let name = eval_attr(&attr, env)?;
                        let value = env
                            .lookup(&name)
                            .ok_or_else(|| EvalError::UndefinedVar(name.clone()))?;
                        env.bind(name, value);
                    }
                }
            }
        }
    }
    Ok(())
}

fn eval_binop(
    op: ast::BinOpKind,
    lhs: &ast::Expr,
    rhs: &ast::Expr,
    env: &Env,
) -> Result<Value, EvalError> {
    // Short-circuit for && and ||
    match op {
        ast::BinOpKind::And => {
            let l = eval_expr(lhs, env)?.as_bool()?;
            if !l {
                return Ok(Value::Bool(false));
            }
            return eval_expr(rhs, env);
        }
        ast::BinOpKind::Or => {
            let l = eval_expr(lhs, env)?.as_bool()?;
            if l {
                return Ok(Value::Bool(true));
            }
            return eval_expr(rhs, env);
        }
        ast::BinOpKind::Implication => {
            let l = eval_expr(lhs, env)?.as_bool()?;
            if !l {
                return Ok(Value::Bool(true));
            }
            return eval_expr(rhs, env);
        }
        _ => {}
    }

    let l = eval_expr(lhs, env)?;
    let r = eval_expr(rhs, env)?;

    match op {
        ast::BinOpKind::Add => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a + b)),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
            (Value::Int(a), Value::Float(b)) => Ok(Value::Float(*a as f64 + b)),
            (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + *b as f64)),
            (Value::String(a), Value::String(b)) => Ok(Value::String(format!("{a}{b}"))),
            (Value::Path(a), Value::String(b)) => Ok(Value::Path(format!("{a}{b}"))),
            (Value::Path(a), Value::Path(b)) => Ok(Value::Path(format!("{a}/{b}"))),
            _ => Err(EvalError::TypeError(format!(
                "cannot add {} and {}",
                l.type_name(),
                r.type_name()
            ))),
        },
        ast::BinOpKind::Sub => num_op(&l, &r, |a, b| a - b, |a, b| a - b),
        ast::BinOpKind::Mul => num_op(&l, &r, |a, b| a * b, |a, b| a * b),
        ast::BinOpKind::Div => match (&l, &r) {
            (Value::Int(_), Value::Int(0)) => Err(EvalError::DivisionByZero),
            _ => num_op(&l, &r, |a, b| a / b, |a, b| a / b),
        },
        ast::BinOpKind::Equal => Ok(Value::Bool(l == r)),
        ast::BinOpKind::NotEqual => Ok(Value::Bool(l != r)),
        ast::BinOpKind::Less => compare(&l, &r, |o| o == std::cmp::Ordering::Less),
        ast::BinOpKind::LessOrEq => compare(&l, &r, |o| o != std::cmp::Ordering::Greater),
        ast::BinOpKind::More => compare(&l, &r, |o| o == std::cmp::Ordering::Greater),
        ast::BinOpKind::MoreOrEq => compare(&l, &r, |o| o != std::cmp::Ordering::Less),
        ast::BinOpKind::Update => {
            let la = l.as_attrs()?;
            let ra = r.as_attrs()?;
            Ok(Value::Attrs(la.update(ra)))
        }
        ast::BinOpKind::Concat => {
            let mut la = l.as_list()?.to_vec();
            la.extend_from_slice(r.as_list()?);
            Ok(Value::List(la))
        }
        ast::BinOpKind::And | ast::BinOpKind::Or | ast::BinOpKind::Implication => {
            unreachable!("handled above")
        }
        ast::BinOpKind::PipeRight | ast::BinOpKind::PipeLeft => {
            Err(EvalError::NotImplemented("pipe operators".to_string()))
        }
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
        (Value::Float(a), Value::Float(b)) => {
            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Value::Int(a), Value::Float(b)) => (*a as f64)
            .partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::Float(a), Value::Int(b)) => a
            .partial_cmp(&(*b as f64))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        _ => {
            return Err(EvalError::TypeError(format!(
                "cannot compare {} and {}",
                l.type_name(),
                r.type_name()
            )));
        }
    };
    Ok(Value::Bool(pred(ord)))
}

/// Apply a function to an argument.
///
/// Supports `__functor`: if `func` is an attrset with a `__functor` key,
/// calls `__functor self arg` (the Nix `__functor` protocol).
pub fn apply(func: Value, arg: Value) -> Result<Value, EvalError> {
    match func {
        Value::Lambda(closure) => {
            let mut call_env = closure.env.child();
            bind_param(&closure.param, &arg, &mut call_env)?;
            eval_expr(&closure.body, &call_env)
        }
        Value::Builtin(b) => (b.func)(&[arg]),
        Value::Attrs(ref attrs) => {
            if let Some(functor) = attrs.get("__functor") {
                // __functor protocol: (functor self) arg
                let partial = apply(functor.clone(), func.clone())?;
                apply(partial, arg)
            } else {
                Err(EvalError::TypeError(format!(
                    "cannot call {} (missing __functor)",
                    func.type_name()
                )))
            }
        }
        _ => Err(EvalError::TypeError(format!(
            "cannot call {}",
            func.type_name()
        ))),
    }
}

fn bind_param(param: &ast::Param, arg: &Value, env: &mut Env) -> Result<(), EvalError> {
    match param {
        ast::Param::IdentParam(ip) => {
            let ident = ip
                .ident()
                .ok_or_else(|| EvalError::ParseError("ident param missing ident".to_string()))?;
            let name = ident_text(&ident);
            env.bind(name, arg.clone());
        }
        ast::Param::Pattern(pat) => {
            let attrs = arg.as_attrs()?;

            // @-binding (either `args @ { ... }` or `{ ... } @ args`)
            if let Some(pat_bind) = pat.pat_bind() {
                if let Some(ident) = pat_bind.ident() {
                    let name = ident_text(&ident);
                    env.bind(name, arg.clone());
                }
            }

            let has_ellipsis = pat.ellipsis_token().is_some();
            let entries: Vec<ast::PatEntry> = pat.pat_entries().collect();

            for entry in &entries {
                let ident = entry.ident().ok_or_else(|| {
                    EvalError::ParseError("pat entry missing ident".to_string())
                })?;
                let name = ident_text(&ident);
                let value = if let Some(v) = attrs.get(&name) {
                    v.clone()
                } else if let Some(default_expr) = entry.default() {
                    eval_expr(&default_expr, env)?
                } else {
                    return Err(EvalError::TypeError(format!(
                        "missing argument '{name}'"
                    )));
                };
                env.bind(name, value);
            }

            if !has_ellipsis {
                for key in attrs.keys() {
                    if !entries.iter().any(|e| {
                        e.ident()
                            .map(|i| ident_text(&i) == *key)
                            .unwrap_or(false)
                    }) {
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

    // ═══════════════════════════════════════════════════════════
    // 1. LITERAL TYPES
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn literal_int_large_zero_negative() {
        // Large positive integer (within i64 range)
        assert_eq!(ev("9223372036854775807"), Value::Int(i64::MAX));
        // Zero
        assert_eq!(ev("0"), Value::Int(0));
        // Negative via unary negate
        assert_eq!(ev("-1"), Value::Int(-1));
        assert_eq!(ev("-999999"), Value::Int(-999999));
    }

    #[test]
    fn literal_float_small_large() {
        assert_eq!(ev("0.001"), Value::Float(0.001));
        assert_eq!(ev("999999.999"), Value::Float(999999.999));
        // Float with scientific notation via expression (1e6 parsed by rnix)
        assert_eq!(ev("1.0e3"), Value::Float(1000.0));
        assert_eq!(ev("1.5e2"), Value::Float(150.0));
    }

    #[test]
    fn literal_string_empty_and_escapes() {
        assert_eq!(ev(r#""""#), Value::String("".to_string()));
        // Escape sequences within strings
        assert_eq!(ev(r#""hello\nworld""#), Value::String("hello\nworld".to_string()));
        assert_eq!(ev(r#""tab\there""#), Value::String("tab\there".to_string()));
    }

    #[test]
    fn literal_multiline_string() {
        // Indented string ('' ... '')
        assert_eq!(
            ev("''hello''"),
            Value::String("hello".to_string()),
        );
        // Multiline indented string strips common indentation
        assert_eq!(
            ev("''\n  line1\n  line2\n''"),
            Value::String("line1\nline2\n".to_string()),
        );
    }

    #[test]
    fn literal_paths() {
        // Relative path
        assert_eq!(ev("./foo"), Value::Path("./foo".to_string()));
        // Absolute path
        assert_eq!(ev("/nix/store/abc"), Value::Path("/nix/store/abc".to_string()));
        // Home path
        assert_eq!(ev("~/myfile"), Value::Path("~/myfile".to_string()));
    }

    #[test]
    fn literal_null_true_false_standalone() {
        assert_eq!(ev("null"), Value::Null);
        assert_eq!(ev("true"), Value::Bool(true));
        assert_eq!(ev("false"), Value::Bool(false));
    }

    // ═══════════════════════════════════════════════════════════
    // 2. OPERATORS — COMPLETE COVERAGE
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn op_arithmetic_int() {
        assert_eq!(ev("100 + 200"), Value::Int(300));
        assert_eq!(ev("50 - 30"), Value::Int(20));
        assert_eq!(ev("7 * 8"), Value::Int(56));
        assert_eq!(ev("17 / 3"), Value::Int(5)); // integer division
    }

    #[test]
    fn op_arithmetic_float() {
        assert_eq!(ev("1.5 + 2.5"), Value::Float(4.0));
        assert_eq!(ev("5.0 - 1.5"), Value::Float(3.5));
        assert_eq!(ev("2.0 * 3.0"), Value::Float(6.0));
        assert_eq!(ev("7.0 / 2.0"), Value::Float(3.5));
    }

    #[test]
    fn op_arithmetic_mixed_int_float() {
        // int + float => float
        assert_eq!(ev("1 + 2.5"), Value::Float(3.5));
        assert_eq!(ev("2.5 + 1"), Value::Float(3.5));
        // int * float => float
        assert_eq!(ev("2 * 1.5"), Value::Float(3.0));
        // float - int => float
        assert_eq!(ev("5.5 - 2"), Value::Float(3.5));
    }

    #[test]
    fn op_string_concat() {
        assert_eq!(ev(r#""foo" + "bar""#), Value::String("foobar".to_string()));
        assert_eq!(ev(r#""" + "x""#), Value::String("x".to_string()));
        assert_eq!(ev(r#""a" + "" + "b""#), Value::String("ab".to_string()));
    }

    #[test]
    fn op_path_concat() {
        // path + string
        assert_eq!(ev(r#"./foo + "/bar""#), Value::Path("./foo/bar".to_string()));
        // path + path (should join with /)
        assert_eq!(ev("./a + ./b"), Value::Path("./a/./b".to_string()));
    }

    #[test]
    fn op_comparison_ints() {
        assert_eq!(ev("1 < 2"), Value::Bool(true));
        assert_eq!(ev("2 < 1"), Value::Bool(false));
        assert_eq!(ev("2 > 1"), Value::Bool(true));
        assert_eq!(ev("1 > 2"), Value::Bool(false));
        assert_eq!(ev("2 <= 2"), Value::Bool(true));
        assert_eq!(ev("3 <= 2"), Value::Bool(false));
        assert_eq!(ev("2 >= 2"), Value::Bool(true));
        assert_eq!(ev("1 >= 2"), Value::Bool(false));
    }

    #[test]
    fn op_comparison_floats() {
        assert_eq!(ev("1.5 < 2.5"), Value::Bool(true));
        assert_eq!(ev("2.5 > 1.5"), Value::Bool(true));
        assert_eq!(ev("1.5 <= 1.5"), Value::Bool(true));
        assert_eq!(ev("1.5 >= 1.5"), Value::Bool(true));
    }

    #[test]
    fn op_comparison_strings() {
        assert_eq!(ev(r#""apple" < "banana""#), Value::Bool(true));
        assert_eq!(ev(r#""banana" > "apple""#), Value::Bool(true));
        assert_eq!(ev(r#""abc" == "abc""#), Value::Bool(true));
        assert_eq!(ev(r#""abc" != "xyz""#), Value::Bool(true));
        assert_eq!(ev(r#""abc" <= "abd""#), Value::Bool(true));
        assert_eq!(ev(r#""abc" >= "abb""#), Value::Bool(true));
    }

    #[test]
    fn op_equality_various_types() {
        assert_eq!(ev("null == null"), Value::Bool(true));
        assert_eq!(ev("true == true"), Value::Bool(true));
        assert_eq!(ev("false == false"), Value::Bool(true));
        assert_eq!(ev("true == false"), Value::Bool(false));
        assert_eq!(ev("1 == 1"), Value::Bool(true));
        assert_eq!(ev("1 != 2"), Value::Bool(true));
        // Different types are not equal
        assert_eq!(ev(r#"1 == "1""#), Value::Bool(false));
        assert_eq!(ev("null == false"), Value::Bool(false));
    }

    #[test]
    fn op_logic_short_circuit() {
        // false && <error> should NOT evaluate the RHS
        assert_eq!(ev("false && (1 / 0 == 0)"), Value::Bool(false));
        // true || <error> should NOT evaluate the RHS
        assert_eq!(ev("true || (1 / 0 == 0)"), Value::Bool(true));
    }

    #[test]
    fn op_logic_full() {
        assert_eq!(ev("true && true"), Value::Bool(true));
        assert_eq!(ev("true && false"), Value::Bool(false));
        assert_eq!(ev("false && true"), Value::Bool(false));
        assert_eq!(ev("false && false"), Value::Bool(false));
        assert_eq!(ev("true || true"), Value::Bool(true));
        assert_eq!(ev("true || false"), Value::Bool(true));
        assert_eq!(ev("false || true"), Value::Bool(true));
        assert_eq!(ev("false || false"), Value::Bool(false));
        assert_eq!(ev("!true"), Value::Bool(false));
        assert_eq!(ev("!false"), Value::Bool(true));
    }

    #[test]
    fn op_implication_truth_table() {
        // false -> anything = true
        assert_eq!(ev("false -> false"), Value::Bool(true));
        assert_eq!(ev("false -> true"), Value::Bool(true));
        // true -> x = x
        assert_eq!(ev("true -> true"), Value::Bool(true));
        assert_eq!(ev("true -> false"), Value::Bool(false));
    }

    #[test]
    fn op_implication_short_circuit() {
        // false -> <error> should NOT evaluate the RHS
        assert_eq!(ev("false -> (1 / 0 == 0)"), Value::Bool(true));
    }

    #[test]
    fn op_update_merge() {
        let v = ev("{ a = 1; } // { b = 2; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn op_update_right_wins() {
        assert_eq!(ev("({ a = 1; } // { a = 2; }).a"), Value::Int(2));
    }

    #[test]
    fn op_list_concat() {
        assert_eq!(
            ev("[1 2] ++ [3 4]"),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)]),
        );
        // Empty list concat
        assert_eq!(ev("[] ++ [1]"), Value::List(vec![Value::Int(1)]));
        assert_eq!(ev("[1] ++ []"), Value::List(vec![Value::Int(1)]));
    }

    #[test]
    fn op_has_attr_present_and_absent() {
        assert_eq!(ev("{ x = 1; y = 2; } ? x"), Value::Bool(true));
        assert_eq!(ev("{ x = 1; } ? z"), Value::Bool(false));
        assert_eq!(ev("{} ? anything"), Value::Bool(false));
    }

    #[test]
    fn op_unary_negate() {
        assert_eq!(ev("-42"), Value::Int(-42));
        assert_eq!(ev("-3.14"), Value::Float(-3.14));
        // Double negate
        assert_eq!(ev("- -5"), Value::Int(5));
    }

    // ═══════════════════════════════════════════════════════════
    // 3. CONTROL FLOW
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn control_if_true_branch() {
        assert_eq!(ev("if true then 42 else 0"), Value::Int(42));
    }

    #[test]
    fn control_if_false_branch() {
        assert_eq!(ev("if false then 42 else 0"), Value::Int(0));
    }

    #[test]
    fn control_if_nested() {
        assert_eq!(
            ev("if true then (if false then 1 else 2) else 3"),
            Value::Int(2),
        );
        assert_eq!(
            ev("if false then 1 else (if true then 2 else 3)"),
            Value::Int(2),
        );
    }

    #[test]
    fn control_assert_passing() {
        assert_eq!(ev("assert 1 == 1; 42"), Value::Int(42));
        assert_eq!(ev("assert true; true"), Value::Bool(true));
    }

    #[test]
    fn control_assert_failing() {
        assert!(eval("assert false; 42").is_err());
        assert!(eval("assert 1 == 2; 42").is_err());
    }

    #[test]
    fn control_with_basic_scope() {
        assert_eq!(ev("with { a = 1; b = 2; }; a + b"), Value::Int(3));
    }

    #[test]
    fn control_with_lexical_precedence() {
        // let binding takes precedence over with scope
        assert_eq!(
            ev("let x = 10; in with { x = 99; }; x"),
            Value::Int(10),
        );
    }

    #[test]
    fn control_with_nested() {
        assert_eq!(
            ev("with { a = 1; }; with { b = 2; }; a + b"),
            Value::Int(3),
        );
    }

    #[test]
    fn control_let_simple_and_multiple() {
        assert_eq!(ev("let x = 5; in x"), Value::Int(5));
        assert_eq!(ev("let x = 1; y = 2; z = 3; in x + y + z"), Value::Int(6));
    }

    #[test]
    fn control_let_shadow_outer() {
        assert_eq!(
            ev("let x = 1; in let x = 2; in x"),
            Value::Int(2),
        );
    }

    #[test]
    fn control_let_recursive_reference() {
        assert_eq!(ev("let a = 1; b = a + 1; in b"), Value::Int(2));
        assert_eq!(ev("let a = 1; b = a + 1; c = b + 1; in c"), Value::Int(3));
    }

    #[test]
    fn control_nested_let_expression() {
        assert_eq!(
            ev("let a = let b = 1; in b; in a"),
            Value::Int(1),
        );
        assert_eq!(
            ev("let a = let b = 10; in b + 5; in a * 2"),
            Value::Int(30),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 4. FUNCTIONS — COMPLETE COVERAGE
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn func_identity_lambda() {
        assert_eq!(ev("(x: x) 42"), Value::Int(42));
        assert_eq!(ev(r#"(x: x) "hello""#), Value::String("hello".to_string()));
    }

    #[test]
    fn func_curried_two_args() {
        assert_eq!(ev("(x: y: x + y) 3 4"), Value::Int(7));
    }

    #[test]
    fn func_curried_three_args() {
        assert_eq!(ev("(a: b: c: a + b + c) 1 2 3"), Value::Int(6));
    }

    #[test]
    fn func_formals_basic() {
        assert_eq!(ev("({ a, b }: a + b) { a = 3; b = 7; }"), Value::Int(10));
    }

    #[test]
    fn func_formals_with_defaults() {
        assert_eq!(ev("({ a, b ? 10 }: a + b) { a = 5; }"), Value::Int(15));
        // Providing the default-able argument overrides the default
        assert_eq!(ev("({ a, b ? 10 }: a + b) { a = 5; b = 20; }"), Value::Int(25));
    }

    #[test]
    fn func_formals_with_ellipsis() {
        assert_eq!(ev("({ a, ... }: a) { a = 1; b = 2; c = 3; }"), Value::Int(1));
    }

    #[test]
    fn func_named_formals_at_before() {
        // args @ { a, b }: ...
        assert_eq!(
            ev("(args @ { a, b }: args.a + args.b) { a = 3; b = 4; }"),
            Value::Int(7),
        );
    }

    #[test]
    fn func_named_formals_at_after() {
        // { a, b } @ args: ...
        assert_eq!(
            ev("({ a, b } @ args: args.a + args.b) { a = 10; b = 20; }"),
            Value::Int(30),
        );
    }

    #[test]
    fn func_nested_application() {
        // Explicit parenthesized application
        assert_eq!(ev("((x: y: x * y) 3) 4"), Value::Int(12));
    }

    #[test]
    fn func_higher_order_map() {
        assert_eq!(
            ev("builtins.map (x: x * 2) [1 2 3]"),
            Value::List(vec![Value::Int(2), Value::Int(4), Value::Int(6)]),
        );
    }

    #[test]
    fn func_higher_order_filter() {
        assert_eq!(
            ev("builtins.filter (x: x > 2) [1 2 3 4 5]"),
            Value::List(vec![Value::Int(3), Value::Int(4), Value::Int(5)]),
        );
    }

    #[test]
    fn func_higher_order_foldl() {
        // Sum of list via foldl'
        assert_eq!(
            ev("builtins.foldl' (acc: x: acc + x) 0 [1 2 3 4]"),
            Value::Int(10),
        );
    }

    #[test]
    fn func_as_attrset_value() {
        assert_eq!(
            ev("let s = { f = x: x + 1; }; in s.f 5"),
            Value::Int(6),
        );
    }

    #[test]
    fn func_immediate_application() {
        assert_eq!(ev("(x: x * x) 7"), Value::Int(49));
    }

    #[test]
    fn func_in_let_binding() {
        assert_eq!(
            ev("let double = x: x * 2; in double 21"),
            Value::Int(42),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 5. ATTRIBUTE SETS — COMPLETE COVERAGE
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn attrs_empty_set() {
        let v = ev("{}");
        if let Value::Attrs(attrs) = v {
            assert!(attrs.is_empty());
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn attrs_simple() {
        assert_eq!(ev("{ a = 1; }.a"), Value::Int(1));
    }

    #[test]
    fn attrs_nested_access() {
        assert_eq!(ev("{ a = { b = { c = 42; }; }; }.a.b.c"), Value::Int(42));
    }

    #[test]
    fn attrs_recursive_set() {
        assert_eq!(ev("(rec { a = 1; b = a + 1; c = b + 1; }).c"), Value::Int(3));
    }

    #[test]
    fn attrs_update_disjoint() {
        let v = ev("{ a = 1; } // { b = 2; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.len(), 2);
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn attrs_update_override() {
        assert_eq!(ev("({ a = 1; } // { a = 2; }).a"), Value::Int(2));
    }

    #[test]
    fn attrs_has_attr_operator() {
        assert_eq!(ev("{ a = 1; } ? a"), Value::Bool(true));
        assert_eq!(ev("{ a = 1; } ? b"), Value::Bool(false));
    }

    #[test]
    fn attrs_select_with_default() {
        assert_eq!(ev("{ a = 1; }.a or 99"), Value::Int(1));
        assert_eq!(ev("{}.missing or 99"), Value::Int(99));
        assert_eq!(ev("{ a = 1; }.b or 42"), Value::Int(42));
    }

    #[test]
    fn attrs_nested_attr_path_in_binding() {
        // { a.b = 1; } creates { a = { b = 1; }; }
        assert_eq!(ev("{ a.b = 1; }.a.b"), Value::Int(1));
    }

    #[test]
    fn attrs_inherit_from_scope() {
        assert_eq!(ev("let x = 1; y = 2; in { inherit x y; }.x"), Value::Int(1));
        assert_eq!(ev("let x = 1; y = 2; in { inherit x y; }.y"), Value::Int(2));
    }

    #[test]
    fn attrs_inherit_from_expr() {
        assert_eq!(
            ev("{ inherit ({ a = 42; b = 10; }) a; }.a"),
            Value::Int(42),
        );
    }

    #[test]
    fn attrs_dynamic_attr_name() {
        assert_eq!(
            ev(r#"let name = "x"; in { ${name} = 42; }.x"#),
            Value::Int(42),
        );
    }

    #[test]
    fn attrs_attr_names_sorted() {
        assert_eq!(
            ev("builtins.attrNames { z = 1; m = 2; a = 3; }"),
            Value::List(vec![
                Value::String("a".to_string()),
                Value::String("m".to_string()),
                Value::String("z".to_string()),
            ]),
        );
    }

    #[test]
    fn attrs_attr_values_follow_key_order() {
        // BTreeMap iteration order: a=1, b=2, c=3
        assert_eq!(
            ev("builtins.attrValues { c = 3; a = 1; b = 2; }"),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn attrs_update_is_shallow() {
        // // is a shallow merge; nested attrs are replaced, not merged
        assert_eq!(
            ev("({ a = { x = 1; }; } // { a = { y = 2; }; }).a ? x"),
            Value::Bool(false),
        );
        assert_eq!(
            ev("({ a = { x = 1; }; } // { a = { y = 2; }; }).a.y"),
            Value::Int(2),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 6. LISTS — COMPLETE COVERAGE
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn list_empty() {
        assert_eq!(ev("[]"), Value::List(vec![]));
    }

    #[test]
    fn list_single_element() {
        assert_eq!(ev("[1]"), Value::List(vec![Value::Int(1)]));
    }

    #[test]
    fn list_mixed_types() {
        assert_eq!(
            ev(r#"[1 "two" true null]"#),
            Value::List(vec![
                Value::Int(1),
                Value::String("two".to_string()),
                Value::Bool(true),
                Value::Null,
            ]),
        );
    }

    #[test]
    fn list_nested() {
        assert_eq!(
            ev("[[1 2] [3 4]]"),
            Value::List(vec![
                Value::List(vec![Value::Int(1), Value::Int(2)]),
                Value::List(vec![Value::Int(3), Value::Int(4)]),
            ]),
        );
    }

    #[test]
    fn list_concat_operator() {
        assert_eq!(
            ev("[1] ++ [2] ++ [3]"),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn list_builtins_length() {
        assert_eq!(ev("builtins.length [1 2 3]"), Value::Int(3));
        assert_eq!(ev("builtins.length []"), Value::Int(0));
    }

    #[test]
    fn list_builtins_elem_at() {
        assert_eq!(ev("builtins.elemAt [10 20 30] 0"), Value::Int(10));
        assert_eq!(ev("builtins.elemAt [10 20 30] 1"), Value::Int(20));
        assert_eq!(ev("builtins.elemAt [10 20 30] 2"), Value::Int(30));
    }

    #[test]
    fn list_equality() {
        assert_eq!(ev("[1 2 3] == [1 2 3]"), Value::Bool(true));
        assert_eq!(ev("[1 2] == [1 2 3]"), Value::Bool(false));
        assert_eq!(ev("[] == []"), Value::Bool(true));
    }

    // ═══════════════════════════════════════════════════════════
    // 7. STRING INTERPOLATION
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn interp_simple_variable() {
        assert_eq!(
            ev(r#"let name = "world"; in "hello ${name}""#),
            Value::String("hello world".to_string()),
        );
    }

    #[test]
    fn interp_nested_expression() {
        assert_eq!(
            ev(r#""result: ${builtins.toString (1 + 2)}""#),
            Value::String("result: 3".to_string()),
        );
    }

    #[test]
    fn interp_int_coercion() {
        // Ints are coerced to string in interpolation
        assert_eq!(
            ev(r#"let x = 42; in "count: ${builtins.toString x}""#),
            Value::String("count: 42".to_string()),
        );
    }

    #[test]
    fn interp_multiple() {
        assert_eq!(
            ev(r#"let a = "foo"; b = "bar"; in "${a} and ${b}""#),
            Value::String("foo and bar".to_string()),
        );
    }

    #[test]
    fn interp_in_let() {
        assert_eq!(
            ev(r#"let x = "world"; in "hello ${x}""#),
            Value::String("hello world".to_string()),
        );
    }

    #[test]
    fn interp_empty_result() {
        assert_eq!(
            ev(r#"let x = ""; in "a${x}b""#),
            Value::String("ab".to_string()),
        );
    }

    #[test]
    fn interp_path_in_string_context() {
        // Paths are coerced to strings in interpolation
        assert_eq!(
            ev(r#""path: ${./foo}""#),
            Value::String("path: ./foo".to_string()),
        );
    }

    #[test]
    fn interp_adjacent_interpolations() {
        assert_eq!(
            ev(r#"let a = "x"; b = "y"; in "${a}${b}""#),
            Value::String("xy".to_string()),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 8. BUILTINS — VERIFY ALL MAJOR ONES
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn builtins_map_filter_foldl() {
        // map
        assert_eq!(
            ev("builtins.map (x: x + 10) [1 2 3]"),
            Value::List(vec![Value::Int(11), Value::Int(12), Value::Int(13)]),
        );
        // filter
        assert_eq!(
            ev("builtins.filter (x: x > 1) [1 2 3]"),
            Value::List(vec![Value::Int(2), Value::Int(3)]),
        );
        // foldl' — product
        assert_eq!(
            ev("builtins.foldl' (a: b: a * b) 1 [2 3 4]"),
            Value::Int(24),
        );
    }

    #[test]
    fn builtins_map_attrs() {
        assert_eq!(
            ev("(builtins.mapAttrs (name: value: value * 2) { a = 1; b = 2; }).a"),
            Value::Int(2),
        );
        assert_eq!(
            ev("(builtins.mapAttrs (name: value: value * 2) { a = 1; b = 2; }).b"),
            Value::Int(4),
        );
    }

    #[test]
    fn builtins_list_to_attrs() {
        assert_eq!(
            ev(r#"(builtins.listToAttrs [{ name = "x"; value = 1; } { name = "y"; value = 2; }]).x"#),
            Value::Int(1),
        );
    }

    #[test]
    fn builtins_concat_map() {
        assert_eq!(
            ev("builtins.concatMap (x: [x (x * 2)]) [1 2 3]"),
            Value::List(vec![
                Value::Int(1), Value::Int(2),
                Value::Int(2), Value::Int(4),
                Value::Int(3), Value::Int(6),
            ]),
        );
    }

    #[test]
    fn builtins_concat_lists() {
        assert_eq!(
            ev("builtins.concatLists [[1 2] [3] [4 5]]"),
            Value::List(vec![
                Value::Int(1), Value::Int(2), Value::Int(3),
                Value::Int(4), Value::Int(5),
            ]),
        );
    }

    #[test]
    fn builtins_concat_strings_sep() {
        assert_eq!(
            ev(r#"builtins.concatStringsSep ", " ["a" "b" "c"]"#),
            Value::String("a, b, c".to_string()),
        );
        assert_eq!(
            ev(r#"builtins.concatStringsSep "" ["x" "y"]"#),
            Value::String("xy".to_string()),
        );
    }

    #[test]
    fn builtins_replace_strings() {
        assert_eq!(
            ev(r#"builtins.replaceStrings ["o"] ["0"] "foobar""#),
            Value::String("f00bar".to_string()),
        );
        assert_eq!(
            ev(r#"builtins.replaceStrings ["hello"] ["goodbye"] "hello world""#),
            Value::String("goodbye world".to_string()),
        );
    }

    #[test]
    fn builtins_has_prefix_has_suffix() {
        assert_eq!(ev(r#"builtins.hasPrefix "he" "hello""#), Value::Bool(true));
        assert_eq!(ev(r#"builtins.hasPrefix "xx" "hello""#), Value::Bool(false));
        assert_eq!(ev(r#"builtins.hasSuffix "lo" "hello""#), Value::Bool(true));
        assert_eq!(ev(r#"builtins.hasSuffix "xx" "hello""#), Value::Bool(false));
    }

    #[test]
    fn builtins_all_any() {
        assert_eq!(ev("builtins.all (x: x > 0) [1 2 3]"), Value::Bool(true));
        assert_eq!(ev("builtins.all (x: x > 1) [1 2 3]"), Value::Bool(false));
        assert_eq!(ev("builtins.any (x: x > 2) [1 2 3]"), Value::Bool(true));
        assert_eq!(ev("builtins.any (x: x > 5) [1 2 3]"), Value::Bool(false));
    }

    #[test]
    fn builtins_sort() {
        assert_eq!(
            ev("builtins.sort (a: b: a < b) [3 1 2]"),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn builtins_remove_attrs() {
        let v = ev(r#"builtins.removeAttrs { a = 1; b = 2; c = 3; } ["b" "c"]"#);
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.len(), 1);
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert!(attrs.get("b").is_none());
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_intersect_attrs() {
        let v = ev("builtins.intersectAttrs { a = 1; b = 2; } { b = 20; c = 30; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.len(), 1);
            // intersectAttrs returns values from the second set
            assert_eq!(attrs.get("b"), Some(&Value::Int(20)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_type_of_all_types() {
        assert_eq!(ev("builtins.typeOf null"), Value::String("null".to_string()));
        assert_eq!(ev("builtins.typeOf true"), Value::String("bool".to_string()));
        assert_eq!(ev("builtins.typeOf 42"), Value::String("int".to_string()));
        assert_eq!(ev("builtins.typeOf 3.14"), Value::String("float".to_string()));
        assert_eq!(ev(r#"builtins.typeOf "hi""#), Value::String("string".to_string()));
        assert_eq!(ev("builtins.typeOf [1]"), Value::String("list".to_string()));
        assert_eq!(ev("builtins.typeOf {}"), Value::String("set".to_string()));
        assert_eq!(ev("builtins.typeOf (x: x)"), Value::String("lambda".to_string()));
    }

    #[test]
    fn builtins_is_type_checks() {
        assert_eq!(ev("builtins.isNull null"), Value::Bool(true));
        assert_eq!(ev("builtins.isNull 0"), Value::Bool(false));
        assert_eq!(ev("builtins.isInt 42"), Value::Bool(true));
        assert_eq!(ev("builtins.isInt 3.14"), Value::Bool(false));
        assert_eq!(ev("builtins.isBool true"), Value::Bool(true));
        assert_eq!(ev("builtins.isBool 1"), Value::Bool(false));
        assert_eq!(ev(r#"builtins.isString "x""#), Value::Bool(true));
        assert_eq!(ev("builtins.isString 1"), Value::Bool(false));
        assert_eq!(ev("builtins.isList []"), Value::Bool(true));
        assert_eq!(ev("builtins.isList {}"), Value::Bool(false));
        assert_eq!(ev("builtins.isAttrs {}"), Value::Bool(true));
        assert_eq!(ev("builtins.isAttrs []"), Value::Bool(false));
        assert_eq!(ev("builtins.isFunction (x: x)"), Value::Bool(true));
        assert_eq!(ev("builtins.isFunction 1"), Value::Bool(false));
        assert_eq!(ev("builtins.isFloat 3.14"), Value::Bool(true));
        assert_eq!(ev("builtins.isFloat 1"), Value::Bool(false));
    }

    #[test]
    fn builtins_to_json_from_json_roundtrip() {
        // int roundtrip
        assert_eq!(ev("builtins.fromJSON (builtins.toJSON 42)"), Value::Int(42));
        // string roundtrip
        assert_eq!(
            ev(r#"builtins.fromJSON (builtins.toJSON "hello")"#),
            Value::String("hello".to_string()),
        );
        // list roundtrip
        assert_eq!(
            ev("builtins.fromJSON (builtins.toJSON [1 2 3])"),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
        // null roundtrip
        assert_eq!(ev("builtins.fromJSON (builtins.toJSON null)"), Value::Null);
        // bool roundtrip
        assert_eq!(ev("builtins.fromJSON (builtins.toJSON true)"), Value::Bool(true));
    }

    #[test]
    fn builtins_to_string_various() {
        assert_eq!(ev("builtins.toString 42"), Value::String("42".to_string()));
        assert_eq!(ev("builtins.toString true"), Value::String("1".to_string()));
        assert_eq!(ev("builtins.toString false"), Value::String("".to_string()));
        assert_eq!(ev("builtins.toString null"), Value::String("".to_string()));
        assert_eq!(ev(r#"builtins.toString "hello""#), Value::String("hello".to_string()));
    }

    #[test]
    fn builtins_function_args() {
        let v = ev("builtins.functionArgs ({ a, b ? 1 }: a)");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Bool(false))); // no default
            assert_eq!(attrs.get("b"), Some(&Value::Bool(true)));  // has default
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_gen_list() {
        assert_eq!(
            ev("builtins.genList (x: x * x) 5"),
            Value::List(vec![
                Value::Int(0), Value::Int(1), Value::Int(4),
                Value::Int(9), Value::Int(16),
            ]),
        );
        assert_eq!(ev("builtins.genList (x: x) 0"), Value::List(vec![]));
    }

    #[test]
    fn builtins_elem() {
        assert_eq!(ev("builtins.elem 2 [1 2 3]"), Value::Bool(true));
        assert_eq!(ev("builtins.elem 5 [1 2 3]"), Value::Bool(false));
        assert_eq!(ev("builtins.elem 1 []"), Value::Bool(false));
    }

    #[test]
    fn builtins_head_tail() {
        assert_eq!(ev("builtins.head [10 20 30]"), Value::Int(10));
        assert_eq!(
            ev("builtins.tail [10 20 30]"),
            Value::List(vec![Value::Int(20), Value::Int(30)]),
        );
    }

    #[test]
    fn builtins_string_length() {
        assert_eq!(ev(r#"builtins.stringLength "hello""#), Value::Int(5));
        assert_eq!(ev(r#"builtins.stringLength """#), Value::Int(0));
        assert_eq!(ev(r#"builtins.stringLength "abc def""#), Value::Int(7));
    }

    #[test]
    fn builtins_ceil_floor() {
        assert_eq!(ev("builtins.ceil 2.3"), Value::Int(3));
        assert_eq!(ev("builtins.ceil 2.0"), Value::Int(2));
        assert_eq!(ev("builtins.floor 2.9"), Value::Int(2));
        assert_eq!(ev("builtins.floor 2.0"), Value::Int(2));
        // Int coercion: ceil/floor on int should work via as_float()
        assert_eq!(ev("builtins.ceil 5"), Value::Int(5));
        assert_eq!(ev("builtins.floor 5"), Value::Int(5));
    }

    #[test]
    fn builtins_try_eval() {
        let v = ev("builtins.tryEval 42");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("success"), Some(&Value::Bool(true)));
            assert_eq!(attrs.get("value"), Some(&Value::Int(42)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_throw() {
        let result = eval(r#"builtins.throw "oops""#);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("oops"));
    }

    #[test]
    fn builtins_seq_deep_seq() {
        // seq forces first arg, returns second
        assert_eq!(ev("builtins.seq 1 42"), Value::Int(42));
        // deepSeq similarly
        assert_eq!(ev("builtins.deepSeq [1 2 3] 99"), Value::Int(99));
    }

    #[test]
    fn builtins_current_system() {
        let v = ev("builtins.currentSystem");
        if let Value::String(s) = v {
            // Should be a valid system string
            assert!(
                s == "aarch64-darwin"
                    || s == "x86_64-darwin"
                    || s == "aarch64-linux"
                    || s == "x86_64-linux",
                "unexpected system: {s}",
            );
        } else {
            panic!("expected string");
        }
    }

    // ═══════════════════════════════════════════════════════════
    // 9. REAL-WORLD NIXPKGS PATTERNS
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn pattern_mkif_like() {
        // lib.mkIf pattern: if condition then { key = value; } else {}
        assert_eq!(
            ev("(if true then { x = 1; } else {}).x"),
            Value::Int(1),
        );
        let v = ev("if false then { x = 1; } else {}");
        if let Value::Attrs(attrs) = v {
            assert!(attrs.is_empty());
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn pattern_optional_attrs() {
        // lib.optionalAttrs pattern
        assert_eq!(
            ev("let optionalAttrs = cond: attrs: if cond then attrs else {}; in (optionalAttrs true { a = 1; }).a"),
            Value::Int(1),
        );
        let v = ev("let optionalAttrs = cond: attrs: if cond then attrs else {}; in optionalAttrs false { a = 1; }");
        if let Value::Attrs(attrs) = v {
            assert!(attrs.is_empty());
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn pattern_filter_attrs_via_remove() {
        // lib.filterAttrs pattern via removeAttrs
        assert_eq!(
            ev(r#"(builtins.removeAttrs { a = 1; b = 2; c = 3; } ["b"]).a"#),
            Value::Int(1),
        );
        assert_eq!(
            ev(r#"(builtins.removeAttrs { a = 1; b = 2; c = 3; } ["b"]) ? b"#),
            Value::Bool(false),
        );
    }

    #[test]
    fn pattern_override() {
        // default // overrides pattern
        let v = ev(r#"
            let
                defaults = { debug = false; port = 8080; host = "localhost"; };
                overrides = { debug = true; port = 9090; };
            in defaults // overrides
        "#);
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("debug"), Some(&Value::Bool(true)));
            assert_eq!(attrs.get("port"), Some(&Value::Int(9090)));
            assert_eq!(attrs.get("host"), Some(&Value::String("localhost".to_string())));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn pattern_functor() {
        // { __functor = self: x: self.value + x; value = 10; } 5
        assert_eq!(
            ev("let s = { __functor = self: x: self.value + x; value = 10; }; in s 5"),
            Value::Int(15),
        );
    }

    #[test]
    fn pattern_platform_check() {
        // Check pattern: if builtins.currentSystem == "..." then ... else ...
        let v = ev(r#"if builtins.currentSystem == "aarch64-darwin" then "arm" else "other""#);
        // We just verify it evaluates without error and produces a string
        if let Value::String(_) = v {
            // ok
        } else {
            panic!("expected string");
        }
    }

    #[test]
    fn pattern_recursive_overlay_lambda_structure() {
        // Test the lambda structure of an overlay (self: super: { ... })
        let v = ev("let overlay = self: super: { pkg = 42; }; in overlay {} {}");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("pkg"), Some(&Value::Int(42)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn pattern_call_package_simplified() {
        // Simplified callPackage: f: f { inherit lib; }
        assert_eq!(
            ev("let callPkg = f: f { lib = { id = x: x; }; }; lib = { id = x: x; }; in callPkg ({ lib }: lib.id 42)"),
            Value::Int(42),
        );
    }

    #[test]
    fn pattern_derivation_like_attrset() {
        let v = ev(r#"{ type = "derivation"; name = "hello"; system = builtins.currentSystem; builder = "/bin/sh"; }"#);
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("type"), Some(&Value::String("derivation".to_string())));
            assert_eq!(attrs.get("name"), Some(&Value::String("hello".to_string())));
            assert_eq!(attrs.get("builder"), Some(&Value::String("/bin/sh".to_string())));
            // system should be a string
            assert!(matches!(attrs.get("system"), Some(Value::String(_))));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn pattern_module_system_simplified() {
        // Simplified NixOS module evaluation
        assert_eq!(
            ev(r#"
                let
                    eval = m: m { config = {}; lib = { mkDefault = x: x; }; };
                in eval ({ config, lib }: { result = lib.mkDefault 42; })
            "#),
            {
                let mut attrs = NixAttrs::new();
                attrs.insert("result".to_string(), Value::Int(42));
                Value::Attrs(attrs)
            },
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 10. ERROR HANDLING
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn error_undefined_variable() {
        let result = eval("nonexistent_var");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("undefined variable") || msg.contains("nonexistent_var"));
    }

    #[test]
    fn error_type_mismatch_arithmetic() {
        let result = eval(r#"1 + "hello""#);
        assert!(result.is_err());
    }

    #[test]
    fn error_missing_attribute() {
        let result = eval("{}.nonexistent");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("nonexistent") || msg.contains("not found"));
    }

    #[test]
    fn error_division_by_zero() {
        assert!(eval("1 / 0").is_err());
        assert!(eval("100 / 0").is_err());
    }

    #[test]
    fn error_missing_required_function_arg() {
        let result = eval("({ a, b }: a + b) { a = 1; }");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("missing argument"));
    }

    #[test]
    fn error_unexpected_function_arg() {
        let result = eval("({ a }: a) { a = 1; b = 2; }");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("unexpected argument"));
    }

    #[test]
    fn error_assertion_failure() {
        assert!(eval("assert false; 1").is_err());
        assert!(eval("assert 1 == 2; 1").is_err());
    }

    #[test]
    fn error_infinite_recursion() {
        // `let x = x; in x` should either hit the depth guard or fail on
        // undefined variable (since sequential let can't see its own binding).
        let result = eval("let x = x; in x");
        assert!(result.is_err());
    }

    #[test]
    fn error_infinite_recursion_via_lambda() {
        // A true infinite recursion via self-application — depth guard catches this.
        let result = eval("let f = x: f x; in f 1");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("infinite recursion") || msg.contains("eval depth") || msg.contains("undefined"),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // ADDITIONAL COVERAGE: edge cases and integration
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn integration_let_with_function_returning_attrset() {
        assert_eq!(
            ev("let mkPkg = name: { inherit name; version = 1; }; in (mkPkg \"hello\").name"),
            Value::String("hello".to_string()),
        );
    }

    #[test]
    fn integration_chained_updates() {
        assert_eq!(
            ev("({ a = 1; } // { b = 2; } // { c = 3; }).c"),
            Value::Int(3),
        );
    }

    #[test]
    fn integration_map_over_attrnames() {
        // Common nixpkgs pattern: map over attrNames
        assert_eq!(
            ev(r#"
                let
                    set = { a = 1; b = 2; };
                    names = builtins.attrNames set;
                in builtins.length names
            "#),
            Value::Int(2),
        );
    }

    #[test]
    fn integration_compose_functions() {
        // Function composition
        assert_eq!(
            ev("let compose = f: g: x: f (g x); double = x: x * 2; inc = x: x + 1; in compose double inc 5"),
            Value::Int(12), // (5 + 1) * 2
        );
    }

    #[test]
    fn integration_recursive_list_building() {
        // Build a list using genList and map
        assert_eq!(
            ev("builtins.map (x: x * x) (builtins.genList (x: x + 1) 4)"),
            Value::List(vec![Value::Int(1), Value::Int(4), Value::Int(9), Value::Int(16)]),
        );
    }

    #[test]
    fn integration_attrset_from_list() {
        // Convert list to attrset via listToAttrs + map
        let v = ev(r#"
            builtins.listToAttrs (builtins.map (x: { name = x; value = true; }) ["a" "b" "c"])
        "#);
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Bool(true)));
            assert_eq!(attrs.get("b"), Some(&Value::Bool(true)));
            assert_eq!(attrs.get("c"), Some(&Value::Bool(true)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn integration_nested_with_and_let() {
        assert_eq!(
            ev("let x = 10; in with { y = 20; }; x + y"),
            Value::Int(30),
        );
    }

    #[test]
    fn integration_complex_pattern_match() {
        // Complex function with defaults, ellipsis, and @ pattern
        assert_eq!(
            ev("(args @ { a, b ? 5, ... }: a + b + (if args ? c then args.c else 0)) { a = 1; c = 10; }"),
            Value::Int(16), // 1 + 5 + 10
        );
    }

    #[test]
    fn integration_substring() {
        assert_eq!(
            ev(r#"builtins.substring 0 5 "hello world""#),
            Value::String("hello".to_string()),
        );
        assert_eq!(
            ev(r#"builtins.substring 6 5 "hello world""#),
            Value::String("world".to_string()),
        );
    }

    #[test]
    fn integration_has_attr_on_nested() {
        // ? on nested attr paths
        assert_eq!(ev("{ a = { b = 1; }; } ? a"), Value::Bool(true));
        assert_eq!(
            ev("({ a = { b = 1; }; }.a) ? b"),
            Value::Bool(true),
        );
    }

    #[test]
    fn integration_cat_attrs() {
        assert_eq!(
            ev(r#"builtins.catAttrs "x" [{ x = 1; } { y = 2; } { x = 3; }]"#),
            Value::List(vec![Value::Int(1), Value::Int(3)]),
        );
    }

    #[test]
    fn integration_get_attr_builtin() {
        assert_eq!(
            ev(r#"builtins.getAttr "a" { a = 42; b = 10; }"#),
            Value::Int(42),
        );
    }

    #[test]
    fn integration_has_attr_builtin() {
        assert_eq!(
            ev(r#"builtins.hasAttr "a" { a = 1; }"#),
            Value::Bool(true),
        );
        assert_eq!(
            ev(r#"builtins.hasAttr "z" { a = 1; }"#),
            Value::Bool(false),
        );
    }

    #[test]
    fn integration_is_path() {
        assert_eq!(ev("builtins.isPath ./foo"), Value::Bool(true));
        assert_eq!(ev("builtins.isPath 42"), Value::Bool(false));
    }

    #[test]
    fn integration_builtins_trace() {
        // trace prints the first arg (as debug) and returns the second
        assert_eq!(ev(r#"builtins.trace "debug msg" 42"#), Value::Int(42));
    }

    #[test]
    fn integration_builtins_split() {
        // Nix spec: split returns alternating non-match strings and match group lists.
        // split "/" "a/b/c" => ["a" ["/"] "b" ["/"] "c"]
        assert_eq!(
            ev(r#"builtins.split "/" "a/b/c""#),
            Value::List(vec![
                Value::String("a".to_string()),
                Value::List(vec![Value::String("/".to_string())]),
                Value::String("b".to_string()),
                Value::List(vec![Value::String("/".to_string())]),
                Value::String("c".to_string()),
            ]),
        );
    }

    #[test]
    fn integration_deeply_nested_let() {
        // Deeply nested let-in expressions
        assert_eq!(
            ev("let a = let b = let c = 10; in c * 2; in b + 1; in a"),
            Value::Int(21),
        );
    }

    #[test]
    fn integration_if_in_attrset_value() {
        assert_eq!(
            ev("{ x = if true then 1 else 2; }.x"),
            Value::Int(1),
        );
    }

    #[test]
    fn integration_lambda_in_list() {
        // Store lambdas in a list and apply them
        assert_eq!(
            ev("let fs = [(x: x + 1) (x: x * 2)]; in (builtins.elemAt fs 0) 5"),
            Value::Int(6),
        );
        assert_eq!(
            ev("let fs = [(x: x + 1) (x: x * 2)]; in (builtins.elemAt fs 1) 5"),
            Value::Int(10),
        );
    }

    #[test]
    fn integration_nixpkgs_lib_id() {
        // lib.id = x: x
        assert_eq!(
            ev("let lib = { id = x: x; const = a: b: a; }; in lib.id 42"),
            Value::Int(42),
        );
        assert_eq!(
            ev("let lib = { id = x: x; const = a: b: a; }; in lib.const 1 2"),
            Value::Int(1),
        );
    }

    #[test]
    fn integration_multiple_inherit() {
        assert_eq!(
            ev("let a = 1; b = 2; c = 3; in { inherit a b c; }.b"),
            Value::Int(2),
        );
    }

    #[test]
    fn integration_rec_set_with_builtins() {
        assert_eq!(
            ev(r#"(rec { a = "hello"; b = builtins.stringLength a; }).b"#),
            Value::Int(5),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 11. __FUNCTOR PROTOCOL
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn functor_simple_callable_attrset() {
        assert_eq!(
            ev("let s = { __functor = self: x: x + 1; }; in s 41"),
            Value::Int(42),
        );
    }

    #[test]
    fn functor_with_self_reference() {
        assert_eq!(
            ev("let s = { __functor = self: x: self.base + x; base = 100; }; in s 23"),
            Value::Int(123),
        );
    }

    #[test]
    fn functor_updated_attrset() {
        // Override a field in the attrset, functor still works
        assert_eq!(
            ev(r#"
                let
                    mk = { __functor = self: x: self.n + x; n = 0; };
                    s = mk // { n = 50; };
                in s 7
            "#),
            Value::Int(57),
        );
    }

    #[test]
    fn functor_error_on_non_callable_attrset() {
        // Attrset without __functor should produce error when called
        let result = eval("let s = { a = 1; }; in s 5");
        assert!(result.is_err());
    }

    // ═══════════════════════════════════════════════════════════
    // 12. __TOSTRING PROTOCOL
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn to_string_protocol_in_interpolation() {
        assert_eq!(
            ev(r#"let s = { __toString = self: "world"; }; in "hello ${s}""#),
            Value::String("hello world".to_string()),
        );
    }

    #[test]
    fn to_string_protocol_accesses_self() {
        assert_eq!(
            ev(r#"let s = { __toString = self: self.val; val = "abc"; }; in "${s}""#),
            Value::String("abc".to_string()),
        );
    }

    #[test]
    fn to_string_protocol_via_builtin_to_string() {
        assert_eq!(
            ev(r#"builtins.toString { __toString = self: "via-builtin"; }"#),
            Value::String("via-builtin".to_string()),
        );
    }

    #[test]
    fn to_string_protocol_attrset_without_toString_fails() {
        // An attrset without __toString should fail in string context
        let result = eval(r#""${{}}"#);
        assert!(result.is_err());
    }

    // ═══════════════════════════════════════════════════════════
    // 13. NEWLY IMPLEMENTED BUILTINS (eval-level tests)
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn eval_builtins_concat_strings() {
        assert_eq!(
            ev(r#"builtins.concatStrings ["a" "b" "c"]"#),
            Value::String("abc".to_string()),
        );
        assert_eq!(
            ev(r#"builtins.concatStrings []"#),
            Value::String("".to_string()),
        );
    }

    #[test]
    fn eval_builtins_partition() {
        let v = ev("builtins.partition (x: x > 3) [1 2 3 4 5]");
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("right"), Some(&Value::List(vec![Value::Int(4), Value::Int(5)])));
            assert_eq!(a.get("wrong"), Some(&Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn eval_builtins_group_by() {
        let v = ev(r#"builtins.groupBy (x: if x > 0 then "pos" else "neg") [1 (0 - 2) 3 (0 - 4)]"#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("pos"), Some(&Value::List(vec![Value::Int(1), Value::Int(3)])));
            assert_eq!(a.get("neg"), Some(&Value::List(vec![Value::Int(-2), Value::Int(-4)])));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn eval_builtins_zip_attrs_with() {
        let v = ev("builtins.zipAttrsWith (n: vs: builtins.head vs) [{ a = 1; } { a = 2; b = 3; }]");
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("a"), Some(&Value::Int(1)));
            assert_eq!(a.get("b"), Some(&Value::Int(3)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn eval_builtins_compare_versions() {
        assert_eq!(ev(r#"builtins.compareVersions "2.0" "1.0""#), Value::Int(1));
        assert_eq!(ev(r#"builtins.compareVersions "1.0" "2.0""#), Value::Int(-1));
        assert_eq!(ev(r#"builtins.compareVersions "1.0" "1.0""#), Value::Int(0));
    }

    #[test]
    fn eval_builtins_parse_drv_name() {
        let v = ev(r#"builtins.parseDrvName "nix-2.3.4""#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("name"), Some(&Value::String("nix".to_string())));
            assert_eq!(a.get("version"), Some(&Value::String("2.3.4".to_string())));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn eval_builtins_base_name_of() {
        assert_eq!(
            ev(r#"builtins.baseNameOf "/foo/bar/baz""#),
            Value::String("baz".to_string()),
        );
    }

    #[test]
    fn eval_builtins_dir_of() {
        assert_eq!(
            ev(r#"builtins.dirOf "/foo/bar/baz""#),
            Value::String("/foo/bar".to_string()),
        );
    }

    #[test]
    fn eval_builtins_add_error_context() {
        assert_eq!(
            ev(r#"builtins.addErrorContext "some context" 42"#),
            Value::Int(42),
        );
    }

    #[test]
    fn eval_builtins_abort() {
        let result = eval(r#"builtins.abort "fatal error""#);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("fatal error"));
    }

    // ═══════════════════════════════════════════════════════════
    // 14. INDENTED STRINGS ('' ... '')
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn indented_string_simple() {
        assert_eq!(ev("''hello''"), Value::String("hello".to_string()));
    }

    #[test]
    fn indented_string_multiline_strips_indent() {
        assert_eq!(
            ev("''\n  line1\n  line2\n''"),
            Value::String("line1\nline2\n".to_string()),
        );
    }

    #[test]
    fn indented_string_with_interpolation() {
        let code = "let x = \"world\"; in ''hello ${x}''";
        assert_eq!(
            ev(code),
            Value::String("hello world".to_string()),
        );
    }

    #[test]
    fn indented_string_deeper_indent_preserved() {
        // Common indent is 2 spaces; the 4-space line keeps 2 extra
        assert_eq!(
            ev("''\n  a\n    b\n''"),
            Value::String("a\n  b\n".to_string()),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 15. DYNAMIC ATTRIBUTE NAMES
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn dynamic_attr_name_in_set() {
        assert_eq!(
            ev(r#"let key = "mykey"; in { ${key} = 42; }.mykey"#),
            Value::Int(42),
        );
    }

    #[test]
    fn dynamic_attr_name_with_expression() {
        assert_eq!(
            ev(r#"let prefix = "foo"; in { ${"${prefix}bar"} = 1; }.foobar"#),
            Value::Int(1),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 16. IGNORED TESTS — features needing major infrastructure
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn eval_builtins_match() {
        assert_eq!(
            ev(r#"builtins.match "([0-9]+)" "42""#),
            Value::List(vec![Value::String("42".to_string())]),
        );
    }

    #[test]
    fn eval_builtins_hash_string() {
        let v = ev(r#"builtins.hashString "sha256" "hello""#);
        if let Value::String(s) = v {
            assert_eq!(s.len(), 64);
        } else {
            panic!("expected string");
        }
    }

    #[test]
    fn eval_builtins_import() {
        let dir = std::env::temp_dir();
        let path = dir.join("sui_eval_test_import_eval.nix");
        std::fs::write(&path, "42").unwrap();
        let expr = format!(r#"import "{}""#, path.display());
        let v = eval(&expr).unwrap();
        assert_eq!(v, Value::Int(42));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn eval_builtins_derivation() {
        let v = eval(r#"builtins.derivation { name = "test"; system = "x86_64-linux"; builder = "/bin/sh"; }"#).unwrap();
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("type"), Some(&Value::String("derivation".to_string())));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    #[ignore = "mutual recursive let bindings need lazy evaluation"]
    fn eval_mutual_recursive_let() {
        // let a = { x = b; }; b = { y = a; }; in a.x.y.x.y
        // This requires lazy evaluation to avoid infinite recursion
        let _v = eval("let a = { x = b; }; b = { y = a; }; in a.x.y");
    }

    #[test]
    fn eval_builtins_read_dir() {
        let dir = std::env::temp_dir().join("sui_eval_test_readdir_eval");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "").unwrap();
        let expr = format!(r#"builtins.readDir "{}""#, dir.display());
        let v = eval(&expr).unwrap();
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("a.txt"), Some(&Value::String("regular".to_string())));
        } else {
            panic!("expected attrs");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
