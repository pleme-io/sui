//! Bytecode compiler and VM for the Nix evaluator.
//!
//! This crate provides an alternative evaluation backend for sui-eval.
//! Instead of tree-walking the rnix AST, expressions are compiled to
//! a stack-based bytecode and executed by a virtual machine.
//!
//! # Architecture
//!
//! ```text
//! Nix source --> rnix parser (CST) --> Compiler --> Chunk (bytecode)
//!                                                        |
//!                                                        v
//!                                                  VM --> VMValue
//! ```
//!
//! # Phase 1 Coverage
//!
//! Currently supports:
//! - Literals: int, float, bool, null, string, path
//! - Arithmetic: `+`, `-`, `*`, `/`, unary `-`
//! - Comparison: `==`, `!=`, `<`, `>`, `<=`, `>=`
//! - Logical: `!`, `&&`, `||`, `->` (with short-circuit)
//! - Strings: literals, interpolation
//! - Variables: `let`/`in` with local binding
//! - Functions: lambda, apply, pattern destructuring with defaults
//! - Lists: construction, `++` concatenation
//! - Attribute sets: construction, `.` selection, `?` has-attr,
//!   `//` update, `or` default
//! - Control flow: `if`/`then`/`else`, `assert`
//!
//! # Not Yet Implemented
//!
//! - `with` scopes
//! - `rec` attribute sets
//! - Thunks / lazy evaluation
//! - Upvalue capture (closures over non-local variables)
//! - `import` / `scopedImport`
//! - Builtins (100+ functions)
//! - String interpolation contexts
//! - Dotted attribute paths in let bindings
//! - Dynamic attribute keys
//! - `inherit (source)` in let/attrset

/// Bytecode container (instructions + constant pool).
pub mod chunk;
/// AST-to-bytecode compiler.
pub mod compiler;
/// Error types for compiler and VM.
pub mod error;
/// Bytecode instruction set.
pub mod opcode;
/// VM-specific value representation.
pub mod value;
/// Bytecode interpreter / execution engine.
pub mod vm;

// Re-exports for ergonomic use.
pub use chunk::Chunk;
pub use compiler::Compiler;
pub use error::{CompileError, VMError};
pub use opcode::OpCode;
pub use value::VMValue;
pub use vm::VM;

/// Compile and execute a Nix expression string via the bytecode VM.
///
/// This is the main entry point for bytecode evaluation. Equivalent to
/// `sui_eval::eval` but uses the bytecode path instead of tree-walking.
pub fn eval(input: &str) -> Result<VMValue, EvalError> {
    let chunk = Compiler::compile(input).map_err(EvalError::Compile)?;
    VM::execute(chunk).map_err(EvalError::Runtime)
}

/// Unified error type wrapping both compile and runtime errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvalError {
    /// A compilation error.
    #[error("compile error: {0}")]
    Compile(CompileError),
    /// A runtime error.
    #[error("runtime error: {0}")]
    Runtime(VMError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_simple_addition() {
        assert_eq!(eval("1 + 2").unwrap(), VMValue::Int(3));
    }

    #[test]
    fn eval_null_literal() {
        assert_eq!(eval("null").unwrap(), VMValue::Null);
    }

    #[test]
    fn eval_bool_logic() {
        assert_eq!(eval("true && false").unwrap(), VMValue::Bool(false));
        assert_eq!(eval("true || false").unwrap(), VMValue::Bool(true));
    }

    #[test]
    fn eval_let_binding() {
        assert_eq!(eval("let x = 10; in x").unwrap(), VMValue::Int(10));
    }

    #[test]
    fn eval_lambda_call() {
        assert_eq!(eval("(x: x + 1) 5").unwrap(), VMValue::Int(6));
    }

    #[test]
    fn eval_compile_error() {
        let result = eval("let in");
        assert!(result.is_err());
        assert!(matches!(result, Err(EvalError::Compile(_))));
    }

    #[test]
    fn eval_runtime_error_div_zero() {
        let result = eval("1 / 0");
        assert!(result.is_err());
        assert!(matches!(
            result,
            Err(EvalError::Runtime(VMError::DivisionByZero))
        ));
    }
}
