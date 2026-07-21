//! The differential-harness renderers: two **independent** walks that emit
//! the same normalized textual form.
//!
//! * [`render_ir`] walks a lowered [`Program`].
//! * [`render_ast`] walks the rnix/rowan AST directly — it deliberately does
//!   **not** call `lower()`, so comparing the two renders proves the lowering
//!   preserved every child, every ordering, and every leaf value. Structure
//!   loss (a dropped/reordered/collapsed child) shows up as a text diff.
//!
//! The only code the two sides share is leaf formatting (string escaping,
//! float-bit rendering, operator-name tables) — names, not structure.
//!
//! The normalized form is an s-expression: parenthesis placement in the
//! *source* is still visible (as `(paren …)` nodes, 1:1 with the AST), while
//! non-semantic surface details (whitespace, comments, `@`-bind position,
//! escape spellings) normalize away on both sides identically.

use rnix::ast::{self, AstToken, HasEntry};
use rowan::ast::AstNode;

use crate::ir::{
    AttrName, Binding, ExprId, Ir, Param, PathKind, PathPart, Program, StrPart,
};
use crate::lower::LowerError;

// ── shared leaf formatting (names, not structure) ─────────────────────────

fn push_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.escape_debug() {
        out.push(c);
    }
    out.push('"');
}

fn push_float(out: &mut String, f: f64) {
    // Exact: render the raw IEEE-754 bits, so both sides agree iff the parsed
    // f64 is bit-identical. (Display-shortest would also work but hides -0.0.)
    out.push_str("bits=");
    out.push_str(&f.to_bits().to_string());
}

fn path_kind_name(kind: PathKind) -> &'static str {
    match kind {
        PathKind::Abs => "abs",
        PathKind::Rel => "rel",
        PathKind::Home => "home",
    }
}

/// Text of an rnix `Ident` node (same `or`-quirk fallback as the lowerer —
/// leaf extraction, not structure).
fn ident_text(ident: &ast::Ident) -> String {
    match ident.ident_token() {
        Some(tok) => tok.text().to_string(),
        None => ident.syntax().text().to_string(),
    }
}

// ── IR-side renderer ──────────────────────────────────────────────────────

/// Render a lowered [`Program`] to the normalized textual form, starting at
/// its root.
#[must_use]
pub fn render_ir(program: &Program) -> String {
    let mut out = String::new();
    render_ir_expr(program, program.root, &mut out);
    out
}

fn render_ir_strparts(program: &Program, parts: &[StrPart], out: &mut String) {
    for part in parts {
        out.push(' ');
        match part {
            StrPart::Literal(text) => {
                out.push_str("(lit ");
                push_escaped(out, text);
                out.push(')');
            }
            StrPart::Interp(e) => {
                out.push_str("(interp ");
                render_ir_expr(program, *e, out);
                out.push(')');
            }
        }
    }
}

fn render_ir_attrname(program: &Program, attr: &AttrName, out: &mut String) {
    match attr {
        AttrName::Ident(sym) => {
            out.push_str("(a-id ");
            out.push_str(&sui_intern::resolve(*sym));
            out.push(')');
        }
        AttrName::Str(parts) => {
            out.push_str("(a-str");
            render_ir_strparts(program, parts, out);
            out.push(')');
        }
        AttrName::Dynamic(e) => {
            out.push_str("(a-dyn ");
            render_ir_expr(program, *e, out);
            out.push(')');
        }
    }
}

fn render_ir_attrpath(program: &Program, path: &[AttrName], out: &mut String) {
    out.push_str("(attrpath");
    for attr in path {
        out.push(' ');
        render_ir_attrname(program, attr, out);
    }
    out.push(')');
}

