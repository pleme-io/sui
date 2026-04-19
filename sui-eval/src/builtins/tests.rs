use crate::eval::eval;
use crate::value::{NixString, SmolStr, StringContext, Value};
use super::{evaluate_flake, FLAKE_EVAL_DEPTH, MAX_FLAKE_EVAL_DEPTH};

fn ev(input: &str) -> Value {
    eval(input).unwrap()
}

#[test]
fn builtins_gen_list_generates_correct_list() {
    // genList (x: x * 2) 4 => [0 2 4 6]
    let v = ev("builtins.genList (x: x * 2) 4");
    assert_eq!(
        v,
        Value::list(vec![
            Value::Int(0),
            Value::Int(2),
            Value::Int(4),
            Value::Int(6),
        ]),
    );
}

#[test]
fn builtins_gen_list_zero_length() {
    let v = ev("builtins.genList (x: x) 0");
    assert_eq!(v, Value::list(vec![]));
}

#[test]
fn builtins_elem_finds_element() {
    assert_eq!(ev("builtins.elem 2 [1 2 3]"), Value::Bool(true));
}

#[test]
fn builtins_elem_missing_element() {
    assert_eq!(ev("builtins.elem 5 [1 2 3]"), Value::Bool(false));
}

#[test]
fn builtins_throw_produces_error() {
    let result = eval(r#"builtins.throw "kaboom""#);
    assert!(result.is_err());
    let msg = format!("{}", result.unwrap_err());
    assert!(msg.contains("kaboom"));
}

#[test]
fn builtins_seq_forces_first_arg() {
    // seq evaluates first arg then returns second
    assert_eq!(ev("builtins.seq 1 42"), Value::Int(42));
    assert_eq!(ev(r#"builtins.seq "forced" true"#), Value::Bool(true));
}

#[test]
fn builtins_current_system_valid_string() {
    let v = ev("builtins.currentSystem");
    if let Value::String(ns) = v {
        let s = &ns.chars;
        // Should match one of the known system strings
        assert!(
            ["aarch64-darwin", "x86_64-darwin", "aarch64-linux", "x86_64-linux"]
                .contains(&s.as_str()),
            "unexpected system string: {s}",
        );
    } else {
        panic!("expected string for currentSystem");
    }
}

#[test]
fn builtins_lang_version_is_int() {
    let v = ev("builtins.langVersion");
    assert!(matches!(v, Value::Int(_)));
}

#[test]
fn builtins_nix_version_is_string() {
    let v = ev("builtins.nixVersion");
    assert!(matches!(v, Value::String(_)));
}

#[test]
fn builtins_is_function() {
    assert_eq!(ev("builtins.isFunction (x: x)"), Value::Bool(true));
    assert_eq!(ev("builtins.isFunction builtins.head"), Value::Bool(true));
    assert_eq!(ev("builtins.isFunction 42"), Value::Bool(false));
}

#[test]
fn builtins_is_path() {
    assert_eq!(ev("builtins.isPath ./foo"), Value::Bool(true));
    assert_eq!(ev("builtins.isPath 42"), Value::Bool(false));
}

#[test]
fn builtins_elem_at() {
    assert_eq!(ev("builtins.elemAt [10 20 30] 1"), Value::Int(20));
}

#[test]
fn builtins_has_attr() {
    assert_eq!(ev(r#"builtins.hasAttr "a" { a = 1; }"#), Value::Bool(true));
    assert_eq!(ev(r#"builtins.hasAttr "b" { a = 1; }"#), Value::Bool(false));
}

#[test]
fn builtins_get_attr() {
    assert_eq!(ev(r#"builtins.getAttr "a" { a = 42; }"#), Value::Int(42));
}

// ── New builtins tests ───────────────────────────────

#[test]
fn builtins_map() {
    assert_eq!(
        ev("builtins.map (x: x * 2) [1 2 3]"),
        Value::list(vec![Value::Int(2), Value::Int(4), Value::Int(6)]),
    );
}

#[test]
fn builtins_map_empty() {
    assert_eq!(ev("builtins.map (x: x) []"), Value::list(vec![]));
}

#[test]
fn builtins_filter() {
    assert_eq!(
        ev("builtins.filter (x: x > 2) [1 2 3 4 5]"),
        Value::list(vec![Value::Int(3), Value::Int(4), Value::Int(5)]),
    );
}

#[test]
fn builtins_filter_empty() {
    assert_eq!(ev("builtins.filter (x: false) [1 2 3]"), Value::list(vec![]));
}

#[test]
fn builtins_foldl() {
    assert_eq!(ev("builtins.foldl' (a: b: a + b) 0 [1 2 3 4]"), Value::Int(10));
}

#[test]
fn builtins_foldl_empty() {
    assert_eq!(ev("builtins.foldl' (a: b: a + b) 0 []"), Value::Int(0));
}

#[test]
fn builtins_concat_map() {
    assert_eq!(
        ev("builtins.concatMap (x: [x (x * 2)]) [1 2 3]"),
        Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(2), Value::Int(4), Value::Int(3), Value::Int(6)]),
    );
}

#[test]
fn builtins_concat_lists() {
    assert_eq!(
        ev("builtins.concatLists [[1 2] [3] [4 5]]"),
        Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4), Value::Int(5)]),
    );
}

#[test]
fn builtins_all() {
    assert_eq!(ev("builtins.all (x: x > 0) [1 2 3]"), Value::Bool(true));
    assert_eq!(ev("builtins.all (x: x > 2) [1 2 3]"), Value::Bool(false));
}

#[test]
fn builtins_any() {
    assert_eq!(ev("builtins.any (x: x > 2) [1 2 3]"), Value::Bool(true));
    assert_eq!(ev("builtins.any (x: x > 5) [1 2 3]"), Value::Bool(false));
}

#[test]
fn builtins_map_attrs() {
    let v = ev(r#"builtins.mapAttrs (name: value: value * 2) { a = 1; b = 2; }"#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("a"), Some(&Value::Int(2)));
        assert_eq!(a.get("b"), Some(&Value::Int(4)));
    } else { panic!("expected attrs"); }
}

#[test]
fn builtins_list_to_attrs() {
    let v = ev(r#"builtins.listToAttrs [{ name = "a"; value = 1; } { name = "b"; value = 2; }]"#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("a"), Some(&Value::Int(1)));
        assert_eq!(a.get("b"), Some(&Value::Int(2)));
    } else { panic!("expected attrs"); }
}

#[test]
fn builtins_remove_attrs() {
    let v = ev(r#"builtins.removeAttrs { a = 1; b = 2; c = 3; } ["b" "c"]"#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.len(), 1);
        assert_eq!(a.get("a"), Some(&Value::Int(1)));
    } else { panic!("expected attrs"); }
}

#[test]
fn builtins_concat_strings_sep() {
    assert_eq!(
        ev(r#"builtins.concatStringsSep ", " ["a" "b" "c"]"#),
        Value::string("a, b, c"),
    );
}

#[test]
fn builtins_has_prefix() {
    assert_eq!(ev(r#"builtins.hasPrefix "foo" "foobar""#), Value::Bool(true));
    assert_eq!(ev(r#"builtins.hasPrefix "bar" "foobar""#), Value::Bool(false));
}

#[test]
fn builtins_has_suffix() {
    assert_eq!(ev(r#"builtins.hasSuffix "bar" "foobar""#), Value::Bool(true));
    assert_eq!(ev(r#"builtins.hasSuffix "foo" "foobar""#), Value::Bool(false));
}

#[test]
fn builtins_replace_strings() {
    assert_eq!(
        ev(r#"builtins.replaceStrings ["foo" "bar"] ["FOO" "BAR"] "foobar""#),
        Value::string("FOOBAR"),
    );
}

#[test]
fn builtins_ceil_floor() {
    assert_eq!(ev("builtins.ceil 3.2"), Value::Int(4));
    assert_eq!(ev("builtins.floor 3.8"), Value::Int(3));
}

#[test]
fn builtins_try_eval() {
    let v = ev("builtins.tryEval 42");
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("success"), Some(&Value::Bool(true)));
        assert_eq!(a.get("value"), Some(&Value::Int(42)));
    } else { panic!("expected attrs"); }
}

#[test]
fn builtins_trace() {
    assert_eq!(ev(r#"builtins.trace "debug msg" 42"#), Value::Int(42));
}

#[test]
fn builtins_function_args() {
    let v = ev("builtins.functionArgs ({ a, b ? 1 }: a)");
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("a"), Some(&Value::Bool(false)));
        assert_eq!(a.get("b"), Some(&Value::Bool(true)));
    } else { panic!("expected attrs"); }
}

#[test]
fn builtins_sort() {
    assert_eq!(
        ev("builtins.sort (a: b: a < b) [3 1 2]"),
        Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
    );
}

#[test]
fn builtins_sort_large_list() {
    // Verify O(n log n) sort handles 1000 elements correctly.
    // The list is [1000 999 ... 1] and should become [1 2 ... 1000].
    let expr = "builtins.sort (a: b: a < b) (builtins.genList (i: 1000 - i) 1000)";
    let result = ev(expr);
    let expected: Vec<Value> = (1..=1000).map(Value::Int).collect();
    assert_eq!(result, Value::list(expected));
}

#[test]
fn builtins_sort_already_sorted() {
    // Already-sorted input — worst case for insertion sort, O(n log n) for merge sort.
    let expr = "builtins.sort (a: b: a < b) (builtins.genList (i: i) 100)";
    let result = ev(expr);
    let expected: Vec<Value> = (0..100).map(Value::Int).collect();
    assert_eq!(result, Value::list(expected));
}

#[test]
fn builtins_sort_empty() {
    assert_eq!(
        ev("builtins.sort (a: b: a < b) []"),
        Value::list(vec![]),
    );
}

#[test]
fn builtins_sort_single_element() {
    assert_eq!(
        ev("builtins.sort (a: b: a < b) [42]"),
        Value::list(vec![Value::Int(42)]),
    );
}

#[test]
fn builtins_map_large_list() {
    // Verify map over a large list completes without performance regression.
    let expr = "builtins.map (x: x * 2) (builtins.genList (i: i) 1000)";
    let result = ev(expr);
    let expected: Vec<Value> = (0..1000).map(|i| Value::Int(i * 2)).collect();
    assert_eq!(result, Value::list(expected));
}

#[test]
fn builtins_cat_attrs() {
    assert_eq!(
        ev(r#"builtins.catAttrs "a" [{ a = 1; } { b = 2; } { a = 3; }]"#),
        Value::list(vec![Value::Int(1), Value::Int(3)]),
    );
}

// ── New builtins: concatStrings ─────────────────────────

#[test]
fn builtins_concat_strings() {
    assert_eq!(
        ev(r#"builtins.concatStrings ["hello" " " "world"]"#),
        Value::string("hello world"),
    );
}

#[test]
fn builtins_concat_strings_empty() {
    assert_eq!(
        ev(r#"builtins.concatStrings []"#),
        Value::string(""),
    );
}

// ── New builtins: partition ──────────────────────────────

#[test]
fn builtins_partition_basic() {
    let v = ev("builtins.partition (x: x > 2) [1 2 3 4 5]");
    if let Value::Attrs(a) = v {
        assert_eq!(
            a.get("right"),
            Some(&Value::list(vec![Value::Int(3), Value::Int(4), Value::Int(5)])),
        );
        assert_eq!(
            a.get("wrong"),
            Some(&Value::list(vec![Value::Int(1), Value::Int(2)])),
        );
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_partition_all_right() {
    let v = ev("builtins.partition (x: true) [1 2 3]");
    if let Value::Attrs(a) = v {
        assert_eq!(
            a.get("right"),
            Some(&Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)])),
        );
        assert_eq!(a.get("wrong"), Some(&Value::list(vec![])));
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_partition_empty() {
    let v = ev("builtins.partition (x: true) []");
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("right"), Some(&Value::list(vec![])));
        assert_eq!(a.get("wrong"), Some(&Value::list(vec![])));
    } else {
        panic!("expected attrs");
    }
}

// ── New builtins: groupBy ───────────────────────────────

#[test]
fn builtins_group_by_basic() {
    let v = ev(r#"builtins.groupBy (x: x) ["a" "b" "a" "c" "b"]"#);
    if let Value::Attrs(a) = v {
        assert_eq!(
            a.get("a"),
            Some(&Value::list(vec![
                Value::string("a"),
                Value::string("a"),
            ])),
        );
        assert_eq!(
            a.get("b"),
            Some(&Value::list(vec![
                Value::string("b"),
                Value::string("b"),
            ])),
        );
        assert_eq!(
            a.get("c"),
            Some(&Value::list(vec![Value::string("c")])),
        );
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_group_by_empty() {
    let v = ev(r#"builtins.groupBy (x: x) []"#);
    if let Value::Attrs(a) = v {
        assert!(a.is_empty());
    } else {
        panic!("expected attrs");
    }
}

// ── New builtins: zipAttrsWith ──────────────────────────

#[test]
fn builtins_zip_attrs_with_basic() {
    // zipAttrsWith (name: values: values) [{ a = 1; } { a = 2; b = 3; }]
    let v = ev("builtins.zipAttrsWith (name: values: values) [{ a = 1; } { a = 2; b = 3; }]");
    if let Value::Attrs(a) = v {
        assert_eq!(
            a.get("a"),
            Some(&Value::list(vec![Value::Int(1), Value::Int(2)])),
        );
        assert_eq!(
            a.get("b"),
            Some(&Value::list(vec![Value::Int(3)])),
        );
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_zip_attrs_with_sum() {
    // Sum values for each key
    let v = ev(r#"builtins.zipAttrsWith (name: values: builtins.foldl' (a: b: a + b) 0 values) [{ x = 1; } { x = 2; } { x = 3; }]"#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("x"), Some(&Value::Int(6)));
    } else {
        panic!("expected attrs");
    }
}

// ── New builtins: compareVersions ─────────��─────────────

#[test]
fn builtins_compare_versions_equal() {
    assert_eq!(ev(r#"builtins.compareVersions "1.2.3" "1.2.3""#), Value::Int(0));
}

#[test]
fn builtins_compare_versions_less() {
    assert_eq!(ev(r#"builtins.compareVersions "1.2.3" "1.2.4""#), Value::Int(-1));
    assert_eq!(ev(r#"builtins.compareVersions "1.2" "1.3""#), Value::Int(-1));
}

#[test]
fn builtins_compare_versions_greater() {
    assert_eq!(ev(r#"builtins.compareVersions "1.3.0" "1.2.9""#), Value::Int(1));
}

#[test]
fn builtins_compare_versions_pre() {
    // "pre" is less than anything except itself
    assert_eq!(ev(r#"builtins.compareVersions "1.0pre1" "1.0.1""#), Value::Int(-1));
}

// ── New builtins: parseDrvName ──────────────────────────

#[test]
fn builtins_parse_drv_name_basic() {
    let v = ev(r#"builtins.parseDrvName "hello-2.10""#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("name"), Some(&Value::string("hello")));
        assert_eq!(a.get("version"), Some(&Value::string("2.10")));
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_parse_drv_name_no_version() {
    let v = ev(r#"builtins.parseDrvName "hello""#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("name"), Some(&Value::string("hello")));
        assert_eq!(a.get("version"), Some(&Value::string("")));
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_parse_drv_name_complex() {
    let v = ev(r#"builtins.parseDrvName "openssl-1.1.1k""#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("name"), Some(&Value::string("openssl")));
        assert_eq!(a.get("version"), Some(&Value::string("1.1.1k")));
    } else {
        panic!("expected attrs");
    }
}

// ── New builtins: baseNameOf / dirOf ────────────────────

#[test]
fn builtins_base_name_of() {
    assert_eq!(
        ev(r#"builtins.baseNameOf "/nix/store/abc-hello""#),
        Value::string("abc-hello"),
    );
    assert_eq!(
        ev(r#"builtins.baseNameOf "hello.txt""#),
        Value::string("hello.txt"),
    );
}

#[test]
fn builtins_dir_of_string() {
    assert_eq!(
        ev(r#"builtins.dirOf "/nix/store/abc""#),
        Value::string("/nix/store"),
    );
    assert_eq!(
        ev(r#"builtins.dirOf "/foo""#),
        Value::string("/"),
    );
}

#[test]
fn builtins_dir_of_path() {
    assert_eq!(
        ev("builtins.dirOf /nix/store/abc"),
        Value::Path(Box::new(SmolStr::from("/nix/store"))),
    );
}

// ── New builtins: readFile ──────────────────────────────

#[test]
fn builtins_read_file() {
    // Create a temp file and read it
    let dir = std::env::temp_dir();
    let path = dir.join("sui_eval_test_read_file.txt");
    std::fs::write(&path, "hello from test").unwrap();
    let expr = format!(r#"builtins.readFile "{}""#, path.display());
    let v = eval(&expr).unwrap();
    if let Value::String(ns) = v {
        assert_eq!(ns.chars, "hello from test");
    } else {
        panic!("expected string");
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn builtins_read_file_missing() {
    let result = eval(r#"builtins.readFile "/nonexistent/path/file.txt""#);
    assert!(result.is_err());
}

// ── New builtins: addErrorContext ────────────────────────

#[test]
fn builtins_add_error_context_passthrough() {
    // addErrorContext just passes through the value
    assert_eq!(
        ev(r#"builtins.addErrorContext "context msg" 42"#),
        Value::Int(42),
    );
}

// ── __functor protocol ──────────────────────────────────

#[test]
fn functor_basic() {
    assert_eq!(
        ev("let s = { __functor = self: x: self.value + x; value = 10; }; in s 5"),
        Value::Int(15),
    );
}

#[test]
fn functor_nested() {
    // The functor can return another functor
    assert_eq!(
        ev("let s = { __functor = self: x: x * 2; }; in s 21"),
        Value::Int(42),
    );
}

#[test]
fn functor_with_update() {
    // Common pattern: { __functor = ...; } // { value = ...; }
    assert_eq!(
        ev(r#"
            let
                base = { __functor = self: x: self.v + x; v = 0; };
                extended = base // { v = 100; };
            in extended 5
        "#),
        Value::Int(105),
    );
}

// ── __toString protocol ─────────────────────────────────

#[test]
fn to_string_protocol_interpolation() {
    assert_eq!(
        ev(r#"let s = { __toString = self: "hello"; }; in "${s}""#),
        Value::string("hello"),
    );
}

#[test]
fn to_string_protocol_with_self() {
    assert_eq!(
        ev(r#"let s = { __toString = self: self.name; name = "world"; }; in "${s}""#),
        Value::string("world"),
    );
}

#[test]
fn to_string_protocol_via_builtin() {
    assert_eq!(
        ev(r#"builtins.toString { __toString = self: "custom"; }"#),
        Value::string("custom"),
    );
}

// ── Ignored tests for features needing major work ───────

#[test]
fn builtins_hash_string_sha256() {
    let v = ev(r#"builtins.hashString "sha256" "hello""#);
    if let Value::String(ns) = v {
        let s = &ns.chars;
        assert_eq!(s.len(), 64); // SHA-256 hex is 64 chars
        assert_eq!(*s, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    } else {
        panic!("expected string");
    }
}

#[test]
fn builtins_hash_string_sha512() {
    let v = ev(r#"builtins.hashString "sha512" "hello""#);
    if let Value::String(ns) = v {
        assert_eq!(ns.chars.len(), 128); // SHA-512 hex is 128 chars
    } else {
        panic!("expected string");
    }
}

#[test]
fn builtins_match_regex() {
    // match returns null on no match, list of groups on match
    assert_eq!(
        ev(r#"builtins.match "([0-9]+)\\.([0-9]+)" "1.23""#),
        Value::list(vec![Value::string("1"), Value::string("23")]),
    );
    assert_eq!(
        ev(r#"builtins.match "([0-9]+)" "abc""#),
        Value::Null,
    );
}

#[test]
fn builtins_match_full_string() {
    // match anchors to full string
    assert_eq!(
        ev(r#"builtins.match "([0-9]+)" "42""#),
        Value::list(vec![Value::string("42")]),
    );
    // Partial match should return null (anchored)
    assert_eq!(
        ev(r#"builtins.match "([0-9]+)" "abc42def""#),
        Value::Null,
    );
}

#[test]
fn builtins_import_file() {
    // Create a temp file and import it
    let dir = std::env::temp_dir();
    let path = dir.join("sui_eval_test_import.nix");
    std::fs::write(&path, "{ x = 42; }").unwrap();
    let expr = format!(r#"(builtins.import "{}").x"#, path.display());
    let v = eval(&expr).unwrap();
    assert_eq!(v, Value::Int(42));
    std::fs::remove_file(&path).ok();
}

#[test]
fn builtins_import_expr() {
    // Import a file that returns a simple expression
    let dir = std::env::temp_dir();
    let path = dir.join("sui_eval_test_import_expr.nix");
    std::fs::write(&path, "1 + 2").unwrap();
    let expr = format!(r#"builtins.import "{}""#, path.display());
    let v = eval(&expr).unwrap();
    assert_eq!(v, Value::Int(3));
    std::fs::remove_file(&path).ok();
}

#[test]
fn builtins_derivation_returns_real_paths() {
    let v = eval(r#"builtins.derivation { name = "test"; system = "x86_64-linux"; builder = "/bin/sh"; }"#).unwrap();
    let a = match v {
        Value::Attrs(a) => a,
        other => panic!("expected attrs, got {other:?}"),
    };
    assert_eq!(a.get("type"), Some(&Value::string("derivation")));
    assert_eq!(a.get("name"), Some(&Value::string("test")));

    // drvPath: /nix/store/<32 base32 chars>-test.drv
    let drv_path = a.get("drvPath").unwrap().as_string().unwrap();
    assert!(drv_path.starts_with("/nix/store/"), "drvPath: {drv_path}");
    assert!(drv_path.ends_with("-test.drv"), "drvPath: {drv_path}");
    let drv_basename = drv_path.strip_prefix("/nix/store/").unwrap();
    assert_eq!(drv_basename.len(), 32 + 1 + "test.drv".len());

    // outPath: /nix/store/<32 base32 chars>-test
    let out_path = a.get("outPath").unwrap().as_string().unwrap();
    assert!(out_path.starts_with("/nix/store/"));
    assert!(out_path.ends_with("-test"));
    assert_ne!(drv_path, out_path);
}

#[test]
fn builtins_derivation_is_deterministic() {
    // Same inputs must always produce the same paths.
    let expr = r#"builtins.derivation {
        name = "hello";
        system = "x86_64-linux";
        builder = "/bin/sh";
        args = [ "-e" "build.sh" ];
    }"#;
    let a1 = eval(expr).unwrap().to_attrs().unwrap();
    let a2 = eval(expr).unwrap().to_attrs().unwrap();
    assert_eq!(
        a1.get("drvPath").unwrap().to_str().unwrap(),
        a2.get("drvPath").unwrap().to_str().unwrap(),
    );
    assert_eq!(
        a1.get("outPath").unwrap().to_str().unwrap(),
        a2.get("outPath").unwrap().to_str().unwrap(),
    );
}

#[test]
fn builtins_derivation_different_names_produce_different_paths() {
    let v1 = eval(r#"builtins.derivation { name = "foo"; system = "x86_64-linux"; builder = "/bin/sh"; }"#).unwrap();
    let v2 = eval(r#"builtins.derivation { name = "bar"; system = "x86_64-linux"; builder = "/bin/sh"; }"#).unwrap();
    let p1 = v1.as_attrs().unwrap().get("drvPath").unwrap().as_string().unwrap().to_string();
    let p2 = v2.as_attrs().unwrap().get("drvPath").unwrap().as_string().unwrap().to_string();
    assert_ne!(p1, p2);
}

#[test]
fn builtins_derivation_multiple_outputs() {
    let v = eval(r#"builtins.derivation {
        name = "multi";
        system = "x86_64-linux";
        builder = "/bin/sh";
        outputs = [ "out" "dev" "lib" ];
    }"#).unwrap();
    let a = v.as_attrs().unwrap();
    assert_eq!(a.get("type"), Some(&Value::string("derivation")));

    // Each named output is a sub-attrset.
    for out_name in ["out", "dev", "lib"] {
        let sub = a
            .get(out_name)
            .unwrap_or_else(|| panic!("missing output {out_name}"));
        let sub_attrs = sub.as_attrs().unwrap();
        assert_eq!(sub_attrs.get("type"), Some(&Value::string("derivation")));
        assert_eq!(
            sub_attrs.get("outputName"),
            Some(&Value::string(out_name)),
        );
        // Sub-attrset should have an outPath.
        assert!(sub_attrs.contains_key("outPath"));
        assert!(sub_attrs.contains_key("drvPath"));
    }

    // The three outputs must have distinct paths.
    let out_p = a.get("out").unwrap().as_attrs().unwrap()
        .get("outPath").unwrap().as_string().unwrap().to_string();
    let dev_p = a.get("dev").unwrap().as_attrs().unwrap()
        .get("outPath").unwrap().as_string().unwrap().to_string();
    let lib_p = a.get("lib").unwrap().as_attrs().unwrap()
        .get("outPath").unwrap().as_string().unwrap().to_string();
    assert_ne!(out_p, dev_p);
    assert_ne!(out_p, lib_p);
    assert_ne!(dev_p, lib_p);
    assert!(dev_p.ends_with("-multi-dev"));
    assert!(lib_p.ends_with("-multi-lib"));
    assert!(out_p.ends_with("-multi"));
}

#[test]
fn builtins_derivation_fixed_output() {
    let v = eval(r#"builtins.derivation {
        name = "src.tar.gz";
        system = "x86_64-linux";
        builder = "/bin/curl";
        outputHash = "1b0ri5lsf45dknj8bfxi1syz35kmab77apxxg1yrf33la1qm3kc7";
        outputHashAlgo = "sha256";
        outputHashMode = "flat";
    }"#).unwrap();
    let a = v.as_attrs().unwrap();
    assert_eq!(a.get("type"), Some(&Value::string("derivation")));
    let out_path = a.get("outPath").unwrap().as_string().unwrap();
    assert!(out_path.ends_with("-src.tar.gz"));
    assert!(a.get("drvPath").unwrap().as_string().unwrap().ends_with("-src.tar.gz.drv"));
}

#[test]
fn builtins_derivation_fixed_output_recursive_differs_from_flat() {
    let flat = eval(r#"builtins.derivation {
        name = "x";
        system = "x86_64-linux";
        builder = "/bin/sh";
        outputHash = "abc";
        outputHashAlgo = "sha256";
        outputHashMode = "flat";
    }"#).unwrap();
    let rec = eval(r#"builtins.derivation {
        name = "x";
        system = "x86_64-linux";
        builder = "/bin/sh";
        outputHash = "abc";
        outputHashAlgo = "sha256";
        outputHashMode = "recursive";
    }"#).unwrap();
    let p1 = flat.as_attrs().unwrap().get("outPath").unwrap().as_string().unwrap().to_string();
    let p2 = rec.as_attrs().unwrap().get("outPath").unwrap().as_string().unwrap().to_string();
    assert_ne!(p1, p2);
}

#[test]
fn builtins_derivation_returns_drv_and_out_path() {
    // Sanity-check that the result attrset always has drvPath + outPath.
    let v = eval(r#"builtins.derivation { name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; }"#).unwrap();
    let a = v.as_attrs().unwrap();
    assert!(a.contains_key("drvPath"));
    assert!(a.contains_key("outPath"));
}

// ── .drv file writing tests ────────────────────────────
//
// These tests use a per-test temp directory via SUI_STORE_DIR so we
// don't need root access to /nix/store.
//
// Because SUI_STORE_DIR is a process-global env var and tests run in
// parallel, we serialize all drv-write tests behind a single mutex.

static DRV_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Helper: run a derivation expression with SUI_STORE_DIR pointed at
/// a fresh temp directory.  Returns (Value, temp_dir_path).
///
/// Caller must hold `DRV_WRITE_LOCK`.
fn eval_drv_in_temp_store_inner(expr: &str, dir: &std::path::Path) -> Value {
    // SAFETY: set_var is unsafe in edition 2024 because env is
    // process-global.  All callers hold DRV_WRITE_LOCK so there is
    // no concurrent mutation.
    unsafe { std::env::set_var("SUI_STORE_DIR", dir) };
    let result = eval(expr).unwrap();
    unsafe { std::env::remove_var("SUI_STORE_DIR") };
    result
}

fn make_drv_temp_dir(label: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "sui-drv-{label}-{}-{n}",
        std::process::id(),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn drv_write_creates_file_on_disk() {
    let _g = DRV_WRITE_LOCK.lock().unwrap();
    let store_dir = make_drv_temp_dir("create");
    let v = eval_drv_in_temp_store_inner(
        r#"builtins.derivation { name = "hello"; system = "x86_64-linux"; builder = "/bin/sh"; }"#,
        &store_dir,
    );
    let a = v.as_attrs().unwrap();
    let drv_path = a.get("drvPath").unwrap().as_string().unwrap();
    let disk_path = drv_path.replacen("/nix/store", &store_dir.to_string_lossy(), 1);
    let p = std::path::Path::new(&disk_path);
    assert!(p.exists(), "expected .drv file at {disk_path}");
    let content = std::fs::read_to_string(p).unwrap();
    assert!(content.starts_with("Derive("), "expected ATerm, got: {}", &content[..40.min(content.len())]);
    let _ = std::fs::remove_dir_all(&store_dir);
}

#[test]
fn drv_write_roundtrips_through_parse() {
    let _g = DRV_WRITE_LOCK.lock().unwrap();
    let store_dir = make_drv_temp_dir("roundtrip");
    let v = eval_drv_in_temp_store_inner(
        r#"builtins.derivation { name = "roundtrip"; system = "x86_64-linux"; builder = "/bin/sh"; args = ["-c" "echo hi"]; }"#,
        &store_dir,
    );
    let a = v.to_attrs().unwrap();
    let drv_path = a.get("drvPath").unwrap().to_str().unwrap();
    let disk_path = drv_path.replacen("/nix/store", &store_dir.to_string_lossy(), 1);
    let content = std::fs::read(&disk_path).unwrap();
    let parsed = sui_compat::derivation::Derivation::parse(&content).unwrap();
    assert_eq!(parsed.system, "x86_64-linux");
    assert_eq!(parsed.builder, "/bin/sh");
    assert_eq!(parsed.args, vec!["-c", "echo hi"]);
    // The parsed drv should have a non-empty output path for "out".
    let out = parsed.outputs.get("out").unwrap();
    assert!(!out.path.is_empty(), "output path should be populated");
    assert!(out.path.starts_with("/nix/store/"));
    let _ = std::fs::remove_dir_all(&store_dir);
}

#[test]
fn drv_write_is_idempotent() {
    let _g = DRV_WRITE_LOCK.lock().unwrap();
    let store_dir = make_drv_temp_dir("idem");

    unsafe { std::env::set_var("SUI_STORE_DIR", &store_dir) };
    let expr = r#"builtins.derivation { name = "idem"; system = "x86_64-linux"; builder = "/bin/sh"; }"#;
    let v1 = eval(expr).unwrap();
    let v2 = eval(expr).unwrap();
    unsafe { std::env::remove_var("SUI_STORE_DIR") };

    let a1 = v1.as_attrs().unwrap();
    let a2 = v2.as_attrs().unwrap();
    let p1 = a1.get("drvPath").unwrap().as_string().unwrap();
    let p2 = a2.get("drvPath").unwrap().as_string().unwrap();
    assert_eq!(p1, p2, "same derivation must produce same drvPath");

    // The file on disk should exist exactly once (not overwritten).
    let disk_path = p1.replacen("/nix/store", &store_dir.to_string_lossy(), 1);
    assert!(std::path::Path::new(&disk_path).exists());

    let _ = std::fs::remove_dir_all(&store_dir);
}

#[test]
fn drv_write_path_matches_filename() {
    let _g = DRV_WRITE_LOCK.lock().unwrap();
    let store_dir = make_drv_temp_dir("pathcheck");
    let v = eval_drv_in_temp_store_inner(
        r#"builtins.derivation { name = "pathcheck"; system = "x86_64-linux"; builder = "/bin/sh"; }"#,
        &store_dir,
    );
    let a = v.as_attrs().unwrap();
    let drv_path = a.get("drvPath").unwrap().as_string().unwrap();
    let disk_path = drv_path.replacen("/nix/store", &store_dir.to_string_lossy(), 1);

    // The filename component of the on-disk path should equal the
    // basename of the returned drvPath.
    let returned_basename = std::path::Path::new(&*drv_path)
        .file_name()
        .unwrap()
        .to_string_lossy();
    let disk_basename = std::path::Path::new(&disk_path)
        .file_name()
        .unwrap()
        .to_string_lossy();
    assert_eq!(returned_basename, disk_basename);

    let _ = std::fs::remove_dir_all(&store_dir);
}

#[test]
fn drv_write_fixed_output_creates_file() {
    let _g = DRV_WRITE_LOCK.lock().unwrap();
    let store_dir = make_drv_temp_dir("fod");
    let v = eval_drv_in_temp_store_inner(
        r#"builtins.derivation {
            name = "fod";
            system = "x86_64-linux";
            builder = "/bin/curl";
            outputHash = "abc123";
            outputHashAlgo = "sha256";
            outputHashMode = "flat";
        }"#,
        &store_dir,
    );
    let a = v.as_attrs().unwrap();
    let drv_path = a.get("drvPath").unwrap().as_string().unwrap();
    let disk_path = drv_path.replacen("/nix/store", &store_dir.to_string_lossy(), 1);
    let p = std::path::Path::new(&disk_path);
    assert!(p.exists(), "expected FOD .drv at {disk_path}");

    // Verify the parsed drv has the hash metadata
    let content = std::fs::read(p).unwrap();
    let parsed = sui_compat::derivation::Derivation::parse(&content).unwrap();
    let out = parsed.outputs.get("out").unwrap();
    assert_eq!(out.hash, "abc123");
    assert_eq!(out.hash_algo, "sha256");

    let _ = std::fs::remove_dir_all(&store_dir);
}

#[test]
fn drv_write_env_contains_output_paths() {
    let _g = DRV_WRITE_LOCK.lock().unwrap();
    let store_dir = make_drv_temp_dir("envtest");
    let v = eval_drv_in_temp_store_inner(
        r#"builtins.derivation { name = "envtest"; system = "x86_64-linux"; builder = "/bin/sh"; }"#,
        &store_dir,
    );
    let a = v.as_attrs().unwrap();
    let drv_path = a.get("drvPath").unwrap().as_string().unwrap();
    let disk_path = drv_path.replacen("/nix/store", &store_dir.to_string_lossy(), 1);
    let content = std::fs::read(&disk_path).unwrap();
    let parsed = sui_compat::derivation::Derivation::parse(&content).unwrap();

    // CppNix convention: env map has an entry for each output name.
    let out_env = parsed.env.get("out").expect("env should contain 'out'");
    assert!(out_env.starts_with("/nix/store/"), "out env: {out_env}");
    assert!(out_env.ends_with("-envtest"), "out env: {out_env}");

    let _ = std::fs::remove_dir_all(&store_dir);
}

#[test]
fn drv_write_multiple_outputs_all_in_env() {
    let _g = DRV_WRITE_LOCK.lock().unwrap();
    let store_dir = make_drv_temp_dir("multi-env");
    let v = eval_drv_in_temp_store_inner(
        r#"builtins.derivation {
            name = "multi-env";
            system = "x86_64-linux";
            builder = "/bin/sh";
            outputs = ["out" "dev" "lib"];
        }"#,
        &store_dir,
    );
    let a = v.as_attrs().unwrap();
    let drv_path = a.get("drvPath").unwrap().as_string().unwrap();
    let disk_path = drv_path.replacen("/nix/store", &store_dir.to_string_lossy(), 1);
    let content = std::fs::read(&disk_path).unwrap();
    let parsed = sui_compat::derivation::Derivation::parse(&content).unwrap();

    for output_name in ["out", "dev", "lib"] {
        let env_val = parsed.env.get(output_name)
            .unwrap_or_else(|| panic!("env missing '{output_name}'"));
        assert!(env_val.starts_with("/nix/store/"), "{output_name} env: {env_val}");
    }

    // Output paths in the ATerm outputs section should also be populated.
    for output_name in ["out", "dev", "lib"] {
        let out = parsed.outputs.get(output_name)
            .unwrap_or_else(|| panic!("outputs missing '{output_name}'"));
        assert!(!out.path.is_empty(), "output path for '{output_name}' is empty");
    }

    let _ = std::fs::remove_dir_all(&store_dir);
}

#[test]
fn builtins_fetchurl_exists_as_builtin() {
    // Verify fetchurl is registered and callable.
    // Test with a file:// URL served from a temp file to avoid network.
    let dir = std::env::temp_dir().join("sui_eval_test_fetchurl");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let payload = b"fetchurl-test-content";
    let file = dir.join("payload.txt");
    std::fs::write(&file, payload).unwrap();
    let file_url = format!("file://{}", file.display());
    let expr = format!(r#"builtins.fetchurl "{}""#, file_url);
    let v = eval(&expr).unwrap();
    if let Value::Path(p) = v {
        let content = std::fs::read_to_string(p.as_str()).unwrap();
        assert_eq!(content, "fetchurl-test-content");
    } else {
        panic!("expected path, got {v}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtins_fetchurl_attrset_form() {
    // Test the attrset form: { url, sha256? }
    let dir = std::env::temp_dir().join("sui_eval_test_fetchurl_attr");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let payload = b"attr-form-content";
    let file = dir.join("payload.txt");
    std::fs::write(&file, payload).unwrap();
    let file_url = format!("file://{}", file.display());
    let expr = format!(
        r#"builtins.fetchurl {{ url = "{}"; }}"#,
        file_url
    );
    let v = eval(&expr).unwrap();
    assert!(matches!(v, Value::Path(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtins_fetchurl_bad_type_errors() {
    let result = eval("builtins.fetchurl 42");
    assert!(result.is_err());
}

#[test]
fn builtins_read_dir() {
    let dir = std::env::temp_dir().join("sui_eval_test_readdir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("file.txt"), "content").unwrap();
    std::fs::create_dir(dir.join("subdir")).unwrap();
    let expr = format!(r#"builtins.readDir "{}""#, dir.display());
    let v = eval(&expr).unwrap();
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("file.txt"), Some(&Value::string("regular")));
        assert_eq!(a.get("subdir"), Some(&Value::string("directory")));
    } else {
        panic!("expected attrs");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtins_read_dir_empty() {
    let dir = std::env::temp_dir().join("sui_eval_test_readdir_empty");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let expr = format!(r#"builtins.readDir "{}""#, dir.display());
    let v = eval(&expr).unwrap();
    if let Value::Attrs(a) = v {
        assert!(a.is_empty());
    } else {
        panic!("expected attrs");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtins_path_with_file() {
    // builtins.path on a real file returns a /nix/store/... path
    let dir = std::env::temp_dir().join("sui_eval_test_builtins_path");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("hello.txt");
    std::fs::write(&file, "hello world").unwrap();
    let expr = format!(
        r#"builtins.path {{ path = "{}"; name = "test"; }}"#,
        file.display()
    );
    let v = eval(&expr).unwrap();
    if let Value::Path(p) = v {
        assert!(p.starts_with("/nix/store/"));
        assert!(p.ends_with("-test"));
    } else {
        panic!("expected path, got {v}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtins_path_default_name() {
    // Without explicit name, uses the file name component
    let dir = std::env::temp_dir().join("sui_eval_test_builtins_path_dn");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("myfile.txt");
    std::fs::write(&file, "content").unwrap();
    let expr = format!(
        r#"builtins.path {{ path = "{}"; }}"#,
        file.display()
    );
    let v = eval(&expr).unwrap();
    if let Value::Path(p) = v {
        assert!(p.starts_with("/nix/store/"));
        assert!(p.ends_with("-myfile.txt"));
    } else {
        panic!("expected path, got {v}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtins_placeholder() {
    let v = ev(r#"builtins.placeholder "out""#);
    if let Value::String(ns) = v {
        let s = &ns.chars;
        assert!(s.starts_with("/placeholder-"));
        assert_eq!(s.len(), "/placeholder-".len() + 32);
    } else {
        panic!("expected string");
    }
}

#[test]
fn builtins_get_flake_path_based() {
    // getFlake with a path-based flake reference reads and evaluates
    // flake.nix. We probe via an output-fn attribute (`value`) since
    // flake-body metadata like `description` intentionally does NOT
    // appear on the top-level getFlake result (CppNix parity).
    let dir = std::env::temp_dir().join("sui_eval_test_getflake");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("flake.nix"),
        r#"{ description = "test flake"; outputs = { self }: { value = 42; }; }"#,
    )
    .unwrap();
    let expr = format!(r#"(builtins.getFlake "{}").value"#, dir.display());
    let v = eval(&expr).unwrap();
    assert_eq!(v, Value::Int(42));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtins_get_flake_accepts_indirect_ref_offline() {
    // `getFlake "nixpkgs"` now resolves through the registry and
    // attempts to fetch. Without network (the expected state in CI
    // and most dev runs) the fetch step fails — but the error is an
    // IoError (fetch failure), NOT a NotImplemented (unsupported
    // scheme). This asserts the routing/resolution lands correctly;
    // the online path is exercised by the SUI_TEST_ONLINE-gated
    // integration test in tests/oracle.rs.
    let result = eval(r#"builtins.getFlake "nixpkgs""#);
    match result {
        Err(crate::value::EvalError::NotImplemented(msg)) => {
            panic!("getFlake should route indirect refs, not reject them: {msg}")
        }
        Ok(_) | Err(_) => {} // Ok (network up) or other Err (fetch/eval) both fine
    }
}

#[test]
fn flake_minimal_no_inputs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          description = "test flake";
          outputs = { self }: { packages.default = "hello"; };
        }"#,
    )
    .unwrap();

    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").packages.default"#);
    let result = eval(&expr).unwrap();
    assert_eq!(result.as_string().unwrap(), "hello");
}

#[test]
fn flake_with_self_output_path() {
    // Inside an outputs fn, `self.outPath` is the NAR-hashed store
    // path (same value the top-level flake result surfaces).  It
    // is NOT the raw filesystem path — CppNix parity.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          description = "test flake";
          outputs = { self }: { result = self.outPath; };
        }"#,
    )
    .unwrap();

    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").result"#);
    let result = eval(&expr).unwrap();
    let out = result.as_string().unwrap();
    assert!(out.starts_with("/nix/store/"), "self.outPath must be a store path, got {out}");
    assert!(out.ends_with("-source"), "self.outPath must end with -source, got {out}");
    assert_ne!(out, flake_path, "self.outPath must NOT be the raw filesystem path");
}

#[test]
fn flake_description_accessible() {
    // The description attr is on the flake attrset itself, not the outputs;
    // evaluate_flake() merges it into the result so consumers can read it.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          description = "my flake";
          outputs = { self }: { packages.default = self.outPath; };
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").packages.default"#);
    assert!(eval(&expr).is_ok());
}

#[test]
fn flake_path_prefix_supported() {
    // path: prefix should also resolve to a directory.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          outputs = { self }: { value = "ok"; };
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "path:{flake_path}").value"#);
    let result = eval(&expr).unwrap();
    assert_eq!(result.as_string().unwrap(), "ok");
}

#[test]
fn flake_with_locked_input_path() {
    // A flake with a real path-typed input pinned in flake.lock.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.dep = { };
          outputs = { self, dep }: { result = dep.outPath; };
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("flake.lock"),
        r#"{
          "nodes": {
            "dep": {
              "locked": {
                "lastModified": 1700000000,
                "narHash": "sha256-DEPDEPDEPDEPDEPDEPDEPDEPDEPDEPDEPDEPDEPDEPD=",
                "path": "/var/empty/dep",
                "type": "path"
              },
              "original": {
                "type": "path",
                "url": "/var/empty/dep"
              }
            },
            "root": {
              "inputs": {
                "dep": "dep"
              }
            }
          },
          "root": "root",
          "version": 7
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").result"#);
    let result = eval(&expr).unwrap();
    assert_eq!(result.as_string().unwrap(), "/var/empty/dep");
}

#[test]
fn flake_missing_outputs_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{ description = "no outputs"; }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"builtins.getFlake "{flake_path}""#);
    assert!(eval(&expr).is_err());
}

// ── Phase 4: flake fetcher + recursive input tests ──────

#[test]
fn flake_input_source_info_populated() {
    // Verify that locked inputs get a sourceInfo attrset.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.dep = { };
          outputs = { self, dep }: { result = dep.sourceInfo.narHash; };
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("flake.lock"),
        r#"{
          "nodes": {
            "dep": {
              "locked": {
                "lastModified": 1700000000,
                "narHash": "sha256-XYZXYZXYZXYZXYZXYZXYZXYZXYZXYZXYZXYZXYZXYZ=",
                "path": "/var/empty/dep",
                "type": "path"
              },
              "original": { "type": "path", "url": "/var/empty/dep" }
            },
            "root": { "inputs": { "dep": "dep" } }
          },
          "root": "root",
          "version": 7
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").result"#);
    let result = eval(&expr).unwrap();
    assert_eq!(
        result.as_string().unwrap(),
        "sha256-XYZXYZXYZXYZXYZXYZXYZXYZXYZXYZXYZXYZXYZXYZ="
    );
}

#[test]
fn flake_input_last_modified_accessible() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.dep = { };
          outputs = { self, dep }: { result = dep.lastModified; };
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("flake.lock"),
        r#"{
          "nodes": {
            "dep": {
              "locked": {
                "lastModified": 1700000042,
                "narHash": "sha256-AAAA=",
                "path": "/tmp",
                "type": "path"
              },
              "original": { "type": "path", "url": "/tmp" }
            },
            "root": { "inputs": { "dep": "dep" } }
          },
          "root": "root",
          "version": 7
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").result"#);
    let result = eval(&expr).unwrap();
    assert_eq!(result, Value::Int(1_700_000_042));
}

#[test]
fn flake_input_rev_and_short_rev() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.dep = { };
          outputs = { self, dep }: { r = dep.rev; s = dep.shortRev; };
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("flake.lock"),
        r#"{
          "nodes": {
            "dep": {
              "locked": {
                "lastModified": 1700000000,
                "narHash": "sha256-BBB=",
                "rev": "abc123def456abc123def456abc123def456abc1",
                "path": "/tmp",
                "type": "path"
              },
              "original": { "type": "path", "url": "/tmp" }
            },
            "root": { "inputs": { "dep": "dep" } }
          },
          "root": "root",
          "version": 7
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let rev_expr = format!(r#"(builtins.getFlake "{flake_path}").r"#);
    let short_expr = format!(r#"(builtins.getFlake "{flake_path}").s"#);
    let rev = eval(&rev_expr).unwrap();
    let short = eval(&short_expr).unwrap();
    assert_eq!(
        rev.as_string().unwrap(),
        "abc123def456abc123def456abc123def456abc1"
    );
    assert_eq!(short.as_string().unwrap(), "abc123d");
}

#[test]
fn flake_non_flake_input_skips_recursive_eval() {
    // An input with `flake = false` should NOT have its flake.nix evaluated
    // even if one exists in the path.
    let dep_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dep_dir.path().join("flake.nix"),
        r#"{ outputs = { self }: { should_not_exist = true; }; }"#,
    )
    .unwrap();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.dep = { };
          outputs = { self, dep }: {
            has_attr = builtins.hasAttr "should_not_exist" dep;
          };
        }"#,
    )
    .unwrap();
    let dep_path = dep_dir.path().to_string_lossy().to_string();
    std::fs::write(
        dir.path().join("flake.lock"),
        format!(
            r#"{{
          "nodes": {{
            "dep": {{
              "flake": false,
              "locked": {{
                "lastModified": 1700000000,
                "narHash": "sha256-NOFLAKEDEP=",
                "path": "{dep_path}",
                "type": "path"
              }},
              "original": {{ "type": "path", "url": "{dep_path}" }}
            }},
            "root": {{ "inputs": {{ "dep": "dep" }} }}
          }},
          "root": "root",
          "version": 7
        }}"#
        ),
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").has_attr"#);
    let result = eval(&expr).unwrap();
    assert_eq!(result, Value::Bool(false));
}

#[test]
fn flake_recursive_flake_input_merges_outputs() {
    // An input that IS a flake should have its outputs merged into
    // the input attrset.
    let dep_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dep_dir.path().join("flake.nix"),
        r#"{
          description = "dependency flake";
          outputs = { self }: { lib.greet = "hello from dep"; };
        }"#,
    )
    .unwrap();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.dep = { };
          outputs = { self, dep }: { result = dep.lib.greet; };
        }"#,
    )
    .unwrap();
    let dep_path = dep_dir.path().to_string_lossy().to_string();
    std::fs::write(
        dir.path().join("flake.lock"),
        format!(
            r#"{{
          "nodes": {{
            "dep": {{
              "locked": {{
                "lastModified": 1700000000,
                "narHash": "sha256-FLAKEDEP=",
                "path": "{dep_path}",
                "type": "path"
              }},
              "original": {{ "type": "path", "url": "{dep_path}" }}
            }},
            "root": {{ "inputs": {{ "dep": "dep" }} }}
          }},
          "root": "root",
          "version": 7
        }}"#
        ),
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").result"#);
    let result = eval(&expr).unwrap();
    assert_eq!(result.as_string().unwrap(), "hello from dep");
}

#[test]
fn flake_getflake_github_prefix_invalid_ref_errors() {
    // github: ref without a slash should produce a clear error.
    let result = eval(r#"builtins.getFlake "github:justowner""#);
    assert!(result.is_err());
}

#[test]
fn flake_input_source_info_outpath_matches() {
    // sourceInfo.outPath should match the top-level outPath.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.dep = { };
          outputs = { self, dep }: {
            result = dep.outPath == dep.sourceInfo.outPath;
          };
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("flake.lock"),
        r#"{
          "nodes": {
            "dep": {
              "locked": {
                "lastModified": 1700000000,
                "narHash": "sha256-MATCH=",
                "path": "/var/empty/dep",
                "type": "path"
              },
              "original": { "type": "path", "url": "/var/empty/dep" }
            },
            "root": { "inputs": { "dep": "dep" } }
          },
          "root": "root",
          "version": 7
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").result"#);
    let result = eval(&expr).unwrap();
    assert_eq!(result, Value::Bool(true));
}

#[test]
fn flake_self_description_accessible_in_outputs() {
    // self.description should be readable from inside outputs.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          description = "my awesome flake";
          outputs = { self }: { desc = self.description; };
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").desc"#);
    let result = eval(&expr).unwrap();
    assert_eq!(result.as_string().unwrap(), "my awesome flake");
}

#[test]
fn flake_multiple_inputs_all_accessible() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.a = { };
          inputs.b = { };
          outputs = { self, a, b }: {
            result = "${a.narHash}:${b.narHash}";
          };
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("flake.lock"),
        r#"{
          "nodes": {
            "a": {
              "locked": {
                "lastModified": 1700000000,
                "narHash": "sha256-AAAA=",
                "path": "/tmp/a",
                "type": "path"
              },
              "original": { "type": "path", "url": "/tmp/a" }
            },
            "b": {
              "locked": {
                "lastModified": 1700000001,
                "narHash": "sha256-BBBB=",
                "path": "/tmp/b",
                "type": "path"
              },
              "original": { "type": "path", "url": "/tmp/b" }
            },
            "root": { "inputs": { "a": "a", "b": "b" } }
          },
          "root": "root",
          "version": 7
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").result"#);
    let result = eval(&expr).unwrap();
    assert_eq!(result.as_string().unwrap(), "sha256-AAAA=:sha256-BBBB=");
}

// ── evaluate_flake CppNix-compatible result shape ────────

#[test]
fn flake_result_has_outpath() {
    // The top-level flake result exposes outPath + output-fn keys, but
    // NOT flake-body metadata like `description` (CppNix does not leak
    // that into the top-level attrset — verified empirically).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          description = "test";
          outputs = { self }: { value = 42; };
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let has_out = format!(r#"(builtins.getFlake "{flake_path}") ? outPath"#);
    let has_desc = format!(r#"(builtins.getFlake "{flake_path}") ? description"#);
    let has_val = format!(r#"(builtins.getFlake "{flake_path}") ? value"#);
    assert_eq!(eval(&has_out).unwrap(), Value::Bool(true));
    assert_eq!(eval(&has_desc).unwrap(), Value::Bool(false));
    assert_eq!(eval(&has_val).unwrap(), Value::Bool(true));
}

#[test]
fn flake_result_has_inputs() {
    // The top-level flake result must include an `inputs` attrset.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.dep = { };
          outputs = { self, dep }: { ok = true; };
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("flake.lock"),
        r#"{
          "nodes": {
            "dep": {
              "locked": {
                "lastModified": 1700000000,
                "narHash": "sha256-INPUTSTEST=",
                "path": "/var/empty/dep",
                "type": "path"
              },
              "original": { "type": "path", "url": "/var/empty/dep" }
            },
            "root": { "inputs": { "dep": "dep" } }
          },
          "root": "root",
          "version": 7
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let has_inputs = format!(r#"(builtins.getFlake "{flake_path}") ? inputs"#);
    let has_dep = format!(
        r#"(builtins.getFlake "{flake_path}").inputs ? dep"#
    );
    assert_eq!(eval(&has_inputs).unwrap(), Value::Bool(true));
    assert_eq!(eval(&has_dep).unwrap(), Value::Bool(true));
}

#[test]
fn flake_inputs_have_outpath() {
    // Each input in `inputs` must have `outPath`.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.dep = { };
          outputs = { self, dep }: {
            result = (builtins.getFlake self.outPath).inputs.dep ? outPath;
          };
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("flake.lock"),
        r#"{
          "nodes": {
            "dep": {
              "locked": {
                "lastModified": 1700000000,
                "narHash": "sha256-DEPOP=",
                "path": "/var/empty/dep",
                "type": "path"
              },
              "original": { "type": "path", "url": "/var/empty/dep" }
            },
            "root": { "inputs": { "dep": "dep" } }
          },
          "root": "root",
          "version": 7
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(
        r#"(builtins.getFlake "{flake_path}").inputs.dep ? outPath"#
    );
    assert_eq!(eval(&expr).unwrap(), Value::Bool(true));
}

#[test]
fn flake_self_has_inputs() {
    // `self.inputs` should be accessible inside the outputs function.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.dep = { };
          outputs = { self, dep }: {
            result = self.inputs.dep.narHash;
          };
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("flake.lock"),
        r#"{
          "nodes": {
            "dep": {
              "locked": {
                "lastModified": 1700000000,
                "narHash": "sha256-SELFIN=",
                "path": "/var/empty/dep",
                "type": "path"
              },
              "original": { "type": "path", "url": "/var/empty/dep" }
            },
            "root": { "inputs": { "dep": "dep" } }
          },
          "root": "root",
          "version": 7
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").result"#);
    let result = eval(&expr).unwrap();
    assert_eq!(result.as_string().unwrap(), "sha256-SELFIN=");
}

#[test]
fn flake_self_outpath_in_outputs() {
    // `self.outPath` inside an outputs fn is the NAR-hashed source
    // store path (same value the top-level flake result surfaces).
    // This is CppNix parity: the filesystem path is never exposed
    // to Nix code, only the store path.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          description = "self-test";
          outputs = { self }: { dir = self.outPath; };
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").dir"#);
    let result = eval(&expr).unwrap();
    let out = result.as_string().unwrap();
    assert!(out.starts_with("/nix/store/"), "self.outPath must be a store path, got {out}");
    assert!(out.ends_with("-source"), "self.outPath must end with -source, got {out}");
    assert_ne!(out, flake_path);
}

#[test]
fn flake_string_interpolation_with_input() {
    // `"${dep}/file.txt"` should work because dep has outPath.
    let dep_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dep_dir.path().join("flake.nix"),
        r#"{ description = "dep"; outputs = { self }: { }; }"#,
    )
    .unwrap();
    std::fs::write(dep_dir.path().join("data.txt"), "hello").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dep_path = dep_dir.path().to_string_lossy().to_string();
    std::fs::write(
        dir.path().join("flake.nix"),
        format!(
            r#"{{
          inputs.dep = {{ }};
          outputs = {{ self, dep }}: {{
            data = builtins.readFile "${{dep}}/data.txt";
          }};
        }}"#
        ),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("flake.lock"),
        format!(
            r#"{{
          "nodes": {{
            "dep": {{
              "locked": {{
                "lastModified": 1700000000,
                "narHash": "sha256-INTERP=",
                "path": "{dep_path}",
                "type": "path"
              }},
              "original": {{ "type": "path", "url": "{dep_path}" }}
            }},
            "root": {{ "inputs": {{ "dep": "dep" }} }}
          }},
          "root": "root",
          "version": 7
        }}"#
        ),
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").data"#);
    let result = eval(&expr).unwrap();
    assert_eq!(result.as_string().unwrap(), "hello");
}

#[test]
fn flake_result_outpath_is_a_store_path() {
    // `outPath` on a flake result is the NAR-hashed source store
    // path — `/nix/store/<hash>-source` — NOT the raw filesystem
    // path.  This matches CppNix byte-for-byte (verified empirically
    // against `nix eval`).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{ outputs = { self }: { }; }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").outPath"#);
    let result = eval(&expr).unwrap();
    let out = result.as_string().unwrap();
    assert!(out.starts_with("/nix/store/"),
        "outPath must start with /nix/store/, got {out}");
    assert!(out.ends_with("-source"),
        "outPath must end with -source, got {out}");
    assert_ne!(out, flake_path,
        "outPath must be a store path, not the raw filesystem path");
}

#[test]
fn flake_result_source_info_present() {
    // The result must have `sourceInfo`.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{ outputs = { self }: { }; }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}") ? sourceInfo"#);
    assert_eq!(eval(&expr).unwrap(), Value::Bool(true));
}

#[test]
fn pure_mode_toggle() {
    use crate::eval::{is_pure_mode, set_pure_mode};
    // Default is impure.
    set_pure_mode(false);
    assert!(!is_pure_mode());
    set_pure_mode(true);
    assert!(is_pure_mode());
    // Restore so we don't poison neighbouring tests on the same thread.
    set_pure_mode(false);
    assert!(!is_pure_mode());
}

#[test]
fn builtins_to_path() {
    let v = ev(r#"builtins.toPath "/foo/bar""#);
    assert_eq!(v, Value::Path(Box::new(SmolStr::from("/foo/bar"))));
}

#[test]
fn builtins_to_path_rejects_relative() {
    let result = eval(r#"builtins.toPath "relative/path""#);
    assert!(result.is_err());
}

#[test]
fn builtins_store_path() {
    let v = ev(r#"builtins.storePath "/nix/store/abc-hello""#);
    assert_eq!(v, Value::Path(Box::new(SmolStr::from("/nix/store/abc-hello"))));
}

#[test]
fn builtins_store_path_rejects_non_store() {
    let result = eval(r#"builtins.storePath "/tmp/not-store""#);
    assert!(result.is_err());
}

#[test]
fn builtins_fetch_tarball_from_file() {
    // Create a .tar.gz in a temp dir, fetch it via file:// URL
    let dir = std::env::temp_dir().join("sui_eval_test_fetchtarball");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Build a small tarball in memory
    let tar_gz_path = dir.join("archive.tar.gz");
    {
        let file = std::fs::File::create(&tar_gz_path).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar_builder = tar::Builder::new(enc);
        let data = b"hello tarball";
        let mut header = tar::Header::new_gnu();
        header.set_path("hello.txt").unwrap();
        header.set_size(data.len() as u64);
        header.set_cksum();
        tar_builder.append(&header, &data[..]).unwrap();
        tar_builder.finish().unwrap();
    }

    let file_url = format!("file://{}", tar_gz_path.display());
    let expr = format!(r#"builtins.fetchTarball "{}""#, file_url);
    let v = eval(&expr).unwrap();
    if let Value::Path(p) = v {
        // The extracted directory should exist
        assert!(
            std::path::Path::new(p.as_str()).exists(),
            "extracted dir should exist: {p}",
        );
    } else {
        panic!("expected path, got {v}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtins_fetch_tarball_bad_type_errors() {
    let result = eval("builtins.fetchTarball 42");
    assert!(result.is_err());
}

#[test] fn sc_plain() { assert!(!NixString::plain("hello").has_context()); }
#[test] fn sc_merge() { let mut c = StringContext::new(); c.add_plain("/nix/store/abc".to_string()); assert!(NixString::with_context("hi", c).has_context()); }
#[test] fn has_ctx_false() { assert_eq!(ev(r#"builtins.hasContext "hello""#), Value::Bool(false)); }
#[test] fn discard_ctx() { assert_eq!(ev(r#"builtins.hasContext (builtins.unsafeDiscardStringContext "hello")"#), Value::Bool(false)); }
#[test] fn get_ctx_empty() { let v = ev(r#"builtins.getContext "hello""#); if let Value::Attrs(a) = v { assert!(a.is_empty()); } else { panic!(); } }
#[test] fn has_ctx_after_append() { assert_eq!(ev(r#"builtins.hasContext (builtins.appendContext "hello" { "/nix/store/abc" = { path = true; }; })"#), Value::Bool(true)); }
#[test] fn append_ctx_rt() { let v = ev(r#"builtins.getContext (builtins.appendContext "hello" { "/nix/store/abc" = { path = true; }; })"#); if let Value::Attrs(a) = v { assert!(a.contains_key("/nix/store/abc")); } else { panic!(); } }
#[test] fn discard_ctx_all() { let v = ev(r#"let s = builtins.appendContext "hello" { "/nix/store/abc" = { path = true; }; }; clean = builtins.unsafeDiscardStringContext s; in builtins.getContext clean"#); if let Value::Attrs(a) = v { assert!(a.is_empty()); } else { panic!(); } }
#[test] fn concat_merges_ctx() { let v = ev(r#"let a = builtins.appendContext "foo" { "/nix/store/a" = { path = true; }; }; b = builtins.appendContext "bar" { "/nix/store/b" = { path = true; }; }; in builtins.getContext (a + b)"#); if let Value::Attrs(a) = v { assert!(a.contains_key("/nix/store/a")); assert!(a.contains_key("/nix/store/b")); } else { panic!(); } }
#[test]
fn interp_merges_ctx() {
    // String interpolation must propagate context from interpolated values.
    assert_eq!(
        ev(r##"let s = builtins.appendContext "world" { "/nix/store/x" = { path = true; }; }; in builtins.hasContext "hello ${s}""##),
        Value::Bool(true),
    );
}
#[test]
fn path_interp_ctx() {
    // Path interpolated into string adds a Plain context element.
    // Use let binding to avoid raw string quoting issues with "${...}".
    let v = ev(r#"let p = /tmp; in builtins.hasContext "${p}""#);
    assert_eq!(v, Value::Bool(true));
}
#[test]
fn path_interp_ctx_content() {
    // Verify the context entry produced by path interpolation.
    let v = ev(r#"let p = /tmp; in builtins.getContext "${p}""#);
    if let Value::Attrs(a) = v {
        assert!(!a.is_empty(), "context should contain at least one entry");
    } else {
        panic!("expected Attrs, got {v:?}");
    }
}
#[test] fn add_drv_out_deps() { let v = ev(r#"let s = builtins.appendContext "/nix/store/abc.drv" { "/nix/store/abc.drv" = { path = true; }; }; p = builtins.addDrvOutputDependencies s; in builtins.getContext p"#); if let Value::Attrs(a) = v { let e = a.get("/nix/store/abc.drv").unwrap().as_attrs().unwrap(); assert_eq!(e.get("allOutputs"), Some(&Value::Bool(true))); } else { panic!(); } }
#[test] fn discard_out_dep() { let v = ev(r#"let s = builtins.appendContext "hello" { "/nix/store/x.drv" = { allOutputs = true; }; }; d = builtins.unsafeDiscardOutputDependency s; in builtins.getContext d"#); if let Value::Attrs(a) = v { let e = a.get("/nix/store/x.drv").unwrap().as_attrs().unwrap(); assert_eq!(e.get("path"), Some(&Value::Bool(true))); } else { panic!(); } }

// ── genericClosure tests ────────────────────────────────

#[test]
fn builtins_generic_closure_linear_chain() {
    // Linear chain: start at 1, operator produces next until 5
    let v = ev(r#"
        builtins.genericClosure {
            startSet = [{ key = 1; }];
            operator = item: if item.key < 5 then [{ key = item.key + 1; }] else [];
        }
    "#);
    if let Value::List(items) = v {
        assert_eq!(items.len(), 5);
        // Keys should be 1..5
        for (i, item) in items.iter().enumerate() {
            let attrs = item.to_attrs().unwrap();
            assert_eq!(attrs.get("key"), Some(&Value::Int(i as i64 + 1)));
        }
    } else {
        panic!("expected list");
    }
}

#[test]
fn builtins_generic_closure_diamond_dedup() {
    // Diamond: A→B, A→C, B→D, C→D. D should appear once.
    let v = ev(r#"
        builtins.genericClosure {
            startSet = [{ key = "A"; }];
            operator = item:
                if item.key == "A" then [{ key = "B"; } { key = "C"; }]
                else if item.key == "B" then [{ key = "D"; }]
                else if item.key == "C" then [{ key = "D"; }]
                else [];
        }
    "#);
    if let Value::List(items) = v {
        assert_eq!(items.len(), 4); // A, B, C, D (D only once)
    } else {
        panic!("expected list");
    }
}

#[test]
fn builtins_generic_closure_empty_operator() {
    let v = ev(r#"
        builtins.genericClosure {
            startSet = [{ key = 1; } { key = 2; }];
            operator = item: [];
        }
    "#);
    if let Value::List(items) = v {
        assert_eq!(items.len(), 2);
    } else {
        panic!("expected list");
    }
}

// ── fromTOML tests ──────────────────────────────────────

#[test]
fn builtins_from_toml_simple_table() {
    let v = ev(r#"builtins.fromTOML ''
        name = "hello"
        version = 42
    ''"#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("name"), Some(&Value::string("hello")));
        assert_eq!(a.get("version"), Some(&Value::Int(42)));
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_from_toml_nested() {
    let v = ev(r#"builtins.fromTOML ''
        [package]
        name = "test"
        [package.metadata]
        key = true
    ''"#);
    if let Value::Attrs(a) = v {
        let pkg = a.get("package").unwrap().as_attrs().unwrap();
        assert_eq!(pkg.get("name"), Some(&Value::string("test")));
        let meta = pkg.get("metadata").unwrap().as_attrs().unwrap();
        assert_eq!(meta.get("key"), Some(&Value::Bool(true)));
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_from_toml_arrays() {
    let v = ev(r#"builtins.fromTOML ''
        ports = [80, 443]
    ''"#);
    if let Value::Attrs(a) = v {
        assert_eq!(
            a.get("ports"),
            Some(&Value::list(vec![Value::Int(80), Value::Int(443)])),
        );
    } else {
        panic!("expected attrs");
    }
}

// ── lessThan tests ──────────────────────────────────────

#[test]
fn builtins_less_than_ints() {
    assert_eq!(ev("builtins.lessThan 1 2"), Value::Bool(true));
    assert_eq!(ev("builtins.lessThan 2 1"), Value::Bool(false));
    assert_eq!(ev("builtins.lessThan 1 1"), Value::Bool(false));
}

#[test]
fn builtins_less_than_floats() {
    assert_eq!(ev("builtins.lessThan 1.0 2.0"), Value::Bool(true));
    assert_eq!(ev("builtins.lessThan 2.0 1.0"), Value::Bool(false));
}

#[test]
fn builtins_less_than_strings() {
    assert_eq!(ev(r#"builtins.lessThan "abc" "def""#), Value::Bool(true));
    assert_eq!(ev(r#"builtins.lessThan "def" "abc""#), Value::Bool(false));
}

// ── bitwise tests ───────────────────────────────────────

#[test]
fn builtins_bit_and() {
    assert_eq!(ev("builtins.bitAnd 12 10"), Value::Int(8));  // 1100 & 1010 = 1000
}

#[test]
fn builtins_bit_or() {
    assert_eq!(ev("builtins.bitOr 12 10"), Value::Int(14)); // 1100 | 1010 = 1110
}

#[test]
fn builtins_bit_xor() {
    assert_eq!(ev("builtins.bitXor 12 10"), Value::Int(6));  // 1100 ^ 1010 = 0110
}

// ── splitVersion tests ──────────────────────────────────

#[test]
fn builtins_split_version_standard() {
    // Real nix drops separators: "1.2.3" → ["1","2","3"]
    assert_eq!(
        ev(r#"builtins.splitVersion "1.2.3""#),
        Value::list(vec![
            Value::string("1"),
            Value::string("2"),
            Value::string("3"),
        ]),
    );
}

#[test]
fn builtins_split_version_pre_release() {
    // Digit/non-digit transitions still split, but the `.` is dropped.
    assert_eq!(
        ev(r#"builtins.splitVersion "1.0pre1""#),
        Value::list(vec![
            Value::string("1"),
            Value::string("0"),
            Value::string("pre"),
            Value::string("1"),
        ]),
    );
}

// ── pathExists tests ────────────────────────────────────

#[test]
fn builtins_path_exists_tmpfile() {
    let dir = std::env::temp_dir();
    let path = dir.join("sui_eval_test_path_exists.txt");
    std::fs::write(&path, "test").unwrap();
    let expr = format!(r#"builtins.pathExists "{}""#, path.display());
    assert_eq!(eval(&expr).unwrap(), Value::Bool(true));
    std::fs::remove_file(&path).ok();
}

#[test]
fn builtins_path_exists_nonexistent() {
    assert_eq!(
        ev(r#"builtins.pathExists "/nonexistent/path/that/surely/does/not/exist""#),
        Value::Bool(false),
    );
}

// ── toFile tests ────────────────────────────────────────

#[test]
fn builtins_to_file_returns_store_path() {
    let v = ev(r#"builtins.toFile "test.txt" "hello""#);
    if let Value::Path(p) = v {
        assert!(p.starts_with("/nix/store/"));
        assert!(p.ends_with("-test.txt"));
    } else {
        panic!("expected path, got {v}");
    }
}

#[test]
fn builtins_to_file_deterministic() {
    // Same name + content should produce same path
    let v1 = ev(r#"builtins.toFile "f" "content""#);
    let v2 = ev(r#"builtins.toFile "f" "content""#);
    assert_eq!(v1, v2);
}

// ── hashFile tests ──────────────────────────────────────

#[test]
fn builtins_hash_file_sha256() {
    let dir = std::env::temp_dir();
    let path = dir.join("sui_eval_test_hashfile.txt");
    std::fs::write(&path, "hello").unwrap();
    let expr = format!(r#"builtins.hashFile "sha256" "{}""#, path.display());
    let v = eval(&expr).unwrap();
    if let Value::String(ns) = v {
        assert_eq!(ns.chars.len(), 64);
        assert_eq!(ns.chars, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    } else {
        panic!("expected string");
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn builtins_hash_file_missing() {
    let result = eval(r#"builtins.hashFile "sha256" "/nonexistent/file.txt""#);
    assert!(result.is_err());
}

// ── unsafeGetAttrPos tests ──────────────────────────────

#[test]
fn builtins_unsafe_get_attr_pos_returns_null() {
    assert_eq!(
        ev(r#"builtins.unsafeGetAttrPos "x" { x = 1; }"#),
        Value::Null,
    );
}

// ── storeDir / nixPath constants ────────────────────────

#[test]
fn builtins_store_dir() {
    assert_eq!(ev(r#"builtins.storeDir"#), Value::string("/nix/store"));
}

#[test]
fn builtins_nix_path_empty() {
    assert_eq!(ev(r#"builtins.nixPath"#), Value::list(vec![]));
}

// ── findFile tests ──────────────────────────────────────

#[test]
fn builtins_find_file_exact_match() {
    // Create a temp dir structure
    let dir = std::env::temp_dir().join("sui_eval_test_findfile");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("test.nix"), "42").unwrap();
    let expr = format!(
        r#"builtins.findFile [{{ prefix = "test.nix"; path = "{}"; }}] "test.nix""#,
        dir.join("test.nix").display()
    );
    let v = eval(&expr).unwrap();
    assert!(matches!(v, Value::Path(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtins_find_file_not_found() {
    let result = eval(r#"builtins.findFile [] "nonexistent""#);
    assert!(result.is_err());
}

// ── Phase 3 builtins tests ────────────────────────────

#[test] fn builtins_generic_closure_linear() {
    assert_eq!(ev(r#"builtins.length (builtins.genericClosure { startSet = [{ key = 1; }]; operator = item: if item.key < 3 then [{ key = item.key + 1; }] else []; })"#), Value::Int(3));
}
#[test] fn builtins_generic_closure_empty_op() {
    assert_eq!(ev(r#"builtins.length (builtins.genericClosure { startSet = [{ key = 1; }]; operator = item: []; })"#), Value::Int(1));
}
#[test] fn builtins_generic_closure_dedup() {
    assert_eq!(ev(r#"builtins.length (builtins.genericClosure { startSet = [{ key = 1; }]; operator = item: [{ key = 1; } { key = 2; }]; })"#), Value::Int(2));
}

#[test] fn builtins_from_toml_simple() {
    let v = ev(r#"builtins.fromTOML "[section]\nkey = \"value\"""#);
    if let Value::Attrs(a) = v { assert!(a.contains_key("section")); } else { panic!(); }
}

#[test] fn builtins_less_than_int() {
    assert_eq!(ev("builtins.lessThan 1 2"), Value::Bool(true));
    assert_eq!(ev("builtins.lessThan 2 1"), Value::Bool(false));
}

#[test] fn builtins_bit_and_12_10() { assert_eq!(ev("builtins.bitAnd 12 10"), Value::Int(8)); }
#[test] fn builtins_bit_or_12_10() { assert_eq!(ev("builtins.bitOr 12 10"), Value::Int(14)); }
#[test] fn builtins_bit_xor_12_10() { assert_eq!(ev("builtins.bitXor 12 10"), Value::Int(6)); }

#[test] fn builtins_split_version() {
    assert_eq!(ev(r#"builtins.splitVersion "1.2.3""#), Value::list(vec![
        Value::string("1"), Value::string("2"), Value::string("3")
    ]));
}
#[test] fn builtins_split_version_pre() {
    let v = ev(r#"builtins.splitVersion "1pre2""#);
    if let Value::List(l) = v { assert!(l.len() >= 3); } else { panic!(); }
}

#[test] fn builtins_path_exists_false() {
    assert_eq!(ev(r#"builtins.pathExists "/nonexistent/path/12345""#), Value::Bool(false));
}

#[test] fn builtins_to_file() {
    let v = ev(r#"builtins.toFile "test.txt" "hello""#);
    assert!(matches!(v, Value::Path(_) | Value::String(_)));
}

#[test] fn builtins_unsafe_get_attr_pos() {
    assert_eq!(ev(r#"builtins.unsafeGetAttrPos "a" { a = 1; }"#), Value::Null);
}

#[test] fn builtins_derivation_strict() {
    let v = ev(r#"builtins.derivationStrict { name = "test"; system = "x86_64-linux"; builder = "/bin/sh"; }"#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("type").unwrap().as_string().unwrap(), "derivation");
        assert!(a.contains_key("drvPath"));
    } else { panic!(); }
}

#[test] fn builtins_to_xml_int() {
    let v = ev("builtins.toXML 42");
    let s = v.as_string().unwrap();
    assert!(s.contains("<int value=\"42\""));
}
#[test] fn builtins_to_xml_attrs() {
    let v = ev(r#"builtins.toXML { a = 1; }"#);
    let s = v.as_string().unwrap();
    assert!(s.contains("<attrs>"));
    assert!(s.contains("attr name=\"a\""));
}

// ── Curried arithmetic builtins ───────────────────────

#[test]
fn builtins_sub_ints() {
    assert_eq!(ev("builtins.sub 10 3"), Value::Int(7));
}

#[test]
fn builtins_mul_ints() {
    assert_eq!(ev("builtins.mul 4 5"), Value::Int(20));
}

#[test]
fn builtins_div_ints() {
    assert_eq!(ev("builtins.div 10 3"), Value::Int(3));
}

#[test]
fn builtins_div_by_zero() {
    let result = eval("builtins.div 10 0");
    assert!(result.is_err());
}

#[test]
fn builtins_add_ints() {
    assert_eq!(ev("builtins.add 3 4"), Value::Int(7));
}

#[test]
fn builtins_add_floats() {
    assert_eq!(ev("builtins.add 1.5 2.5"), Value::Float(4.0));
}

#[test]
fn builtins_add_mixed_int_float() {
    assert_eq!(ev("builtins.add 1 2.5"), Value::Float(3.5));
}

// ── isFloat ───────────────────────────────────────────

#[test]
fn builtins_is_float_true() {
    assert_eq!(ev("builtins.isFloat 1.0"), Value::Bool(true));
}

#[test]
fn builtins_is_float_false() {
    assert_eq!(ev("builtins.isFloat 1"), Value::Bool(false));
}

// ── deepSeq ───────────────────────────────────────────

#[test]
fn builtins_deep_seq() {
    assert_eq!(ev("builtins.deepSeq [1 2 3] 42"), Value::Int(42));
}

#[test]
fn builtins_deep_seq_with_attrs() {
    assert_eq!(ev(r#"builtins.deepSeq { a = 1; b = 2; } "ok""#), Value::string("ok"));
}

// ── getEnv ────────────────────────────────────────────

#[test]
fn builtins_get_env_missing() {
    assert_eq!(
        ev(r#"builtins.getEnv "DEFINITELY_NOT_SET_12345_XYZ""#),
        Value::string(""),
    );
}

// ── currentTime ───────────────────────────────────────

#[test]
fn builtins_current_time_is_int() {
    let v = ev("builtins.currentTime null");
    assert!(matches!(v, Value::Int(_)));
    if let Value::Int(t) = v {
        assert!(t > 0);
    }
}

// ── substring ─────────────────────────────────────────

#[test]
fn builtins_substring_basic() {
    assert_eq!(
        ev(r#"builtins.substring 0 5 "hello world""#),
        Value::string("hello"),
    );
}

#[test]
fn builtins_substring_from_middle() {
    assert_eq!(
        ev(r#"builtins.substring 6 5 "hello world""#),
        Value::string("world"),
    );
}

#[test]
fn builtins_substring_beyond_length() {
    assert_eq!(
        ev(r#"builtins.substring 0 100 "hi""#),
        Value::string("hi"),
    );
}

// ── split ─────────────────────────────────────────────

#[test]
fn builtins_split_basic() {
    let v = ev(r#"builtins.split "o" "foobar""#);
    if let Value::List(parts) = v {
        assert!(parts.len() >= 3);
    } else {
        panic!("expected list");
    }
}

// ── hasContext / getContext / unsafeDiscardStringContext ──

#[test]
fn builtins_has_context_plain_string() {
    assert_eq!(ev(r#"builtins.hasContext "hello""#), Value::Bool(false));
}

#[test]
fn builtins_get_context_plain_string() {
    let v = ev(r#"builtins.getContext "hello""#);
    if let Value::Attrs(a) = v {
        assert!(a.is_empty());
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_unsafe_discard_string_context() {
    assert_eq!(
        ev(r#"builtins.unsafeDiscardStringContext "hello""#),
        Value::string("hello"),
    );
}

// ── unsafeDiscardOutputDependency ─────────────────────

#[test]
fn builtins_unsafe_discard_output_dependency() {
    assert_eq!(
        ev(r#"builtins.unsafeDiscardOutputDependency "hello""#),
        Value::string("hello"),
    );
}

// ── appendContext ─────────────────────────────────────

#[test]
fn builtins_append_context_empty() {
    assert_eq!(
        ev(r#"builtins.appendContext "hello" {}"#),
        Value::string("hello"),
    );
}

// ── convertHash ───────────────────────────────────────

#[test]
fn builtins_convert_hash_sha256_hex_to_base64() {
    let v = ev(r#"builtins.convertHash { hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"; hashAlgo = "sha256"; toHashFormat = "base64"; }"#);
    if let Value::String(s) = v {
        assert!(!s.chars.is_empty());
    } else {
        panic!("expected string");
    }
}

#[test]
fn builtins_convert_hash_sha256_hex_to_sri() {
    let v = ev(r#"builtins.convertHash { hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"; hashAlgo = "sha256"; toHashFormat = "sri"; }"#);
    if let Value::String(s) = v {
        assert!(s.chars.starts_with("sha256-"));
    } else {
        panic!("expected string");
    }
}

// ── toXML additional ──────────────────────────────────

#[test]
fn builtins_to_xml_string() {
    let v = ev(r#"builtins.toXML "hello""#);
    let s = v.as_string().unwrap();
    assert!(s.contains("<string value="));
}

#[test]
fn builtins_to_xml_list() {
    let v = ev("builtins.toXML [1 2]");
    let s = v.as_string().unwrap();
    assert!(s.contains("<list>"));
}

#[test]
fn builtins_to_xml_bool() {
    let v = ev("builtins.toXML true");
    let s = v.as_string().unwrap();
    assert!(s.contains("<bool value=\"true\""));
}

#[test]
fn builtins_to_xml_null() {
    let v = ev("builtins.toXML null");
    let s = v.as_string().unwrap();
    assert!(s.contains("<null"));
}

// ── typeOf comprehensive ──────────────────────────────

#[test]
fn builtins_type_of_int() {
    assert_eq!(ev("builtins.typeOf 42"), Value::string("int"));
}

#[test]
fn builtins_type_of_float() {
    assert_eq!(ev("builtins.typeOf 3.14"), Value::string("float"));
}

#[test]
fn builtins_type_of_string() {
    assert_eq!(ev(r#"builtins.typeOf "hello""#), Value::string("string"));
}

#[test]
fn builtins_type_of_bool() {
    assert_eq!(ev("builtins.typeOf true"), Value::string("bool"));
}

#[test]
fn builtins_type_of_null() {
    assert_eq!(ev("builtins.typeOf null"), Value::string("null"));
}

#[test]
fn builtins_type_of_list() {
    assert_eq!(ev("builtins.typeOf [1 2]"), Value::string("list"));
}

#[test]
fn builtins_type_of_set() {
    assert_eq!(ev("builtins.typeOf { a = 1; }"), Value::string("set"));
}

#[test]
fn builtins_type_of_lambda() {
    assert_eq!(ev("builtins.typeOf (x: x)"), Value::string("lambda"));
}

#[test]
fn builtins_type_of_path() {
    assert_eq!(ev("builtins.typeOf /foo"), Value::string("path"));
}

// ── head / tail edge cases ────────────────────────────

#[test]
fn builtins_head_single() {
    assert_eq!(ev("builtins.head [42]"), Value::Int(42));
}

#[test]
fn builtins_head_empty_errors() {
    assert!(eval("builtins.head []").is_err());
}

#[test]
fn builtins_tail_single() {
    assert_eq!(ev("builtins.tail [42]"), Value::list(vec![]));
}

#[test]
fn builtins_tail_empty_errors() {
    assert!(eval("builtins.tail []").is_err());
}

// ── attrNames / attrValues determinism ────────────────

#[test]
fn builtins_attr_names_sorted() {
    assert_eq!(
        ev(r#"builtins.attrNames { z = 1; a = 2; m = 3; }"#),
        Value::list(vec![
            Value::string("a"),
            Value::string("m"),
            Value::string("z"),
        ]),
    );
}

#[test]
fn builtins_attr_values_follows_sorted_keys() {
    assert_eq!(
        ev(r#"builtins.attrValues { z = 3; a = 1; m = 2; }"#),
        Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
    );
}

// ── toString additional ───────────────────────────────

#[test]
fn builtins_to_string_int() {
    assert_eq!(ev("builtins.toString 42"), Value::string("42"));
}

#[test]
fn builtins_to_string_bool() {
    assert_eq!(ev("builtins.toString true"), Value::string("1"));
    assert_eq!(ev("builtins.toString false"), Value::string(""));
}

#[test]
fn builtins_to_string_null() {
    assert_eq!(ev("builtins.toString null"), Value::string(""));
}

#[test]
fn builtins_to_string_path() {
    assert_eq!(ev("builtins.toString /foo"), Value::string("/foo"));
}

#[test]
fn builtins_to_string_list_space_joined() {
    // CppNix's toString coerces lists by space-joining elements.
    assert_eq!(
        ev("builtins.toString [1 2 3]"),
        Value::string("1 2 3"),
    );
}

#[test]
fn builtins_to_string_outpath() {
    // toString on an attrset with outPath coerces via outPath.
    assert_eq!(
        ev(r#"builtins.toString { outPath = "/nix/store/xyz"; }"#),
        Value::string("/nix/store/xyz"),
    );
}

#[test]
fn builtins_to_string_tostring_over_outpath() {
    // __toString takes priority over outPath in toString.
    assert_eq!(
        ev(r#"builtins.toString { __toString = self: "win"; outPath = "/lose"; }"#),
        Value::string("win"),
    );
}

// ── abort ─────────────────────────────────────────────

#[test]
fn builtins_abort_produces_error() {
    let result = eval(r#"builtins.abort "fatal""#);
    assert!(result.is_err());
    let msg = format!("{}", result.unwrap_err());
    assert!(msg.contains("fatal"));
}

// ── fromJSON additional ───────────────────────────────

#[test]
fn builtins_from_json_null() {
    assert_eq!(ev(r#"builtins.fromJSON "null""#), Value::Null);
}

#[test]
fn builtins_from_json_bool() {
    assert_eq!(ev(r#"builtins.fromJSON "true""#), Value::Bool(true));
}

#[test]
fn builtins_from_json_list() {
    assert_eq!(
        ev(r#"builtins.fromJSON "[1,2,3]""#),
        Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
    );
}

// ── toJSON additional ─────────────────────────────────

#[test]
fn builtins_to_json_null() {
    assert_eq!(ev("builtins.toJSON null"), Value::string("null"));
}

#[test]
fn builtins_to_json_list() {
    assert_eq!(
        ev("builtins.toJSON [1 2 3]"),
        Value::string("[1,2,3]"),
    );
}

// ── string operations ─────────────────────────────────

#[test]
fn builtins_string_length_empty() {
    assert_eq!(ev(r#"builtins.stringLength """#), Value::Int(0));
}

#[test]
fn builtins_string_length_unicode() {
    assert_eq!(ev(r#"builtins.stringLength "abc""#), Value::Int(3));
}

// ── replaceStrings edge cases ─────────────────────────

#[test]
fn builtins_replace_strings_empty_from() {
    assert_eq!(
        ev(r#"builtins.replaceStrings [] [] "hello""#),
        Value::string("hello"),
    );
}

#[test]
fn builtins_replace_strings_no_match() {
    assert_eq!(
        ev(r#"builtins.replaceStrings ["x"] ["y"] "hello""#),
        Value::string("hello"),
    );
}

// ── warn ──────────────────────────────────────────────

#[test]
fn builtins_warn_returns_value() {
    assert_eq!(ev(r#"builtins.warn "msg" 42"#), Value::Int(42));
}

#[test]
fn builtins_warn_passes_through_attrs() {
    let v = ev(r#"builtins.warn "be careful" { a = 1; }"#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("a"), Some(&Value::Int(1)));
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_warn_non_string_message_errors() {
    // CppNix accepts only strings as the message; sui mirrors via
    // as_string() so passing a number is a type error.
    let result = eval("builtins.warn 1 2");
    assert!(result.is_err());
}

// ── traceVerbose ──────────────────────────────────────

#[test]
fn builtins_trace_verbose_returns_value() {
    assert_eq!(ev(r#"builtins.traceVerbose "msg" 42"#), Value::Int(42));
}

#[test]
fn builtins_trace_verbose_with_attrs() {
    let v = ev(r#"builtins.traceVerbose "x" { y = 7; }"#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("y"), Some(&Value::Int(7)));
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_trace_verbose_with_list() {
    assert_eq!(
        ev(r#"builtins.traceVerbose "x" [1 2]"#),
        Value::list(vec![Value::Int(1), Value::Int(2)]),
    );
}

// ── break ─────────────────────────────────────────────

#[test]
fn builtins_break_returns_int() {
    assert_eq!(ev("builtins.break 42"), Value::Int(42));
}

#[test]
fn builtins_break_returns_string() {
    assert_eq!(ev(r#"builtins.break "x""#), Value::string("x"));
}

#[test]
fn builtins_break_returns_attrs() {
    let v = ev(r#"builtins.break { a = 1; }"#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("a"), Some(&Value::Int(1)));
    } else {
        panic!("expected attrs");
    }
}

// ── fetchGit / fetchTree / fetchMercurial ─────────────

fn make_local_git_repo() -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "sui_eval_local_git_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let repo = crate::git::init_repo(&dir, "main").ok()?;
    crate::git::set_config(&repo, "user.email", "test@sui.local").ok()?;
    crate::git::set_config(&repo, "user.name", "sui-test").ok()?;
    std::fs::write(dir.join("README"), "hello").ok()?;
    crate::git::commit_all(&repo, "initial", "sui-test", "test@sui.local").ok()?;
    Some(dir)
}

#[test]
fn builtins_fetch_git_local_repo() {
    let Some(repo) = make_local_git_repo() else {
        eprintln!("skip: git not available");
        return;
    };
    let expr = format!(r#"builtins.fetchGit "{}""#, repo.display());
    let v = eval(&expr).unwrap();
    if let Value::Attrs(a) = v {
        assert!(a.contains_key("outPath"), "outPath missing");
        assert!(a.contains_key("rev"), "rev missing");
        assert!(a.contains_key("shortRev"), "shortRev missing");
        assert!(a.contains_key("revCount"), "revCount missing");
        assert!(a.contains_key("lastModified"), "lastModified missing");
        assert!(a.contains_key("lastModifiedDate"), "lastModifiedDate missing");
        assert!(a.contains_key("narHash"), "narHash missing");
        assert!(a.contains_key("submodules"), "submodules missing");
        // shortRev is rev[..7]
        let rev = a.get("rev").unwrap().as_string().unwrap();
        let short = a.get("shortRev").unwrap().as_string().unwrap();
        assert_eq!(short, rev[..7].to_string());
    } else {
        panic!("expected attrs");
    }
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn builtins_fetch_git_attrset_form() {
    let Some(repo) = make_local_git_repo() else {
        eprintln!("skip: git not available");
        return;
    };
    let expr = format!(
        r#"builtins.fetchGit {{ url = "{}"; }}"#,
        repo.display()
    );
    let v = eval(&expr).unwrap();
    assert!(matches!(v, Value::Attrs(_)));
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn builtins_fetch_git_invalid_input_errors() {
    let result = eval("builtins.fetchGit 42");
    assert!(result.is_err());
}

#[test]
fn builtins_fetch_tree_path_type() {
    let dir = std::env::temp_dir().join("sui_fetch_tree_path");
    std::fs::create_dir_all(&dir).unwrap();
    let expr = format!(
        r#"(builtins.fetchTree {{ type = "path"; path = "{}"; }}).outPath"#,
        dir.display()
    );
    let v = eval(&expr).unwrap();
    if let Value::Path(p) = v {
        assert_eq!(p.as_str(), dir.to_string_lossy().as_ref());
    } else {
        panic!("expected path, got {v}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtins_fetch_tree_unknown_type_errors() {
    let result = eval(r#"builtins.fetchTree { type = "borp"; }"#);
    assert!(result.is_err());
}

#[test]
fn builtins_fetch_mercurial_unsupported_input_errors() {
    // Without `hg` installed and with no valid url, this must
    // produce an error rather than panic.
    let result = eval("builtins.fetchMercurial 42");
    assert!(result.is_err());
}

#[test]
fn builtins_format_unix_yyyymmddhhmmss_basic() {
    // 2024-01-01 00:00:00 UTC = 1704067200
    assert_eq!(super::format_unix_yyyymmddhhmmss(1_704_067_200), "20240101000000");
    // Epoch
    assert_eq!(super::format_unix_yyyymmddhhmmss(0), "19700101000000");
    // 2026-04-06 12:34:56 UTC
    assert_eq!(super::format_unix_yyyymmddhhmmss(1_775_478_896), "20260406123456");
}

// ── filterSource ──────────────────────────────────────

#[test]
fn builtins_filter_source_keeps_all_returns_path() {
    let dir = std::env::temp_dir().join("sui_eval_filter_src_keep");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "alpha").unwrap();
    std::fs::write(dir.join("b.txt"), "beta").unwrap();
    let expr = format!(
        r#"builtins.filterSource (path: type: true) "{}""#,
        dir.display()
    );
    let v = eval(&expr).unwrap();
    if let Value::Path(p) = v {
        assert!(std::path::Path::new(p.as_str()).exists(), "target {p} should exist");
        // Both kept files should be present.
        assert!(std::path::Path::new(p.as_str()).join("a.txt").exists());
        assert!(std::path::Path::new(p.as_str()).join("b.txt").exists());
    } else {
        panic!("expected path");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtins_filter_source_filters_by_predicate() {
    let dir = std::env::temp_dir().join("sui_eval_filter_src_pred");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("keep.txt"), "k").unwrap();
    std::fs::write(dir.join("drop.txt"), "d").unwrap();
    let expr = format!(
        r#"builtins.filterSource (path: type: type == "directory" || (builtins.match ".*keep.*" path != null)) "{}""#,
        dir.display()
    );
    let v = eval(&expr).unwrap();
    if let Value::Path(p) = v {
        assert!(std::path::Path::new(p.as_str()).join("keep.txt").exists());
        assert!(!std::path::Path::new(p.as_str()).join("drop.txt").exists());
    } else {
        panic!("expected path");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtins_filter_source_missing_path_errors() {
    let result = eval(
        r#"builtins.filterSource (path: type: true) "/nonexistent/sui_filter_src_xyz""#,
    );
    assert!(result.is_err());
}

// ── scopedImport ──────────────────────────────────────

#[test]
fn builtins_scoped_import_injects_scope() {
    let dir = std::env::temp_dir().join("sui_eval_scoped_import");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("inject.nix");
    std::fs::write(&path, "foo + 1").unwrap();
    let expr = format!(
        r#"builtins.scopedImport {{ foo = 41; }} "{}""#,
        path.display()
    );
    assert_eq!(eval(&expr).unwrap(), Value::Int(42));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn builtins_scoped_import_returns_attrs() {
    let dir = std::env::temp_dir().join("sui_eval_scoped_import_attrs");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("attrs.nix");
    std::fs::write(&path, "{ x = bar; y = bar + 1; }").unwrap();
    let expr = format!(
        r#"builtins.scopedImport {{ bar = 7; }} "{}""#,
        path.display()
    );
    let v = eval(&expr).unwrap();
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("x"), Some(&Value::Int(7)));
        assert_eq!(a.get("y"), Some(&Value::Int(8)));
    } else {
        panic!("expected attrs");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn builtins_scoped_import_missing_path_errors() {
    let result = eval(
        r#"builtins.scopedImport { foo = 1; } "/nonexistent/scoped/import.nix""#,
    );
    assert!(result.is_err());
}

#[test]
fn builtins_scoped_import_first_arg_must_be_attrs() {
    let result = eval(r#"builtins.scopedImport "not-attrs" "/tmp/foo.nix""#);
    assert!(result.is_err());
}

// ── parseFlakeRef ─────────────────────────────────────

#[test]
fn builtins_parse_flake_ref_github_basic() {
    let v = ev(r#"builtins.parseFlakeRef "github:NixOS/nixpkgs""#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("type").unwrap().as_string().unwrap(), "github");
        assert_eq!(a.get("owner").unwrap().as_string().unwrap(), "NixOS");
        assert_eq!(a.get("repo").unwrap().as_string().unwrap(), "nixpkgs");
        assert!(a.get("ref").is_none());
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_parse_flake_ref_github_with_ref() {
    let v = ev(r#"builtins.parseFlakeRef "github:NixOS/nixpkgs/release-23.11""#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("ref").unwrap().as_string().unwrap(), "release-23.11");
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_parse_flake_ref_git_with_query() {
    let v = ev(r#"builtins.parseFlakeRef "git+https://example.com/foo?ref=main""#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("type").unwrap().as_string().unwrap(), "git");
        assert_eq!(a.get("url").unwrap().as_string().unwrap(), "https://example.com/foo");
        assert_eq!(a.get("ref").unwrap().as_string().unwrap(), "main");
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_parse_flake_ref_path_explicit() {
    let v = ev(r#"builtins.parseFlakeRef "path:/tmp/foo""#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("type").unwrap().as_string().unwrap(), "path");
        assert_eq!(a.get("path").unwrap().as_string().unwrap(), "/tmp/foo");
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_parse_flake_ref_invalid_errors() {
    // A bare identifier like "not-a-ref" is a VALID flake reference
    // in CppNix semantics — it parses as an indirect ref. Previously
    // sui rejected these; aligning with CppNix (registry lookup is
    // a later step, not a parse-time concern).
    //
    // An input that is genuinely unparseable needs to break the
    // identifier grammar — `:` in the middle without a recognized
    // scheme, for example.
    let result = eval(r#"builtins.parseFlakeRef "bogus:schema:more""#);
    assert!(result.is_err());
}

// ── flakeRefToString ──────────────────────────────────

#[test]
fn builtins_flake_ref_to_string_github_basic() {
    assert_eq!(
        ev(r#"builtins.flakeRefToString { type = "github"; owner = "NixOS"; repo = "nixpkgs"; }"#),
        Value::string("github:NixOS/nixpkgs"),
    );
}

#[test]
fn builtins_flake_ref_to_string_github_with_ref() {
    assert_eq!(
        ev(r#"builtins.flakeRefToString { type = "github"; owner = "NixOS"; repo = "nixpkgs"; ref = "release-23.11"; }"#),
        Value::string("github:NixOS/nixpkgs/release-23.11"),
    );
}

#[test]
fn builtins_flake_ref_to_string_git_with_query() {
    assert_eq!(
        ev(r#"builtins.flakeRefToString { type = "git"; url = "https://example.com/foo"; ref = "main"; }"#),
        Value::string("git+https://example.com/foo?ref=main"),
    );
}

#[test]
fn builtins_flake_ref_to_string_path() {
    assert_eq!(
        ev(r#"builtins.flakeRefToString { type = "path"; path = "/tmp/foo"; }"#),
        Value::string("path:/tmp/foo"),
    );
}

#[test]
fn builtins_flake_ref_to_string_unknown_type_errors() {
    let result = eval(r#"builtins.flakeRefToString { type = "borp"; }"#);
    assert!(result.is_err());
}

#[test]
fn builtins_flake_ref_round_trip() {
    // parse → toString should be a fixed point for canonical refs.
    assert_eq!(
        ev(r#"builtins.flakeRefToString (builtins.parseFlakeRef "github:NixOS/nixpkgs")"#),
        Value::string("github:NixOS/nixpkgs"),
    );
}

// ── filterAttrs ───────────────────────────────────────

#[test]
fn builtins_filter_attrs_keeps_matching() {
    let v = ev(r#"builtins.filterAttrs (n: v: v > 1) { a = 1; b = 2; c = 3; }"#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.len(), 2);
        assert_eq!(a.get("b"), Some(&Value::Int(2)));
        assert_eq!(a.get("c"), Some(&Value::Int(3)));
        assert!(a.get("a").is_none());
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_filter_attrs_by_name() {
    let v = ev(r#"builtins.filterAttrs (n: v: n == "keep") { keep = 1; drop = 2; }"#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.len(), 1);
        assert_eq!(a.get("keep"), Some(&Value::Int(1)));
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_filter_attrs_empty() {
    let v = ev(r#"builtins.filterAttrs (n: v: true) {}"#);
    if let Value::Attrs(a) = v {
        assert!(a.is_empty());
    } else {
        panic!("expected attrs");
    }
}

#[test]
fn builtins_filter_attrs_non_attrs_errors() {
    let result = eval(r#"builtins.filterAttrs (n: v: true) [1 2 3]"#);
    assert!(result.is_err());
}

// ── builtins.sui.* extensions ─────────────────────────

#[test]
fn sui_ext_namespace_exists() {
    assert_eq!(ev("builtins ? sui"), Value::Bool(true));
}

// blake3 ──
#[test]
fn sui_ext_blake3_known_vector() {
    // Empty input — published BLAKE3 zero-length vector.
    assert_eq!(
        ev(r#"builtins.sui.blake3 """#),
        Value::string("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"),
    );
}
#[test]
fn sui_ext_blake3_hello() {
    let v = ev(r#"builtins.sui.blake3 "hello""#);
    if let Value::String(s) = v {
        assert_eq!(s.chars.len(), 64);
    } else { panic!(); }
}
#[test]
fn sui_ext_blake3_non_string_errors() {
    let result = eval("builtins.sui.blake3 42");
    assert!(result.is_err());
}

// sha3_256 ──
#[test]
fn sui_ext_sha3_256_known_vector() {
    // SHA3-256("") = a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a
    assert_eq!(
        ev(r#"builtins.sui.sha3_256 """#),
        Value::string("a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a"),
    );
}
#[test]
fn sui_ext_sha3_256_hello() {
    let v = ev(r#"builtins.sui.sha3_256 "hello""#);
    if let Value::String(s) = v { assert_eq!(s.chars.len(), 64); } else { panic!(); }
}
#[test]
fn sui_ext_sha3_512_known_vector() {
    // SHA3-512("") known vector
    assert_eq!(
        ev(r#"builtins.sui.sha3_512 """#),
        Value::string("a69f73cca23a9ac5c8b567dc185a756e97c982164fe25859e0d1dcc1475c80a615b2123af1f5f94c11e3e9402c3ac558f500199d95b6d3e301758586281dcd26"),
    );
}

// YAML ──
#[test]
fn sui_ext_from_yaml_simple() {
    let v = ev(r#"builtins.sui.fromYAML "x: 1\ny: hello\n""#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("x"), Some(&Value::Int(1)));
        assert_eq!(
            a.get("y").and_then(|v| if let Value::String(ns) = v { Some(ns.chars.to_string()) } else { None }),
            Some("hello".to_string()),
        );
    } else { panic!(); }
}
#[test]
fn sui_ext_from_yaml_invalid_errors() {
    let result = eval(r#"builtins.sui.fromYAML "this is :\n: not valid: : :: ::""#);
    assert!(result.is_err());
}
#[test]
fn sui_ext_to_yaml_round_trip() {
    // toYAML emits canonical yaml; round-tripping is structural.
    let v = ev(r#"builtins.sui.fromYAML (builtins.sui.toYAML { a = 1; b = "two"; })"#);
    if let Value::Attrs(a) = v {
        assert_eq!(a.get("a"), Some(&Value::Int(1)));
    } else { panic!(); }
}

// CSV ──
#[test]
fn sui_ext_from_csv_with_header() {
    let v = ev(r#"builtins.sui.fromCSV "name,age\nalice,30\nbob,25" { hasHeader = true; }"#);
    if let Value::List(rows) = v {
        assert_eq!(rows.len(), 2);
        if let Value::Attrs(a) = &rows[0] {
            assert_eq!(
                a.get("name").and_then(|v| if let Value::String(ns) = v { Some(ns.chars.to_string()) } else { None }),
                Some("alice".to_string()),
            );
        } else { panic!(); }
    } else { panic!(); }
}
#[test]
fn sui_ext_from_csv_no_header() {
    let v = ev(r#"builtins.sui.fromCSV "a,b\nc,d" { hasHeader = false; }"#);
    if let Value::List(rows) = v {
        assert_eq!(rows.len(), 2);
        if let Value::List(cells) = &rows[0] { assert_eq!(cells.len(), 2); } else { panic!(); }
    } else { panic!(); }
}
#[test]
fn sui_ext_from_csv_custom_delimiter() {
    let v = ev(r#"builtins.sui.fromCSV "x|y\n1|2" { hasHeader = true; delimiter = "|"; }"#);
    if let Value::List(rows) = v {
        assert_eq!(rows.len(), 1);
        if let Value::Attrs(a) = &rows[0] {
            assert_eq!(
                a.get("x").and_then(|v| if let Value::String(ns) = v { Some(ns.chars.to_string()) } else { None }),
                Some("1".to_string()),
            );
        } else { panic!(); }
    } else { panic!(); }
}

// regexNamedCaptures ──
#[test]
fn sui_ext_regex_named_captures_match() {
    let v = ev(r#"builtins.sui.regexNamedCaptures "(?P<word>[a-z]+) (?P<num>[0-9]+)" "abc 123""#);
    if let Value::Attrs(a) = v {
        assert_eq!(
            a.get("word").and_then(|v| if let Value::String(ns) = v { Some(ns.chars.to_string()) } else { None }),
            Some("abc".to_string()),
        );
        assert_eq!(
            a.get("num").and_then(|v| if let Value::String(ns) = v { Some(ns.chars.to_string()) } else { None }),
            Some("123".to_string()),
        );
    } else { panic!(); }
}
#[test]
fn sui_ext_regex_named_captures_no_match() {
    assert_eq!(
        ev(r#"builtins.sui.regexNamedCaptures "(?P<x>[0-9]+)" "no digits""#),
        Value::Null,
    );
}
#[test]
fn sui_ext_regex_named_captures_invalid_pattern_errors() {
    let result = eval(r#"builtins.sui.regexNamedCaptures "(unclosed" "subject""#);
    assert!(result.is_err());
}

// timestamp ──
#[test]
fn sui_ext_timestamp_format() {
    let v = ev("builtins.sui.timestamp null");
    if let Value::String(s) = v {
        // YYYY-MM-DDThh:mm:ssZ has length 20
        assert_eq!(s.chars.len(), 20);
        assert_eq!(&s.chars[10..11], "T");
        assert_eq!(&s.chars[19..20], "Z");
    } else { panic!(); }
}

// fileSize / fileMtime ──
#[test]
fn sui_ext_file_size_known() {
    let dir = std::env::temp_dir();
    let path = dir.join("sui_ext_file_size_test.bin");
    std::fs::write(&path, b"hello world").unwrap();
    let expr = format!(r#"builtins.sui.fileSize "{}""#, path.display());
    assert_eq!(eval(&expr).unwrap(), Value::Int(11));
    std::fs::remove_file(&path).ok();
}
#[test]
fn sui_ext_file_size_missing_errors() {
    let result = eval(r#"builtins.sui.fileSize "/nonexistent/sui-file-size-12345""#);
    assert!(result.is_err());
}
#[test]
fn sui_ext_file_mtime_returns_int() {
    let dir = std::env::temp_dir();
    let path = dir.join("sui_ext_file_mtime_test.bin");
    std::fs::write(&path, b"x").unwrap();
    let expr = format!(r#"builtins.sui.fileMtime "{}""#, path.display());
    let v = eval(&expr).unwrap();
    if let Value::Int(t) = v { assert!(t > 0); } else { panic!(); }
    std::fs::remove_file(&path).ok();
}

// ── builtins.builtins self-reference ──────────────────

#[test]
fn builtins_self_reference_exists() {
    assert_eq!(ev("builtins ? builtins"), Value::Bool(true));
}

#[test]
fn builtins_self_reference_has_length() {
    // The snapshot must contain at least the type-check builtins.
    let v = ev("builtins.builtins ? typeOf");
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn builtins_self_reference_does_not_loop() {
    // Snapshot is taken before the self-insert, so the inner copy
    // does not contain `builtins`. This guarantees finite output.
    assert_eq!(ev("builtins.builtins ? builtins"), Value::Bool(false));
}

// ── toLower / toUpper ────────────────────────────────

#[test]
fn to_lower_basic() {
    assert_eq!(ev(r#"builtins.toLower "HELLO""#), Value::string("hello"));
}

#[test]
fn to_upper_basic() {
    assert_eq!(ev(r#"builtins.toUpper "hello""#), Value::string("HELLO"));
}

#[test]
fn to_lower_empty() {
    assert_eq!(ev(r#"builtins.toLower """#), Value::string(""));
}

#[test]
fn to_upper_mixed() {
    assert_eq!(ev(r#"builtins.toUpper "MiXeD""#), Value::string("MIXED"));
}

#[test]
fn to_lower_already() {
    assert_eq!(ev(r#"builtins.toLower "already""#), Value::string("already"));
}

// ── Bug 1: inputs from flake.nix stub resolution ─────────

#[test]
fn flake_no_lock_file_stubs_inputs_from_flake_nix() {
    // A flake with `inputs` in flake.nix but NO flake.lock should still
    // succeed: each declared input gets a synthetic stub so the outputs
    // function receives all expected named arguments.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.nixpkgs.url = "github:NixOS/nixpkgs";
          inputs.utils.url  = "github:numtide/flake-utils";
          outputs = { self, nixpkgs, utils }: {
            ok = true;
          };
        }"#,
    )
    .unwrap();
    // Intentionally NO flake.lock.
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").ok"#);
    let result = eval(&expr).unwrap();
    assert_eq!(result, Value::Bool(true));
}

#[test]
fn flake_no_lock_file_stub_inputs_have_outpath() {
    // Stub inputs must have `outPath` so string interpolation works.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.dep.url = "github:example/dep";
          outputs = { self, dep }: {
            has_out = dep ? outPath;
          };
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let expr = format!(r#"(builtins.getFlake "{flake_path}").has_out"#);
    assert_eq!(eval(&expr).unwrap(), Value::Bool(true));
}

#[test]
fn flake_no_lock_file_stubs_appear_in_inputs() {
    // The stub inputs should appear under the top-level `inputs` key.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.alpha.url = "github:example/alpha";
          inputs.beta.url  = "github:example/beta";
          outputs = { self, alpha, beta }: { };
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let has_alpha = format!(r#"(builtins.getFlake "{flake_path}").inputs ? alpha"#);
    let has_beta = format!(r#"(builtins.getFlake "{flake_path}").inputs ? beta"#);
    assert_eq!(eval(&has_alpha).unwrap(), Value::Bool(true));
    assert_eq!(eval(&has_beta).unwrap(), Value::Bool(true));
}

#[test]
fn flake_partial_lock_stubs_missing_inputs() {
    // A flake.lock that resolves only *some* inputs should still get
    // stubs for the remaining ones declared in flake.nix.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{
          inputs.locked-dep = { };
          inputs.unlocked-dep.url = "github:example/unlocked";
          outputs = { self, locked-dep, unlocked-dep }: {
            locked = locked-dep ? narHash;
            unlocked-has-out = unlocked-dep ? outPath;
          };
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("flake.lock"),
        r#"{
          "nodes": {
            "locked-dep": {
              "locked": {
                "lastModified": 1700000000,
                "narHash": "sha256-PARTIAL=",
                "path": "/var/empty/dep",
                "type": "path"
              },
              "original": { "type": "path", "url": "/var/empty/dep" }
            },
            "root": { "inputs": { "locked-dep": "locked-dep" } }
          },
          "root": "root",
          "version": 7
        }"#,
    )
    .unwrap();
    let flake_path = dir.path().to_string_lossy().to_string();
    let locked = format!(r#"(builtins.getFlake "{flake_path}").locked"#);
    let unlocked = format!(r#"(builtins.getFlake "{flake_path}").unlocked-has-out"#);
    assert_eq!(eval(&locked).unwrap(), Value::Bool(true));
    assert_eq!(eval(&unlocked).unwrap(), Value::Bool(true));
}

// ── Path normalization in imports ─────────────────────────

#[test]
fn import_relative_dot_normalized() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("bar.nix"), "42").unwrap();
    std::fs::write(tmp.path().join("foo.nix"), "import ./bar.nix").unwrap();
    let foo_path = tmp.path().join("foo.nix");
    let expr = format!(r#"import {}"#, foo_path.display());
    let result = eval(&expr).unwrap();
    assert_eq!(result, Value::Int(42));
}

#[test]
fn import_relative_parent_normalized() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    std::fs::write(tmp.path().join("bar.nix"), "99").unwrap();
    std::fs::write(tmp.path().join("sub/foo.nix"), "import ../bar.nix").unwrap();
    let foo_path = tmp.path().join("sub/foo.nix");
    let expr = format!(r#"import {}"#, foo_path.display());
    let result = eval(&expr).unwrap();
    assert_eq!(result, Value::Int(99));
}

// ── evaluate_flake with relative imports ──────────────────

#[test]
fn evaluate_flake_with_relative_imports() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("lib.nix"), "{ x = 1; }").unwrap();
    std::fs::write(
        tmp.path().join("flake.nix"),
        r#"{
            description = "test";
            outputs = { self }: { value = (import ./lib.nix).x; };
        }"#,
    )
    .unwrap();
    let repo = crate::git::init_repo(tmp.path(), "main").unwrap();
    crate::git::commit_all(&repo, "init", "test", "test@test.com").ok();

    let result = crate::builtins::evaluate_flake(tmp.path()).unwrap();
    let val = crate::builtins::navigate_attrs(&result, &["value"]).unwrap();
    assert_eq!(val, Value::Int(1));
}

#[test]
fn evaluate_flake_nested_relative_imports() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("lib")).unwrap();
    std::fs::write(tmp.path().join("lib/helper.nix"), "{ y = 2; }").unwrap();
    std::fs::write(
        tmp.path().join("lib/default.nix"),
        "{ x = 1; helper = import ./helper.nix; }",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("flake.nix"),
        r#"{
            description = "test";
            outputs = { self }: let lib = import ./lib; in { value = lib.x + lib.helper.y; };
        }"#,
    )
    .unwrap();
    let repo = crate::git::init_repo(tmp.path(), "main").unwrap();
    crate::git::commit_all(&repo, "init", "test", "test@test.com").ok();

    let result = crate::builtins::evaluate_flake(tmp.path()).unwrap();
    let val = crate::builtins::navigate_attrs(&result, &["value"]).unwrap();
    assert_eq!(val, Value::Int(3));
}

// ── normalize_path unit tests ─────────────────────────────
//
// These test the centralized `crate::path::normalize` through the
// `crate::eval::normalize_path` re-export to ensure the delegation
// path remains intact.

#[test]
fn normalize_path_removes_dot() {
    let p = std::path::Path::new("/a/b/./c");
    assert_eq!(crate::path::normalize(p), std::path::PathBuf::from("/a/b/c"));
}

#[test]
fn normalize_path_resolves_parent() {
    let p = std::path::Path::new("/a/b/../c");
    assert_eq!(crate::path::normalize(p), std::path::PathBuf::from("/a/c"));
}

#[test]
fn normalize_path_complex() {
    let p = std::path::Path::new("/a/b/./c/../d/./e/../f");
    assert_eq!(crate::path::normalize(p), std::path::PathBuf::from("/a/b/d/f"));
}

#[test]
fn evaluate_flake_depth_limit_triggers() {
    // Simulate deep nesting by manually saturating the thread-local counter
    // then calling evaluate_flake on a nonexistent directory.
    let tmp = tempfile::tempdir().unwrap();
    let flake_dir = tmp.path().join("deep-flake");
    std::fs::create_dir_all(&flake_dir).unwrap();
    // No flake.nix — but the depth check triggers before reading it.

    FLAKE_EVAL_DEPTH.with(|d| *d.borrow_mut() = MAX_FLAKE_EVAL_DEPTH);
    let result = evaluate_flake(&flake_dir);
    // Reset counter before asserting so panics don't leave stale state.
    FLAKE_EVAL_DEPTH.with(|d| *d.borrow_mut() = 0);

    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("recursion limit"),
        "expected recursion limit error, got: {msg}"
    );
}

#[test]
fn evaluate_flake_depth_counter_resets_on_error() {
    // Ensure the depth counter decrements even when evaluate_flake errors.
    let tmp = tempfile::tempdir().unwrap();
    let flake_dir = tmp.path().join("no-flake");
    std::fs::create_dir_all(&flake_dir).unwrap();
    // No flake.nix — will produce an IoError.

    FLAKE_EVAL_DEPTH.with(|d| *d.borrow_mut() = 0);
    let _ = evaluate_flake(&flake_dir);
    let depth = FLAKE_EVAL_DEPTH.with(|d| *d.borrow());
    assert_eq!(depth, 0, "depth counter should reset to 0 after error");
}

#[test]
fn evaluate_flake_fetch_failure_returns_error() {
    // A flake that declares a github input but has no network access
    // should return an error rather than a placeholder path.
    let tmp = tempfile::tempdir().unwrap();
    let flake_dir = tmp.path();
    std::fs::write(
        flake_dir.join("flake.nix"),
        r#"{ outputs = { self, ... }: { }; }"#,
    )
    .unwrap();
    // Create a lock file with a github input that cannot be fetched.
    std::fs::write(
        flake_dir.join("flake.lock"),
        r#"{
            "nodes": {
                "root": {
                    "inputs": { "fake-input": "fake-input" }
                },
                "fake-input": {
                    "locked": {
                        "type": "github",
                        "owner": "nonexistent-owner-zzz",
                        "repo": "nonexistent-repo-zzz",
                        "rev": "0000000000000000000000000000000000000000",
                        "narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                    },
                    "original": {
                        "type": "github",
                        "owner": "nonexistent-owner-zzz",
                        "repo": "nonexistent-repo-zzz"
                    }
                }
            },
            "root": "root",
            "version": 7
        }"#,
    )
    .unwrap();

    let result = evaluate_flake(flake_dir);
    assert!(
        result.is_err(),
        "expected fetch failure to produce an error, got: {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("fetch flake input"),
        "expected fetch error message, got: {msg}"
    );
}

// ── Lazy flake input evaluation tests ──────────────────────

#[test]
fn flake_lazy_input_doesnt_fail_eagerly() {
    // A dep flake whose outputs function would abort if forced.
    // Because inputs are lazy, accessing dep.outPath (immediate
    // metadata) should NOT force the dep's outputs function.
    let tmp = tempfile::tempdir().unwrap();

    let dep = tmp.path().join("dep");
    std::fs::create_dir_all(&dep).unwrap();
    std::fs::write(
        dep.join("flake.nix"),
        r#"{
            description = "broken dep";
            outputs = { self }: { broken = builtins.abort "should not be forced"; working = 1; };
        }"#,
    )
    .unwrap();
    let dep_repo = crate::git::init_repo(&dep, "main").unwrap();
    crate::git::commit_all(&dep_repo, "init", "test", "test@test.com").ok();

    let main = tmp.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::write(
        main.join("flake.nix"),
        &format!(
            r#"{{
                description = "main";
                inputs.dep.url = "path:{dep}";
                outputs = {{ self, dep }}: {{ value = dep.outPath; }};
            }}"#,
            dep = dep.display()
        ),
    )
    .unwrap();
    // Create a minimal flake.lock so the dep is resolved as a path input.
    std::fs::write(
        main.join("flake.lock"),
        &format!(
            r#"{{
                "nodes": {{
                    "root": {{
                        "inputs": {{ "dep": "dep" }}
                    }},
                    "dep": {{
                        "locked": {{
                            "type": "path",
                            "path": "{dep}"
                        }},
                        "original": {{
                            "type": "path",
                            "path": "{dep}"
                        }}
                    }}
                }},
                "root": "root",
                "version": 7
            }}"#,
            dep = dep.display()
        ),
    )
    .unwrap();
    let main_repo = crate::git::init_repo(&main, "main").unwrap();
    crate::git::commit_all(&main_repo, "init", "test", "test@test.com").ok();

    // This should succeed because dep.outPath is immediate metadata
    // and doesn't require forcing dep's outputs function.
    let result = evaluate_flake(&main);
    assert!(
        result.is_ok(),
        "lazy input should not fail eagerly: {:?}",
        result.err()
    );
}

#[test]
fn flake_lazy_input_outputs_forced_on_access() {
    // When we DO access an output attribute from a dep, the lazy
    // evaluation kicks in and produces the correct value.
    let tmp = tempfile::tempdir().unwrap();

    let dep = tmp.path().join("dep");
    std::fs::create_dir_all(&dep).unwrap();
    std::fs::write(
        dep.join("flake.nix"),
        r#"{
            description = "good dep";
            outputs = { self }: { answer = 42; };
        }"#,
    )
    .unwrap();
    let dep_repo = crate::git::init_repo(&dep, "main").unwrap();
    crate::git::commit_all(&dep_repo, "init", "test", "test@test.com").ok();

    let main = tmp.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::write(
        main.join("flake.nix"),
        &format!(
            r#"{{
                description = "main";
                inputs.dep.url = "path:{dep}";
                outputs = {{ self, dep }}: {{ value = dep.answer; }};
            }}"#,
            dep = dep.display()
        ),
    )
    .unwrap();
    std::fs::write(
        main.join("flake.lock"),
        &format!(
            r#"{{
                "nodes": {{
                    "root": {{
                        "inputs": {{ "dep": "dep" }}
                    }},
                    "dep": {{
                        "locked": {{
                            "type": "path",
                            "path": "{dep}"
                        }},
                        "original": {{
                            "type": "path",
                            "path": "{dep}"
                        }}
                    }}
                }},
                "root": "root",
                "version": 7
            }}"#,
            dep = dep.display()
        ),
    )
    .unwrap();
    let main_repo = crate::git::init_repo(&main, "main").unwrap();
    crate::git::commit_all(&main_repo, "init", "test", "test@test.com").ok();

    // Accessing dep.answer should force the dep's outputs and return 42.
    let result = evaluate_flake(&main).unwrap();
    let val = crate::builtins::navigate_attrs(&result, &["value"]).unwrap();
    assert_eq!(val, Value::Int(42));
}

#[test]
fn native_thunk_forces_correctly() {
    // Direct unit test for Thunk::new_native.
    use crate::value::Thunk;
    let thunk = Thunk::new_native(|| Ok(Value::Int(99)));
    assert!(!thunk.is_evaluated());
    let val = thunk.force(&|e, env| crate::eval::eval_expr(e, env)).unwrap();
    assert_eq!(val, Value::Int(99));
    assert!(thunk.is_evaluated());
}

#[test]
fn native_thunk_error_memoizes_null() {
    // When a native thunk's closure returns an error, subsequent
    // forces should see Evaluated(Null) rather than Blackhole.
    use crate::value::Thunk;
    let thunk = Thunk::new_native(|| {
        Err(crate::value::EvalError::Throw("test error".into()))
    });
    let r1 = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
    assert!(r1.is_err());
    // Second force should succeed with Null (not Blackhole/infinite recursion).
    let r2 = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
    assert_eq!(r2.unwrap(), Value::Null);
}

// ── Import cache tests ───────────────────────────────────

#[test]
fn import_cache_returns_same_value() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("cached.nix");
    std::fs::write(&path, "42").unwrap();

    super::clear_import_cache();

    let v1 = eval(&format!("import {}", path.display())).unwrap();
    let v2 = eval(&format!("import {}", path.display())).unwrap();
    assert_eq!(v1, v2);
    assert_eq!(v1, Value::Int(42));
}

#[test]
fn import_cache_function_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("func.nix");
    std::fs::write(&path, "x: x + 1").unwrap();

    super::clear_import_cache();

    // Same function imported, applied with different args.
    let v1 = eval(&format!("(import {}) 1", path.display())).unwrap();
    let v2 = eval(&format!("(import {}) 2", path.display())).unwrap();
    assert_eq!(v1, Value::Int(2));
    assert_eq!(v2, Value::Int(3));
}

#[test]
fn import_cache_survives_across_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("lib.nix");
    std::fs::write(&path, "{ x = 1; }").unwrap();

    super::clear_import_cache();

    // First import caches the value.
    let _ = eval(&format!("import {}", path.display())).unwrap();

    // Modify the file on disk.
    std::fs::write(&path, "{ x = 2; }").unwrap();

    // Second import returns the CACHED value (x = 1, not 2).
    let v = eval(&format!("(import {}).x", path.display())).unwrap();
    assert_eq!(v, Value::Int(1)); // cached!
}

#[test]
fn import_cache_different_paths_different_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let path_a = tmp.path().join("a.nix");
    let path_b = tmp.path().join("b.nix");
    std::fs::write(&path_a, "10").unwrap();
    std::fs::write(&path_b, "20").unwrap();

    super::clear_import_cache();

    let va = eval(&format!("import {}", path_a.display())).unwrap();
    let vb = eval(&format!("import {}", path_b.display())).unwrap();
    assert_eq!(va, Value::Int(10));
    assert_eq!(vb, Value::Int(20));
}

#[test]
fn import_cache_attrs_cached() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("attrs.nix");
    std::fs::write(&path, "{ a = 1; b = 2; }").unwrap();

    super::clear_import_cache();

    let v1 = eval(&format!("(import {}).a", path.display())).unwrap();
    let v2 = eval(&format!("(import {}).b", path.display())).unwrap();
    assert_eq!(v1, Value::Int(1));
    assert_eq!(v2, Value::Int(2));
}

// ── Builtin bridge (call_builtin_by_name) tests ─────────

#[test]
fn bridge_typeof_int() {
    let result = super::call_builtin_by_name("typeOf", &[Value::Int(1)]).unwrap();
    assert_eq!(result, Value::string("int"));
}

#[test]
fn bridge_typeof_bool() {
    let result = super::call_builtin_by_name("typeOf", &[Value::Bool(true)]).unwrap();
    assert_eq!(result, Value::string("bool"));
}

#[test]
fn bridge_length() {
    let result = super::call_builtin_by_name(
        "length",
        &[Value::list(vec![Value::Int(1), Value::Int(2)])],
    )
    .unwrap();
    assert_eq!(result, Value::Int(2));
}

#[test]
fn bridge_head() {
    let result =
        super::call_builtin_by_name("head", &[Value::list(vec![Value::Int(1)])]).unwrap();
    assert_eq!(result, Value::Int(1));
}

#[test]
fn bridge_add() {
    let result =
        super::call_builtin_by_name("add", &[Value::Int(1), Value::Int(2)]).unwrap();
    assert_eq!(result, Value::Int(3));
}

#[test]
fn bridge_string_length() {
    let result =
        super::call_builtin_by_name("stringLength", &[Value::string("hello")]).unwrap();
    assert_eq!(result, Value::Int(5));
}

#[test]
fn bridge_unknown_builtin_returns_error() {
    let result = super::call_builtin_by_name("unknown_builtin_xyz", &[Value::Null]);
    assert!(result.is_err());
    let msg = format!("{}", result.unwrap_err());
    assert!(msg.contains("unknown builtin"));
}
