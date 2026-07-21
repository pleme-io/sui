//! `lower()` — the rnix/rowan AST → [`Program`] pass, run **once** per
//! source file.
//!
//! Totality contract: every construct rnix can parse either lowers to exactly
//! one [`Ir`] node or returns a typed [`LowerError`] naming the construct and
//! the missing piece. No silent gaps, no panics, no placeholder Ok values.
//!
//! Phase-1 lowering is 1:1 structural (SPEED.md L3): the mapping is bijective
//! on the parse surface (even `(e)` keeps its `Paren` node), so force order is
//! untouched by construction. What lowering *does* precompute in this slice:
//! interned ident / static-attr-key [`Symbol`]s and normalized string-literal
//! parts. The rest of the L3 precompute menu (needed-bindings sets, free-var
//! sets, attrset shapes) lands in later slices on top of this skeleton.

use rnix::ast::{self, AstToken, HasEntry};
use rowan::ast::AstNode;

use crate::ir::{
    AttrName, Binding, ExprId, Ir, Param, PathKind, PathPart, PatternEntry, Program, Span,
    StrPart,
};

/// Typed lowering failure. Every variant names the construct it came from —
/// a `LowerError` is a mechanical "this exact spot in the parse surface is
/// not lowerable", never a generic failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LowerError {
    /// The source did not parse at all (rnix returned parse errors).
    #[error("source failed to parse: {message}")]
    ParseFailure { message: String },
    /// An rnix `NODE_ERROR` (parse-error recovery node) inside the tree.
    #[error("parse-error node in the AST at bytes {start}..{end}")]
    ParseErrorNode { start: u32, end: u32 },
    /// A node is missing a required child (malformed but recovered parse,
    /// e.g. `if x then y` with no `else`).
    #[error("`{construct}` node is missing its `{field}`")]
    Missing {
        construct: &'static str,
        field: &'static str,
    },
    /// An integer literal that does not fit `i64`.
    #[error("integer literal `{text}` does not fit i64")]
    IntOutOfRange { text: String },
    /// A float literal that fails to parse as `f64`.
    #[error("float literal `{text}` does not parse as f64")]
    BadFloat { text: String },
}

/// Lower a full source file: parse + lower the root expression.
///
/// # Errors
/// Returns [`LowerError::ParseFailure`] when rnix reports parse errors, and
/// any [`LowerError`] the lowering of the root expression produces.
pub fn lower_file(src: &str) -> Result<Program, LowerError> {
    let parse = rnix::Root::parse(src);
    if let Some(err) = parse.errors().first() {
        return Err(LowerError::ParseFailure {
            message: err.to_string(),
        });
    }
    let root = parse.tree();
    let expr = root.expr().ok_or(LowerError::Missing {
        construct: "Root",
        field: "expr",
    })?;
    lower(&expr)
}

/// Lower one expression tree into a fresh [`Program`].
///
/// Ids are assigned post-order (children first), so the root is always the
/// last entry and every child id is strictly less than its parent's.
///
/// # Errors
/// Returns a typed [`LowerError`] naming the first construct that cannot be
/// lowered. Never panics on any rnix-produced tree.
pub fn lower(expr: &ast::Expr) -> Result<Program, LowerError> {
    let mut lo = Lowerer {
        exprs: Vec::new(),
        spans: Vec::new(),
    };
    let root = lo.lower_expr(expr)?;
    debug_assert_eq!(root.index() + 1, lo.exprs.len());
    Ok(Program {
        exprs: lo.exprs,
        spans: lo.spans,
        root,
    })
}

struct Lowerer {
    exprs: Vec<Ir>,
    spans: Vec<Span>,
}

fn span_of(node: &rnix::SyntaxNode) -> Span {
    let r = node.text_range();
    Span {
        start: u32::from(r.start()),
        end: u32::from(r.end()),
    }
}

