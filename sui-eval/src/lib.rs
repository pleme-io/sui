//! Clean-room Nix language evaluator.
//!
//! Architecture:
//! Nix source → rnix parser (CST) → Evaluator (values)
//!
//! Parsing is delegated to the `rnix` crate (MIT).

pub mod builtins;
pub mod eval;
pub mod value;

pub use eval::eval;
pub use value::{EvalError, Value};
