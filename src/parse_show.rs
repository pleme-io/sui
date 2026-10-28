use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rnix::ast::{self, AstToken, HasEntry};
use rowan::ast::AstNode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowError {
    pub message: String,
    pub offset: usize,
}

impl ShowError {
    fn at(node: &impl AstNode, message: impl Into<String>) -> Self {
        Self { message: message.into(), offset: usize::from(node.syntax().text_range().start()) }
    }

    #[must_use]
    pub fn render(&self, source: &str, origin: &str) -> String {
        let before = &source[..self.offset.min(source.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        let text = source.lines().nth(line - 1).unwrap_or_default();
        format!(
            "error: {}\n       at {origin}:{line}:{column}:\n{:>13}| {text}\n             | {}^",
            self.message,
            line,
            " ".repeat(column - 1)
        )
    }
}

#[derive(Debug, Clone)]
enum E {
    Int(i64),
    Float(f64),
    Str(String),
    Path(String),
    Var(String),
    Select { e: Box<E>, path: Vec<Name>, def: Option<Box<E>> },
    HasAttr { e: Box<E>, path: Vec<Name> },
    Attrs(Attrs),
    List(Vec<E>),
    Lambda { arg: Option<String>, formals: Option<Formals>, body: Box<E> },
    Call { fun: Box<E>, args: Vec<E> },
    Let { attrs: Attrs, body: Box<E> },
    With { attrs: Box<E>, body: Box<E> },
    If { cond: Box<E>, then: Box<E>, other: Box<E> },
    Assert { cond: Box<E>, body: Box<E> },
    Not(Box<E>),
    Op { op: &'static str, l: Box<E>, r: Box<E> },
    Concat(Vec<E>),
    CurPos,
}

#[derive(Debug, Clone)]
enum Name {
    Static(String),
    Dynamic(E),
}

#[derive(Debug, Clone)]
struct Formals {
    entries: Vec<(String, Option<E>)>,
    ellipsis: bool,
}

#[derive(Debug, Clone)]
enum Def {
    Plain(E),
    Inherited,
    InheritedFrom(usize),
}

#[derive(Debug, Clone, Default)]
struct Attrs {
    rec: bool,
    attrs: BTreeMap<String, Def>,
    dynamic: Vec<(E, E)>,
    inherit_from: Vec<E>,
}

#[derive(Clone)]
enum Frame<'a> {
    Root,
    Names(&'a HashSet<String>, &'a Frame<'a>),
    With,
}

impl Frame<'_> {
    fn resolves(&self, name: &str, globals: &HashSet<String>) -> bool {
        match self {
            Frame::Root => globals.contains(name),
            Frame::Names(names, up) => names.contains(name) || up.resolves(name, globals),
            Frame::With => true,
        }
    }
}

pub struct Shower {
    base: PathBuf,
    globals: HashSet<String>,
}

pub fn globals() -> HashSet<String> {
    let mut names: HashSet<String> = ["builtins", "true", "false", "null"].iter().map(|s| (*s).to_string()).collect();
    names.extend(sui_compat::scope::CALLABLE_GLOBALS.iter().map(|s| (*s).to_string()));
    if let Ok(sui_eval::Value::List(list)) = sui_eval::eval::eval("builtins.attrNames builtins") {
        for v in list.iter() {
            if let Ok(v) = sui_eval::eval::force_value(v)
                && let Ok(s) = v.as_string()
            {
                names.insert(format!("__{s}"));
            }
        }
    }
    names
}

impl Shower {
    #[must_use]
    pub fn new(base: &Path, globals: HashSet<String>) -> Self {
        Self { base: base.to_path_buf(), globals }
    }

    pub fn show(&self, source: &str) -> Result<String, ShowError> {
        let parsed = rnix::Root::parse(source);
        if let Some(err) = parsed.errors().first() {
            let offset = match err {
                rnix::ParseError::Unexpected(r)
                | rnix::ParseError::UnexpectedExtra(r)
                | rnix::ParseError::UnexpectedWanted(_, r, _)
                | rnix::ParseError::UnexpectedDoubleBind(r)
                | rnix::ParseError::DuplicatedArgs(r, _) => usize::from(r.start()),
                _ => source.len(),
            };
            return Err(ShowError { message: format!("syntax error, {err}"), offset });
        }
        let root = parsed.tree();
        let expr = root.expr().ok_or_else(|| ShowError { message: "syntax error, unexpected end of file".into(), offset: source.len() })?;
        let lowered = self.lower(&expr, &Frame::Root)?;
        let mut out = String::new();
        show(&lowered, &mut out);
        Ok(out)
    }

    fn var(&self, node: &impl AstNode, name: String, scope: &Frame) -> Result<E, ShowError> {
        if scope.resolves(&name, &self.globals) {
            Ok(E::Var(name))
        } else {
            Err(ShowError::at(node, format!("undefined variable '{name}'")))
        }
    }

    fn lower(&self, expr: &ast::Expr, scope: &Frame) -> Result<E, ShowError> {
        let sub = |e: Option<ast::Expr>, node: &dyn Fn() -> ShowError| -> Result<E, ShowError> {
            match e {
                Some(e) => self.lower(&e, scope),
                None => Err(node()),
            }
        };
        Ok(match expr {
            ast::Expr::Paren(p) => sub(p.expr(), &|| eof(p))?,
            ast::Expr::Root(r) => sub(r.expr(), &|| eof(r))?,
            ast::Expr::Literal(l) => match l.kind() {
                ast::LiteralKind::Integer(i) => E::Int(i.value().map_err(|e| ShowError::at(l, e.to_string()))?),
                ast::LiteralKind::Float(f) => E::Float(f.value().map_err(|e| ShowError::at(l, e.to_string()))?),
                ast::LiteralKind::Uri(u) => E::Str(u.syntax().text().to_string()),
            },
            ast::Expr::Ident(i) => {
                let name = i.syntax().text().to_string();
                self.var(i, name, scope)?
            }
            ast::Expr::CurPos(_) => E::CurPos,
            ast::Expr::Str(s) => self.string(s, scope)?,
            ast::Expr::PathAbs(p) => self.path(p.parts(), PathKind::Abs, scope)?,
            ast::Expr::PathRel(p) => self.path(p.parts(), PathKind::Rel, scope)?,
            ast::Expr::PathHome(p) => self.path(p.parts(), PathKind::Home, scope)?,
            ast::Expr::PathSearch(p) => {
                let text = p.syntax().text().to_string();
                let inner = text.trim_start_matches('<').trim_end_matches('>').to_string();
                E::Call {
                    fun: Box::new(self.var(p, "__findFile".into(), scope)?),
                    args: vec![self.var(p, "__nixPath".into(), scope)?, E::Str(inner)],
                }
            }
            ast::Expr::List(l) => E::List(l.items().map(|i| self.lower(&i, scope)).collect::<Result<_, _>>()?),
            ast::Expr::Select(s) => E::Select {
                e: Box::new(sub(s.expr(), &|| eof(s))?),
                path: self.attrpath(s.attrpath(), scope)?,
                def: s.default_expr().map(|d| self.lower(&d, scope)).transpose()?.map(Box::new),
            },
            ast::Expr::HasAttr(h) => E::HasAttr {
                e: Box::new(sub(h.expr(), &|| eof(h))?),
                path: self.attrpath(h.attrpath(), scope)?,
            },
            ast::Expr::Apply(a) => {
                let fun = sub(a.lambda(), &|| eof(a))?;
                let arg = sub(a.argument(), &|| eof(a))?;
                match fun {
                    E::Call { fun, mut args } => {
                        args.push(arg);
                        E::Call { fun, args }
                    }
                    other => E::Call { fun: Box::new(other), args: vec![arg] },
                }
            }
            ast::Expr::IfElse(i) => E::If {
                cond: Box::new(sub(i.condition(), &|| eof(i))?),
                then: Box::new(sub(i.body(), &|| eof(i))?),
                other: Box::new(sub(i.else_body(), &|| eof(i))?),
            },
            ast::Expr::Assert(a) => E::Assert {
                cond: Box::new(sub(a.condition(), &|| eof(a))?),
                body: Box::new(sub(a.body(), &|| eof(a))?),
            },
            ast::Expr::With(w) => {
                let attrs = sub(w.namespace(), &|| eof(w))?;
                let inner = Frame::With;
                let body = match w.body() {
                    Some(b) => self.lower(&b, &inner)?,
                    None => return Err(eof(w)),
                };
                E::With { attrs: Box::new(attrs), body: Box::new(body) }
            }
            ast::Expr::UnaryOp(u) => {
                let e = sub(u.expr(), &|| eof(u))?;
                match u.operator() {
                    Some(ast::UnaryOpKind::Invert) => E::Not(Box::new(e)),
                    _ => E::Call { fun: Box::new(self.var(u, "__sub".into(), scope)?), args: vec![E::Int(0), e] },
                }
            }
            ast::Expr::BinOp(b) => {
                let l = sub(b.lhs(), &|| eof(b))?;
                let r = sub(b.rhs(), &|| eof(b))?;
                let call = |name: &str, args: Vec<E>| -> Result<E, ShowError> {
                    Ok(E::Call { fun: Box::new(self.var(b, name.into(), scope)?), args })
                };
                let op = |op: &'static str, l: E, r: E| E::Op { op, l: Box::new(l), r: Box::new(r) };
                match b.operator().ok_or_else(|| eof(b))? {
                    ast::BinOpKind::Concat => op("++", l, r),
                    ast::BinOpKind::Update => op("//", l, r),
                    ast::BinOpKind::Add => E::Concat(vec![l, r]),
                    ast::BinOpKind::Sub => call("__sub", vec![l, r])?,
                    ast::BinOpKind::Mul => call("__mul", vec![l, r])?,
                    ast::BinOpKind::Div => call("__div", vec![l, r])?,
                    ast::BinOpKind::And => op("&&", l, r),
                    ast::BinOpKind::Or => op("||", l, r),
                    ast::BinOpKind::Implication => op("->", l, r),
                    ast::BinOpKind::Equal => op("==", l, r),
                    ast::BinOpKind::NotEqual => op("!=", l, r),
                    ast::BinOpKind::Less => call("__lessThan", vec![l, r])?,
                    ast::BinOpKind::More => call("__lessThan", vec![r, l])?,
                    ast::BinOpKind::LessOrEq => E::Not(Box::new(call("__lessThan", vec![r, l])?)),
                    ast::BinOpKind::MoreOrEq => E::Not(Box::new(call("__lessThan", vec![l, r])?)),
                    ast::BinOpKind::PipeRight | ast::BinOpKind::PipeLeft => {
                        return Err(ShowError::at(b, "experimental Nix feature 'pipe-operators' is disabled"));
                    }
                }
            }
            ast::Expr::Lambda(l) => self.lambda(l, scope)?,
            ast::Expr::AttrSet(s) => {
                let rec = s.rec_token().is_some();
                E::Attrs(self.bindings(s, rec, scope)?.0)
            }
            ast::Expr::LetIn(l) => {
                let (attrs, names) = self.bindings(l, true, scope)?;
                let inner = Frame::Names(&names, scope);
                let body = match l.body() {
                    Some(b) => self.lower(&b, &inner)?,
                    None => return Err(eof(l)),
                };
                let mut attrs = attrs;
                attrs.rec = false;
                E::Let { attrs, body: Box::new(body) }
            }
            ast::Expr::LegacyLet(l) => {
                let (attrs, _) = self.bindings(l, true, scope)?;
                E::Select { e: Box::new(E::Attrs(attrs)), path: vec![Name::Static("body".into())], def: None }
            }
            ast::Expr::Error(e) => return Err(ShowError::at(e, "syntax error")),
        })
    }

    fn lambda(&self, l: &ast::Lambda, scope: &Frame) -> Result<E, ShowError> {
        let param = l.param().ok_or_else(|| ShowError::at(l, "syntax error"))?;
        let mut names = HashSet::new();
        let (arg, pattern) = match &param {
            ast::Param::IdentParam(i) => (i.ident().map(|i| i.syntax().text().to_string()), None),
            ast::Param::Pattern(p) => (p.pat_bind().and_then(|b| b.ident()).map(|i| i.syntax().text().to_string()), Some(p)),
        };
        if let Some(a) = &arg {
            names.insert(a.clone());
        }
        if let Some(p) = pattern {
            for entry in p.pat_entries() {
                let name = entry.ident().map(|i| i.syntax().text().to_string()).unwrap_or_default();
                if !names.insert(name.clone()) {
                    return Err(ShowError::at(&entry, format!("duplicate formal function argument '{name}'")));
                }
            }
        }
        let inner = Frame::Names(&names, scope);
        let formals = match pattern {
            Some(p) => {
                let mut entries = Vec::new();
                for entry in p.pat_entries() {
                    let name = entry.ident().map(|i| i.syntax().text().to_string()).unwrap_or_default();
                    entries.push((name, entry.default().map(|d| self.lower(&d, &inner)).transpose()?));
                }
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                Some(Formals { entries, ellipsis: p.ellipsis_token().is_some() })
            }
            None => None,
        };
        let body = match l.body() {
            Some(b) => self.lower(&b, &inner)?,
            None => return Err(ShowError::at(l, "syntax error, unexpected end of file")),
        };
        Ok(E::Lambda { arg, formals, body: Box::new(body) })
    }

    fn attr_name(&self, attr: &ast::Attr, scope: &Frame) -> Result<Name, ShowError> {
        Ok(match attr {
            ast::Attr::Ident(i) => Name::Static(i.syntax().text().to_string()),
            ast::Attr::Str(s) => match self.string(s, scope)? {
                E::Str(text) => Name::Static(text),
                other => Name::Dynamic(other),
            },
            ast::Attr::Dynamic(d) => {
                let e = d.expr().ok_or_else(|| ShowError::at(d, "syntax error"))?;
                match self.lower(&e, scope)? {
                    E::Str(text) => Name::Static(text),
                    other => Name::Dynamic(other),
                }
            }
        })
    }

    fn attrpath(&self, path: Option<ast::Attrpath>, scope: &Frame) -> Result<Vec<Name>, ShowError> {
        match path {
            Some(p) => p.attrs().map(|a| self.attr_name(&a, scope)).collect(),
            None => Ok(Vec::new()),
        }
    }

    fn bindings(&self, node: &impl HasEntry, rec: bool, scope: &Frame) -> Result<(Attrs, HashSet<String>), ShowError> {
        let mut names = HashSet::new();
        for entry in node.entries() {
            match entry {
                ast::Entry::Inherit(i) => {
                    for a in i.attrs() {
                        if let Ok(Name::Static(n)) = self.attr_name(&a, &Frame::With) {
                            names.insert(n);
                        }
                    }
                }
                ast::Entry::AttrpathValue(v) => {
                    if let Some(first) = v.attrpath().and_then(|p| p.attrs().next())
                        && let Ok(Name::Static(n)) = self.attr_name(&first, &Frame::With)
                    {
                        names.insert(n);
                    }
                }
            }
        }
        let inner_frame = Frame::Names(&names, scope);
        let inner = if rec { &inner_frame } else { scope };
        let mut attrs = Attrs { rec, ..Attrs::default() };
        for entry in node.entries() {
            match entry {
                ast::Entry::Inherit(i) => {
                    let from = match i.from() {
                        Some(f) => {
                            let e = f.expr().ok_or_else(|| ShowError::at(&f, "syntax error"))?;
                            attrs.inherit_from.push(self.lower(&e, inner)?);
                            Some(attrs.inherit_from.len() - 1)
                        }
                        None => None,
                    };
                    for a in i.attrs() {
                        let Name::Static(n) = self.attr_name(&a, scope)? else {
                            return Err(ShowError::at(&a, "dynamic attributes not allowed in inherit"));
                        };
                        if from.is_none() {
                            self.var(&a, n.clone(), scope)?;
                        }
                        if attrs.attrs.contains_key(&n) {
                            return Err(ShowError::at(&a, format!("attribute '{n}' already defined")));
                        }
                        attrs.attrs.insert(n, from.map_or(Def::Inherited, Def::InheritedFrom));
                    }
                }
                ast::Entry::AttrpathValue(v) => {
                    let path = self.attrpath(v.attrpath(), inner)?;
                    let value = match v.value() {
                        Some(e) => self.lower(&e, inner)?,
                        None => return Err(ShowError::at(&v, "syntax error")),
                    };
                    add_attr(&mut attrs, &path, value).map_err(|n| ShowError::at(&v, format!("attribute '{n}' already defined")))?;
                }
            }
        }
        Ok((attrs, names))
    }

    fn string(&self, s: &ast::Str, scope: &Frame) -> Result<E, ShowError> {
        if s.syntax().first_token().is_some_and(|t| t.text() == "''") {
            return self.indented_string(s, scope);
        }
        let mut parts = Vec::new();
        let mut interpolated = false;
        for part in s.normalized_parts() {
            match part {
                ast::InterpolPart::Literal(text) => {
                    if !text.is_empty() {
                        parts.push(E::Str(text));
                    }
                }
                ast::InterpolPart::Interpolation(i) => {
                    interpolated = true;
                    let e = i.expr().ok_or_else(|| ShowError::at(&i, "syntax error"))?;
                    parts.push(self.lower(&e, scope)?);
                }
            }
        }
        Ok(if interpolated {
            E::Concat(parts)
        } else {
            E::Str(parts.into_iter().map(|p| if let E::Str(s) = p { s } else { String::new() }).collect())
        })
    }

    fn indented_string(&self, s: &ast::Str, scope: &Frame) -> Result<E, ShowError> {
        let mut tokens: Vec<IndPart> = Vec::new();
        let mut first = true;
        for part in s.parts() {
            match part {
                ast::InterpolPart::Literal(content) => {
                    let raw = content.syntax().text().to_string();
                    let mut text = raw.as_str();
                    if first {
                        let spaces = text.len() - text.trim_start_matches(' ').len();
                        if text[spaces..].starts_with('\n') {
                            text = &text[spaces + 1..];
                        }
                    }
                    lex_indented(text, &mut tokens);
                }
                ast::InterpolPart::Interpolation(i) => {
                    let e = i.expr().ok_or_else(|| ShowError::at(&i, "syntax error"))?;
                    tokens.push(IndPart::Expr(self.lower(&e, scope)?));
                }
            }
            first = false;
        }
        Ok(strip_indentation(tokens))
    }

    fn path(&self, parts: Vec<ast::InterpolPart<ast::PathContent>>, kind: PathKind, scope: &Frame) -> Result<E, ShowError> {
        let resolve = |text: &str| -> String {
            let joined = match kind {
                PathKind::Abs => text.to_string(),
                PathKind::Rel => format!("{}/{text}", self.base.display()),
                PathKind::Home => sui_compat::path_literal::expand_home(text),
            };
            let trailing = joined.ends_with('/') && joined.len() > 1;
            let canon = sui_compat::canon_path::CanonPath::new(&joined).as_str().to_string();
            if trailing && !canon.ends_with('/') { format!("{canon}/") } else { canon }
        };
        let interpolated = parts.iter().any(|p| matches!(p, ast::InterpolPart::Interpolation(_)));
        if !interpolated {
            let text: String = parts
                .iter()
                .map(|p| if let ast::InterpolPart::Literal(c) = p { c.text().to_string() } else { String::new() })
                .collect();
            return Ok(E::Path(resolve(&text)));
        }
        let mut out = Vec::new();
        for (i, part) in parts.into_iter().enumerate() {
            match part {
                ast::InterpolPart::Literal(c) if i == 0 => out.push(E::Path(resolve(c.text()))),
                ast::InterpolPart::Literal(c) => out.push(E::Str(c.text().to_string())),
                ast::InterpolPart::Interpolation(interp) => {
                    let e = interp.expr().ok_or_else(|| ShowError::at(&interp, "syntax error"))?;
                    out.push(self.lower(&e, scope)?);
                }
            }
        }
        Ok(E::Concat(out))
    }
}

enum IndPart {
    Text(String, bool),
    Expr(E),
}

fn lex_indented(text: &str, out: &mut Vec<IndPart>) {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"''$") {
            out.push(IndPart::Text("$".into(), false));
            i += 3;
        } else if b[i..].starts_with(b"'''") {
            out.push(IndPart::Text("''".into(), false));
            i += 3;
        } else if b[i..].starts_with(b"''\\") && i + 3 < b.len() {
            let rest = &text[i + 3..];
            let ch = rest.chars().next().unwrap_or_default();
            let mut len = ch.len_utf8();
            let unescaped = match ch {
                'n' => "\n".to_string(),
                'r' => "\r".to_string(),
                't' => "\t".to_string(),
                '\r' => {
                    if rest[len..].starts_with('\n') {
                        len += 1;
                    }
                    "\n".to_string()
                }
                c => c.to_string(),
            };
            out.push(IndPart::Text(unescaped, false));
            i += 3 + len;
        } else {
            let start = i;
            while i < b.len() {
                let next = b.get(i + 1).copied();
                match b[i] {
                    b'$' if next.is_some_and(|n| n != b'{' && n != b'\'') => i += 2,
                    b'\'' if next.is_some_and(|n| n != b'\'' && n != b'$') => i += 2,
                    b'$' | b'\'' => break,
                    _ => i += 1,
                }
            }
            let end = i.min(b.len());
            if end > start {
                out.push(IndPart::Text(text[start..end].to_string(), true));
                i = end;
            } else {
                let c = text[i..].chars().next().unwrap_or_default();
                let token = if c == '$' { "$" } else { "'" };
                out.push(IndPart::Text(token.to_string(), false));
                i += c.len_utf8();
            }
        }
    }
}

