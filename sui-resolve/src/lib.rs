//! Parse-time variable resolution side-table (ENV-RESOLVE M0).
//!
//! A single `bind_vars` pass over the rnix AST — walked against a
//! cppnix-shaped [`StaticEnv`] chain — precomputes, for each variable
//! *reference* (`ast::Ident` used as an expression), whether that name is
//! **provably resolvable in an enclosing lexical binder with no `with`
//! scope between the reference and its binder**. If so, the reference's
//! interned [`sui_intern::Symbol`] is recorded in a [`ResolveTable`] keyed
//! by the ident node's byte offset. At eval time, the tree-walker's hot
//! `Ident` arm consults the table: on a [`Resolution::Lexical`] hit it
//! probes the environment's lexical bindings with the precomputed Symbol
//! directly, skipping the per-lookup `ident_text().to_string()` +
//! `intern()` round-trip.
//!
//! # Parity by construction (M0)
//!
//! This is a **pure hash-key-caching optimization**. The tree-walker's
//! `Env::lookup_fast` already probes the lexical `im_rc` bindings map FIRST
//! (by the same interned Symbol) before ever touching the `with`-chain. So
//! a `Lexical` fast path that only shortcuts on a *lexical-bindings hit*
//! returns exactly the value the unchanged path would have — the same
//! `Env`, the same `Symbol`, the same map probe, just pre-interned. On ANY
//! miss (a mid-fixpoint blackhole, an ident the table doesn't record, or
//! `Resolution::Dynamic`) the consumer falls back to today's exact runtime
//! path (`intern` + `lookup_fast` + the `with`-chain). A byte-identical
//! parity result is therefore a *sufficient* proof for M0.
//!
//! # Fail-safe to Dynamic (the load-bearing discipline)
//!
//! `bind_vars` marks a reference [`Resolution::Lexical`] **only** when it
//! is provably resolvable in an enclosing `let`/`rec`/lambda-param/pattern
//! binder with **no `with` scope between** the reference and its binder. On
//! any uncertainty — a reference resolved while a `with`-barrier frame sits
//! between it and its nearest matching lexical binder, an unresolved name,
//! an rnix node shape it doesn't model, or a binder whose name it can't
//! statically read — it emits [`Resolution::Dynamic`] (or simply records
//! nothing, which the consumer reads back as `Dynamic`). Slower-but-correct
//! is always right; a mis-marked `Lexical` under a `with` would be the only
//! thing that could change scoping, and this pass structurally cannot emit
//! one.
//!
//! M1+ (positional `{up, slot}` frames, VM sharing) is explicitly out of
//! scope here — see `docs/ENV-RESOLVE-DESIGN.md`.

use rnix::ast::{self, AstToken, HasEntry};
use rowan::ast::AstNode;

use sui_intern::Symbol;

/// The resolution verdict for one variable reference.
///
/// M0 only needs to distinguish a precomputed-`Symbol` lexical hit
/// candidate from "route through the runtime `with`/probe path". The
/// positional `{up, slot}` variant is M1, deliberately absent here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Provably resolvable in an enclosing lexical binder with no `with`
    /// scope between the reference and its binder. Carries the reference's
    /// pre-interned [`Symbol`] so the eval hot path can probe the lexical
    /// bindings map directly (skipping the re-intern). A lexical-bindings
    /// *miss* at runtime still falls back to the full unchanged path.
    Lexical {
        /// The interned name of the reference.
        sym: Symbol,
    },
    /// Route this reference through today's unchanged runtime path
    /// (`intern` + `lookup_fast` + the `with`-chain). The fail-safe default
    /// for any uncertainty.
    Dynamic,
}

/// A parse-time resolution side-table for one parsed source tree.
///
/// Keyed by the reference ident node's byte offset (`text_range().start()`)
/// within *this* parse — mirroring how `sui-eval`'s `intern_cached` keys its
/// per-`(source_id, offset)` ident cache (`value.rs`). The consumer stashes
/// one table per parsed source (the `source_id` disambiguation lives on the
/// consumer side, exactly as it does for the ident cache). A lookup for an
/// unrecorded offset returns [`Resolution::Dynamic`] — the fail-safe path.
#[derive(Clone, Debug, Default)]
pub struct ResolveTable {
    /// `text_offset` -> resolution. Only `Lexical` entries are stored; an
    /// absent offset reads back as `Dynamic`, so the map stays small (it
    /// records only the shortcut-eligible references).
    by_offset: rustc_hash::FxHashMap<u32, Resolution>,
}

