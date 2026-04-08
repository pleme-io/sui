//! Error types for the bytecode compiler and VM.

/// Errors produced during bytecode compilation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    /// An AST construct that the compiler does not yet support.
    #[error("unsupported expression: {0}")]
    Unsupported(String),
    /// A required AST child node was missing.
    #[error("missing AST node: {0}")]
    MissingNode(String),
    /// The constant pool exceeded the u16 index limit.
    #[error("constant pool overflow (>{max} constants)", max = u16::MAX)]
    ConstantPoolOverflow,
    /// A jump target exceeded the u16 offset limit.
    #[error("jump offset overflow")]
    JumpOverflow,
    /// A local variable count exceeded the u16 limit.
    #[error("too many local variables")]
    TooManyLocals,
    /// Syntax error in the input.
    #[error("parse error: {0}")]
    ParseError(String),
}

/// Errors produced during bytecode VM execution.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VMError {
    /// A type mismatch at runtime.
    #[error("type error in {context}: expected {expected}, got {got}")]
    TypeError {
        expected: &'static str,
        got: &'static str,
        context: String,
    },
    /// Integer division by zero.
    #[error("division by zero")]
    DivisionByZero,
    /// An assertion failed.
    #[error("assertion failed")]
    AssertionFailed,
    /// Stack underflow (internal compiler/VM bug).
    #[error("stack underflow")]
    StackUnderflow,
    /// Invalid opcode byte encountered.
    #[error("invalid opcode: {0}")]
    InvalidOpcode(u8),
    /// Attempt to call a non-function value.
    #[error("not a function: {0}")]
    NotCallable(String),
    /// An undefined variable was referenced.
    #[error("undefined variable: {0}")]
    UndefinedVariable(String),
    /// An attribute was not found in an attrset.
    #[error("attribute not found: {0}")]
    AttrNotFound(String),
    /// Internal VM error (should not happen in correct programs).
    #[error("internal error: {0}")]
    Internal(String),
    /// Maximum call depth exceeded.
    #[error("stack overflow: call depth exceeded")]
    StackOverflow,
    /// A `throw` or `abort` was invoked.
    #[error("{0}")]
    Throw(String),
    /// An unknown builtin was referenced.
    #[error("unknown builtin: {0}")]
    UnknownBuiltin(String),
    /// A thunk entered infinite recursion (blackhole).
    #[error("infinite recursion detected")]
    InfiniteRecursion,
    /// An I/O error during import.
    #[error("import error: {0}")]
    ImportError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_error_display() {
        let e = CompileError::Unsupported("with".to_string());
        assert!(e.to_string().contains("with"));
    }

    #[test]
    fn vm_error_display() {
        let e = VMError::DivisionByZero;
        assert!(e.to_string().contains("division by zero"));
    }

    #[test]
    fn type_error_display() {
        let e = VMError::TypeError {
            expected: "int",
            got: "string",
            context: "addition".to_string(),
        };
        let msg = e.to_string();
        assert!(msg.contains("int"));
        assert!(msg.contains("string"));
        assert!(msg.contains("addition"));
    }
}
