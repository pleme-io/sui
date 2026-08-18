//! Parse-time attrset-binding normalizer — CppNix's `ExprAttrs::addAttr`.
//!
//! # The defect this exists to remove
//!
//! sui decides duplicate-key **merge-vs-overwrite at EVAL time**, by forcing the
//! colliding value to WHNF and asking *"is it an attrset?"*. Real nix decides it
//! at **PARSE time, from SYNTAX**. Because sui asks a different question at a
//! different time, all three engines give SILENT WRONG ANSWERS on legal nix.
//! Measured against nix 2.31.5 on 2026-08-18:
//!
//! ```text
//! let a = {b=1;}; a = {c=2;}; in a       nix {b=1;c=2;}    sui {c=2;}
//! rec { o = {e=1;}; o.x = 2; }           nix {o={e=1;x=2;};} sui {o={x=2;};}
//! let b=1; in { a={x=2;}; a=rec{b=99;c=b;}; }
//!                                        nix c=1           sui c=99
//! ```
//!
//! Every row exits 0. No error anywhere.
//!
//! # The rule, measured
//!
//! Merge iff **both sides are syntactic attrset literals** (`{…}` / `rec {…}`,
//! including the implicit ones a dotted path creates), **recursively**. Reject
//! otherwise. Decided from SYNTAX, never from the runtime value — so
//! `let x = {b=1;}; in { s = x; s = {c=2;}; }` is a parse error even though the
//! value of `x` *is* an attrset.
//!
//! # ★ It is a destructive SPLICE, not predicate-plus-union
//!
//! nix splices the second side's bindings INTO THE FIRST-DECLARED NODE. The
//! first node's `rec` governs and a later `rec` is DISCARDED; the second side's
//! bindings are RE-SCOPED into the first node's scope:
//!
//! ```text
//! { a = rec {b=1;}; a = {c = b+1;}; }.a.c   -> 2    non-rec side binds to the FIRST's rec scope
//! { a = {b=1;}; a = rec {c=2; d=c;}; }      -> parse error: undefined variable 'c'
//!                                                  a standalone-valid rec block's INTERNAL
//!                                                  reference is destroyed by the merge
//! { a = rec {b=1; c=b+1;}; a.d = 3; }       -> `{ a = rec { b=1; c=(b+1); d=3; }; }`
//!                                                  the DOTTED member lands INSIDE the rec
//! ```
//!
//! **No value-level merge can express re-scoping.** That is why this is a
//! structural pass and not a fix inside any engine's collection loop.
//!
//! # Why a side-table rather than a rewritten tree
//!
//! rnix's CST is lossless and IMMUTABLE — the tree cannot be spliced. So this
//! pass emits a [`NormalizeTable`]: a plan per binding-group node, which each
//! engine consumes INSTEAD OF its own `for entry in set.entries()` loop. The
//! shape is deliberately modelled on `sui-resolve`'s `ResolveTable`, including
//! its most useful property:
//!
//! **An absent entry means "nothing to normalize"** — no collision, no dotted
//! path — so every engine's existing fast path is untouched for the
//! overwhelming majority of attrsets, and the table stays small. That is the
//! parity-by-construction argument: this pass can only change the groups it
//! records, and it records only the groups that today are wrong.
//!
//! # Placement
//!
//! This crate must be reachable by all three engines, so it sits at
//! `sui-resolve`'s level and depends on `sui-intern` + `rnix` + `rowan` only.
//! It must NOT depend on `sui-eval`: `sui-bytecode` carries `sui-eval` as a
//! path-only dev-dep to break a publish cycle, and a real edge here would close
//! it. Hence the error type is self-contained — no `EvalError` — and each
//! engine maps [`NormalizeError`] into its own error on the way out.
//!
//! # Status
//!
//! **STAGE 0: UNWIRED.** Nothing consumes this yet, by design — the rule is
//! built and proven against the oracle before it can change any engine's
//! behaviour. Wiring is stages 1–4.

use rnix::ast::{self, HasEntry};
use rowan::ast::AstNode;
use sui_intern::Symbol;

/// One component of an attribute path, after constant-folding.
///
/// The static/dynamic split is a **constant-fold on node kind**, not
/// "quoted vs unquoted" — which is the trap that makes intuition wrong here:
///
/// ```text
/// a          Ident        -> STATIC
/// "a"        Str, no interpolation
///                         -> STATIC   (`{ "a" = 1; a = 2; }` is a duplicate)
/// ${"a"}     Dynamic wrapping a pure Str
///                         -> STATIC   (`{ ${"a"} = 1; a = 2; }` is a duplicate)
/// "${"a"}"   Str WITH interpolation
///                         -> DYNAMIC  (parses fine; errors only when FORCED)
/// ${"a"+""}  Dynamic wrapping a non-Str
///                         -> DYNAMIC
/// ```
///
/// Getting this wrong in the permissive direction turns a currently-correct
/// answer into a throw: `{ a = {p=1;}; ${"a"} = {q=2;}; }` merges in nix, and
/// sui already agrees with it today.
#[derive(Clone, Debug)]
pub enum AttrKey {
    /// Folded to a compile-time-known name.
    Static(Symbol),
    /// Genuinely dynamic — resolved, and checked for collisions, only when the
    /// enclosing attrset is FORCED. Never participates in a parse-time merge.
    Dynamic(ast::Expr),
}

/// A binding after normalization.
#[derive(Clone, Debug)]
pub enum Binding {
    /// A value that is NOT a syntactic attrset literal, so it can never merge.
    Leaf(ast::Expr),
    /// A syntactic attrset literal, or an implicit one created by a dotted
    /// path. Mergeable — and merging splices into THIS node.
    ///
    /// `Rc` because a consumer clones a sub-plan every time it builds the
    /// nested group lazily; owned, that is a DEEP copy of the whole subtree on
    /// each evaluation. During construction the `Rc` is uniquely owned, so
    /// `Rc::make_mut` below is a no-op rather than a copy.
    Group(std::rc::Rc<GroupPlan>),
    /// `inherit x` — resolve `x` in the group's ENCLOSING scope, never its own
    /// rec scope. That is what makes an inherited binding shadow rather than
    /// self-reference, and why it can never merge.
    Inherit,
    /// `inherit (e) x` — force `GroupPlan::inherit_froms[from]`, then select.
    ///
    /// ★ An INDEX, not a cloned expression. One `inherit (e) a b c;` clause
    /// must evaluate `e` AT MOST ONCE and share the result across all three
    /// names; cloning the expr per name evaluates it three times, which is
    /// observable through `builtins.trace` and through any impure source. The
    /// index is per-GROUP because the splice moves a clause between groups.
    InheritFrom {
        /// Index into the owning [`GroupPlan::inherit_froms`].
        from: usize,
    },
}

