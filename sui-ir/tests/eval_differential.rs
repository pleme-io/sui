//! L3 slice 2 — the eval differential (the load-bearing proof).
//!
//! For every seed expression (the generated parity-corpus rows, the shared
//! render-harness supplement, a closed-value seed, and property-generated
//! closed expressions) we evaluate BOTH ways:
//!
//! 1. the tree-walker (`sui_eval::eval` — the parity engine and semantic
//!    oracle), and
//! 2. `lower()` + `eval_ir` (the flat-IR engine, no rowan on the eval path),
//!
//! then render both results through ONE normalized textual form (identical
//! leaf formatting: CppNix float format, sorted attrs, the walker's string
//! escaping) and byte-compare. Every row either
//!
//! * MATCHES (both engines yield the same rendered value, or both fail), or
//! * is a typed KNOWN GAP the test EXPECTS — an explicit allowlist entry
//!   naming the row and the exact typed gap (`unsupported:<construct>` /
//!   `missing-builtin:<ident>`). The allowlist may only SHRINK: a listed row
//!   that stops exhibiting its gap FAILS the suite until it is removed, and
//!   a gap on an unlisted row FAILS the suite outright. `both-error` rows
//!   count as matches (error CLASSES are not byte-compared), and the
//!   closed-value seed additionally requires a rendered VALUE on both sides
//!   so real value-parity coverage cannot silently degrade into
//!   error-vs-error trivia.

use std::rc::Rc;

use sui_ir::eval_ir::{eval_ir, IrEnv, IrEvalError, IrValue};
use sui_ir::lower_file;

/// The generated parity-corpus rows, lifted verbatim from the root crate —
/// the same typed `gen_nix`-built expression list the sealed sui↔nix parity
/// gate byte-checks.
#[allow(dead_code)]
#[path = "../../src/parity_corpus.rs"]
mod parity_corpus;

/// The shared hand-authored supplement (also consumed by the slice-1 render
/// differential in `differential.rs`).
mod common;
use common::SUPPLEMENT;

// ── the normalized render (one textual form, two implementations) ─────────