fn strip_indentation(parts: Vec<IndPart>) -> E {
    if parts.is_empty() {
        return E::Str(String::new());
    }
    let mut at_start = true;
    let mut min_indent = usize::MAX;
    let mut cur = 0;
    for part in &parts {
        match part {
            IndPart::Text(t, true) => {
                for c in t.chars() {
                    if at_start {
                        if c == ' ' {
                            cur += 1;
                        } else if c == '\n' {
                            cur = 0;
                        } else {
                            at_start = false;
                            min_indent = min_indent.min(cur);
                        }
                    } else if c == '\n' {
                        at_start = true;
                        cur = 0;
                    }
                }
            }
            _ => {
                if at_start {
                    at_start = false;
                    min_indent = min_indent.min(cur);
                }
            }
        }
    }
    let mut out = Vec::new();
    at_start = true;
    let mut dropped = 0;
    let total = parts.len();
    for (idx, part) in parts.into_iter().enumerate() {
        match part {
            IndPart::Expr(e) => {
                at_start = false;
                dropped = 0;
                out.push(e);
            }
            IndPart::Text(t, _) => {
                let mut s2 = String::new();
                for c in t.chars() {
                    if at_start {
                        if c == ' ' {
                            if dropped >= min_indent {
                                s2.push(c);
                            }
                            dropped += 1;
                        } else if c == '\n' {
                            dropped = 0;
                            s2.push(c);
                        } else {
                            at_start = false;
                            dropped = 0;
                            s2.push(c);
                        }
                    } else {
                        s2.push(c);
                        if c == '\n' {
                            at_start = true;
                        }
                    }
                }
                if idx + 1 == total
                    && let Some(p) = s2.rfind('\n')
                    && s2[p + 1..].chars().all(|c| c == ' ')
                {
                    s2.truncate(p + 1);
                }
                if !s2.is_empty() {
                    out.push(E::Str(s2));
                }
            }
        }
    }
    match out.len() {
        0 => E::Str(String::new()),
        1 if matches!(out[0], E::Str(_)) => out.pop().unwrap_or(E::Str(String::new())),
        _ => E::Concat(out),
    }
}

