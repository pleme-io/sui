//! Parity tests: verify the bytecode VM produces identical results
//! to the tree-walking evaluator for every supported expression.
//!
//! These tests are the primary correctness guarantee: they compile and
//! execute an expression via both backends and assert the results match.
//!
//! Comparison is done via [`StringKeyedValue`] which resolves all
//! interned `Symbol` keys back to strings for structural equality.

use sui_bytecode::StringKeyedValue;

/// Convert a tree-walker `Value` to a `StringKeyedValue` for comparison.
///
/// This only handles the value types we can currently produce in Phase 1.
fn tree_to_skv(val: &sui_eval::Value) -> StringKeyedValue {
    match val {
        sui_eval::Value::Null => StringKeyedValue::Null,
        sui_eval::Value::Bool(b) => StringKeyedValue::Bool(*b),
        sui_eval::Value::Int(n) => StringKeyedValue::Int(*n),
        sui_eval::Value::Float(f) => StringKeyedValue::Float(*f),
        sui_eval::Value::String(s) => StringKeyedValue::String(s.chars.clone()),
        sui_eval::Value::Path(p) => StringKeyedValue::Path(p.clone()),
        sui_eval::Value::List(items) => {
            StringKeyedValue::List(items.iter().map(|v| tree_to_skv(v)).collect())
        }
        sui_eval::Value::Attrs(attrs) => {
            let map = attrs
                .iter()
                .map(|(k, v)| (k.clone(), tree_to_skv(v)))
                .collect();
            StringKeyedValue::Attrs(map)
        }
        sui_eval::Value::Lambda(_) | sui_eval::Value::Builtin(_) => {
            // Functions can't be compared structurally; skip.
            StringKeyedValue::Lambda
        }
        sui_eval::Value::Thunk(thunk) => {
            // Force the thunk for comparison.
            match thunk.force(&|e, env| sui_eval::eval::eval_expr(e, env)) {
                Ok(v) => tree_to_skv(&v),
                Err(_) => StringKeyedValue::Null,
            }
        }
    }
}

/// Assert that both evaluation backends produce the same result.
///
/// Panics with a clear message showing both results on mismatch.
fn assert_same(expr: &str) {
    let tree_result = sui_eval::eval(expr)
        .unwrap_or_else(|e| panic!("tree-walker failed for '{expr}': {e}"));
    let tree_as_skv = tree_to_skv(&tree_result);

    let bc_result = sui_bytecode::eval_full(expr)
        .unwrap_or_else(|e| panic!("bytecode VM failed for '{expr}': {e}"));
    let bc_as_skv = bc_result.to_string_keyed();

    assert_eq!(
        tree_as_skv, bc_as_skv,
        "parity mismatch for: {expr}\n  tree-walker => {tree_as_skv:?}\n  bytecode VM => {bc_as_skv:?}"
    );
}

// ── Integer arithmetic ─────────────────────────────────────────

#[test]
fn parity_int_addition() {
    assert_same("1 + 2");
}

#[test]
fn parity_int_subtraction() {
    assert_same("10 - 3");
}

#[test]
fn parity_int_multiplication() {
    assert_same("3 * 4");
}

#[test]
fn parity_int_division() {
    assert_same("10 / 3");
}

#[test]
fn parity_compound_arithmetic() {
    assert_same("2 * 3 + 1");
}

#[test]
fn parity_nested_arithmetic() {
    assert_same("(1 + 2) * (3 + 4)");
}

#[test]
fn parity_negative_integer() {
    assert_same("-42");
}

// ── Float arithmetic ───────────────────────────────────────────

#[test]
fn parity_float_literal() {
    assert_same("3.14");
}

#[test]
fn parity_float_addition() {
    assert_same("1.5 + 2.5");
}

#[test]
fn parity_mixed_int_float() {
    assert_same("1 + 2.0");
}

#[test]
fn parity_negate_float() {
    assert_same("-3.14");
}

// ── Booleans and logic ─────────────────────────────────────────

#[test]
fn parity_bool_true() {
    assert_same("true");
}

#[test]
fn parity_bool_false() {
    assert_same("false");
}

#[test]
fn parity_not() {
    assert_same("!true");
    assert_same("!false");
}

