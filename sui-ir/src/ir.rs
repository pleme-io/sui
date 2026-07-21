//! The flat IR types: [`Program`], [`ExprId`], [`Ir`] and the supporting
//! node vocabulary.
//!
//! One [`Program`] per source file. `exprs` is an arena `Vec<Ir>` indexed by
//! [`ExprId`]; `spans` is the parallel side-table mapping each expression back
//! to its source byte range. Ids are assigned **post-order** during lowering —
//! every child's id is strictly less than its parent's, and the root is always
//! the last entry. That makes `exprs` a topologically-sorted flat vector:
//! a forward scan visits children before parents by construction.

use sui_intern::Symbol;

/// Index of an expression inside its [`Program`]'s `exprs` arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExprId(pub u32);

impl ExprId {
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Source byte range of an expression (from rowan's `TextRange`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

/// One piece of a (possibly interpolated) string literal. Literal pieces are
/// stored **normalized** — indent-stripped for `''` strings and
/// escape-processed — exactly as `rnix`'s `Str::normalized_parts` yields them
/// (the same surface the tree-walker evaluates today).
#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    Literal(String),
    Interp(ExprId),
}

/// One piece of a (possibly interpolated) path literal. Literal pieces are
/// stored as raw source text (`rnix` `PathContent`); path *normalization* is
/// an eval-time concern and deliberately not baked in by this slice.
#[derive(Debug, Clone, PartialEq)]
pub enum PathPart {
    Literal(String),
    Interp(ExprId),
}

/// Which path-literal form the source used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathKind {
    /// `/a/b`
    Abs,
    /// `./a` / `a/b`
    Rel,
    /// `~/a`
    Home,
}

/// One element of an attrpath (`a`, `"a${x}"`, `${e}`) — used by `Select`,
/// `HasAttr`, attrset/let bindings and `inherit`.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrName {
    /// Static identifier key. Interned at lower time (the L3 spec's
    /// "interned ident/select-path Symbols").
    Ident(Symbol),
    /// String key, possibly interpolated (`"a"` / `"a${x}"`).
    Str(Vec<StrPart>),
    /// Bare dynamic key `${e}`.
    Dynamic(ExprId),
}

/// One binding inside an attrset / `let … in` / legacy `let` body, in source
/// order (`HasEntry::entries()` order — attrpath-values and inherits stay
/// interleaved exactly as authored, which the evaluator's merge semantics
/// depend on).
#[derive(Debug, Clone, PartialEq)]
pub enum Binding {
    /// `a.b."c".${d} = value;`
    Path { path: Vec<AttrName>, value: ExprId },
    /// `inherit a "b";` / `inherit (expr) a b;`
    Inherit {
        from: Option<ExprId>,
        attrs: Vec<AttrName>,
    },
}

/// One `{ name ? default }` entry of a lambda pattern.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternEntry {
    pub name: Symbol,
    pub default: Option<ExprId>,
}

/// A lambda's formal parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    /// `x: body`
    Ident(Symbol),
    /// `{ a, b ? d, ... } @ bind: body`
    Pattern {
        entries: Vec<PatternEntry>,
        ellipsis: bool,
        bind: Option<Symbol>,
    },
}

/// Binary operators, 1:1 with `rnix::ast::BinOpKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOp {
    Concat,
    Update,
    Add,
    Sub,
    Mul,
    Div,
    And,
    Equal,
    Implication,
    Less,
    LessOrEq,
    More,
    MoreOrEq,
    NotEqual,
    Or,
    PipeRight,
    PipeLeft,
}

impl From<rnix::ast::BinOpKind> for BinOp {
    fn from(k: rnix::ast::BinOpKind) -> Self {
        use rnix::ast::BinOpKind as K;
        match k {
            K::Concat => BinOp::Concat,
            K::Update => BinOp::Update,
            K::Add => BinOp::Add,
            K::Sub => BinOp::Sub,
            K::Mul => BinOp::Mul,
            K::Div => BinOp::Div,
            K::And => BinOp::And,
            K::Equal => BinOp::Equal,
            K::Implication => BinOp::Implication,
            K::Less => BinOp::Less,
            K::LessOrEq => BinOp::LessOrEq,
            K::More => BinOp::More,
            K::MoreOrEq => BinOp::MoreOrEq,
            K::NotEqual => BinOp::NotEqual,
            K::Or => BinOp::Or,
            K::PipeRight => BinOp::PipeRight,
            K::PipeLeft => BinOp::PipeLeft,
        }
    }
}