fn eof(node: &impl AstNode) -> ShowError {
    ShowError { message: "syntax error, unexpected end of file".into(), offset: usize::from(node.syntax().text_range().end()) }
}

#[derive(Clone, Copy)]
enum PathKind {
    Abs,
    Rel,
    Home,
}

fn add_attr(attrs: &mut Attrs, path: &[Name], value: E) -> Result<(), String> {
    let Some((last, prefix)) = path.split_last() else { return Ok(()) };
    let mut cur = attrs;
    for name in prefix {
        cur = match name {
            Name::Static(n) => {
                let entry = cur.attrs.entry(n.clone()).or_insert_with(|| Def::Plain(E::Attrs(Attrs::default())));
                match entry {
                    Def::Plain(E::Attrs(nested)) => nested,
                    _ => return Err(n.clone()),
                }
            }
            Name::Dynamic(e) => {
                cur.dynamic.push((e.clone(), E::Attrs(Attrs::default())));
                match cur.dynamic.last_mut() {
                    Some((_, E::Attrs(nested))) => nested,
                    _ => unreachable!(),
                }
            }
        };
    }
    match last {
        Name::Static(n) => match cur.attrs.get_mut(n) {
            None => {
                cur.attrs.insert(n.clone(), Def::Plain(value));
            }
            Some(Def::Plain(E::Attrs(existing))) => {
                let E::Attrs(incoming) = value else { return Err(n.clone()) };
                let offset = existing.inherit_from.len();
                for (k, def) in incoming.attrs {
                    if existing.attrs.contains_key(&k) {
                        return Err(k);
                    }
                    let def = match def {
                        Def::InheritedFrom(i) => Def::InheritedFrom(i + offset),
                        other => other,
                    };
                    existing.attrs.insert(k, def);
                }
                existing.dynamic.extend(incoming.dynamic);
                existing.inherit_from.extend(incoming.inherit_from);
            }
            Some(_) => return Err(n.clone()),
        },
        Name::Dynamic(e) => cur.dynamic.push((e.clone(), value)),
    }
    Ok(())
}