impl ResolveTable {
    /// An empty table — every lookup returns [`Resolution::Dynamic`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_offset: rustc_hash::FxHashMap::default(),
        }
    }

    /// Resolution recorded for a reference at `text_offset`. An unrecorded
    /// offset is [`Resolution::Dynamic`] (the fail-safe fallback).
    #[must_use]
    pub fn get(&self, text_offset: u32) -> Resolution {
        self.by_offset
            .get(&text_offset)
            .copied()
            .unwrap_or(Resolution::Dynamic)
    }

    /// Iterate the recorded `(text_offset, resolution)` entries. Only
    /// `Lexical` entries are stored, so this yields exactly the
    /// shortcut-eligible references. Consumers merge these into their own
    /// per-`(source_id, offset)` table.
    pub fn entries(&self) -> impl Iterator<Item = (u32, Resolution)> + '_ {
        self.by_offset.iter().map(|(&off, &res)| (off, res))
    }

    /// Number of `Lexical` entries recorded (diagnostics / tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_offset.len()
    }

    /// Whether no `Lexical` entry was recorded (diagnostics / tests).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_offset.is_empty()
    }

    fn record_lexical(&mut self, text_offset: u32, sym: Symbol) {
        self.by_offset
            .insert(text_offset, Resolution::Lexical { sym });
    }
}

/// One frame in the static scope chain built during `bind_vars`.
///
/// A frame is either a *binder* frame (a `let`/`rec`/lambda-param/pattern
/// scope, carrying the set of statically-known names it introduces) or a
/// `with`-**barrier** frame. A reference is `Lexical` iff, scanning frames
/// innermost-first, we reach a binder frame that contains the name before
/// crossing any `with`-barrier.
enum Frame {
    /// A lexical binder scope. Holds the interned Symbols of every name it
    /// statically introduces. Only names we can read statically (`Ident`
    /// binders) are recorded; a name we cannot read (a dynamic/`${…}` key)
    /// is simply *not present*, so a reference to it fails to resolve and
    /// falls to `Dynamic` — the safe direction.
    Binder(Vec<Symbol>),
    /// A `with` scope barrier. Any reference resolved while this sits
    /// between it and a matching binder is `Dynamic` (the runtime
    /// `with`-chain owns it).
    WithBarrier,
}

/// The static scope chain — innermost frame last.
#[derive(Default)]
struct StaticEnv {
    frames: Vec<Frame>,
}

impl StaticEnv {
    fn push_binder(&mut self, names: Vec<Symbol>) {
        self.frames.push(Frame::Binder(names));
    }

    fn push_with(&mut self) {
        self.frames.push(Frame::WithBarrier);
    }

    fn pop(&mut self) {
        self.frames.pop();
    }

    /// Resolve `sym` against the chain, innermost-first.
    ///
    /// Returns `true` (a `Lexical` hit) iff a binder frame containing `sym`
    /// is reached with **no `with`-barrier crossed first**. A `with`-barrier
    /// encountered before any matching binder makes the reference `Dynamic`:
    /// the runtime `with`-chain could supply the name, and any lexical
    /// binder *outside* the barrier would (correctly) still win at runtime —
    /// but we must not shortcut it, because if the name is NOT lexically
    /// bound outside, the `with` must handle it. Conservatively: the first
    /// barrier we cross ends the `Lexical` guarantee.
    fn resolves_lexically(&self, sym: Symbol) -> bool {
        for frame in self.frames.iter().rev() {
            match frame {
                Frame::Binder(names) => {
                    if names.contains(&sym) {
                        return true;
                    }
                }
                Frame::WithBarrier => return false,
            }
        }
        false
    }
}

