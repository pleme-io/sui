//! Clean-room Nix language evaluator.
//!
//! Architecture:
//! Nix source → rnix parser (CST) → Evaluator (values)
//!
//! Parsing is delegated to the `rnix` crate (MIT).

pub mod builtins;
pub mod eval;
pub mod value;

/// Re-export flake lock types from sui-compat where they canonically live.
pub mod flake {
    pub use sui_compat::flake::*;
}

pub use eval::eval;
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
            .map_err(|e| EvalError::ParseError(format!("cannot read file: {e}")))?;
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
}