/// A dynamic-keyed binding, kept out of the static map.
#[derive(Clone, Debug)]
pub struct DynamicBinding {
    /// The key expression, evaluated at force time in the owning group's scope.
    pub key: ast::Expr,
    /// The bound value. A [`Binding`], not a bare expression: a dynamic key
    /// can bind an attrset literal (`{ ${k} = {a=1;}; }`), which is already
    /// lowered to a `Group` by the time it arrives here.
    pub value: Binding,
}

/// One static binding, with the source position of its FIRST definition.
///
/// The position is carried because nix's duplicate message names two of them —
/// `attribute 'a.b' already defined at «string»:1:3` for the first, and the
/// error's own location for the second.
#[derive(Clone, Debug)]
pub struct StaticBinding {
    /// The folded attribute name.
    pub name: Symbol,
    /// What is bound.
    pub binding: Binding,
    /// Byte offset where this name was FIRST defined.
    pub pos: u32,
}

/// The normalized plan for one binding group — an attrset, a `let`, a `rec`,
/// or a legacy `let { … }`.
#[derive(Clone, Debug, Default)]
pub struct GroupPlan {
    /// Effective recursiveness. **The FIRST definition's, never an OR of the
    /// two** — a later `rec` is discarded, which is what makes
    /// `{ a = {b=1;}; a = rec {c=2; d=c;}; }` a parse error in nix.
    pub recursive: bool,
    /// Static bindings after the splice, in nix's insertion order.
    pub statics: Vec<StaticBinding>,
    /// Dynamic-keyed bindings. Collisions among these — and against
    /// `statics` — are an EVAL-time error, and only when forced.
    pub dynamics: Vec<DynamicBinding>,
    /// One entry per `inherit (e) …;` clause that landed on this group,
    /// INCLUDING clauses spliced in from a later side. Referenced by index
    /// from [`Binding::InheritFrom`] so a clause's source is evaluated once
    /// and shared across its names.
    ///
    /// Evaluated in the group's OWN scope, which is rec-visible when the group
    /// is recursive — measured: `rec { b = {x=99;}; inherit (b) x; }` is
    /// `x = 99`, so the source sees the group it is being bound into.
    pub inherit_froms: Vec<ast::Expr>,
}

impl GroupPlan {
    fn index_of(&self, sym: Symbol) -> Option<usize> {
        self.statics.iter().position(|b| b.name == sym)
    }
}

/// A parse-time rejection. Carries what is needed to render nix's message.
///
/// nix has FIVE distinct reject messages, not one; this models the two that
/// duplicate-attribute normalization produces. The other three (dynamic
/// attributes in `let`/`inherit`, and the `${e}` force-time collision) belong
/// to their own layers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NormalizeError {
    /// `attribute '<dotted-path>' already defined at <pos>`
    DuplicateAttr {
        /// The full dotted path, already quoted per `showAttrPath` rules.
        path: String,
        /// Byte offset of the FIRST definition.
        first: u32,
        /// Byte offset of the offending one.
        second: u32,
    },
    /// `duplicate formal function argument '<name>'`
    DuplicateFormal {
        /// The repeated formal's name.
        name: String,
        /// Byte offset to report.
        at: u32,
    },
}

impl std::fmt::Display for NormalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateAttr { path, .. } => {
                write!(f, "attribute '{path}' already defined")
            }
            Self::DuplicateFormal { name, .. } => {
                write!(f, "duplicate formal function argument '{name}'")
            }
        }
    }
}

impl std::error::Error for NormalizeError {}

/// Render one path component the way CppNix's `showAttrPath` does.
///
/// A component is emitted bare iff it matches `[A-Za-z_][A-Za-z0-9_'-]*` AND is
/// not one of exactly NINE reserved words. Everything else is quoted, with `"`
/// and newline escaped. Measured bare: `a`, `a1`, `_a`, `a-b`, `a'b`, and
/// notably `or`, `true`, `false`, `null` — which are NOT reserved in this
/// position. Measured quoted: `"1a"`, `"a b"`, `"a.b"`, `""`, and the nine.
#[must_use]
pub fn show_attr_component(name: &str) -> String {
    const RESERVED: [&str; 9] = [
        "if", "then", "else", "assert", "with", "let", "in", "rec", "inherit",
    ];
    let mut chars = name.chars();
    let bare = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => chars
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '\'' | '-')),
        _ => false,
    } && !RESERVED.contains(&name);

    if bare {
        name.to_string()
    } else {
        format!(
            "\"{}\"",
            name.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
        )
    }
}

/// True iff `expr` is a syntactic attrset literal, seeing through PARENTHESES.
///
/// ★ Parens are the trap. CppNix's grammar discards them (`'(' expr ')' { $$ =
/// $2; }`) so its `dynamic_cast<ExprAttrs *>` sees straight through; rnix keeps
/// `NODE_PAREN` as a real CST node, so a bare `matches!(e, Expr::AttrSet(_))`
/// misses `((({b=1;})))` — which nix merges. Parens are transparent
/// RECURSIVELY, including around `rec`.
///
/// Every OTHER wrapper is opaque, verified: `let`, `with`, `assert`, `if`, and
/// `//` all make the value non-mergeable even when it evaluates to an attrset.
#[must_use]
pub fn as_attrset_literal(expr: &ast::Expr) -> Option<ast::AttrSet> {
    match expr {
        ast::Expr::AttrSet(set) => Some(set.clone()),
        ast::Expr::Paren(p) => p.expr().as_ref().and_then(as_attrset_literal),
        _ => None,
    }
}

/// Strip `(…)` recursively. CppNix's grammar discards parens outright, so
/// anything that asks "what KIND of expression is this?" must strip them first
/// or it will answer about the wrapper.
#[must_use]
pub fn strip_parens(expr: &ast::Expr) -> ast::Expr {
    match expr {
        ast::Expr::Paren(p) => p.expr().as_ref().map_or_else(|| expr.clone(), strip_parens),
        other => other.clone(),
    }
}