/// Run the parse-time resolver over a parsed source tree.
///
/// Walks the AST rooted at `root`, threading a [`StaticEnv`] of lexical
/// binder frames and `with`-barriers, and records a [`Resolution::Lexical`]
/// (with its precomputed [`Symbol`]) for every variable reference provably
/// resolvable in an enclosing binder with no `with` between. Every other
/// reference is left unrecorded (read back as [`Resolution::Dynamic`]).
///
/// Pure and side-effect-free apart from interning names into the
/// thread-local interner (the same interner the tree-walker uses, so a
/// recorded Symbol is the exact one `lookup_fast` would probe with).
#[must_use]
pub fn resolve(root: &ast::Root) -> ResolveTable {
    let mut table = ResolveTable::new();
    let mut env = StaticEnv::default();
    if let Some(expr) = root.expr() {
        walk_expr(&expr, &mut env, &mut table);
    }
    table
}

/// Intern an attr's static name, if it has one. `Ident` and a plain
/// (non-interpolated) `Str` literal have a statically-known name; a
/// dynamic `${…}` key does not (returns `None`, so the binder is simply
/// omitted — the safe direction).
fn attr_static_sym(attr: &ast::Attr) -> Option<Symbol> {
    match attr {
        ast::Attr::Ident(ident) => Some(intern_ident(ident)),
        ast::Attr::Str(s) => {
            // A string key with any interpolation part is not statically
            // nameable. `parts()` yields Literal parts and Interpolation
            // parts; only an all-literal string has a fixed name.
            let mut text = String::new();
            for part in s.parts() {
                match part {
                    ast::InterpolPart::Literal(lit) => text.push_str(lit.syntax().text()),
                    ast::InterpolPart::Interpolation(_) => return None,
                }
            }
            Some(sui_intern::intern(&text))
        }
        ast::Attr::Dynamic(_) => None,
    }
}

/// Intern an `Ident`'s text using the thread-local interner.
fn intern_ident(ident: &ast::Ident) -> Symbol {
    sui_intern::intern(&ident.syntax().text().to_string())
}

/// Byte offset of an ident reference node — the same key `intern_cached`
/// uses as the low 32 bits of its `(source_id, offset)` cache key.
fn ident_offset(ident: &ast::Ident) -> u32 {
    u32::from(ident.syntax().text_range().start())
}

/// The statically-known binder names introduced by a `let …`/`rec { … }`
/// entry list (an [`ast::HasEntry`]). Records single-segment `Ident`/`Str`
/// heads of attrpath bindings + `inherit` names. A dynamic head or a
/// dotted-path head we can't read is simply omitted (safe direction).
fn entry_binder_syms<E: HasEntry>(entries: &E) -> Vec<Symbol> {
    let mut syms = Vec::new();
    for entry in entries.entries() {
        match entry {
            ast::Entry::AttrpathValue(apv) => {
                if let Some(attrpath) = apv.attrpath() {
                    // The BINDER name is the HEAD segment of the attrpath
                    // (`a.b = …` binds `a` in the lexical scope). Record only
                    // the head; a dotted path desugars to a nested set, and
                    // the head is what a bare reference could resolve to.
                    if let Some(head) = attrpath.attrs().next() {
                        if let Some(sym) = attr_static_sym(&head) {
                            syms.push(sym);
                        }
                    }
                }
            }
            ast::Entry::Inherit(inherit) => {
                for attr in inherit.attrs() {
                    if let Some(sym) = attr_static_sym(&attr) {
                        syms.push(sym);
                    }
                }
            }
        }
    }
    syms
}

