//! Tree-walking Nix evaluator using rnix's typed AST.

use rnix::ast::{self, AstToken, HasEntry, InterpolPart};
use rowan::ast::AstNode;

use crate::builtins;
use crate::value::*;

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
pub fn apply(func: Value, arg: Value) -> Result<Value, EvalError> {
    match func {
        Value::Lambda(closure) => {
            let mut call_env = closure.env.child();
            bind_param(&closure.param, &arg, &mut call_env)?;
            eval_expr(&closure.body, &call_env)
        }
        Value::Builtin(b) => (b.func)(&[arg]),
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
}
