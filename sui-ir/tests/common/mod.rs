//! Shared seed rows for the sui-ir differential harnesses.
//!
//! `SUPPLEMENT` is the hand-authored expression list covering the rest of
//! rnix's expression surface beyond the generated parity-corpus rows. It is
//! consumed by BOTH test binaries: `differential.rs` (slice 1 — the
//! rowan↔IR render differential) and `eval_differential.rs` (slice 2 — the
//! tree-walker↔eval_ir result differential). One list, two proofs.

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
];
