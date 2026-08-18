//! Shared seed rows for the sui-ir differential harnesses.
//!
//! `SUPPLEMENT` is the hand-authored expression list covering the rest of
//! rnix's expression surface beyond the generated parity-corpus rows. It is
//! consumed by BOTH test binaries: `differential.rs` (slice 1 — the
//! rowan↔IR render differential) and `eval_differential.rs` (slice 2 — the
//! tree-walker↔eval_ir result differential). One list, two proofs.
//! `render` is the shared normalized two-engine render (slices 2 + 3).

// Each integration-test binary compiles `common` independently and uses a
// different subset of it.
#[allow(dead_code)]
pub mod render;

/// Hand-authored rows covering the rest of rnix's expression surface:
/// floats, URIs, search/home paths, path interpolation, multiline strings,
/// escapes, every lambda-param form, with/assert/legacy-let, select-or,
/// has-attr, unary ops, every binop (pipes included), rec attrsets,
/// inherit(-from), dynamic + string keys, `or` as an attr name, `__curPos`.
pub const SUPPLEMENT: &[&str] = &[
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

    // ── ★ the parse-time splice (`sui-normalize`), EVERY shape with a NESTED
    //    twin ────────────────────────────────────────────────────────────
    //
    // nix decides duplicate-key merge-vs-overwrite at PARSE time from SYNTAX,
    // as a destructive splice into the FIRST-declared node whose `rec` flag
    // governs and into whose scope the second side is re-scoped. These rows
    // are here rather than in a bespoke test because ONE list buys both
    // proofs: `differential.rs` byte-compares `render_ir` against
    // `render_ast` on them (the side arena must leave the two renders
    // identical), and `eval_differential.rs` compares eval_ir's ANSWER
    // against the tree-walker's.
    //
    // ★ Every shape appears twice, at top level and NESTED, and that is not
    // padding. The walker's adoption shipped working only at the top level
    // for a day: its descendant walk recursed through children that cast to
    // `ast::Expr`, but an attrset's children are `AttrpathValue` nodes, which
    // do not — so the recursion stopped at the first attrset and never
    // reached a binding's VALUE. Every test written for it was a top-level
    // binder, which is exactly why none of them caught it, and almost all
    // real nix nests.
    "rec { o = {e=1;}; o.x = 2; }",
    "{ w = rec { o = {e=1;}; o.x = 2; }; }",
    "rec { a = {b=1;}; a = {c=2;}; }",
    "{ w = rec { a = {b=1;}; a = {c=2;}; }; }",
    // reverse order, with a sibling reading THROUGH the merge
    "rec { a.c = 2; a = {b=1;}; x = a.b; }",
    "{ w = rec { a.c = 2; a = {b=1;}; x = a.b; }; }",
    // the merged node stays rec: `y` reads a sibling the OTHER half introduced
    "rec { a = {b=1;}; a = {c=2;}; y = a.b + a.c; }",
    // `let` — the same rule, one binder up
    "let a = {b=1;}; a = {c=2;}; in a",
    "{ w = (let a = {b=1;}; a = {c=2;}; in a); }",
    "let a = {b=1;}; a.c = 2; in a",
    "let a.c = 2; a = {b=1;}; in a",
    // ★ RE-SCOPING — the half no value-level merge can express. The second
    // side's bindings become bindings OF THE FIRST NODE, so they are scoped
    // by it and the LATER `rec` is discarded.
    "let b=1; in { a={x=2;}; a=rec{b=99;c=b;}; }",
    "let b=5; in { a=rec{c=b;}; a={b=9;}; }",
    // ★ THE ACCEPTANCE CASE: a dotted path splices INTO a `rec` literal, the
    // spliced member resolves `d` from inside that rec scope, and the rec
    // body's `b` reads `c` from the spliced-in member — mutual recursion
    // ACROSS the merge boundary. Nothing short of a real splice produces it.
    "{ a = rec { b = c + 1; d = 2; }; a.c = d + 3; }.a.b",
    "{ outer = { a = rec { b = c + 1; d = 2; }; a.c = d + 3; }; }",
    "{ p = { q = { a = rec { b = c + 1; d = 2; }; a.c = d + 3; }; }; }",
    "[ { a = rec { b = c + 1; d = 2; }; a.c = d + 3; } ]",
    "let f = x: x; in { z = { a = rec { b = c + 1; d = 2; }; a.c = d + 3; }; }",
    // plain dotted merge — no duplicate key, correct before AND after, so a
    // regression in the common case cannot hide behind the fixes above
    "rec { a.b = 1; a.c = 2; }",
    "{ w = rec { a.b = 1; a.c = 2; }; }",
];