/// Text of an rnix `Ident` node. Mirrors the tree-walker: the identifier
/// `or` is lexed as a nested `TOKEN_OR` (rnix quirk), so `ident_token()` is
/// `None` there — fall back to the node's full text.
fn ident_text(ident: &ast::Ident) -> String {
    match ident.ident_token() {
        Some(tok) => tok.text().to_string(),
        None => ident.syntax().text().to_string(),
    }
}

impl Lowerer {
    fn push(&mut self, ir: Ir, node: &rnix::SyntaxNode) -> ExprId {
        let id = ExprId(u32::try_from(self.exprs.len()).expect("program exceeds u32 exprs"));
        self.exprs.push(ir);
        self.spans.push(span_of(node));
        id
    }

    #[allow(clippy::too_many_lines)]
    fn lower_expr(&mut self, expr: &ast::Expr) -> Result<ExprId, LowerError> {
        match expr {
            ast::Expr::Literal(lit) => self.lower_literal(lit),
            ast::Expr::Ident(ident) => {
                let sym = sui_intern::intern(&ident_text(ident));
                Ok(self.push(Ir::Ident(sym), ident.syntax()))
            }
            ast::Expr::Str(st) => {
                let parts = self.lower_str_parts(st)?;
                Ok(self.push(Ir::Str(parts), st.syntax()))
            }
            ast::Expr::PathAbs(p) => {
                let parts = self.lower_path_parts(&p.parts())?;
                Ok(self.push(
                    Ir::Path {
                        kind: PathKind::Abs,
                        parts,
                    },
                    p.syntax(),
                ))
            }
            ast::Expr::PathRel(p) => {
                let parts = self.lower_path_parts(&p.parts())?;
                Ok(self.push(
                    Ir::Path {
                        kind: PathKind::Rel,
                        parts,
                    },
                    p.syntax(),
                ))
            }
            ast::Expr::PathHome(p) => {
                let parts = self.lower_path_parts(&p.parts())?;
                Ok(self.push(
                    Ir::Path {
                        kind: PathKind::Home,
                        parts,
                    },
                    p.syntax(),
                ))
            }
            ast::Expr::PathSearch(p) => {
                let content = p.content().ok_or(LowerError::Missing {
                    construct: "PathSearch",
                    field: "content",
                })?;
                Ok(self.push(Ir::SearchPath(content.text().to_string()), p.syntax()))
            }
            ast::Expr::Select(sel) => {
                let subject_ast = sel.expr().ok_or(LowerError::Missing {
                    construct: "Select",
                    field: "expr",
                })?;
                let subject = self.lower_expr(&subject_ast)?;
                let attrpath = sel.attrpath().ok_or(LowerError::Missing {
                    construct: "Select",
                    field: "attrpath",
                })?;
                let path = self.lower_attrpath(&attrpath)?;
                let or_default = match sel.default_expr() {
                    Some(d) => Some(self.lower_expr(&d)?),
                    None => None,
                };
                Ok(self.push(
                    Ir::Select {
                        subject,
                        path,
                        or_default,
                    },
                    sel.syntax(),
                ))
            }
            ast::Expr::HasAttr(ha) => {
                let subject_ast = ha.expr().ok_or(LowerError::Missing {
                    construct: "HasAttr",
                    field: "expr",
                })?;
                let subject = self.lower_expr(&subject_ast)?;
                let attrpath = ha.attrpath().ok_or(LowerError::Missing {
                    construct: "HasAttr",
                    field: "attrpath",
                })?;
                let path = self.lower_attrpath(&attrpath)?;
                Ok(self.push(Ir::HasAttr { subject, path }, ha.syntax()))
            }
            ast::Expr::Apply(app) => {
                let func_ast = app.lambda().ok_or(LowerError::Missing {
                    construct: "Apply",
                    field: "lambda",
                })?;
                let func = self.lower_expr(&func_ast)?;
                let arg_ast = app.argument().ok_or(LowerError::Missing {
                    construct: "Apply",
                    field: "argument",
                })?;
                let arg = self.lower_expr(&arg_ast)?;
                Ok(self.push(Ir::Apply { func, arg }, app.syntax()))
            }
            ast::Expr::Lambda(lam) => {
                let param_ast = lam.param().ok_or(LowerError::Missing {
                    construct: "Lambda",
                    field: "param",
                })?;
                let param = self.lower_param(&param_ast)?;
                let body_ast = lam.body().ok_or(LowerError::Missing {
                    construct: "Lambda",
                    field: "body",
                })?;
                let body = self.lower_expr(&body_ast)?;
                Ok(self.push(Ir::Lambda { param, body }, lam.syntax()))
            }
            ast::Expr::LetIn(li) => {
                let bindings = self.lower_entries(li)?;
                let body_ast = li.body().ok_or(LowerError::Missing {
                    construct: "LetIn",
                    field: "body",
                })?;
                let body = self.lower_expr(&body_ast)?;
                Ok(self.push(Ir::LetIn { bindings, body }, li.syntax()))
            }
            ast::Expr::LegacyLet(ll) => {
                let bindings = self.lower_entries(ll)?;
                Ok(self.push(Ir::LegacyLet { bindings }, ll.syntax()))
            }
            ast::Expr::AttrSet(set) => {
                let rec = set.rec_token().is_some();
                let bindings = self.lower_entries(set)?;
                Ok(self.push(Ir::AttrSet { rec, bindings }, set.syntax()))
            }
            ast::Expr::List(list) => {
                let mut items = Vec::new();
                for item in list.items() {
                    items.push(self.lower_expr(&item)?);
                }
                Ok(self.push(Ir::List(items), list.syntax()))
            }
            ast::Expr::BinOp(bo) => {
                let lhs_ast = bo.lhs().ok_or(LowerError::Missing {
                    construct: "BinOp",
                    field: "lhs",
                })?;
                let lhs = self.lower_expr(&lhs_ast)?;
                let rhs_ast = bo.rhs().ok_or(LowerError::Missing {
                    construct: "BinOp",
                    field: "rhs",
                })?;
                let rhs = self.lower_expr(&rhs_ast)?;
                let op = bo.operator().ok_or(LowerError::Missing {
                    construct: "BinOp",
                    field: "operator",
                })?;
                Ok(self.push(
                    Ir::BinOp {
                        op: op.into(),
                        lhs,
                        rhs,
                    },
                    bo.syntax(),
                ))
            }
            ast::Expr::UnaryOp(uo) => {
                let op = uo.operator().ok_or(LowerError::Missing {
                    construct: "UnaryOp",
                    field: "operator",
                })?;
                let inner_ast = uo.expr().ok_or(LowerError::Missing {
                    construct: "UnaryOp",
                    field: "expr",
                })?;
                let inner = self.lower_expr(&inner_ast)?;
                Ok(self.push(
                    Ir::UnaryOp {
                        op: op.into(),
                        expr: inner,
                    },
                    uo.syntax(),
                ))
            }
            ast::Expr::IfElse(ie) => {
                let cond_ast = ie.condition().ok_or(LowerError::Missing {
                    construct: "IfElse",
                    field: "condition",
                })?;
                let condition = self.lower_expr(&cond_ast)?;
                let then_ast = ie.body().ok_or(LowerError::Missing {
                    construct: "IfElse",
                    field: "body",
                })?;
                let then_body = self.lower_expr(&then_ast)?;
                let else_ast = ie.else_body().ok_or(LowerError::Missing {
                    construct: "IfElse",
                    field: "else_body",
                })?;
                let else_body = self.lower_expr(&else_ast)?;
                Ok(self.push(
                    Ir::IfElse {
                        condition,
                        then_body,
                        else_body,
                    },
                    ie.syntax(),
                ))
            }
            ast::Expr::With(w) => {
                let ns_ast = w.namespace().ok_or(LowerError::Missing {
                    construct: "With",
                    field: "namespace",
                })?;
                let namespace = self.lower_expr(&ns_ast)?;
                let body_ast = w.body().ok_or(LowerError::Missing {
                    construct: "With",
                    field: "body",
                })?;
                let body = self.lower_expr(&body_ast)?;
                Ok(self.push(Ir::With { namespace, body }, w.syntax()))
            }
            ast::Expr::Assert(a) => {
                let cond_ast = a.condition().ok_or(LowerError::Missing {
                    construct: "Assert",
                    field: "condition",
                })?;
                let condition = self.lower_expr(&cond_ast)?;
                let body_ast = a.body().ok_or(LowerError::Missing {
                    construct: "Assert",
                    field: "body",
                })?;
                let body = self.lower_expr(&body_ast)?;
                Ok(self.push(Ir::Assert { condition, body }, a.syntax()))
            }
            ast::Expr::Paren(p) => {
                let inner_ast = p.expr().ok_or(LowerError::Missing {
                    construct: "Paren",
                    field: "expr",
                })?;
                let inner = self.lower_expr(&inner_ast)?;
                Ok(self.push(Ir::Paren(inner), p.syntax()))
            }
            // `Root` sits in the Expr enum but only ever occurs at file top;
            // if one shows up nested, lower through it transparently (the
            // AST-side renderer unwraps it the same way).
            ast::Expr::Root(r) => {
                let inner = r.expr().ok_or(LowerError::Missing {
                    construct: "Root",
                    field: "expr",
                })?;
                self.lower_expr(&inner)
            }
            ast::Expr::CurPos(cp) => Ok(self.push(Ir::CurPos, cp.syntax())),
            ast::Expr::Error(e) => {
                let s = span_of(e.syntax());
                Err(LowerError::ParseErrorNode {
                    start: s.start,
                    end: s.end,
                })
            }
        }
    }