#[test]
fn parity_and() {
    assert_same("true && true");
    assert_same("true && false");
    assert_same("false && true");
    assert_same("false && false");
}

#[test]
fn parity_or() {
    assert_same("true || true");
    assert_same("true || false");
    assert_same("false || true");
    assert_same("false || false");
}

#[test]
fn parity_implication() {
    assert_same("true -> true");
    assert_same("true -> false");
    assert_same("false -> true");
    assert_same("false -> false");
}

// ── Comparison ─────────────────────────────────────────────────

#[test]
fn parity_equal() {
    assert_same("1 == 1");
    assert_same("1 == 2");
}

#[test]
fn parity_not_equal() {
    assert_same("1 != 2");
    assert_same("1 != 1");
}

#[test]
fn parity_less() {
    assert_same("1 < 2");
    assert_same("2 < 1");
    assert_same("1 < 1");
}

#[test]
fn parity_greater() {
    assert_same("2 > 1");
    assert_same("1 > 2");
    assert_same("1 > 1");
}

#[test]
fn parity_less_equal() {
    assert_same("1 <= 2");
    assert_same("2 <= 1");
    assert_same("1 <= 1");
}

#[test]
fn parity_greater_equal() {
    assert_same("2 >= 1");
    assert_same("1 >= 2");
    assert_same("1 >= 1");
}