fn escape_str(s: &str) -> String {
    // Byte-identical to the walker's Display escaping.
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render a TREE-WALKER value: deep-forcing, error-propagating (unlike the
/// walker's `Display`, which swallows a failed force as `<<thunk:error>>` —
/// the differential must see errors as errors).
fn render_tree(v: &sui_eval::Value) -> Result<String, String> {
    use sui_eval::value::Concrete;
    let c = sui_eval::eval::force_concrete(v).map_err(|e| e.to_string())?;
    Ok(match c {
        Concrete::Null => "null".to_string(),
        Concrete::Bool(b) => b.to_string(),
        Concrete::Int(n) => n.to_string(),
        Concrete::Float(f) => sui_compat::versions::cppnix_format_float(f),
        Concrete::String(s) => {
            let mut out = String::from("\"");
            out.push_str(&escape_str(&s));
            out.push('"');
            out
        }
        Concrete::Path(p) => p.to_string(),
        Concrete::List(items) => {
            let mut out = String::from("[ ");
            for item in items.iter() {
                out.push_str(&render_tree(item)?);
                out.push(' ');
            }
            out.push(']');
            out
        }
        Concrete::Attrs(attrs) => {
            let mut out = String::from("{ ");
            for (k, v) in attrs.iter() {
                out.push_str(&k);
                out.push_str(" = ");
                out.push_str(&render_tree(v)?);
                out.push_str("; ");
            }
            out.push('}');
            out
        }
        Concrete::Lambda(_) => "<<lambda>>".to_string(),
        Concrete::Builtin(b) => {
            let mut out = String::from("<<builtin ");
            out.push_str(b.name);
            out.push_str(">>");
            out
        }
    })
}

/// Render an IR value through the SAME normalized form.
fn render_ir_value(v: &IrValue) -> Result<String, IrEvalError> {
    let f = v.force()?;
    Ok(match f {
        IrValue::Null => "null".to_string(),
        IrValue::Bool(b) => b.to_string(),
        IrValue::Int(n) => n.to_string(),
        IrValue::Float(x) => sui_compat::versions::cppnix_format_float(x),
        IrValue::Str(s) => {
            let mut out = String::from("\"");
            out.push_str(&escape_str(&s));
            out.push('"');
            out
        }
        IrValue::List(items) => {
            let mut out = String::from("[ ");
            for item in items.iter() {
                out.push_str(&render_ir_value(item)?);
                out.push(' ');
            }
            out.push(']');
            out
        }
        IrValue::Attrs(attrs) => {
            let mut out = String::from("{ ");
            for (k, v) in attrs.iter() {
                out.push_str(k);
                out.push_str(" = ");
                out.push_str(&render_ir_value(v)?);
                out.push_str("; ");
            }
            out.push('}');
            out
        }
        IrValue::Lambda(_) => "<<lambda>>".to_string(),
        IrValue::Builtin(kind, _) => {
            let mut out = String::from("<<builtin ");
            out.push_str(kind.name());
            out.push_str(">>");
            out
        }
        IrValue::Thunk(_) => unreachable!("force() returned a thunk"),
    })
}

// ── outcomes + typed gap classification ───────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Val(String),
    Error(String),
}

fn tree_outcome(src: &str) -> Outcome {
    match sui_eval::eval(src) {
        Ok(v) => match render_tree(&v) {
            Ok(s) => Outcome::Val(s),
            Err(e) => Outcome::Error(e),
        },
        Err(e) => Outcome::Error(e.to_string()),
    }
}

fn ir_outcome(src: &str) -> Result<String, IrEvalError> {
    let prog = Rc::new(lower_file(src).unwrap_or_else(|e| {
        panic!("seed expression failed to lower (slice-1 gate should catch this): {e}\n{src}")
    }));
    let env = IrEnv::with_pure_builtins();
    eval_ir(&prog, prog.root, &env).and_then(|v| render_ir_value(&v))
}

/// Identifiers that are builtins in the full evaluator but (deliberately)
/// unbound in the pure-subset base env. An `UndefinedVar` of one of these is
/// a typed known gap, not a semantic divergence.
const KNOWN_BUILTIN_IDENTS: &[&str] = &[
    "builtins",
    "derivation",
    "import",
    "throw",
    "abort",
    "elem",
    "concatLists",
    "concatMap",
    "toJSON",
    "baseNameOf",
    "dirOf",
];

fn classify_gap(e: &IrEvalError) -> Option<String> {
    match e {
        IrEvalError::Unsupported(c) => {
            let mut s = String::from("unsupported:");
            s.push_str(c);
            Some(s)
        }
        IrEvalError::UndefinedVar(n) if KNOWN_BUILTIN_IDENTS.contains(&n.as_str()) => {
            let mut s = String::from("missing-builtin:");
            s.push_str(n);
            Some(s)
        }
        _ => None,
    }
}

// ── the shrink-only allowlists ────────────────────────────────────────────
//
// Each entry: (row identifier, expected typed gap tag). A listed row MUST
// fail on the IR side with exactly that tag; a listed row that evaluates (or
// fails differently) FAILS the suite with a remove-me/investigate message —
// the list can only shrink. Unlisted rows must match.

/// Corpus rows (identified by `CorpusRow::name`). Enumerated by the
/// `enumerate_gap_candidates` probe — 17 of the 26 rows reach
/// `derivation`/`builtins`/path literals; the 9 rows that stay inside the
/// pure subset (attr-merge ×3, dynamic-attrpath laziness ×4, colliding-head
/// dynamic tail ×2) are NOT listed and must byte-match.
const CORPUS_KNOWN_GAPS: &[(&str, &str)] = &[
    ("concatStringsSep multi-output context", "missing-builtin:derivation"),
    ("concatStringsSep empty-sep single-element context", "missing-builtin:derivation"),
    ("multi-output producer .dev drvPath", "missing-builtin:derivation"),
    ("multi-level dynamic-tail attrpath — middle-level read stays lazy", "missing-builtin:builtins"),
    ("with-namespace laziness — body WHNF does not force the namespace", "missing-builtin:builtins"),
    ("dotted full-set leaf deep-merges with a deeper sibling (forward)", "missing-builtin:builtins"),
    ("dotted full-set leaf deep-merges with a deeper sibling (reverse)", "missing-builtin:builtins"),
    ("path-literal interp (absolute, string splice)", "unsupported:path"),
    ("path-literal interp (absolute, slash in value)", "unsupported:path"),
    ("path-literal interp (path-typed splice, seam normalized)", "unsupported:path"),
    ("path-literal interp yields a path value", "missing-builtin:builtins"),
    ("list-concat fold — left-associative ++ (value)", "missing-builtin:builtins"),
    ("concatLists flatten (value)", "missing-builtin:builtins"),
    ("concatMap expand (value)", "missing-builtin:builtins"),
    ("list-concat ++ into derivation args — drvPath", "missing-builtin:derivation"),
    ("attrs-eq deep true/false + elem (value)", "missing-builtin:builtins"),
    ("attrs-eq selects derivation arg — drvPath", "missing-builtin:derivation"),
];

/// Supplement rows (identified by source text). The search-path rows are
/// listed (rather than relying on both-error) so the outcome is independent
/// of whether the host has a `NIX_PATH` the tree-walker could resolve; same
/// for the `import` row and the working directory.
const SUPPLEMENT_KNOWN_GAPS: &[(&str, &str)] = &[
    ("<nixpkgs>", "unsupported:search-path"),
    ("<nixpkgs/lib>", "unsupported:search-path"),
    ("~/dir/file", "unsupported:path"),
    ("/abs/path", "unsupported:path"),
    ("./rel/path", "unsupported:path"),
    ("../up/one", "unsupported:path"),
    (r#"let x = "foo"; in /a/${x}/b"#, "unsupported:path"),
    (r#"let x = "foo"; in ./${x}.nix"#, "unsupported:path"),
    (r"toString /bar/${/tmp/foo}", "unsupported:path"),
    ("let { body = 1; }", "unsupported:legacy-let"),
    ("let { a = 2; body = a; }", "unsupported:legacy-let"),
    (
        "map (x: import ./m.nix { inherit x; }) [ 1 2 ]",
        "missing-builtin:import",
    ),
];

// ── the row driver ────────────────────────────────────────────────────────

struct Stats {
    matched_values: usize,
    matched_both_error: usize,
    known_gaps: usize,
}

fn check_rows(
    rows: &[(String, String)],
    allow: &[(&str, &str)],
    require_value: bool,
) -> Result<Stats, Vec<String>> {
    let mut stats = Stats {
        matched_values: 0,
        matched_both_error: 0,
        known_gaps: 0,
    };
    let mut failures: Vec<String> = Vec::new();
    for (id, src) in rows {
        let listed = allow
            .iter()
            .find(|(a, _)| *a == id.as_str())
            .map(|(_, tag)| *tag);
        let ir = ir_outcome(src);
        match (listed, ir) {
            (Some(tag), Err(e)) => match classify_gap(&e) {
                Some(actual) if actual == tag => stats.known_gaps += 1,
                Some(actual) => failures.push(format!(
                    "allowlisted row {id:?}: expected gap {tag}, got gap {actual}"
                )),
                None => failures.push(format!(
                    "allowlisted row {id:?}: expected gap {tag}, got non-gap error {e}"
                )),
            },
            (Some(tag), Ok(rendered)) => failures.push(format!(
                "allowlisted row {id:?} ({tag}) now EVALUATES to {rendered} — remove it from \
                 the allowlist (the list may only shrink)"
            )),
            (None, Ok(rendered)) => match tree_outcome(src) {
                Outcome::Val(expected) if expected == rendered => stats.matched_values += 1,
                Outcome::Val(expected) => failures.push(format!(
                    "row {id:?} DIVERGED\n  tree: {expected}\n  ir:   {rendered}\n  src:  {src}"
                )),
                Outcome::Error(e) => failures.push(format!(
                    "row {id:?}: tree-walker errors ({e}) but IR evaluates to {rendered}\n  src: {src}"
                )),
            },
            (None, Err(e)) => match tree_outcome(src) {
                Outcome::Error(_) if !require_value => stats.matched_both_error += 1,
                Outcome::Error(tree_err) => failures.push(format!(
                    "closed-seed row {id:?} must yield a VALUE on both engines, got \
                     tree={tree_err} ir={e}\n  src: {src}"
                )),
                Outcome::Val(expected) => failures.push(format!(
                    "row {id:?}: tree-walker yields {expected} but IR errors ({e})\n  src: {src}"
                )),
            },
        }
    }
    if failures.is_empty() {
        Ok(stats)
    } else {
        Err(failures)
    }
}

fn named(rows: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    rows.into_iter().collect()
}

// ── 1. corpus rows ────────────────────────────────────────────────────────

#[test]
fn corpus_eval_differential() {
    let rows = named(
        parity_corpus::generate()
            .into_iter()
            .map(|r| (r.name, r.expr)),
    );
    assert!(rows.len() >= 20, "corpus unexpectedly small: {}", rows.len());
    match check_rows(&rows, CORPUS_KNOWN_GAPS, false) {
        Ok(stats) => {
            println!(
                "corpus: {} value matches, {} both-error, {} known gaps",
                stats.matched_values, stats.matched_both_error, stats.known_gaps
            );
            assert!(
                stats.matched_values >= 8,
                "corpus value coverage collapsed: only {} value matches",
                stats.matched_values
            );
        }
        Err(failures) => panic!(
            "{} corpus rows failed the eval differential:\n{}",
            failures.len(),
            failures.join("\n")
        ),
    }
}

// ── 2. supplement rows ────────────────────────────────────────────────────

#[test]
fn supplement_eval_differential() {
    let rows = named(
        SUPPLEMENT
            .iter()
            .map(|s| ((*s).to_string(), (*s).to_string())),
    );
    match check_rows(&rows, SUPPLEMENT_KNOWN_GAPS, false) {
        Ok(stats) => {
            println!(
                "supplement: {} value matches, {} both-error, {} known gaps",
                stats.matched_values, stats.matched_both_error, stats.known_gaps
            );
            assert!(
                stats.matched_values >= 40,
                "supplement value coverage collapsed: only {} value matches",
                stats.matched_values
            );
        }
        Err(failures) => panic!(
            "{} supplement rows failed the eval differential:\n{}",
            failures.len(),
            failures.join("\n")
        ),
    }
}

// ── 3. the closed-value seed (every row must yield a VALUE, both engines) ─

/// Closed, pure, well-formed expressions exercising every subset construct
/// with concrete results — the rows where value parity is guaranteed, so
/// both-error trivia cannot mask coverage.
const CLOSED_SEED: &[&str] = &[
    // literals + strings
    "42",
    "-7",
    "1.5",
    "2.0 + 1",
    r#""plain""#,
    r#"let x = "b"; in "a${x}c""#,
    r#""${1} ${1.5} ${"s"} ${[ 1 2 ]}""#,
    "''\n  two\n  lines''",
    "https://example.org/leaf",
    // arithmetic + comparison + logic
    "1 + 2 * 3 - 4",
    "7 / 2",
    "7.0 / 2",
    "1 + 2.5",
    "1 == 1.0",
    "\"a\" < \"b\"",
    "1 < 2 && 2 <= 2 || false",
    "true -> false",
    "false -> true",
    "!false",
    "-(3 - 5)",
    // short-circuit quirks (walker semantics, deliberately mirrored)
    "false || 1",
    "true && 2",
    "true -> 3",
    // strings + equality
    "\"ab\" + \"cd\"",
    "\"x\" == \"x\"",
    "[ 1 \"a\" ] == [ 1 \"a\" ]",
    "{ a = 1; } == { a = 1; }",
    "{ a = 1; } == { a = 2; }",
    "(x: x) == (y: y)",
    "let f = x: x; in f == f",
    // lists
    "[ ]",
    "[ 1 2 3 ]",
    "[ 1 ] ++ [ 2 3 ]",
    "map (x: x * 2) [ 1 2 3 ]",
    "map toString [ 1 true null ]",
    // attrsets + select + or + hasattr
    "{ }",
    "{ b = 2; a = 1; }",
    "{ a = { b = { c = 3; }; }; }.a.b.c",
    "{ a = 1; }.z or 9",
    "(1).z or 8",
    "{ a = 1; } ? a",
    "{ a = 1; } ? z",
    "1 ? z",
    "{ a.b = 1; a.c = 2; }",
    "{ a.b = 1; a = { c = 2; }; }",
    "{ a = { c = 2; }; a.b = 1; }",
    "{ or = 1; }.or",
    r#"{ "k with space" = 1; }"#,
    "{ ${\"dyn\"} = 4; }",
    "{ ${null} = 4; }",
    "{ a.${null} = 4; }",
    "{ a.${\"t\"} = 4; }",
    "rec { a = 1; b = a + 1; }",
    "rec { a = b; b = 5; }.a",
    "{ a = 1; } // { b = 2; a = 3; }",
    // let / inherit
    "let a = 1; in a",
    "let a = 1; b = a + 1; in b",
    "let a.b = 1; a.c = 2; in a",
    "let x = { q = 7; }; in let inherit (x) q; in q",
    "let a = 3; s = { inherit a; }; in s.a",
    "let k = { x = 8; }; in rec { inherit (k) x; y = x; }.y",
    "let boom = 1 / 0; in 7",
    // lambdas — every param form, defaults, ellipsis, @-bind
    "x: x",
    "(x: x + 1) 41",
    "({ }: 5) { }",
    "({ ... }: 6) { extra = 1; }",
    "({ a }: a) { a = 7; }",
    "({ a, b ? a + 1 }: b) { a = 1; }",
    "({ a ? 1, b ? a }: a + b) { }",
    "(args @ { a, ... }: args.a + a) { a = 4; b = 9; }",
    "({ a, ... } @ args: args ? b) { a = 1; b = 2; }",
    "let f = x: y: x - y; in f 10 3",
    // with / assert / if
    "with { a = 1; }; a",
    "let a = 2; in with { a = 1; }; a",
    "with { a = 1; }; with { a = 2; }; a",
    "with { a = 1; }; b: a",
    "with { m = { n = 9; }; }; m.n",
    "assert true; 1",
    "if 1 == 1 then \"y\" else \"n\"",
    "if false then 1 else 2",
    // __functor
    "{ __functor = self: x: x + 1; } 41",
    // mixed / nested
    "let s = rec { a = { b = 1; }; c = a.b or 0; }; in s.c",
    "let f = { a ? 3 }: a; in f { } + f { a = 4; }",
    "toString 42",
    "toString [ 1 2 ]",
    "\"${{ __toString = self: \"ts\"; }}\"",
    "\"${{ outPath = \"op\"; }}\"",
];

#[test]
fn closed_seed_eval_differential() {
    let rows = named(
        CLOSED_SEED
            .iter()
            .map(|s| ((*s).to_string(), (*s).to_string())),
    );
    match check_rows(&rows, &[], true) {
        Ok(stats) => {
            assert_eq!(
                stats.matched_values,
                rows.len(),
                "every closed-seed row must be a VALUE match"
            );
        }
        Err(failures) => panic!(
            "{} closed-seed rows failed the eval differential:\n{}",
            failures.len(),
            failures.join("\n")
        ),
    }
}

/// The direct self-alias cycle errors on BOTH engines (the walker via its
/// force-chain depth guard, the IR engine via the mirrored guard) — pinned
/// explicitly since it is the one Promise-bridge edge where "evaluates to
/// `{ }`" would have been a plausible-but-wrong mirror.
#[test]
fn self_alias_cycle_errors_on_both_engines() {
    for src in ["let x = x; in x", "let a = b; b = a; in a"] {
        assert!(
            matches!(tree_outcome(src), Outcome::Error(_)),
            "tree-walker unexpectedly evaluated {src:?}"
        );
        assert!(
            matches!(ir_outcome(src), Err(IrEvalError::InfiniteRecursion)),
            "IR engine must hit the cycle guard for {src:?}"
        );
    }
}

// ── 4. determinism ────────────────────────────────────────────────────────

#[test]
fn ir_eval_is_deterministic() {
    for src in CLOSED_SEED {
        let a = ir_outcome(src);
        let b = ir_outcome(src);
        assert_eq!(a, b, "two IR evals diverged for:\n{src}");
    }
}

// ── 5. property tests: generated CLOSED pure expressions ──────────────────

mod generated {
    use super::{ir_outcome, tree_outcome, Outcome};
    use proptest::prelude::*;
    use std::fmt;

    /// A typed generator AST for the CLOSED pure subset — like the slice-1
    /// render-differential generator, minus paths/URIs/search-paths (typed
    /// gaps) and with every free identifier bound by [`Closed`]'s binder, so
    /// generated rows exercise VALUE parity, not just error-vs-error.
    #[derive(Clone, Debug)]
    enum G {
        Int(i64),
        Float(u16, u16),
        Ident(&'static str),
        Str(Vec<GS>),
        List(Vec<G>),
        Attrs { rec: bool, binds: Vec<(GK, G)> },
        LetIn(Vec<(&'static str, G)>, Box<G>),
        If(Box<G>, Box<G>, Box<G>),
        With(Box<G>, Box<G>),
        Assert(Box<G>, Box<G>),
        LambdaIdent(&'static str, Box<G>),
        LambdaPattern {
            entries: Vec<(&'static str, Option<Box<G>>)>,
            ellipsis: bool,
            bind: Option<&'static str>,
            body: Box<G>,
        },
        Select(Box<G>, Vec<GK>, Option<Box<G>>),
        HasAttr(Box<G>, Vec<GK>),
        Apply(Box<G>, Box<G>),
        BinOp(&'static str, Box<G>, Box<G>),
        Unary(&'static str, Box<G>),
        Inherited {
            from: Option<Box<G>>,
            names: Vec<&'static str>,
            body_key: &'static str,
        },
    }

    #[derive(Clone, Debug)]
    enum GS {
        Lit(String),
        Interp(G),
    }

    #[derive(Clone, Debug)]
    enum GK {
        Ident(&'static str),
        Str(String),
        Dynamic(G),
    }

    impl fmt::Display for GK {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                GK::Ident(name) => write!(f, "{name}"),
                GK::Str(s) => write!(f, "\"{s}\""),
                GK::Dynamic(e) => write!(f, "${{({e})}}"),
            }
        }
    }

    impl fmt::Display for G {
        #[allow(clippy::too_many_lines)]
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                G::Int(n) => write!(f, "{n}"),
                G::Float(a, b) => write!(f, "{a}.{b}"),
                G::Ident(name) => write!(f, "{name}"),
                G::Str(parts) => {
                    write!(f, "\"")?;
                    for p in parts {
                        match p {
                            GS::Lit(s) => write!(f, "{s}")?,
                            GS::Interp(e) => write!(f, "${{({e})}}")?,
                        }
                    }
                    write!(f, "\"")
                }
                G::List(items) => {
                    write!(f, "[")?;
                    for item in items {
                        write!(f, " ({item})")?;
                    }
                    write!(f, " ]")
                }
                G::Attrs { rec, binds } => {
                    if *rec {
                        write!(f, "rec ")?;
                    }
                    write!(f, "{{")?;
                    for (key, value) in binds {
                        write!(f, " {key} = ({value});")?;
                    }
                    write!(f, " }}")
                }
                G::LetIn(binds, body) => {
                    write!(f, "let")?;
                    for (name, value) in binds {
                        write!(f, " {name} = ({value});")?;
                    }
                    write!(f, " in ({body})")
                }
                G::If(c, t, e) => write!(f, "if ({c}) then ({t}) else ({e})"),
                G::With(ns, body) => write!(f, "with ({ns}); ({body})"),
                G::Assert(c, body) => write!(f, "assert ({c}); ({body})"),
                G::LambdaIdent(param, body) => write!(f, "{param}: ({body})"),
                G::LambdaPattern {
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
                G::Select(subject, path, or_default) => {
                    write!(f, "({subject})")?;
                    for key in path {
                        write!(f, ".{key}")?;
                    }
                    if let Some(d) = or_default {
                        write!(f, " or ({d})")?;
                    }
                    Ok(())
                }
                G::HasAttr(subject, path) => {
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
                G::Apply(func, arg) => write!(f, "({func}) ({arg})"),
                G::BinOp(op, lhs, rhs) => write!(f, "({lhs}) {op} ({rhs})"),
                G::Unary(op, e) => write!(f, "{op}({e})"),
                G::Inherited {
                    from,
                    names,
                    body_key,
                } => {
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

    /// The closed wrapper: binds every identifier the generator can emit,
    /// with a spread of value types, so a generated expression is CLOSED.
    struct Closed(G);

    impl fmt::Display for Closed {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "let a = 1; b = 2; c = \"cc\"; x = {{ q = 1; }}; y = [ 1 2 ]; \
                 foo = (w: w); bar = 4.5; v0 = 0; in ({})",
                self.0
            )
        }
    }

    const IDENTS: &[&str] = &["a", "b", "c", "x", "y", "foo", "bar", "v0"];
    const BINOPS: &[&str] = &[
        "++", "//", "+", "-", "*", "/", "&&", "||", "->", "==", "!=", "<", "<=", ">", ">=",
    ];

    fn arb_ident() -> impl Strategy<Value = &'static str> {
        prop::sample::select(IDENTS)
    }

    fn arb_leaf() -> impl Strategy<Value = G> {
        prop_oneof![
            (0i64..=99_999).prop_map(G::Int),
            (any::<u16>(), any::<u16>()).prop_map(|(a, b)| G::Float(a, b)),
            arb_ident().prop_map(G::Ident),
            "[a-z]{0,8}".prop_map(|s| G::Str(vec![GS::Lit(s)])),
        ]
    }

    fn arb_key(inner: BoxedStrategy<G>) -> BoxedStrategy<GK> {
        prop_oneof![
            4 => arb_ident().prop_map(GK::Ident),
            2 => "[a-z]{1,5}".prop_map(GK::Str),
            1 => inner.prop_map(GK::Dynamic),
        ]
        .boxed()
    }

    fn arb_expr() -> impl Strategy<Value = G> {
        arb_leaf().prop_recursive(3, 40, 5, |inner| {
            let key = arb_key(inner.clone());
            let strparts = prop::collection::vec(
                prop_oneof![
                    3 => "[a-z ]{0,6}".prop_map(GS::Lit),
                    1 => inner.clone().prop_map(GS::Interp),
                ],
                0..4,
            );
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(G::List),
                (
                    any::<bool>(),
                    prop::collection::vec((key.clone(), inner.clone()), 0..4)
                )
                    .prop_map(|(rec, binds)| G::Attrs { rec, binds }),
                (
                    prop::collection::vec((arb_ident(), inner.clone()), 1..4),
                    inner.clone()
                )
                    .prop_map(|(binds, body)| G::LetIn(binds, Box::new(body))),
                (inner.clone(), inner.clone(), inner.clone())
                    .prop_map(|(c, t, e)| G::If(Box::new(c), Box::new(t), Box::new(e))),
                (inner.clone(), inner.clone())
                    .prop_map(|(ns, b)| G::With(Box::new(ns), Box::new(b))),
                (inner.clone(), inner.clone())
                    .prop_map(|(c, b)| G::Assert(Box::new(c), Box::new(b))),
                (arb_ident(), inner.clone()).prop_map(|(p, b)| G::LambdaIdent(p, Box::new(b))),
                (
                    prop::collection::vec(
                        (arb_ident(), prop::option::of(inner.clone().prop_map(Box::new))),
                        0..3
                    ),
                    any::<bool>(),
                    prop::option::of(arb_ident()),
                    inner.clone()
                )
                    .prop_map(|(entries, ellipsis, bind, body)| G::LambdaPattern {
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
                    .prop_map(|(s, path, d)| G::Select(Box::new(s), path, d)),
                (inner.clone(), prop::collection::vec(key, 1..3))
                    .prop_map(|(s, path)| G::HasAttr(Box::new(s), path)),
                (inner.clone(), inner.clone())
                    .prop_map(|(func, arg)| G::Apply(Box::new(func), Box::new(arg))),
                (prop::sample::select(BINOPS), inner.clone(), inner.clone())
                    .prop_map(|(op, l, r)| G::BinOp(op, Box::new(l), Box::new(r))),
                (prop::sample::select(&["!", "-"][..]), inner.clone())
                    .prop_map(|(op, e)| G::Unary(op, Box::new(e))),
                strparts.prop_map(G::Str),
                (
                    prop::option::of(inner.prop_map(Box::new)),
                    prop::collection::vec(arb_ident(), 1..3),
                    arb_ident()
                )
                    .prop_map(|(from, names, body_key)| G::Inherited {
                        from,
                        names,
                        body_key,
                    }),
            ]
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// The eval differential over generated CLOSED expressions: both
        /// engines agree — same rendered value, or both fail.
        #[test]
        fn generated_closed_expressions_agree(gen_expr in arb_expr()) {
            let src = Closed(gen_expr).to_string();
            let parse = rnix::Root::parse(&src);
            prop_assert!(
                parse.errors().is_empty(),
                "generator emitted unparseable source: {:?}\n{}",
                parse.errors(),
                src
            );
            let ir = ir_outcome(&src);
            let tree = tree_outcome(&src);
            match (tree, ir) {
                (Outcome::Val(t), Ok(i)) => prop_assert_eq!(
                    t, i, "engines diverged on generated source:\n{}", src
                ),
                (Outcome::Error(_), Err(_)) => {}
                (Outcome::Val(t), Err(e)) => prop_assert!(
                    false, "tree yields {} but IR errors ({})\nsource:\n{}", t, e, src
                ),
                (Outcome::Error(e), Ok(i)) => prop_assert!(
                    false, "tree errors ({}) but IR yields {}\nsource:\n{}", e, i, src
                ),
            }
        }
    }
}

// ── 6. the micro A/B (interleaved timing; run with --ignored --nocapture) ─

/// A synthetic let/apply/binop-heavy expression (no paths, no builtins):
/// deep enough that per-eval AST-vs-IR traversal cost dominates.
const AB_SRC: &str = "let
  f = x: y: x + y * 2 - (x - y);
  g = h: n: h n (n + 1);
  h3 = a1: a2: a3: f (g f a1) (f a2 a3);
  s1 = g f 1;
  s2 = g f (s1 + 2);
  s3 = h3 s1 s2 3;
  s4 = f (s3 - s2) (s1 * 2);
  s5 = g (x: y: x * y - 1) (s4 - s3);
  s6 = h3 s5 s4 s3;
  s7 = f s6 (g f s5);
  s8 = g (x: y: x - y + 7) (s7 - s6);
  t1 = if s8 > s7 then s8 - s7 else s7 - s8;
  t2 = let u = t1 + s6; v = u * 2; in v - u;
  t3 = (w: w w1) (z: z + t2);
  w1 = t2 - t1;
  acc = s1 + s2 + s3 + s4 + s5 + s6 + s7 + s8 + t1 + t2 + t3;
in acc * 2 - (acc / 3)";

#[test]
#[ignore = "micro A/B timing — run explicitly with --ignored --nocapture"]
fn micro_ab_ir_vs_tree_walker() {
    use std::time::Instant;

    // Parse ONCE (both engines), lower ONCE (IR engine) — the A/B measures
    // eval only: tree-walk re-traverses rowan per eval; IR walks the flat
    // Program per eval.
    let parse = rnix::Root::parse(AB_SRC);
    assert!(parse.errors().is_empty());
    let expr = parse.tree().expr().expect("non-empty");
    let prog = Rc::new(lower_file(AB_SRC).expect("lowers"));

    // Byte-agreement first — a timing of diverging engines is meaningless.
    let tree_env = sui_eval::value::Env::new();
    let tree_val = sui_eval::eval::eval_expr(&expr, &tree_env).expect("tree evals");
    let tree_txt = render_tree(&tree_val).expect("tree renders");
    let ir_txt = ir_outcome(AB_SRC).expect("ir evals");
    assert_eq!(tree_txt, ir_txt, "A/B expression must agree before timing");

    const ITERS: u32 = 2_000;
    const ROUNDS: usize = 5;
    println!("micro A/B — {ITERS} evals/round, {ROUNDS} interleaved rounds, result {tree_txt}");
    println!("round | tree-walker | eval_ir | ratio (tree/ir)");
    for round in 0..ROUNDS {
        let t0 = Instant::now();
        for _ in 0..ITERS {
            let env = sui_eval::value::Env::new();
            let v = sui_eval::eval::eval_expr(&expr, &env).expect("tree evals");
            std::hint::black_box(sui_eval::eval::force_value(&v).expect("forces"));
        }
        let tree_dt = t0.elapsed();

        let t1 = Instant::now();
        for _ in 0..ITERS {
            let env = IrEnv::new();
            let v = eval_ir(&prog, prog.root, &env).expect("ir evals");
            std::hint::black_box(v.force().expect("forces"));
        }
        let ir_dt = t1.elapsed();

        println!(
            "{round:>5} | {tree_us:>9}us | {ir_us:>7}us | {ratio:.2}x",
            tree_us = tree_dt.as_micros(),
            ir_us = ir_dt.as_micros(),
            ratio = tree_dt.as_secs_f64() / ir_dt.as_secs_f64(),
        );
    }
}

// ── gap-probe helper (development aid; run with --ignored --nocapture) ────

#[test]
#[ignore = "prints per-row gap classification for allowlist maintenance"]
fn enumerate_gap_candidates() {
    let corpus = parity_corpus::generate();
    println!("── corpus rows ──");
    for row in &corpus {
        match ir_outcome(&row.expr) {
            Ok(_) => {}
            Err(e) => match classify_gap(&e) {
                Some(tag) => println!("GAP  {:?} → {tag}", row.name),
                None => println!("ERR  {:?} → {e}", row.name),
            },
        }
    }
    println!("── supplement rows ──");
    for src in SUPPLEMENT {
        match ir_outcome(src) {
            Ok(_) => {}
            Err(e) => match classify_gap(&e) {
                Some(tag) => println!("GAP  {src:?} → {tag}"),
                None => println!("ERR  {src:?} → {e}"),
            },
        }
    }
}
