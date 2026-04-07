//! Layer 4: differential eval — primitives & builtins.
//!
//! Each `#[test]` function drives a slice of Nix expressions through
//! both sui and real `nix-instantiate --eval --json --strict` and
//! asserts the JSON outputs match. The whole file silently skips in
//! offline mode (see `common::skip_if_offline`), so `cargo test`
//! stays green on CI without nix installed.
//!
//! **To run this layer:** `SUI_TEST_ONLINE=1 cargo test -p sui-eval
//! --test diff_eval_primitives`.
//!
//! Failures are expected here as sui's compat surface grows — each
//! failing case is a candidate for a follow-up gap ticket. Comment
//! out a broken case with `// BROKEN: <reason>` so the remaining
//! cases in that category still run.

mod common;

/// Run every expression in `cases` as an individual differential
/// assertion. All failures are collected and the test panics at the
/// end with a summary — so one broken case doesn't hide the rest of
/// the category.
fn run_cases(label: &str, cases: &[&str]) {
    if common::skip_if_offline(label) {
        return;
    }
    let mut failures: Vec<String> = Vec::new();
    for (i, expr) in cases.iter().enumerate() {
        let oracle = common::nix_eval_json(expr);
        let ours = common::sui_eval_json(expr);
        if oracle != ours {
            failures.push(format!(
                "  [{i}] {expr}\n       nix: {}\n       sui: {}",
                serde_json::to_string(&oracle).unwrap_or_default(),
                serde_json::to_string(&ours).unwrap_or_default(),
            ));
        }
    }
    if !failures.is_empty() {
        panic!(
            "{label}: {} / {} cases failed:\n{}",
            failures.len(),
            cases.len(),
            failures.join("\n")
        );
    }
}

// ── Arithmetic ───────────────────────────────────────────────────────

#[test]
fn diff_arithmetic() {
    run_cases(
        "arithmetic",
        &[
            "1 + 1",
            "2 + 3 * 4",
            "(2 + 3) * 4",
            "10 - 3",
            "20 / 4",
            "7 / 2",
            "7 - 3",
            "-5 + 3",
            "-(3 + 4)",
            "1.5 + 2.5",
            "5.0 / 2.0",
            "3.0 * 1.5",
            "1 + 2.0",
            "2.0 * 3",
        ],
    );
}

// ── Comparison ───────────────────────────────────────────────────────

#[test]
fn diff_comparison() {
    run_cases(
        "comparison",
        &[
            "1 < 2",
            "2 < 2",
            "3 < 2",
            "1 <= 2",
            "2 <= 2",
            "3 > 2",
            "2 > 2",
            "1 >= 2",
            "2 >= 2",
            "1 == 1",
            "1 == 2",
            "1 != 2",
            r#""abc" == "abc""#,
            r#""abc" < "abd""#,
            "[1 2] == [1 2]",
            "[1 2] == [1 3]",
            "{ a = 1; } == { a = 1; }",
            "{ a = 1; } == { a = 2; }",
        ],
    );
}

// ── Boolean & logical ────────────────────────────────────────────────

#[test]
fn diff_boolean() {
    run_cases(
        "boolean",
        &[
            "true && true",
            "true && false",
            "false && true",
            "true || false",
            "false || false",
            "!true",
            "!false",
            "!(1 == 2)",
            "true -> false",
            "false -> false",
            "true -> true",
        ],
    );
}

// ── String concat & interpolation ────────────────────────────────────

#[test]
fn diff_strings() {
    run_cases(
        "strings",
        &[
            r#""hello" + " " + "world""#,
            r#""" + "abc""#,
            r#""foo${"bar"}""#,
            r#""${"a"}${"b"}""#,
            r#"let x = "abc"; in "${x}def""#,
            r#"let n = 42; in "n=${toString n}""#,
            r#""abc" + (if true then "d" else "e")"#,
        ],
    );
}

// ── List operations ──────────────────────────────────────────────────