/// Constant-fold an attribute-path component, per the rule on [`AttrKey`].
/// The TEXT-level half of [`fold_attr`]: the static name an attribute folds
/// to, or `None` if it is genuinely dynamic.
///
/// Split out because [`needs_plan`] runs on every attribute of every attrset in
/// every parsed file just to decide whether a plan is needed, and interning
/// there costs a `String` allocation plus a hash into the intern table per
/// attribute — measured as the dominant parse-time cost of this pass. This
/// borrows the token's text instead.
///
/// `fold_attr` is defined in terms of this, so the fold rule still has exactly
/// ONE implementation.
#[must_use]
pub fn fold_attr_name(attr: &ast::Attr) -> Option<String> {
    match attr {
        // `ident_token()` borrows; `syntax().text().to_string()` allocates.
        ast::Attr::Ident(ident) => Some(
            ident
                .ident_token()
                .map_or_else(|| ident.syntax().text().to_string(), |t| t.text().to_string()),
        ),
        ast::Attr::Str(s) => literal_str_text(s),
        ast::Attr::Dynamic(dy) => match dy.expr().as_ref().map(strip_parens) {
            Some(ast::Expr::Str(ref s)) => literal_str_text(s),
            _ => None,
        },
    }
}

#[must_use]
pub fn fold_attr(attr: &ast::Attr) -> Option<AttrKey> {
    match attr {
        ast::Attr::Ident(ident) => Some(AttrKey::Static(sui_intern::intern(
            &ident.syntax().text().to_string(),
        ))),
        // A quoted key folds to STATIC iff it has no interpolation; with
        // interpolation it is DYNAMIC.
        //
        // ★ It must never yield `None`. The caller skips a `None` component,
        // which DROPS it from the attrpath — so `a."${"b"}" = 1; a."${"c"}" = 2;`
        // collapsed to two bindings of plain `a` and was rejected as a
        // duplicate, on input nix ACCEPTS. A false reject is the dangerous
        // direction: it refuses working code. Caught by scanning the fleet,
        // where it hit two of sui's own corpus fixtures.
        ast::Attr::Str(s) => Some(literal_str_text(s).map_or_else(
            || AttrKey::Dynamic(ast::Expr::Str(s.clone())),
            |t| AttrKey::Static(sui_intern::intern(&t)),
        )),
        // `${e}` folds iff `e` is a pure string literal — AFTER stripping
        // parens. `${("a")}` and `${''a''}` both fold in nix; a fold that
        // only looks for a bare `Expr::Str` misses them and demotes a
        // mergeable static key to dynamic, which silently changes the answer.
        ast::Attr::Dynamic(dy) => {
            let inner = dy.expr()?;
            let stripped = strip_parens(&inner);
            match &stripped {
                ast::Expr::Str(s) => literal_str_text(s)
                    .map(|t| AttrKey::Static(sui_intern::intern(&t)))
                    .or(Some(AttrKey::Dynamic(inner.clone()))),
                _ => Some(AttrKey::Dynamic(inner.clone())),
            }
        }
    }
}

/// The text of a string node iff it is a pure literal (no `${…}` parts).
fn literal_str_text(s: &ast::Str) -> Option<String> {
    // `normalized_parts`, not `parts`: it applies `''` indentation stripping and
    // escape processing, so the folded key is the string nix would compare —
    // `{ "a\tb" = 1; }` must fold to a TAB, not to a backslash and a `t`.
    let mut out = String::new();
    for part in s.normalized_parts() {
        match part {
            ast::InterpolPart::Literal(text) => out.push_str(&text),
            ast::InterpolPart::Interpolation(_) => return None,
        }
    }
    Some(out)
}

/// The normalized plans for one parsed source tree.
///
/// Keyed by the binding-group node's `text_range().start()`, exactly as
/// `sui_resolve::ResolveTable` keys idents. An unrecorded offset means the
/// group needs no normalization — no duplicate static key and no dotted path —
/// so consumers keep their existing path for it.
#[derive(Clone, Debug, Default)]
pub struct NormalizeTable {
    by_offset: rustc_hash::FxHashMap<u32, GroupPlan>,
}

impl NormalizeTable {
    /// An empty table — every lookup returns `None`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The plan recorded for the group node starting at `text_offset`, if any.
    #[must_use]
    pub fn get(&self, text_offset: u32) -> Option<&GroupPlan> {
        self.by_offset.get(&text_offset)
    }

    /// Number of recorded groups. Small by construction — see the module docs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_offset.len()
    }

    /// True when no group needed normalization.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_offset.is_empty()
    }

    /// Iterate the recorded `(text_offset, plan)` pairs. Consumers merge these
    /// into their own per-`(source_id, offset)` table, exactly as
    /// `sui-resolve`'s consumers do.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &GroupPlan)> {
        self.by_offset.iter().map(|(o, p)| (*o, p))
    }

    fn insert(&mut self, offset: u32, plan: GroupPlan) {
        self.by_offset.insert(offset, plan);
    }
}

// ── The splice ───────────────────────────────────────────────────────────

/// Byte offset where a node starts.
fn offset_of(node: &rowan::SyntaxNode<rnix::NixLanguage>) -> u32 {
    u32::from(node.text_range().start())
}

