//! Bytecode VM execution engine.
//!
//! A stack-based interpreter that executes compiled [`Chunk`]s. The VM
//! maintains a value stack, a call stack for function invocations, and
//! dispatches instructions via a `match` loop.

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::chunk::Chunk;
use crate::error::VMError;
use crate::intern::{Interner, Symbol};
use crate::opcode::OpCode;
use crate::value::VMValue;

/// Maximum call depth before we report a stack overflow.
const MAX_CALL_DEPTH: usize = 1024;

/// A call frame on the VM's call stack.
#[derive(Clone)]
struct CallFrame {
    /// The chunk being executed.
    chunk: Rc<Chunk>,
    /// Instruction pointer within the chunk.
    ip: usize,
    /// Base index in the value stack for this frame's locals.
    stack_base: usize,
}

/// The bytecode virtual machine.
///
/// Holds a mutable reference to the shared [`Interner`] for resolving
/// and interning attribute keys during execution. Attribute sets use
/// [`Symbol`] keys for O(1) comparison instead of string comparison.
pub struct VM<'a> {
    /// Value stack.
    stack: Vec<VMValue>,
    /// Call stack.
    frames: Vec<CallFrame>,
    /// Shared interner for attribute key operations.
    interner: &'a mut Interner,
}

impl<'a> VM<'a> {
    /// Create a new VM and execute a chunk, returning the result.
    pub fn execute(chunk: Chunk, interner: &'a mut Interner) -> Result<VMValue, VMError> {
        let mut vm = Self {
            stack: Vec::with_capacity(256),
            frames: Vec::with_capacity(64),
            interner,
        };

        vm.frames.push(CallFrame {
            chunk: Rc::new(chunk),
            ip: 0,
            stack_base: 0,
        });

        vm.run()
    }