    fn lower_literal(&mut self, lit: &ast::Literal) -> Result<ExprId, LowerError> {
        match lit.kind() {
            ast::LiteralKind::Integer(i) => match i.value() {
                Ok(v) => Ok(self.push(Ir::Int(v), lit.syntax())),
                Err(_) => Err(LowerError::IntOutOfRange {
                    text: i.syntax().text().to_string(),
                }),
            },
            ast::LiteralKind::Float(f) => match f.value() {
                Ok(v) => Ok(self.push(Ir::Float(v), lit.syntax())),
                Err(_) => Err(LowerError::BadFloat {
                    text: f.syntax().text().to_string(),
                }),
            },
            ast::LiteralKind::Uri(u) => {
                Ok(self.push(Ir::Uri(u.syntax().text().to_string()), lit.syntax()))
            }
        }
    }

    fn lower_str_parts(&mut self, st: &ast::Str) -> Result<Vec<StrPart>, LowerError> {
        let mut parts = Vec::new();
        for part in st.normalized_parts() {
            match part {
                ast::InterpolPart::Literal(text) => parts.push(StrPart::Literal(text)),
                ast::InterpolPart::Interpolation(interp) => {
                    let inner = interp.expr().ok_or(LowerError::Missing {
                        construct: "Interpol",
                        field: "expr",
                    })?;
                    parts.push(StrPart::Interp(self.lower_expr(&inner)?));
                }
            }
        }
        Ok(parts)
    }