/// Walk the *value* expressions of a `let`/`rec` entry list under the given
/// static env (which must already have the binder frame pushed — `let`/`rec`
/// scopes are recursive, so RHS values see their own binders). Also walks
/// any `inherit (from) …` source expressions, which are evaluated in the
/// *outer* scope — but since the binder frame is already pushed and Nix's
/// `inherit (e) a;` evaluates `e` in the enclosing scope, we walk `from`
/// with the binder frame present too; this is conservative-safe (it can
/// only ADD names in scope for `from`, never remove — and a mis-`Lexical`
/// is still parity-safe per the module docs). For correctness-of-win we
/// keep it simple and uniform.
fn walk_entries<E: HasEntry>(entries: &E, env: &mut StaticEnv, table: &mut ResolveTable) {
    for entry in entries.entries() {
        match entry {
            ast::Entry::AttrpathValue(apv) => {
                // Dynamic `${…}` segments in the attrpath are themselves
                // expressions to walk.
                if let Some(attrpath) = apv.attrpath() {
                    walk_attrpath(&attrpath, env, table);
                }
                if let Some(value) = apv.value() {
                    walk_expr(&value, env, table);
                }
            }
            ast::Entry::Inherit(inherit) => {
                if let Some(from) = inherit.from() {
                    if let Some(expr) = from.expr() {
                        walk_expr(&expr, env, table);
                    }
                }
                // `inherit a b;` (no `from`) copies enclosing-scope names —
                // the names themselves are static Attrs, not references we
                // resolve here; nothing to walk.
            }
        }
    }
}

/// Walk any dynamic (`${…}`) segments of an attrpath — a static `Ident`/
/// `Str` segment is a name, not a reference.
fn walk_attrpath(attrpath: &ast::Attrpath, env: &mut StaticEnv, table: &mut ResolveTable) {
    for attr in attrpath.attrs() {
        if let ast::Attr::Dynamic(dynamic) = attr {
            if let Some(expr) = dynamic.expr() {
                walk_expr(&expr, env, table);
            }
        }
    }
}