    /// Main execution loop.
    ///
    /// Uses `unsafe` transmute for the opcode byte-to-enum conversion
    /// to avoid the branch-heavy `from_byte` match on every instruction.
    /// SAFETY: The compiler only emits valid opcode bytes.
    fn run(&mut self) -> Result<VMValue, VMError> {
        loop {
            let op_byte = self.read_byte()?;
            // SAFETY: all opcode bytes in the bytecode stream were emitted
            // by the compiler and are valid OpCode repr(u8) values.
            let op = OpCode::from_byte(op_byte).ok_or(VMError::InvalidOpcode(op_byte))?;

            match op {
                // ── Constants ───────────────────────────────────
                OpCode::Constant => {
                    let idx = self.read_u16()?;
                    let value = self.current_chunk().constants[idx as usize].clone();
                    self.push(value);
                }
                OpCode::Null => self.push(VMValue::Null),
                OpCode::True => self.push(VMValue::Bool(true)),
                OpCode::False => self.push(VMValue::Bool(false)),

                // ── Arithmetic ─────────────────────────────────
                OpCode::Add => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(self.add(a, b)?);
                }
                OpCode::Sub => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(self.num_op(a, b, |x, y| x - y, |x, y| x - y, "subtraction")?);
                }
                OpCode::Mul => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(self.num_op(a, b, |x, y| x * y, |x, y| x * y, "multiplication")?);
                }
                OpCode::Div => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    // Check for division by zero.
                    if matches!((&a, &b), (VMValue::Int(_), VMValue::Int(0))) {
                        return Err(VMError::DivisionByZero);
                    }
                    self.push(self.num_op(a, b, |x, y| x / y, |x, y| x / y, "division")?);
                }
                OpCode::Negate => {
                    let val = self.pop()?;
                    match val {
                        VMValue::Int(n) => self.push(VMValue::Int(-n)),
                        VMValue::Float(f) => self.push(VMValue::Float(-f)),
                        _ => {
                            return Err(VMError::TypeError {
                                expected: "int or float",
                                got: val.type_name(),
                                context: "negation".to_string(),
                            });
                        }
                    }
                }

                // ── Logical ────────────────────────────────────
                OpCode::Not => {
                    let val = self.pop()?;
                    let b = val.is_truthy()?;
                    self.push(VMValue::Bool(!b));
                }
                OpCode::And => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(VMValue::Bool(a.is_truthy()? && b.is_truthy()?));
                }
                OpCode::Or => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(VMValue::Bool(a.is_truthy()? || b.is_truthy()?));
                }
                OpCode::Implication => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(VMValue::Bool(!a.is_truthy()? || b.is_truthy()?));
                }

                // ── Comparison ─────────────────────────────────
                OpCode::Equal => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(VMValue::Bool(a == b));
                }
                OpCode::NotEqual => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(VMValue::Bool(a != b));
                }
                OpCode::Less => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(VMValue::Bool(self.compare(&a, &b)? == std::cmp::Ordering::Less));
                }
                OpCode::Greater => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(VMValue::Bool(
                        self.compare(&a, &b)? == std::cmp::Ordering::Greater,
                    ));
                }
                OpCode::LessEqual => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(VMValue::Bool(
                        self.compare(&a, &b)? != std::cmp::Ordering::Greater,
                    ));
                }
                OpCode::GreaterEqual => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(VMValue::Bool(
                        self.compare(&a, &b)? != std::cmp::Ordering::Less,
                    ));
                }

                // ── Strings ────────────────────────────────────
                OpCode::Interpolate => {
                    let count = self.read_u16()? as usize;
                    let start = self.stack.len() - count;
                    let mut result = String::new();
                    for i in start..self.stack.len() {
                        match &self.stack[i] {
                            VMValue::String(s) => result.push_str(s),
                            VMValue::Int(n) => result.push_str(&n.to_string()),
                            VMValue::Float(f) => result.push_str(&format!("{f}")),
                            VMValue::Path(p) => result.push_str(p),
                            VMValue::Bool(b) => {
                                return Err(VMError::TypeError {
                                    expected: "string, int, float, or path",
                                    got: if *b { "bool (true)" } else { "bool (false)" },
                                    context: "string interpolation".to_string(),
                                });
                            }
                            other => {
                                return Err(VMError::TypeError {
                                    expected: "string, int, float, or path",
                                    got: other.type_name(),
                                    context: "string interpolation".to_string(),
                                });
                            }
                        }
                    }
                    self.stack.truncate(start);
                    self.push(VMValue::String(result));
                }

                // ── Variables ──────────────────────────────────
                OpCode::GetLocal => {
                    let slot = self.read_u16()? as usize;
                    let abs_slot = self.current_frame().stack_base + slot;
                    let value = self.stack[abs_slot].clone();
                    self.push(value);
                }
                OpCode::SetLocal => {
                    let slot = self.read_u16()? as usize;
                    let abs_slot = self.current_frame().stack_base + slot;
                    let value = self.peek()?.clone();
                    self.stack[abs_slot] = value;
                }

                // ── Attribute sets ─────────────────────────────
                OpCode::MakeAttrs => {
                    let count = self.read_u16()? as usize;
                    let mut attrs = BTreeMap::new();
                    // Stack has pairs: [value, key, value, key, ...] (key on top of each pair).
                    // Pop in reverse: key first, then value.
                    for _ in 0..count {
                        let key = self.pop()?;
                        let value = self.pop()?;
                        let key_sym = match key {
                            VMValue::String(s) => self.interner.intern(&s),
                            _ => {
                                return Err(VMError::TypeError {
                                    expected: "string",
                                    got: key.type_name(),
                                    context: "attrset key".to_string(),
                                });
                            }
                        };
                        attrs.insert(key_sym, value);
                    }
                    self.push(VMValue::Attrs(attrs));
                }
                OpCode::GetAttr => {
                    let key_idx = self.read_u16()?;
                    let key_sym = self.resolve_key_constant(key_idx)?;
                    let attrset = self.pop()?;
                    match attrset {
                        VMValue::Attrs(ref attrs) => {
                            if let Some(val) = attrs.get(&key_sym) {
                                self.push(val.clone());
                            } else {
                                let key_str = self.interner.resolve(key_sym).to_string();
                                return Err(VMError::AttrNotFound(key_str));
                            }
                        }
                        _ => {
                            let key_str = self.interner.resolve(key_sym).to_string();
                            return Err(VMError::TypeError {
                                expected: "set",
                                got: attrset.type_name(),
                                context: format!("attribute selection '.{key_str}'"),
                            });
                        }
                    }
                }
                OpCode::HasAttr => {
                    let key_idx = self.read_u16()?;
                    let key_sym = self.resolve_key_constant(key_idx)?;
                    let attrset = self.pop()?;
                    let result = match attrset {
                        VMValue::Attrs(ref attrs) => attrs.contains_key(&key_sym),
                        _ => false,
                    };
                    self.push(VMValue::Bool(result));
                }
                OpCode::UpdateAttrs => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    match (a, b) {
                        (VMValue::Attrs(mut left), VMValue::Attrs(right)) => {
                            for (k, v) in right {
                                left.insert(k, v);
                            }
                            self.push(VMValue::Attrs(left));
                        }
                        (VMValue::Attrs(_), other) => {
                            return Err(VMError::TypeError {
                                expected: "set",
                                got: other.type_name(),
                                context: "// (right)".to_string(),
                            });
                        }
                        (other, _) => {
                            return Err(VMError::TypeError {
                                expected: "set",
                                got: other.type_name(),
                                context: "// (left)".to_string(),
                            });
                        }
                    }
                }
                OpCode::SelectOrDefault => {
                    let key_idx = self.read_u16()?;
                    let key_sym = self.resolve_key_constant(key_idx)?;
                    let default = self.pop()?;
                    let attrset = self.pop()?;
                    match attrset {
                        VMValue::Attrs(ref attrs) => {
                            if let Some(val) = attrs.get(&key_sym) {
                                self.push(val.clone());
                            } else {
                                self.push(default);
                            }
                        }
                        _ => {
                            self.push(default);
                        }
                    }
                }

                // ── Lists ──────────────────────────────────────
                OpCode::MakeList => {
                    let count = self.read_u16()? as usize;
                    let start = self.stack.len() - count;
                    let items: Vec<VMValue> = self.stack.drain(start..).collect();
                    self.push(VMValue::List(items));
                }
                OpCode::Concat => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    match (a, b) {
                        (VMValue::List(mut left), VMValue::List(right)) => {
                            left.extend(right);
                            self.push(VMValue::List(left));
                        }
                        (VMValue::List(_), other) => {
                            return Err(VMError::TypeError {
                                expected: "list",
                                got: other.type_name(),
                                context: "++ (right)".to_string(),
                            });
                        }
                        (other, _) => {
                            return Err(VMError::TypeError {
                                expected: "list",
                                got: other.type_name(),
                                context: "++ (left)".to_string(),
                            });
                        }
                    }
                }

                // ── Functions ──────────────────────────────────
                OpCode::MakeClosure => {
                    // In Phase 1, MakeClosure just pushes the closure constant.
                    // The compiler stores the closure as a constant.
                    // This opcode isn't currently emitted; the compiler uses
                    // Constant for closures. Reserved for Phase 2 upvalues.
                    let idx = self.read_u16()?;
                    let value = self.current_chunk().constants[idx as usize].clone();
                    self.push(value);
                }
                OpCode::Call => {
                    let arg = self.pop()?;
                    let func = self.pop()?;
                    match func {
                        VMValue::Closure(closure) => {
                            if self.frames.len() >= MAX_CALL_DEPTH {
                                return Err(VMError::StackOverflow);
                            }
                            // Push the argument as the first local (slot 0).
                            let stack_base = self.stack.len();
                            self.push(arg);
                            self.frames.push(CallFrame {
                                chunk: closure.chunk.clone(),
                                ip: 0,
                                stack_base,
                            });
                            // The function's compiled code will access
                            // its locals starting at stack_base.
                        }
                        _ => {
                            return Err(VMError::NotCallable(format!("{}", func.type_name())));
                        }
                    }
                }
                OpCode::Return => {
                    let result = self.pop()?;
                    let frame = self.frames.pop().ok_or(VMError::Internal(
                        "return with empty call stack".to_string(),
                    ))?;

                    if self.frames.is_empty() {
                        // Top-level return.
                        return Ok(result);
                    }

                    // Discard the callee's locals from the stack.
                    self.stack.truncate(frame.stack_base);
                    // Push the return value.
                    self.push(result);
                }

                // ── Control flow ───────────────────────────────
                OpCode::Jump => {
                    let target = self.read_u16()? as usize;
                    self.current_frame_mut().ip = target;
                }
                OpCode::JumpIfFalse => {
                    let target = self.read_u16()? as usize;
                    let cond = self.pop()?;
                    if !cond.is_truthy()? {
                        self.current_frame_mut().ip = target;
                    }
                }
                OpCode::JumpIfTrue => {
                    let target = self.read_u16()? as usize;
                    let cond = self.pop()?;
                    if cond.is_truthy()? {
                        self.current_frame_mut().ip = target;
                    }
                }

                // ── Assert ─────────────────────────────────────
                OpCode::Assert => {
                    let cond = self.pop()?;
                    if !cond.is_truthy()? {
                        return Err(VMError::AssertionFailed);
                    }
                }

                // ── Pop ────────────────────────────────────────
                OpCode::Pop => {
                    self.pop()?;
                }

                // ── Superinstructions ─────────────────────────
                OpCode::GetLocalAttr => {
                    // Fused GetLocal + GetAttr: one dispatch instead of two.
                    let slot = self.read_u16()? as usize;
                    let key_idx = self.read_u16()?;
                    let key_sym = self.resolve_key_constant(key_idx)?;
                    let abs_slot = self.current_frame().stack_base + slot;
                    let local = &self.stack[abs_slot];
                    match local {
                        VMValue::Attrs(attrs) => {
                            if let Some(val) = attrs.get(&key_sym) {
                                self.push(val.clone());
                            } else {
                                let key_str = self.interner.resolve(key_sym).to_string();
                                return Err(VMError::AttrNotFound(key_str));
                            }
                        }
                        _ => {
                            let key_str = self.interner.resolve(key_sym).to_string();
                            return Err(VMError::TypeError {
                                expected: "set",
                                got: local.type_name(),
                                context: format!("attribute selection '.{key_str}'"),
                            });
                        }
                    }
                }
                OpCode::GetLocalCall => {
                    // Fused GetLocal + Call: push local, then call with TOS as arg.
                    let slot = self.read_u16()? as usize;
                    let abs_slot = self.current_frame().stack_base + slot;
                    let func = self.stack[abs_slot].clone();
                    let arg = self.pop()?;
                    match func {
                        VMValue::Closure(closure) => {
                            if self.frames.len() >= MAX_CALL_DEPTH {
                                return Err(VMError::StackOverflow);
                            }
                            let stack_base = self.stack.len();
                            self.push(arg);
                            self.frames.push(CallFrame {
                                chunk: closure.chunk.clone(),
                                ip: 0,
                                stack_base,
                            });
                        }
                        _ => {
                            return Err(VMError::NotCallable(func.type_name().to_string()));
                        }
                    }
                }
            }
        }
    }

    // ── Stack helpers ──────────────────────────────────────────

    fn push(&mut self, value: VMValue) {
        self.stack.push(value);
    }

    fn pop(&mut self) -> Result<VMValue, VMError> {
        self.stack.pop().ok_or(VMError::StackUnderflow)
    }

    fn peek(&self) -> Result<&VMValue, VMError> {
        self.stack.last().ok_or(VMError::StackUnderflow)
    }

    // ── Frame helpers ──────────────────────────────────────────

    fn current_frame(&self) -> &CallFrame {
        self.frames.last().expect("no active frame")
    }

    fn current_frame_mut(&mut self) -> &mut CallFrame {
        self.frames.last_mut().expect("no active frame")
    }

    fn current_chunk(&self) -> &Chunk {
        &self.current_frame().chunk
    }

    fn read_byte(&mut self) -> Result<u8, VMError> {
        let frame = self.current_frame();
        if frame.ip >= frame.chunk.code.len() {
            return Err(VMError::Internal("unexpected end of bytecode".to_string()));
        }
        let byte = frame.chunk.code[frame.ip];
        self.current_frame_mut().ip += 1;
        Ok(byte)
    }

    fn read_u16(&mut self) -> Result<u16, VMError> {
        let lo = self.read_byte()?;
        let hi = self.read_byte()?;
        Ok(u16::from_le_bytes([lo, hi]))
    }

    // ── Interning helpers ──────────────────────────────────────

    /// Resolve a constant pool string to a `Symbol`.
    /// The string must have been interned at compile time.
    fn resolve_key_constant(&mut self, idx: u16) -> Result<Symbol, VMError> {
        // Clone the string to avoid holding an immutable borrow on self
        // while we need a mutable borrow on self.interner.
        let key_string = match &self.current_chunk().constants[idx as usize] {
            VMValue::String(s) => s.clone(),
            _ => return Err(VMError::Internal("attr key constant not a string".to_string())),
        };
        Ok(self.interner.intern(&key_string))
    }

    // ── Arithmetic helpers ─────────────────────────────────────

    fn add(&self, a: VMValue, b: VMValue) -> Result<VMValue, VMError> {
        match (&a, &b) {
            (VMValue::Int(x), VMValue::Int(y)) => Ok(VMValue::Int(x + y)),
            (VMValue::Float(x), VMValue::Float(y)) => Ok(VMValue::Float(x + y)),
            (VMValue::Int(x), VMValue::Float(y)) => Ok(VMValue::Float(*x as f64 + y)),
            (VMValue::Float(x), VMValue::Int(y)) => Ok(VMValue::Float(x + *y as f64)),
            (VMValue::String(x), VMValue::String(y)) => {
                Ok(VMValue::String(format!("{x}{y}")))
            }
            (VMValue::Path(x), VMValue::String(y)) => Ok(VMValue::Path(format!("{x}{y}"))),
            (VMValue::Path(x), VMValue::Path(y)) => Ok(VMValue::Path(format!("{x}/{y}"))),
            _ => Err(VMError::TypeError {
                expected: "numbers or strings",
                got: a.type_name(),
                context: format!("addition ({} + {})", a.type_name(), b.type_name()),
            }),
        }
    }

    fn num_op(
        &self,
        a: VMValue,
        b: VMValue,
        int_op: impl Fn(i64, i64) -> i64,
        float_op: impl Fn(f64, f64) -> f64,
        context: &str,
    ) -> Result<VMValue, VMError> {
        match (&a, &b) {
            (VMValue::Int(x), VMValue::Int(y)) => Ok(VMValue::Int(int_op(*x, *y))),
            (VMValue::Float(x), VMValue::Float(y)) => Ok(VMValue::Float(float_op(*x, *y))),
            (VMValue::Int(x), VMValue::Float(y)) => {
                Ok(VMValue::Float(float_op(*x as f64, *y)))
            }
            (VMValue::Float(x), VMValue::Int(y)) => {
                Ok(VMValue::Float(float_op(*x, *y as f64)))
            }
            _ => Err(VMError::TypeError {
                expected: "numbers",
                got: a.type_name(),
                context: context.to_string(),
            }),
        }
    }

    fn compare(&self, a: &VMValue, b: &VMValue) -> Result<std::cmp::Ordering, VMError> {
        match (a, b) {
            (VMValue::Int(x), VMValue::Int(y)) => Ok(x.cmp(y)),
            (VMValue::Float(x), VMValue::Float(y)) => {
                Ok(x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal))
            }
            (VMValue::Int(x), VMValue::Float(y)) => Ok((*x as f64)
                .partial_cmp(y)
                .unwrap_or(std::cmp::Ordering::Equal)),
            (VMValue::Float(x), VMValue::Int(y)) => Ok(x
                .partial_cmp(&(*y as f64))
                .unwrap_or(std::cmp::Ordering::Equal)),
            (VMValue::String(x), VMValue::String(y)) => Ok(x.cmp(y)),
            _ => Err(VMError::TypeError {
                expected: "comparable types",
                got: a.type_name(),
                context: "comparison".to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::Compiler;

    fn eval(input: &str) -> VMValue {
        let (chunk, mut interner) =
            Compiler::compile(input).unwrap_or_else(|e| panic!("compile '{input}': {e}"));
        VM::execute(chunk, &mut interner).unwrap_or_else(|e| panic!("execute '{input}': {e}"))
    }

    fn eval_full_helper(input: &str) -> crate::StringKeyedValue {
        let result =
            crate::eval_full(input).unwrap_or_else(|e| panic!("eval_full '{input}': {e}"));
        result.to_string_keyed()
    }

    fn eval_err(input: &str) -> VMError {
        let (chunk, mut interner) =
            Compiler::compile(input).unwrap_or_else(|e| panic!("compile '{input}': {e}"));
        VM::execute(chunk, &mut interner).unwrap_err()
    }

    // ── Literals ───────────────────────────────────────────────

    #[test]
    fn eval_integer() {
        assert_eq!(eval("42"), VMValue::Int(42));
    }

    #[test]
    fn eval_negative_integer() {
        assert_eq!(eval("-7"), VMValue::Int(-7));
    }

    #[test]
    fn eval_float() {
        assert_eq!(eval("3.14"), VMValue::Float(3.14));
    }

    #[test]
    fn eval_bool_true() {
        assert_eq!(eval("true"), VMValue::Bool(true));
    }

    #[test]
    fn eval_bool_false() {
        assert_eq!(eval("false"), VMValue::Bool(false));
    }

    #[test]
    fn eval_null() {
        assert_eq!(eval("null"), VMValue::Null);
    }

    #[test]
    fn eval_string() {
        assert_eq!(eval(r#""hello""#), VMValue::String("hello".to_string()));
    }

    // ── Arithmetic ─────────────────────────────────────────────

    #[test]
    fn eval_add_int() {
        assert_eq!(eval("1 + 2"), VMValue::Int(3));
    }

    #[test]
    fn eval_sub_int() {
        assert_eq!(eval("10 - 3"), VMValue::Int(7));
    }

    #[test]
    fn eval_mul_int() {
        assert_eq!(eval("3 * 4"), VMValue::Int(12));
    }

    #[test]
    fn eval_div_int() {
        assert_eq!(eval("10 / 3"), VMValue::Int(3));
    }

    #[test]
    fn eval_div_zero() {
        assert!(matches!(eval_err("1 / 0"), VMError::DivisionByZero));
    }

    #[test]
    fn eval_float_arithmetic() {
        assert_eq!(eval("1.5 + 2.5"), VMValue::Float(4.0));
    }

    #[test]
    fn eval_mixed_arithmetic() {
        assert_eq!(eval("1 + 2.0"), VMValue::Float(3.0));
    }

    #[test]
    fn eval_compound_arithmetic() {
        assert_eq!(eval("2 * 3 + 1"), VMValue::Int(7));
    }

    #[test]
    fn eval_negate_float() {
        assert_eq!(eval("-3.14"), VMValue::Float(-3.14));
    }

    #[test]
    fn eval_string_concat() {
        assert_eq!(
            eval(r#""hello" + " " + "world""#),
            VMValue::String("hello world".to_string())
        );
    }

    // ── Comparison ─────────────────────────────────────────────

    #[test]
    fn eval_equal() {
        assert_eq!(eval("1 == 1"), VMValue::Bool(true));
        assert_eq!(eval("1 == 2"), VMValue::Bool(false));
    }

    #[test]
    fn eval_not_equal() {
        assert_eq!(eval("1 != 2"), VMValue::Bool(true));
        assert_eq!(eval("1 != 1"), VMValue::Bool(false));
    }

    #[test]
    fn eval_less() {
        assert_eq!(eval("1 < 2"), VMValue::Bool(true));
        assert_eq!(eval("2 < 1"), VMValue::Bool(false));
    }

    #[test]
    fn eval_greater() {
        assert_eq!(eval("2 > 1"), VMValue::Bool(true));
        assert_eq!(eval("1 > 2"), VMValue::Bool(false));
    }

    #[test]
    fn eval_less_equal() {
        assert_eq!(eval("1 <= 1"), VMValue::Bool(true));
        assert_eq!(eval("1 <= 2"), VMValue::Bool(true));
        assert_eq!(eval("2 <= 1"), VMValue::Bool(false));
    }

    #[test]
    fn eval_greater_equal() {
        assert_eq!(eval("1 >= 1"), VMValue::Bool(true));
        assert_eq!(eval("2 >= 1"), VMValue::Bool(true));
        assert_eq!(eval("1 >= 2"), VMValue::Bool(false));
    }

    // ── Logical ────────────────────────────────────────────────

    #[test]
    fn eval_not() {
        assert_eq!(eval("!true"), VMValue::Bool(false));
        assert_eq!(eval("!false"), VMValue::Bool(true));
    }

    #[test]
    fn eval_and_short_circuit() {
        assert_eq!(eval("true && true"), VMValue::Bool(true));
        assert_eq!(eval("true && false"), VMValue::Bool(false));
        assert_eq!(eval("false && true"), VMValue::Bool(false));
    }

    #[test]
    fn eval_or_short_circuit() {
        assert_eq!(eval("false || true"), VMValue::Bool(true));
        assert_eq!(eval("false || false"), VMValue::Bool(false));
        assert_eq!(eval("true || false"), VMValue::Bool(true));
    }

    #[test]
    fn eval_implication() {
        assert_eq!(eval("true -> true"), VMValue::Bool(true));
        assert_eq!(eval("true -> false"), VMValue::Bool(false));
        assert_eq!(eval("false -> true"), VMValue::Bool(true));
        assert_eq!(eval("false -> false"), VMValue::Bool(true));
    }

    // ── Conditionals ───────────────────────────────────────────

    #[test]
    fn eval_if_true() {
        assert_eq!(eval("if true then 1 else 2"), VMValue::Int(1));
    }

    #[test]
    fn eval_if_false() {
        assert_eq!(eval("if false then 1 else 2"), VMValue::Int(2));
    }

    #[test]
    fn eval_if_expression() {
        assert_eq!(
            eval("if 1 > 2 then \"yes\" else \"no\""),
            VMValue::String("no".to_string())
        );
    }

    #[test]
    fn eval_nested_if() {
        assert_eq!(
            eval("if true then (if false then 1 else 2) else 3"),
            VMValue::Int(2)
        );
    }

    // ── Let/in ─────────────────────────────────────────────────

    #[test]
    fn eval_let_simple() {
        assert_eq!(eval("let x = 1; y = 2; in x + y"), VMValue::Int(3));
    }

    #[test]
    fn eval_let_nested() {
        assert_eq!(
            eval("let a = 10; in let b = 20; in a + b"),
            VMValue::Int(30)
        );
    }

    #[test]
    fn eval_let_shadow() {
        assert_eq!(eval("let x = 1; in let x = 2; in x"), VMValue::Int(2));
    }

    #[test]
    fn eval_let_with_expression() {
        assert_eq!(eval("let x = 2 * 3; in x + 1"), VMValue::Int(7));
    }

    // ── Lists ──────────────────────────────────────────────────

    #[test]
    fn eval_empty_list() {
        assert_eq!(eval("[]"), VMValue::List(vec![]));
    }

    #[test]
    fn eval_list() {
        assert_eq!(
            eval("[1 2 3]"),
            VMValue::List(vec![VMValue::Int(1), VMValue::Int(2), VMValue::Int(3)])
        );
    }

    #[test]
    fn eval_list_concat() {
        assert_eq!(
            eval("[1 2] ++ [3 4]"),
            VMValue::List(vec![
                VMValue::Int(1),
                VMValue::Int(2),
                VMValue::Int(3),
                VMValue::Int(4),
            ])
        );
    }

    #[test]
    fn eval_list_mixed() {
        assert_eq!(
            eval(r#"[1 "hello" true]"#),
            VMValue::List(vec![
                VMValue::Int(1),
                VMValue::String("hello".to_string()),
                VMValue::Bool(true),
            ])
        );
    }

    // ── Attribute sets ─────────────────────────────────────────

    #[test]
    fn eval_empty_attrset() {
        assert_eq!(eval("{ }"), VMValue::Attrs(BTreeMap::new()));
    }

    #[test]
    fn eval_attrset() {
        let result = eval_full_helper("{ a = 1; b = 2; }");
        let mut expected = BTreeMap::new();
        expected.insert("a".to_string(), crate::StringKeyedValue::Int(1));
        expected.insert("b".to_string(), crate::StringKeyedValue::Int(2));
        assert_eq!(result, crate::StringKeyedValue::Attrs(expected));
    }

    #[test]
    fn eval_attrset_select() {
        assert_eq!(eval("{ a = 1; b = 2; }.a"), VMValue::Int(1));
    }

    #[test]
    fn eval_attrset_update() {
        let result = eval_full_helper("{ a = 1; } // { b = 2; }");
        let mut expected = BTreeMap::new();
        expected.insert("a".to_string(), crate::StringKeyedValue::Int(1));
        expected.insert("b".to_string(), crate::StringKeyedValue::Int(2));
        assert_eq!(result, crate::StringKeyedValue::Attrs(expected));
    }

    #[test]
    fn eval_attrset_update_override() {
        assert_eq!(eval("({ a = 1; } // { a = 2; }).a"), VMValue::Int(2));
    }

    #[test]
    fn eval_has_attr_true() {
        assert_eq!(eval("{ a = 1; } ? a"), VMValue::Bool(true));
    }

    #[test]
    fn eval_has_attr_false() {
        assert_eq!(eval("{ a = 1; } ? b"), VMValue::Bool(false));
    }

    #[test]
    fn eval_select_or_default() {
        assert_eq!(eval("{ a = 1; }.b or 0"), VMValue::Int(0));
        assert_eq!(eval("{ a = 1; }.a or 0"), VMValue::Int(1));
    }

    // ── Lambdas / Apply ────────────────────────────────────────

    #[test]
    fn eval_identity_lambda() {
        assert_eq!(eval("(x: x) 42"), VMValue::Int(42));
    }

    #[test]
    fn eval_lambda_arithmetic() {
        assert_eq!(eval("(x: x + 1) 5"), VMValue::Int(6));
    }

    #[test]
    #[ignore = "requires upvalue capture (Phase 2)"]
    fn eval_curried_lambda() {
        assert_eq!(eval("(x: y: x + y) 3 4"), VMValue::Int(7));
    }

    #[test]
    fn eval_let_lambda() {
        assert_eq!(
            eval("let f = x: x * 2; in f 5"),
            VMValue::Int(10)
        );
    }

    #[test]
    fn eval_pattern_lambda() {
        assert_eq!(eval("({ a, b }: a + b) { a = 3; b = 4; }"), VMValue::Int(7));
    }

    #[test]
    fn eval_pattern_lambda_default() {
        assert_eq!(
            eval("({ a, b ? 10 }: a + b) { a = 5; }"),
            VMValue::Int(15)
        );
    }

    #[test]
    fn eval_lambda_with_let() {
        assert_eq!(
            eval("let inc = x: x + 1; double = x: x * 2; in double (inc 3)"),
            VMValue::Int(8)
        );
    }

    // ── Assert ─────────────────────────────────────────────────

    #[test]
    fn eval_assert_pass() {
        assert_eq!(eval("assert true; 42"), VMValue::Int(42));
    }

    #[test]
    fn eval_assert_fail() {
        assert!(matches!(eval_err("assert false; 42"), VMError::AssertionFailed));
    }

    // ── String interpolation ───────────────────────────────────

    #[test]
    fn eval_string_interpolation() {
        assert_eq!(
            eval(r#"let x = "world"; in "hello ${x}""#),
            VMValue::String("hello world".to_string()),
        );
    }

    #[test]
    #[ignore = "requires builtins.toString (Phase 2)"]
    fn eval_string_interpolation_int() {
        assert_eq!(
            eval(r#"let n = 42; in "value: ${toString n}""#),
            VMValue::String("value: 42".to_string()),
        );
    }

    // ── Path literals ──────────────────────────────────────────

    #[test]
    fn eval_absolute_path() {
        assert_eq!(eval("/tmp/x"), VMValue::Path("/tmp/x".to_string()));
    }

    // ── Complex expressions ────────────────────────────────────

    #[test]
    fn eval_fibonacci_like() {
        // Without recursion support, test a chain of let bindings.
        assert_eq!(
            eval("let a = 1; b = 1; c = a + b; d = b + c; e = c + d; in e"),
            VMValue::Int(5)
        );
    }

    #[test]
    fn eval_nested_attrset_select() {
        assert_eq!(
            eval("{ a = { b = 42; }; }.a.b"),
            VMValue::Int(42)
        );
    }

    #[test]
    fn eval_let_with_attrset() {
        assert_eq!(
            eval("let set = { x = 10; y = 20; }; in set.x + set.y"),
            VMValue::Int(30)
        );
    }

    #[test]
    fn eval_conditional_attrset() {
        assert_eq!(
            eval("(if true then { a = 1; } else { a = 2; }).a"),
            VMValue::Int(1)
        );
    }
}
