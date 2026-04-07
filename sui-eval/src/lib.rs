//! Clean-room Nix language evaluator.
//!
//! Architecture:
//! Nix source → rnix parser (CST) → Evaluator (values)
//!
//! Parsing is delegated to the `rnix` crate (MIT).

/// Core Nix builtins (90+ functions).
pub mod builtins;
/// MVP delegation to the real `nix` binary for complex evaluation.
pub mod delegate;
/// Tree-walking evaluator using rnix's typed AST.
pub mod eval;
/// Content-addressed input fetcher for flake.lock resolved inputs.
pub mod fetcher;
/// Native flake lock management — update, check, write.
pub mod flake_lock;
/// Nix value types, environments, thunks, and error types.
pub mod value;

/// Re-export flake lock types from sui-compat where they canonically live.
pub mod flake {
    pub use sui_compat::flake::*;
}

/// Evaluate a Nix expression string (convenience re-export).
pub use eval::eval;
/// Re-exported for ergonomic access from dependent crates.
pub use value::{EvalError, Value};

/// The evaluator trait — enables swapping evaluation strategies.
///
/// Implementations: tree-walking (current), bytecode VM (future),
/// delegation to external `nix eval` (fallback during transition).
pub trait Evaluator {
    /// Evaluate a Nix expression string.
    fn eval_expr(&self, input: &str) -> Result<Value, EvalError>;

    /// Evaluate a Nix file.
    fn eval_file(&self, path: &std::path::Path) -> Result<Value, EvalError>;
}

/// The default tree-walking evaluator.
pub struct TreeWalkEvaluator;

impl Evaluator for TreeWalkEvaluator {
    fn eval_expr(&self, input: &str) -> Result<Value, EvalError> {
        eval(input)
    }

    fn eval_file(&self, path: &std::path::Path) -> Result<Value, EvalError> {
        let source = std::fs::read_to_string(path)
            .map_err(|e| EvalError::IoError {
                context: format!("eval_file: {}", path.display()),
                message: e.to_string(),
            })?;
        eval(&source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockEvaluator(Result<Value, EvalError>);
    impl Evaluator for MockEvaluator {
        fn eval_expr(&self, _: &str) -> Result<Value, EvalError> {
            match &self.0 { Ok(v) => Ok(v.clone()), Err(_) => Err(EvalError::NotImplemented("mock".into())) }
        }
        fn eval_file(&self, _: &std::path::Path) -> Result<Value, EvalError> {
            self.eval_expr("")
        }
    }

    #[test]
    fn mock_evaluator_ok() {
        let e = MockEvaluator(Ok(Value::Int(42)));
        assert_eq!(e.eval_expr("anything").unwrap(), Value::Int(42));
    }

    #[test]
    fn mock_evaluator_err() {
        let e = MockEvaluator(Err(EvalError::NotImplemented("x".into())));
        assert!(e.eval_expr("anything").is_err());
    }

    #[test]
    fn tree_walk_evaluator() {
        let e = TreeWalkEvaluator;
        assert_eq!(e.eval_expr("1 + 2").unwrap(), Value::Int(3));
    }

    #[test]
    fn evaluator_trait_object_safe() {
        fn _assert(_: &dyn Evaluator) {}
    }

    // ── TreeWalkEvaluator through Evaluator trait ────────────

    #[test]
    fn tree_walk_eval_integer_arithmetic() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(e.eval_expr("2 + 3").unwrap(), Value::Int(5));
    }

    #[test]
    fn tree_walk_eval_string_literal() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(
            e.eval_expr(r#""hello world""#).unwrap(),
            Value::string("hello world"),
        );
    }

    #[test]
    fn tree_walk_eval_boolean() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(e.eval_expr("true && false").unwrap(), Value::Bool(false));
    }

