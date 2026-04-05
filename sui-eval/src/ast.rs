//! Nix AST — the tree structure produced by the parser.

/// A Nix expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    // Literals
    Int(i64),
    Float(f64),
    Str(String),
    Path(String),
    SearchPath(String),
    Bool(bool),
    Null,

    // Variables
    Var(String),

    // Compound
    List(Vec<Expr>),
    AttrSet(AttrSet),
    Select(Box<Expr>, AttrPath, Option<Box<Expr>>),  // expr.attr or expr.attr or default
    HasAttr(Box<Expr>, AttrPath),                      // expr ? attr

    // Operations
    UnaryOp(UnaryOp, Box<Expr>),
    BinOp(BinOp, Box<Expr>, Box<Expr>),
    Apply(Box<Expr>, Box<Expr>),  // function application (f x)

    // Control flow
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    Assert(Box<Expr>, Box<Expr>),
    With(Box<Expr>, Box<Expr>),
    Let(Vec<Binding>, Box<Expr>),

    // Functions
    Lambda(Pattern, Box<Expr>),
}

/// An attribute path (e.g., `a.b.c`).
pub type AttrPath = Vec<AttrName>;

/// An attribute name — either a static identifier or a dynamic expression.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrName {
    Static(String),
    Dynamic(Expr),
}

/// Function parameter patterns.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// Simple identifier: `x: body`
    Ident(String),
    /// Formal set: `{ a, b, c }: body`
    Formals {
        formals: Vec<Formal>,
        ellipsis: bool,
        name: Option<String>,  // for `args @ { ... }` or `{ ... } @ args`
    },
}

/// A single formal parameter in a set pattern.
#[derive(Debug, Clone, PartialEq)]
pub struct Formal {
    pub name: String,
    pub default: Option<Expr>,
}

/// An attribute set (regular or recursive).
#[derive(Debug, Clone, PartialEq)]
pub struct AttrSet {
    pub recursive: bool,
    pub bindings: Vec<Binding>,
}

/// A binding in a let or attribute set.
#[derive(Debug, Clone, PartialEq)]
pub enum Binding {
    /// `name = expr;`
    AttrPath(AttrPath, Expr),
    /// `inherit expr;` or `inherit (expr) attrs;`
    Inherit(Option<Expr>, Vec<String>),
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Impl,
    Update,
    Concat,
}