impl BinOp {
    /// Stable name used by the normalized render (shared leaf-formatting
    /// between the IR and AST renderers — a name table, not structure).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            BinOp::Concat => "concat",
            BinOp::Update => "update",
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            BinOp::Div => "div",
            BinOp::And => "and",
            BinOp::Equal => "eq",
            BinOp::Implication => "impl",
            BinOp::Less => "lt",
            BinOp::LessOrEq => "le",
            BinOp::More => "gt",
            BinOp::MoreOrEq => "ge",
            BinOp::NotEqual => "ne",
            BinOp::Or => "or",
            BinOp::PipeRight => "pipe-right",
            BinOp::PipeLeft => "pipe-left",
        }
    }
}

/// Unary operators, 1:1 with `rnix::ast::UnaryOpKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    /// `!e`
    Invert,
    /// `-e`
    Negate,
}

impl From<rnix::ast::UnaryOpKind> for UnaryOp {
    fn from(k: rnix::ast::UnaryOpKind) -> Self {
        match k {
            rnix::ast::UnaryOpKind::Invert => UnaryOp::Invert,
            rnix::ast::UnaryOpKind::Negate => UnaryOp::Negate,
        }
    }
}

impl UnaryOp {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            UnaryOp::Invert => "invert",
            UnaryOp::Negate => "negate",
        }
    }
}

/// One lowered expression. Phase-1 lowering is 1:1 structural with the rnix
/// AST (SPEED.md L3): force order is untouched by construction — every rnix
/// `Expr` variant maps to exactly one `Ir` variant (including `Paren`, kept
/// as a node so the mapping is bijective on the parse surface).
#[derive(Debug, Clone, PartialEq)]
pub enum Ir {
    /// Integer literal.
    Int(i64),
    /// Float literal.
    Float(f64),
    /// URI literal (`https://…` — nix's bare-URI syntax).
    Uri(String),
    /// Variable reference, interned.
    Ident(Symbol),
    /// String literal, normalized parts (possibly interpolated).
    Str(Vec<StrPart>),
    /// Interpolatable path literal (`/a`, `./a`, `~/a`).
    Path { kind: PathKind, parts: Vec<PathPart> },
    /// Search path `<nixpkgs>` (raw source text, brackets included).
    SearchPath(String),
    /// `subject.a.b or default`
    Select {
        subject: ExprId,
        path: Vec<AttrName>,
        or_default: Option<ExprId>,
    },
    /// `subject ? a.b`
    HasAttr {
        subject: ExprId,
        path: Vec<AttrName>,
    },
    /// `func arg`
    Apply { func: ExprId, arg: ExprId },
    /// `param: body` / `{ … }: body`
    Lambda { param: Param, body: ExprId },
    /// `let …; in body`
    LetIn { bindings: Vec<Binding>, body: ExprId },
    /// `let { …; body = …; }` (legacy)
    LegacyLet { bindings: Vec<Binding> },
    /// `{ … }` / `rec { … }`
    AttrSet { rec: bool, bindings: Vec<Binding> },
    /// `[ a b c ]`
    List(Vec<ExprId>),
    /// `lhs <op> rhs`
    BinOp {
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
    },
    /// `!e` / `-e`
    UnaryOp { op: UnaryOp, expr: ExprId },
    /// `if cond then a else b`
    IfElse {
        condition: ExprId,
        then_body: ExprId,
        else_body: ExprId,
    },
    /// `with namespace; body`
    With { namespace: ExprId, body: ExprId },
    /// `assert cond; body`
    Assert { condition: ExprId, body: ExprId },
    /// `(e)` — kept 1:1 (the evaluator treats it as transparent; the IR
    /// keeps it so lowering is bijective and spans stay exact).
    Paren(ExprId),
    /// `__curPos` (parses; eval support is a later-slice question — the
    /// tree-walker returns NotImplemented for it today, and eval-through-IR
    /// will mirror that).
    CurPos,
}

