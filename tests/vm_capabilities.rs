//! Documents the bytecode VM's current capabilities and known limitations.
//!
//! These tests exercise the VM (default backend) on increasingly complex
//! Nix patterns. Tests that exercise features not yet supported by the VM
//! are marked `#[ignore]` with a reason.

use assert_cmd::Command;

fn eval_json(expr: &str) -> serde_json::Value {
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["eval", "--json", expr])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("JSON parse failed for {expr:?}: {e}\n{}", stdout.trim()))
}

fn eval_fails(expr: &str) -> String {
    let assert = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["eval", expr])
        .assert()
        .failure();
    String::from_utf8_lossy(&assert.get_output().stderr).to_string()
}

// ── Core language features (all pass) ─────────────────────────

#[test]
fn vm_rec_attrset() {
    assert_eq!(
        eval_json("rec { x = 1; y = x + 1; z = y * 2; }.z"),
        serde_json::json!(4)
    );
}

#[test]
fn vm_inherit() {
    assert_eq!(
        eval_json("let a = 1; b = 2; in { inherit a b; }.a"),
        serde_json::json!(1)
    );
}

#[test]
fn vm_with_scope() {
    assert_eq!(
        eval_json("with { x = 10; y = 20; }; x + y"),
        serde_json::json!(30)
    );
}

#[test]
fn vm_pattern_destructuring_with_defaults() {
    assert_eq!(
        eval_json("({ a, b ? 100 }: a + b) { a = 1; }"),
        serde_json::json!(101)
    );
}

#[test]
fn vm_string_interpolation() {
    assert_eq!(
        eval_json(r#"let name = "world"; in "hello ${name}""#),
        serde_json::json!("hello world")
    );
}

#[test]
fn vm_fixpoint_pattern() {
    // This is the core pattern used by nixpkgs' lib.makeExtensible.
    assert_eq!(
        eval_json(r#"
            let makeExtensible = f: let self = f self; in self;
                lib = makeExtensible (self: { version = "1.0"; });
            in lib.version
        "#),
        serde_json::json!("1.0")
    );
}

#[test]
fn vm_nested_attrset_access() {
    assert_eq!(
        eval_json("{ a = { b = { c = 42; }; }; }.a.b.c"),
        serde_json::json!(42)
    );
}

#[test]
fn vm_curried_functions() {
    assert_eq!(
        eval_json("let add = x: y: x + y; in add 3 4"),
        serde_json::json!(7)
    );
}

#[test]
fn vm_higher_order_pattern() {
    assert_eq!(
        eval_json("let apply = f: x: f x; double = x: x * 2; in apply double 21"),
        serde_json::json!(42)
    );
}

#[test]
fn vm_assert_with_message() {
    assert_eq!(
        eval_json("assert 1 == 1; 42"),
        serde_json::json!(42)
    );
}

#[test]
fn vm_assert_failure() {
    let err = eval_fails("assert false; 42");
    assert!(err.contains("assert") || err.contains("failed"), "expected assertion error, got: {err}");
}

#[test]
fn vm_import_file() {
    // Write a temp file and import it.
    let dir = std::env::temp_dir().join("sui-vm-cap-test");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("test.nix");
    std::fs::write(&path, "{ x = 42; }").unwrap();
    let expr = format!("(import {}).x", path.display());
    assert_eq!(eval_json(&expr), serde_json::json!(42));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

// ── Builtin coverage ──────────────────────────────────────────

#[test]
fn vm_builtins_string_ops() {
    assert_eq!(
        eval_json(r#"builtins.stringLength "hello""#),
        serde_json::json!(5)
    );
    assert_eq!(
        eval_json(r#"builtins.substring 1 3 "hello""#),
        serde_json::json!("ell")
    );
    assert_eq!(
        eval_json(r#"builtins.replaceStrings ["o"] ["0"] "foo""#),
        serde_json::json!("f00")
    );
}

#[test]
fn vm_builtins_list_ops() {
    assert_eq!(eval_json("builtins.head [10 20 30]"), serde_json::json!(10));
    assert_eq!(eval_json("builtins.tail [1 2 3]"), serde_json::json!([2, 3]));
    assert_eq!(eval_json("builtins.length [1 2 3 4]"), serde_json::json!(4));
    assert_eq!(eval_json("builtins.elemAt [10 20 30] 2"), serde_json::json!(30));
    assert_eq!(eval_json("builtins.elem 2 [1 2 3]"), serde_json::json!(true));
}

#[test]
fn vm_builtins_arithmetic() {
    assert_eq!(eval_json("builtins.add 3 4"), serde_json::json!(7));
    assert_eq!(eval_json("builtins.sub 10 3"), serde_json::json!(7));
    assert_eq!(eval_json("builtins.mul 6 7"), serde_json::json!(42));
    assert_eq!(eval_json("builtins.div 42 6"), serde_json::json!(7));
}

#[test]
fn vm_builtins_type_checks() {
    assert_eq!(eval_json("builtins.isInt 42"), serde_json::json!(true));
    assert_eq!(eval_json(r#"builtins.isString "hi""#), serde_json::json!(true));
    assert_eq!(eval_json("builtins.isList [1]"), serde_json::json!(true));
    assert_eq!(eval_json("builtins.isAttrs { }"), serde_json::json!(true));
    assert_eq!(eval_json("builtins.isFunction (x: x)"), serde_json::json!(true));
    assert_eq!(eval_json("builtins.isBool true"), serde_json::json!(true));
    assert_eq!(eval_json("builtins.isNull null"), serde_json::json!(true));
}

#[test]
fn vm_builtins_json() {
    assert_eq!(eval_json(r#"builtins.fromJSON "42""#), serde_json::json!(42));
    assert_eq!(eval_json(r#"builtins.fromJSON "true""#), serde_json::json!(true));
}

// ── Known limitations (not yet implemented in VM) ─────────────

#[test]
#[ignore = "VM does not implement builtins.getFlake"]
fn vm_flake_eval() {
    eval_json(r#"(builtins.getFlake "path:.").outputs"#);
}

#[test]
#[ignore = "VM does not implement builtins.map (higher-order closure invocation)"]
fn vm_builtins_map() {
    assert_eq!(
        eval_json("builtins.map (x: x * 2) [1 2 3]"),
        serde_json::json!([2, 4, 6])
    );
}

#[test]
#[ignore = "VM does not implement builtins.filter"]
fn vm_builtins_filter() {
    assert_eq!(
        eval_json("builtins.filter (x: x > 2) [1 2 3 4 5]"),
        serde_json::json!([3, 4, 5])
    );
}

#[test]
#[ignore = "VM does not implement builtins.foldl'"]
fn vm_builtins_foldl() {
    assert_eq!(
        eval_json("builtins.foldl' (acc: x: acc + x) 0 [1 2 3 4 5]"),
        serde_json::json!(15)
    );
}

#[test]
#[ignore = "VM does not implement derivation/derivationStrict"]
fn vm_derivation() {
    eval_json(r#"derivation { name = "test"; system = "x86_64-linux"; builder = "/bin/sh"; }"#);
}

#[test]
#[ignore = "VM does not implement scopedImport"]
fn vm_scoped_import() {
    eval_json("scopedImport {} /dev/null");
}