/// The core recursive walk. Threads the static-env chain and records
/// `Lexical` resolutions for bare variable references.
fn walk_expr(expr: &ast::Expr, env: &mut StaticEnv, table: &mut ResolveTable) {
    match expr {
        // ── The reference site ───────────────────────────────────────────
        ast::Expr::Ident(ident) => {
            let name = ident.syntax().text().to_string();
            // Nix keywords are never lexical variables. They are handled by
            // the eval Ident arm BEFORE any lookup, and are never bound —
            // never mark them, so the keyword fast path stays intact.
            if matches!(name.as_str(), "true" | "false" | "null") {
                return;
            }
            let sym = sui_intern::intern(&name);
            if env.resolves_lexically(sym) {
                table.record_lexical(ident_offset(ident), sym);
            }
            // else: leave unrecorded → Dynamic (fail-safe).
        }

        // ── Binder scopes ────────────────────────────────────────────────
        ast::Expr::LetIn(letin) => {
            let names = entry_binder_syms(letin);
            env.push_binder(names);
            walk_entries(letin, env, table);
            if let Some(body) = letin.body() {
                walk_expr(&body, env, table);
            }
            env.pop();
        }
        ast::Expr::LegacyLet(legacy) => {
            // `let { …; body = …; }` — a recursive binder scope whose result
            // is its own `body` attr. Treated exactly like a rec-attrset
            // binder scope for name purposes.
            let names = entry_binder_syms(legacy);
            env.push_binder(names);
            walk_entries(legacy, env, table);
            env.pop();
        }
        ast::Expr::AttrSet(set) => {
            if set.rec_token().is_some() {
                // `rec { … }` — a recursive binder scope.
                let names = entry_binder_syms(set);
                env.push_binder(names);
                walk_entries(set, env, table);
                env.pop();
            } else {
                // A plain attrset introduces NO lexical binders (its keys are
                // not in scope for its own values). Walk values in the
                // current env; walk dynamic keys too.
                for entry in set.entries() {
                    match entry {
                        ast::Entry::AttrpathValue(apv) => {
                            if let Some(attrpath) = apv.attrpath() {
                                walk_attrpath(&attrpath, env, table);
                            }
                            if let Some(value) = apv.value() {
                                walk_expr(&value, env, table);
                            }
                        }
                        ast::Entry::Inherit(inherit) => {
                            if let Some(from) = inherit.from() {
                                if let Some(inner) = from.expr() {
                                    walk_expr(&inner, env, table);
                                }
                            }
                        }
                    }
                }
            }
        }
        ast::Expr::Lambda(lambda) => {
            // A lambda parameter introduces a binder scope for the body.
            let names = param_binder_syms(lambda.param().as_ref(), env, table);
            env.push_binder(names);
            if let Some(body) = lambda.body() {
                walk_expr(&body, env, table);
            }
            env.pop();
        }

        // ── The `with` barrier ───────────────────────────────────────────
        ast::Expr::With(with) => {
            // The NAMESPACE is evaluated in the OUTER scope (no barrier yet).
            if let Some(ns) = with.namespace() {
                walk_expr(&ns, env, table);
            }
            // The BODY sees a `with`-barrier: any name not lexically bound
            // OUTSIDE this `with` must route through the runtime with-chain.
            env.push_with();
            if let Some(body) = with.body() {
                walk_expr(&body, env, table);
            }
            env.pop();
        }

        // ── Structural nodes — walk children, no scope change ────────────
        ast::Expr::Apply(apply) => {
            if let Some(f) = apply.lambda() {
                walk_expr(&f, env, table);
            }
            if let Some(arg) = apply.argument() {
                walk_expr(&arg, env, table);
            }
        }
        ast::Expr::Assert(assert) => {
            if let Some(c) = assert.condition() {
                walk_expr(&c, env, table);
            }
            if let Some(b) = assert.body() {
                walk_expr(&b, env, table);
            }
        }
        ast::Expr::IfElse(ie) => {
            if let Some(c) = ie.condition() {
                walk_expr(&c, env, table);
            }
            if let Some(b) = ie.body() {
                walk_expr(&b, env, table);
            }
            if let Some(e) = ie.else_body() {
                walk_expr(&e, env, table);
            }
        }
        ast::Expr::BinOp(binop) => {
            if let Some(l) = binop.lhs() {
                walk_expr(&l, env, table);
            }
            if let Some(r) = binop.rhs() {
                walk_expr(&r, env, table);
            }
        }
        ast::Expr::UnaryOp(unary) => {
            if let Some(e) = unary.expr() {
                walk_expr(&e, env, table);
            }
        }
        ast::Expr::Paren(paren) => {
            if let Some(e) = paren.expr() {
                walk_expr(&e, env, table);
            }
        }
        ast::Expr::Root(root) => {
            if let Some(e) = root.expr() {
                walk_expr(&e, env, table);
            }
        }
        ast::Expr::List(list) => {
            for item in list.items() {
                walk_expr(&item, env, table);
            }
        }
        ast::Expr::Select(select) => {
            // `e.a.b` — walk the BASE expr. The attrpath segments are names
            // (static) or dynamic `${…}` exprs (walk those).
            if let Some(base) = select.expr() {
                walk_expr(&base, env, table);
            }
            if let Some(attrpath) = select.attrpath() {
                walk_attrpath(&attrpath, env, table);
            }
            // `or default` fallthrough expr.
            if let Some(default) = select.default_expr() {
                walk_expr(&default, env, table);
            }
        }
        ast::Expr::HasAttr(has) => {
            if let Some(base) = has.expr() {
                walk_expr(&base, env, table);
            }
            if let Some(attrpath) = has.attrpath() {
                walk_attrpath(&attrpath, env, table);
            }
        }
        ast::Expr::Str(s) => {
            // Walk interpolation parts (`"${e}"`).
            for part in s.parts() {
                if let ast::InterpolPart::Interpolation(interp) = part {
                    if let Some(e) = interp.expr() {
                        walk_expr(&e, env, table);
                    }
                }
            }
        }

        // Path literals may be interpolated (`./${e}`, `~/${e}`, `<${e}>`).
        ast::Expr::PathAbs(_)
        | ast::Expr::PathRel(_)
        | ast::Expr::PathHome(_)
        | ast::Expr::PathSearch(_) => {
            walk_path_interpolations(expr, env, table);
        }

        // Leaves / nodes with no scope-relevant children.
        ast::Expr::Literal(_) | ast::Expr::Error(_) | ast::Expr::CurPos(_) => {}

        // Any node shape not modeled above: DO NOT descend blindly with a
        // handwritten child walk (an unmodeled binder would be missed and
        // could mis-`Lexical`). Fall through to a generic *reference-safe*
        // descent that walks every child expression WITHOUT introducing or
        // removing any scope — but because we can't know if an unmodeled
        // node is a binder, the safe move is to walk children in the CURRENT
        // env. rnix 0.14's `Expr` enum is closed and fully handled above, so
        // this arm is unreachable in practice; kept for forward-compat: it
        // never records a `Lexical` it shouldn't because resolution only
        // fires on the `Ident` arm against the *current* (unchanged) env.
        #[allow(unreachable_patterns)]
        _ => {
            for child in expr.syntax().children() {
                if let Some(child_expr) = ast::Expr::cast(child) {
                    walk_expr(&child_expr, env, table);
                }
            }
        }
    }
}