fn render_ir_bindings(program: &Program, bindings: &[Binding], out: &mut String) {
    for b in bindings {
        out.push(' ');
        match b {
            Binding::Path { path, value } => {
                out.push_str("(bind ");
                render_ir_attrpath(program, path, out);
                out.push(' ');
                render_ir_expr(program, *value, out);
                out.push(')');
            }
            Binding::Inherit { from, attrs } => {
                out.push_str("(inherit");
                if let Some(f) = from {
                    out.push_str(" (from ");
                    render_ir_expr(program, *f, out);
                    out.push(')');
                }
                for attr in attrs {
                    out.push(' ');
                    render_ir_attrname(program, attr, out);
                }
                out.push(')');
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn render_ir_expr(program: &Program, id: ExprId, out: &mut String) {
    match program.expr(id) {
        Ir::Int(v) => {
            out.push_str("(int ");
            out.push_str(&v.to_string());
            out.push(')');
        }
        Ir::Float(f) => {
            out.push_str("(float ");
            push_float(out, *f);
            out.push(')');
        }
        Ir::Uri(u) => {
            out.push_str("(uri ");
            push_escaped(out, u);
            out.push(')');
        }
        Ir::Ident(sym) => {
            out.push_str("(id ");
            out.push_str(&sui_intern::resolve(*sym));
            out.push(')');
        }
        Ir::Str(parts) => {
            out.push_str("(str");
            render_ir_strparts(program, parts, out);
            out.push(')');
        }
        Ir::Path { kind, parts } => {
            out.push_str("(path ");
            out.push_str(path_kind_name(*kind));
            for part in parts {
                out.push(' ');
                match part {
                    PathPart::Literal(text) => {
                        out.push_str("(lit ");
                        push_escaped(out, text);
                        out.push(')');
                    }
                    PathPart::Interp(e) => {
                        out.push_str("(interp ");
                        render_ir_expr(program, *e, out);
                        out.push(')');
                    }
                }
            }
            out.push(')');
        }
        Ir::SearchPath(text) => {
            out.push_str("(spath ");
            push_escaped(out, text);
            out.push(')');
        }
        Ir::Select {
            subject,
            path,
            or_default,
        } => {
            out.push_str("(select ");
            render_ir_expr(program, *subject, out);
            out.push(' ');
            render_ir_attrpath(program, path, out);
            if let Some(d) = or_default {
                out.push_str(" (or ");
                render_ir_expr(program, *d, out);
                out.push(')');
            }
            out.push(')');
        }
        Ir::HasAttr { subject, path } => {
            out.push_str("(has ");
            render_ir_expr(program, *subject, out);
            out.push(' ');
            render_ir_attrpath(program, path, out);
            out.push(')');
        }
        Ir::Apply { func, arg } => {
            out.push_str("(apply ");
            render_ir_expr(program, *func, out);
            out.push(' ');
            render_ir_expr(program, *arg, out);
            out.push(')');
        }
        Ir::Lambda { param, body } => {
            out.push_str("(lambda ");
            match param {
                Param::Ident(sym) => {
                    out.push_str("(param ");
                    out.push_str(&sui_intern::resolve(*sym));
                    out.push(')');
                }
                Param::Pattern {
                    entries,
                    ellipsis,
                    bind,
                } => {
                    out.push_str("(pattern");
                    if let Some(b) = bind {
                        out.push_str(" (bind ");
                        out.push_str(&sui_intern::resolve(*b));
                        out.push(')');
                    }
                    for entry in entries {
                        out.push_str(" (entry ");
                        out.push_str(&sui_intern::resolve(entry.name));
                        if let Some(d) = entry.default {
                            out.push_str(" (default ");
                            render_ir_expr(program, d, out);
                            out.push(')');
                        }
                        out.push(')');
                    }
                    if *ellipsis {
                        out.push_str(" (ellipsis)");
                    }
                    out.push(')');
                }
            }
            out.push(' ');
            render_ir_expr(program, *body, out);
            out.push(')');
        }
        Ir::LetIn { bindings, body } => {
            out.push_str("(let");
            render_ir_bindings(program, bindings, out);
            out.push_str(" (in ");
            render_ir_expr(program, *body, out);
            out.push_str("))");
        }
        Ir::LegacyLet { bindings } => {
            out.push_str("(legacy-let");
            render_ir_bindings(program, bindings, out);
            out.push(')');
        }
        Ir::AttrSet { rec, bindings } => {
            out.push_str("(attrs");
            if *rec {
                out.push_str(" rec");
            }
            render_ir_bindings(program, bindings, out);
            out.push(')');
        }
        Ir::List(items) => {
            out.push_str("(list");
            for item in items {
                out.push(' ');
                render_ir_expr(program, *item, out);
            }
            out.push(')');
        }
        Ir::BinOp { op, lhs, rhs } => {
            out.push_str("(binop ");
            out.push_str(op.name());
            out.push(' ');
            render_ir_expr(program, *lhs, out);
            out.push(' ');
            render_ir_expr(program, *rhs, out);
            out.push(')');
        }
        Ir::UnaryOp { op, expr } => {
            out.push_str("(unop ");
            out.push_str(op.name());
            out.push(' ');
            render_ir_expr(program, *expr, out);
            out.push(')');
        }
        Ir::IfElse {
            condition,
            then_body,
            else_body,
        } => {
            out.push_str("(if ");
            render_ir_expr(program, *condition, out);
            out.push(' ');
            render_ir_expr(program, *then_body, out);
            out.push(' ');
            render_ir_expr(program, *else_body, out);
            out.push(')');
        }
        Ir::With { namespace, body } => {
            out.push_str("(with ");
            render_ir_expr(program, *namespace, out);
            out.push(' ');
            render_ir_expr(program, *body, out);
            out.push(')');
        }
        Ir::Assert { condition, body } => {
            out.push_str("(assert ");
            render_ir_expr(program, *condition, out);
            out.push(' ');
            render_ir_expr(program, *body, out);
            out.push(')');
        }
        Ir::Paren(inner) => {
            out.push_str("(paren ");
            render_ir_expr(program, *inner, out);
            out.push(')');
        }
        Ir::CurPos => out.push_str("(curpos)"),
    }
}

// ── AST-side renderer (independent walk over rnix, no lowering) ───────────

/// Render an rnix expression tree to the normalized textual form directly —
/// without going through `lower()`.
///
/// # Errors
/// Mirrors `lower()`'s totality contract: any construct the lowerer would
/// refuse (parse-error nodes, missing children, out-of-range literals) is
/// refused here with the same typed [`LowerError`], so the differential can
/// also assert the two sides agree on *what fails*.
pub fn render_ast(expr: &ast::Expr) -> Result<String, LowerError> {
    let mut out = String::new();
    render_ast_expr(expr, &mut out)?;
    Ok(out)
}

fn render_ast_strparts(st: &ast::Str, out: &mut String) -> Result<(), LowerError> {
    for part in st.normalized_parts() {
        out.push(' ');
        match part {
            ast::InterpolPart::Literal(text) => {
                out.push_str("(lit ");
                push_escaped(out, &text);
                out.push(')');
            }
            ast::InterpolPart::Interpolation(interp) => {
                let inner = interp.expr().ok_or(LowerError::Missing {
                    construct: "Interpol",
                    field: "expr",
                })?;
                out.push_str("(interp ");
                render_ast_expr(&inner, out)?;
                out.push(')');
            }
        }
    }
    Ok(())
}

fn render_ast_attrname(attr: &ast::Attr, out: &mut String) -> Result<(), LowerError> {
    match attr {
        ast::Attr::Ident(ident) => {
            out.push_str("(a-id ");
            out.push_str(&ident_text(ident));
            out.push(')');
        }
        ast::Attr::Str(st) => {
            out.push_str("(a-str");
            render_ast_strparts(st, out)?;
            out.push(')');
        }
        ast::Attr::Dynamic(dy) => {
            let inner = dy.expr().ok_or(LowerError::Missing {
                construct: "Dynamic",
                field: "expr",
            })?;
            out.push_str("(a-dyn ");
            render_ast_expr(&inner, out)?;
            out.push(')');
        }
    }
    Ok(())
}

fn render_ast_attrpath(path: &ast::Attrpath, out: &mut String) -> Result<(), LowerError> {
    out.push_str("(attrpath");
    let mut any = false;
    for attr in path.attrs() {
        any = true;
        out.push(' ');
        render_ast_attrname(&attr, out)?;
    }
    if !any {
        return Err(LowerError::Missing {
            construct: "Attrpath",
            field: "attrs",
        });
    }
    out.push(')');
    Ok(())
}

fn render_ast_entries<N: HasEntry>(node: &N, out: &mut String) -> Result<(), LowerError> {
    for entry in node.entries() {
        out.push(' ');
        match entry {
            ast::Entry::AttrpathValue(apv) => {
                let attrpath = apv.attrpath().ok_or(LowerError::Missing {
                    construct: "AttrpathValue",
                    field: "attrpath",
                })?;
                let value = apv.value().ok_or(LowerError::Missing {
                    construct: "AttrpathValue",
                    field: "value",
                })?;
                out.push_str("(bind ");
                render_ast_attrpath(&attrpath, out)?;
                out.push(' ');
                render_ast_expr(&value, out)?;
                out.push(')');
            }
            ast::Entry::Inherit(inh) => {
                out.push_str("(inherit");
                if let Some(f) = inh.from() {
                    let inner = f.expr().ok_or(LowerError::Missing {
                        construct: "InheritFrom",
                        field: "expr",
                    })?;
                    out.push_str(" (from ");
                    render_ast_expr(&inner, out)?;
                    out.push(')');
                }
                for attr in inh.attrs() {
                    out.push(' ');
                    render_ast_attrname(&attr, out)?;
                }
                out.push(')');
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn render_ast_expr(expr: &ast::Expr, out: &mut String) -> Result<(), LowerError> {
    match expr {
        ast::Expr::Literal(lit) => match lit.kind() {
            ast::LiteralKind::Integer(i) => match i.value() {
                Ok(v) => {
                    out.push_str("(int ");
                    out.push_str(&v.to_string());
                    out.push(')');
                }
                Err(_) => {
                    return Err(LowerError::IntOutOfRange {
                        text: i.syntax().text().to_string(),
                    });
                }
            },
            ast::LiteralKind::Float(f) => match f.value() {
                Ok(v) => {
                    out.push_str("(float ");
                    push_float(out, v);
                    out.push(')');
                }
                Err(_) => {
                    return Err(LowerError::BadFloat {
                        text: f.syntax().text().to_string(),
                    });
                }
            },
            ast::LiteralKind::Uri(u) => {
                out.push_str("(uri ");
                push_escaped(out, u.syntax().text());
                out.push(')');
            }
        },
        ast::Expr::Ident(ident) => {
            out.push_str("(id ");
            out.push_str(&ident_text(ident));
            out.push(')');
        }
        ast::Expr::Str(st) => {
            out.push_str("(str");
            render_ast_strparts(st, out)?;
            out.push(')');
        }
        ast::Expr::PathAbs(p) => render_ast_path("abs", &p.parts(), out)?,
        ast::Expr::PathRel(p) => render_ast_path("rel", &p.parts(), out)?,
        ast::Expr::PathHome(p) => render_ast_path("home", &p.parts(), out)?,
        ast::Expr::PathSearch(p) => {
            let content = p.content().ok_or(LowerError::Missing {
                construct: "PathSearch",
                field: "content",
            })?;
            out.push_str("(spath ");
            push_escaped(out, content.text());
            out.push(')');
        }
        ast::Expr::Select(sel) => {
            let subject = sel.expr().ok_or(LowerError::Missing {
                construct: "Select",
                field: "expr",
            })?;
            let attrpath = sel.attrpath().ok_or(LowerError::Missing {
                construct: "Select",
                field: "attrpath",
            })?;
            out.push_str("(select ");
            render_ast_expr(&subject, out)?;
            out.push(' ');
            render_ast_attrpath(&attrpath, out)?;
            if let Some(d) = sel.default_expr() {
                out.push_str(" (or ");
                render_ast_expr(&d, out)?;
                out.push(')');
            }
            out.push(')');
        }
        ast::Expr::HasAttr(ha) => {
            let subject = ha.expr().ok_or(LowerError::Missing {
                construct: "HasAttr",
                field: "expr",
            })?;
            let attrpath = ha.attrpath().ok_or(LowerError::Missing {
                construct: "HasAttr",
                field: "attrpath",
            })?;
            out.push_str("(has ");
            render_ast_expr(&subject, out)?;
            out.push(' ');
            render_ast_attrpath(&attrpath, out)?;
            out.push(')');
        }
        ast::Expr::Apply(app) => {
            let func = app.lambda().ok_or(LowerError::Missing {
                construct: "Apply",
                field: "lambda",
            })?;
            let arg = app.argument().ok_or(LowerError::Missing {
                construct: "Apply",
                field: "argument",
            })?;
            out.push_str("(apply ");
            render_ast_expr(&func, out)?;
            out.push(' ');
            render_ast_expr(&arg, out)?;
            out.push(')');
        }
        ast::Expr::Lambda(lam) => {
            let param = lam.param().ok_or(LowerError::Missing {
                construct: "Lambda",
                field: "param",
            })?;
            let body = lam.body().ok_or(LowerError::Missing {
                construct: "Lambda",
                field: "body",
            })?;
            out.push_str("(lambda ");
            match &param {
                ast::Param::IdentParam(ip) => {
                    let ident = ip.ident().ok_or(LowerError::Missing {
                        construct: "IdentParam",
                        field: "ident",
                    })?;
                    out.push_str("(param ");
                    out.push_str(&ident_text(&ident));
                    out.push(')');
                }
                ast::Param::Pattern(pat) => {
                    out.push_str("(pattern");
                    if let Some(pb) = pat.pat_bind() {
                        let ident = pb.ident().ok_or(LowerError::Missing {
                            construct: "PatBind",
                            field: "ident",
                        })?;
                        out.push_str(" (bind ");
                        out.push_str(&ident_text(&ident));
                        out.push(')');
                    }
                    for pe in pat.pat_entries() {
                        let ident = pe.ident().ok_or(LowerError::Missing {
                            construct: "PatEntry",
                            field: "ident",
                        })?;
                        out.push_str(" (entry ");
                        out.push_str(&ident_text(&ident));
                        if let Some(d) = pe.default() {
                            out.push_str(" (default ");
                            render_ast_expr(&d, out)?;
                            out.push(')');
                        }
                        out.push(')');
                    }
                    if pat.ellipsis_token().is_some() {
                        out.push_str(" (ellipsis)");
                    }
                    out.push(')');
                }
            }
            out.push(' ');
            render_ast_expr(&body, out)?;
            out.push(')');
        }
        ast::Expr::LetIn(li) => {
            out.push_str("(let");
            render_ast_entries(li, out)?;
            let body = li.body().ok_or(LowerError::Missing {
                construct: "LetIn",
                field: "body",
            })?;
            out.push_str(" (in ");
            render_ast_expr(&body, out)?;
            out.push_str("))");
        }
        ast::Expr::LegacyLet(ll) => {
            out.push_str("(legacy-let");
            render_ast_entries(ll, out)?;
            out.push(')');
        }
        ast::Expr::AttrSet(set) => {
            out.push_str("(attrs");
            if set.rec_token().is_some() {
                out.push_str(" rec");
            }
            render_ast_entries(set, out)?;
            out.push(')');
        }
        ast::Expr::List(list) => {
            out.push_str("(list");
            for item in list.items() {
                out.push(' ');
                render_ast_expr(&item, out)?;
            }
            out.push(')');
        }
        ast::Expr::BinOp(bo) => {
            let lhs = bo.lhs().ok_or(LowerError::Missing {
                construct: "BinOp",
                field: "lhs",
            })?;
            let rhs = bo.rhs().ok_or(LowerError::Missing {
                construct: "BinOp",
                field: "rhs",
            })?;
            let op = bo.operator().ok_or(LowerError::Missing {
                construct: "BinOp",
                field: "operator",
            })?;
            out.push_str("(binop ");
            out.push_str(crate::ir::BinOp::from(op).name());
            out.push(' ');
            render_ast_expr(&lhs, out)?;
            out.push(' ');
            render_ast_expr(&rhs, out)?;
            out.push(')');
        }
        ast::Expr::UnaryOp(uo) => {
            let op = uo.operator().ok_or(LowerError::Missing {
                construct: "UnaryOp",
                field: "operator",
            })?;
            let inner = uo.expr().ok_or(LowerError::Missing {
                construct: "UnaryOp",
                field: "expr",
            })?;
            out.push_str("(unop ");
            out.push_str(crate::ir::UnaryOp::from(op).name());
            out.push(' ');
            render_ast_expr(&inner, out)?;
            out.push(')');
        }
        ast::Expr::IfElse(ie) => {
            let cond = ie.condition().ok_or(LowerError::Missing {
                construct: "IfElse",
                field: "condition",
            })?;
            let then_body = ie.body().ok_or(LowerError::Missing {
                construct: "IfElse",
                field: "body",
            })?;
            let else_body = ie.else_body().ok_or(LowerError::Missing {
                construct: "IfElse",
                field: "else_body",
            })?;
            out.push_str("(if ");
            render_ast_expr(&cond, out)?;
            out.push(' ');
            render_ast_expr(&then_body, out)?;
            out.push(' ');
            render_ast_expr(&else_body, out)?;
            out.push(')');
        }
        ast::Expr::With(w) => {
            let ns = w.namespace().ok_or(LowerError::Missing {
                construct: "With",
                field: "namespace",
            })?;
            let body = w.body().ok_or(LowerError::Missing {
                construct: "With",
                field: "body",
            })?;
            out.push_str("(with ");
            render_ast_expr(&ns, out)?;
            out.push(' ');
            render_ast_expr(&body, out)?;
            out.push(')');
        }
        ast::Expr::Assert(a) => {
            let cond = a.condition().ok_or(LowerError::Missing {
                construct: "Assert",
                field: "condition",
            })?;
            let body = a.body().ok_or(LowerError::Missing {
                construct: "Assert",
                field: "body",
            })?;
            out.push_str("(assert ");
            render_ast_expr(&cond, out)?;
            out.push(' ');
            render_ast_expr(&body, out)?;
            out.push(')');
        }
        ast::Expr::Paren(p) => {
            let inner = p.expr().ok_or(LowerError::Missing {
                construct: "Paren",
                field: "expr",
            })?;
            out.push_str("(paren ");
            render_ast_expr(&inner, out)?;
            out.push(')');
        }
        ast::Expr::Root(r) => {
            let inner = r.expr().ok_or(LowerError::Missing {
                construct: "Root",
                field: "expr",
            })?;
            render_ast_expr(&inner, out)?;
        }
        ast::Expr::CurPos(_) => out.push_str("(curpos)"),
        ast::Expr::Error(e) => {
            let r = e.syntax().text_range();
            return Err(LowerError::ParseErrorNode {
                start: u32::from(r.start()),
                end: u32::from(r.end()),
            });
        }
    }
    Ok(())
}

fn render_ast_path(
    kind: &'static str,
    parts: &[ast::InterpolPart<ast::PathContent>],
    out: &mut String,
) -> Result<(), LowerError> {
    out.push_str("(path ");
    out.push_str(kind);
    for part in parts {
        out.push(' ');
        match part {
            ast::InterpolPart::Literal(content) => {
                out.push_str("(lit ");
                push_escaped(out, content.text());
                out.push(')');
            }
            ast::InterpolPart::Interpolation(interp) => {
                let inner = interp.expr().ok_or(LowerError::Missing {
                    construct: "Interpol",
                    field: "expr",
                })?;
                out.push_str("(interp ");
                render_ast_expr(&inner, out)?;
                out.push(')');
            }
        }
    }
    out.push(')');
    Ok(())
}