/// Insert `value` at `path` into `group` — CppNix's `ExprAttrs::addAttr`.
///
/// Descends `path`, creating IMPLICIT non-recursive groups for intermediate
/// components (that is what makes `{ a.b = 1; }` and `{ a = { b = 1; }; }`
/// produce identical trees). At the leaf:
///
/// * absent            -> insert
/// * both are `Group`  -> **SPLICE**: recursively add the incoming group's
///                        members into the EXISTING one, so the existing
///                        node's `recursive` survives and the incoming
///                        group's is discarded
/// * anything else     -> reject, naming the dotted path
///
/// `trail` carries the components consumed so far, purely so the error can name
/// the full path — nix reports `attribute 'a.b' already defined`, not `'b'`.
fn add_attr(
    group: &mut GroupPlan,
    path: &[AttrKey],
    value: Binding,
    pos: u32,
    trail: &mut Vec<String>,
) -> Result<(), NormalizeError> {
    let Some((head, rest)) = path.split_first() else {
        // An empty attrpath is not constructible from the grammar; treat it as
        // "nothing to add" rather than panicking on a slice index — the exact
        // shape that made the bytecode compiler crash on `{ a.b = 1; a.b = 2; }`.
        return Ok(());
    };

    let sym = match head {
        AttrKey::Static(s) => *s,
        AttrKey::Dynamic(key) => {
            // A dynamic component never participates in a parse-time merge and
            // never rejects at parse. It is deferred wholesale: nix checks it
            // when the attrset is FORCED, and not at all if it never is —
            // `let s = { "${"a"}" = 1; a = 2; }; in 42` is `42`, no error.
            //
            // An INTERMEDIATE dynamic component (`a.${k}.b = 1`) mints a
            // fresh implicit group and the REST OF THE PATH GOES INSIDE IT.
            //
            // ★ Returning here without recording anything silently DROPPED
            // the binding: `let k = "z"; in { a.${k}.b = 1; }` built
            // `{ a = { }; }` where nix builds `{ a = { z = { b = 1; }; }; }`.
            // That is the exact failure class this crate exists to remove,
            // reintroduced by its own fix — and the pre-existing path had it
            // RIGHT. Caught by `sui-ir`'s eval differential, which is the
            // only gate that compares the two engines on this shape.
            //
            // A fresh group per occurrence is also why an intermediate
            // dynamic can never collide, and therefore why a reported
            // duplicate path is always all-static.
            if rest.is_empty() {
                group.dynamics.push(DynamicBinding {
                    key: key.clone(),
                    value,
                });
            } else {
                let mut sub = GroupPlan::default();
                add_attr(&mut sub, rest, value, pos, trail)?;
                group.dynamics.push(DynamicBinding {
                    key: key.clone(),
                    value: Binding::Group(std::rc::Rc::new(sub)),
                });
            }
            return Ok(());
        }
    };

    trail.push(show_attr_component(&sui_intern::resolve(sym)));

    if rest.is_empty() {
        match group.index_of(sym) {
            None => {
                group.statics.push(StaticBinding {
                    name: sym,
                    binding: value,
                    pos,
                });
            }
            Some(idx) => {
                let first = group.statics[idx].pos;
                // ★ THE SPLICE. Both sides must be syntactic groups; the
                // EXISTING one is the survivor, so its `recursive` governs and
                // the incoming one's is dropped on the floor — which is exactly
                // why a later `rec`'s internal references break.
                match (&mut group.statics[idx].binding, value) {
                    (Binding::Group(existing_rc), Binding::Group(incoming_rc)) => {
                        let existing = std::rc::Rc::make_mut(existing_rc);
                        let incoming = std::rc::Rc::try_unwrap(incoming_rc)
                            .unwrap_or_else(|rc| (*rc).clone());
                        // The incoming group's `inherit (e)` clauses move into
                        // the existing group, so every `InheritFrom` index in
                        // its members must be REBASED onto the existing
                        // group's arena. Missing this silently points a
                        // spliced inherit at the wrong source expression.
                        let base = existing.inherit_froms.len();
                        existing.inherit_froms.extend(incoming.inherit_froms);
                        let incoming_statics: Vec<StaticBinding> = incoming
                            .statics
                            .into_iter()
                            .map(|mut m| {
                                rebase_inherit_from(&mut m.binding, base);
                                m
                            })
                            .collect();
                        for member in incoming_statics {
                            let mut sub_trail = trail.clone();
                            add_attr(
                                existing,
                                &[AttrKey::Static(member.name)],
                                member.binding,
                                member.pos,
                                &mut sub_trail,
                            )?;
                        }
                        existing.dynamics.extend(incoming.dynamics);
                    }
                    _ => {
                        return Err(NormalizeError::DuplicateAttr {
                            path: trail.join("."),
                            first,
                            second: pos,
                        });
                    }
                }
            }
        }
    } else {
        // Intermediate component: get-or-create an implicit NON-recursive group.
        let idx = match group.index_of(sym) {
            Some(idx) => idx,
            None => {
                group.statics.push(StaticBinding {
                    name: sym,
                    binding: Binding::Group(std::rc::Rc::new(GroupPlan::default())),
                    pos,
                });
                group.statics.len() - 1
            }
        };
        let first = group.statics[idx].pos;
        let Binding::Group(sub_rc) = &mut group.statics[idx].binding else {
            // The path descends THROUGH a non-mergeable binding, e.g.
            // `{ a = 1; a.b = 2; }`. nix names the SHALLOWEST level where a
            // side is not a literal — which is the trail as it stands now.
            return Err(NormalizeError::DuplicateAttr {
                path: trail.join("."),
                first,
                second: pos,
            });
        };
        add_attr(std::rc::Rc::make_mut(sub_rc), rest, value, pos, trail)?;
    }
    Ok(())
}

/// Shift every `InheritFrom` index in `binding` (and, recursively, in any
/// nested group) by `base`. Used when a group is spliced into another and its
/// `inherit_froms` arena is appended to the survivor's.
fn rebase_inherit_from(binding: &mut Binding, base: usize) {
    match binding {
        Binding::InheritFrom { from } => *from += base,
        Binding::Group(sub) => {
            for m in &mut std::rc::Rc::make_mut(sub).statics {
                rebase_inherit_from(&mut m.binding, base);
            }
        }
        Binding::Leaf(_) | Binding::Inherit => {}
    }
}

/// Lower one value expression to a [`Binding`].
///
/// A syntactic attrset literal becomes a `Group` — recursively normalized —
/// so that merging is uniformly "both sides are Groups". Everything else is a
/// `Leaf` and can never merge. This is the single place the syntax-not-value
/// rule is decided.
fn lower_value(expr: &ast::Expr) -> Result<Binding, NormalizeError> {
    match as_attrset_literal(expr) {
        Some(set) => Ok(Binding::Group(std::rc::Rc::new(plan_for_entries(
            &set,
            set.rec_token().is_some(),
        )?))),
        None => Ok(Binding::Leaf(expr.clone())),
    }
}

