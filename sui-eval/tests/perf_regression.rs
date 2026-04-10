//! Performance regression tests for the sui Nix evaluator.
//!
//! These tests evaluate Nix expressions and assert they complete within
//! a generous time budget (5-10x current speed). They exist to catch
//! severe regressions, not micro-fluctuations.
//!
//! Run: `cargo test -p sui-eval --test perf_regression`

use std::time::Instant;

fn eval(expr: &str) -> String {
    let result = sui_eval::eval(expr).expect("eval failed");
    sui_eval::eval::force_value(&result)
        .expect("force failed")
        .to_string()
}

fn eval_timed(expr: &str, budget_ms: u64) -> String {
    let start = Instant::now();
    let result = eval(expr);
    let elapsed = start.elapsed().as_millis() as u64;
    assert!(
        elapsed < budget_ms,
        "Performance regression: '{}' took {}ms (budget: {}ms)",
        &expr[..expr.len().min(60)],
        elapsed,
        budget_ms,
    );
    result
}

#[test]
fn perf_arithmetic() {
    eval_timed("1 + 2 * 3", 500); // Should be <50ms
}

#[test]
fn perf_lambda() {
    eval_timed("(x: x + 1) 42", 500);
}

#[test]
fn perf_curried() {
    eval_timed("(a: b: a + b) 1 2", 500);
}

#[test]
fn perf_pattern_destructure() {
    eval_timed("({a, b}: a + b) { a = 1; b = 2; }", 500);
}

#[test]
fn perf_recursion_100() {
    eval_timed(
        "let f = n: if n <= 0 then 0 else n + f (n - 1); in f 100",
        500,
    );
}

#[test]
fn perf_attrset_construction() {
    eval_timed("{ a = 1; b = 2; c = 3; }", 500);
}

#[test]
fn perf_attrset_select() {
    eval_timed("{ a = 1; b = 2; }.b", 500);
}

#[test]
fn perf_attrset_update() {
    eval_timed("{ a = 1; } // { b = 2; }", 500);
}

#[test]
fn perf_recursive_attrset() {
    eval_timed("rec { a = 1; b = a + 1; c = b + 1; }.c", 500);
}

#[test]
fn perf_50_key_attrset() {
    eval_timed(
        r#"let s = builtins.listToAttrs (builtins.genList (i: { name = "k${toString i}"; value = i; }) 50); in builtins.length (builtins.attrNames s)"#,
        500,
    );
}

#[test]
fn perf_list_operations() {
    eval_timed("builtins.map (x: x * 2) [1 2 3 4 5]", 500);
    eval_timed("builtins.filter (x: x > 2) [1 2 3 4 5]", 500);
    eval_timed("builtins.sort builtins.lessThan [5 3 1 4 2]", 500);
    eval_timed("builtins.genList (i: i * i) 100", 500);
    eval_timed("builtins.foldl' (a: b: a + b) 0 [1 2 3 4 5]", 500);
}

#[test]
fn perf_string_operations() {
    eval_timed(r#"let x = "world"; in "hello ${x}""#, 500);
    eval_timed(r#"builtins.stringLength "hello world""#, 500);
    eval_timed(
        r#"builtins.replaceStrings ["o"] ["0"] "hello world""#,
        500,
    );
    eval_timed(r#"builtins.concatStringsSep ", " ["a" "b" "c"]"#, 500);
}

#[test]
fn perf_let_scoping() {
    eval_timed("let x = 1; in x + 1", 500);
    eval_timed("let a = 1; in let b = a + 1; in let c = b + 1; in c", 500);
    eval_timed(
        "let a=1;b=2;c=3;d=4;e=5;f=6;g=7;h=8;i=9;j=10; in a+b+c+d+e+f+g+h+i+j",
        500,
    );
}

#[test]
fn perf_with_expression() {
    eval_timed("with { x = 42; }; x", 500);
    eval_timed("let x = 10; in with { x = 1; }; x", 500);
}

#[test]
fn perf_type_checking() {
    eval_timed("builtins.typeOf 42", 500);
    eval_timed(r#"builtins.typeOf "hello""#, 500);
    eval_timed("builtins.typeOf { x = 1; }", 500);
    eval_timed("builtins.isInt 42", 500);
}

#[test]
fn perf_json() {
    eval_timed(
        r#"builtins.toJSON { a = 1; b = [1 2]; c = "hi"; }"#,
        500,
    );
    eval_timed(r#"builtins.fromJSON "{\"a\":1,\"b\":2}""#, 500);
}

#[test]
fn perf_error_handling() {
    eval_timed("builtins.tryEval 42", 500);
    eval_timed("assert true; 42", 500);
}

#[test]
fn perf_fixpoint_micro() {
    // Micro fixpoint: sui should be faster than CppNix (~3x)
    eval_timed(
        r#"
let
  fix = f: let x = f x; in x;
  extends = f: rattrs: self: let super = rattrs self; in super // f self super;
  base = self: { a = 1; b = 2; c = self.a + 1; };
  overlay1 = self: super: { d = 4; };
  overlay2 = self: super: { e = 5; };
  composed = builtins.foldl' (acc: ov: extends ov acc) base [overlay1 overlay2];
in (fix composed).c
"#,
        500,
    );
}

#[test]
fn perf_fixpoint_scaled() {
    // Scaled fixpoint: 20 overlays x 100 attrs
    eval_timed(
        r#"
let
  fix = f: let x = f x; in x;
  extends = f: rattrs: self: let super = rattrs self; in super // f self super;
  mkOverlay = n: self: super: builtins.listToAttrs (builtins.genList (i: { name = "pkg_${toString n}_${toString i}"; value = i + n; }) 100);
  base = self: builtins.listToAttrs (builtins.genList (i: { name = "base_${toString i}"; value = i; }) 100);
  overlays = builtins.genList mkOverlay 20;
  composed = builtins.foldl' (acc: ov: extends ov acc) base overlays;
in builtins.length (builtins.attrNames (fix composed))
"#,
        2000, // 2s budget -- currently takes ~23ms
    );
}