#[test]
fn parity_string_comparison() {
    assert_same(r#""abc" < "abd""#);
    assert_same(r#""abc" == "abc""#);
}

// ── Null ───────────────────────────────────────────────────────

#[test]
fn parity_null() {
    assert_same("null");
}

#[test]
fn parity_null_equality() {
    assert_same("null == null");
}

// ── Strings ────────────────────────────────────────────────────

#[test]
fn parity_string_literal() {
    assert_same(r#""hello world""#);
}

#[test]
fn parity_empty_string() {
    assert_same(r#""""#);
}

#[test]
fn parity_string_addition() {
    assert_same(r#""hello" + " " + "world""#);
}

#[test]
fn parity_string_interpolation() {
    assert_same(r#"let x = "world"; in "hello ${x}""#);
}

// ── Conditionals ───────────────────────────────────────────────

#[test]
fn parity_if_true() {
    assert_same("if true then 1 else 2");
}

#[test]
fn parity_if_false() {
    assert_same("if false then 1 else 2");
}

#[test]
fn parity_if_expression() {
    assert_same(r#"if 1 > 2 then "yes" else "no""#);
}

#[test]
fn parity_nested_if() {
    assert_same("if true then (if false then 1 else 2) else 3");
}

// ── Let/in ─────────────────────────────────────────────────────

#[test]
fn parity_let_simple() {
    assert_same("let x = 1; y = 2; in x + y");
}

#[test]
fn parity_let_nested() {
    assert_same("let a = 10; in let b = 20; in a + b");
}

#[test]
fn parity_let_shadow() {
    assert_same("let x = 1; in let x = 2; in x");
}

#[test]
fn parity_let_with_expression() {
    assert_same("let x = 2 * 3; in x + 1");
}

#[test]
fn parity_let_chain() {
    assert_same("let a = 1; b = 1; c = a + b; d = b + c; e = c + d; in e");
}

// ── Functions ──────────────────────────────────────────────────

#[test]
fn parity_identity() {
    assert_same("(x: x) 42");
}

#[test]
fn parity_lambda_arithmetic() {
    assert_same("(x: x + 1) 5");
}

#[test]
fn parity_let_lambda() {
    assert_same("let f = x: x * 2; in f 5");
}

#[test]
fn parity_pattern_lambda() {
    assert_same("({ a, b }: a + b) { a = 3; b = 4; }");
}

#[test]
fn parity_pattern_lambda_default() {
    assert_same("({ a, b ? 10 }: a + b) { a = 5; }");
}

#[test]
fn parity_lambda_composition() {
    assert_same("let inc = x: x + 1; double = x: x * 2; in double (inc 3)");
}

// ── Lists ──────────────────────────────────────────────────────

#[test]
fn parity_empty_list() {
    assert_same("[]");
}

#[test]
fn parity_list() {
    assert_same("[1 2 3]");
}

#[test]
fn parity_list_concat() {
    assert_same("[1 2] ++ [3 4]");
}

#[test]
fn parity_list_mixed() {
    assert_same(r#"[1 "hello" true null]"#);
}

// ── Attribute sets ─────────────────────────────────────────────

#[test]
fn parity_empty_attrset() {
    assert_same("{ }");
}

#[test]
fn parity_attrset() {
    assert_same("{ a = 1; b = 2; }");
}

#[test]
fn parity_attrset_select() {
    assert_same("{ a = 1; b = 2; }.a");
}

#[test]
fn parity_nested_attrset_select() {
    assert_same("{ a = { b = 42; }; }.a.b");
}

#[test]
fn parity_attrset_update() {
    assert_same("{ a = 1; } // { b = 2; }");
}

#[test]
fn parity_attrset_update_override() {
    assert_same("({ a = 1; } // { a = 2; }).a");
}

#[test]
fn parity_has_attr_true() {
    assert_same("{ a = 1; } ? a");
}

#[test]
fn parity_has_attr_false() {
    assert_same("{ a = 1; } ? b");
}

#[test]
fn parity_select_or_default() {
    assert_same("{ a = 1; }.b or 0");
    assert_same("{ a = 1; }.a or 0");
}

// ── Assert ─────────────────────────────────────────────────────

#[test]
fn parity_assert_pass() {
    assert_same("assert true; 42");
}

#[test]
fn parity_assert_with_expression() {
    assert_same("assert 1 < 2; 42");
}

// ── Complex expressions ────────────────────────────────────────

#[test]
fn parity_let_with_attrset() {
    assert_same("let set = { x = 10; y = 20; }; in set.x + set.y");
}

#[test]
fn parity_conditional_attrset() {
    assert_same("(if true then { a = 1; } else { a = 2; }).a");
}

#[test]
fn parity_lambda_returning_attrset() {
    assert_same("(x: { result = x * 2; }) 5");
}

#[test]
fn parity_lambda_with_conditional() {
    assert_same("(x: if x > 0 then x else 0 - x) (-5)");
}

#[test]
fn parity_list_in_attrset() {
    assert_same("{ items = [1 2 3]; }");
}

#[test]
fn parity_attrset_in_list() {
    assert_same("[{ a = 1; } { b = 2; }]");
}

// ── Error parity ───────────────────────────────────────────────

#[test]
fn parity_div_zero_is_error() {
    let tree = sui_eval::eval("1 / 0");
    let bc = sui_bytecode::eval("1 / 0");
    assert!(tree.is_err(), "tree-walker should error on div by zero");
    assert!(bc.is_err(), "bytecode VM should error on div by zero");
}

#[test]
fn parity_assert_fail_is_error() {
    let tree = sui_eval::eval("assert false; 42");
    let bc = sui_bytecode::eval("assert false; 42");
    assert!(tree.is_err(), "tree-walker should error on assert false");
    assert!(bc.is_err(), "bytecode VM should error on assert false");
}

#[test]
fn parity_attr_not_found_is_error() {
    let tree = sui_eval::eval("{ a = 1; }.b");
    let bc = sui_bytecode::eval("{ a = 1; }.b");
    assert!(tree.is_err(), "tree-walker should error on missing attr");
    assert!(bc.is_err(), "bytecode VM should error on missing attr");
}

// ── Upvalues (closures) ───────────────────────────────────────

#[test]
fn parity_upvalue_basic() {
    assert_same("let f = let x = 10; in y: x + y; in f 5");
}

#[test]
fn parity_upvalue_curried() {
    assert_same("let add = a: b: a + b; in add 3 4");
}

#[test]
fn parity_upvalue_nested_closure() {
    assert_same("let x = 1; f = y: z: x + y + z; in f 2 3");
}

#[test]
fn parity_upvalue_shared() {
    assert_same("let x = 10; f = a: x + a; g = b: x * b; in f 1 + g 2");
}

#[test]
fn parity_upvalue_let_closure() {
    assert_same("let x = 5; in (y: x + y) 10");
}

#[test]
fn parity_upvalue_deep_nesting() {
    assert_same("let a = 1; in let b = 2; in let c = 3; in a + b + c");
}

#[test]
fn parity_upvalue_lambda_returning_lambda() {
    assert_same("let f = x: y: x + y; g = f 10; in g 20");
}

// ── With expressions ──────────────────────────────────────────

#[test]
fn parity_with_basic() {
    assert_same("with { x = 1; }; x");
}

#[test]
fn parity_with_nested_inner_wins() {
    assert_same("with { x = 1; }; with { x = 2; }; x");
}

#[test]
fn parity_with_let_shadows() {
    assert_same("with { x = 1; }; let x = 2; in x");
}

#[test]
fn parity_with_multiple_attrs() {
    assert_same("with { x = 1; y = 2; }; x + y");
}

#[test]
fn parity_with_outer_fallback() {
    assert_same("with { x = 1; }; with { y = 2; }; x + y");
}

#[test]
fn parity_with_set_expr() {
    assert_same("let s = { a = 10; b = 20; }; in with s; a + b");
}

// ── Rec attrsets ──────────────────────────────────────────────

#[test]
fn parity_rec_basic() {
    assert_same("rec { a = 1; b = a + 1; }.b");
}

#[test]
fn parity_rec_mutual() {
    assert_same("rec { a = 1; b = a + 1; c = b + 1; }.c");
}

#[test]
fn parity_rec_full_set() {
    assert_same("rec { a = 1; b = a + 1; }");
}

// ── Inherit ───────────────────────────────────────────────────

#[test]
fn parity_inherit_in_let() {
    assert_same("let x = 1; in let inherit x; in x");
}

#[test]
fn parity_inherit_from_in_let() {
    assert_same("let inherit ({ x = 42; }) x; in x");
}

#[test]
fn parity_inherit_in_attrset() {
    assert_same("let x = 1; in { inherit x; }");
}

#[test]
fn parity_inherit_from_in_attrset() {
    assert_same("{ inherit ({ x = 1; y = 2; }) x y; }");
}

#[test]
fn parity_inherit_from_multiple() {
    assert_same("let src = { a = 10; b = 20; }; in { inherit (src) a b; }");
}

#[test]
fn parity_rec_inherit() {
    assert_same("let x = 100; in rec { inherit x; y = x + 1; }.y");
}

#[test]
fn parity_rec_inherit_from() {
    assert_same("rec { inherit ({ x = 5; }) x; y = x + 1; }.y");
}

// ── Dotted bindings ───────────────────────────────────────────

#[test]
fn parity_dotted_basic() {
    assert_same("{ a.b = 1; }");
}

#[test]
fn parity_dotted_merge() {
    assert_same("{ a.b = 1; a.c = 2; }");
}

#[test]
fn parity_dotted_deep() {
    assert_same("{ a.b.c = 1; }");
}

#[test]
fn parity_dotted_select() {
    assert_same("{ a.b = 1; a.c = 2; }.a.b");
}

#[test]
fn parity_dotted_select_deep() {
    assert_same("{ a.b.c = 42; }.a.b.c");
}

// ── Dynamic attribute keys ────────────────────────────────────

#[test]
fn parity_dynamic_attr_basic() {
    assert_same(r#"let name = "x"; in { ${name} = 1; }.x"#);
}

#[test]
fn parity_dynamic_attr_interpolation() {
    assert_same(r#"let k = "hello"; in { ${k} = 42; }"#);
}

// ── Fixpoint / lazy let bindings ─────────────────────────────

#[test]
fn parity_lazy_let_unused_binding() {
    // Unused non-trivial binding should not error.
    assert_same("let x = 1 + 2; y = 1; in y");
}

#[test]
fn parity_lazy_let_cross_ref() {
    // Let-binding thunks can reference other bindings.
    assert_same("let f = x: x + 1; g = f 10; in g");
}

#[test]
fn parity_fixpoint_via_intermediate() {
    // Fixpoint pattern accessed through intermediate bindings.
    assert_same("let fix = f: let x = f x; in x; r = fix (self: { a = 1; }); s = r.a; in s");
}

// NOTE: Mutual recursion in let-blocks requires open upvalues (not yet implemented).

// ── Higher-order builtins (closure calling) ──────────────────

#[test]
fn parity_map_basic() {
    assert_same("builtins.map (x: x + 1) [1 2 3]");
}

#[test]
fn parity_map_identity() {
    assert_same("builtins.map (x: x) [1 2 3]");
}

#[test]
fn parity_map_empty() {
    assert_same("builtins.map (x: x + 1) []");
}

#[test]
fn parity_map_strings() {
    assert_same(r#"builtins.map (x: x + "!") ["a" "b" "c"]"#);
}

#[test]
fn parity_filter_basic() {
    assert_same("builtins.filter (x: x > 2) [1 2 3 4 5]");
}

#[test]
fn parity_filter_all_true() {
    assert_same("builtins.filter (x: true) [1 2 3]");
}

#[test]
fn parity_filter_all_false() {
    assert_same("builtins.filter (x: false) [1 2 3]");
}

#[test]
fn parity_filter_empty() {
    assert_same("builtins.filter (x: x > 0) []");
}

#[test]
fn parity_foldl_sum() {
    assert_same("builtins.foldl' (acc: x: acc + x) 0 [1 2 3]");
}

#[test]
fn parity_foldl_product() {
    assert_same("builtins.foldl' (acc: x: acc * x) 1 [1 2 3 4]");
}

#[test]
fn parity_foldl_empty() {
    assert_same("builtins.foldl' (acc: x: acc + x) 42 []");
}

#[test]
fn parity_sort_basic() {
    assert_same("builtins.sort (a: b: a < b) [3 1 2]");
}

#[test]
fn parity_sort_descending() {
    assert_same("builtins.sort (a: b: a > b) [3 1 2]");
}

#[test]
fn parity_sort_empty() {
    assert_same("builtins.sort (a: b: a < b) []");
}

#[test]
fn parity_sort_single() {
    assert_same("builtins.sort (a: b: a < b) [42]");
}

#[test]
fn parity_genlist_basic() {
    assert_same("builtins.genList (i: i * 2) 5");
}

#[test]
fn parity_genlist_zero() {
    assert_same("builtins.genList (i: i) 0");
}

#[test]
fn parity_genlist_identity() {
    assert_same("builtins.genList (i: i) 4");
}

#[test]
fn parity_concatmap_basic() {
    assert_same("builtins.concatMap (x: [x (x+1)]) [1 2 3]");
}

#[test]
fn parity_concatmap_empty_results() {
    assert_same("builtins.concatMap (x: []) [1 2 3]");
}

#[test]
fn parity_concatmap_singleton() {
    assert_same("builtins.concatMap (x: [x]) [1 2 3]");
}

#[test]
fn parity_any_true() {
    assert_same("builtins.any (x: x > 3) [1 2 3 4]");
}

#[test]
fn parity_any_false() {
    assert_same("builtins.any (x: x > 10) [1 2 3]");
}

#[test]
fn parity_any_empty() {
    assert_same("builtins.any (x: true) []");
}

#[test]
fn parity_all_true() {
    assert_same("builtins.all (x: x > 0) [1 2 3]");
}

#[test]
fn parity_all_false() {
    assert_same("builtins.all (x: x > 2) [1 2 3]");
}

#[test]
fn parity_all_empty() {
    assert_same("builtins.all (x: false) []");
}

#[test]
fn parity_partition_basic() {
    assert_same("builtins.partition (x: x > 2) [1 2 3 4 5]");
}

#[test]
fn parity_partition_all_right() {
    assert_same("builtins.partition (x: true) [1 2 3]");
}

#[test]
fn parity_partition_all_wrong() {
    assert_same("builtins.partition (x: false) [1 2 3]");
}

#[test]
fn parity_groupby_basic() {
    assert_same(r#"builtins.groupBy (x: if x > 2 then "big" else "small") [1 2 3 4]"#);
}

#[test]
fn parity_map_square() {
    assert_same("builtins.map (x: x * x) [1 2 3 4]");
}

#[test]
fn parity_filter_even() {
    assert_same("builtins.filter (x: x - (x / 2) * 2 == 0) [1 2 3 4 5 6]");
}

#[test]
fn parity_foldl_string_concat() {
    assert_same(r#"builtins.foldl' (acc: x: acc + x) "" ["a" "b" "c"]"#);
}