/// Build the plan for one `HasEntry` node's entries, in source order.
fn plan_for_entries<N: HasEntry>(node: &N, recursive: bool) -> Result<GroupPlan, NormalizeError> {
    let mut plan = GroupPlan {
        recursive,
        ..GroupPlan::default()
    };

    for entry in node.entries() {
        match entry {
            ast::Entry::AttrpathValue(av) => {
                let (Some(attrpath), Some(value)) = (av.attrpath(), av.value()) else {
                    continue;
                };
                let mut path = Vec::new();
                for attr in attrpath.attrs() {
                    let Some(key) = fold_attr(&attr) else { continue };
                    path.push(key);
                }
                let pos = offset_of(av.syntax());
                let binding = lower_value(&value)?;
                let mut trail = Vec::new();
                add_attr(&mut plan, &path, binding, pos, &mut trail)?;
            }
            ast::Entry::Inherit(inh) => {
                // Register the clause's source ONCE; every name it binds
                // refers to it by index. Cloning the expr per name would
                // evaluate the source N times.
                let from_idx = inh.from().and_then(|f| f.expr()).map(|e| {
                    plan.inherit_froms.push(e);
                    plan.inherit_froms.len() - 1
                });
                for attr in inh.attrs() {
                    let Some(AttrKey::Static(sym)) = fold_attr(&attr) else {
                        // A dynamic key in `inherit` is rejected by nix at
                        // parse with its OWN message, which belongs to a
                        // different layer than duplicate detection.
                        continue;
                    };
                    let pos = offset_of(attr.syntax());
                    let mut trail = Vec::new();
                    let binding = match from_idx {
                        Some(from) => Binding::InheritFrom { from },
                        None => Binding::Inherit,
                    };
                    add_attr(&mut plan, &[AttrKey::Static(sym)], binding, pos, &mut trail)?;
                }
            }
        }
    }
    Ok(plan)
}

/// Normalize every binding group in a parsed tree.
///
/// Returns the plans for groups that actually need one. A group with no
/// duplicate static key and no dotted path is NOT recorded — consumers keep
/// their existing path for it, which is what bounds this pass's blast radius.
///
/// # Errors
///
/// Returns the first [`NormalizeError`] in source order, matching nix's
/// parse-time rejection.
pub fn normalize(root: &ast::Root) -> Result<NormalizeTable, NormalizeError> {
    let mut table = NormalizeTable::new();
    record_plans(root, &mut table)?;
    Ok(table)
}

/// Does this group need a plan at all? True iff some entry has a dotted path
/// or two entries share a folded static key.
fn needs_plan<N: HasEntry>(node: &N) -> bool {
    // Interns and compares SYMBOLS (u32), not text. Measured: swapping this for
    // borrowed text to "avoid the intern" made a real nixpkgs eval SLOWER —
    // interning buys cheap integer comparison in `seen`, and the replacement
    // paid a `String` allocation per attribute plus O(n) string compares. The
    // intern is the optimization, not the cost.
    //
    // The cheap discriminator still runs first: a dotted path returns `true`
    // with no allocation at all, and most attrsets exit there or with `seen`
    // never growing past a few entries.
    let mut seen: Vec<Symbol> = Vec::new();
    for entry in node.entries() {
        match entry {
            ast::Entry::AttrpathValue(av) => {
                let Some(attrpath) = av.attrpath() else { continue };
                // Cheapest discriminator first: a dotted path always needs a
                // plan, and answering that costs no allocation at all.
                let mut attrs = attrpath.attrs();
                let Some(first) = attrs.next() else { continue };
                if attrs.next().is_some() {
                    return true;
                }
                if let Some(AttrKey::Static(sym)) = fold_attr(&first) {
                    if seen.contains(&sym) {
                        return true;
                    }
                    seen.push(sym);
                }
            }
            ast::Entry::Inherit(inh) => {
                for attr in inh.attrs() {
                    if let Some(AttrKey::Static(sym)) = fold_attr(&attr) {
                        if seen.contains(&sym) {
                            return true;
                        }
                        seen.push(sym);
                    }
                }
            }
        }
    }
    false
}

/// The plan for ONE binding group, computed on demand.
///
/// `None` means the group needs no normalization — no duplicate static key and
/// no dotted path — so the caller keeps its existing construction path.
///
/// ★ THIS IS THE PREFERRED ENTRY POINT. [`normalize`] walks a whole file and
/// plans every binder in it, including the ones laziness never evaluates;
/// measured on a real nixpkgs eval that is a ~4% wall-clock tax for work that
/// is mostly discarded. Planning a group when it is first EVALUATED moves the
/// cost onto the groups that actually matter, and the result is identical: a
/// group's plan depends only on its own entries.
///
/// # Errors
///
/// Returns the group's [`NormalizeError`] if its bindings are a duplicate nix
/// rejects.
pub fn plan_for_group<N: HasEntry>(
    node: &N,
    recursive: bool,
) -> Result<Option<GroupPlan>, NormalizeError> {
    if !needs_plan(node) {
        return Ok(None);
    }
    plan_for_entries(node, recursive).map(Some)
}

/// The plan for a group, **whether or not it needs one** — the total sibling
/// of [`plan_for_group`].
///
/// The two exist because their consumers want opposite things and only one of
/// them can be right for both.
///
/// [`plan_for_group`] is GATED, and that gate is what bounds a consumer's blast
/// radius: a `None` says "no duplicate static key, no dotted path", i.e.
/// exactly the groups whose existing construction path is already correct, so
/// adopting it can only change the answers that were wrong. The tree-walker
/// and `eval_ir` both want that — they keep their entry loops for the other
/// 67% of the fleet.
///
/// A consumer that intends to RETIRE its entry loops wants this one instead. A
/// gated planner cannot retire anything, because the un-planned groups still
/// need the code path that would be deleted; and keeping both forever is two
/// implementations of one rule, which is how the two halves drift. The bytecode
/// compiler is that consumer: its five entry buckets are the defect (a key
/// reaching two buckets is emitted twice), so it routes EVERY group through the
/// plan and deletes them.
///
/// # Errors
///
/// Same as [`plan_for_group`] — a group nix itself rejects.
pub fn plan_for_group_total<N: HasEntry>(
    node: &N,
    recursive: bool,
) -> Result<GroupPlan, NormalizeError> {
    plan_for_entries(node, recursive)
}

