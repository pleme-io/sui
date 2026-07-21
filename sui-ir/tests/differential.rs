//! The lowering differential harness (the load-bearing proof of this slice).
//!
//! For every expression in the seed corpus (the generated parity-corpus rows
//! lifted verbatim from `src/parity_corpus.rs`, plus a hand-authored
//! supplement covering constructs the corpus does not exercise), we:
//!
//! 1. parse with rnix (assert parse-clean),
//! 2. lower the AST once into a flat `Program`,
//! 3. re-render the IR to the normalized textual form (`render_ir`),
//! 4. independently render the rowan AST to the same normalized form
//!    (`render_ast` — a direct AST walk that never touches `lower()`),
//!
//! and assert byte-equality — proving the lowering lost nothing: no dropped
//! child, no reordered binding, no collapsed leaf. Property tests then run
//! the same differential over generated expression trees, plus structural
//! invariants (post-order ids, spans table, determinism).

use sui_ir::render::{render_ast, render_ir};
use sui_ir::{lower, lower_file, ExprId, LowerError, Program};

/// The generated parity-corpus rows, lifted verbatim from the root crate
/// (`src/parity_corpus.rs`) — the same typed `gen_nix`-built expression list
/// the sealed sui↔nix parity gate byte-checks.
#[allow(dead_code)]
#[path = "../../src/parity_corpus.rs"]
mod parity_corpus;

// ── helpers ───────────────────────────────────────────────────────────────

/// Parse one expression source; panic (with the source) if rnix reports
/// parse errors. Returns the top-level expression.
fn parse_clean(src: &str) -> rnix::ast::Expr {
    let parse = rnix::Root::parse(src);
    assert!(
        parse.errors().is_empty(),
        "seed expression failed to parse: {errors:?}\nsource:\n{src}",
        errors = parse.errors()
    );
    parse
        .tree()
        .expr()
        .unwrap_or_else(|| panic!("seed expression parsed to an empty Root:\n{src}"))
}

/// The differential for one source: lower → render_ir must equal the
/// independent render_ast. Returns an error description on divergence.
fn differential(src: &str) -> Result<Program, String> {
    let expr = parse_clean(src);
    let program = lower(&expr).map_err(|e| {
        let mut s = String::from("lower() failed: ");
        s.push_str(&e.to_string());
        s
    })?;
    let ast_txt = render_ast(&expr).map_err(|e| {
        let mut s = String::from("render_ast() failed: ");
        s.push_str(&e.to_string());
        s
    })?;
    let ir_txt = render_ir(&program);
    if ir_txt == ast_txt {
        Ok(program)
    } else {
        let mut s = String::from("IR render diverged from AST render\n  ast: ");
        s.push_str(&ast_txt);
        s.push_str("\n  ir:  ");
        s.push_str(&ir_txt);
        Err(s)
    }
}

/// Structural invariants every lowered `Program` must satisfy:
/// * spans table parallel to exprs,
/// * root is the last entry (post-order),
/// * every child id strictly less than its parent's id,
/// * every expr reachable-by-index is in bounds.
fn assert_program_invariants(program: &Program, src: &str) {
    assert_eq!(
        program.exprs.len(),
        program.spans.len(),
        "spans table not parallel to exprs for:\n{src}"
    );
    assert_eq!(
        program.root.index() + 1,
        program.exprs.len(),
        "root is not the last (post-order) entry for:\n{src}"
    );
    for i in 0..program.exprs.len() {
        let id = ExprId(u32::try_from(i).unwrap());
        for child in program.children(id) {
            assert!(
                child.index() < program.exprs.len(),
                "child id out of bounds for:\n{src}"
            );
            assert!(
                child < id,
                "post-order violated: child {child:?} >= parent {id:?} for:\n{src}"
            );
        }
    }
}

// ── the corpus differential (the seed list from the parity gate) ──────────

