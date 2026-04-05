//! Clean-room Nix language evaluator.
//!
//! Architecture:
//! Nix source → rnix parser (CST) → Evaluator (values)
//!
//! Parsing is delegated to the `rnix` crate (MIT).

pub mod builtins;
pub mod eval;
pub mod flake;
pub mod value;

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