#[test]
fn diff_lists() {
    run_cases(
        "lists",
        &[
            "[]",
            "[1]",
            "[1 2 3]",
            "[1 2] ++ [3 4]",
            "builtins.length [1 2 3]",
            "builtins.length []",
            "builtins.head [1 2 3]",
            "builtins.tail [1 2 3]",
            "builtins.elemAt [10 20 30] 0",
            "builtins.elemAt [10 20 30] 2",
            "builtins.elem 2 [1 2 3]",
            "builtins.elem 4 [1 2 3]",
            "builtins.map (x: x + 1) [1 2 3]",
            "builtins.filter (x: x > 2) [1 2 3 4]",
            "builtins.foldl' (acc: x: acc + x) 0 [1 2 3 4]",
            "builtins.concatLists [[1 2] [3] [4 5]]",
            "builtins.concatMap (x: [x x]) [1 2 3]",
            "builtins.genList (x: x * x) 5",
            "builtins.genList (x: x) 0",
            "builtins.all (x: x > 0) [1 2 3]",
            "builtins.all (x: x > 0) [1 (-2) 3]",
            "builtins.any (x: x > 2) [1 2 3]",
            "builtins.any (x: x > 5) [1 2 3]",
        ],
    );
}

// ── Attribute set operations ─────────────────────────────────────────

#[test]
fn diff_attrs() {
    run_cases(
        "attrs",
        &[
            "{}",
            "{ a = 1; }",
            "{ a = 1; b = 2; }",
            "{ a = 1; } // { b = 2; }",
            "{ a = 1; } // { a = 2; }",
            "builtins.attrNames { b = 1; a = 2; c = 3; }",
            "builtins.attrValues { b = 1; a = 2; c = 3; }",
            r#"builtins.hasAttr "a" { a = 1; }"#,
            r#"builtins.hasAttr "x" { a = 1; }"#,
            r#"builtins.getAttr "a" { a = 42; }"#,
            "builtins.intersectAttrs { a = 1; b = 2; } { a = 10; c = 20; }",
            r#"builtins.removeAttrs { a = 1; b = 2; c = 3; } [ "b" ]"#,
            "builtins.mapAttrs (n: v: v + 1) { a = 1; b = 2; }",
            r#"builtins.listToAttrs [ { name = "a"; value = 1; } { name = "b"; value = 2; } ]"#,
            r#"builtins.catAttrs "x" [ { x = 1; } { y = 2; } { x = 3; } ]"#,
            "{ a.b.c = 1; }.a.b.c",
            "let x = { a = 1; }; in x.a",
        ],
    );
}

// ── Control flow ─────────────────────────────────────────────────────

#[test]
fn diff_control_flow() {
    run_cases(
        "control_flow",
        &[
            "if true then 1 else 2",
            "if false then 1 else 2",
            "if 1 < 2 then \"a\" else \"b\"",
            "let x = 1; in x",
            "let x = 1; y = x + 1; in y",
            "with { x = 5; }; x * 2",
            "rec { a = 1; b = a + 1; }.b",
            "rec { a = b; b = 3; }.a",
            "let inherit (rec { a = 10; }) a; in a",
        ],
    );
}

// ── Functions ────────────────────────────────────────────────────────

#[test]
fn diff_functions() {
    run_cases(
        "functions",
        &[
            "(x: x + 1) 5",
            "(x: y: x + y) 2 3",
            "({ a, b }: a + b) { a = 1; b = 2; }",
            "({ a, b ? 10 }: a + b) { a = 1; }",
            "({ a, b ? 10 }: a + b) { a = 1; b = 2; }",
            "(args@{ a, b }: a + b + args.a) { a = 1; b = 2; }",
            "let id = x: x; in id 42",
            "let const = x: y: x; in const 1 99",
        ],
    );
}

// ── Type introspection ───────────────────────────────────────────────

#[test]
fn diff_types() {
    run_cases(
        "types",
        &[
            "builtins.typeOf 1",
            "builtins.typeOf 1.5",
            "builtins.typeOf true",
            r#"builtins.typeOf "abc""#,
            "builtins.typeOf null",
            "builtins.typeOf [1 2]",
            "builtins.typeOf { a = 1; }",
            "builtins.typeOf (x: x)",
            "builtins.isInt 1",
            "builtins.isInt 1.5",
            "builtins.isFloat 1.5",
            "builtins.isFloat 1",
            "builtins.isBool true",
            "builtins.isBool 1",
            r#"builtins.isString "abc""#,
            "builtins.isString 1",
            "builtins.isList [1 2]",
            "builtins.isList 1",
            "builtins.isAttrs { a = 1; }",
            "builtins.isAttrs 1",
            "builtins.isFunction (x: x)",
            "builtins.isFunction 1",
            "builtins.isNull null",
            "builtins.isNull 1",
        ],
    );
}

