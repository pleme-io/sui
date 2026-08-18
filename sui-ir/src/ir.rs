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

/// Index into a [`Program`]'s plan arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlanId(pub u32);

impl PlanId {
    /// "No plan for this binder." A sentinel rather than `Option<PlanId>`
    /// because `PlanId` is a plain `u32` with no niche, so the `Option` would
    /// widen the field to 8 bytes; as a sentinel it fits in padding `Ir`
    /// already had and `size_of::<Ir>()` is unchanged at 48.
    pub const NONE: PlanId = PlanId(u32::MAX);

    /// The plan, or `None` when this binder needs none.
    ///
    /// A `None` is a POSITIVE statement, not a fallback: `sui-normalize`
    /// records a group ONLY when it has a duplicate static key or a dotted
    /// path, so its absence means the group has neither — exactly when the
    /// ordinary entry loop is already correct.
    #[must_use]
    pub fn get(self) -> Option<PlanId> {
        (self != PlanId::NONE).then_some(self)
    }

    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A binding group after `sui-normalize`'s parse-time splice, lowered to IR
/// indices.
///
/// ★ THIS LIVES IN A SIDE ARENA THAT NEITHER RENDERER READS, and that is the
/// whole design. `sui-ir/src/render.rs` says `render_ast` *"deliberately does
/// **not** call `lower()`, so comparing the two renders proves the lowering
/// preserved every child … Structure loss (a dropped/reordered/collapsed child)
/// shows up as a text diff"*, and `tests/differential.rs` byte-compares them.
/// A splice IS a collapsed child, so rewriting `Ir::AttrSet::bindings` would
/// turn that differential red on every merge seed. Keeping the plan beside the
/// tree instead of in it means the differential stays green BY CONSTRUCTION.
///
/// Normalizing in BOTH walks was rejected: it keeps the bytes equal but makes
/// the splice shared structure, so both sides would agree on a plan that dropped
/// a binding — and that is a real bug this pass has already produced once. It
/// would delete the last independent structural check on the pre-splice tree.
#[derive(Debug, Clone, PartialEq)]
pub struct IrGroupPlan {
    /// Effective recursiveness — the FIRST definition's, never an OR.
    pub recursive: bool,
    /// Static bindings after the splice, in insertion order. No name repeats.
    pub statics: Vec<IrStaticBinding>,
    /// `${e}` keys that did not constant-fold, resolved after the statics.
    pub dynamics: Vec<IrDynamicBinding>,
    /// One entry per `inherit (e) …;` clause, indexed by
    /// [`IrBound::InheritFrom`] so a clause's source evaluates once and is
    /// shared across the names it binds.
    pub inherit_froms: Vec<ExprId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IrStaticBinding {
    pub name: Symbol,
    pub value: IrBound,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IrDynamicBinding {
    /// The key, as an [`AttrName`] rather than an `ExprId`.
    ///
    /// A key IS an attr name, and the distinction is load-bearing here: an
    /// interpolated `Attr::Str` lowers to `AttrName::Str(parts)` and never
    /// produces an `Expr` node at all, so there is no `ExprId` to point at —
    /// and the arena invariant forbids creating one after lowering. Reusing
    /// the `AttrName` the ordinary lowering already produced is both correct
    /// and free.
    pub key: AttrName,
    pub value: IrBound,
}

/// What a planned binding binds.
///
/// Every arm is an INDEX. No `rnix` type appears here, deliberately:
/// `file_eval.rs` caches `Rc<Program>` per canonical path for the process
/// lifetime, so a rowan handle in the arena would pin every imported file's
/// whole green tree — the memory the lower-once design exists to release.
#[derive(Debug, Clone, PartialEq)]
pub enum IrBound {
    /// Evaluate this expression in the owning group's scope.
    Expr(ExprId),
    /// A nested group — a merged literal, or one invented by a dotted path.
    Group(PlanId),
    /// `inherit x` — resolve in the group's ENCLOSING scope, never its own rec
    /// scope, which is what makes it shadow rather than self-reference.
    Inherit,
    /// `inherit (e) x` — force `inherit_froms[from]` in the group's OWN scope,
    /// then select.
    InheritFrom { from: usize },
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
    /// `plan` is [`PlanId::NONE`] unless `sui-normalize` recorded a splice for
    /// this binder.
    ///
    /// ★ This does NOT weaken the render differential, which is what forbids
    /// putting the splice in `bindings`: `plan` is an opaque index the
    /// renderers ignore, `bindings` still holds every pre-splice child, and
    /// the plan's CONTENT stays in `Program::plans`. `render_ir` vs
    /// `render_ast` remains an independent structural check on the tree.
    ///
    /// It rides on the NODE rather than in a `FxHashMap<ExprId, PlanId>` on
    /// `Program` for two measured reasons and one design one:
    ///
    /// * `size_of::<Ir>()` is **unchanged at 48** — the `u32` lands in padding
    ///   the enum already had, which is why the sentinel is a `PlanId::NONE`
    ///   rather than an `Option<PlanId>` (no niche, so that would be 8 bytes).
    /// * it deletes a whole hash map from `Program`, which `file_eval.rs`
    ///   caches per canonical path for the PROCESS LIFETIME — memory is the
    ///   thing the lower-once design exists to conserve.
    /// * an arena indexes; it does not hash. `eval_ir` reads this field off
    ///   the `Ir` it is already matching on, so the lookup is not a lookup.
    ///
    /// ★ NOT among the reasons: speed. An earlier revision of this comment
    /// claimed the map cost ~62ns per binder eval for **+8.1%** on an
    /// attrset-saturated workload. That number was an ARTIFACT and is
    /// retracted. The A/B differed by more than the variable — the planned
    /// source carried one extra `let` binding, and `IrEnv::lookup` scans its
    /// scope, so every variable reference in a 40k-iteration hot loop paid for
    /// it. The +8% survived deleting the map entirely, which is what exposed
    /// it. With the binding count matched the delta is +0.65% / +0.98% /
    /// -0.00% against a ±1% A-vs-A noise floor: **not measurable.**
    /// `examples/plan_lookup_cost.rs` is that instrument, control included.
    LetIn {
        bindings: Vec<Binding>,
        body: ExprId,
        plan: PlanId,
    },
    /// `let { …; body = …; }` (legacy)
    LegacyLet { bindings: Vec<Binding> },
    /// `{ … }` / `rec { … }`
    AttrSet {
        rec: bool,
        bindings: Vec<Binding>,
        /// See [`Ir::LetIn`]'s `plan`.
        plan: PlanId,
    },
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
    /// Binding-group plans, in a side arena. See [`IrGroupPlan`] for why this
    /// is beside the tree rather than in it.
    ///
    /// Note the arena invariant `root.index() + 1 == exprs.len()` (asserted by
    /// `tests/differential.rs`) covers `exprs`/`spans` only — nothing may be
    /// APPENDED to `exprs` after lowering, which is why every `ExprId` a plan
    /// references is one the ordinary walk already produced.
    pub plans: Vec<PlanEntry>,
}

/// One entry in a [`Program`]'s plan arena.
///
/// ★ A REJECTED group is carried as DATA rather than failing `lower()`, and
/// that is deliberate. `lower()` is total on anything rnix parses, because
/// `tests/differential.rs` renders every lowered tree and byte-compares it
/// against the rowan render — including the proptest-generated seeds, which
/// DO emit `{ a = 1; a = 2; }`. Making lowering fail would panic that suite,
/// and "fixing" it by filtering duplicates out of the generators would delete
/// structural coverage of exactly the trees most at risk.
///
/// So the split mirrors the real architecture: rnix parses it, `sui-normalize`
/// rejects it, the IR carries the rejection, and `eval_ir` raises it when the
/// group is actually evaluated — which is also when nix's own error surfaces
/// to a user.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanEntry {
    /// A normalized binding group.
    Plan(IrGroupPlan),
    /// A group nix itself rejects, as the rendered message. A `String` rather
    /// than the typed `NormalizeError` because raising it is all eval needs,
    /// and it keeps `sui-normalize`'s error type out of the IR's public shape.
    Rejected(String),
}

impl Program {
    #[must_use]
    pub fn expr(&self, id: ExprId) -> &Ir {
        &self.exprs[id.index()]
    }

    /// The plan for the binder at `id`, if it needed one.
    ///
    /// `eval_ir` does NOT call this — it reads `plan` straight out of the
    /// `Ir::AttrSet` / `Ir::LetIn` it is already matching on, which is the
    /// whole point of moving the field onto the node. This is for callers
    /// holding only an `ExprId`.
    #[must_use]
    pub fn plan(&self, id: ExprId) -> Option<&PlanEntry> {
        self.plan_id(id).map(|p| &self.plans[p.index()])
    }

    /// The [`PlanId`] on the binder at `id`, if it needed a plan.
    #[must_use]
    pub fn plan_id(&self, id: ExprId) -> Option<PlanId> {
        match self.expr(id) {
            Ir::AttrSet { plan, .. } | Ir::LetIn { plan, .. } => plan.get(),
            _ => None,
        }
    }

    /// A nested plan by index.
    #[must_use]
    pub fn plan_at(&self, id: PlanId) -> &PlanEntry {
        &self.plans[id.index()]
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
            Ir::LetIn { bindings, body, .. } => {
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
