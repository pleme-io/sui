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
}