// ── String builtins ──────────────────────────────────────────────────

#[test]
fn diff_string_builtins() {
    // Note: `builtins.hasPrefix` / `builtins.hasSuffix` are *not*
    // core Nix builtins (they live at `lib.strings.hasPrefix` in
    // nixpkgs). sui exposes them as an extension for nixpkgs-lib
    // compatibility but they are not covered by this parity test —
    // they belong to Layer 5 (nixpkgs-lib parity) instead.
    run_cases(
        "string_builtins",
        &[
            r#"builtins.stringLength "abc""#,
            r#"builtins.stringLength """#,
            r#"builtins.substring 0 3 "abcdef""#,
            r#"builtins.substring 2 3 "abcdef""#,
            r#"builtins.substring 0 100 "abc""#,
            r#"builtins.concatStringsSep ", " [ "a" "b" "c" ]"#,
            r#"builtins.concatStringsSep "-" []"#,
            r#"builtins.replaceStrings [ "a" ] [ "X" ] "abcabc""#,
            r#"builtins.replaceStrings [ "a" "b" ] [ "X" "Y" ] "abcabc""#,
            r#"builtins.toString 42"#,
            r#"builtins.toString true"#,
        ],
    );
}

// ── JSON / TOML ──────────────────────────────────────────────────────

#[test]
fn diff_json() {
    run_cases(
        "json",
        &[
            r#"builtins.toJSON 42"#,
            r#"builtins.toJSON "abc""#,
            r#"builtins.toJSON [1 2 3]"#,
            r#"builtins.toJSON { a = 1; b = "two"; }"#,
            r#"builtins.fromJSON "42""#,
            r#"builtins.fromJSON "\"abc\"""#,
            r#"builtins.fromJSON "[1,2,3]""#,
            r#"builtins.fromJSON "{\"a\":1}""#,
        ],
    );
}

// ── Version / parseDrvName ───────────────────────────────────────────

#[test]
fn diff_versions() {
    run_cases(
        "versions",
        &[
            r#"builtins.compareVersions "1.0" "1.1""#,
            r#"builtins.compareVersions "1.1" "1.1""#,
            r#"builtins.compareVersions "2.0" "1.9""#,
            r#"builtins.splitVersion "1.2.3""#,
            r#"builtins.splitVersion "1.2-pre1""#,
            r#"builtins.parseDrvName "hello-1.0""#,
            r#"builtins.parseDrvName "firefox-beta-100.0""#,
        ],
    );
}

// ── Integer bitwise + comparisons ────────────────────────────────────

#[test]
fn diff_bitwise_and_lessthan() {
    run_cases(
        "bitwise",
        &[
            "builtins.bitAnd 12 10",
            "builtins.bitOr 12 10",
            "builtins.bitXor 12 10",
            "builtins.lessThan 1 2",
            "builtins.lessThan 2 1",
            "builtins.lessThan 2 2",
        ],
    );
}

// ── Math builtins ────────────────────────────────────────────────────

#[test]
fn diff_math() {
    run_cases(
        "math",
        &[
            "builtins.ceil 1.2",
            "builtins.ceil 1.0",
            "builtins.ceil (-1.5)",
            "builtins.floor 1.8",
            "builtins.floor 1.0",
            "builtins.floor (-1.2)",
        ],
    );
}

// ── tryEval ──────────────────────────────────────────────────────────

#[test]
fn diff_try_eval() {
    run_cases(
        "try_eval",
        &[
            r#"(builtins.tryEval (throw "boom")).success"#,
            "(builtins.tryEval 42).value",
            "(builtins.tryEval 42).success",
        ],
    );
}

// ── genericClosure ───────────────────────────────────────────────────

#[test]
fn diff_generic_closure() {
    run_cases(
        "generic_closure",
        &[
            r#"builtins.genericClosure { startSet = [ { key = 1; } ]; operator = x: []; }"#,
            r#"builtins.genericClosure {
                startSet = [ { key = 1; } { key = 2; } ];
                operator = x: [];
              }"#,
        ],
    );
}

