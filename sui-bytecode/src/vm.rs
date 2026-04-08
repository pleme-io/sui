//! Bytecode VM execution engine.
//!
//! A stack-based interpreter that executes compiled [`Chunk`]s. The VM
//! maintains a NaN-boxed value stack (8 bytes per entry), a call stack
//! for function invocations, and dispatches instructions via a `match` loop.
//!
//! # NaN-boxing
//!
//! The value stack uses [`NanBox`] instead of [`VMValue`]. Scalars (null,
//! bool, int, float) are stored inline as 8-byte values without heap
//! allocation. Complex types (strings, lists, attrsets, closures, builtins,
//! thunks) use an `Rc<HeapObject>` pointer encoded in the NaN payload bits.
//!
//! The constant pool (in `Chunk`) still uses `VMValue`; values are converted
//! to `NanBox` when pushed onto the stack and converted back only at the
//! external API boundary (`execute` returns `VMValue`).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use crate::builtins::BuiltinRegistry;
use crate::chunk::Chunk;
use crate::compiler::Compiler;
use crate::error::VMError;
use crate::intern::{Interner, Symbol};
use crate::nanbox::NanBox;
use crate::opcode::OpCode;
use crate::value::{ThunkState, VMValue};

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
    /// Upvalues captured by this frame's closure (NaN-boxed).
    upvalues: Vec<NanBox>,
}

/// The bytecode virtual machine.
///
/// Uses NaN-boxed values on the value stack: each entry is exactly 8 bytes,
/// making the stack cache-friendly. Scalars (null, bool, int, float) are
/// stored inline without heap allocation. Complex types use heap pointers
/// encoded in the NaN payload bits.
pub struct VM<'a> {
    /// NaN-boxed value stack (8 bytes per entry).
    stack: Vec<NanBox>,
    /// Call stack.
    frames: Vec<CallFrame>,
    /// Shared interner for attribute key operations.
    interner: &'a mut Interner,
    /// With-scope stack (dynamic variable scoping, NaN-boxed).
    with_stack: Vec<NanBox>,
    /// Registry of built-in functions.
    builtins: BuiltinRegistry,
    /// Import cache: canonical path -> evaluated result.
    import_cache: Rc<RefCell<HashMap<String, VMValue>>>,
}

impl<'a> VM<'a> {
    /// Create a new VM and execute a chunk, returning the result.
    pub fn execute(chunk: Chunk, interner: &'a mut Interner) -> Result<VMValue, VMError> {
        let mut vm = Self {
            stack: Vec::with_capacity(256),
            frames: Vec::with_capacity(64),
            interner,
            with_stack: Vec::new(),
            builtins: BuiltinRegistry::new(),
            import_cache: Rc::new(RefCell::new(HashMap::new())),
        };

        vm.frames.push(CallFrame {
            chunk: Rc::new(chunk),
            ip: 0,
            stack_base: 0,
            upvalues: Vec::new(),
        });

        let result = vm.run()?;
        Ok(result.to_vmvalue())
    }

    /// Main execution loop -- delegates to `run_until(0)`.
    fn run(&mut self) -> Result<NanBox, VMError> {
        self.run_until(0)
    }