fn identifier(s: &str, out: &mut String) {
    const RESERVED: [&str; 9] = ["if", "then", "else", "assert", "with", "let", "in", "rec", "inherit"];
    if s.is_empty() {
        out.push_str("\"\"");
    } else if RESERVED.contains(&s) {
        let _ = write!(out, "\"{s}\"");
    } else if !s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '\'' || c == '-')
    {
        literal_string(s, out);
    } else {
        out.push_str(s);
    }
}

fn literal_string(s: &str, out: &mut String) {
    out.push('"');
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[must_use]
pub fn cpp_float(f: f64) -> String {
    if f.is_nan() {
        return if f.is_sign_negative() { "-nan".into() } else { "nan".into() };
    }
    if f.is_infinite() {
        return if f < 0.0 { "-inf".into() } else { "inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let sci = format!("{f:.5e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let strip = |s: &str| -> String {
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s.to_string() }
    };
    if !(-4..6).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip(mantissa), exp.abs())
    } else {
        let decimals = usize::try_from(5 - exp).unwrap_or(0);
        strip(&format!("{f:.decimals$}"))
    }
}

fn attr_path(path: &[Name], out: &mut String) {
    for (i, name) in path.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        match name {
            Name::Static(s) => identifier(s, out),
            Name::Dynamic(e) => {
                out.push_str("\"${");
                show(e, out);
                out.push_str("}\"");
            }
        }
    }
}