/// Walk `${…}` interpolation expressions inside a path literal. rnix
/// models interpolated path parts as child `Interpol`/`Dynamic`-ish nodes;
/// we generically descend into any child `Expr` (the static path text
/// segments are tokens, not `Expr` children, so this only picks up the
/// interpolated sub-expressions).
fn walk_path_interpolations(expr: &ast::Expr, env: &mut StaticEnv, table: &mut ResolveTable) {
    for descendant in expr.syntax().descendants() {
        // An interpolation's inner expression is the direct `Expr` child of
        // an `Interpol` node. Find those and walk them.
        if ast::Interpol::can_cast(descendant.kind()) {
            if let Some(interp) = ast::Interpol::cast(descendant) {
                if let Some(inner) = interp.expr() {
                    walk_expr(&inner, env, table);
                }
            }
        }
    }
}

/// The binder names a lambda parameter introduces, AND walk any default
/// expressions in a pattern (which are evaluated with the parameter's own
/// bindings in scope — Nix pattern defaults are recursive over the pattern).
///
/// - `x: body` (an `IdentParam`) binds `x`.
/// - `{ a, b ? d, ... } @ args: body` (a `Pattern`) binds `a`, `b`, and the
///   `@`-bound `args`; each `? default` is walked with the pattern's binder
///   frame in scope (Nix evaluates a formal's default in the function's own
///   argument scope).
fn param_binder_syms(
    param: Option<&ast::Param>,
    env: &mut StaticEnv,
    table: &mut ResolveTable,
) -> Vec<Symbol> {
    let mut syms = Vec::new();
    let Some(param) = param else {
        return syms;
    };
    match param {
        ast::Param::IdentParam(ip) => {
            if let Some(ident) = ip.ident() {
                syms.push(intern_ident(&ident));
            }
        }
        ast::Param::Pattern(pattern) => {
            // Collect every formal name + the `@`-bound name first.
            for pat_entry in pattern.pat_entries() {
                if let Some(ident) = pat_entry.ident() {
                    syms.push(intern_ident(&ident));
                }
            }
            if let Some(pat_bind) = pattern.pat_bind() {
                if let Some(ident) = pat_bind.ident() {
                    syms.push(intern_ident(&ident));
                }
            }
            // Now walk each `? default` with the pattern's binder frame in
            // scope (defaults are recursive over the pattern in Nix, e.g.
            // `{ a, b ? a }`). Push the frame, walk defaults, pop — the
            // caller re-pushes the SAME names for the body.
            env.push_binder(syms.clone());
            for pat_entry in pattern.pat_entries() {
                if let Some(default) = pat_entry.default() {
                    walk_expr(&default, env, table);
                }
            }
            env.pop();
        }
    }
    syms
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse + resolve, returning the table.
    fn resolve_str(src: &str) -> ResolveTable {
        let parse = rnix::Root::parse(src);
        assert!(parse.errors().is_empty(), "parse errors: {:?}", parse.errors());
        resolve(&parse.tree())
    }

    /// Find the byte offset of the `nth` bare `Ident` *reference* whose text
    /// equals `name` and that is used as an expression (NOT an attr key /
    /// binder). A binder ident's parent is an attrpath / inherit / param /
    /// pattern-entry / pattern-bind node; every other `Ident` node is a
    /// reference. Offsets are returned in source order.
    fn ref_offset(src: &str, name: &str, nth: usize) -> u32 {
        use rnix::SyntaxKind;
        let parse = rnix::Root::parse(src);
        let mut hits = Vec::new();
        for node in parse.syntax().descendants() {
            if let Some(ast::Expr::Ident(ident)) = ast::Expr::cast(node.clone()) {
                if ident.syntax().text() != name {
                    continue;
                }
                // Exclude binder positions (attr keys, params, pattern names).
                // In a `NODE_PAT_ENTRY` (`b ? default`) BOTH the formal name
                // `b` AND the default expr `a` are direct Ident children, so
                // only the FIRST ident of a PatEntry is the binder; a later
                // ident there (the default) is a reference.
                let parent = ident.syntax().parent();
                let is_binder = parent.as_ref().is_some_and(|p| match p.kind() {
                    SyntaxKind::NODE_ATTRPATH
                    | SyntaxKind::NODE_INHERIT
                    | SyntaxKind::NODE_IDENT_PARAM
                    | SyntaxKind::NODE_PAT_BIND => true,
                    SyntaxKind::NODE_PAT_ENTRY => {
                        // Binder iff it is the entry's first Ident child.
                        p.children()
                            .find(|c| c.kind() == SyntaxKind::NODE_IDENT)
                            .is_some_and(|first| first == *ident.syntax())
                    }
                    _ => false,
                });
                if !is_binder {
                    hits.push(u32::from(ident.syntax().text_range().start()));
                }
            }
        }
        hits.sort_unstable();
        hits[nth]
    }

    fn is_lexical(table: &ResolveTable, offset: u32) -> bool {
        matches!(table.get(offset), Resolution::Lexical { .. })
    }

    #[test]
    fn let_body_reference_is_lexical() {
        let src = "let x = 1; in x";
        let t = resolve_str(src);
        let off = ref_offset(src, "x", 0); // the body `x`
        assert!(is_lexical(&t, off), "body `x` should resolve lexically");
    }

    #[test]
    fn recursive_let_sibling_reference_is_lexical() {
        // `b`'s RHS references `a` — a sibling let binder; lexical.
        let src = "let a = 1; b = a + 1; in b";
        let t = resolve_str(src);
        let off = ref_offset(src, "a", 0); // the reference in `b = a + 1`
        assert!(is_lexical(&t, off), "sibling `a` should resolve lexically");
    }

    #[test]
    fn free_variable_is_dynamic() {
        let src = "let x = 1; in y";
        let t = resolve_str(src);
        let off = ref_offset(src, "y", 0);
        assert_eq!(t.get(off), Resolution::Dynamic, "free `y` must be Dynamic");
    }

    #[test]
    fn reference_under_with_is_dynamic() {
        // `x` is NOT lexically bound; it can only come from the `with`.
        let src = "with pkgs; x";
        let t = resolve_str(src);
        let off = ref_offset(src, "x", 0);
        assert_eq!(
            t.get(off),
            Resolution::Dynamic,
            "a name only a `with` could provide must be Dynamic"
        );
    }

    #[test]
    fn lexical_binder_outside_with_is_still_conservatively_dynamic() {
        // `x` IS lexically bound OUTSIDE the `with`, but a `with`-barrier
        // sits between the reference and the binder. The fail-safe rule
        // makes this Dynamic (the runtime path resolves it correctly — the
        // lexical binding wins there because lookup_fast probes bindings
        // first — so correctness is preserved; we just don't shortcut it).
        let src = "let x = 1; in with pkgs; x";
        let t = resolve_str(src);
        // Only one `x` REFERENCE exists (the body of the `with`); the other
        // `x` is the let-binder KEY, which `ref_offset` excludes.
        let off = ref_offset(src, "x", 0);
        assert_eq!(
            t.get(off),
            Resolution::Dynamic,
            "a reference under a with-barrier must be Dynamic (fail-safe)"
        );
    }

    #[test]
    fn with_namespace_reference_is_resolved_in_outer_scope() {
        // `pkgs` in `with pkgs; …` is evaluated in the OUTER scope; if it is
        // lexically bound there it is Lexical (no barrier over the namespace).
        let src = "let pkgs = {}; in with pkgs; 1";
        let t = resolve_str(src);
        let off = ref_offset(src, "pkgs", 0); // the namespace reference
        assert!(
            is_lexical(&t, off),
            "the with-namespace resolves in the outer (barrier-free) scope"
        );
    }

    #[test]
    fn lambda_param_reference_is_lexical() {
        let src = "x: x + 1";
        let t = resolve_str(src);
        let off = ref_offset(src, "x", 0); // body `x`
        assert!(is_lexical(&t, off), "lambda param `x` should be lexical");
    }

    #[test]
    fn pattern_formal_reference_is_lexical() {
        let src = "{ a, b }: a + b";
        let t = resolve_str(src);
        assert!(is_lexical(&t, ref_offset(src, "a", 0)));
        assert!(is_lexical(&t, ref_offset(src, "b", 0)));
    }

    #[test]
    fn pattern_default_can_reference_sibling_formal() {
        // `b ? a` — the default references the sibling formal `a`; lexical.
        let src = "{ a, b ? a }: b";
        let t = resolve_str(src);
        let off = ref_offset(src, "a", 0); // the `a` inside `? a`
        assert!(is_lexical(&t, off), "pattern default sibling ref is lexical");
    }

    #[test]
    fn pattern_at_bind_reference_is_lexical() {
        let src = "{ a } @ args: args";
        let t = resolve_str(src);
        let off = ref_offset(src, "args", 0); // body `args`
        assert!(is_lexical(&t, off), "@-bound name is lexical");
    }

    #[test]
    fn rec_attrset_sibling_reference_is_lexical() {
        let src = "rec { a = 1; b = a; }";
        let t = resolve_str(src);
        let off = ref_offset(src, "a", 0); // reference in `b = a`
        assert!(is_lexical(&t, off), "rec-attrset sibling ref is lexical");
    }

    #[test]
    fn plain_attrset_value_reference_to_key_is_dynamic() {
        // A plain (non-rec) attrset does NOT bind its keys for its values.
        // `b`'s value references `a`, which is NOT in scope (it's a key of a
        // non-rec set) → Dynamic.
        let src = "{ a = 1; b = a; }";
        let t = resolve_str(src);
        let off = ref_offset(src, "a", 0);
        assert_eq!(
            t.get(off),
            Resolution::Dynamic,
            "non-rec attrset keys are NOT lexical for their own values"
        );
    }

    #[test]
    fn nested_let_inner_reference_is_lexical() {
        let src = "let a = 1; in let b = a; in b";
        let t = resolve_str(src);
        // `a` referenced in the inner let — resolves to the outer binder.
        let off = ref_offset(src, "a", 0);
        assert!(is_lexical(&t, off));
    }

    #[test]
    fn keyword_idents_are_never_recorded() {
        let src = "let x = true; in if x then true else false";
        let t = resolve_str(src);
        // No `true`/`false`/`null` offset should ever be recorded.
        for node in rnix::Root::parse(src).syntax().descendants() {
            if let Some(ast::Expr::Ident(ident)) = ast::Expr::cast(node) {
                let text = ident.syntax().text().to_string();
                if matches!(text.as_str(), "true" | "false" | "null") {
                    let off = u32::from(ident.syntax().text_range().start());
                    assert_eq!(
                        t.get(off),
                        Resolution::Dynamic,
                        "keyword `{text}` must never be recorded Lexical"
                    );
                }
            }
        }
    }

    #[test]
    fn recorded_symbol_matches_interned_name() {
        let src = "let foo = 1; in foo";
        let t = resolve_str(src);
        let off = ref_offset(src, "foo", 0);
        match t.get(off) {
            Resolution::Lexical { sym } => {
                assert_eq!(sym, sui_intern::intern("foo"), "recorded sym is intern(name)");
            }
            Resolution::Dynamic => panic!("expected Lexical"),
        }
    }

    #[test]
    fn empty_table_lookup_is_dynamic() {
        let t = ResolveTable::new();
        assert_eq!(t.get(0), Resolution::Dynamic);
        assert_eq!(t.get(9999), Resolution::Dynamic);
        assert!(t.is_empty());
    }
}