/// One lowered source file: the flat expression arena + the parallel spans
/// side-table + the root id.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub exprs: Vec<Ir>,
    pub spans: Vec<Span>,
    pub root: ExprId,
}

impl Program {
    #[must_use]
    pub fn expr(&self, id: ExprId) -> &Ir {
        &self.exprs[id.index()]
    }

    #[must_use]
    pub fn span(&self, id: ExprId) -> Span {
        self.spans[id.index()]
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.exprs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exprs.is_empty()
    }

    /// Every child `ExprId` referenced by `exprs[id]`, in structural order.
    /// (Used by the invariant tests; later passes get real visitors.)
    #[must_use]
    pub fn children(&self, id: ExprId) -> Vec<ExprId> {
        let mut out = Vec::new();
        let push_attrname = |out: &mut Vec<ExprId>, a: &AttrName| match a {
            AttrName::Ident(_) => {}
            AttrName::Str(parts) => {
                for p in parts {
                    if let StrPart::Interp(e) = p {
                        out.push(*e);
                    }
                }
            }
            AttrName::Dynamic(e) => out.push(*e),
        };
        let push_bindings = |out: &mut Vec<ExprId>, bindings: &[Binding]| {
            for b in bindings {
                match b {
                    Binding::Path { path, value } => {
                        for a in path {
                            push_attrname(out, a);
                        }
                        out.push(*value);
                    }
                    Binding::Inherit { from, attrs } => {
                        if let Some(f) = from {
                            out.push(*f);
                        }
                        for a in attrs {
                            push_attrname(out, a);
                        }
                    }
                }
            }
        };
        match self.expr(id) {
            Ir::Int(_)
            | Ir::Float(_)
            | Ir::Uri(_)
            | Ir::Ident(_)
            | Ir::SearchPath(_)
            | Ir::CurPos => {}
            Ir::Str(parts) => {
                for p in parts {
                    if let StrPart::Interp(e) = p {
                        out.push(*e);
                    }
                }
            }
            Ir::Path { parts, .. } => {
                for p in parts {
                    if let PathPart::Interp(e) = p {
                        out.push(*e);
                    }
                }
            }
            Ir::Select {
                subject,
                path,
                or_default,
            } => {
                out.push(*subject);
                for a in path {
                    push_attrname(&mut out, a);
                }
                if let Some(d) = or_default {
                    out.push(*d);
                }
            }
            Ir::HasAttr { subject, path } => {
                out.push(*subject);
                for a in path {
                    push_attrname(&mut out, a);
                }
            }
            Ir::Apply { func, arg } => {
                out.push(*func);
                out.push(*arg);
            }
            Ir::Lambda { param, body } => {
                if let Param::Pattern { entries, .. } = param {
                    for e in entries {
                        if let Some(d) = e.default {
                            out.push(d);
                        }
                    }
                }
                out.push(*body);
            }
            Ir::LetIn { bindings, body } => {
                push_bindings(&mut out, bindings);
                out.push(*body);
            }
            Ir::LegacyLet { bindings } => push_bindings(&mut out, bindings),
            Ir::AttrSet { bindings, .. } => push_bindings(&mut out, bindings),
            Ir::List(items) => out.extend(items.iter().copied()),
            Ir::BinOp { lhs, rhs, .. } => {
                out.push(*lhs);
                out.push(*rhs);
            }
            Ir::UnaryOp { expr, .. } => out.push(*expr),
            Ir::IfElse {
                condition,
                then_body,
                else_body,
            } => {
                out.push(*condition);
                out.push(*then_body);
                out.push(*else_body);
            }
            Ir::With { namespace, body } => {
                out.push(*namespace);
                out.push(*body);
            }
            Ir::Assert { condition, body } => {
                out.push(*condition);
                out.push(*body);
            }
            Ir::Paren(e) => out.push(*e),
        }
        out
    }
}