fn bindings(attrs: &Attrs, out: &mut String) {
    let inherits: Vec<&String> = attrs.attrs.iter().filter(|(_, d)| matches!(d, Def::Inherited)).map(|(k, _)| k).collect();
    if !inherits.is_empty() {
        out.push_str("inherit");
        for k in inherits {
            out.push(' ');
            identifier(k, out);
        }
        out.push_str("; ");
    }
    let mut from: BTreeMap<usize, Vec<&String>> = BTreeMap::new();
    for (k, d) in &attrs.attrs {
        if let Def::InheritedFrom(i) = d {
            from.entry(*i).or_default().push(k);
        }
    }
    for (i, keys) in from {
        out.push_str("inherit (");
        show(&attrs.inherit_from[i], out);
        out.push(')');
        for k in keys {
            out.push(' ');
            identifier(k, out);
        }
        out.push_str("; ");
    }
    for (k, d) in &attrs.attrs {
        if let Def::Plain(e) = d {
            identifier(k, out);
            out.push_str(" = ");
            show(e, out);
            out.push_str("; ");
        }
    }
    for (n, v) in &attrs.dynamic {
        out.push_str("\"${");
        show(n, out);
        out.push_str("}\" = ");
        show(v, out);
        out.push_str("; ");
    }
}

