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

use sui_ir::eval_ir::{eval_ir, IrEnv, IrEvalError};
use sui_ir::lower_file;

/// The generated parity-corpus rows, lifted verbatim from the root crate —
/// the same typed `gen_nix`-built expression list the sealed sui↔nix parity
/// gate byte-checks.
#[allow(dead_code)]
#[path = "../../src/parity_corpus.rs"]
mod parity_corpus;

/// The shared hand-authored supplement (also consumed by the slice-1 render
/// differential in `differential.rs`) + the shared two-engine render (also
/// consumed by the slice-3 file differential in `file_differential.rs`).
mod common;
use common::render::{render_ir_value, render_tree};
use common::SUPPLEMENT;

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
        // Since slice 3 every walker builtin NAME is bound (implemented or
        // pre-seeded failed thunk), so unimplemented builtins surface as
        // the typed `MissingBuiltin` — the UndefinedVar arm below only
        // remains for idents that are unbound on BOTH engines.
        IrEvalError::MissingBuiltin(n) => {
            let mut s = String::from("missing-builtin:");
            s.push_str(n);
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
/// `enumerate_gap_candidates` probe. Slice 4 shrank this from 7 → 5; **slice 6
/// shrank it 5 → 0**: `derivation`/`derivationStrict` are now implemented on
/// `eval_ir` (routing through the shared `sui_spec::derivation::apply` spec),
/// so all five derivation-with-context rows — including the multi-output +
/// `++`-into-args + attrs-eq-selects-arg cases — now evaluate AND byte-match
/// the tree-walker's drvPath (which the walker's own suite proves == nix). The
/// list is empty: the pure-subset corpus has no remaining derivation gap.
const CORPUS_KNOWN_GAPS: &[(&str, &str)] = &[];

/// Supplement rows (identified by source text). Slice 4 SHRANK this list from
/// 4 rows to 2: the two `<nixpkgs>` search-path rows now RESOLVE through
/// NIX_PATH exactly like the walker — a hit is a byte-identical `Path` (value
/// match), a miss is a `Throw` on both (both-error match) — so they no longer
/// need an allowlist entry. Only the two `let { … }` legacy-let rows remain
/// (deliberately still unsupported in the pure subset).
const SUPPLEMENT_KNOWN_GAPS: &[(&str, &str)] = &[
    ("let { body = 1; }", "unsupported:legacy-let"),
    ("let { a = 2; body = a; }", "unsupported:legacy-let"),
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
    // ── slice 3: path literals (plain coercion only — value rows) ──
    "/abs/path",
    "./rel/path",
    "../up/one",
    "~/dir/file",
    "/foo/../bar",
    "/..",
    "/foo/./bar/baz",
    r#"let x = "foo"; in /a/${x}/b"#,
    r#"let x = "a/b"; in /root/${x}.nix"#,
    r#"let x = "foo"; in ./${x}.nix"#,
    "toString /bar/${/tmp/foo}",
    "toString ./x",
    "toString [ 1 ./p true ]",
    "/a/b == /a/b",
    "/a/b == /a/c",
    r#"/a/b == "/a/b""#,
    "./x + \"/y\"",
    "/a + /b",
    "builtins.typeOf /a",
    "builtins.isPath ./x",
    "builtins.isPath \"/x\"",
    // ── slice 3: the builtins bridge — dedicated rows per builtin ──
    "builtins.attrNames { b = 1; a = 2; }",
    "builtins.attrValues { b = 10; a = 20; }",
    "builtins.hasAttr \"a\" { a = 1; }",
    "builtins.hasAttr \"z\" { a = 1; }",
    "builtins.getAttr \"a\" { a = 41; }",
    "builtins.length [ 1 2 3 ]",
    "builtins.length [ ]",
    "builtins.head [ 7 8 ]",
    "builtins.tail [ 7 8 9 ]",
    "builtins.elemAt [ 1 2 3 ] 1",
    "builtins.filter (x: x > 1) [ 1 2 3 ]",
    "builtins.filter (x: false) [ 1 2 ]",
    "builtins.concatLists [ [ 1 ] [ ] [ 2 3 ] ]",
    "builtins.concatLists [ ]",
    "builtins.listToAttrs [ { name = \"a\"; value = 1; } { name = \"a\"; value = 2; } { name = \"b\"; value = 3; } ]",
    "builtins.mapAttrs (k: v: k + toString v) { a = 1; b = 2; }",
    "builtins.removeAttrs { a = 1; b = 2; c = 3; } [ \"a\" \"c\" \"zz\" ]",
    "builtins.intersectAttrs { a = 1; b = 2; } { b = 20; c = 30; }",
    "builtins.toString 42",
    "builtins.typeOf { }",
    "builtins.typeOf (x: x)",
    "builtins.typeOf builtins.length",
    "builtins.isInt 1",
    "builtins.isInt 1.5",
    "builtins.isFloat 1.5",
    "builtins.isBool true",
    "builtins.isString \"s\"",
    "builtins.isList [ ]",
    "builtins.isAttrs { }",
    "builtins.isFunction (x: x)",
    "builtins.isFunction builtins.head",
    "builtins.isNull null",
    "builtins.isNull 0",
    "builtins.seq 1 2",
    "builtins.deepSeq [ 1 2 ] 3",
    "builtins.foldl' (a: b: a + b) 0 [ 1 2 3 4 ]",
    "builtins.foldl' (a: b: a - b) 100 [ ]",
    "builtins.genList (i: i * i) 5",
    "builtins.genList (i: i) 0",
    "builtins.substring 1 2 \"abcdef\"",
    "builtins.substring 3 (-1) \"abcdef\"",
    "builtins.substring 10 2 \"abc\"",
    "builtins.substring 0 0 \"abc\"",
    "builtins.stringLength \"abc\"",
    "builtins.stringLength \"\"",
    "builtins.split \"b\" \"abc\"",
    "builtins.split \"([0-9]+)\" \"a1b22c\"",
    "builtins.split \"x\" \"\"",
    "builtins.split \"(a)|(b)\" \"ab\"",
    "builtins.concatStringsSep \", \" [ \"a\" \"b\" ]",
    "builtins.concatStringsSep \"-\" [ ]",
    "builtins.concatStringsSep \"-\" [ 1 ./p ]",
    "builtins.replaceStrings [ \"a\" \"b\" ] [ \"x\" \"y\" ] \"aabbc\"",
    "builtins.replaceStrings [ \"\" ] [ \"-\" ] \"ab\"",
    "builtins.replaceStrings [ ] [ ] \"abc\"",
    "builtins.map (x: x * 2) [ 1 2 ]",
    "map (x: x + 1) (builtins.tail [ 1 2 3 ])",
    // Constants + registry surface.
    "builtins.currentSystem",
    "builtins.storeDir",
    "builtins.nixVersion",
    "builtins.langVersion",
    "builtins ? sort",
    "builtins ? builtins",
    "builtins.builtins ? builtins",
    "builtins.hasAttr \"map\" builtins.builtins",
    // DEFAULT_SCOPE bare globals.
    "isNull null",
    "removeAttrs { a = 1; b = 2; } [ \"a\" ]",
    // Slice 7 — the new pure builtins the hello.drvPath probe added. Each
    // byte-matches the walker (a value row here proves it); the store-/IO-bound
    // ones (pathExists/readFile/readDir/getEnv-of-real-file) are exercised by
    // the real-nixpkgs probe, not the pure corpus.
    "builtins.placeholder \"out\"",
    "builtins.placeholder \"dev\"",
    "builtins.addErrorContext \"ctx\" 42",
    "builtins.addErrorContext \"ctx\" [ 1 2 ]",
    "builtins.getEnv \"SUI_IR_SURELY_UNSET_VAR_XYZ\"",
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

// ── 3b. the slice-4 builtin surface — per-builtin value + error rows ──────
//
// Coverage-debt paydown (the adversary's note): slice 3 leaned on ~1
// differential row per builtin. Slice 4 ships, for EVERY new pure builtin, a
// dedicated VALUE row (a concrete result byte-matched on both engines) AND a
// dedicated ERROR/edge row (a type error / OOB / empty edge — both-error is a
// match, so the row proves the *class* agrees). The per-builtin row count is
// reported in the slice write-up; the two seeds below are the mechanical
// source of that count. Grouped + commented per builtin so a reader can audit
// coverage at a glance.

/// VALUE rows — each must render an identical value on BOTH engines
/// (`require_value = true`). ≥1 (usually ≥2) per new builtin.
const BUILTINS_VALUE_SEED: &[&str] = &[
    // tryEval (catches throw + assert; success + both failure classes)
    "builtins.tryEval 42",
    "builtins.tryEval \"ok\"",
    "builtins.tryEval (builtins.throw \"boom\")",
    "builtins.tryEval (assert false; 1)",
    "builtins.tryEval (assert true; 7)",
    "builtins.tryEval [ 1 2 ]",
    // trace / traceVerbose (return the 2nd argument)
    "builtins.trace \"msg\" 42",
    "builtins.trace \"x\" [ 1 2 ]",
    "builtins.traceVerbose \"m\" \"v\"",
    "builtins.traceVerbose \"a\" 99",
    // add / sub / mul / div (int, float, mixed)
    "builtins.add 2 3",
    "builtins.add 1.5 2",
    "builtins.add 2 1.5",
    "builtins.sub 10 4",
    "builtins.sub 1.5 0.5",
    "builtins.mul 6 7",
    "builtins.mul 2.0 3",
    "builtins.div 9 2",
    "builtins.div 9.0 2",
    "builtins.div 8 3",
    // lessThan (int/float/string)
    "builtins.lessThan 1 2",
    "builtins.lessThan 2 1",
    "builtins.lessThan 1.5 2",
    "builtins.lessThan \"a\" \"b\"",
    "builtins.lessThan \"b\" \"a\"",
    // bitAnd / bitOr / bitXor
    "builtins.bitAnd 12 10",
    "builtins.bitOr 12 10",
    "builtins.bitXor 12 10",
    // ceil / floor
    "builtins.ceil 3.2",
    "builtins.ceil 3.0",
    "builtins.ceil 3",
    "builtins.floor 3.8",
    "builtins.floor (0 - 1.5)",
    // elem
    "builtins.elem 2 [ 1 2 3 ]",
    "builtins.elem 5 [ 1 2 3 ]",
    "builtins.elem \"a\" [ \"a\" \"b\" ]",
    "builtins.elem { x = 1; } [ { p = 0; } { x = 1; } ]",
    // sort (comparator, empty, singleton, builtin comparator)
    "builtins.sort (a: b: a < b) [ 3 1 2 ]",
    "builtins.sort (a: b: a > b) [ 1 2 3 ]",
    "builtins.sort builtins.lessThan [ 5 2 8 1 ]",
    "builtins.sort (a: b: a < b) [ ]",
    "builtins.sort (a: b: a < b) [ 7 ]",
    // all / any (including empty edges)
    "builtins.all (x: x > 0) [ 1 2 3 ]",
    "builtins.all (x: x > 1) [ 1 2 ]",
    "builtins.all (x: false) [ ]",
    "builtins.any (x: x > 2) [ 1 2 3 ]",
    "builtins.any (x: x > 5) [ 1 2 ]",
    "builtins.any (x: true) [ ]",
    // partition
    "builtins.partition (x: x > 1) [ 1 2 3 ]",
    "builtins.partition (x: true) [ ]",
    // groupBy
    "builtins.groupBy (x: if x > 1 then \"big\" else \"small\") [ 1 2 3 ]",
    "builtins.groupBy (x: \"k\") [ ]",
    // concatMap
    "builtins.concatMap (x: [ x x ]) [ 1 2 ]",
    "builtins.concatMap (x: [ ]) [ 1 2 ]",
    // catAttrs (skips non-attrs elements silently, like the walker)
    "builtins.catAttrs \"a\" [ { a = 1; } { b = 2; } { a = 3; } ]",
    "builtins.catAttrs \"a\" [ 1 { a = 5; } ]",
    "builtins.catAttrs \"x\" [ ]",
    // zipAttrsWith
    "builtins.zipAttrsWith (n: vs: vs) [ { a = 1; } { a = 2; b = 3; } ]",
    "builtins.zipAttrsWith (n: vs: builtins.length vs) [ { a = 1; } { a = 2; } ]",
    // filterAttrs rows removed: `builtins.filterAttrs` is a nixpkgs
    // `lib.attrsets` leak, dropped from all three engines 2026-08-17.
    // functionArgs (pattern lambda, ident lambda, builtin)
    "builtins.functionArgs ({ a, b ? 1 }: a)",
    "builtins.functionArgs (x: x)",
    "builtins.functionArgs builtins.head",
    // genericClosure (trivial + BFS-with-dedup)
    "builtins.genericClosure { startSet = [ { key = 1; } ]; operator = item: [ ]; }",
    "builtins.genericClosure { startSet = [ { key = 1; } { key = 2; } ]; operator = item: if item.key < 3 then [ { key = item.key + 1; } ] else [ ]; }",
    // concatStrings / toLower / toUpper / hasPrefix / hasSuffix rows removed:
    // all five are nixpkgs `lib.strings` leaks, dropped from all three engines
    // 2026-08-17. `concatStringsSep` (a REAL builtin, and the correct spelling
    // of what `concatStrings` did) is still exercised above, so the
    // string-concatenation path keeps its differential coverage.
    // match (anchored regex; null on no match; optional groups)
    "builtins.match \"[0-9]+\" \"123\"",
    "builtins.match \"a\" \"b\"",
    "builtins.match \"(a)(b)?\" \"a\"",
    "builtins.match \"([a-z]+)-([0-9]+)\" \"foo-42\"",
    // compareVersions / splitVersion / parseDrvName
    "builtins.compareVersions \"1.0\" \"1.1\"",
    "builtins.compareVersions \"2.0\" \"1.9\"",
    "builtins.compareVersions \"1.0\" \"1.0\"",
    "builtins.splitVersion \"1.2.3\"",
    "builtins.splitVersion \"1.0-rc1\"",
    "builtins.parseDrvName \"foo-1.2\"",
    "builtins.parseDrvName \"hello\"",
    // context (pure subset → hasContext false, getContext {}, discard identity)
    "builtins.hasContext \"x\"",
    "builtins.getContext \"x\"",
    "builtins.unsafeDiscardStringContext \"abc\"",
    "builtins.hasContext (builtins.unsafeDiscardStringContext \"y\")",
    // baseNameOf / dirOf (string args → string; path args: baseNameOf→string,
    // dirOf→path)
    "builtins.baseNameOf \"/a/b/c\"",
    "builtins.baseNameOf \"/a/b/\"",
    "builtins.baseNameOf \"file\"",
    "builtins.dirOf \"/a/b/c\"",
    "builtins.dirOf \"/a\"",
    "builtins.dirOf \"x\"",
    "builtins.baseNameOf /a/b/c",
    "builtins.dirOf /a/b/c",
    // toJSON (scalars, list, attrs, empty)
    "builtins.toJSON 42",
    "builtins.toJSON \"s\"",
    "builtins.toJSON true",
    "builtins.toJSON null",
    "builtins.toJSON 1.5",
    "builtins.toJSON [ 1 2 3 ]",
    "builtins.toJSON { a = 1; b = [ true null ]; }",
    "builtins.toJSON { }",
    // fromJSON (scalars, array, nested object with a float)
    "builtins.fromJSON \"42\"",
    "builtins.fromJSON \"true\"",
    "builtins.fromJSON \"[1, 2, 3]\"",
    "builtins.fromJSON \"{\\\"a\\\": 1, \\\"b\\\": [2, 3.5]}\"",
    "builtins.fromJSON \"\\\"hi\\\"\"",
    // toXML (scalars + the function edge → <function />)
    "builtins.toXML 42",
    "builtins.toXML \"hi\"",
    "builtins.toXML true",
    "builtins.toXML null",
    "builtins.toXML 1.5",
    "builtins.toXML (x: x)",
    // nixPath constant (registry presence + type)
    "builtins ? nixPath",
    "builtins.isList builtins.nixPath",
];

/// ERROR / edge rows — both engines must error (any class; both-error is a
/// match). ≥1 per new builtin, covering type errors, OOB, and the two
/// tryEval NON-catch paths (abort + a real eval error).
const BUILTINS_ERROR_SEED: &[&str] = &[
    // throw / abort raise directly
    "builtins.throw \"boom\"",
    "builtins.abort \"halt\"",
    // tryEval does NOT catch abort or a real eval error (both propagate)
    "builtins.tryEval (builtins.abort \"halt\")",
    "builtins.tryEval (1 + \"a\")",
    // arithmetic type errors + /0
    "builtins.add 1 \"x\"",
    "builtins.sub true 1",
    "builtins.mul 1 [ ]",
    "builtins.div 1 0",
    "builtins.div 5 0",
    "builtins.bitAnd 1 \"x\"",
    "builtins.bitOr {} 1",
    "builtins.bitXor \"a\" 1",
    "builtins.ceil \"x\"",
    "builtins.floor [ ]",
    "builtins.lessThan 1 \"x\"",
    // list HOF type errors / non-bool predicate
    "builtins.elem 1 \"notalist\"",
    "builtins.sort (a: b: 1) [ 1 2 ]",
    "builtins.sort (a: b: a < b) \"notalist\"",
    "builtins.all (x: x) [ 1 2 ]",
    "builtins.any (x: x) [ 1 ]",
    "builtins.partition (x: x) [ 1 ]",
    "builtins.concatMap (x: 5) [ 1 ]",
    "builtins.groupBy (x: 5) [ 1 ]",
    // attr HOF type errors
    "builtins.catAttrs \"a\" 5",
    "builtins.zipAttrsWith (n: v: v) [ 5 ]",
    "builtins.functionArgs 5",
    "builtins.genericClosure { startSet = [ ]; }",
    "builtins.genericClosure 5",
    // string / version type errors
    // `builtins.{concatStrings,toLower,toUpper,hasPrefix,hasSuffix} <bad arg>`
    // rows removed. They WOULD still pass — both engines error — but they
    // would be proving "the attribute is missing", not the argument type error
    // they were written to pin. A row that passes while exercising nothing is
    // the vacuity this suite exists to avoid.
    "builtins.match 1 \"x\"",
    "builtins.compareVersions 1 \"x\"",
    "builtins.splitVersion 5",
    "builtins.parseDrvName 5",
    // context type errors (non-string arg)
    "builtins.hasContext 5",
    "builtins.getContext 5",
    "builtins.unsafeDiscardStringContext 5",
    // path type errors
    "builtins.baseNameOf 5",
    "builtins.dirOf 5",
    // convert errors
    "builtins.toJSON (x: x)",
    "builtins.fromJSON \"not json\"",
    "builtins.fromJSON 5",
    // findFile: not-found + type error
    "builtins.findFile [ ] \"x\"",
    "builtins.findFile [ { prefix = \"a\"; path = \"/no/such/dir/xyz\"; } ] \"a/y\"",
    "builtins.findFile 5 \"x\"",
];

#[test]
fn builtins_value_seed_differential() {
    let rows = named(
        BUILTINS_VALUE_SEED
            .iter()
            .map(|s| ((*s).to_string(), (*s).to_string())),
    );
    match check_rows(&rows, &[], true) {
        Ok(stats) => assert_eq!(
            stats.matched_values,
            rows.len(),
            "every slice-4 builtin VALUE row must be a value match"
        ),
        Err(failures) => panic!(
            "{} slice-4 builtin value rows failed:\n{}",
            failures.len(),
            failures.join("\n")
        ),
    }
}

#[test]
fn builtins_error_seed_differential() {
    let rows = named(
        BUILTINS_ERROR_SEED
            .iter()
            .map(|s| ((*s).to_string(), (*s).to_string())),
    );
    match check_rows(&rows, &[], false) {
        Ok(stats) => assert_eq!(
            stats.matched_both_error,
            rows.len(),
            "every slice-4 builtin ERROR row must error on BOTH engines (got {} both-error, {} \
             value matches — a value match means one engine evaluated where the other errored)",
            stats.matched_both_error,
            stats.matched_values
        ),
        Err(failures) => panic!(
            "{} slice-4 builtin error rows failed:\n{}",
            failures.len(),
            failures.join("\n")
        ),
    }
}

/// `builtins.findFile` VALUE + error rows, built dynamically because a HIT
/// needs a real on-disk file. Uses EXPLICIT search-path entries (findFile
/// takes the search path as an argument — it does NOT read `NIX_PATH`), so
/// this test mutates no global env and is parallel-safe. (The `<name>`
/// search-path form — which DOES read `NIX_PATH` — is differential-tested in
/// `tests/path_parity.rs`, a separate process, to avoid a cross-test env
/// data race.)
#[test]
fn findfile_eval_differential() {
    let tmp = std::env::temp_dir().join("sui-ir-slice4-findfile");
    std::fs::create_dir_all(&tmp).expect("temp tree");
    std::fs::write(tmp.join("thing.nix"), "1").expect("thing.nix");
    let tmp_s = tmp.display().to_string();

    // HIT: `path`+suffix exists on disk → identical `Path` on both engines.
    let hit = format!(
        "builtins.findFile [ {{ prefix = \"sp\"; path = \"{tmp_s}\"; }} ] \"sp/thing.nix\""
    );
    // HIT with the exact-prefix (empty suffix) form.
    let hit_dir = format!(
        "builtins.baseNameOf (builtins.findFile [ {{ prefix = \"sp/thing.nix\"; path = \"{tmp_s}/thing.nix\"; }} ] \"sp/thing.nix\")"
    );
    let value_rows = named([hit, hit_dir].into_iter().map(|s| (s.clone(), s)));
    match check_rows(&value_rows, &[], true) {
        Ok(stats) => assert_eq!(
            stats.matched_values,
            value_rows.len(),
            "findFile HIT rows must resolve to identical values on both engines"
        ),
        Err(failures) => panic!("findFile value rows failed:\n{}", failures.join("\n")),
    }
}

/// The `builtins` attrset carries EXACTLY the walker's eval-visible key
/// set — implemented natives, constants, and every unimplemented name
/// pre-seeded as a typed missing-builtin thunk. Byte-compares the full
/// sorted key list through both engines, so a builtin added to the walker
/// without a bridge entry (or a phantom IR-only name) is a red test.
#[test]
fn builtins_registry_parity() {
    let src = "builtins.attrNames builtins";
    let ir = ir_outcome(src).expect("IR renders the builtins key list");
    match tree_outcome(src) {
        Outcome::Val(tree) => {
            // ── ANTI-VACUITY FLOOR, before the comparison. Two empty renders
            // compare EQUAL, so without this the strongest evidence that the
            // two registries agree would also be produced by both of them
            // vanishing. The IR registry is hand-maintained independently of
            // the walker's (87 native builtins + 24 missing-seeded names +
            // constants, in sui-ir/src/builtins.rs), so this really is a
            // comparison of two lists and not a tautology.
            // The render is nix list syntax — space-separated quoted strings,
            // NOT comma-separated. Count quote pairs.
            let entries = ir.matches('"').count() / 2;
            assert!(
                entries >= 100,
                "the builtins key list rendered only ~{entries} entries — that \
                 is not a usable comparison; two collapsed renders would agree \
                 trivially.\n  ir: {ir}"
            );
            assert_eq!(
                tree, ir,
                "builtins key sets diverge between the walker and the IR bridge"
            );
            eprintln!(
                "builtins_registry_parity: walker == eval_ir, ~{entries} names"
            );
        }
        Outcome::Error(e) => panic!("walker failed to enumerate builtins: {e}"),
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

// ── 3b′. slice 5: the M2.6 recursive-binding (Promise/Blackhole) semantics ─
//
// Mirrors the tree-walker's overlay-fixpoint PROMOTION + Promise-body
// SOFTENING (`sui-eval/src/value.rs::force_inner` + `eval.rs` in_promise_eval).
// Every fixture is evaluated on BOTH engines (walker = oracle) and the
// outcomes must agree byte-for-byte (value) or by error-class (both-error).
// The `Expect` disposition additionally PINS which shapes must converge to a
// VALUE (the previously-divergent (Val, Err) rows this slice closes) vs which
// remain genuine infinite-recursion errors on both.

#[derive(Clone, Copy)]
enum Expect {
    /// Both engines must yield this exact rendered value (pins the promoted
    /// result — the previously-divergent rows this slice fixes).
    Value(&'static str),
    /// Both engines must ERROR (a genuine non-terminating cycle — no promotion
    /// can make it converge; caught by the force-chain guard / nest cap).
    BothError,
}

/// One fixture per row. The comment on each says WHICH mechanism it exercises.
const REC_SEMANTICS: &[(&str, Expect)] = &[
    // ── the three contract shapes ──
    // #1 laziness: WHNF stops at the outer constructor; the inner `inherit b`
    // binds the thunk unforced, so no Blackhole/Promise machinery is exercised.
    // Renders identically on both (the shared 128-deep render sentinel).
    (
        "rec { b = { inherit b; }; }",
        Expect::Value(
            // depth-128 nesting elided — both engines emit the SAME string, so
            // exact-match still proves parity; the tail sentinel is what matters.
            "",
        ),
    ),
    // #2 the direct + mutual self-alias: classified RECURSIVE (Promise), yet the
    // thunk memoizes to itself and the depth-100 force-chain guard fires. Errors
    // on both — the edge where "evaluates to { }" would be a plausible-but-wrong
    // mirror.
    ("let x = x; in x", Expect::BothError),
    ("let a = b; b = a; in a", Expect::BothError),
    // #3 a reduced self:super overlay: the attribute binding's RHS never names
    // itself (`x.a`-shaped) → classified NON-recursive → Blackhole; the fixpoint
    // re-enters it mid-force → SEMANTIC PROMOTION. The `// { p = 1; }` supplies
    // the real content the empty-partial self-reference doesn't (the exact shape
    // of libxcrypt keeping its perl nativeBuildInput).
    (
        "let x = { a = x.a // { p = 1; }; }; in x.a",
        Expect::Value("{ p = 1; }"),
    ),
    // ── promotion + softening together ──
    // Promoted self-reference (`self.a` → { }) is then CALLED as a function; the
    // non-function-call softening (active because IN_PROMISE_EVAL was bumped by
    // the promotion) yields null. Exercises BOTH new mechanisms in one row.
    ("let self = { a = (self.a) 1; }; in self.a", Expect::Value("null")),
    // ── genuine cycles that promotion cannot rescue → error on both ──
    // Pure self-cycle whose body is just the selection `x.a` (select does NOT
    // force its result): the thunk memoizes to itself → force-chain guard.
    ("let x = { a = x.a; }; in x.a", Expect::BothError),
    // Mutual select cycle, same reason.
    (
        "let x = { a = x.b; b = x.a; }; in x.a",
        Expect::BothError,
    ),
    // ── convergent fixpoints that need NO promotion (regression guard) ──
    // a→b, b=5: laziness carries it; no Blackhole re-entry.
    ("let x = { a = x.b; b = 5; }; in x.a", Expect::Value("5")),
    ("let x = { a = 1; b = x.a; }; in x.b", Expect::Value("1")),
    // ── softening must NOT over-fire (outside a Promise body) ──
    // `nope` is undefined but evaluated AFTER self's Promise body completed
    // (IN_PROMISE_EVAL back to 0) → hard UndefinedVar on both, NOT softened.
    (
        "let self = { a = with self; nope; }; in self.a",
        Expect::BothError,
    ),
];

/// Evaluate a fixture on BOTH engines on a fresh, large-stack worker thread —
/// fresh thread-locals per fixture (so a promoting fixture's `PROMOTION_OCCURRED`
/// latch cannot arm the runaway backstop for the next one) and enough stack for
/// the 128-deep render walks.
fn rec_eval_both(src: &str) -> (Outcome, Result<String, IrEvalError>) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || (tree_outcome(&src), ir_outcome(&src)))
        .expect("spawn rec worker")
        .join()
        .expect("rec worker panicked")
}

#[test]
fn rec_semantics_differential() {
    let mut failures: Vec<String> = Vec::new();
    let mut promoted_value_rows = 0usize;
    let mut both_error_rows = 0usize;

    for (src, expect) in REC_SEMANTICS {
        let (tree, ir) = rec_eval_both(src);
        match (expect, &tree, &ir) {
            // Both engines must agree on a value; if `expect` pins a non-empty
            // exact string, both must equal it (the "" pin means "agree,
            // whatever it is" — used for the depth-capped infinite render).
            (Expect::Value(want), Outcome::Val(t), Ok(i)) => {
                if t != i {
                    failures.push(format!("row {src:?} DIVERGED\n  tree: {t}\n  ir:   {i}"));
                } else if !want.is_empty() && t != want {
                    failures.push(format!(
                        "row {src:?} agreed but not on the pinned value\n  got:  {t}\n  want: {want}"
                    ));
                } else {
                    promoted_value_rows += 1;
                }
            }
            (Expect::Value(_), Outcome::Error(e), Ok(i)) => failures.push(format!(
                "row {src:?}: expected a VALUE on both, tree ERRORED ({e}) but ir yielded {i}"
            )),
            (Expect::Value(_), Outcome::Val(t), Err(e)) => failures.push(format!(
                "row {src:?}: expected a VALUE on both, tree={t} but IR ERRORED ({e}) — \
                 the rec-semantics fix regressed"
            )),
            (Expect::Value(_), Outcome::Error(te), Err(ie)) => failures.push(format!(
                "row {src:?}: expected a VALUE on both, but BOTH errored (tree={te} ir={ie})"
            )),
            // Both engines must error (any class — a genuine cycle).
            (Expect::BothError, Outcome::Error(_), Err(_)) => both_error_rows += 1,
            (Expect::BothError, t, i) => failures.push(format!(
                "row {src:?}: expected BOTH to error, got tree={t:?} ir={i:?}"
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "{} rec-semantics rows failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    // Coverage floor: the promotion/softening path really did converge on the
    // (Val, Val) rows (not silently degrade into both-error), and the cycle
    // rows really error.
    assert!(
        promoted_value_rows >= 5,
        "rec-semantics value coverage collapsed: only {promoted_value_rows} value rows"
    );
    assert!(
        both_error_rows >= 4,
        "rec-semantics error coverage collapsed: only {both_error_rows} both-error rows"
    );
}

// ── 3c. a REAL nixpkgs lib.* differential ─────────────────────────────────
//
// Import the ACTUAL nixpkgs `lib` and evaluate a handful of PURE lib.*
// functions — the ones that route through the slice-4 builtins
// (`concatMap`/`foldl'`/`genericClosure`/`splitVersion`/…) — on BOTH engines,
// byte-comparing the rendered result. This proves real nixpkgs library code
// evaluates identically through `eval_ir` and the tree-walker, not just the
// synthetic seeds.
//
// Gated on `SUI_IR_NIXPKGS_LIB` (a path to a nixpkgs `lib` directory) so the
// committed suite stays reproducible on a machine with no nixpkgs checkout —
// a hardcoded `/nix/store/<hash>-source/lib` path would be host-specific. Run
// it with e.g. `SUI_IR_NIXPKGS_LIB=$(nix eval --impure --raw --expr
// 'toString <nixpkgs>')/lib cargo test -p sui-ir nixpkgs_lib_differential`.

#[test]
fn nixpkgs_lib_differential() {
    let Ok(lib) = std::env::var("SUI_IR_NIXPKGS_LIB") else {
        eprintln!(
            "nixpkgs_lib_differential: SUI_IR_NIXPKGS_LIB unset — skipping the real lib.* \
             differential (set it to a nixpkgs `lib` dir to run)"
        );
        return;
    };
    // Each row exercises a pure lib.* function that flows through the
    // slice-4 builtins. `lib` is a deep fixpoint, so evaluate on a large-stack
    // worker thread (the IR recurses on the native stack; see the property
    // test's `eval_both`).
    let rows: Vec<String> = vec![
        format!("(import {lib}).strings.concatMapStringsSep \", \" (x: x + \"!\") [ \"a\" \"b\" \"c\" ]"),
        format!("(import {lib}).lists.foldl' (a: b: a + b) 0 [ 1 2 3 4 5 ]"),
        format!("(import {lib}).lists.concatMap (x: [ x x ]) [ 1 2 3 ]"),
        format!("(import {lib}).versions.splitVersion \"1.2.3-rc1\""),
        format!("(import {lib}).strings.toUpper \"hello\""),
        format!("(import {lib}).strings.hasPrefix \"lib\" \"libfoo\""),
        format!("(import {lib}).lists.unique [ 1 2 2 3 1 3 ]"),
        format!("(import {lib}).attrsets.recursiveUpdate {{ a = {{ b = 1; }}; c = 2; }} {{ a = {{ d = 3; }}; }}"),
        format!("(import {lib}).trivial.compare 1 2"),
    ];
    let mut failures = Vec::new();
    let mut matched = 0usize;
    for src in &rows {
        let src2 = src.clone();
        let (tree, ir) = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(move || (tree_outcome(&src2), ir_outcome(&src2)))
            .expect("spawn")
            .join()
            .expect("lib eval worker panicked");
        match (tree, ir) {
            (Outcome::Val(t), Ok(i)) if t == i => {
                matched += 1;
                eprintln!("lib.* OK  {src}\n      => {t}");
            }
            (Outcome::Val(t), Ok(i)) => failures.push(format!(
                "DIVERGED {src}\n  tree: {t}\n  ir:   {i}"
            )),
            (Outcome::Val(t), Err(e)) => failures.push(format!(
                "IR GAP  {src}\n  tree: {t}\n  ir errors: {e}"
            )),
            (Outcome::Error(e), Ok(i)) => failures.push(format!(
                "WALKER-ERR {src}\n  tree errors: {e}\n  ir: {i}"
            )),
            (Outcome::Error(te), Err(ie)) => failures.push(format!(
                "BOTH-ERR {src}\n  tree: {te}\n  ir: {ie}"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{}/{} lib.* rows matched; {} problem(s):\n{}",
        matched,
        rows.len(),
        failures.len(),
        failures.join("\n")
    );
    eprintln!("nixpkgs_lib_differential: {matched}/{} lib.* rows byte-matched", rows.len());
}

// ── 3d. the REAL nixpkgs hello.drvPath frontier (slice 7) ─────────────────
//
// The whole point of the L3 hello arc: evaluate a REAL nixpkgs package's
// `drvPath` through `eval_ir` and byte-compare against the tree-walker (which
// the walker's own suite proves == `nix eval …drvPath`). Two things are pinned:
//
//   1. THREE-WAY MATCH (asserted): `bootstrap-stage-xgcc-stdenv-linux.drvPath`
//      — a real, complete nixpkgs stdenv derivation reached through the full
//      `import <nixpkgs>` top-level — byte-matches the walker on `eval_ir`.
//      This is the FURTHEST real derivation on hello's `x86_64-linux` bootstrap
//      chain that currently three-way matches; divergence first enters one step
//      up, at `bootstrap-stage2-stdenv-linux`.
//   2. THE FRONTIER (documented, not asserted-equal): `hello.drvPath` on
//      `x86_64-linux` — `eval_ir` reaches a REAL `hello-2.12.2.drv` (asserted:
//      the path ends `-hello-2.12.2.drv`), but does NOT yet byte-match, because
//      the slice-5 overlay-fixpoint promotion drops string-context dependency
//      EDGES the walker preserves through the deep stdenv fixpoint (libxcrypt→
//      perl, openssl→coreutils, xgcc→zlib.out). Whether it matches is PRINTED,
//      never asserted either way — a later slice that lands the walker's
//      "materialized-attrset-with-keys partial" flips it to a match without
//      breaking this test.
//
// Gated on `SUI_IR_NIXPKGS` (a path to a nixpkgs source tree) so the committed
// suite stays hermetic. Run with e.g.
//   SUI_IR_NIXPKGS=$(nix eval --impure --raw --expr 'toString <nixpkgs>') \
//     cargo test -p sui-ir nixpkgs_hello_frontier -- --nocapture

#[test]
fn nixpkgs_hello_frontier() {
    let Ok(nixpkgs) = std::env::var("SUI_IR_NIXPKGS") else {
        eprintln!(
            "nixpkgs_hello_frontier: SUI_IR_NIXPKGS unset — skipping the real hello.drvPath \
             frontier (set it to a nixpkgs source dir to run)"
        );
        return;
    };
    // `.stdenv(.__bootPackages.stdenv)^5` = bootstrap-stage-xgcc-stdenv-linux —
    // the furthest real derivation on hello's x86_64-linux bootstrap chain that
    // three-way matches (depth 4 = bootstrap-stage2-stdenv-linux diverges).
    let boot = "__bootPackages.stdenv.".repeat(5);
    let fixture = format!(
        "(import {nixpkgs} {{ system = \"x86_64-linux\"; }}).stdenv.{boot}drvPath"
    );
    let hello = format!(
        "(import {nixpkgs} {{ system = \"x86_64-linux\"; }}).hello.drvPath"
    );

    // The full nixpkgs top-level is a deep fixpoint; evaluate on a large-stack
    // worker like nixpkgs_lib_differential.
    let eval_both = |src: String| -> (Outcome, Result<String, IrEvalError>) {
        std::thread::Builder::new()
            .stack_size(512 * 1024 * 1024)
            .spawn(move || (tree_outcome(&src), ir_outcome(&src)))
            .expect("spawn")
            .join()
            .expect("hello eval worker panicked")
    };

    // 1. The three-way match (asserted).
    let (ftree, fir) = eval_both(fixture.clone());
    let Outcome::Val(ft) = &ftree else {
        panic!("walker (oracle) failed on the stage-xgcc-stdenv fixture: {ftree:?}");
    };
    let fi = fir.unwrap_or_else(|e| panic!("eval_ir gapped on the stage-xgcc-stdenv fixture: {e}"));
    assert_eq!(
        *ft, fi,
        "bootstrap-stage-xgcc-stdenv-linux.drvPath must THREE-WAY match \
         (eval_ir == walker == nix)\n  walker: {ft}\n  eval_ir: {fi}"
    );
    // (`tree_outcome`/`ir_outcome` render a string value WITH surrounding
    // quotes, so match on `contains`, not `ends_with`.)
    assert!(
        ft.contains("-bootstrap-stage-xgcc-stdenv-linux.drv"),
        "fixture drvPath shape unexpected: {ft}"
    );
    eprintln!("nixpkgs_hello_frontier: THREE-WAY MATCH at bootstrap-stage-xgcc-stdenv-linux\n  {ft}");

    // 2. The frontier (hello reaches a real drv; match is printed, not asserted).
    let (htree, hir) = eval_both(hello.clone());
    let Outcome::Val(ht) = &htree else {
        panic!("walker (oracle) failed on hello.drvPath: {htree:?}");
    };
    match hir {
        Ok(hi) => {
            assert!(
                hi.contains("-hello-2.12.2.drv"),
                "eval_ir must reach a REAL hello-2.12.2.drv (not a placeholder): {hi}"
            );
            if hi == *ht {
                eprintln!("nixpkgs_hello_frontier: hello.drvPath NOW THREE-WAY MATCHES: {hi}");
            } else {
                eprintln!(
                    "nixpkgs_hello_frontier: hello.drvPath reaches a REAL drv but diverges \
                     (the slice-5 promotion edge-drop frontier)\n  walker: {ht}\n  eval_ir: {hi}"
                );
            }
        }
        Err(e) => panic!("eval_ir failed to reach a real hello.drvPath on x86_64-linux: {e}"),
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
    use super::{ir_outcome, tree_outcome, IrEvalError, Outcome};
    use proptest::prelude::*;
    use std::fmt;

    /// Evaluate a generated source on BOTH engines on a large-stack worker
    /// thread, so deep-but-finite recursion (which the walker survives via
    /// `stacker` and the fixed-stack IR would otherwise overflow) completes
    /// on both and the differential compares values, not a stack artefact.
    /// The worker is spawned + joined per case (only one alive at a time),
    /// and the returned outcomes are owned `String`s (Send).
    fn eval_both(src: String) -> (Outcome, Result<String, IrEvalError>) {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let tree = tree_outcome(&src);
                let ir = ir_outcome(&src);
                (tree, ir)
            })
            .expect("spawn eval worker")
            .join()
            .expect("eval worker panicked")
    }

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
        /// `builtins.<name> (arg)` — a 1-arg pure builtin applied to a
        /// generated argument (slice 4: the generator now REACHES builtin
        /// application, closing the proptest-can't-reach-builtins gap).
        Builtin1(&'static str, Box<G>),
        /// `builtins.<name> (a) (b)` — a 2-arg pure builtin.
        Builtin2(&'static str, Box<G>, Box<G>),
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
                    // Emit the direct `body_key = 1;` binding ONLY when it does not
                    // collide with an inherited name. `{ inherit (x) b; b = 1; }` is
                    // a DUPLICATE-attribute error in real nix ("attribute 'b' already
                    // defined") — malformed source a VALID-nix parity differential
                    // must not generate. When body_key is inherited the body reads the
                    // inherited value; otherwise the direct binding provides it. (Known
                    // gap, documented not hidden: on that malformed duplicate input
                    // eval_ir is LENIENT — it drops the unforced `InheritSelect` thunk
                    // and yields a value where nix AND the tree-walker error — an
                    // eval_ir gap on malformed-only input, distinct from valid-surface
                    // parity and out of scope for this gate.)
                    if names.contains(body_key) {
                        write!(f, "; }}")
                    } else {
                        write!(f, "; {body_key} = 1; }}")
                    }
                }
                G::Builtin1(name, arg) => write!(f, "(builtins.{name} ({arg}))"),
                G::Builtin2(name, a, b) => write!(f, "(builtins.{name} ({a}) ({b}))"),
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

    /// 1-arg pure builtins the generator applies to a random argument. Every
    /// one mirrors the walker EXACTLY, so an engine divergence on ANY random
    /// input is a real bug the property test surfaces (that IS the kill-power
    /// the adversary asked for). Excluded on purpose: `toXML` (does not force
    /// nested values, so a thunk-vs-value split on a generated container would
    /// be a rendering artefact, not a semantics bug — tested on scalars only
    /// in the seed).
    const BUILTIN1: &[&str] = &[
        "typeOf", "isNull", "isInt", "isFloat", "isBool", "isString", "isList", "isAttrs",
        "isFunction", "isPath", "length", "stringLength", "attrNames", "attrValues", "head",
        "tail", "toString", "ceil", "floor", "functionArgs", "toJSON",
    ];

    /// 2-arg pure builtins. Excluded on purpose: `add`/`sub`/`mul`/`div`
    /// (plain arithmetic — an unchecked overflow would PANIC the process on a
    /// deep generated mul chain; the `+`/`-`/`*` OPERATORS, which ARE overflow-
    /// checked, stay in `BINOPS`, and the builtins are seed-tested with small
    /// numbers).
    /// `hasPrefix`/`hasSuffix` were dropped from this list on 2026-08-17 with
    /// the six nixpkgs-lib leaks. Leaving them would not have turned the suite
    /// red — both engines now reject them, so every generated row would still
    /// AGREE — but it would agree on "attribute missing" rather than on the
    /// builtin's semantics. That is generator capacity spent proving nothing,
    /// which is worse than a red row because it never asks to be looked at.
    const BUILTIN2: &[&str] = &[
        "elem", "lessThan", "map", "elemAt", "sort", "all", "any", "concatMap", "hasAttr",
        "getAttr", "catAttrs", "compareVersions", "match",
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
                    prop::option::of(inner.clone().prop_map(Box::new)),
                    prop::collection::vec(arb_ident(), 1..3),
                    arb_ident()
                )
                    .prop_map(|(from, names, body_key)| G::Inherited {
                        from,
                        names,
                        body_key,
                    }),
                (prop::sample::select(BUILTIN1), inner.clone())
                    .prop_map(|(name, a)| G::Builtin1(name, Box::new(a))),
                (prop::sample::select(BUILTIN2), inner.clone(), inner.clone())
                    .prop_map(|(name, a, b)| G::Builtin2(name, Box::new(a), Box::new(b))),
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
            // Evaluate BOTH engines on a large-stack worker thread. The
            // tree-walker grows its stack on deep recursion via
            // `stacker::maybe_grow`; the flat-IR `eval_ir` recurses on the
            // native call stack (deliberately — keeping the eval hot path
            // free of the stacker check is part of the L3 speed goal). A
            // generated expression can be deep enough that the walker
            // succeeds (grown stack) while the IR overflows the default
            // ~2 MB test-thread stack — a resource asymmetry, NOT a semantics
            // divergence. Giving the worker a large stack lets deep-but-finite
            // recursion complete on both engines so the differential compares
            // VALUES, never a stack-limit artefact. (Test-only; the library
            // eval path is unchanged.)
            let (tree, ir) = eval_both(src.clone());
            match (tree, ir) {
                // The KILL PATH: both engines produced a value — they MUST be
                // byte-equal. This is where a builtin mirror bug (a wrong
                // VALUE) is caught over random structured arguments.
                (Outcome::Val(t), Ok(i)) => prop_assert_eq!(
                    t, i, "engines diverged on generated source:\n{}", src
                ),
                // Both errored — a match (error classes are not byte-compared).
                (Outcome::Error(_), Err(_)) => {}
                // Walker yields a VALUE, IR errors. Slice 5 CLOSED the
                // dominant historical class here — the walker's M2.6 Promise
                // bridge softening a self/mutually-recursive binding's error to
                // a partial value — by mirroring the overlay-fixpoint PROMOTION
                // and the three `in_promise_eval` softening sites in `eval_ir`.
                // So the tolerance is TIGHTENED: the ONLY remaining acceptable
                // (Val, Err) is the depth-limit asymmetry between the engines —
                // the IR's `force()` chases a thunk chain only 100 links deep
                // (mirrored from `force_value`) and recurses on a fixed native
                // stack, while the walker grows its stack via `stacker`; a
                // generated value deep enough to trip the IR's 100-chain /
                // native-stack limit where the walker still converges is a
                // RESOURCE artefact, not a semantics divergence, and surfaces as
                // `InfiniteRecursion`. Every OTHER (Val, Err) — including any
                // typed GAP — is now a HARD failure: a genuine "IR errors where
                // the walker has a value" bug the rec-semantics slice must not
                // reintroduce.
                (Outcome::Val(t), Err(e)) => prop_assert!(
                    matches!(e, IrEvalError::InfiniteRecursion),
                    "IR errors where the walker yields a value (rec-semantics regression \
                     or new divergence — NOT a tolerated depth artefact): {}\n\
                     tree yielded {}\nsource:\n{}",
                    e, t, src
                ),
                // IR yields where the walker errors — the softening is a
                // (Val, Err) class, NEVER this direction, so an IR value where
                // the walker errored is a genuine "IR too lenient" bug.
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