#[test]
fn parity_corpus_differential() {
    let rows = parity_corpus::generate();
    assert!(
        rows.len() >= 20,
        "parity corpus unexpectedly small ({} rows) — seed lift broken?",
        rows.len()
    );
    let mut failures: Vec<(String, String)> = Vec::new();
    for row in &rows {
        match differential(&row.expr) {
            Ok(program) => assert_program_invariants(&program, &row.expr),
            Err(detail) => failures.push((row.name.clone(), detail)),
        }
    }
    assert!(
        failures.is_empty(),
        "{} corpus rows diverged:\n{:#?}",
        failures.len(),
        failures
    );
}

// ── the supplemental seed (constructs the corpus does not exercise) ───────

/// Hand-authored rows covering the rest of rnix's expression surface:
/// floats, URIs, search/home paths, path interpolation, multiline strings,
/// escapes, every lambda-param form, with/assert/legacy-let, select-or,
/// has-attr, unary ops, every binop (pipes included), rec attrsets,
/// inherit(-from), dynamic + string keys, `or` as an attr name, `__curPos`.
const SUPPLEMENT: &[&str] = &[
    // literals
    "1.5",
    "0.0",
    "3.141592653589793",
    "https://example.org/x?y=1",
    "<nixpkgs>",
    "<nixpkgs/lib>",
    "~/dir/file",
    "/abs/path",
    "./rel/path",
    "../up/one",
    // path interpolation
    r#"let x = "foo"; in /a/${x}/b"#,
    r#"let x = "foo"; in ./${x}.nix"#,
    r"toString /bar/${/tmp/foo}",
    // strings
    r#""""#,
    r#""plain""#,
    "\"esc \\\" \\n \\t \\\\ done\"",
    r#""a${"b"}c${toString 1}""#,
    "''\n  multi\n  line ${\"interp\"}\n  tail''",
    "''''",
    // idents + select + or-default + has-attr
    "a",
    "a.b.c",
    r#"a."k".c"#,
    "a.${k}.c",
    "a.b or c",
    "(f x).y or (g z)",
    "a ? b",
    "a ? b.c.\"d\"",
    "a ? b.${k}",
    "{ or = 1; }.or",
    // apply
    "f x",
    "f x y z",
    "(f: f 1) (x: x)",
    // lambdas — every param form
    "x: x",
    "x: y: x",
    "{ }: 1",
    "{ ... }: 1",
    "{ a }: a",
    "{ a, b }: a",
    "{ a ? 1, b ? a }: b",
    "{ a, ... }: a",
    "args @ { a, ... }: a",
    "{ a, ... } @ args: args",
    // let / legacy let / rec
    "let a = 1; in a",
    "let a = 1; b = a; in b",
    "let a.b = 1; in a.b",
    "let inherit (s) k; in k",
    "let { body = 1; }",
    "let { a = 2; body = a; }",
    "rec { a = 1; b = a; }",
    "rec { a = b; b = 1; }.a",
    // attrsets — keys + inherit interleave
    "{ }",
    "{ a = 1; }",
    "{ a.b.c = 1; }",
    r#"{ "k" = 1; }"#,
    r#"{ "k${"i"}" = 1; }"#,
    "{ ${k} = 1; }",
    "{ a.${k}.b = 1; }",
    "{ inherit a; b = 1; inherit (s) c d; e = 2; }",
    r#"{ inherit (s) "k"; }"#,
    // lists
    "[ ]",
    "[ 1 2 3 ]",
    "[ a ./p \"s\" { x = 1; } (f y) ]",
    // binops — all of them
    "[1] ++ [2]",
    "{ a = 1; } // { b = 2; }",
    "1 + 2",
    "1 - 2",
    "2 * 3",
    "4 / 2",
    "true && false",
    "true || false",
    "true -> false",
    "1 == 2",
    "1 != 2",
    "1 < 2",
    "1 <= 2",
    "1 > 2",
    "1 >= 2",
    // unary
    "!true",
    "-x",
    "-(1 + 2)",
    "!(a && b)",
    // if / with / assert
    "if a then b else c",
    "if a == 1 then { x = 1; } else [ 2 ]",
    "with pkgs; [ hello ]",
    "with (import ./x.nix); y",
    "assert a == 1; b",
    "assert true; assert false; x",
    // parens (kept 1:1)
    "(1)",
    "((x))",
    // __curPos
    "__curPos",
    "{ pos = __curPos; }",
    // deep nesting / mixed
    "let f = { a ? 3 }: a; in f { }",
    "map (x: import ./m.nix { inherit x; }) [ 1 2 ]",
    "let s = rec { a = { b = 1; }; c = a.b or 0; }; in s.c",
];