fn show(e: &E, out: &mut String) {
    match e {
        E::Int(i) => {
            let _ = write!(out, "{i}");
        }
        E::Float(f) => out.push_str(&cpp_float(*f)),
        E::Str(s) => literal_string(s, out),
        E::Path(p) => out.push_str(p),
        E::Var(v) => identifier(v, out),
        E::CurPos => out.push_str("__curPos"),
        E::Select { e, path, def } => {
            out.push('(');
            show(e, out);
            out.push_str(").");
            attr_path(path, out);
            if let Some(d) = def {
                out.push_str(" or (");
                show(d, out);
                out.push(')');
            }
        }
        E::HasAttr { e, path } => {
            out.push_str("((");
            show(e, out);
            out.push_str(") ? ");
            attr_path(path, out);
            out.push(')');
        }
        E::Attrs(a) => {
            if a.rec {
                out.push_str("rec ");
            }
            out.push_str("{ ");
            bindings(a, out);
            out.push('}');
        }
        E::List(items) => {
            out.push_str("[ ");
            for i in items {
                out.push('(');
                show(i, out);
                out.push_str(") ");
            }
            out.push(']');
        }
        E::Lambda { arg, formals, body } => {
            out.push('(');
            if let Some(f) = formals {
                out.push_str("{ ");
                let mut first = true;
                for (name, def) in &f.entries {
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    identifier(name, out);
                    if let Some(d) = def {
                        out.push_str(" ? ");
                        show(d, out);
                    }
                }
                if f.ellipsis {
                    if !first {
                        out.push_str(", ");
                    }
                    out.push_str("...");
                }
                out.push_str(" }");
                if arg.is_some() {
                    out.push_str(" @ ");
                }
            }
            if let Some(a) = arg {
                identifier(a, out);
            }
            out.push_str(": ");
            show(body, out);
            out.push(')');
        }
        E::Call { fun, args } => {
            out.push('(');
            show(fun, out);
            for a in args {
                out.push(' ');
                show(a, out);
            }
            out.push(')');
        }
        E::Let { attrs, body } => {
            out.push_str("(let ");
            bindings(attrs, out);
            out.push_str("in ");
            show(body, out);
            out.push(')');
        }
        E::With { attrs, body } => {
            out.push_str("(with ");
            show(attrs, out);
            out.push_str("; ");
            show(body, out);
            out.push(')');
        }
        E::If { cond, then, other } => {
            out.push_str("(if ");
            show(cond, out);
            out.push_str(" then ");
            show(then, out);
            out.push_str(" else ");
            show(other, out);
            out.push(')');
        }
        E::Assert { cond, body } => {
            out.push_str("assert ");
            show(cond, out);
            out.push_str("; ");
            show(body, out);
        }
        E::Not(e) => {
            out.push_str("(! ");
            show(e, out);
            out.push(')');
        }
        E::Op { op, l, r } => {
            out.push('(');
            show(l, out);
            let _ = write!(out, " {op} ");
            show(r, out);
            out.push(')');
        }
        E::Concat(parts) => {
            out.push('(');
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    out.push_str(" + ");
                }
                show(p, out);
            }
            out.push(')');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(src: &str) -> String {
        Shower::new(Path::new("/base"), globals()).show(src).unwrap()
    }

    #[test]
    fn expressions_print_as_cppnix_show_prints_them() {
        for (src, want) in [
            ("1 + 2", "(1 + 2)"),
            (r#"let a = 1; in { inherit a; b = "x${a}y"; c = [ 1 2.5 ./foo ]; }"#, r#"(let a = 1; in { inherit a; b = ("x" + a + "y"); c = [ (1) (2.5) (/base/foo) ]; })"#),
            ("x: y: x.a or y", "(x: (y: (x).a or (y)))"),
            ("{ a ? 1, ... }@args: if a then !a else -a", "({ a ? 1, ... } @ args: (if a then (! a) else (__sub 0 a)))"),
            (r#"with builtins; rec { a = 1; "b c" = a; }"#, r#"(with builtins; rec { a = 1; "b c" = a; })"#),
            (r#"with {}; { a.b = 1; a.c = 2; "x y" = 3; ${"q"} = 4; ${z} = 5; "${w}" = 6; inherit (b) c d; inherit e; inherit (f) g; }"#,
             r#"(with { }; { inherit e; inherit (b) c d; inherit (f) g; a = { b = 1; c = 2; }; q = 4; "x y" = 3; "${z}" = 5; "${(w)}" = 6; })"#),
            ("with {}; let { x = 1; body = x; }", "(with { }; (rec { body = x; x = 1; }).body)"),
            ("[ 1.0 0.1 123456789.0 1.5e-7 3.14159265 ]", "[ (1) (0.1) (1.23457e+08) (1.5e-07) (3.14159) ]"),
            ("with {}; a <= b", "(with { }; (! (__lessThan b a)))"),
            ("with {}; (f a) b", "(with { }; (f a b))"),
            ("with {}; a - b c", "(with { }; (__sub a (b c)))"),
            ("with {}; a ++ b // c", "(with { }; ((a ++ b) // c))"),
            ("with {}; a ? b.c", "(with { }; ((a) ? b.c))"),
            ("with {}; a.${b}.c or d", r#"(with { }; (a)."${b}".c or (d))"#),
            ("with {}; ./a/${b}.nix", r#"(with { }; (/base/a/ + b + ".nix"))"#),
            ("<nixpkgs/lib>", r#"(__findFile __nixPath "nixpkgs/lib")"#),
            (r#""a\nb\t\"\${""#, r#""a\nb\t\"\${""#),
            ("with {}; assert a; b", "(with { }; assert a; b)"),
            ("with {}; let a = 1; b.c = a; in b", "(with { }; (let a = 1; b = { c = a; }; in b))"),
            ("args@{ a }: a", "({ a } @ args: a)"),
            ("with {}; ''\n  x\n    y ${z}\n  ''", r#"(with { }; ("x\n  y " + z + "\n"))"#),
            (r#"''
  a '$B' '''c'' ''$d ''\n x
  ''"#, r#"(("a " + "'" + "$B' " + "''" + "c") ("$d " + "\n" + " x\n"))"#),
        ] {
            assert_eq!(shown(src), want, "{src}");
        }
    }

    #[test]
    fn an_unbound_variable_and_a_syntax_error_are_refused() {
        let s = Shower::new(Path::new("/base"), globals());
        assert_eq!(s.show("a == b").unwrap_err().message, "undefined variable 'a'");
        assert!(s.show("let x = ;").unwrap_err().message.starts_with("syntax error"));
        assert!(s.show("{ a = 1; a = 2; }").unwrap_err().message.contains("attribute 'a' already defined"));
    }
}