// ── debugging / introspection builtins ──────────────────────────────

#[test]
fn diff_warn_passthrough() {
    run_cases(
        "warn_passthrough",
        &[
            r#"builtins.warn "msg" 42"#,
            r#"builtins.warn "msg" "value""#,
            r#"builtins.warn "msg" [1 2 3]"#,
        ],
    );
}

#[test]
fn diff_trace_verbose_passthrough() {
    run_cases(
        "trace_verbose_passthrough",
        &[
            r#"builtins.traceVerbose "msg" 42"#,
            r#"builtins.traceVerbose "msg" { a = 1; }"#,
        ],
    );
}

// builtins.break is interactive-only in CppNix and crashes the
// `nix-instantiate` process under Determinate Nix 3.17 even when the
// argument is a finite literal, so it cannot be diff'd against the
// oracle. The unit tests in builtins.rs cover the sui semantics
// (passthrough) directly.

#[test]
fn diff_parse_flake_ref() {
    run_cases(
        "parse_flake_ref",
        &[
            r#"builtins.parseFlakeRef "github:NixOS/nixpkgs""#,
            r#"builtins.parseFlakeRef "github:NixOS/nixpkgs/release-23.11""#,
            r#"builtins.parseFlakeRef "github:NixOS/nixpkgs?dir=lib""#,
            r#"builtins.parseFlakeRef "git+https://example.com/foo""#,
            r#"builtins.parseFlakeRef "git+https://example.com/foo?ref=main""#,
            r#"builtins.parseFlakeRef "tarball+https://example.com/foo.tar.gz""#,
            r#"builtins.parseFlakeRef "path:/tmp/foo""#,
            r#"builtins.parseFlakeRef "/tmp/abs""#,
            r#"builtins.parseFlakeRef "gitlab:owner/repo""#,
            r#"builtins.parseFlakeRef "sourcehut:~user/repo""#,
        ],
    );
}

#[test]
fn diff_flake_ref_to_string() {
    run_cases(
        "flake_ref_to_string",
        &[
            r#"builtins.flakeRefToString { type = "github"; owner = "NixOS"; repo = "nixpkgs"; }"#,
            r#"builtins.flakeRefToString { type = "github"; owner = "NixOS"; repo = "nixpkgs"; ref = "release-23.11"; }"#,
            r#"builtins.flakeRefToString { type = "github"; owner = "NixOS"; repo = "nixpkgs"; ref = "main"; dir = "lib"; }"#,
            r#"builtins.flakeRefToString { type = "git"; url = "https://example.com/foo"; ref = "main"; }"#,
            r#"builtins.flakeRefToString { type = "tarball"; url = "https://example.com/foo.tar.gz"; }"#,
            r#"builtins.flakeRefToString { type = "path"; path = "/tmp/foo"; }"#,
        ],
    );
}

#[test]
fn diff_flake_ref_round_trip() {
    run_cases(
        "flake_ref_round_trip",
        &[
            r#"builtins.flakeRefToString (builtins.parseFlakeRef "github:NixOS/nixpkgs")"#,
            r#"builtins.flakeRefToString (builtins.parseFlakeRef "github:NixOS/nixpkgs/release-23.11")"#,
            r#"builtins.flakeRefToString (builtins.parseFlakeRef "git+https://example.com/foo?ref=main")"#,
            r#"builtins.flakeRefToString (builtins.parseFlakeRef "path:/tmp/foo")"#,
        ],
    );
}

#[test]
fn diff_filter_attrs() {
    run_cases(
        "filter_attrs",
        &[
            r#"builtins.filterAttrs (n: v: v > 1) { a = 1; b = 2; c = 3; }"#,
            r#"builtins.filterAttrs (n: v: n == "keep") { keep = 1; drop = 2; }"#,
            r#"builtins.filterAttrs (n: v: true) {}"#,
            r#"builtins.filterAttrs (n: v: false) { a = 1; b = 2; }"#,
        ],
    );
}

#[test]
fn diff_builtins_self_reference() {
    run_cases(
        "builtins_self_reference",
        &[
            "builtins ? builtins",
            "builtins.builtins ? typeOf",
            "builtins.builtins ? attrNames",
        ],
    );
}