#[test]
fn supplement_differential() {
    let mut failures: Vec<(String, String)> = Vec::new();
    for src in SUPPLEMENT {
        match differential(src) {
            Ok(program) => assert_program_invariants(&program, src),
            Err(detail) => failures.push(((*src).to_string(), detail)),
        }
    }
    assert!(
        failures.is_empty(),
        "{} supplemental rows diverged:\n{:#?}",
        failures.len(),
        failures
    );
}

// ── pipe operators (rnix 0.14 parses |> / <|) ─────────────────────────────

#[test]
fn pipe_operators_lower_if_parsed() {
    // rnix 0.14 knows TOKEN_PIPE_RIGHT / TOKEN_PIPE_LEFT; if the parser
    // accepts them, the lowering must too (eval support is a later-slice
    // question). If the parser rejects them, that is a parse failure — not
    // a silent lowering gap — and this test documents which.
    for src in ["1 |> f", "f <| 1"] {
        let parse = rnix::Root::parse(src);
        if parse.errors().is_empty() {
            if let Err(detail) = differential(src) {
                panic!("pipe expression parsed but diverged: {detail}");
            }
        } else {
            // Parse-level rejection: lower_file must surface it as a typed
            // ParseFailure, never a panic.
            assert!(matches!(
                lower_file(src),
                Err(LowerError::ParseFailure { .. })
            ));
        }
    }
}

// ── totality: typed errors, never panics ──────────────────────────────────

#[test]
fn broken_source_returns_typed_parse_failure() {
    for src in ["let x =", "{ a = ; }", "if x then", "a.", "1 +"] {
        match lower_file(src) {
            Err(LowerError::ParseFailure { .. } | LowerError::ParseErrorNode { .. }) => {}
            other => panic!("expected typed parse failure for {src:?}, got {other:?}"),
        }
    }
}

#[test]
fn empty_source_is_a_typed_missing_root() {
    match lower_file("") {
        Err(
            LowerError::Missing {
                construct: "Root",
                field: "expr",
            }
            | LowerError::ParseFailure { .. },
        ) => {}
        other => panic!("expected Missing(Root.expr) or ParseFailure, got {other:?}"),
    }
}

#[test]
fn curpos_lowers_structurally() {
    let program = lower_file("__curPos").expect("__curPos must lower (1:1 structural)");
    assert_eq!(program.exprs.len(), 1);
    assert!(matches!(program.exprs[0], sui_ir::Ir::CurPos));
}

#[test]
fn huge_integer_is_a_typed_error() {
    match lower_file("99999999999999999999999999") {
        Err(LowerError::IntOutOfRange { .. }) => {}
        // rnix may reject at parse level instead — either is a typed error.
        Err(LowerError::ParseFailure { .. }) => {}
        other => panic!("expected typed overflow error, got {other:?}"),
    }
}

// ── determinism + spans ───────────────────────────────────────────────────

#[test]
fn lowering_is_deterministic() {
    for src in SUPPLEMENT {
        let expr = parse_clean(src);
        let a = lower(&expr).expect("lowers");
        let b = lower(&expr).expect("lowers");
        assert_eq!(a, b, "two lowerings of the same AST diverged for:\n{src}");
    }
}

#[test]
fn root_span_covers_the_whole_expression() {
    let src = "let a = 1; in a + 2";
    let expr = parse_clean(src);
    let program = lower(&expr).expect("lowers");
    let span = program.span(program.root);
    assert_eq!(span.start, 0);
    assert_eq!(span.end, u32::try_from(src.len()).unwrap());
}

// ── property tests: generated expressions round-trip ──────────────────────

mod generated {
    use super::{assert_program_invariants, differential};
    use proptest::prelude::*;
    use std::fmt;