    fn lower_path_parts(
        &mut self,
        raw: &[ast::InterpolPart<ast::PathContent>],
    ) -> Result<Vec<PathPart>, LowerError> {
        let mut parts = Vec::new();
        for part in raw {
            match part {
                ast::InterpolPart::Literal(content) => {
                    parts.push(PathPart::Literal(content.text().to_string()));
                }
                ast::InterpolPart::Interpolation(interp) => {
                    let inner = interp.expr().ok_or(LowerError::Missing {
                        construct: "Interpol",
                        field: "expr",
                    })?;
                    parts.push(PathPart::Interp(self.lower_expr(&inner)?));
                }
            }
        }
        Ok(parts)
    }

    fn lower_attr(&mut self, attr: &ast::Attr) -> Result<AttrName, LowerError> {
        match attr {
            ast::Attr::Ident(ident) => {
                Ok(AttrName::Ident(sui_intern::intern(&ident_text(ident))))
            }
            ast::Attr::Str(st) => Ok(AttrName::Str(self.lower_str_parts(st)?)),
            ast::Attr::Dynamic(dy) => {
                let inner = dy.expr().ok_or(LowerError::Missing {
                    construct: "Dynamic",
                    field: "expr",
                })?;
                Ok(AttrName::Dynamic(self.lower_expr(&inner)?))
            }
        }
    }