    /// Execute until the frame stack drops to `stop_depth`.
    ///
    /// When the `Return` opcode pops a frame and the stack depth equals
    /// `stop_depth`, the loop exits and returns the result. This lets
    /// `import_file` and `force_value` run sub-programs without a separate VM.
    fn run_until(&mut self, stop_depth: usize) -> Result<NanBox, VMError> {
        loop {
            let op_byte = self.read_byte()?;
            // SAFETY: all opcode bytes in the bytecode stream were emitted
            // by the compiler and are valid OpCode repr(u8) values.
            let op = OpCode::from_byte(op_byte).ok_or(VMError::InvalidOpcode(op_byte))?;

            match op {
                // -- Constants ------------------------------------------
                OpCode::Constant => {
                    let idx = self.read_u16()?;
                    let value = &self.current_chunk().constants[idx as usize];
                    let boxed = NanBox::from_vmvalue(value);
                    self.push(boxed);
                }
                OpCode::Null => self.push(NanBox::null()),
                OpCode::True => self.push(NanBox::bool(true)),
                OpCode::False => self.push(NanBox::bool(false)),

                // -- Arithmetic -----------------------------------------
                OpCode::Add => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(self.add(&a, &b)?);
                }
                OpCode::Sub => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(self.num_op(&a, &b, |x, y| x - y, |x, y| x - y, "subtraction")?);
                }
                OpCode::Mul => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(self.num_op(&a, &b, |x, y| x * y, |x, y| x * y, "multiplication")?);
                }
                OpCode::Div => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    if a.is_int() && b.as_int() == Some(0) {
                        return Err(VMError::DivisionByZero);
                    }
                    self.push(self.num_op(&a, &b, |x, y| x / y, |x, y| x / y, "division")?);
                }
                OpCode::Negate => {
                    let val = self.pop()?;
                    if let Some(n) = val.as_int() {
                        self.push(NanBox::int(-n));
                    } else if let Some(f) = val.as_float() {
                        self.push(NanBox::float(-f));
                    } else {
                        return Err(VMError::TypeError {
                            expected: "int or float",
                            got: val.type_name(),
                            context: "negation".to_string(),
                        });
                    }
                }

                // -- Logical --------------------------------------------
                OpCode::Not => {
                    let val = self.pop()?;
                    let b = val.is_truthy()?;
                    self.push(NanBox::bool(!b));
                }
                OpCode::And => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(NanBox::bool(a.is_truthy()? && b.is_truthy()?));
                }
                OpCode::Or => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(NanBox::bool(a.is_truthy()? || b.is_truthy()?));
                }
                OpCode::Implication => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(NanBox::bool(!a.is_truthy()? || b.is_truthy()?));
                }

                // -- Comparison -----------------------------------------
                OpCode::Equal => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(NanBox::bool(a == b));
                }
                OpCode::NotEqual => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(NanBox::bool(a != b));
                }
                OpCode::Less => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(NanBox::bool(self.compare(&a, &b)? == std::cmp::Ordering::Less));
                }
                OpCode::Greater => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(NanBox::bool(
                        self.compare(&a, &b)? == std::cmp::Ordering::Greater,
                    ));
                }
                OpCode::LessEqual => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(NanBox::bool(
                        self.compare(&a, &b)? != std::cmp::Ordering::Greater,
                    ));
                }
                OpCode::GreaterEqual => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(NanBox::bool(
                        self.compare(&a, &b)? != std::cmp::Ordering::Less,
                    ));
                }

                // -- Strings --------------------------------------------
                OpCode::Interpolate => {
                    let count = self.read_u16()? as usize;
                    let start = self.stack.len() - count;
                    let mut result = String::new();
                    for i in start..self.stack.len() {
                        let v = &self.stack[i];
                        if let Some(s) = v.as_string() {
                            result.push_str(s);
                        } else if let Some(n) = v.as_int() {
                            result.push_str(&n.to_string());
                        } else if let Some(f) = v.as_float() {
                            result.push_str(&format!("{f}"));
                        } else if let Some(p) = v.as_path() {
                            result.push_str(p);
                        } else if v.is_bool() {
                            let b = v.as_bool().unwrap();
                            return Err(VMError::TypeError {
                                expected: "string, int, float, or path",
                                got: if b { "bool (true)" } else { "bool (false)" },
                                context: "string interpolation".to_string(),
                            });
                        } else {
                            return Err(VMError::TypeError {
                                expected: "string, int, float, or path",
                                got: v.type_name(),
                                context: "string interpolation".to_string(),
                            });
                        }
                    }
                    self.stack.truncate(start);
                    self.push(NanBox::string(result));
                }

                // -- Variables ------------------------------------------
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
                OpCode::GetUpvalue => {
                    let idx = self.read_u16()? as usize;
                    let value = self.current_frame().upvalues[idx].clone();
                    self.push(value);
                }
                OpCode::SetUpvalue => {
                    let idx = self.read_u16()? as usize;
                    let value = self.peek()?.clone();
                    self.current_frame_mut().upvalues[idx] = value;
                }

                // -- With scopes ----------------------------------------
                OpCode::PushWith => {
                    let scope = self.pop()?;
                    self.with_stack.push(scope);
                }
                OpCode::PopWith => {
                    self.with_stack.pop().ok_or_else(|| {
                        VMError::Internal("PopWith: empty with-stack".to_string())
                    })?;
                }
                OpCode::LookupWith => {
                    let name_idx = self.read_u16()?;
                    let name_string = match &self.current_chunk().constants[name_idx as usize] {
                        VMValue::String(s) => s.clone(),
                        _ => {
                            return Err(VMError::Internal(
                                "LookupWith: constant not a string".to_string(),
                            ));
                        }
                    };
                    let sym = self.interner.intern(&name_string);
                    let mut found = None;
                    for scope in self.with_stack.iter().rev() {
                        if let Some(attrs) = scope.as_attrs() {
                            if let Some(val) = attrs.get(&sym) {
                                found = Some(val.clone());
                                break;
                            }
                        }
                    }
                    match found {
                        Some(val) => self.push(val),
                        None => {
                            return Err(VMError::UndefinedVariable(name_string));
                        }
                    }
                }

                // -- Attribute sets -------------------------------------
                OpCode::MakeAttrs => {
                    let count = self.read_u16()? as usize;
                    let mut attrs: BTreeMap<Symbol, NanBox> = BTreeMap::new();
                    for _ in 0..count {
                        let key = self.pop()?;
                        let value = self.pop()?;
                        let key_sym = if let Some(s) = key.as_string() {
                            self.interner.intern(s)
                        } else {
                            return Err(VMError::TypeError {
                                expected: "string",
                                got: key.type_name(),
                                context: "attrset key".to_string(),
                            });
                        };
                        attrs.insert(key_sym, value);
                    }
                    self.push(NanBox::attrs(attrs));
                }
                OpCode::GetAttr => {
                    let key_idx = self.read_u16()?;
                    let key_sym = self.resolve_key_constant(key_idx)?;
                    let attrset = self.pop()?;
                    if let Some(attrs) = attrset.as_attrs() {
                        if let Some(val) = attrs.get(&key_sym) {
                            self.push(val.clone());
                        } else {
                            let key_str = self.interner.resolve(key_sym).to_string();
                            return Err(VMError::AttrNotFound(key_str));
                        }
                    } else {
                        let key_str = self.interner.resolve(key_sym).to_string();
                        return Err(VMError::TypeError {
                            expected: "set",
                            got: attrset.type_name(),
                            context: format!("attribute selection '.{key_str}'"),
                        });
                    }
                }
                OpCode::HasAttr => {
                    let key_idx = self.read_u16()?;
                    let key_sym = self.resolve_key_constant(key_idx)?;
                    let attrset = self.pop()?;
                    let result = if let Some(attrs) = attrset.as_attrs() {
                        attrs.contains_key(&key_sym)
                    } else {
                        false
                    };
                    self.push(NanBox::bool(result));
                }
                OpCode::UpdateAttrs => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    // Both must be attrsets. Convert through VMValue for mutation.
                    let b_vmval = b.to_vmvalue();
                    let a_vmval = a.to_vmvalue();
                    match (a_vmval, b_vmval) {
                        (VMValue::Attrs(mut left), VMValue::Attrs(right)) => {
                            for (k, v) in right {
                                left.insert(k, v);
                            }
                            self.push(NanBox::from_vmvalue(&VMValue::Attrs(left)));
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
                    if let Some(attrs) = attrset.as_attrs() {
                        if let Some(val) = attrs.get(&key_sym) {
                            self.push(val.clone());
                        } else {
                            self.push(default);
                        }
                    } else {
                        self.push(default);
                    }
                }

                // -- Lists ----------------------------------------------
                OpCode::MakeList => {
                    let count = self.read_u16()? as usize;
                    let start = self.stack.len() - count;
                    let items: Vec<NanBox> = self.stack.drain(start..).collect();
                    self.push(NanBox::list(items));
                }
                OpCode::Concat => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    let a_vmval = a.to_vmvalue();
                    let b_vmval = b.to_vmvalue();
                    match (a_vmval, b_vmval) {
                        (VMValue::List(mut left), VMValue::List(right)) => {
                            left.extend(right);
                            self.push(NanBox::from_vmvalue(&VMValue::List(left)));
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

                // -- Functions ------------------------------------------
                OpCode::MakeClosure => {
                    let idx = self.read_u16()?;
                    let upvalue_count = self.read_u16()? as usize;
                    let closure_template = self.current_chunk().constants[idx as usize].clone();
                    if let VMValue::Closure(mut closure) = closure_template {
                        let mut upvalues = Vec::with_capacity(upvalue_count);
                        for _ in 0..upvalue_count {
                            let is_local = self.read_byte()? != 0;
                            let uv_index = self.read_u16()? as usize;
                            if is_local {
                                let abs_slot = self.current_frame().stack_base + uv_index;
                                upvalues.push(self.stack[abs_slot].clone());
                            } else {
                                let val = self.current_frame().upvalues[uv_index].clone();
                                upvalues.push(val);
                            }
                        }
                        // Store upvalues as VMValue on the closure (constant pool compat).
                        closure.upvalues = upvalues.iter().map(NanBox::to_vmvalue).collect();
                        self.push(NanBox::closure(closure));
                    } else {
                        return Err(VMError::Internal(
                            "MakeClosure: constant is not a closure".to_string(),
                        ));
                    }
                }
                OpCode::Call => {
                    let arg = self.pop()?;
                    let func = self.pop()?;
                    if let Some(closure) = func.as_closure() {
                        let is_tail = self.peek_next_is_return();
                        let chunk = closure.chunk.clone();
                        let upvalues: Vec<NanBox> =
                            closure.upvalues.iter().map(NanBox::from_vmvalue).collect();

                        if is_tail && self.frames.len() > 1 {
                            let base = self.current_frame().stack_base;
                            self.stack.truncate(base);
                            self.push(arg);
                            let frame = self.current_frame_mut();
                            frame.chunk = chunk;
                            frame.ip = 0;
                            frame.upvalues = upvalues;
                        } else {
                            if self.frames.len() >= MAX_CALL_DEPTH {
                                return Err(VMError::StackOverflow);
                            }
                            let stack_base = self.stack.len();
                            self.push(arg);
                            self.frames.push(CallFrame {
                                chunk,
                                ip: 0,
                                stack_base,
                                upvalues,
                            });
                        }
                    } else if let Some(builtin) = func.as_builtin() {
                        let arg_vmval = arg.to_vmvalue();
                        let builtin_func = builtin.func.clone();
                        let result = builtin_func(vec![arg_vmval])?;
                        self.push(NanBox::from_vmvalue(&result));
                    } else {
                        return Err(VMError::NotCallable(func.type_name().to_string()));
                    }
                }
                OpCode::Return => {
                    let result = self.pop()?;
                    let frame = self.frames.pop().ok_or(VMError::Internal(
                        "return with empty call stack".to_string(),
                    ))?;

                    if self.frames.len() <= stop_depth {
                        return Ok(result);
                    }

                    self.stack.truncate(frame.stack_base);
                    self.push(result);
                }

                // -- Control flow ---------------------------------------
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

                // -- Assert ---------------------------------------------
                OpCode::Assert => {
                    let cond = self.pop()?;
                    if !cond.is_truthy()? {
                        return Err(VMError::AssertionFailed);
                    }
                }

                // -- Pop ------------------------------------------------
                OpCode::Pop => {
                    self.pop()?;
                }

                // -- Superinstructions ----------------------------------
                OpCode::GetLocalAttr => {
                    let slot = self.read_u16()? as usize;
                    let key_idx = self.read_u16()?;
                    let key_sym = self.resolve_key_constant(key_idx)?;
                    let abs_slot = self.current_frame().stack_base + slot;
                    let local = &self.stack[abs_slot];
                    if let Some(attrs) = local.as_attrs() {
                        if let Some(val) = attrs.get(&key_sym) {
                            self.push(val.clone());
                        } else {
                            let key_str = self.interner.resolve(key_sym).to_string();
                            return Err(VMError::AttrNotFound(key_str));
                        }
                    } else {
                        let key_str = self.interner.resolve(key_sym).to_string();
                        return Err(VMError::TypeError {
                            expected: "set",
                            got: local.type_name(),
                            context: format!("attribute selection '.{key_str}'"),
                        });
                    }
                }
                OpCode::GetLocalCall => {
                    let slot = self.read_u16()? as usize;
                    let abs_slot = self.current_frame().stack_base + slot;
                    let func = self.stack[abs_slot].clone();
                    let arg = self.pop()?;
                    if let Some(closure) = func.as_closure() {
                        if self.frames.len() >= MAX_CALL_DEPTH {
                            return Err(VMError::StackOverflow);
                        }
                        let upvalues: Vec<NanBox> =
                            closure.upvalues.iter().map(NanBox::from_vmvalue).collect();
                        let chunk = closure.chunk.clone();
                        let stack_base = self.stack.len();
                        self.push(arg);
                        self.frames.push(CallFrame {
                            chunk,
                            ip: 0,
                            stack_base,
                            upvalues,
                        });
                    } else if let Some(builtin) = func.as_builtin() {
                        let arg_vmval = arg.to_vmvalue();
                        let builtin_func = builtin.func.clone();
                        let result = builtin_func(vec![arg_vmval])?;
                        self.push(NanBox::from_vmvalue(&result));
                    } else {
                        return Err(VMError::NotCallable(func.type_name().to_string()));
                    }
                }

                // -- Builtins -------------------------------------------
                OpCode::PushBuiltins => {
                    let builtins_val = self.builtins.make_builtins_attrset(self.interner);
                    self.push(NanBox::from_vmvalue(&builtins_val));
                }
                OpCode::CallBuiltin => {
                    let builtin_idx = self.read_u16()?;
                    let arg_count = self.read_u16()? as usize;
                    let start = self.stack.len() - arg_count;
                    let args: Vec<VMValue> =
                        self.stack.drain(start..).map(|nb| nb.to_vmvalue()).collect();
                    let result = self.builtins.call(builtin_idx, args)?;
                    self.push(NanBox::from_vmvalue(&result));
                }

                // -- Thunks (lazy evaluation) ---------------------------
                OpCode::MakeThunk => {
                    let chunk_idx = self.read_u16()?;
                    let thunk_chunk =
                        match &self.current_chunk().constants[chunk_idx as usize] {
                            VMValue::Closure(c) => c.chunk.clone(),
                            _ => {
                                return Err(VMError::Internal(
                                    "MakeThunk: constant is not a closure".to_string(),
                                ))
                            }
                        };
                    let thunk = crate::value::VMThunk::new(thunk_chunk, Vec::new());
                    self.push(NanBox::thunk(thunk));
                }
                OpCode::Force => {
                    let val = self.pop()?;
                    let forced = self.force_value(val)?;
                    self.push(forced);
                }

                // -- Import ---------------------------------------------
                OpCode::Import => {
                    let path_val = self.pop()?;
                    let path = if let Some(p) = path_val.as_path() {
                        p.to_string()
                    } else if let Some(s) = path_val.as_string() {
                        s.to_string()
                    } else {
                        return Err(VMError::TypeError {
                            expected: "path or string",
                            got: path_val.type_name(),
                            context: "import".to_string(),
                        });
                    };
                    let result = self.import_file(&path)?;
                    self.push(result);
                }
            }
        }
    }

    // -- Stack helpers --------------------------------------------------

    fn push(&mut self, value: NanBox) {
        self.stack.push(value);
    }

    fn pop(&mut self) -> Result<NanBox, VMError> {
        self.stack.pop().ok_or(VMError::StackUnderflow)
    }

    fn peek(&self) -> Result<&NanBox, VMError> {
        self.stack.last().ok_or(VMError::StackUnderflow)
    }

    // -- Frame helpers --------------------------------------------------

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

    /// Peek ahead: check if the next instruction in the current frame
    /// is a `Return` opcode (used for tail-call optimization).
    fn peek_next_is_return(&self) -> bool {
        let frame = self.current_frame();
        if frame.ip < frame.chunk.code.len() {
            frame.chunk.code[frame.ip] == OpCode::Return as u8
        } else {
            false
        }
    }

    // -- Interning helpers ----------------------------------------------

    /// Resolve a constant pool string to a `Symbol`.
    fn resolve_key_constant(&mut self, idx: u16) -> Result<Symbol, VMError> {
        let idx_usize = idx as usize;
        let chunk = self.current_frame().chunk.clone();

        if let Some(Some(sym)) = chunk.key_symbols.get(idx_usize) {
            return Ok(*sym);
        }

        let key_string = match &chunk.constants[idx_usize] {
            VMValue::String(s) => s.clone(),
            _ => return Err(VMError::Internal("attr key constant not a string".to_string())),
        };
        Ok(self.interner.intern(&key_string))
    }

    // -- Arithmetic helpers (NanBox) ------------------------------------

    fn add(&self, a: &NanBox, b: &NanBox) -> Result<NanBox, VMError> {
        // Fast paths for inline scalars.
        if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
            return Ok(NanBox::int(x + y));
        }
        if let (Some(x), Some(y)) = (a.as_float(), b.as_float()) {
            return Ok(NanBox::float(x + y));
        }
        if let (Some(x), Some(y)) = (a.as_int(), b.as_float()) {
            return Ok(NanBox::float(x as f64 + y));
        }
        if let (Some(x), Some(y)) = (a.as_float(), b.as_int()) {
            return Ok(NanBox::float(x + y as f64));
        }
        // String/path concat (heap path).
        if let (Some(x), Some(y)) = (a.as_string(), b.as_string()) {
            return Ok(NanBox::string(format!("{x}{y}")));
        }
        if let (Some(x), Some(y)) = (a.as_path(), b.as_string()) {
            return Ok(NanBox::path(format!("{x}{y}")));
        }
        if let (Some(x), Some(y)) = (a.as_path(), b.as_path()) {
            return Ok(NanBox::path(format!("{x}/{y}")));
        }
        Err(VMError::TypeError {
            expected: "numbers or strings",
            got: a.type_name(),
            context: format!("addition ({} + {})", a.type_name(), b.type_name()),
        })
    }

    fn num_op(
        &self,
        a: &NanBox,
        b: &NanBox,
        int_op: impl Fn(i64, i64) -> i64,
        float_op: impl Fn(f64, f64) -> f64,
        context: &str,
    ) -> Result<NanBox, VMError> {
        if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
            return Ok(NanBox::int(int_op(x, y)));
        }
        if let (Some(x), Some(y)) = (a.as_float(), b.as_float()) {
            return Ok(NanBox::float(float_op(x, y)));
        }
        if let (Some(x), Some(y)) = (a.as_int(), b.as_float()) {
            return Ok(NanBox::float(float_op(x as f64, y)));
        }
        if let (Some(x), Some(y)) = (a.as_float(), b.as_int()) {
            return Ok(NanBox::float(float_op(x, y as f64)));
        }
        Err(VMError::TypeError {
            expected: "numbers",
            got: a.type_name(),
            context: context.to_string(),
        })
    }

    fn compare(&self, a: &NanBox, b: &NanBox) -> Result<std::cmp::Ordering, VMError> {
        if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
            return Ok(x.cmp(&y));
        }
        if let (Some(x), Some(y)) = (a.as_float(), b.as_float()) {
            return Ok(x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal));
        }
        if let (Some(x), Some(y)) = (a.as_int(), b.as_float()) {
            return Ok((x as f64)
                .partial_cmp(&y)
                .unwrap_or(std::cmp::Ordering::Equal));
        }
        if let (Some(x), Some(y)) = (a.as_float(), b.as_int()) {
            return Ok(x
                .partial_cmp(&(y as f64))
                .unwrap_or(std::cmp::Ordering::Equal));
        }
        if let (Some(x), Some(y)) = (a.as_string(), b.as_string()) {
            return Ok(x.cmp(y));
        }
        Err(VMError::TypeError {
            expected: "comparable types",
            got: a.type_name(),
            context: "comparison".to_string(),
        })
    }

    // -- Thunk forcing --------------------------------------------------

    /// Force a value: if it is a thunk, evaluate it (with memoization
    /// and blackhole detection). If it is already a concrete value,
    /// return it unchanged.
    fn force_value(&mut self, val: NanBox) -> Result<NanBox, VMError> {
        if !val.is_thunk() {
            return Ok(val);
        }

        // Convert to VMValue to access ThunkState machinery.
        let vmval = val.to_vmvalue();
        match vmval {
            VMValue::Thunk(ref thunk) => {
                let state = thunk.state.take();
                match state {
                    Some(ThunkState::Done(boxed)) => {
                        thunk.state.set(Some(ThunkState::Done(boxed.clone())));
                        Ok(NanBox::from_vmvalue(&*boxed))
                    }
                    Some(ThunkState::Evaluating) => {
                        thunk.state.set(Some(ThunkState::Evaluating));
                        Err(VMError::InfiniteRecursion)
                    }
                    Some(ThunkState::Pending { chunk, upvalues: _ }) => {
                        thunk.state.set(Some(ThunkState::Evaluating));

                        if self.frames.len() >= MAX_CALL_DEPTH {
                            thunk.state.set(Some(ThunkState::Pending {
                                chunk,
                                upvalues: Vec::new(),
                            }));
                            return Err(VMError::StackOverflow);
                        }

                        let return_depth = self.frames.len();
                        let stack_base = self.stack.len();
                        self.frames.push(CallFrame {
                            chunk: chunk.clone(),
                            ip: 0,
                            stack_base,
                            upvalues: Vec::new(),
                        });

                        let result = self.run_until(return_depth);

                        match result {
                            Ok(value) => {
                                let forced = if value.is_thunk() {
                                    self.force_value(value)?
                                } else {
                                    value
                                };
                                let forced_vmval = forced.to_vmvalue();
                                thunk
                                    .state
                                    .set(Some(ThunkState::Done(Box::new(forced_vmval))));
                                Ok(forced)
                            }
                            Err(e) => {
                                thunk.state.set(Some(ThunkState::Pending {
                                    chunk,
                                    upvalues: Vec::new(),
                                }));
                                Err(e)
                            }
                        }
                    }
                    None => Err(VMError::Internal("thunk state is None".to_string())),
                }
            }
            _ => Ok(NanBox::from_vmvalue(&vmval)),
        }
    }

    // -- Import ---------------------------------------------------------

    /// Import a Nix file: compile it, execute it, cache the result.
    fn import_file(&mut self, path: &str) -> Result<NanBox, VMError> {
        let canonical = std::fs::canonicalize(path)
            .map_err(|e| VMError::ImportError(format!("{path}: {e}")))?
            .to_string_lossy()
            .to_string();

        // Check cache.
        if let Some(cached) = self.import_cache.borrow().get(&canonical) {
            return Ok(NanBox::from_vmvalue(cached));
        }

        // Read and compile.
        let source = std::fs::read_to_string(&canonical)
            .map_err(|e| VMError::ImportError(format!("{canonical}: {e}")))?;

        let (chunk, _file_interner) = Compiler::compile(&source)
            .map_err(|e| VMError::ImportError(format!("{canonical}: {e}")))?;

        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(VMError::StackOverflow);
        }
        let return_depth = self.frames.len();
        let stack_base = self.stack.len();
        self.frames.push(CallFrame {
            chunk: Rc::new(chunk),
            ip: 0,
            stack_base,
            upvalues: Vec::new(),
        });

        let result = self.run_until(return_depth)?;

        // Cache as VMValue and return as NanBox.
        let result_vmval = result.to_vmvalue();
        self.import_cache
            .borrow_mut()
            .insert(canonical, result_vmval);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::Compiler;
    use crate::value::StringKeyedValue;

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

    // -- Literals -------------------------------------------------------

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

    // -- Arithmetic -----------------------------------------------------

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

    // -- Comparison -----------------------------------------------------

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

    // -- Logical --------------------------------------------------------

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

    // -- Conditionals ---------------------------------------------------

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

    // -- Let/in ---------------------------------------------------------

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

    // -- Lists ----------------------------------------------------------

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

    // -- Attribute sets -------------------------------------------------

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

    // -- Lambdas / Apply ------------------------------------------------

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

    // -- Assert ---------------------------------------------------------

    #[test]
    fn eval_assert_pass() {
        assert_eq!(eval("assert true; 42"), VMValue::Int(42));
    }

    #[test]
    fn eval_assert_fail() {
        assert!(matches!(eval_err("assert false; 42"), VMError::AssertionFailed));
    }

    // -- String interpolation -------------------------------------------

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

    // -- Path literals --------------------------------------------------

    #[test]
    fn eval_absolute_path() {
        assert_eq!(eval("/tmp/x"), VMValue::Path("/tmp/x".to_string()));
    }

    // -- Complex expressions --------------------------------------------

    #[test]
    fn eval_fibonacci_like() {
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

    // -- Builtin tests --------------------------------------------------

    #[test]
    fn builtin_length() {
        assert_eq!(eval("builtins.length [1 2 3]"), VMValue::Int(3));
    }

    #[test]
    fn builtin_length_empty() {
        assert_eq!(eval("builtins.length []"), VMValue::Int(0));
    }

    #[test]
    fn builtin_head() {
        assert_eq!(eval("builtins.head [10 20 30]"), VMValue::Int(10));
    }

    #[test]
    fn builtin_tail() {
        let result = eval_full_helper("builtins.tail [1 2 3]");
        assert_eq!(
            result,
            StringKeyedValue::List(vec![StringKeyedValue::Int(2), StringKeyedValue::Int(3)])
        );
    }

    #[test]
    fn builtin_type_of_int() {
        assert_eq!(
            eval("builtins.typeOf 42"),
            VMValue::String("int".to_string())
        );
    }

    #[test]
    fn builtin_type_of_string() {
        assert_eq!(
            eval("builtins.typeOf \"hello\""),
            VMValue::String("string".to_string())
        );
    }

    #[test]
    fn builtin_type_of_bool() {
        assert_eq!(
            eval("builtins.typeOf true"),
            VMValue::String("bool".to_string())
        );
    }

    #[test]
    fn builtin_type_of_null() {
        assert_eq!(
            eval("builtins.typeOf null"),
            VMValue::String("null".to_string())
        );
    }

    #[test]
    fn builtin_type_of_list() {
        assert_eq!(
            eval("builtins.typeOf [1 2]"),
            VMValue::String("list".to_string())
        );
    }

    #[test]
    fn builtin_type_of_set() {
        assert_eq!(
            eval("builtins.typeOf { a = 1; }"),
            VMValue::String("set".to_string())
        );
    }

    #[test]
    fn builtin_type_of_lambda() {
        assert_eq!(
            eval("builtins.typeOf (x: x)"),
            VMValue::String("lambda".to_string())
        );
    }

    #[test]
    fn builtin_is_int() {
        assert_eq!(eval("builtins.isInt 42"), VMValue::Bool(true));
        assert_eq!(
            eval("builtins.isInt \"hello\""),
            VMValue::Bool(false)
        );
    }

    #[test]
    fn builtin_is_string() {
        assert_eq!(eval("builtins.isString \"hi\""), VMValue::Bool(true));
        assert_eq!(eval("builtins.isString 42"), VMValue::Bool(false));
    }

    #[test]
    fn builtin_is_list() {
        assert_eq!(eval("builtins.isList [1]"), VMValue::Bool(true));
        assert_eq!(eval("builtins.isList 42"), VMValue::Bool(false));
    }

    #[test]
    fn builtin_is_attrs() {
        assert_eq!(
            eval("builtins.isAttrs { a = 1; }"),
            VMValue::Bool(true)
        );
        assert_eq!(eval("builtins.isAttrs 42"), VMValue::Bool(false));
    }

    #[test]
    fn builtin_is_function() {
        assert_eq!(
            eval("builtins.isFunction (x: x)"),
            VMValue::Bool(true)
        );
        assert_eq!(eval("builtins.isFunction 42"), VMValue::Bool(false));
    }

    #[test]
    fn builtin_is_bool() {
        assert_eq!(eval("builtins.isBool true"), VMValue::Bool(true));
        assert_eq!(eval("builtins.isBool 42"), VMValue::Bool(false));
    }

    #[test]
    fn builtin_is_null() {
        assert_eq!(eval("builtins.isNull null"), VMValue::Bool(true));
        assert_eq!(eval("builtins.isNull 42"), VMValue::Bool(false));
    }

    #[test]
    fn builtin_string_length() {
        assert_eq!(
            eval("builtins.stringLength \"hello\""),
            VMValue::Int(5)
        );
    }

    #[test]
    fn builtin_to_string_int() {
        assert_eq!(
            eval("builtins.toString 42"),
            VMValue::String("42".to_string())
        );
    }

    #[test]
    fn builtin_to_string_bool() {
        assert_eq!(
            eval("builtins.toString true"),
            VMValue::String("1".to_string())
        );
    }

    #[test]
    fn builtin_throw() {
        let result = eval_err("builtins.throw \"test error\"");
        assert!(matches!(result, VMError::Throw(_)));
    }

    #[test]
    fn builtin_abort() {
        let result = eval_err("builtins.abort \"fatal\"");
        assert!(matches!(result, VMError::Throw(_)));
    }

    #[test]
    fn builtin_add_curried() {
        assert_eq!(eval("builtins.add 3 4"), VMValue::Int(7));
    }

    #[test]
    fn builtin_sub_curried() {
        assert_eq!(eval("builtins.sub 10 3"), VMValue::Int(7));
    }

    #[test]
    fn builtin_mul_curried() {
        assert_eq!(eval("builtins.mul 6 7"), VMValue::Int(42));
    }

    #[test]
    fn builtin_div_curried() {
        assert_eq!(eval("builtins.div 42 6"), VMValue::Int(7));
    }

    #[test]
    fn builtin_elem_at() {
        assert_eq!(eval("builtins.elemAt [10 20 30] 1"), VMValue::Int(20));
    }

    #[test]
    fn builtin_elem() {
        assert_eq!(eval("builtins.elem 2 [1 2 3]"), VMValue::Bool(true));
        assert_eq!(eval("builtins.elem 5 [1 2 3]"), VMValue::Bool(false));
    }

    #[test]
    fn builtin_concat_lists() {
        let result = eval_full_helper("builtins.concatLists [[1 2] [3 4]]");
        assert_eq!(
            result,
            StringKeyedValue::List(vec![
                StringKeyedValue::Int(1),
                StringKeyedValue::Int(2),
                StringKeyedValue::Int(3),
                StringKeyedValue::Int(4),
            ])
        );
    }

    #[test]
    fn builtin_has_prefix() {
        assert_eq!(
            eval("builtins.hasPrefix \"he\" \"hello\""),
            VMValue::Bool(true)
        );
        assert_eq!(
            eval("builtins.hasPrefix \"wo\" \"hello\""),
            VMValue::Bool(false)
        );
    }

    #[test]
    fn builtin_has_suffix() {
        assert_eq!(
            eval("builtins.hasSuffix \"lo\" \"hello\""),
            VMValue::Bool(true)
        );
    }

    #[test]
    fn builtin_concat_strings_sep() {
        assert_eq!(
            eval("builtins.concatStringsSep \", \" [\"a\" \"b\" \"c\"]"),
            VMValue::String("a, b, c".to_string())
        );
    }

    #[test]
    fn builtin_to_lower() {
        assert_eq!(
            eval("builtins.toLower \"Hello World\""),
            VMValue::String("hello world".to_string())
        );
    }

    #[test]
    fn builtin_to_upper() {
        assert_eq!(
            eval("builtins.toUpper \"hello\""),
            VMValue::String("HELLO".to_string())
        );
    }

    #[test]
    fn builtin_from_json() {
        assert_eq!(
            eval("builtins.fromJSON \"42\""),
            VMValue::Int(42)
        );
        assert_eq!(
            eval("builtins.fromJSON \"true\""),
            VMValue::Bool(true)
        );
    }

    #[test]
    fn builtin_seq() {
        assert_eq!(eval("builtins.seq 1 42"), VMValue::Int(42));
    }

    #[test]
    fn builtin_deep_seq() {
        assert_eq!(eval("builtins.deepSeq [1 2] 42"), VMValue::Int(42));
    }

    #[test]
    fn builtin_trace() {
        assert_eq!(
            eval("builtins.trace \"debug\" 42"),
            VMValue::Int(42)
        );
    }

    #[test]
    fn builtin_ceil_floor() {
        assert_eq!(eval("builtins.ceil 3.2"), VMValue::Int(4));
        assert_eq!(eval("builtins.floor 3.8"), VMValue::Int(3));
    }

    #[test]
    fn builtin_bit_ops() {
        assert_eq!(eval("builtins.bitAnd 12 10"), VMValue::Int(8));
        assert_eq!(eval("builtins.bitOr 12 10"), VMValue::Int(14));
        assert_eq!(eval("builtins.bitXor 12 10"), VMValue::Int(6));
    }

    #[test]
    fn builtin_intersect_attrs() {
        let result =
            eval_full_helper("builtins.intersectAttrs { a = 1; b = 2; } { a = 10; c = 30; }");
        match result {
            StringKeyedValue::Attrs(map) => {
                assert_eq!(map.get("a"), Some(&StringKeyedValue::Int(10)));
                assert!(!map.contains_key("b"));
                assert!(!map.contains_key("c"));
            }
            _ => panic!("expected Attrs, got {result:?}"),
        }
    }

    #[test]
    fn builtin_attr_values() {
        let result = eval_full_helper("builtins.attrValues { a = 1; b = 2; }");
        match result {
            StringKeyedValue::List(items) => {
                assert_eq!(items.len(), 2);
                assert!(items.contains(&StringKeyedValue::Int(1)));
                assert!(items.contains(&StringKeyedValue::Int(2)));
            }
            _ => panic!("expected List, got {result:?}"),
        }
    }

    #[test]
    fn builtin_to_int() {
        assert_eq!(eval("builtins.toInt \"42\""), VMValue::Int(42));
    }

    #[test]
    fn builtin_replace_strings() {
        assert_eq!(
            eval("builtins.replaceStrings [\"o\"] [\"0\"] \"foo\""),
            VMValue::String("f00".to_string())
        );
    }

    #[test]
    fn builtin_substring() {
        assert_eq!(
            eval("builtins.substring 1 3 \"hello\""),
            VMValue::String("ell".to_string())
        );
    }

    // -- Import tests ---------------------------------------------------

    #[test]
    fn import_basic() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.nix");
        std::fs::write(&file_path, "42").unwrap();
        let nix_expr = format!("import {}", file_path.display());
        assert_eq!(eval(&nix_expr), VMValue::Int(42));
    }

    #[test]
    fn import_cached() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("cached.nix");
        std::fs::write(&file_path, "{ x = 1; }").unwrap();
        let nix_expr = format!(
            "let a = import {}; b = import {}; in a == b",
            file_path.display(),
            file_path.display()
        );
        assert_eq!(eval(&nix_expr), VMValue::Bool(true));
    }

    #[test]
    fn import_attrset() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("attrs.nix");
        std::fs::write(&file_path, "{ greeting = \"hello\"; }").unwrap();
        let nix_expr = format!("(import {}).greeting", file_path.display());
        assert_eq!(eval(&nix_expr), VMValue::String("hello".to_string()));
    }

    // -- Lazy evaluation tests ------------------------------------------

    #[test]
    fn lazy_unused_throw_in_attrset() {
        assert_eq!(
            eval("let s = { a = 1; }; in s.a"),
            VMValue::Int(1)
        );
    }

    #[test]
    fn lazy_unused_let_binding() {
        assert_eq!(eval("let x = 1; y = 2; in x"), VMValue::Int(1));
    }
}