    /// A tiny typed generator AST rendered to always-parseable Nix source
    /// via `Display` (typed emission — no string templating of syntax at
    /// call sites). Operands are parenthesized aggressively so operator
    /// precedence can never produce an unparseable string; the extra
    /// `Paren` nodes are part of the parse surface both renders see.
    #[derive(Clone, Debug)]
    enum GenExpr {
        Int(i64),
        Float(u16, u16),
        Ident(&'static str),
        Str(Vec<GenStrPart>),
        PathRel(&'static str),
        PathAbs(&'static str),
        SearchPath(&'static str),
        Uri(&'static str),
        List(Vec<GenExpr>),
        AttrSet {
            rec: bool,
            binds: Vec<(GenKey, GenExpr)>,
        },
        LetIn(Vec<(&'static str, GenExpr)>, Box<GenExpr>),
        If(Box<GenExpr>, Box<GenExpr>, Box<GenExpr>),
        With(Box<GenExpr>, Box<GenExpr>),
        Assert(Box<GenExpr>, Box<GenExpr>),
        LambdaIdent(&'static str, Box<GenExpr>),
        LambdaPattern {
            entries: Vec<(&'static str, Option<Box<GenExpr>>)>,
            ellipsis: bool,
            bind: Option<&'static str>,
            body: Box<GenExpr>,
        },
        Select(Box<GenExpr>, Vec<GenKey>, Option<Box<GenExpr>>),
        HasAttr(Box<GenExpr>, Vec<GenKey>),
        Apply(Box<GenExpr>, Box<GenExpr>),
        BinOp(&'static str, Box<GenExpr>, Box<GenExpr>),
        Unary(&'static str, Box<GenExpr>),
        Inherited {
            from: Option<Box<GenExpr>>,
            names: Vec<&'static str>,
            body_key: &'static str,
        },
    }

    #[derive(Clone, Debug)]
    enum GenStrPart {
        Lit(String),
        Interp(GenExpr),
    }

    #[derive(Clone, Debug)]
    enum GenKey {
        Ident(&'static str),
        Str(String),
        Dynamic(GenExpr),
    }

    impl fmt::Display for GenKey {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                GenKey::Ident(name) => write!(f, "{name}"),
                GenKey::Str(s) => write!(f, "\"{s}\""),
                GenKey::Dynamic(e) => write!(f, "${{({e})}}"),
            }
        }
    }

    impl fmt::Display for GenExpr {
        #[allow(clippy::too_many_lines)]
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                GenExpr::Int(n) => write!(f, "{n}"),
                GenExpr::Float(a, b) => write!(f, "{a}.{b}"),
                GenExpr::Ident(name) => write!(f, "{name}"),
                GenExpr::Str(parts) => {
                    write!(f, "\"")?;
                    for p in parts {
                        match p {
                            GenStrPart::Lit(s) => write!(f, "{s}")?,
                            GenStrPart::Interp(e) => write!(f, "${{({e})}}")?,
                        }
                    }
                    write!(f, "\"")
                }
                GenExpr::PathRel(p) | GenExpr::PathAbs(p) => write!(f, "{p}"),
                GenExpr::SearchPath(p) => write!(f, "<{p}>"),
                GenExpr::Uri(u) => write!(f, "{u}"),
                GenExpr::List(items) => {
                    write!(f, "[")?;
                    for item in items {
                        write!(f, " ({item})")?;
                    }
                    write!(f, " ]")
                }
                GenExpr::AttrSet { rec, binds } => {
                    if *rec {
                        write!(f, "rec ")?;
                    }
                    write!(f, "{{")?;
                    for (key, value) in binds {
                        write!(f, " {key} = ({value});")?;
                    }
                    write!(f, " }}")
                }
                GenExpr::LetIn(binds, body) => {
                    write!(f, "let")?;
                    for (name, value) in binds {
                        write!(f, " {name} = ({value});")?;
                    }
                    write!(f, " in ({body})")
                }
                GenExpr::If(c, t, e) => write!(f, "if ({c}) then ({t}) else ({e})"),
                GenExpr::With(ns, body) => write!(f, "with ({ns}); ({body})"),
                GenExpr::Assert(c, body) => write!(f, "assert ({c}); ({body})"),
                GenExpr::LambdaIdent(param, body) => write!(f, "{param}: ({body})"),
                GenExpr::LambdaPattern {
                    entries,
                    ellipsis,
                    bind,
                    body,
                } => {
                    if let Some(b) = bind {
                        write!(f, "{b} @ ")?;
                    }
                    write!(f, "{{")?;
                    let mut first = true;
                    for (name, default) in entries {
                        if !first {
                            write!(f, ",")?;
                        }
                        first = false;
                        write!(f, " {name}")?;
                        if let Some(d) = default {
                            write!(f, " ? ({d})")?;
                        }
                    }
                    if *ellipsis {
                        if !first {
                            write!(f, ",")?;
                        }
                        write!(f, " ...")?;
                    }
                    write!(f, " }}: ({body})")
                }
                GenExpr::Select(subject, path, or_default) => {
                    write!(f, "({subject})")?;
                    for (i, key) in path.iter().enumerate() {
                        write!(f, "{}{key}", if i == 0 { "." } else { "." })?;
                    }
                    if let Some(d) = or_default {
                        write!(f, " or ({d})")?;
                    }
                    Ok(())
                }
                GenExpr::HasAttr(subject, path) => {
                    write!(f, "({subject}) ?")?;
                    for (i, key) in path.iter().enumerate() {
                        if i == 0 {
                            write!(f, " {key}")?;
                        } else {
                            write!(f, ".{key}")?;
                        }
                    }
                    Ok(())
                }
                GenExpr::Apply(func, arg) => write!(f, "({func}) ({arg})"),
                GenExpr::BinOp(op, lhs, rhs) => write!(f, "({lhs}) {op} ({rhs})"),
                GenExpr::Unary(op, e) => write!(f, "{op}({e})"),
                GenExpr::Inherited {
                    from,
                    names,
                    body_key,
                } => {
                    // `{ inherit a b; k = 1; }` — inherits interleaved with a
                    // binding, optionally `inherit (expr) …`.
                    write!(f, "{{ inherit")?;
                    if let Some(src) = from {
                        write!(f, " (({src}))")?;
                    }
                    for name in names {
                        write!(f, " {name}")?;
                    }
                    write!(f, "; {body_key} = 1; }}")
                }
            }
        }
    }

    const IDENTS: &[&str] = &["a", "b", "c", "x", "y", "foo", "bar", "v0"];
    const BINOPS: &[&str] = &[
        "++", "//", "+", "-", "*", "/", "&&", "||", "->", "==", "!=", "<", "<=", ">", ">=",
    ];

    fn arb_ident() -> impl Strategy<Value = &'static str> {
        prop::sample::select(IDENTS)
    }

    fn arb_leaf() -> impl Strategy<Value = GenExpr> {
        prop_oneof![
            (0i64..=99_999).prop_map(GenExpr::Int),
            (any::<u16>(), any::<u16>()).prop_map(|(a, b)| GenExpr::Float(a, b)),
            arb_ident().prop_map(GenExpr::Ident),
            "[a-z]{0,8}".prop_map(|s| GenExpr::Str(vec![GenStrPart::Lit(s)])),
            Just(GenExpr::PathRel("./rel/leaf")),
            Just(GenExpr::PathAbs("/abs/leaf")),
            Just(GenExpr::SearchPath("nixpkgs")),
            Just(GenExpr::Uri("https://example.org/leaf")),
        ]
    }

    fn arb_key(inner: BoxedStrategy<GenExpr>) -> BoxedStrategy<GenKey> {
        prop_oneof![
            4 => arb_ident().prop_map(GenKey::Ident),
            2 => "[a-z]{1,5}".prop_map(GenKey::Str),
            1 => inner.prop_map(GenKey::Dynamic),
        ]
        .boxed()
    }

    fn arb_expr() -> impl Strategy<Value = GenExpr> {
        arb_leaf().prop_recursive(3, 40, 5, |inner| {
            let key = arb_key(inner.clone());
            let strparts = prop::collection::vec(
                prop_oneof![
                    3 => "[a-z ]{0,6}".prop_map(GenStrPart::Lit),
                    1 => inner.clone().prop_map(GenStrPart::Interp),
                ],
                0..4,
            );
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(GenExpr::List),
                (
                    any::<bool>(),
                    prop::collection::vec((key.clone(), inner.clone()), 0..4)
                )
                    .prop_map(|(rec, binds)| GenExpr::AttrSet { rec, binds }),
                (
                    prop::collection::vec((arb_ident(), inner.clone()), 1..4),
                    inner.clone()
                )
                    .prop_map(|(binds, body)| GenExpr::LetIn(binds, Box::new(body))),
                (inner.clone(), inner.clone(), inner.clone()).prop_map(|(c, t, e)| {
                    GenExpr::If(Box::new(c), Box::new(t), Box::new(e))
                }),
                (inner.clone(), inner.clone())
                    .prop_map(|(ns, b)| GenExpr::With(Box::new(ns), Box::new(b))),
                (inner.clone(), inner.clone())
                    .prop_map(|(c, b)| GenExpr::Assert(Box::new(c), Box::new(b))),
                (arb_ident(), inner.clone())
                    .prop_map(|(p, b)| GenExpr::LambdaIdent(p, Box::new(b))),
                (
                    prop::collection::vec(
                        (arb_ident(), prop::option::of(inner.clone().prop_map(Box::new))),
                        0..3
                    ),
                    any::<bool>(),
                    prop::option::of(arb_ident()),
                    inner.clone()
                )
                    .prop_map(|(entries, ellipsis, bind, body)| GenExpr::LambdaPattern {
                        entries,
                        ellipsis,
                        bind,
                        body: Box::new(body),
                    }),
                (
                    inner.clone(),
                    prop::collection::vec(key.clone(), 1..3),
                    prop::option::of(inner.clone().prop_map(Box::new))
                )
                    .prop_map(|(s, path, d)| GenExpr::Select(Box::new(s), path, d)),
                (inner.clone(), prop::collection::vec(key, 1..3))
                    .prop_map(|(s, path)| GenExpr::HasAttr(Box::new(s), path)),
                (inner.clone(), inner.clone())
                    .prop_map(|(func, arg)| GenExpr::Apply(Box::new(func), Box::new(arg))),
                (prop::sample::select(BINOPS), inner.clone(), inner.clone())
                    .prop_map(|(op, l, r)| GenExpr::BinOp(op, Box::new(l), Box::new(r))),
                (prop::sample::select(&["!", "-"][..]), inner.clone())
                    .prop_map(|(op, e)| GenExpr::Unary(op, Box::new(e))),
                strparts.prop_map(GenExpr::Str),
                (
                    prop::option::of(inner.prop_map(Box::new)),
                    prop::collection::vec(arb_ident(), 1..3),
                    arb_ident()
                )
                    .prop_map(|(from, names, body_key)| GenExpr::Inherited {
                        from,
                        names,
                        body_key,
                    }),
            ]
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// The differential over generated trees: every generated source
        /// parses, lowers, and the two independent renders agree.
        #[test]
        fn generated_expressions_round_trip(gen_expr in arb_expr()) {
            let src = gen_expr.to_string();
            // The generator only emits parseable text; a parse error here is
            // a generator bug and should fail loudly.
            let parse = rnix::Root::parse(&src);
            prop_assert!(
                parse.errors().is_empty(),
                "generator emitted unparseable source: {:?}\n{}",
                parse.errors(),
                src
            );
            match differential(&src) {
                Ok(program) => assert_program_invariants(&program, &src),
                Err(detail) => prop_assert!(false, "{}\nsource:\n{}", detail, src),
            }
        }

        /// Lowering the same parsed tree twice yields identical Programs.
        #[test]
        fn generated_lowering_deterministic(gen_expr in arb_expr()) {
            let src = gen_expr.to_string();
            let parse = rnix::Root::parse(&src);
            prop_assert!(parse.errors().is_empty());
            let expr = parse.tree().expr().expect("non-empty");
            let a = sui_ir::lower(&expr).expect("lowers");
            let b = sui_ir::lower(&expr).expect("lowers");
            prop_assert_eq!(a, b);
        }
    }
}