    fn lower_attrpath(&mut self, path: &ast::Attrpath) -> Result<Vec<AttrName>, LowerError> {
        let mut out = Vec::new();
        for attr in path.attrs() {
            out.push(self.lower_attr(&attr)?);
        }
        if out.is_empty() {
            return Err(LowerError::Missing {
                construct: "Attrpath",
                field: "attrs",
            });
        }
        Ok(out)
    }

    /// Lower a `HasEntry` node's entries **in source order** — inherits and
    /// attrpath-values stay interleaved exactly as authored.
    fn lower_entries<N: HasEntry>(&mut self, node: &N) -> Result<Vec<Binding>, LowerError> {
        let mut out = Vec::new();
        for entry in node.entries() {
            match entry {
                ast::Entry::AttrpathValue(apv) => {
                    let attrpath = apv.attrpath().ok_or(LowerError::Missing {
                        construct: "AttrpathValue",
                        field: "attrpath",
                    })?;
                    let path = self.lower_attrpath(&attrpath)?;
                    let value_ast = apv.value().ok_or(LowerError::Missing {
                        construct: "AttrpathValue",
                        field: "value",
                    })?;
                    let value = self.lower_expr(&value_ast)?;
                    out.push(Binding::Path { path, value });
                }
                ast::Entry::Inherit(inh) => {
                    let from = match inh.from() {
                        Some(f) => {
                            let inner = f.expr().ok_or(LowerError::Missing {
                                construct: "InheritFrom",
                                field: "expr",
                            })?;
                            Some(self.lower_expr(&inner)?)
                        }
                        None => None,
                    };
                    let mut attrs = Vec::new();
                    for attr in inh.attrs() {
                        attrs.push(self.lower_attr(&attr)?);
                    }
                    out.push(Binding::Inherit { from, attrs });
                }
            }
        }
        Ok(out)
    }

    fn lower_param(&mut self, param: &ast::Param) -> Result<Param, LowerError> {
        match param {
            ast::Param::IdentParam(ip) => {
                let ident = ip.ident().ok_or(LowerError::Missing {
                    construct: "IdentParam",
                    field: "ident",
                })?;
                Ok(Param::Ident(sui_intern::intern(&ident_text(&ident))))
            }
            ast::Param::Pattern(pat) => {
                let mut entries = Vec::new();
                for pe in pat.pat_entries() {
                    let ident = pe.ident().ok_or(LowerError::Missing {
                        construct: "PatEntry",
                        field: "ident",
                    })?;
                    let name = sui_intern::intern(&ident_text(&ident));
                    let default = match pe.default() {
                        Some(d) => Some(self.lower_expr(&d)?),
                        None => None,
                    };
                    entries.push(PatternEntry { name, default });
                }
                let ellipsis = pat.ellipsis_token().is_some();
                let bind = match pat.pat_bind() {
                    Some(pb) => {
                        let ident = pb.ident().ok_or(LowerError::Missing {
                            construct: "PatBind",
                            field: "ident",
                        })?;
                        Some(sui_intern::intern(&ident_text(&ident)))
                    }
                    None => None,
                };
                Ok(Param::Pattern {
                    entries,
                    ellipsis,
                    bind,
                })
            }
        }
    }
}
