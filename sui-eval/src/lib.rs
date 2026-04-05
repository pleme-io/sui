//! Clean-room Nix language evaluator.
//!
//! Architecture:
//! Nix source → Lexer (tokens) → Parser (AST) → Evaluator (values)
//!
//! All code written from scratch. No vendored evaluator code.

pub mod ast;
pub mod builtins;
pub mod eval;
pub mod lexer;
pub mod parser;
pub mod value;

pub use eval::eval;
pub use value::{EvalError, Value};
