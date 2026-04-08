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
//! # Phase 1 + 2 Coverage
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
//! - Upvalue capture: Lua 5.x-style closures over non-local variables
//! - `with` scopes: dynamic variable lookup via with-scope stack
//! - `rec` attribute sets: self-referencing bindings
//! - `inherit` and `inherit (source)`: in both `let` and attrset
//! - Dotted attribute paths: `{ a.b = 1; a.c = 2; }` merging
//! - Dynamic attribute keys: `{ ${expr} = value; }`
//! - Builtins: 50+ functions (type checks, list ops, attrset ops,
//!   string ops, arithmetic, control flow, conversion)
//! - `import` with file caching
//! - Thunks / lazy evaluation (MakeThunk/Force opcodes, blackhole detection)
//!
//! # Not Yet Implemented
//!
//! - `scopedImport`
//! - Higher-order builtins that invoke VM closures (map, filter, foldl')
//! - `derivation` / `derivationStrict`
//! - String interpolation contexts

/// Built-in function registry for the VM.
pub mod builtins;
/// Bytecode container (instructions + constant pool).
pub mod chunk;
/// AST-to-bytecode compiler.
pub mod compiler;
/// Error types for compiler and VM.
pub mod error;
/// String interning for attribute names and identifiers.
pub mod intern;
/// NaN-boxed value representation for the VM stack.
pub mod nanbox;
/// Bytecode instruction set.
pub mod opcode;
/// VM-specific value representation.
pub mod value;
/// Bytecode interpreter / execution engine.
pub mod vm;

// Re-exports for ergonomic use.
pub use builtins::BuiltinRegistry;
pub use chunk::Chunk;
pub use compiler::Compiler;
pub use error::{CompileError, VMError};
pub use intern::{Interner, Symbol};
pub use opcode::OpCode;
pub use value::{StringKeyedValue, VMBuiltin, VMThunk, VMValue};
pub use vm::VM;

/// Result of bytecode evaluation: the value plus the interner needed
/// to resolve symbol-keyed attrsets.
pub struct EvalResult {
    /// The evaluated value (may contain `Symbol`-keyed attrsets).
    pub value: VMValue,
    /// The interner used during compilation and execution.
    pub interner: Interner,
}

impl EvalResult {
    /// Convert the result to a fully string-keyed value.
    #[must_use]
    pub fn to_string_keyed(&self) -> StringKeyedValue {
        self.value.to_string_keyed(&self.interner)
    }
}

/// Compile and execute a Nix expression string via the bytecode VM.
///
/// Returns the raw [`VMValue`] (which may contain `Symbol`-keyed attrsets).
/// For a fully resolved result, use [`eval_full`] instead.
pub fn eval(input: &str) -> Result<VMValue, EvalError> {
    let result = eval_full(input)?;
    Ok(result.value)
}

/// Compile and execute a Nix expression, returning the value and interner.
///
/// Use this when you need to inspect attrset keys or display results.
pub fn eval_full(input: &str) -> Result<EvalResult, EvalError> {
    let (chunk, mut interner) = Compiler::compile(input).map_err(EvalError::Compile)?;
    let value = VM::execute(chunk, &mut interner).map_err(EvalError::Runtime)?;
    Ok(EvalResult { value, interner })
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