/// Record a plan for every binding group in the tree.
///
/// ★ ITERATES `descendants()`, NOT a recursion over `ast::Expr` children.
///
/// The first cut recursed with
/// `for child in expr.syntax().children() { if let Some(e) = ast::Expr::cast(child) { … } }`,
/// which looks total and is not: an attrset's children are `AttrpathValue` and
/// `Inherit` nodes, and NEITHER casts to `ast::Expr`. So the walk stopped at
/// the first attrset and never descended into a binding's VALUE. Only a binder
/// that was itself an expression-child of the root ever got a plan; everything
/// nested silently kept the old, wrong construction path:
///
/// ```text
/// { outer = { a = rec { b = c + 1; d = 2; }; a.c = d + 3; }; }
///   nix     {"outer":{"a":{"b":6,"c":5,"d":2}}}
///   sui     Error: Eval(UndefinedVar("'c'"))
/// ```
///
/// Every test written for the walker's adoption used a TOP-LEVEL binder, which
/// is exactly why none of them caught it — and almost all real nix nests. The
/// lesson is in the shape of the bug, not the fix: a traversal that filters by
/// node type while recursing can be silently non-total, because the filter
/// hides the nodes it refuses to descend through.
///
/// `descendants()` visits every node in the tree exactly once, so totality is
/// structural rather than argued.
fn record_plans(root: &ast::Root, table: &mut NormalizeTable) -> Result<(), NormalizeError> {
    for node in root.syntax().descendants() {
        // Dispatch on `kind()` — ONE enum compare — before attempting any
        // cast. `descendants()` visits every node in the file and the
        // overwhelming majority are not binders, so three speculative `cast`
        // calls per node is three kind-checks where one will do.
        if !matches!(
            node.kind(),
            rnix::SyntaxKind::NODE_ATTR_SET
                | rnix::SyntaxKind::NODE_LET_IN
                | rnix::SyntaxKind::NODE_LEGACY_LET
        ) {
            continue;
        }
        if let Some(set) = ast::AttrSet::cast(node.clone()) {
            if needs_plan(&set) {
                let plan = plan_for_entries(&set, set.rec_token().is_some())?;
                table.insert(offset_of(set.syntax()), plan);
            }
        } else if let Some(letin) = ast::LetIn::cast(node.clone()) {
            // A `let` binds recursively by construction.
            if needs_plan(&letin) {
                let plan = plan_for_entries(&letin, true)?;
                table.insert(offset_of(letin.syntax()), plan);
            }
        } else if let Some(ll) = ast::LegacyLet::cast(node) {
            // `let { … body = …; }` — the deprecated form. Also recursive, and
            // also subject to the merge rule; the tree-walker silently
            // DISCARDED every multi-segment attrpath here.
            if needs_plan(&ll) {
                let plan = plan_for_entries(&ll, true)?;
                table.insert(offset_of(ll.syntax()), plan);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> ast::Root {
        let parse = rnix::Root::parse(src);
        assert!(
            parse.errors().is_empty(),
            "{src}: rnix parse errors {:?}",
            parse.errors()
        );
        parse.tree()
    }

    fn plan(src: &str) -> Result<NormalizeTable, NormalizeError> {
        normalize(&parse(src))
    }

    /// Render the FIRST recorded group as a canonical shape string, so tests
    /// read as the tree they assert. `rec` is shown because rec-ness is the
    /// half of this rule that a value-level merge cannot express.
    fn shape(src: &str) -> String {
        let table = plan(src).unwrap_or_else(|e| panic!("{src}: rejected: {e}"));
        let mut offsets: Vec<u32> = table.by_offset.keys().copied().collect();
        offsets.sort_unstable();
        let first = offsets
            .first()
            .unwrap_or_else(|| panic!("{src}: no group was recorded"));
        render(table.get(*first).expect("recorded"))
    }

    fn render(plan: &GroupPlan) -> String {
        let mut parts: Vec<String> = plan
            .statics
            .iter()
            .map(|b| {
                let name = sui_intern::resolve(b.name);
                match &b.binding {
                    Binding::Leaf(_) => name.to_string(),
                    Binding::Inherit | Binding::InheritFrom { .. } => format!("inherit {name}"),
                    Binding::Group(sub) => format!("{name} = {}", render(sub)),
                }
            })
            .collect();
        for d in &plan.dynamics {
            let _ = &d.key;
            parts.push("${…}".to_string());
        }
        let body = parts.join("; ");
        if plan.recursive {
            format!("rec {{ {body} }}")
        } else {
            format!("{{ {body} }}")
        }
    }

    fn err(src: &str) -> NormalizeError {
        plan(src).expect_err(&format!("{src}: expected a rejection"))
    }

    // ── the MERGE rows ───────────────────────────────────────────────────

    #[test]
    fn dotted_paths_merge() {
        assert_eq!(shape("{ a.b = 1; a.c = 2; }"), "{ a = { b; c } }");
        assert_eq!(shape("{ a.b.c = 1; a.b.d = 2; }"), "{ a = { b = { c; d } } }");
    }

    /// ★ The literal and dotted forms produce the SAME tree — that is the whole
    /// reason a duplicate-key check cannot be "reject repeated keys".
    #[test]
    fn attrset_literals_merge_like_dotted_paths() {
        let dotted = shape("{ a.b = 1; a.c = 2; }");
        let literal = shape("{ a = {b=1;}; a = {c=2;}; }");
        let mixed = shape("{ a = {b=1;}; a.c = 2; }");
        assert_eq!(dotted, literal, "literal form must merge like the dotted one");
        assert_eq!(dotted, mixed, "mixed form must merge like the dotted one");
    }

    /// ★ rec-ness comes from the FIRST definition, and a later `rec` is
    /// DISCARDED. This is the row a value-level merge structurally cannot
    /// express, and it is why `{ a = {b=1;}; a = rec {c=2; d=c;}; }` is a nix
    /// parse error: the incoming block's own internal reference is destroyed.
    #[test]
    fn recness_is_taken_from_the_first_definition() {
        assert_eq!(
            shape("{ a = rec {b=1;}; a = {c=2;}; }"),
            "{ a = rec { b; c } }",
            "the FIRST definition's rec must survive"
        );
        assert_eq!(
            shape("{ a = {b=1;}; a = rec {c=2;}; }"),
            "{ a = { b; c } }",
            "a LATER rec must be discarded, not OR-ed in"
        );
    }

    /// A dotted member splices INSIDE an existing rec.
    #[test]
    fn a_dotted_member_lands_inside_the_rec() {
        assert_eq!(shape("{ a = rec {b=1;}; a.d = 3; }"), "{ a = rec { b; d } }");
    }

    /// ★ Parens are transparent RECURSIVELY. CppNix's grammar discards them;
    /// rnix keeps NODE_PAREN, so a bare `matches!(e, Expr::AttrSet(_))` misses
    /// these and turns a legal merge into a rejection.
    #[test]
    fn parentheses_are_transparent() {
        assert_eq!(shape("{ a = ({b=1;}); a = {c=2;}; }"), "{ a = { b; c } }");
        assert_eq!(shape("{ a = ((({b=1;}))); a = {c=2;}; }"), "{ a = { b; c } }");
        assert_eq!(
            shape("{ a = ((rec {b=1;})); a = {c=2;}; }"),
            "{ a = rec { b; c } }",
            "a rec inside parens must still be seen, WITH its rec-ness"
        );
    }

    /// `let` obeys the identical rule — the class of bug this crate exists for
    /// is at its worst here, because sui silently DROPS keys.
    #[test]
    fn let_bindings_merge_by_the_same_rule() {
        assert_eq!(shape("let a = {b=1;}; a = {c=2;}; in a"), "rec { a = { b; c } }");
        assert_eq!(shape("let a.b = 1; a.c = 2; in a"), "rec { a = { b; c } }");
    }

    // ── the REJECT rows ──────────────────────────────────────────────────

    #[test]
    fn a_plain_duplicate_is_rejected() {
        assert!(matches!(
            err("{ a = 1; a = 2; }"),
            NormalizeError::DuplicateAttr { ref path, .. } if path == "a"
        ));
    }

    /// ★ The check is RECURSIVE and the message names the FULL dotted path —
    /// `a.b`, not `b`.
    #[test]
    fn a_nested_conflict_names_the_full_path() {
        let NormalizeError::DuplicateAttr { path, .. } = err("{ a = {b=1;}; a = {b=2;}; }") else {
            panic!("expected DuplicateAttr");
        };
        assert_eq!(path, "a.b");

        let NormalizeError::DuplicateAttr { path, .. } = err("{ a.b.c = 1; a.b.c = 2; }") else {
            panic!("expected DuplicateAttr");
        };
        assert_eq!(path, "a.b.c");
    }

    /// ★ Decided from SYNTAX, never the value. Every wrapper except parens is
    /// opaque, even when it plainly evaluates to an attrset.
    #[test]
    fn non_literal_wrappers_are_opaque() {
        for src in [
            "{ a = if true then {b=1;} else {}; a = {c=2;}; }",
            "{ a = let x = 1; in {b=x;}; a = {c=2;}; }",
            "{ a = with {}; {b=1;}; a = {c=2;}; }",
            "{ a = assert true; {b=1;}; a = {c=2;}; }",
            "{ a = {b=1;} // {z=9;}; a = {c=2;}; }",
        ] {
            assert!(
                plan(src).is_err(),
                "{src}: the value is an attrset but the SYNTAX is not — must reject"
            );
        }
    }

    /// A path descending THROUGH a non-mergeable binding rejects at the
    /// shallowest level where a side is not a literal.
    #[test]
    fn descending_through_a_leaf_is_rejected() {
        let NormalizeError::DuplicateAttr { path, .. } = err("{ a = 1; a.b = 2; }") else {
            panic!("expected DuplicateAttr");
        };
        assert_eq!(path, "a");
    }

    /// An inherited binding never merges, in either order.
    #[test]
    fn inherit_never_merges() {
        assert!(plan("let x = 1; in { inherit x; x = 2; }").is_err());
        assert!(plan("let x = 1; in { x = 2; inherit x; }").is_err());
        assert!(plan("{ inherit (s) a; a = 1; }").is_err());
    }

    /// ★ One `inherit (e) a b c;` registers its source EXACTLY ONCE, and all
    /// three names index it. A source cloned per name is evaluated per name —
    /// observable through `builtins.trace`, and through any impure source.
    #[test]
    fn an_inherit_from_clause_shares_one_source() {
        let table = plan("{ inherit (src) a b c; d.e = 1; }").expect("ok");
        let mut offs: Vec<u32> = table.by_offset.keys().copied().collect();
        offs.sort_unstable();
        let g = table.get(offs[0]).expect("recorded");
        assert_eq!(
            g.inherit_froms.len(),
            1,
            "one clause must yield one arena entry, got {}",
            g.inherit_froms.len()
        );
        let idxs: Vec<usize> = g
            .statics
            .iter()
            .filter_map(|b| match b.binding {
                Binding::InheritFrom { from } => Some(from),
                _ => None,
            })
            .collect();
        assert_eq!(idxs, vec![0, 0, 0], "all three names must share index 0");
    }

    /// And when a group carrying an `inherit (e)` is SPLICED into another that
    /// already has one, the incoming indices must be REBASED onto the
    /// survivor's arena. Without the rebase a spliced inherit silently points
    /// at the wrong source expression — a wrong value, not an error.
    #[test]
    fn a_spliced_inherit_from_is_rebased_onto_the_survivor() {
        let table = plan("{ a = { inherit (p) x; }; a = { inherit (q) y; }; }").expect("ok");
        let mut offs: Vec<u32> = table.by_offset.keys().copied().collect();
        offs.sort_unstable();
        let outer = table.get(offs[0]).expect("recorded");
        let Binding::Group(merged) = &outer.statics[0].binding else {
            panic!("expected a merged group");
        };
        assert_eq!(merged.inherit_froms.len(), 2, "both clauses must survive");
        let idxs: Vec<usize> = merged
            .statics
            .iter()
            .filter_map(|b| match b.binding {
                Binding::InheritFrom { from } => Some(from),
                _ => None,
            })
            .collect();
        assert_eq!(
            idxs,
            vec![0, 1],
            "the spliced clause must point at its OWN source, not the survivor's"
        );
    }

    // ── the constant-fold boundary ───────────────────────────────────────

    /// ★ Static/dynamic is a fold on NODE KIND, not quoted-vs-unquoted.
    /// Getting this wrong in the permissive direction turns a
    /// currently-CORRECT sui answer into a throw.
    #[test]
    fn static_dynamic_split_is_a_constant_fold() {
        // Folds to static -> participates in the merge, and can reject.
        assert!(plan(r#"{ "a" = 1; a = 2; }"#).is_err(), "a quoted key IS the same key");
        assert!(
            plan(r#"{ ${"a"} = 1; a = 2; }"#).is_err(),
            "a bare interpolation of a pure string literal folds to a static key"
        );
        assert_eq!(
            shape(r#"{ ${"a"}.b = 1; a.c = 2; }"#),
            "{ a = { b; c } }",
            "a folded bare interpolation must MERGE with the plain ident"
        );

        // Stays dynamic -> deferred to force time, never a parse rejection.
        assert!(
            plan(r#"{ "${"a"}" = 1; a = 2; }"#).is_ok(),
            "an INTERPOLATED string key stays dynamic and must not reject at parse"
        );
        assert!(
            plan(r#"{ ${"a"+""} = 1; a = 2; }"#).is_ok(),
            "a non-Str dynamic key stays dynamic"
        );
    }

    /// ★ An interpolated key must stay in the PATH, not vanish from it.
    ///
    /// `fold_attr` returning `None` made the caller skip the component, so
    /// `a."${"b"}" = 1; a."${"c"}" = 2;` collapsed to two bindings of plain
    /// `a` and was rejected as a duplicate — on input nix ACCEPTS. A false
    /// reject is the dangerous direction: it refuses working code. Both of
    /// these are real sui corpus fixtures that a fleet scan caught.
    #[test]
    fn an_interpolated_key_stays_in_the_path() {
        for src in [
            r#"{ a."${"b"}" = true; a."${"c"}" = false; }"#,
            r#"{ set3.a = 1; set3."${"b" + ""}" = 2; }"#,
            // The trailing-dynamic case at depth.
            r#"{ x.y."${"z"}" = 1; x.y."${"w"}" = 2; }"#,
        ] {
            assert!(
                plan(src).is_ok(),
                "{src}: nix accepts this; rejecting it refuses working code"
            );
        }
    }

    /// And the component must be preserved as DYNAMIC, not silently folded to
    /// some static name — otherwise two different interpolations would
    /// collide with each other.
    #[test]
    fn two_different_interpolated_keys_do_not_collide() {
        let table = plan(r#"{ a."${"b"}" = 1; a."${"c"}" = 2; }"#).expect("ok");
        let mut offs: Vec<u32> = table.by_offset.keys().copied().collect();
        offs.sort_unstable();
        let outer = table.get(offs[0]).expect("recorded");
        let Binding::Group(inner) = &outer.statics[0].binding else {
            panic!("expected `a` to be an implicit group");
        };
        assert_eq!(
            inner.dynamics.len(),
            2,
            "both interpolated keys must survive as dynamics, got {}",
            inner.dynamics.len()
        );
    }

    /// ★ An INTERMEDIATE dynamic component must keep the rest of its path.
    ///
    /// `let k = "z"; in { a.${k}.b = 1; }` is `{ a = { z = { b = 1; }; }; }`
    /// in nix. An earlier cut returned without recording anything for an
    /// intermediate dynamic, silently building `{ a = { }; }` — the very
    /// failure class this crate removes, reintroduced by its own fix, on a
    /// shape the PRE-EXISTING walker got right.
    #[test]
    fn an_intermediate_dynamic_keeps_the_rest_of_its_path() {
        let table = plan(r#"{ a.${k}.b = 1; }"#).expect("ok");
        let mut offs: Vec<u32> = table.by_offset.keys().copied().collect();
        offs.sort_unstable();
        let outer = table.get(offs[0]).expect("recorded");
        let Binding::Group(a) = &outer.statics[0].binding else {
            panic!("expected `a` to be an implicit group");
        };
        assert_eq!(a.dynamics.len(), 1, "the dynamic component must be recorded");
        let Binding::Group(inner) = &a.dynamics[0].value else {
            panic!("an intermediate dynamic must carry a GROUP, not a leaf — \
                   otherwise the `.b` tail is dropped");
        };
        assert_eq!(
            inner.statics.len(),
            1,
            "the `.b` tail must live inside the dynamic's group"
        );
    }

    /// A quoted dotted string is ONE key, not a path.
    #[test]
    fn a_quoted_dotted_string_is_a_single_key() {
        assert!(
            plan(r#"{ "a.b" = 1; a.b = 2; }"#).is_ok(),
            r#""a.b" and a.b are DIFFERENT keys"#
        );
        assert!(plan(r#"{ "a.b" = 1; "a.b" = 2; }"#).is_err());
    }

    // ── error rendering ──────────────────────────────────────────────────

    /// `showAttrPath` quotes iff the component is not an identifier or IS one
    /// of exactly nine reserved words. Note `or`/`true`/`false`/`null` are NOT
    /// reserved in this position — measured, not assumed.
    #[test]
    fn attr_path_components_quote_like_cppnix() {
        for bare in ["a", "a1", "_a", "a-b", "a'b", "or", "true", "false", "null"] {
            assert_eq!(show_attr_component(bare), bare, "{bare} must render bare");
        }
        for (raw, want) in [
            ("1a", "\"1a\""),
            ("a b", "\"a b\""),
            ("a.b", "\"a.b\""),
            ("", "\"\""),
            ("if", "\"if\""),
            ("rec", "\"rec\""),
            ("inherit", "\"inherit\""),
        ] {
            assert_eq!(show_attr_component(raw), want, "{raw} must render quoted");
        }
    }

    // ── the table stays small ────────────────────────────────────────────

    /// ★ ANTI-VACUITY + the parity argument in one row. A group with no
    /// duplicate and no dotted path must NOT be recorded, or this pass would
    /// take over every attrset in the fleet instead of only the broken ones.
    /// The second half is the anti-vacuity check: a group that DOES need a
    /// plan must actually be recorded, or "records nothing" would pass
    /// trivially.
    #[test]
    fn only_groups_that_need_normalizing_are_recorded() {
        for clean in [
            "{ a = 1; b = 2; }",
            "{ }",
            "let a = 1; b = 2; in a",
            "rec { a = 1; b = a; }",
            "{ a = { b = 1; }; }",
        ] {
            assert!(
                plan(clean).expect("clean").is_empty(),
                "{clean}: nothing to normalize, so nothing may be recorded"
            );
        }
        for dirty in ["{ a.b = 1; }", "{ a.b = 1; a.c = 2; }", "let a.b = 1; in a"] {
            assert!(
                !plan(dirty).expect("dirty").is_empty(),
                "{dirty}: needs a plan, so one must be recorded"
            );
        }
    }
}
