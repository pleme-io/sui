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
//! - Lazy attrset values (non-trivial values wrapped in thunks)
//! - `derivation` / `derivationStrict` (native implementation via sui-compat)
//! - `builtins.getFlake` (path-based flake references)
//! - `builtins.scopedImport` (with-wrapping approach)
//! - VM-level dispatch for interner-dependent builtins (attrNames,
//!   listToAttrs, removeAttrs, hasAttr, getAttr, catAttrs)
//! - Deep-force at VM boundary (recursively forces thunks in attrsets/lists)
//!
//! # Not Yet Implemented
//!
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
pub use vm::{FlakeResolverGuard, set_flake_resolver, VM};

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

/// A cached compilation result: the chunk (shared via Rc) and a cloned interner.
struct CachedCompile {
    chunk: Rc<Chunk>,
    interner: Interner,
}

thread_local! {
    /// Per-thread compilation cache keyed by expression string hash.
    ///
    /// Benchmarks show that compilation takes 85-92% of total eval time,
    /// so caching compiled chunks provides a dramatic speedup on repeated
    /// evaluations of the same expression (the common case in benchmarks
    /// and in real evaluation loops like `builtins.map` over many items).
    static COMPILE_CACHE: RefCell<HashMap<u64, CachedCompile>> =
        RefCell::new(HashMap::new());
}

/// Hash an expression string for the compile cache.
fn hash_expr(input: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}

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
///
/// Uses a thread-local compilation cache: if the same expression string
/// has been compiled before, the cached bytecode is reused (avoiding the
/// rnix parse + compile overhead which benchmarks show is 85-92% of total
/// eval time).
pub fn eval_full(input: &str) -> Result<EvalResult, EvalError> {
    let key = hash_expr(input);

    // Try the cache first.
    let cached = COMPILE_CACHE.with(|cache| {
        cache.borrow().get(&key).map(|entry| {
            (entry.chunk.clone(), entry.interner.clone())
        })
    });

    let (chunk, mut interner) = if let Some((rc_chunk, interner)) = cached {
        // Cache hit: use the Rc<Chunk> directly. The VM needs an owned Chunk,
        // so we clone from the Rc (the Rc makes this cheap for re-use).
        ((*rc_chunk).clone(), interner)
    } else {
        // Cache miss: compile, cache, and return.
        let (chunk, interner) = Compiler::compile(input).map_err(EvalError::Compile)?;
        let rc_chunk = Rc::new(chunk.clone());
        COMPILE_CACHE.with(|cache| {
            cache.borrow_mut().insert(key, CachedCompile {
                chunk: rc_chunk,
                interner: interner.clone(),
            });
        });
        (chunk, interner)
    };

    let value = VM::execute(chunk, &mut interner).map_err(EvalError::Runtime)?;
    Ok(EvalResult { value, interner })
}

/// Clear the thread-local compilation cache.
///
/// Useful in tests or when memory pressure is a concern.
pub fn clear_compile_cache() {
    COMPILE_CACHE.with(|cache| cache.borrow_mut().clear());
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

    #[test]
    fn eval_lazy_let_thunk() {
        // Non-trivial let binding should be lazily evaluated.
        assert_eq!(eval("let x = 2 * 3; in x").unwrap(), VMValue::Int(6));
    }

    #[test]
    fn eval_lazy_let_cross_ref() {
        // Let-binding thunks can reference other bindings from the same block.
        clear_compile_cache();
        assert_eq!(
            eval("let f = x: x + 1; g = f 10; in g").unwrap(),
            VMValue::Int(11)
        );
    }

    #[test]
    fn eval_fixpoint_via_intermediate() {
        // The fixpoint pattern works when accessed through an intermediate variable.
        clear_compile_cache();
        let result = eval(
            "let fix = f: let x = f x; in x; r = fix (self: { a = 1; }); s = r.a; in s",
        );
        assert_eq!(result.unwrap(), VMValue::Int(1));
    }
}
