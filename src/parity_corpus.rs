//! Typed Nix-expression corpus generator for the sealed parity gate.
//!
//! CLOSED-LOOP MASS-SYNTHESIS applied to sui↔nix eval parity: instead of
//! hand-writing escaped Nix strings, we build eval-surface shapes from a
//! typed AST and render each to canonical Nix source, then byte-check every
//! generated row against the nix oracle. A new eval-surface variant becomes a
//! generated row, not a hand-authored probe — the corpus grows by adding a
//! typed shape, and every Match row can never silently regress.
//!
//! This `Nx` type is an INTERIM, minimal mirror of `gen-nix`'s `NixValue`
//! (`theory/NIX-AST.md` — the canonical typed Nix-expression AST, in the `gen`
//! repo). DESTINATION (extract-and-reuse): depend on `gen-nix` directly once
//! its cross-repo build integration into sui lands, and delete this. Kept
//! minimal + test-only on purpose so the duplication is bounded and obvious.
//! Per TYPED EMISSION, call sites never `format!()` Nix syntax — they build
//! `Nx` values and `render()` them.

/// A part of an interpolated string: literal text or a `${expr}` splice.
pub enum StrPart {
    Lit(&'static str),
    Interp(Nx),
}

/// A minimal typed Nix expression. Only the variants the parity corpus needs.
pub enum Nx {
    /// String literal — `"..."`.
    Str(&'static str),
    /// Bare identifier or keyword — `builtins`, `x`, `true`.
    Ident(&'static str),
    /// Interpolated string — `"a${e}b"`.
    IStr(Vec<StrPart>),
    /// List — `[ e1 e2 ... ]`.
    List(Vec<Nx>),
    /// Attribute set — `{ k = v; ... }` / `rec { ... }`. Keys are written
    /// verbatim, so a dotted key like `"a.b"` renders as a nested path
    /// binding (exactly the CppNix desugar the merge fix exercises).
    Attr { rec: bool, entries: Vec<(&'static str, Nx)> },
    /// `let <bindings> in <body>`.
    Let { bindings: Vec<(&'static str, Nx)>, body: Box<Nx> },
    /// Application — `f a b ...`.
    App(Box<Nx>, Vec<Nx>),
    /// Select — `base.<path>` (path written verbatim, may be dotted).
    Select(Box<Nx>, &'static str),
    /// Binary op — `a <op> b`.
    Bin(Box<Nx>, &'static str, Box<Nx>),
    /// Pre-rendered fragment (escape hatch, e.g. `builtins.currentSystem`).
    Raw(&'static str),
}

impl Nx {
    fn str(s: &'static str) -> Nx { Nx::Str(s) }
    fn ident(s: &'static str) -> Nx { Nx::Ident(s) }
    fn select(base: Nx, path: &'static str) -> Nx { Nx::Select(Box::new(base), path) }
    fn app(f: Nx, args: Vec<Nx>) -> Nx { Nx::App(Box::new(f), args) }
    fn bin(l: Nx, op: &'static str, r: Nx) -> Nx { Nx::Bin(Box::new(l), op, Box::new(r)) }

    /// Render to canonical Nix source.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.render_into(&mut out);
        out
    }

    fn render_into(&self, out: &mut String) {
        match self {
            Nx::Str(s) => { out.push('"'); out.push_str(s); out.push('"'); }
            Nx::Ident(s) | Nx::Raw(s) => out.push_str(s),
            Nx::IStr(parts) => {
                out.push('"');
                for p in parts {
                    match p {
                        StrPart::Lit(l) => out.push_str(l),
                        StrPart::Interp(e) => { out.push_str("${"); e.render_into(out); out.push('}'); }
                    }
                }
                out.push('"');
            }
            Nx::List(items) => {
                out.push_str("[ ");
                for it in items { it.render_into(out); out.push(' '); }
                out.push(']');
            }
            Nx::Attr { rec, entries } => {
                if *rec { out.push_str("rec "); }
                out.push_str("{ ");
                for (k, v) in entries {
                    out.push_str(k); out.push_str(" = "); v.render_into(out); out.push_str("; ");
                }
                out.push('}');
            }
            Nx::Let { bindings, body } => {
                out.push_str("let ");
                for (k, v) in bindings {
                    out.push_str(k); out.push_str(" = "); v.render_into(out); out.push_str("; ");
                }
                out.push_str("in "); body.render_into(out);
            }
            Nx::App(f, args) => {
                out.push('('); f.render_into(out);
                for a in args { out.push(' '); a.render_into(out); }
                out.push(')');
            }
            Nx::Select(base, path) => { base.render_into(out); out.push('.'); out.push_str(path); }
            Nx::Bin(l, op, r) => {
                out.push('('); l.render_into(out);
                out.push(' '); out.push_str(op); out.push(' ');
                r.render_into(out); out.push(')');
            }
        }
    }
}

/// Whether a generated row is expected to be byte-identical to nix (`Match`)
/// or is a tracked frontier that a fix must graduate (`KnownDiverge`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RowExpect { Match, KnownDiverge }

/// One generated corpus row: a stable name, the rendered Nix expression, and
/// its expected verdict.
pub struct CorpusRow {
    pub name: String,
    pub expr: String,
    pub expect: RowExpect,
}

fn drv(name: &'static str, extra: Vec<(&'static str, Nx)>) -> Nx {
    // derivation { name = "<name>"; system = builtins.currentSystem;
    //              builder = "/bin/sh"; <extra> }
    let mut entries: Vec<(&'static str, Nx)> = vec![
        ("name", Nx::str(name)),
        ("system", Nx::Raw("builtins.currentSystem")),
        ("builder", Nx::str("/bin/sh")),
    ];
    entries.extend(extra);
    Nx::app(Nx::ident("derivation"), vec![Nx::Attr { rec: false, entries }])
}

fn interp(parts: Vec<StrPart>) -> Nx { Nx::IStr(parts) }

fn row(name: &str, expr: Nx, expect: RowExpect) -> CorpusRow {
    CorpusRow { name: name.to_string(), expr: expr.render(), expect }
}

/// Generate the mass-synthesis parity matrix. Every row is byte-checked
/// sui-vs-nix by the caller. Grouped by the eval-surface category each root
/// this session hardened, plus close variants that guard the *class*.
pub fn generate() -> Vec<CorpusRow> {
    let mut rows: Vec<CorpusRow> = Vec::new();

    // ── Category: attrset dotted + full-set deep-merge (root 73b904d) ──────
    // order-1: dotted then full-set — MUST merge (was the pkg-config-wrapper
    // env.addFlags drop). s.a.b + s.a.c == "xy".
    rows.push(row(
        "attr-merge order1 (dotted then fullset)",
        Nx::Let {
            bindings: vec![("s", Nx::Attr { rec: false, entries: vec![
                ("a.b", Nx::str("x")),
                ("a", Nx::Attr { rec: false, entries: vec![("c", Nx::str("y"))] }),
            ] })],
            body: Box::new(Nx::bin(Nx::select(Nx::ident("s"), "a.b"), "+", Nx::select(Nx::ident("s"), "a.c"))),
        },
        RowExpect::Match,
    ));
    // deep-nested collision — a.b.c + a.b.e + a.d == "132".
    rows.push(row(
        "attr-merge deep-nested",
        Nx::Let {
            bindings: vec![("s", Nx::Attr { rec: false, entries: vec![
                ("a.b.c", Nx::str("1")),
                ("a", Nx::Attr { rec: false, entries: vec![("d", Nx::str("2"))] }),
                ("a.b.e", Nx::str("3")),
            ] })],
            body: Box::new(Nx::bin(
                Nx::bin(Nx::select(Nx::ident("s"), "a.b.c"), "+", Nx::select(Nx::ident("s"), "a.b.e")),
                "+", Nx::select(Nx::ident("s"), "a.d"))),
        },
        RowExpect::Match,
    ));
    // non-colliding control — a plain attrset is unchanged by the merge path.
    rows.push(row(
        "attr-merge non-colliding control",
        Nx::Let {
            bindings: vec![("s", Nx::Attr { rec: false, entries: vec![
                ("a", Nx::Attr { rec: false, entries: vec![("b", Nx::str("x"))] }),
                ("c", Nx::str("z")),
            ] })],
            body: Box::new(Nx::bin(Nx::select(Nx::ident("s"), "a.b"), "+", Nx::select(Nx::ident("s"), "c"))),
        },
        RowExpect::Match,
    ));

    // ── Category: concatStringsSep/concatStrings context (root a67c244) ────
    // A multi-output element's `out` must survive into the drv input set.
    let ctx_l = Nx::app(
        Nx::select(Nx::ident("builtins"), "concatStringsSep"),
        vec![Nx::str(":"), Nx::List(vec![
            interp(vec![StrPart::Interp(Nx::select(Nx::ident("m"), "out")), StrPart::Lit("/lib")]),
            interp(vec![StrPart::Interp(Nx::select(Nx::ident("m"), "dev")), StrPart::Lit("/include")]),
        ])],
    );
    rows.push(row(
        "concatStringsSep multi-output context",
        Nx::Let {
            bindings: vec![("m", drv("m", vec![("outputs", Nx::List(vec![Nx::str("out"), Nx::str("dev")]))]))],
            body: Box::new(Nx::select(drv("c", vec![("L", ctx_l)]), "drvPath")),
        },
        RowExpect::Match,
    ));
    // Empty-separator concatStringsSep (the concatStrings shape) preserves
    // context the same way. (`builtins.concatStrings` is not a nix builtin —
    // it is `lib.concatStrings` — so the no-separator path is exercised via
    // `concatStringsSep ""`, which both engines expose.)
    let cs_l = Nx::app(
        Nx::select(Nx::ident("builtins"), "concatStringsSep"),
        vec![Nx::str(""), Nx::List(vec![
            interp(vec![StrPart::Interp(Nx::select(Nx::ident("m"), "out")), StrPart::Lit("/lib")]),
        ])],
    );
    rows.push(row(
        "concatStringsSep empty-sep single-element context",
        Nx::Let {
            bindings: vec![("m", drv("m2", vec![("outputs", Nx::List(vec![Nx::str("out"), Nx::str("dev")]))]))],
            body: Box::new(Nx::select(drv("c2", vec![("L", cs_l)]), "drvPath")),
        },
        RowExpect::Match,
    ));

    // ── Category: multi-output modulo (the class fixed earlier this arc) ────
    // A consumer selecting a specific output of a multi-output producer.
    rows.push(row(
        "multi-output producer .dev drvPath",
        Nx::Let {
            bindings: vec![("p", drv("prod", vec![("outputs", Nx::List(vec![Nx::str("out"), Nx::str("dev"), Nx::str("lib")]))]))],
            body: Box::new(Nx::select(
                drv("cons", vec![("BI", interp(vec![StrPart::Interp(Nx::select(Nx::ident("p"), "dev"))]))]),
                "drvPath")),
        },
        RowExpect::Match,
    ));

    rows
}