    #[test]
    fn tree_walk_eval_if_else() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(
            e.eval_expr("if true then 42 else 0").unwrap(),
            Value::Int(42),
        );
    }

    #[test]
    fn tree_walk_eval_let_binding() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(
            e.eval_expr("let x = 10; in x * 2").unwrap(),
            Value::Int(20),
        );
    }

    #[test]
    fn tree_walk_eval_attrset() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let val = e.eval_expr("{ a = 1; b = 2; }.a").unwrap();
        assert_eq!(val, Value::Int(1));
    }

    #[test]
    fn tree_walk_eval_list() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let val = e.eval_expr("[1 2 3]").unwrap();
        assert_eq!(
            val,
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn tree_walk_eval_lambda_application() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(
            e.eval_expr("(x: x + 1) 5").unwrap(),
            Value::Int(6),
        );
    }

    #[test]
    fn tree_walk_eval_builtin_via_trait() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(
            e.eval_expr("builtins.length [1 2 3]").unwrap(),
            Value::Int(3),
        );
    }

    #[test]
    fn tree_walk_eval_parse_error_via_trait() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let result = e.eval_expr("let in");
        assert!(result.is_err());
    }

    #[test]
    fn tree_walk_eval_null_via_trait() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(e.eval_expr("null").unwrap(), Value::Null);
    }

    #[test]
    fn tree_walk_eval_file_missing() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let result = e.eval_file(std::path::Path::new("/nonexistent/file.nix"));
        assert!(result.is_err());
    }

    #[test]
    fn tree_walk_eval_string_interpolation_via_trait() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(
            e.eval_expr(r#"let name = "world"; in "hello ${name}""#).unwrap(),
            Value::string("hello world"),
        );
    }

    #[test]
    fn tree_walk_eval_comparison_via_trait() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(e.eval_expr("3 > 2").unwrap(), Value::Bool(true));
        assert_eq!(e.eval_expr("1 == 1").unwrap(), Value::Bool(true));
    }

    #[test]
    fn tree_walk_eval_recursive_attrset_via_trait() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(
            e.eval_expr("rec { x = 1; y = x + 1; }.y").unwrap(),
            Value::Int(2),
        );
    }

    // ── Re-exports & convenience eval shim ─────────────────

    #[test]
    fn re_export_eval_function_works() {
        // The top-level `eval` re-export from `eval::eval` should be
        // identical to the inner function.
        assert_eq!(eval("1 + 1").unwrap(), Value::Int(2));
    }

    #[test]
    fn re_export_value_and_error_types_constructible() {
        let v: Value = Value::Int(7);
        let e: EvalError = EvalError::UndefinedVar("x".into());
        assert_eq!(v.type_name(), "int");
        assert!(e.to_string().contains("undefined"));
    }

    // ── flake re-export from sui-compat ────────────────────

    #[test]
    fn flake_module_re_exports_compat_types() {
        // Smoke test that the flake submodule re-export compiles. We
        // reference a known type from the sui_compat::flake module via
        // our own re-export. Whatever types live there must be reachable.
        // We use the path explicitly to force compilation of the import.
        #[allow(unused_imports)]
        use crate::flake::*;
        // The block is intentionally empty: success is "this compiled".
    }

    // ── TreeWalkEvaluator passes path through ──────────────

    #[test]
    fn tree_walk_eval_file_with_real_temp_file() {
        let dir = std::env::temp_dir().join("sui-eval-test-tree-walk");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("simple.nix");
        std::fs::write(&path, "1 + 2").unwrap();
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let result = e.eval_file(&path).unwrap();
        assert_eq!(result, Value::Int(3));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn tree_walk_eval_file_propagates_io_error_kind() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let result = e.eval_file(std::path::Path::new("/nonexistent/never/exists.nix"));
        match result {
            Err(EvalError::IoError { context, .. }) => {
                assert!(context.contains("eval_file"));
            }
            other => panic!("expected IoError, got {other:?}"),
        }
    }

    #[test]
    fn tree_walk_eval_file_parse_error_propagates() {
        let dir = std::env::temp_dir().join("sui-eval-test-tw-parse");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad.nix");
        std::fs::write(&path, "let in").unwrap();
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let result = e.eval_file(&path);
        assert!(result.is_err());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    // ── Mock evaluator additional ──────────────────────────

    #[test]
    fn mock_evaluator_dispatched_via_trait_object() {
        let m: Box<dyn Evaluator> = Box::new(MockEvaluator(Ok(Value::Bool(true))));
        let r = m.eval_expr("anything").unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn mock_evaluator_eval_file_routes_through_eval_expr() {
        let m = MockEvaluator(Ok(Value::Int(1)));
        let r = m.eval_file(std::path::Path::new("/dev/null"));
        assert_eq!(r.unwrap(), Value::Int(1));
    }

    // ── Through-trait coverage of more constructs ──────────

    #[test]
    fn tree_walk_eval_function_with_default_args() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(
            e.eval_expr("({a, b ? 10}: a + b) {a = 5;}").unwrap(),
            Value::Int(15),
        );
    }

    #[test]
    fn tree_walk_eval_with_throws_propagated() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let result = e.eval_expr(r#"builtins.throw "boom""#);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.is_throw());
    }

    #[test]
    fn tree_walk_eval_assert_failure() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let result = e.eval_expr("assert false; 42");
        assert!(matches!(result, Err(EvalError::AssertionFailed)));
    }

    #[test]
    fn tree_walk_eval_division_by_zero() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let result = e.eval_expr("1 / 0");
        assert!(matches!(result, Err(EvalError::DivisionByZero)));
    }

    #[test]
    fn tree_walk_eval_undefined_variable() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let result = e.eval_expr("nonexistent_xyz");
        assert!(matches!(result, Err(EvalError::UndefinedVar(_))));
    }

    #[test]
    fn tree_walk_eval_path_literal() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let v = e.eval_expr("/tmp/x").unwrap();
        assert!(matches!(v, Value::Path(_)));
    }

    #[test]
    fn tree_walk_eval_float_literal() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        assert_eq!(e.eval_expr("3.14").unwrap(), Value::Float(3.14));
    }

    #[test]
    fn tree_walk_eval_lambda_returns_lambda() {
        let e: &dyn Evaluator = &TreeWalkEvaluator;
        let v = e.eval_expr("x: x").unwrap();
        assert!(matches!(v, Value::Lambda(_)));
    }
}
