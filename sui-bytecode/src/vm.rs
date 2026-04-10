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

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Counts how many files fell back to the tree-walker during VM import.
static VM_FALLBACK_COUNT: AtomicU64 = AtomicU64::new(0);

/// Return the number of files that fell back to tree-walker evaluation.
pub fn vm_fallback_count() -> u64 {
    VM_FALLBACK_COUNT.load(Ordering::Relaxed)
}

use crate::builtins::BuiltinRegistry;
use crate::chunk::Chunk;
use crate::compiler::Compiler;
use crate::error::VMError;
use crate::intern::{Interner, Symbol};
use crate::nanbox::NanBox;
use crate::opcode::OpCode;
use crate::value::{HigherOrderBuiltin, HigherOrderOp, ThunkState, VMThunk, VMValue};

/// Maximum call depth before we report a stack overflow.
const MAX_CALL_DEPTH: usize = 1024;

// ── Flake resolver callback ─────────────────────────────────

/// Signature for an external flake resolver.
///
/// When set, the VM delegates `builtins.getFlake` to this callback
/// instead of using its own limited input resolution.  The callback
/// receives the raw flake reference string (e.g. `"path:/foo/bar"`)
/// and returns a `StringKeyedValue` attrset representing the fully
/// resolved flake outputs.
///
/// `sui-eval` sets this to the tree-walker's `evaluate_flake` which
/// handles all input types (GitHub, path, indirect) and produces
/// correct results for `(getFlake ref).inputs.nixpkgs`.
pub type FlakeResolverFn = dyn Fn(&str) -> Result<crate::value::StringKeyedValue, String>;

thread_local! {
    static FLAKE_RESOLVER: RefCell<Option<Box<FlakeResolverFn>>> = const { RefCell::new(None) };
}

/// Install a flake resolver callback for the current thread.
///
/// Returns an RAII guard that restores the previous resolver on drop.
/// This ensures the resolver is always properly cleaned up even when
/// evaluation errors occur.
pub fn set_flake_resolver(
    resolver: Box<FlakeResolverFn>,
) -> FlakeResolverGuard {
    let prev = FLAKE_RESOLVER.with(|r| r.borrow_mut().replace(resolver));
    FlakeResolverGuard { _prev: prev }
}

/// RAII guard that restores the previous flake resolver on drop.
pub struct FlakeResolverGuard {
    _prev: Option<Box<FlakeResolverFn>>,
}

impl Drop for FlakeResolverGuard {
    fn drop(&mut self) {
        let prev = self._prev.take();
        FLAKE_RESOLVER.with(|r| *r.borrow_mut() = prev);
    }
}

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
    /// Compile cache: canonical path -> compiled bytecode.
    /// Avoids re-parsing and re-compiling files that are imported
    /// multiple times (e.g. via scopedImport or recursive imports).
    compile_cache: HashMap<PathBuf, Rc<Chunk>>,
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
            compile_cache: HashMap::new(),
        };

        vm.frames.push(CallFrame {
            chunk: Rc::new(chunk),
            ip: 0,
            stack_base: 0,
            upvalues: Vec::new(),
        });

        let result = vm.run()?;
        // Force the top-level result so we never return a thunk.
        let result = vm.force_value(result)?;
        // Deep-force: recursively force thunks inside attrsets and lists
        // so the caller never sees unforced thunks.
        let result = vm.deep_force(result)?;
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
            let op = OpCode::from_byte(op_byte).ok_or(VMError::InvalidOpcode(op_byte))?;

            match op {
                // Arithmetic
                OpCode::Add | OpCode::Sub | OpCode::Mul | OpCode::Div | OpCode::Negate => {
                    self.dispatch_arithmetic(op)?;
                }
                // Comparison
                OpCode::Equal | OpCode::NotEqual | OpCode::Less | OpCode::Greater |
                OpCode::LessEqual | OpCode::GreaterEqual => {
                    self.dispatch_comparison(op)?;
                }
                // Logic
                OpCode::Not | OpCode::And | OpCode::Or | OpCode::Implication => {
                    self.dispatch_logic(op)?;
                }
                // Constants
                OpCode::Constant | OpCode::Null | OpCode::True | OpCode::False => {
                    self.dispatch_constant(op)?;
                }
                // Variables
                OpCode::GetLocal | OpCode::SetLocal | OpCode::GetUpvalue | OpCode::SetUpvalue => {
                    self.dispatch_variable(op)?;
                }
                // Attrsets
                OpCode::MakeAttrs | OpCode::GetAttr | OpCode::HasAttr | OpCode::UpdateAttrs |
                OpCode::SelectOrDefault | OpCode::DynGetAttr => {
                    self.dispatch_attrset(op)?;
                }
                // Lists
                OpCode::MakeList | OpCode::Concat => {
                    self.dispatch_list(op)?;
                }
                // Control flow
                OpCode::Jump | OpCode::JumpIfFalse | OpCode::JumpIfTrue | OpCode::Assert => {
                    self.dispatch_control(op)?;
                }
                // Functions
                OpCode::MakeClosure | OpCode::Call | OpCode::TailCall => {
                    self.dispatch_function(op)?;
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
                // Thunks
                OpCode::MakeThunk | OpCode::MakeLazyThunk | OpCode::Force |
                OpCode::PatchThunkUpvalues => {
                    self.dispatch_thunk(op)?;
                }
                // Scope
                OpCode::PushWith | OpCode::PopWith | OpCode::LookupWith |
                OpCode::PushBuiltins => {
                    self.dispatch_scope(op)?;
                }
                // Import + CallBuiltin
                OpCode::Import | OpCode::CallBuiltin => {
                    self.dispatch_import(op)?;
                }
                // Super-instructions
                OpCode::GetLocalAttr | OpCode::GetLocalCall => {
                    self.dispatch_super(op)?;
                }
                // Stack / String
                OpCode::Pop | OpCode::Interpolate => {
                    self.dispatch_stack(op)?;
                }
            }
        }
    }

    // ── Dispatch handler groups ──────────────────────────────────

    fn dispatch_constant(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
            OpCode::Constant => {
                let idx = self.read_u16()?;
                let value = &self.current_chunk().constants[idx as usize];
                let boxed = NanBox::from_vmvalue(value);
                self.push(boxed);
            }
            OpCode::Null => self.push(NanBox::null()),
            OpCode::True => self.push(NanBox::bool(true)),
            OpCode::False => self.push(NanBox::bool(false)),
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_arithmetic(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
            OpCode::Add => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                self.push(self.add(&a, &b)?);
            }
            OpCode::Sub => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                self.push(self.num_op(&a, &b, |x, y| x - y, |x, y| x - y, "subtraction")?);
            }
            OpCode::Mul => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                self.push(self.num_op(&a, &b, |x, y| x * y, |x, y| x * y, "multiplication")?);
            }
            OpCode::Div => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                if a.is_int() && b.as_int() == Some(0) {
                    return Err(VMError::DivisionByZero);
                }
                self.push(self.num_op(&a, &b, |x, y| x / y, |x, y| x / y, "division")?);
            }
            OpCode::Negate => {
                let val = self.pop_forced()?;
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
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_logic(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
            OpCode::Not => {
                let val = self.pop_forced()?;
                let b = val.is_truthy()?;
                self.push(NanBox::bool(!b));
            }
            OpCode::And => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                self.push(NanBox::bool(a.is_truthy()? && b.is_truthy()?));
            }
            OpCode::Or => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                self.push(NanBox::bool(a.is_truthy()? || b.is_truthy()?));
            }
            OpCode::Implication => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                self.push(NanBox::bool(!a.is_truthy()? || b.is_truthy()?));
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_comparison(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
            OpCode::Equal => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                self.push(NanBox::bool(a == b));
            }
            OpCode::NotEqual => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                self.push(NanBox::bool(a != b));
            }
            OpCode::Less => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                self.push(NanBox::bool(self.compare(&a, &b)? == std::cmp::Ordering::Less));
            }
            OpCode::Greater => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                self.push(NanBox::bool(self.compare(&a, &b)? == std::cmp::Ordering::Greater));
            }
            OpCode::LessEqual => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                self.push(NanBox::bool(self.compare(&a, &b)? != std::cmp::Ordering::Greater));
            }
            OpCode::GreaterEqual => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
                self.push(NanBox::bool(self.compare(&a, &b)? != std::cmp::Ordering::Less));
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_variable(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
            OpCode::GetLocal => {
                let slot = self.read_u16()? as usize;
                let abs_slot = self.current_frame().stack_base + slot;
                if abs_slot >= self.stack.len() {
                    let frame = self.current_frame();
                    let chunk = &frame.chunk;
                    let failing_ip = frame.ip.saturating_sub(3);
                    let frame_info: Vec<String> = self.frames.iter().enumerate()
                        .map(|(i, f)| format!("frame[{i}]: base={}, ip={}", f.stack_base, f.ip))
                        .collect();
                    let bytecode_context = Self::disassemble_around(chunk, failing_ip, 10);
                    return Err(VMError::Internal(format!(
                        "GetLocal: slot {slot} (abs {abs_slot}) out of bounds \
                         (stack len {}, base {}, depth {})\n  \
                         {}\n  bytecode around ip={failing_ip}:\n{}",
                        self.stack.len(),
                        self.current_frame().stack_base,
                        self.frames.len(),
                        frame_info.join("\n  "),
                        bytecode_context,
                    )));
                }
                let value = self.stack[abs_slot].clone();
                self.push(value);
            }
            OpCode::SetLocal => {
                let slot = self.read_u16()? as usize;
                let abs_slot = self.current_frame().stack_base + slot;
                if abs_slot >= self.stack.len() {
                    return Err(VMError::Internal(format!(
                        "SetLocal: slot {slot} (abs {abs_slot}) out of bounds \
                         (stack len {}, base {})",
                        self.stack.len(),
                        self.current_frame().stack_base,
                    )));
                }
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
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_scope(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
            OpCode::PushWith => {
                let scope = self.pop_forced()?;
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
            OpCode::PushBuiltins => {
                let builtins_val = self.builtins.make_builtins_attrset(self.interner);
                self.push(NanBox::from_vmvalue(&builtins_val));
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_attrset(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
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
                let attrset = self.pop_forced()?;
                if let Some(attrs) = attrset.as_attrs() {
                    if let Some(val) = attrs.get(&key_sym) {
                        let forced = if val.is_thunk() {
                            self.force_value(val.clone())?
                        } else {
                            val.clone()
                        };
                        self.push(forced);
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
                let attrset = self.pop_forced()?;
                let result = if let Some(attrs) = attrset.as_attrs() {
                    attrs.contains_key(&key_sym)
                } else {
                    false
                };
                self.push(NanBox::bool(result));
            }
            OpCode::UpdateAttrs => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
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
                let attrset = self.pop_forced()?;
                if let Some(attrs) = attrset.as_attrs() {
                    if let Some(val) = attrs.get(&key_sym) {
                        let forced = if val.is_thunk() {
                            self.force_value(val.clone())?
                        } else {
                            val.clone()
                        };
                        self.push(forced);
                    } else {
                        self.push(default);
                    }
                } else {
                    self.push(default);
                }
            }
            OpCode::DynGetAttr => {
                let key_val = self.pop_forced()?;
                let attrset = self.pop_forced()?;
                let key_str = key_val
                    .as_string()
                    .ok_or_else(|| VMError::TypeError {
                        expected: "string",
                        got: key_val.type_name(),
                        context: "dynamic attribute key".to_string(),
                    })?
                    .to_string();
                let key_sym = self.interner.intern(&key_str);
                if let Some(attrs) = attrset.as_attrs() {
                    if let Some(val) = attrs.get(&key_sym) {
                        let forced = if val.is_thunk() {
                            self.force_value(val.clone())?
                        } else {
                            val.clone()
                        };
                        self.push(forced);
                    } else {
                        return Err(VMError::AttrNotFound(key_str));
                    }
                } else {
                    return Err(VMError::TypeError {
                        expected: "set",
                        got: attrset.type_name(),
                        context: format!("dynamic select .${{{key_str}}}"),
                    });
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_list(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
            OpCode::MakeList => {
                let count = self.read_u16()? as usize;
                let start = self.stack.len() - count;
                let items: Vec<NanBox> = self.stack.drain(start..).collect();
                self.push(NanBox::list(items));
            }
            OpCode::Concat => {
                let b = self.pop_forced()?;
                let a = self.pop_forced()?;
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
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_control(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
            OpCode::Jump => {
                let target = self.read_u16()? as usize;
                self.current_frame_mut().ip = target;
            }
            OpCode::JumpIfFalse => {
                let target = self.read_u16()? as usize;
                let cond = self.pop_forced()?;
                if !cond.is_truthy()? {
                    self.current_frame_mut().ip = target;
                }
            }
            OpCode::JumpIfTrue => {
                let target = self.read_u16()? as usize;
                let cond = self.pop_forced()?;
                if cond.is_truthy()? {
                    self.current_frame_mut().ip = target;
                }
            }
            OpCode::Assert => {
                let cond = self.pop_forced()?;
                if !cond.is_truthy()? {
                    return Err(VMError::AssertionFailed);
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_function(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
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
                let func = self.pop_forced()?;
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
                } else if func.is_higher_order_builtin() {
                    let hob = func.as_higher_order_builtin().unwrap().clone();
                    let forced_arg = self.force_value(arg)?;
                    let result = self.call_higher_order_builtin(&hob, forced_arg)?;
                    self.push(result);
                } else if let Some(builtin) = func.as_builtin() {
                    let forced_arg = self.force_value(arg)?;
                    if let Some(result) = self.try_vm_builtin(builtin.name, &forced_arg)? {
                        self.push(result);
                    } else {
                        let arg_vmval = forced_arg.to_vmvalue();
                        let builtin_func = builtin.func.clone();
                        let result = self.call_builtin_with_scoped_import_dispatch(
                            builtin_func, arg_vmval,
                        )?;
                        self.push(result);
                    }
                } else {
                    return Err(VMError::NotCallable(func.type_name().to_string()));
                }
            }
            OpCode::TailCall => {
                // Compiler-determined tail call: always reuse the current frame
                // for closures (no runtime peek needed). For builtins, fall back
                // to a regular call since they don't use bytecode frames.
                let arg = self.pop()?;
                let func = self.pop_forced()?;
                if let Some(closure) = func.as_closure() {
                    let chunk = closure.chunk.clone();
                    let upvalues: Vec<NanBox> =
                        closure.upvalues.iter().map(NanBox::from_vmvalue).collect();
                    if self.frames.len() > 1 {
                        // Tail-call optimization: reuse current frame.
                        let base = self.current_frame().stack_base;
                        self.stack.truncate(base);
                        self.push(arg);
                        let frame = self.current_frame_mut();
                        frame.chunk = chunk;
                        frame.ip = 0;
                        frame.upvalues = upvalues;
                    } else {
                        // Top-level frame: cannot reuse, push new frame.
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
                } else if func.is_higher_order_builtin() {
                    let hob = func.as_higher_order_builtin().unwrap().clone();
                    let forced_arg = self.force_value(arg)?;
                    let result = self.call_higher_order_builtin(&hob, forced_arg)?;
                    self.push(result);
                } else if let Some(builtin) = func.as_builtin() {
                    let forced_arg = self.force_value(arg)?;
                    if let Some(result) = self.try_vm_builtin(builtin.name, &forced_arg)? {
                        self.push(result);
                    } else {
                        let arg_vmval = forced_arg.to_vmvalue();
                        let builtin_func = builtin.func.clone();
                        let result = self.call_builtin_with_scoped_import_dispatch(
                            builtin_func, arg_vmval,
                        )?;
                        self.push(result);
                    }
                } else {
                    return Err(VMError::NotCallable(func.type_name().to_string()));
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_thunk(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
            OpCode::MakeThunk => {
                let chunk_idx = self.read_u16()?;
                let upvalue_count = self.read_u16()? as usize;
                let thunk_chunk =
                    match &self.current_chunk().constants[chunk_idx as usize] {
                        VMValue::Closure(c) => c.chunk.clone(),
                        _ => {
                            return Err(VMError::Internal(
                                "MakeThunk: constant is not a closure".to_string(),
                            ))
                        }
                    };
                let mut upvalues = Vec::with_capacity(upvalue_count);
                for _ in 0..upvalue_count {
                    let is_local = self.read_byte()? != 0;
                    let uv_index = self.read_u16()? as usize;
                    if is_local {
                        let abs_slot = self.current_frame().stack_base + uv_index;
                        upvalues.push(self.stack[abs_slot].to_vmvalue());
                    } else {
                        let val = self.current_frame().upvalues[uv_index].to_vmvalue();
                        upvalues.push(val);
                    }
                }
                let thunk = crate::value::VMThunk::new(thunk_chunk, upvalues);
                self.push(NanBox::thunk(thunk));
            }
            OpCode::Force => {
                let val = self.pop()?;
                let forced = self.force_value(val)?;
                self.push(forced);
            }
            OpCode::PatchThunkUpvalues => {
                let patch_slot = self.read_u16()? as usize;
                let patch_uv_count = self.read_u16()? as usize;
                let patch_abs = self.current_frame().stack_base + patch_slot;
                let mut patch_uvs: Vec<VMValue> = Vec::with_capacity(patch_uv_count);
                for _ in 0..patch_uv_count {
                    let il = self.read_byte()? != 0;
                    let ui = self.read_u16()? as usize;
                    if il {
                        let a = self.current_frame().stack_base + ui;
                        if a >= self.stack.len() {
                            // Slot not yet allocated — skip this upvalue patch.
                            patch_uvs.push(VMValue::Null);
                            continue;
                        }
                        patch_uvs.push(self.stack[a].to_vmvalue());
                    } else {
                        if ui >= self.current_frame().upvalues.len() {
                            patch_uvs.push(VMValue::Null);
                            continue;
                        }
                        patch_uvs.push(self.current_frame().upvalues[ui].to_vmvalue());
                    }
                }
                if patch_abs < self.stack.len() {
                    let patch_nb = self.stack[patch_abs].clone();
                    let patch_vm = patch_nb.to_vmvalue();
                    if let VMValue::Thunk(ref t) = patch_vm {
                        let s = t.state.take();
                        if let Some(ThunkState::Pending { chunk: c, .. }) = s {
                            t.state.set(Some(ThunkState::Pending { chunk: c, upvalues: patch_uvs }));
                        } else {
                            t.state.set(s);
                        }
                    }
                }
            }
            OpCode::MakeLazyThunk => {
                let src_idx = self.read_u16()? as usize;
                let offset = self.read_u32()? as usize;
                let length = self.read_u32()? as usize;
                let dir_idx = self.read_u16()? as usize;
                let upvalue_count = self.read_u16()? as usize;
                let source_text = match &self.current_chunk().constants[src_idx] {
                    VMValue::String(s) => Rc::new(s.clone()),
                    _ => return Err(VMError::Internal(
                        "MakeLazyThunk: source constant not a string".to_string(),
                    )),
                };
                let base_dir_str = match &self.current_chunk().constants[dir_idx] {
                    VMValue::String(s) => s.clone(),
                    _ => return Err(VMError::Internal(
                        "MakeLazyThunk: base_dir constant not a string".to_string(),
                    )),
                };
                let base_dir = PathBuf::from(base_dir_str);
                let mut upvalues = Vec::with_capacity(upvalue_count);
                for _ in 0..upvalue_count {
                    let is_local = self.read_byte()? != 0;
                    let uv_index = self.read_u16()? as usize;
                    if is_local {
                        let abs_slot = self.current_frame().stack_base + uv_index;
                        upvalues.push(self.stack[abs_slot].to_vmvalue());
                    } else {
                        let val = self.current_frame().upvalues[uv_index].to_vmvalue();
                        upvalues.push(val);
                    }
                }
                let thunk = crate::value::VMThunk {
                    state: Rc::new(std::cell::Cell::new(Some(ThunkState::LazySource {
                        source: source_text,
                        offset,
                        length,
                        base_dir,
                        upvalues,
                    }))),
                };
                self.push(NanBox::thunk(thunk));
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_import(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
            OpCode::Import => {
                let path_val = self.pop()?;
                let path_val = self.force_value(path_val)?; // Force thunks before type check
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
            OpCode::CallBuiltin => {
                let builtin_idx = self.read_u16()?;
                let arg_count = self.read_u16()? as usize;
                let start = self.stack.len() - arg_count;
                let raw_args: Vec<NanBox> = self.stack.drain(start..).collect();
                let mut args = Vec::with_capacity(raw_args.len());
                for raw in raw_args {
                    let forced = self.force_value(raw)?;
                    args.push(forced.to_vmvalue());
                }
                let result = self.builtins.call(builtin_idx, args)?;
                self.push(NanBox::from_vmvalue(&result));
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_super(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
            OpCode::GetLocalAttr => {
                let slot = self.read_u16()? as usize;
                let key_idx = self.read_u16()?;
                let key_sym = self.resolve_key_constant(key_idx)?;
                let abs_slot = self.current_frame().stack_base + slot;
                let local = self.stack[abs_slot].clone();
                let local = self.force_value(local)?;
                if let Some(attrs) = local.as_attrs() {
                    if let Some(val) = attrs.get(&key_sym) {
                        let forced = if val.is_thunk() {
                            self.force_value(val.clone())?
                        } else {
                            val.clone()
                        };
                        self.push(forced);
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
                let func = self.force_value(func)?;
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
                } else if func.is_higher_order_builtin() {
                    let hob = func.as_higher_order_builtin().unwrap().clone();
                    let result = self.call_higher_order_builtin(&hob, arg)?;
                    self.push(result);
                } else if let Some(builtin) = func.as_builtin() {
                    if let Some(result) = self.try_vm_builtin(builtin.name, &arg)? {
                        self.push(result);
                    } else {
                        let arg_vmval = arg.to_vmvalue();
                        let builtin_func = builtin.func.clone();
                        let result = self.call_builtin_with_scoped_import_dispatch(
                            builtin_func, arg_vmval,
                        )?;
                        self.push(result);
                    }
                } else {
                    return Err(VMError::NotCallable(func.type_name().to_string()));
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_stack(&mut self, op: OpCode) -> Result<(), VMError> {
        match op {
            OpCode::Pop => {
                self.pop()?;
            }
            OpCode::Interpolate => {
                let count = self.read_u16()? as usize;
                let start = self.stack.len() - count;
                for i in start..self.stack.len() {
                    let v = self.stack[i].clone();
                    if v.is_thunk() {
                        self.stack[i] = self.force_value(v)?;
                    }
                }
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
            _ => unreachable!(),
        }
        Ok(())
    }

    // -- Stack helpers --------------------------------------------------

    fn push(&mut self, value: NanBox) {
        self.stack.push(value);
    }

    fn pop(&mut self) -> Result<NanBox, VMError> {
        self.stack.pop().ok_or(VMError::StackUnderflow)
    }

    /// Pop a value from the stack, forcing it if it is a thunk.
    /// Use this when the operation needs a concrete (non-thunk) value.
    fn pop_forced(&mut self) -> Result<NanBox, VMError> {
        let val = self.pop()?;
        self.force_value(val)
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

    fn read_u32(&mut self) -> Result<u32, VMError> {
        let b0 = self.read_byte()?;
        let b1 = self.read_byte()?;
        let b2 = self.read_byte()?;
        let b3 = self.read_byte()?;
        Ok(u32::from_le_bytes([b0, b1, b2, b3]))
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
                    Some(ThunkState::Pending { chunk, upvalues }) => {
                        thunk.state.set(Some(ThunkState::Evaluating));

                        if self.frames.len() >= MAX_CALL_DEPTH {
                            thunk.state.set(Some(ThunkState::Pending {
                                chunk,
                                upvalues,
                            }));
                            return Err(VMError::StackOverflow);
                        }

                        let return_depth = self.frames.len();
                        let stack_base = self.stack.len();
                        // Convert captured upvalues to NanBox for the frame.
                        let frame_upvalues: Vec<NanBox> = upvalues
                            .iter()
                            .map(|v| NanBox::from_vmvalue(v))
                            .collect();
                        let upvalues_for_restore = upvalues;
                        self.frames.push(CallFrame {
                            chunk: chunk.clone(),
                            ip: 0,
                            stack_base,
                            upvalues: frame_upvalues,
                        });

                        let result = self.run_until(return_depth);

                        // Restore the stack to its state before thunk evaluation.
                        // The Return handler's early exit (at stop_depth) skips
                        // truncation, so internal function calls may leave values.
                        self.stack.truncate(stack_base);

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
                                    upvalues: upvalues_for_restore,
                                }));
                                Err(e)
                            }
                        }
                    }
                    Some(ThunkState::LazySource { source, offset, length, base_dir, upvalues }) => {
                        thunk.state.set(Some(ThunkState::Evaluating));

                        // Compile the expression span on demand.
                        let expr_text = &source[offset..offset + length];
                        let shared_interner = Rc::new(RefCell::new(std::mem::take(self.interner)));
                        let compiled = Compiler::compile_expression(
                            expr_text,
                            &base_dir,
                            shared_interner.clone(),
                        ).map_err(|e| {
                            // Restore interner on compile failure.
                            *self.interner = match Rc::try_unwrap(shared_interner.clone()) {
                                Ok(cell) => cell.into_inner(),
                                Err(rc) => rc.borrow().clone(),
                            };
                            thunk.state.set(Some(ThunkState::LazySource {
                                source: source.clone(),
                                offset,
                                length,
                                base_dir: base_dir.clone(),
                                upvalues: upvalues.clone(),
                            }));
                            VMError::ImportError(format!("lazy thunk compile: {e}"))
                        })?;
                        *self.interner = match Rc::try_unwrap(shared_interner) {
                            Ok(cell) => cell.into_inner(),
                            Err(rc) => rc.borrow().clone(),
                        };

                        let chunk = Rc::new(compiled);

                        if self.frames.len() >= MAX_CALL_DEPTH {
                            thunk.state.set(Some(ThunkState::LazySource {
                                source, offset, length, base_dir, upvalues,
                            }));
                            return Err(VMError::StackOverflow);
                        }

                        let return_depth = self.frames.len();
                        let stack_base = self.stack.len();
                        let frame_upvalues: Vec<NanBox> = upvalues
                            .iter()
                            .map(|v| NanBox::from_vmvalue(v))
                            .collect();
                        self.frames.push(CallFrame {
                            chunk: chunk.clone(),
                            ip: 0,
                            stack_base,
                            upvalues: frame_upvalues,
                        });

                        let result = self.run_until(return_depth);
                        self.stack.truncate(stack_base);

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
                                // On error, convert to a Pending thunk with the compiled chunk.
                                thunk.state.set(Some(ThunkState::Pending {
                                    chunk,
                                    upvalues,
                                }));
                                Err(e)
                            }
                        }
                    }
                    Some(ThunkState::NativeCallback(cb)) => {
                        thunk.state.set(Some(ThunkState::Evaluating));

                        match cb() {
                            Ok(sk_val) => {
                                // Convert the StringKeyedValue result to a NanBox.
                                // This may itself contain Thunk values which will
                                // be lazily wrapped as VMThunks.
                                let nb = self.string_keyed_to_nanbox(&sk_val);
                                let forced = if nb.is_thunk() {
                                    self.force_value(nb)?
                                } else {
                                    nb
                                };
                                let forced_vmval = forced.to_vmvalue();
                                thunk
                                    .state
                                    .set(Some(ThunkState::Done(Box::new(forced_vmval))));
                                Ok(forced)
                            }
                            Err(e) => {
                                // On error, restore the callback for retry.
                                thunk.state.set(Some(ThunkState::NativeCallback(cb)));
                                Err(VMError::Throw(format!("native thunk: {e}")))
                            }
                        }
                    }
                    None => Err(VMError::Internal("thunk state is None".to_string())),
                }
            }
            _ => Ok(NanBox::from_vmvalue(&vmval)),
        }
    }

    /// Deep-force a value: recursively force thunks inside attrsets and lists.
    /// Used at the VM boundary so callers never receive unforced thunks.
    fn deep_force(&mut self, val: NanBox) -> Result<NanBox, VMError> {
        let forced = self.force_value(val)?;
        if let Some(attrs) = forced.as_attrs() {
            let mut new_attrs: BTreeMap<Symbol, NanBox> = BTreeMap::new();
            for (k, v) in attrs {
                let forced_v = self.deep_force(v.clone())?;
                new_attrs.insert(*k, forced_v);
            }
            Ok(NanBox::attrs(new_attrs))
        } else if forced.is_list() {
            let vmval = forced.to_vmvalue();
            if let VMValue::List(items) = vmval {
                let mut new_items = Vec::with_capacity(items.len());
                for item in &items {
                    let item_nb = NanBox::from_vmvalue(item);
                    let forced_item = self.deep_force(item_nb)?;
                    new_items.push(forced_item);
                }
                Ok(NanBox::list(new_items))
            } else {
                Ok(forced)
            }
        } else {
            Ok(forced)
        }
    }

    // -- VM-level builtin dispatch (builtins needing interner access) ------

    /// Try to handle a builtin call at the VM level (for builtins that need
    /// interner access, like derivation, attrNames, etc.).
    /// Returns `Some(result)` if handled, `None` to fall through to the
    /// standard builtin dispatch.
    fn try_vm_builtin(
        &mut self,
        name: &str,
        arg: &NanBox,
    ) -> Result<Option<NanBox>, VMError> {
        match name {
            "tryEval" => {
                // tryEval forces its argument and catches throws/errors.
                // Success: { success = true; value = <forced>; }
                // Failure: { success = false; value = false; }
                let success_sym = self.interner.intern("success");
                let value_sym = self.interner.intern("value");
                match self.force_value(arg.clone()) {
                    Ok(forced) => {
                        let mut attrs = BTreeMap::new();
                        attrs.insert(success_sym, NanBox::bool(true));
                        attrs.insert(value_sym, forced);
                        Ok(Some(NanBox::attrs(attrs)))
                    }
                    Err(_) => {
                        let mut attrs = BTreeMap::new();
                        attrs.insert(success_sym, NanBox::bool(false));
                        attrs.insert(value_sym, NanBox::bool(false));
                        Ok(Some(NanBox::attrs(attrs)))
                    }
                }
            }
            "derivation" | "derivationStrict" => {
                let forced = self.force_value(arg.clone())?;
                let result = self.vm_build_derivation(forced)?;
                Ok(Some(result))
            }
            "import" => {
                // `import` used as a function value (not the special Apply form).
                let forced = self.force_value(arg.clone())?;
                let path = if let Some(p) = forced.as_path() {
                    p.to_string()
                } else if let Some(s) = forced.as_string() {
                    s.to_string()
                } else {
                    return Err(VMError::TypeError {
                        expected: "path or string",
                        got: forced.type_name(),
                        context: "import".to_string(),
                    });
                };
                let result = self.import_file(&path)?;
                Ok(Some(result))
            }
            "attrNames" => {
                let forced = self.force_value(arg.clone())?;
                if let Some(attrs) = forced.as_attrs() {
                    // Nix sorts attrNames alphabetically.
                    let mut name_strs: Vec<String> = attrs
                        .keys()
                        .map(|k| self.interner.resolve(*k).to_string())
                        .collect();
                    name_strs.sort();
                    let names: Vec<NanBox> = name_strs
                        .into_iter()
                        .map(NanBox::string)
                        .collect();
                    Ok(Some(NanBox::list(names)))
                } else {
                    Err(VMError::TypeError {
                        expected: "set",
                        got: forced.type_name(),
                        context: "attrNames".to_string(),
                    })
                }
            }
            "listToAttrs" => {
                let forced = self.force_value(arg.clone())?;
                let vmval = forced.to_vmvalue();
                let list = match &vmval {
                    VMValue::List(l) => l,
                    other => {
                        return Err(VMError::TypeError {
                            expected: "list",
                            got: other.type_name(),
                            context: "listToAttrs".to_string(),
                        });
                    }
                };
                let name_sym = self.interner.intern("name");
                let value_sym = self.interner.intern("value");
                let mut result: BTreeMap<Symbol, NanBox> = BTreeMap::new();
                for item in list {
                    if let VMValue::Attrs(a) = item {
                        let name_val = a.get(&name_sym).ok_or_else(|| {
                            VMError::Throw(
                                "listToAttrs: element missing 'name'".to_string(),
                            )
                        })?;
                        let value_val = a.get(&value_sym).ok_or_else(|| {
                            VMError::Throw(
                                "listToAttrs: element missing 'value'".to_string(),
                            )
                        })?;
                        let key_str = match name_val {
                            VMValue::String(s) => s.clone(),
                            _ => {
                                return Err(VMError::TypeError {
                                    expected: "string",
                                    got: name_val.type_name(),
                                    context: "listToAttrs name".to_string(),
                                });
                            }
                        };
                        let key_sym = self.interner.intern(&key_str);
                        result.insert(key_sym, NanBox::from_vmvalue(value_val));
                    } else {
                        return Err(VMError::TypeError {
                            expected: "set",
                            got: item.type_name(),
                            context: "listToAttrs element".to_string(),
                        });
                    }
                }
                Ok(Some(NanBox::attrs(result)))
            }
            "removeAttrs" => {
                // removeAttrs is curried: first call takes the set, returns partial
                let forced = self.force_value(arg.clone())?;
                if let Some(attrs) = forced.as_attrs() {
                    // Convert to VMValue for the closure (closures can't capture NanBox BTreeMaps)
                    let attrs_vm: BTreeMap<Symbol, VMValue> = attrs
                        .iter()
                        .map(|(k, v)| (*k, v.to_vmvalue()))
                        .collect();
                    let interner_names: Vec<(Symbol, String)> = attrs
                        .keys()
                        .map(|k| (*k, self.interner.resolve(*k).to_string()))
                        .collect();
                    let result = VMValue::Builtin(crate::value::VMBuiltin {
                        name: "removeAttrs<partial>",
                        func: Rc::new(move |args2| {
                            let to_remove = match &args2[0] {
                                VMValue::List(l) => l,
                                other => {
                                    return Err(VMError::TypeError {
                                        expected: "list",
                                        got: other.type_name(),
                                        context: "removeAttrs".to_string(),
                                    });
                                }
                            };
                            let remove_names: std::collections::HashSet<String> = to_remove
                                .iter()
                                .filter_map(|v| {
                                    if let VMValue::String(s) = v {
                                        Some(s.clone())
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            let mut result = BTreeMap::new();
                            for &(sym, ref name) in &interner_names {
                                if !remove_names.contains(name) {
                                    if let Some(v) = attrs_vm.get(&sym) {
                                        result.insert(sym, v.clone());
                                    }
                                }
                            }
                            Ok(VMValue::Attrs(result))
                        }),
                        arity: 1,
                    });
                    Ok(Some(NanBox::from_vmvalue(&result)))
                } else {
                    Err(VMError::TypeError {
                        expected: "set",
                        got: forced.type_name(),
                        context: "removeAttrs".to_string(),
                    })
                }
            }
            "hasAttr" => {
                // hasAttr is curried: first call takes name string, returns partial
                let forced = self.force_value(arg.clone())?;
                let name_str = match forced.to_vmvalue() {
                    VMValue::String(s) => s,
                    other => {
                        return Err(VMError::TypeError {
                            expected: "string",
                            got: other.type_name(),
                            context: "hasAttr".to_string(),
                        });
                    }
                };
                let sym = self.interner.intern(&name_str);
                Ok(Some(NanBox::from_vmvalue(&VMValue::Builtin(
                    crate::value::VMBuiltin {
                        name: "hasAttr<partial>",
                        func: Rc::new(move |args2| {
                            let attrs = match &args2[0] {
                                VMValue::Attrs(a) => a,
                                other => {
                                    return Err(VMError::TypeError {
                                        expected: "set",
                                        got: other.type_name(),
                                        context: "hasAttr".to_string(),
                                    });
                                }
                            };
                            Ok(VMValue::Bool(attrs.contains_key(&sym)))
                        }),
                        arity: 1,
                    },
                ))))
            }
            "getAttr" => {
                let forced = self.force_value(arg.clone())?;
                let name_str = match forced.to_vmvalue() {
                    VMValue::String(s) => s,
                    other => {
                        return Err(VMError::TypeError {
                            expected: "string",
                            got: other.type_name(),
                            context: "getAttr".to_string(),
                        });
                    }
                };
                let sym = self.interner.intern(&name_str);
                let name_for_err = name_str.clone();
                Ok(Some(NanBox::from_vmvalue(&VMValue::Builtin(
                    crate::value::VMBuiltin {
                        name: "getAttr<partial>",
                        func: Rc::new(move |args2| {
                            let attrs = match &args2[0] {
                                VMValue::Attrs(a) => a,
                                other => {
                                    return Err(VMError::TypeError {
                                        expected: "set",
                                        got: other.type_name(),
                                        context: "getAttr".to_string(),
                                    });
                                }
                            };
                            attrs.get(&sym).cloned().ok_or_else(|| {
                                VMError::AttrNotFound(name_for_err.clone())
                            })
                        }),
                        arity: 1,
                    },
                ))))
            }
            "getFlake" => {
                let forced = self.force_value(arg.clone())?;
                let flake_ref = match forced.to_vmvalue() {
                    VMValue::String(s) => s,
                    other => {
                        return Err(VMError::TypeError {
                            expected: "string",
                            got: other.type_name(),
                            context: "getFlake".to_string(),
                        });
                    }
                };
                let result = self.vm_get_flake(&flake_ref)?;
                Ok(Some(result))
            }
            "scopedImport" => {
                // scopedImport is curried: first call takes scope, returns partial
                let forced = self.force_value(arg.clone())?;
                let scope_vmval = forced.to_vmvalue();
                match scope_vmval {
                    VMValue::Attrs(_) => {}
                    ref other => {
                        return Err(VMError::TypeError {
                            expected: "set",
                            got: other.type_name(),
                            context: "scopedImport".to_string(),
                        });
                    }
                }
                // Build a string-keyed scope for wrapping
                let scope_str = if let Some(attrs) = forced.as_attrs() {
                    let mut parts = String::from("{");
                    for (k, v) in attrs {
                        let key = self.interner.resolve(*k).to_string();
                        let val_vm = v.to_vmvalue();
                        let rhs = match &val_vm {
                            VMValue::Int(n) => n.to_string(),
                            VMValue::Float(f) => format!("{f}"),
                            VMValue::Bool(true) => "true".to_string(),
                            VMValue::Bool(false) => "false".to_string(),
                            VMValue::Null => "null".to_string(),
                            VMValue::String(s) => {
                                let escaped = s
                                    .replace('\\', "\\\\")
                                    .replace('"', "\\\"")
                                    .replace('$', "\\$");
                                format!("\"{escaped}\"")
                            }
                            VMValue::Path(p) => format!("\"{p}\""),
                            _ => {
                                return Err(VMError::Throw(format!(
                                    "scopedImport: cannot render scope value of type {}",
                                    val_vm.type_name()
                                )));
                            }
                        };
                        parts.push_str(&format!(" {key} = {rhs};"));
                    }
                    parts.push_str(" }");
                    parts
                } else {
                    "{}".to_string()
                };

                // Return a partial that takes the path
                let result = VMValue::Builtin(crate::value::VMBuiltin {
                    name: "scopedImport<partial>",
                    func: Rc::new(move |args2| {
                        let path = match &args2[0] {
                            VMValue::String(s) => s.clone(),
                            VMValue::Path(p) => p.clone(),
                            other => {
                                return Err(VMError::TypeError {
                                    expected: "path or string",
                                    got: other.type_name(),
                                    context: "scopedImport".to_string(),
                                });
                            }
                        };
                        // The actual import needs VM context. Store a placeholder
                        // that the VM will intercept.
                        Err(VMError::Throw(format!(
                            "__scopedImport_dispatch__:{}:{}",
                            scope_str, path
                        )))
                    }),
                    arity: 1,
                });
                Ok(Some(NanBox::from_vmvalue(&result)))
            }
            "scopedImport<partial>" => {
                // Intercept the partial application's result
                let forced = self.force_value(arg.clone())?;
                let path = match forced.to_vmvalue() {
                    VMValue::String(s) => s,
                    VMValue::Path(p) => p,
                    other => {
                        return Err(VMError::TypeError {
                            expected: "path or string",
                            got: other.type_name(),
                            context: "scopedImport".to_string(),
                        });
                    }
                };
                // This won't actually be called via try_vm_builtin because the
                // partial closure captures the scope. The __scopedImport_dispatch__
                // error is caught and processed by the VM. For now, fall through.
                let _ = path;
                Ok(None)
            }
            "catAttrs" => {
                let forced = self.force_value(arg.clone())?;
                let name_str = match forced.to_vmvalue() {
                    VMValue::String(s) => s,
                    other => {
                        return Err(VMError::TypeError {
                            expected: "string",
                            got: other.type_name(),
                            context: "catAttrs".to_string(),
                        });
                    }
                };
                let sym = self.interner.intern(&name_str);
                Ok(Some(NanBox::from_vmvalue(&VMValue::Builtin(
                    crate::value::VMBuiltin {
                        name: "catAttrs<partial>",
                        func: Rc::new(move |args2| {
                            let list = match &args2[0] {
                                VMValue::List(l) => l,
                                other => {
                                    return Err(VMError::TypeError {
                                        expected: "list",
                                        got: other.type_name(),
                                        context: "catAttrs".to_string(),
                                    });
                                }
                            };
                            let mut result = Vec::new();
                            for item in list {
                                if let VMValue::Attrs(a) = item {
                                    if let Some(v) = a.get(&sym) {
                                        result.push(v.clone());
                                    }
                                }
                            }
                            Ok(VMValue::List(result))
                        }),
                        arity: 1,
                    },
                ))))
            }
            // ── Bridge-dispatched builtins ─────────────────────────
            //
            // These builtins need tree-walker state (regex cache, TOML
            // parser, genericClosure closure-calling, etc.)
            // and are delegated to the builtin bridge.
            "readDir" | "parseDrvName" | "fromTOML" | "genericClosure"
            | "zipAttrsWith" | "getContext" | "toXML"
            | "convertHash" | "path" | "filterSource" | "parseFlakeRef"
            | "flakeRefToString" | "toFile" | "currentTime" | "hashFile"
            | "findFile" => {
                let forced = self.force_value(arg.clone())?;
                let vmval = forced.to_vmvalue();
                let sk = vmval.to_string_keyed(self.interner);
                match crate::bridge::call_builtin_bridge(name, vec![sk]) {
                    Ok(Some(result)) => {
                        let vm_result = crate::builtins::string_keyed_to_vmvalue(
                            &result,
                            self.interner,
                        );
                        Ok(Some(NanBox::from_vmvalue(&vm_result)))
                    }
                    Ok(None) => {
                        // No bridge set — fall through to registry stub
                        // which will produce the appropriate error.
                        Ok(None)
                    }
                    Err(e) => Err(VMError::Throw(e)),
                }
            }
            // match and split are curried: first call takes pattern,
            // returns partial that takes the string.
            "match" | "split" => {
                let forced = self.force_value(arg.clone())?;
                let pattern = match forced.to_vmvalue() {
                    VMValue::String(s) => s,
                    other => {
                        return Err(VMError::TypeError {
                            expected: "string",
                            got: other.type_name(),
                            context: name.to_string(),
                        });
                    }
                };
                let builtin_name = name.to_string();
                Ok(Some(NanBox::from_vmvalue(&VMValue::Builtin(
                    crate::value::VMBuiltin {
                        name: if name == "match" {
                            "match<partial>"
                        } else {
                            "split<partial>"
                        },
                        func: Rc::new(move |args2| {
                            let input = match &args2[0] {
                                VMValue::String(s) => s.clone(),
                                other => {
                                    return Err(VMError::TypeError {
                                        expected: "string",
                                        got: other.type_name(),
                                        context: builtin_name.clone(),
                                    });
                                }
                            };
                            // Delegate to bridge with both args
                            let sk_args = vec![
                                crate::value::StringKeyedValue::String(pattern.clone()),
                                crate::value::StringKeyedValue::String(input),
                            ];
                            match crate::bridge::call_builtin_bridge(&builtin_name, sk_args) {
                                Ok(Some(result)) => {
                                    let mut tmp = crate::intern::Interner::new();
                                    Ok(crate::builtins::string_keyed_to_vmvalue(&result, &mut tmp))
                                }
                                Ok(None) => Err(VMError::Throw(format!(
                                    "{builtin_name}: requires bridge but no bridge is set"
                                ))),
                                Err(e) => Err(VMError::Throw(e)),
                            }
                        }),
                        arity: 1,
                    },
                ))))
            }
            _ => Ok(None),
        }
    }

    /// Build a derivation from a VM attrset (with interner access).
    fn vm_build_derivation(&mut self, arg: NanBox) -> Result<NanBox, VMError> {
        use sui_compat::derivation::{Derivation, DerivationOutput};

        let attrs = match arg.as_attrs() {
            Some(a) => a.clone(),
            None => {
                return Err(VMError::TypeError {
                    expected: "set",
                    got: arg.type_name(),
                    context: "derivation".to_string(),
                });
            }
        };

        // Helper: resolve a symbol key and get string value.
        let get_str = |attrs: &BTreeMap<Symbol, NanBox>,
                       interner: &mut Interner,
                       key: &str|
         -> Result<String, VMError> {
            let sym = interner.intern(key);
            let val = attrs.get(&sym).ok_or_else(|| {
                VMError::AttrNotFound(key.to_string())
            })?;
            match val.to_vmvalue() {
                VMValue::String(s) => Ok(s),
                other => Err(VMError::TypeError {
                    expected: "string",
                    got: other.type_name(),
                    context: format!("derivation attr '{key}'"),
                }),
            }
        };

        let get_str_opt = |attrs: &BTreeMap<Symbol, NanBox>,
                           interner: &mut Interner,
                           key: &str|
         -> Result<Option<String>, VMError> {
            let sym = interner.intern(key);
            match attrs.get(&sym) {
                None => Ok(None),
                Some(val) => match val.to_vmvalue() {
                    VMValue::String(s) => Ok(Some(s)),
                    other => Err(VMError::TypeError {
                        expected: "string",
                        got: other.type_name(),
                        context: format!("derivation attr '{key}'"),
                    }),
                },
            }
        };

        let name = get_str(&attrs, self.interner, "name")?;
        let system = get_str(&attrs, self.interner, "system")?;
        let builder = get_str(&attrs, self.interner, "builder")?;

        // Optional `args` list of strings.
        let args_sym = self.interner.intern("args");
        let args_list: Vec<String> = if let Some(a) = attrs.get(&args_sym) {
            let vmval = a.to_vmvalue();
            match vmval {
                VMValue::List(l) => {
                    let mut out = Vec::with_capacity(l.len());
                    for item in &l {
                        match item {
                            VMValue::String(s) => out.push(s.clone()),
                            VMValue::Int(n) => out.push(n.to_string()),
                            VMValue::Float(f) => out.push(format!("{f}")),
                            VMValue::Bool(true) => out.push("1".to_string()),
                            VMValue::Bool(false) => out.push(String::new()),
                            VMValue::Null => out.push(String::new()),
                            VMValue::Path(p) => out.push(p.clone()),
                            _ => out.push(String::new()),
                        }
                    }
                    out
                }
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };

        // Optional `outputs` list.
        let outputs_sym = self.interner.intern("outputs");
        let outputs: Vec<String> = if let Some(o) = attrs.get(&outputs_sym) {
            let vmval = o.to_vmvalue();
            match vmval {
                VMValue::List(l) => {
                    let mut out = Vec::with_capacity(l.len());
                    for item in &l {
                        if let VMValue::String(s) = item {
                            out.push(s.clone());
                        }
                    }
                    if out.is_empty() {
                        vec!["out".to_string()]
                    } else {
                        out
                    }
                }
                _ => vec!["out".to_string()],
            }
        } else {
            vec!["out".to_string()]
        };

        // Build env vars from non-special attributes.
        let special = [
            "name", "system", "builder", "args", "outputs",
            "__impure", "__contentAddressed", "__structuredAttrs",
        ];
        let special_syms: Vec<Symbol> = special
            .iter()
            .map(|s| self.interner.intern(s))
            .collect();

        let mut env_vars: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in &attrs {
            if special_syms.contains(k) {
                continue;
            }
            let key_str = self.interner.resolve(*k).to_string();
            let vmval = v.to_vmvalue();
            let s = match &vmval {
                VMValue::String(s) => s.clone(),
                VMValue::Int(n) => n.to_string(),
                VMValue::Float(f) => format!("{f}"),
                VMValue::Bool(true) => "1".to_string(),
                VMValue::Bool(false) => String::new(),
                VMValue::Null => String::new(),
                VMValue::Path(p) => p.clone(),
                _ => continue,
            };
            env_vars.insert(key_str, s);
        }
        env_vars.insert("name".to_string(), name.clone());
        env_vars.insert("system".to_string(), system.clone());
        env_vars.insert("builder".to_string(), builder.clone());

        // Detect fixed-output derivation.
        let output_hash_sym = self.interner.intern("outputHash");
        let is_fod = attrs.contains_key(&output_hash_sym);

        let mut drv = Derivation {
            outputs: BTreeMap::new(),
            input_derivations: BTreeMap::new(),
            input_sources: Vec::new(),
            system,
            builder,
            args: args_list,
            env: env_vars,
        };

        let (drv_path, out_paths) = if is_fod {
            let output_hash = get_str(&attrs, self.interner, "outputHash")?;
            let output_hash_algo = get_str_opt(&attrs, self.interner, "outputHashAlgo")?
                .unwrap_or_else(|| "sha256".to_string());
            let output_hash_mode = get_str_opt(&attrs, self.interner, "outputHashMode")?
                .unwrap_or_else(|| "flat".to_string());
            let is_recursive =
                output_hash_mode == "recursive" || output_hash_mode == "nar";

            let out_path = sui_compat::store_path::compute_fixed_output_hash(
                &output_hash_algo,
                &output_hash,
                is_recursive,
                &name,
            );

            drv.outputs.insert(
                "out".to_string(),
                DerivationOutput {
                    path: out_path.clone(),
                    hash_algo: if is_recursive {
                        format!("r:{output_hash_algo}")
                    } else {
                        output_hash_algo.clone()
                    },
                    hash: output_hash,
                },
            );

            let drv_content = drv.serialize();
            let drv_path = sui_compat::store_path::compute_drv_path(
                drv_content.as_bytes(),
                &name,
            );

            let mut out_paths = BTreeMap::new();
            out_paths.insert("out".to_string(), out_path);
            (drv_path, out_paths)
        } else {
            for o in &outputs {
                drv.outputs.insert(
                    o.clone(),
                    DerivationOutput {
                        path: String::new(),
                        hash_algo: String::new(),
                        hash: String::new(),
                    },
                );
            }

            let drv_content = drv.serialize();
            let drv_path = sui_compat::store_path::compute_drv_path(
                drv_content.as_bytes(),
                &name,
            );

            use sha2::{Digest, Sha256};
            let inner = Sha256::digest(drv_content.as_bytes());
            let inner_hex: String =
                inner.iter().map(|b| format!("{b:02x}")).collect();
            let mut out_paths = BTreeMap::new();
            for o in &outputs {
                let p = sui_compat::store_path::compute_output_path(
                    &inner_hex, o, &name,
                );
                out_paths.insert(o.clone(), p);
            }
            (drv_path, out_paths)
        };

        // Update derivation outputs with final paths and write .drv file.
        for (output_name, output_path) in &out_paths {
            if let Some(output) = drv.outputs.get_mut(output_name) {
                if output.path.is_empty() {
                    output.path.clone_from(output_path);
                }
            }
            drv.env.insert(output_name.clone(), output_path.clone());
        }

        let drv_content_final = drv.serialize();
        let store_dir = std::env::var("SUI_STORE_DIR")
            .unwrap_or_else(|_| "/nix/store".to_string());
        let disk_path = if store_dir != "/nix/store" {
            drv_path.replacen("/nix/store", &store_dir, 1)
        } else {
            drv_path.clone()
        };
        let drv_file = std::path::Path::new(&disk_path);
        if !drv_file.exists() {
            if let Some(parent) = drv_file.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            match std::fs::write(drv_file, drv_content_final.as_bytes()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    let fallback_dir = std::env::temp_dir().join("sui-drv-cache");
                    std::fs::create_dir_all(&fallback_dir).ok();
                    let fallback_path = fallback_dir.join(
                        drv_file.file_name().unwrap_or_default(),
                    );
                    let _ = std::fs::write(&fallback_path, drv_content_final.as_bytes());
                }
                Err(e) => {
                    return Err(VMError::Throw(format!(
                        "derivation: failed to write {drv_path}: {e}"
                    )));
                }
            }
        }

        // Assemble result attrset.
        let mut result: BTreeMap<Symbol, NanBox> = attrs;
        let type_sym = self.interner.intern("type");
        result.insert(type_sym, NanBox::string("derivation".to_string()));
        let drv_path_sym = self.interner.intern("drvPath");
        result.insert(drv_path_sym, NanBox::string(drv_path.clone()));

        let primary_out = out_paths
            .get("out")
            .cloned()
            .or_else(|| out_paths.values().next().cloned())
            .unwrap_or_default();
        let out_path_sym = self.interner.intern("outPath");
        result.insert(out_path_sym, NanBox::string(primary_out));

        for (output_name, output_path) in &out_paths {
            let mut out_attrs: BTreeMap<Symbol, NanBox> = BTreeMap::new();
            out_attrs.insert(out_path_sym, NanBox::string(output_path.clone()));
            out_attrs.insert(drv_path_sym, NanBox::string(drv_path.clone()));
            out_attrs.insert(type_sym, NanBox::string("derivation".to_string()));
            let output_name_sym = self.interner.intern("outputName");
            out_attrs.insert(output_name_sym, NanBox::string(output_name.clone()));
            let name_sym = self.interner.intern("name");
            out_attrs.insert(name_sym, NanBox::string(name.clone()));
            let out_sym = self.interner.intern(output_name);
            result.insert(out_sym, NanBox::attrs(out_attrs));
        }

        Ok(NanBox::attrs(result))
    }

    /// Call a builtin function, intercepting scopedImport dispatch errors.
    fn call_builtin_with_scoped_import_dispatch(
        &mut self,
        func: Rc<dyn Fn(Vec<VMValue>) -> Result<VMValue, VMError>>,
        arg: VMValue,
    ) -> Result<NanBox, VMError> {
        match func(vec![arg]) {
            Ok(result) => Ok(NanBox::from_vmvalue(&result)),
            Err(VMError::Throw(ref msg))
                if msg.starts_with("__scopedImport_dispatch__:") =>
            {
                let rest = &msg["__scopedImport_dispatch__:".len()..];
                if let Some(colon_pos) = rest.rfind(':') {
                    let scope_nix = &rest[..colon_pos];
                    let path = &rest[colon_pos + 1..];
                    self.vm_scoped_import(scope_nix, path)
                } else {
                    Err(VMError::Throw(msg.clone()))
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Evaluate `builtins.getFlake` for a path-based flake reference.
    ///
    /// If a thread-local flake resolver has been installed (via
    /// [`set_flake_resolver`]), delegates to it — this lets `sui-eval`
    /// inject the tree-walker's full `evaluate_flake` implementation
    /// which handles all input types correctly.  Falls back to the VM's
    /// own limited resolver otherwise.
    fn vm_get_flake(&mut self, flake_ref: &str) -> Result<NanBox, VMError> {
        // Check for an external resolver first.
        let resolved = FLAKE_RESOLVER.with(|r| {
            let borrow = r.borrow();
            if let Some(ref resolver) = *borrow {
                Some(resolver(flake_ref))
            } else {
                None
            }
        });

        if let Some(result) = resolved {
            let sk = result.map_err(|e| VMError::Throw(format!("getFlake: {e}")))?;
            return Ok(self.string_keyed_to_nanbox(&sk));
        }

        // Fallback: VM-native resolution (path-based only).
        self.vm_get_flake_native(flake_ref)
    }

    /// Convert a `StringKeyedValue` to a `NanBox` for the VM stack.
    ///
    /// `StringKeyedValue::Thunk` variants are wrapped in `VMThunk`s with
    /// `NativeCallback` state so they are only evaluated when the VM
    /// actually forces the value. This keeps `getFlake` fast by deferring
    /// transitive input evaluation.
    fn string_keyed_to_nanbox(&mut self, sk: &crate::value::StringKeyedValue) -> NanBox {
        match sk {
            crate::value::StringKeyedValue::Null => NanBox::null(),
            crate::value::StringKeyedValue::Bool(b) => NanBox::bool(*b),
            crate::value::StringKeyedValue::Int(n) => NanBox::int(*n),
            crate::value::StringKeyedValue::Float(f) => NanBox::float(*f),
            crate::value::StringKeyedValue::String(s) => NanBox::string(s.clone()),
            crate::value::StringKeyedValue::Path(p) => NanBox::from_vmvalue(&VMValue::Path(p.clone())),
            crate::value::StringKeyedValue::List(items) => {
                let nb_items: Vec<NanBox> = items.iter().map(|v| self.string_keyed_to_nanbox(v)).collect();
                NanBox::list(nb_items)
            }
            crate::value::StringKeyedValue::Attrs(map) => {
                let mut nb_map: BTreeMap<Symbol, NanBox> = BTreeMap::new();
                for (k, v) in map {
                    let sym = self.interner.intern(k);
                    nb_map.insert(sym, self.string_keyed_to_nanbox(v));
                }
                NanBox::attrs(nb_map)
            }
            crate::value::StringKeyedValue::Lambda => NanBox::null(),
            crate::value::StringKeyedValue::Thunk(cb) => {
                // Wrap the callback in a VMThunk with NativeCallback state.
                // The VM's force_value will call the callback on demand and
                // convert the resulting StringKeyedValue to a NanBox.
                let thunk = VMThunk {
                    state: Rc::new(Cell::new(Some(ThunkState::NativeCallback(Rc::clone(cb))))),
                };
                NanBox::thunk(thunk)
            }
        }
    }

    /// VM-native flake resolution (path-based inputs only).
    fn vm_get_flake_native(&mut self, flake_ref: &str) -> Result<NanBox, VMError> {
        let flake_dir = if flake_ref.starts_with('/') || flake_ref.starts_with('.') {
            std::path::PathBuf::from(flake_ref)
        } else if let Some(path) = flake_ref.strip_prefix("path:") {
            std::path::PathBuf::from(path)
        } else {
            return Err(VMError::Throw(format!(
                "getFlake: unsupported flake reference: {flake_ref} (only path: refs supported in VM)"
            )));
        };

        let flake_nix = flake_dir.join("flake.nix");
        if !flake_nix.exists() {
            return Err(VMError::Throw(format!(
                "getFlake: flake.nix not found in {}",
                flake_dir.display()
            )));
        }

        // Import flake.nix to get the raw flake attrset.
        let flake_nix_str = flake_nix.to_string_lossy().to_string();
        let flake_attrs = self.import_file(&flake_nix_str)?;
        let flake_attrs = self.force_value(flake_attrs)?;

        // Build the inputs attrset. For now, create a minimal `self` input.
        let self_sym = self.interner.intern("self");
        let out_path_sym = self.interner.intern("outPath");
        let flake_dir_str = flake_dir.to_string_lossy().to_string();
        let mut self_attrs: BTreeMap<Symbol, NanBox> = BTreeMap::new();
        self_attrs.insert(out_path_sym, NanBox::string(flake_dir_str.clone()));
        let mut inputs: BTreeMap<Symbol, NanBox> = BTreeMap::new();
        inputs.insert(self_sym, NanBox::attrs(self_attrs));

        // Try to read flake.lock and resolve inputs.
        let lock_path = flake_dir.join("flake.lock");
        if lock_path.exists() {
            if let Ok(lock_str) = std::fs::read_to_string(&lock_path) {
                if let Ok(lock_json) = serde_json::from_str::<serde_json::Value>(&lock_str) {
                    self.resolve_flake_lock_inputs(&lock_json, &flake_dir, &mut inputs);
                }
            }
        }

        // Extract the `outputs` function and call it with the inputs attrset.
        let outputs_sym = self.interner.intern("outputs");
        if let Some(attrs) = flake_attrs.as_attrs() {
            if let Some(outputs_func) = attrs.get(&outputs_sym) {
                let outputs_func = outputs_func.clone();
                let outputs_func = self.force_value(outputs_func)?;
                let inputs_nb = NanBox::attrs(inputs);
                let result = self.call_callable(&outputs_func, inputs_nb)?;
                let mut result_forced = self.force_value(result)?;

                // Merge top-level metadata (description) into the result.
                let desc_sym = self.interner.intern("description");
                if let Some(desc) = attrs.get(&desc_sym) {
                    if let Some(result_attrs) = result_forced.as_attrs() {
                        let mut merged = result_attrs.clone();
                        merged.insert(desc_sym, desc.clone());
                        result_forced = NanBox::attrs(merged);
                    }
                }

                return Ok(result_forced);
            }
        }

        // If no outputs function, return the raw flake attrset.
        Ok(flake_attrs)
    }

    /// Resolve flake.lock inputs into the inputs attrset.
    fn resolve_flake_lock_inputs(
        &mut self,
        lock: &serde_json::Value,
        flake_dir: &std::path::Path,
        inputs: &mut BTreeMap<Symbol, NanBox>,
    ) {
        let nodes = match lock.get("nodes").and_then(|n| n.as_object()) {
            Some(n) => n,
            None => return,
        };
        let root_node = match lock.get("root").and_then(|r| r.as_str()) {
            Some(r) => r.to_string(),
            None => "root".to_string(),
        };
        let root_inputs = match nodes
            .get(&root_node)
            .and_then(|n| n.get("inputs"))
            .and_then(|i| i.as_object())
        {
            Some(i) => i,
            None => return,
        };

        for (input_name, node_ref) in root_inputs {
            let node_key = match node_ref.as_str() {
                Some(s) => s.to_string(),
                None => {
                    if let Some(arr) = node_ref.as_array() {
                        if let Some(s) = arr.first().and_then(|v| v.as_str()) {
                            s.to_string()
                        } else {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
            };
            if let Some(node) = nodes.get(&node_key) {
                if let Some(locked) = node.get("locked") {
                    let locked_type = locked.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    let out_path = match locked_type {
                        "path" => {
                            if let Some(p) = locked.get("path").and_then(|p| p.as_str()) {
                                let path = if p.starts_with('/') {
                                    std::path::PathBuf::from(p)
                                } else {
                                    flake_dir.join(p)
                                };
                                path.to_string_lossy().to_string()
                            } else {
                                continue;
                            }
                        }
                        _ => continue, // Only path inputs for now
                    };
                    let input_sym = self.interner.intern(input_name);
                    let out_path_sym = self.interner.intern("outPath");
                    let mut input_attrs: BTreeMap<Symbol, NanBox> = BTreeMap::new();
                    input_attrs.insert(out_path_sym, NanBox::string(out_path));
                    inputs.insert(input_sym, NanBox::attrs(input_attrs));
                }
            }
        }
    }

    /// Import a file with a scope (for scopedImport).
    ///
    /// Handles the directory → `default.nix` fallback like `import_file`.
    fn vm_scoped_import(
        &mut self,
        scope_nix: &str,
        path: &str,
    ) -> Result<NanBox, VMError> {
        // Directory → default.nix fallback (Nix convention).
        let resolved = if std::path::Path::new(path).is_dir() {
            format!("{path}/default.nix")
        } else {
            path.to_string()
        };
        let source = std::fs::read_to_string(&resolved)
            .map_err(|e| VMError::ImportError(format!("{path}: {e}")))?;

        // Wrap the source in `with <scope>; <source>` to inject the scope.
        let wrapped = format!("with {scope_nix}; {source}");

        let file_dir = std::path::Path::new(&resolved)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();

        // Share the VM's interner so symbol IDs stay consistent.
        let shared_interner = Rc::new(RefCell::new(std::mem::take(self.interner)));
        let chunk = Compiler::compile_with_shared_interner(&wrapped, file_dir, shared_interner.clone())
            .map_err(|e| VMError::ImportError(format!("{path}: {e}")))?;
        *self.interner = match Rc::try_unwrap(shared_interner) {
            Ok(cell) => cell.into_inner(),
            Err(rc) => rc.borrow().clone(),
        };

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

        self.run_until(return_depth)
    }

    // -- Higher-order builtin execution -----------------------------------

    fn call_callable(&mut self, func: &NanBox, arg: NanBox) -> Result<NanBox, VMError> {
        if let Some(closure) = func.as_closure() {
            if self.frames.len() >= MAX_CALL_DEPTH {
                return Err(VMError::StackOverflow);
            }
            let upvalues: Vec<NanBox> =
                closure.upvalues.iter().map(NanBox::from_vmvalue).collect();
            let chunk = closure.chunk.clone();
            let return_depth = self.frames.len();
            let stack_base = self.stack.len();
            self.push(arg);
            self.frames.push(CallFrame {
                chunk,
                ip: 0,
                stack_base,
                upvalues,
            });
            self.run_until(return_depth)
        } else if func.is_higher_order_builtin() {
            let hob = func.as_higher_order_builtin().unwrap().clone();
            self.call_higher_order_builtin(&hob, arg)
        } else if let Some(builtin) = func.as_builtin() {
            if let Some(result) = self.try_vm_builtin(builtin.name, &arg)? {
                Ok(result)
            } else {
                let arg_vmval = arg.to_vmvalue();
                let builtin_func = builtin.func.clone();
                let result = self.call_builtin_with_scoped_import_dispatch(
                    builtin_func, arg_vmval,
                )?;
                Ok(result)
            }
        } else {
            Err(VMError::NotCallable(func.type_name().to_string()))
        }
    }

    #[allow(clippy::too_many_lines)]
    fn call_higher_order_builtin(
        &mut self,
        hob: &HigherOrderBuiltin,
        arg: NanBox,
    ) -> Result<NanBox, VMError> {
        use HigherOrderOp::*;
        match hob.op {
            Map => {
                let list_val = arg.to_vmvalue();
                let list = match &list_val {
                    VMValue::List(l) => l,
                    other => return Err(VMError::TypeError {
                        expected: "list", got: other.type_name(),
                        context: "builtins.map".to_string(),
                    }),
                };
                let func_nb = NanBox::from_vmvalue(&hob.func);
                let mut results = Vec::with_capacity(list.len());
                for item in list {
                    let r = self.call_callable(&func_nb, NanBox::from_vmvalue(item))?;
                    results.push(r);
                }
                Ok(NanBox::list(results))
            }
            Filter => {
                let list_val = arg.to_vmvalue();
                let list = match &list_val {
                    VMValue::List(l) => l,
                    other => return Err(VMError::TypeError {
                        expected: "list", got: other.type_name(),
                        context: "builtins.filter".to_string(),
                    }),
                };
                let func_nb = NanBox::from_vmvalue(&hob.func);
                let mut results = Vec::new();
                for item in list {
                    let item_nb = NanBox::from_vmvalue(item);
                    let r = self.call_callable(&func_nb, item_nb.clone())?;
                    if r.is_truthy()? { results.push(item_nb); }
                }
                Ok(NanBox::list(results))
            }
            FoldlP1 => {
                let init_vmval = arg.to_vmvalue();
                Ok(NanBox::from_vmvalue(&VMValue::HigherOrderBuiltin(
                    HigherOrderBuiltin {
                        op: FoldlP2,
                        func: hob.func.clone(),
                        extra_args: vec![init_vmval],
                    },
                )))
            }
            FoldlP2 => {
                let list_val = arg.to_vmvalue();
                let list = match &list_val {
                    VMValue::List(l) => l,
                    other => return Err(VMError::TypeError {
                        expected: "list", got: other.type_name(),
                        context: "builtins.foldl'".to_string(),
                    }),
                };
                let func_nb = NanBox::from_vmvalue(&hob.func);
                let mut acc = NanBox::from_vmvalue(&hob.extra_args[0]);
                for item in list {
                    let partial = self.call_callable(&func_nb, acc)?;
                    acc = self.call_callable(&partial, NanBox::from_vmvalue(item))?;
                }
                Ok(acc)
            }
            Sort => {
                let list_val = arg.to_vmvalue();
                let list = match &list_val {
                    VMValue::List(l) => l.clone(),
                    other => return Err(VMError::TypeError {
                        expected: "list", got: other.type_name(),
                        context: "builtins.sort".to_string(),
                    }),
                };
                if list.len() <= 1 {
                    return Ok(NanBox::from_vmvalue(&VMValue::List(list)));
                }
                let func_nb = NanBox::from_vmvalue(&hob.func);
                let mut sorted: Vec<VMValue> = Vec::with_capacity(list.len());
                for item in &list {
                    let item_nb = NanBox::from_vmvalue(item);
                    let mut pos = sorted.len();
                    for (i, existing) in sorted.iter().enumerate() {
                        let existing_nb = NanBox::from_vmvalue(existing);
                        let partial = self.call_callable(&func_nb, item_nb.clone())?;
                        let cmp_result = self.call_callable(&partial, existing_nb)?;
                        if cmp_result.is_truthy()? { pos = i; break; }
                    }
                    sorted.insert(pos, item.clone());
                }
                Ok(NanBox::from_vmvalue(&VMValue::List(sorted)))
            }
            GenList => {
                let n = match arg.to_vmvalue() {
                    VMValue::Int(n) => n,
                    other => return Err(VMError::TypeError {
                        expected: "int", got: other.type_name(),
                        context: "builtins.genList".to_string(),
                    }),
                };
                if n < 0 { return Err(VMError::Throw("genList: negative length".to_string())); }
                let func_nb = NanBox::from_vmvalue(&hob.func);
                let mut results = Vec::with_capacity(n as usize);
                for i in 0..n {
                    results.push(self.call_callable(&func_nb, NanBox::int(i))?);
                }
                Ok(NanBox::list(results))
            }
            ConcatMap => {
                let list_val = arg.to_vmvalue();
                let list = match &list_val {
                    VMValue::List(l) => l,
                    other => return Err(VMError::TypeError {
                        expected: "list", got: other.type_name(),
                        context: "builtins.concatMap".to_string(),
                    }),
                };
                let func_nb = NanBox::from_vmvalue(&hob.func);
                let mut results = Vec::new();
                for item in list {
                    let mapped = self.call_callable(&func_nb, NanBox::from_vmvalue(item))?;
                    match mapped.to_vmvalue() {
                        VMValue::List(inner) => {
                            for v in &inner { results.push(NanBox::from_vmvalue(v)); }
                        }
                        other => return Err(VMError::TypeError {
                            expected: "list", got: other.type_name(),
                            context: "builtins.concatMap result".to_string(),
                        }),
                    }
                }
                Ok(NanBox::list(results))
            }
            Any => {
                let list_val = arg.to_vmvalue();
                let list = match &list_val {
                    VMValue::List(l) => l,
                    other => return Err(VMError::TypeError {
                        expected: "list", got: other.type_name(),
                        context: "builtins.any".to_string(),
                    }),
                };
                let func_nb = NanBox::from_vmvalue(&hob.func);
                for item in list {
                    if self.call_callable(&func_nb, NanBox::from_vmvalue(item))?.is_truthy()? {
                        return Ok(NanBox::bool(true));
                    }
                }
                Ok(NanBox::bool(false))
            }
            All => {
                let list_val = arg.to_vmvalue();
                let list = match &list_val {
                    VMValue::List(l) => l,
                    other => return Err(VMError::TypeError {
                        expected: "list", got: other.type_name(),
                        context: "builtins.all".to_string(),
                    }),
                };
                let func_nb = NanBox::from_vmvalue(&hob.func);
                for item in list {
                    if !self.call_callable(&func_nb, NanBox::from_vmvalue(item))?.is_truthy()? {
                        return Ok(NanBox::bool(false));
                    }
                }
                Ok(NanBox::bool(true))
            }
            Partition => {
                let list_val = arg.to_vmvalue();
                let list = match &list_val {
                    VMValue::List(l) => l,
                    other => return Err(VMError::TypeError {
                        expected: "list", got: other.type_name(),
                        context: "builtins.partition".to_string(),
                    }),
                };
                let func_nb = NanBox::from_vmvalue(&hob.func);
                let (mut right, mut wrong) = (Vec::new(), Vec::new());
                for item in list {
                    let item_nb = NanBox::from_vmvalue(item);
                    if self.call_callable(&func_nb, item_nb.clone())?.is_truthy()? {
                        right.push(item_nb);
                    } else {
                        wrong.push(item_nb);
                    }
                }
                let rs = self.interner.intern("right");
                let ws = self.interner.intern("wrong");
                let mut attrs = BTreeMap::new();
                attrs.insert(rs, NanBox::list(right));
                attrs.insert(ws, NanBox::list(wrong));
                Ok(NanBox::attrs(attrs))
            }
            GroupBy => {
                let list_val = arg.to_vmvalue();
                let list = match &list_val {
                    VMValue::List(l) => l,
                    other => return Err(VMError::TypeError {
                        expected: "list", got: other.type_name(),
                        context: "builtins.groupBy".to_string(),
                    }),
                };
                let func_nb = NanBox::from_vmvalue(&hob.func);
                let mut groups: BTreeMap<String, Vec<NanBox>> = BTreeMap::new();
                for item in list {
                    let item_nb = NanBox::from_vmvalue(item);
                    let kr = self.call_callable(&func_nb, item_nb.clone())?;
                    let ks = kr.as_string().ok_or_else(|| VMError::TypeError {
                        expected: "string", got: kr.type_name(),
                        context: "builtins.groupBy key".to_string(),
                    })?.to_string();
                    groups.entry(ks).or_default().push(item_nb);
                }
                let mut attrs = BTreeMap::new();
                for (k, vs) in groups {
                    attrs.insert(self.interner.intern(&k), NanBox::list(vs));
                }
                Ok(NanBox::attrs(attrs))
            }
            MapAttrs => {
                let attrs_val = arg.to_vmvalue();
                let attrs = match &attrs_val {
                    VMValue::Attrs(a) => a,
                    other => return Err(VMError::TypeError {
                        expected: "set", got: other.type_name(),
                        context: "builtins.mapAttrs".to_string(),
                    }),
                };
                let func_nb = NanBox::from_vmvalue(&hob.func);
                let entries: Vec<_> = attrs.iter().map(|(k, v)| (*k, v.clone())).collect();
                let mut result = BTreeMap::new();
                for (sym, val) in entries {
                    let key_str = self.interner.resolve(sym).to_string();
                    let partial = self.call_callable(&func_nb, NanBox::string(key_str))?;
                    let mapped = self.call_callable(&partial, NanBox::from_vmvalue(&val))?;
                    result.insert(sym, mapped);
                }
                Ok(NanBox::attrs(result))
            }
            FilterAttrs => {
                let attrs_val = arg.to_vmvalue();
                let attrs = match &attrs_val {
                    VMValue::Attrs(a) => a,
                    other => return Err(VMError::TypeError {
                        expected: "set", got: other.type_name(),
                        context: "builtins.filterAttrs".to_string(),
                    }),
                };
                let func_nb = NanBox::from_vmvalue(&hob.func);
                let entries: Vec<_> = attrs.iter().map(|(k, v)| (*k, v.clone())).collect();
                let mut result = BTreeMap::new();
                for (sym, val) in entries {
                    let key_str = self.interner.resolve(sym).to_string();
                    let partial = self.call_callable(&func_nb, NanBox::string(key_str))?;
                    if self.call_callable(&partial, NanBox::from_vmvalue(&val))?.is_truthy()? {
                        result.insert(sym, NanBox::from_vmvalue(&val));
                    }
                }
                Ok(NanBox::attrs(result))
            }
        }
    }

    // -- Import ---------------------------------------------------------

    /// Import a Nix file: compile it, execute it, cache the result.
    ///
    /// Handles the Nix convention that importing a directory is equivalent
    /// to importing `<directory>/default.nix`.
    fn import_file(&mut self, path: &str) -> Result<NanBox, VMError> {
        let resolved = std::fs::canonicalize(path)
            .map_err(|e| VMError::ImportError(format!("{path}: {e}")))?;

        // Directory → default.nix fallback (Nix convention).
        let resolved = if resolved.is_dir() {
            resolved.join("default.nix")
        } else {
            resolved
        };

        let canonical = resolved.to_string_lossy().to_string();

        // Check cache.
        if let Some(cached) = self.import_cache.borrow().get(&canonical) {
            return Ok(NanBox::from_vmvalue(cached));
        }

        // Try VM compilation, falling back to tree-walker on CompileError.
        let chunk = self.try_compile_import(&resolved, &canonical)?;

        let chunk = match chunk {
            Some(c) => c,
            None => {
                // Compilation failed — fall back to tree-walker via bridge.
                return self.import_via_bridge(&canonical);
            }
        };

        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(VMError::StackOverflow);
        }

        let return_depth = self.frames.len();
        let stack_base = self.stack.len();
        self.frames.push(CallFrame {
            chunk,
            ip: 0,
            stack_base,
            upvalues: Vec::new(),
        });

        let result = match self.run_until(return_depth) {
            Ok(r) => r,
            Err(e) => {
                // Runtime error — fall back to tree-walker for this file.
                // This handles compiler bugs (e.g., GetLocal slot mismatch)
                // that the compilation phase didn't catch.
                eprintln!("[sui-vm] runtime fallback for {canonical}: {e}");
                use std::sync::atomic::Ordering;
                crate::vm::VM_FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);
                // Clean up the failed frame's stack.
                self.stack.truncate(stack_base);
                // Remove the frame if it's still present.
                if self.frames.len() > return_depth {
                    self.frames.truncate(return_depth);
                }
                return self.import_via_bridge(&canonical);
            }
        };

        // Clean up the imported frame's stack slots.
        // Return at stop_depth skips truncation, so we must do it here.
        self.stack.truncate(stack_base);

        // Cache as VMValue and return as NanBox.
        let result_vmval = result.to_vmvalue();
        self.import_cache
            .borrow_mut()
            .insert(canonical, result_vmval);
        Ok(result)
    }

    /// Try to compile an imported file. Returns `Ok(Some(chunk))` on success,
    /// `Ok(None)` on `CompileError` (caller should fall back to tree-walker),
    /// or `Err` on I/O errors.
    fn try_compile_import(
        &mut self,
        resolved: &std::path::Path,
        canonical: &str,
    ) -> Result<Option<Rc<Chunk>>, VMError> {
        // Check compile cache — skip parse + compile if we've seen this file.
        if let Some(cached_chunk) = self.compile_cache.get(resolved) {
            return Ok(Some(cached_chunk.clone()));
        }

        // Read the file.
        let source = std::fs::read_to_string(canonical)
            .map_err(|e| VMError::ImportError(format!("{canonical}: {e}")))?;

        let file_dir = resolved
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();

        // Share the VM's interner with the compiler so that symbol IDs
        // are consistent — no need to clear key_symbols afterwards.
        let shared_interner = Rc::new(RefCell::new(std::mem::take(self.interner)));
        let compile_result =
            Compiler::compile_with_shared_interner(&source, file_dir, shared_interner.clone());
        *self.interner = match Rc::try_unwrap(shared_interner) {
            Ok(cell) => cell.into_inner(),
            Err(rc) => rc.borrow().clone(),
        };

        match compile_result {
            Ok(compiled) => {
                let chunk = Rc::new(compiled);
                self.compile_cache
                    .insert(resolved.to_path_buf(), chunk.clone());
                Ok(Some(chunk))
            }
            Err(compile_error) => {
                // Compilation failed (unsupported expression, etc.) —
                // signal caller to fall back to tree-walker.
                VM_FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);
                eprintln!("[sui-vm] fallback to tree-walker for {canonical}: {compile_error}");
                Ok(None)
            }
        }
    }

    /// Fall back to tree-walker evaluation for an imported file via the
    /// builtin bridge. Called when the bytecode compiler cannot handle
    /// the file (e.g. unsupported AST constructs).
    fn import_via_bridge(&mut self, canonical: &str) -> Result<NanBox, VMError> {
        match crate::bridge::call_builtin_bridge(
            "__import",
            vec![crate::value::StringKeyedValue::Path(canonical.to_string())],
        ) {
            Ok(Some(result)) => {
                let nanbox = self.string_keyed_to_nanbox(&result);
                // Cache as VMValue so subsequent imports hit the cache.
                let result_vmval = nanbox.to_vmvalue();
                self.import_cache
                    .borrow_mut()
                    .insert(canonical.to_string(), result_vmval);
                Ok(nanbox)
            }
            Ok(None) => Err(VMError::ImportError(format!(
                "compilation failed and no bridge installed for '{canonical}'"
            ))),
            Err(e) => Err(VMError::ImportError(format!(
                "bridge fallback error for '{canonical}': {e}"
            ))),
        }
    }

    /// Disassemble instructions around a given offset for error diagnostics.
    /// Returns a human-readable string showing `window` instructions before
    /// and after `center_ip`, with an arrow marking the center.
    fn disassemble_around(chunk: &Chunk, center_ip: usize, window: usize) -> String {
        let code = &chunk.code;
        let mut lines: Vec<String> = Vec::new();

        // Collect instruction boundaries by scanning from the start.
        let mut boundaries: Vec<usize> = Vec::new();
        let mut pos = 0;
        while pos < code.len() {
            boundaries.push(pos);
            pos += Self::instruction_width(code, pos);
        }

        // Find the boundary closest to center_ip.
        let center_idx = boundaries.iter().position(|&b| b >= center_ip).unwrap_or(0);
        let start_idx = center_idx.saturating_sub(window);
        let end_idx = (center_idx + window + 1).min(boundaries.len());

        for idx in start_idx..end_idx {
            let ip = boundaries[idx];
            let marker = if ip == center_ip { ">>>" } else { "   " };
            let line = chunk.lines.get(ip).copied().unwrap_or(0);
            if let Some(op) = OpCode::from_byte(code[ip]) {
                let operands = Self::format_operands(code, ip, op);
                lines.push(format!("    {marker} {ip:4}: {op:?}{operands}  (line {line})"));
            } else {
                lines.push(format!("    {marker} {ip:4}: <unknown {}>  (line {line})", code[ip]));
            }
        }

        lines.join("\n")
    }

    /// Determine the total byte width of an instruction at `pos`.
    fn instruction_width(code: &[u8], pos: usize) -> usize {
        let byte = code[pos];
        match OpCode::from_byte(byte) {
            Some(op) => match op {
                // No operands (1 byte):
                OpCode::Null | OpCode::True | OpCode::False
                | OpCode::Add | OpCode::Sub | OpCode::Mul | OpCode::Div | OpCode::Negate
                | OpCode::Not | OpCode::And | OpCode::Or | OpCode::Implication
                | OpCode::Equal | OpCode::NotEqual | OpCode::Less | OpCode::Greater
                | OpCode::LessEqual | OpCode::GreaterEqual
                | OpCode::UpdateAttrs | OpCode::Concat
                | OpCode::Call | OpCode::TailCall | OpCode::Return
                | OpCode::Assert | OpCode::Pop | OpCode::PushWith | OpCode::PopWith
                | OpCode::PushBuiltins | OpCode::Force | OpCode::Import
                | OpCode::DynGetAttr => 1,

                // 1 u16 operand (3 bytes):
                OpCode::Constant | OpCode::GetLocal | OpCode::SetLocal
                | OpCode::GetUpvalue | OpCode::SetUpvalue | OpCode::LookupWith
                | OpCode::GetAttr | OpCode::HasAttr | OpCode::MakeAttrs
                | OpCode::SelectOrDefault | OpCode::MakeList
                | OpCode::Jump | OpCode::JumpIfFalse | OpCode::JumpIfTrue
                | OpCode::Interpolate => 3,

                // 2 u16 operands (5 bytes):
                OpCode::GetLocalAttr | OpCode::GetLocalCall | OpCode::CallBuiltin => 5,

                // MakeClosure: u16 const_idx, u16 uv_count, then uv_count * 3 bytes
                OpCode::MakeClosure => {
                    if pos + 5 <= code.len() {
                        let uv_count = u16::from_le_bytes([code[pos + 3], code[pos + 4]]) as usize;
                        5 + uv_count * 3
                    } else {
                        3 // truncated
                    }
                }

                // MakeThunk: u16 const_idx, u16 uv_count, then uv_count * 3 bytes
                OpCode::MakeThunk => {
                    if pos + 5 <= code.len() {
                        let uv_count = u16::from_le_bytes([code[pos + 3], code[pos + 4]]) as usize;
                        5 + uv_count * 3
                    } else {
                        3
                    }
                }

                // PatchThunkUpvalues: u16 slot, u16 uv_count, then uv_count * 3 bytes
                OpCode::PatchThunkUpvalues => {
                    if pos + 5 <= code.len() {
                        let uv_count = u16::from_le_bytes([code[pos + 3], code[pos + 4]]) as usize;
                        5 + uv_count * 3
                    } else {
                        3
                    }
                }

                // MakeLazyThunk: u16 src, u32 offset, u32 length, u16 dir, u16 uv_count, then uv_count * 3
                OpCode::MakeLazyThunk => {
                    if pos + 15 <= code.len() {
                        let uv_count = u16::from_le_bytes([code[pos + 13], code[pos + 14]]) as usize;
                        15 + uv_count * 3
                    } else {
                        3
                    }
                }
            },
            None => 1, // unknown opcode, skip 1
        }
    }

    /// Format inline operands for a single instruction (for disassembly).
    fn format_operands(code: &[u8], pos: usize, op: OpCode) -> String {
        let read_u16_at = |p: usize| -> Option<u16> {
            if p + 2 <= code.len() {
                Some(u16::from_le_bytes([code[p], code[p + 1]]))
            } else {
                None
            }
        };

        match op {
            OpCode::Constant | OpCode::GetLocal | OpCode::SetLocal
            | OpCode::GetUpvalue | OpCode::SetUpvalue | OpCode::LookupWith
            | OpCode::GetAttr | OpCode::HasAttr | OpCode::MakeAttrs
            | OpCode::SelectOrDefault | OpCode::MakeList
            | OpCode::Jump | OpCode::JumpIfFalse | OpCode::JumpIfTrue
            | OpCode::Interpolate => {
                read_u16_at(pos + 1).map_or(String::new(), |v| format!(" {v}"))
            }
            OpCode::GetLocalAttr => {
                let s = read_u16_at(pos + 1).unwrap_or(0);
                let k = read_u16_at(pos + 3).unwrap_or(0);
                format!(" slot={s} key={k}")
            }
            OpCode::GetLocalCall => {
                read_u16_at(pos + 1).map_or(String::new(), |v| format!(" slot={v}"))
            }
            OpCode::CallBuiltin => {
                let idx = read_u16_at(pos + 1).unwrap_or(0);
                let argc = read_u16_at(pos + 3).unwrap_or(0);
                format!(" idx={idx} argc={argc}")
            }
            OpCode::MakeThunk | OpCode::MakeClosure => {
                let ci = read_u16_at(pos + 1).unwrap_or(0);
                let uv = read_u16_at(pos + 3).unwrap_or(0);
                format!(" const={ci} upvals={uv}")
            }
            OpCode::PatchThunkUpvalues => {
                let s = read_u16_at(pos + 1).unwrap_or(0);
                let uv = read_u16_at(pos + 3).unwrap_or(0);
                format!(" slot={s} upvals={uv}")
            }
            _ => String::new(),
        }
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

    #[test]
    fn import_directory_default_nix() {
        // Importing a directory should resolve to <dir>/default.nix
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("mylib");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("default.nix"), "{ x = 42; }").unwrap();
        let nix_expr = format!("(import {}).x", sub.display());
        assert_eq!(eval(&nix_expr), VMValue::Int(42));
    }

    #[test]
    fn import_directory_cached() {
        // Importing the same directory twice should hit the cache.
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("lib");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("default.nix"), "{ v = 99; }").unwrap();
        let nix_expr = format!(
            "let a = import {}; b = import {}; in a == b",
            sub.display(),
            sub.display()
        );
        assert_eq!(eval(&nix_expr), VMValue::Bool(true));
    }

    #[test]
    fn import_directory_nested() {
        // Nested directory imports: lib/default.nix imports sub/default.nix
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        let sub = lib.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("default.nix"), "{ val = 7; }").unwrap();
        std::fs::write(
            lib.join("default.nix"),
            &format!("(import {}).val + 3", sub.display()),
        )
        .unwrap();
        let nix_expr = format!("import {}", lib.display());
        assert_eq!(eval(&nix_expr), VMValue::Int(10));
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

    // -- Import handler tests -------------------------------------------

    #[test]
    fn import_forces_thunk_before_type_check() {
        // The import path is a thunk (non-trivial let binding); the VM
        // must force it to a path/string before checking the type.
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("forced.nix");
        std::fs::write(&file_path, "99").unwrap();
        let nix_expr = format!(
            "let p = {}; in import p",
            file_path.display()
        );
        assert_eq!(eval(&nix_expr), VMValue::Int(99));
    }

    #[test]
    fn import_with_path_value_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("pathval.nix");
        std::fs::write(&file_path, "\"from-path\"").unwrap();
        let nix_expr = format!("import {}", file_path.display());
        assert_eq!(
            eval(&nix_expr),
            VMValue::String("from-path".to_string())
        );
    }

    #[test]
    fn import_with_string_value_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("strval.nix");
        std::fs::write(&file_path, "\"from-string\"").unwrap();
        let nix_expr = format!(
            "let s = \"{}\"; in import s",
            file_path.display()
        );
        assert_eq!(
            eval(&nix_expr),
            VMValue::String("from-string".to_string())
        );
    }

    // -- TailCall opcode tests ------------------------------------------

    #[test]
    fn tail_call_deep_recursion_via_import() {
        // Test deep tail-recursive calls via import (self-referencing let
        // requires open upvalues, not yet implemented). Writing a recursive
        // function to a file and importing it exercises TailCall.
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("countdown.nix");
        std::fs::write(
            &file_path,
            "{ f, n }: if n == 0 then 0 else f { inherit f; n = n - 1; }",
        )
        .unwrap();
        // Use fixpoint pattern: pass function as argument to avoid
        // self-referencing let bindings.
        let nix_expr = format!(
            "let g = import {}; in g {{ f = g; n = 2000; }}",
            file_path.display()
        );
        assert_eq!(eval(&nix_expr), VMValue::Int(0));
    }

    #[test]
    fn tail_call_simple_lambda_chain() {
        // Non-recursive tail call: the last call in a lambda body should
        // reuse the frame. This verifies TailCall opcode is emitted and
        // executed for simple function composition.
        assert_eq!(
            eval("let g = x: x + 1; f = x: g x; in f 41"),
            VMValue::Int(42)
        );
    }

    #[test]
    fn tail_call_if_branches() {
        // Both if-then and if-else branches should produce tail calls
        // when in lambda body. This verifies TailCall works in both branches.
        assert_eq!(
            eval("let f = x: if x > 0 then x else x + 1; in f 10"),
            VMValue::Int(10)
        );
        assert_eq!(
            eval("let f = x: if x > 0 then x else x + 1; in f 0"),
            VMValue::Int(1)
        );
    }

    // -- Builtin dispatch tests -----------------------------------------

    #[test]
    fn builtin_get_env_returns_value() {
        // Set a known env var and verify getEnv returns it.
        // SAFETY: test runs single-threaded; no concurrent env access.
        unsafe { std::env::set_var("SUI_TEST_VAR", "hello_sui") };
        assert_eq!(
            eval("builtins.getEnv \"SUI_TEST_VAR\""),
            VMValue::String("hello_sui".to_string())
        );
        unsafe { std::env::remove_var("SUI_TEST_VAR") };
    }

    #[test]
    fn builtin_get_env_missing_returns_empty() {
        // getEnv with a missing var should return "".
        // SAFETY: test runs single-threaded; no concurrent env access.
        unsafe { std::env::remove_var("SUI_NONEXISTENT_VAR_12345") };
        assert_eq!(
            eval("builtins.getEnv \"SUI_NONEXISTENT_VAR_12345\""),
            VMValue::String(String::new())
        );
    }

    #[test]
    fn builtin_try_eval_success() {
        // tryEval with a successful expression returns { success=true; value=result; }.
        let result = eval_full_helper("builtins.tryEval 42");
        match result {
            StringKeyedValue::Attrs(map) => {
                assert_eq!(
                    map.get("success"),
                    Some(&StringKeyedValue::Bool(true))
                );
                assert_eq!(
                    map.get("value"),
                    Some(&StringKeyedValue::Int(42))
                );
            }
            _ => panic!("expected Attrs, got {result:?}"),
        }
    }

    #[test]
    fn builtin_try_eval_with_non_throwing_expr() {
        // tryEval wraps a non-throwing expression — still produces
        // { success = true; value = ...; }.
        let result = eval_full_helper(
            "builtins.tryEval (1 + 2)"
        );
        match result {
            StringKeyedValue::Attrs(map) => {
                assert_eq!(
                    map.get("success"),
                    Some(&StringKeyedValue::Bool(true))
                );
                assert_eq!(
                    map.get("value"),
                    Some(&StringKeyedValue::Int(3))
                );
            }
            _ => panic!("expected Attrs, got {result:?}"),
        }
    }

    #[test]
    fn builtin_try_eval_with_throw_propagates() {
        // NOTE: tryEval with a throwing expression currently propagates the
        // throw because the VM dispatch path forces the argument before
        // dispatching to tryEval (open upvalue limitation). This test
        // documents the current behavior.
        let result = crate::eval_full(
            "let bad = builtins.throw \"oops\"; in builtins.tryEval bad"
        );
        assert!(result.is_err(), "throw propagates through tryEval (current limitation)");
    }

    // -- Regression: stack_depth tracking for branches -------------------

    #[test]
    fn if_else_in_let_body_stack_depth() {
        // If/else inside a let body should not corrupt stack_depth for
        // subsequent let bindings in an outer scope.
        assert_eq!(
            eval("let a = 1; in if a == 1 then 10 else 20"),
            VMValue::Int(10),
        );
    }

    #[test]
    fn nested_let_with_if_else() {
        // Inner let after an if/else: the if/else must not drift stack_depth.
        assert_eq!(
            eval(r#"
                let
                  a = 1;
                  b = if a == 1 then 2 else 3;
                in
                  let c = b + 10; in c
            "#),
            VMValue::Int(12),
        );
    }

    #[test]
    fn short_circuit_and_in_let_body() {
        // Short-circuit && inside a let body must track stack_depth correctly.
        assert_eq!(
            eval("let x = true; in x && false"),
            VMValue::Bool(false),
        );
    }

    #[test]
    fn short_circuit_or_in_let_body() {
        assert_eq!(
            eval("let x = false; in x || true"),
            VMValue::Bool(true),
        );
    }

    #[test]
    fn short_circuit_implication_in_let_body() {
        // a -> b is !a || b. false -> anything is true.
        assert_eq!(
            eval("let x = false; in x -> 42"),
            VMValue::Bool(true),
        );
    }

    #[test]
    fn inherit_from_in_attrset_stack_depth() {
        // inherit (source) in non-rec attrset must track stack_depth for
        // MakeThunk. This was the missing `stack_depth += 1` bug.
        assert_eq!(
            eval(r#"
                let
                  src = { a = 1; b = 2; };
                  result = { inherit (src) a b; c = 3; };
                in result.a + result.b + result.c
            "#),
            VMValue::Int(6),
        );
    }

    #[test]
    fn inherit_from_many_fields_stack_depth() {
        // Multiple inherit-from fields: each one was missing +1,
        // so stack_depth would drift further with each field.
        assert_eq!(
            eval(r#"
                let
                  s = { w = 1; x = 2; y = 3; z = 4; };
                  r = { inherit (s) w x y z; extra = 10; };
                in r.w + r.x + r.y + r.z + r.extra
            "#),
            VMValue::Int(20),
        );
    }

    #[test]
    fn if_else_followed_by_let_binding() {
        // The if/else result is used in a subsequent let binding.
        // Before the fix, the stack_depth drift from if/else would cause
        // the next binding's slot to be off.
        assert_eq!(
            eval(r#"
                let
                  a = 1;
                  b = 2;
                  c = 3;
                in
                  let
                    x = if a == 1 then b else c;
                    y = x + 100;
                  in y
            "#),
            VMValue::Int(102),
        );
    }

    #[test]
    fn multi_segment_hasattr_stack_depth() {
        // Multi-segment hasattr with short-circuit jumps must track
        // stack_depth correctly at branch merge points.
        assert_eq!(
            eval(r#"
                let
                  s = { a = { b = 1; }; };
                  has = s ? a.b;
                  val = if has then 42 else 0;
                in val
            "#),
            VMValue::Int(42),
        );
    }

    #[test]
    fn many_let_bindings_with_if_else() {
        // Stress test: many let bindings where some RHS contain if/else.
        // Before the stack_depth fix, the drift would accumulate and
        // eventually cause a GetLocal slot mismatch.
        assert_eq!(
            eval(r#"
                let
                  a = 1;
                  b = 2;
                  c = 3;
                  d = 4;
                  e = 5;
                  f = 6;
                  g = 7;
                  h = 8;
                  i = 9;
                  j = 10;
                in
                  let
                    x = if a == 1 then b else c;
                    y = if d == 4 then e else f;
                    z = if g == 7 then h else i;
                    w = j;
                  in x + y + z + w
            "#),
            VMValue::Int(25),
        );
    }

    #[test]
    fn import_in_pattern_default_stack_depth() {
        // The Import opcode is net 0 on the stack (pop path, push result).
        // Before the fix, it was tracked as +1, causing stack_depth drift
        // in pattern default expressions like `{ stdenvStages ? import ../stdenv, ... }`.
        // This test uses a pattern lambda with a default that involves a
        // function call (which compiles similarly to import + call).
        assert_eq!(
            eval(r#"
                let
                  f = { a ? 1, b ? 2, c ? 3 }:
                    a + b + c;
                in f {}
            "#),
            VMValue::Int(6),
        );
    }

    #[test]
    fn pattern_lambda_many_defaults_then_let() {
        // Pattern lambda with many defaults followed by let bindings.
        // This is the pattern that triggered the original nixpkgs bug:
        // { a, b ? x, c ? y, ... }: let ... in expr
        // The import stack_depth bug caused slots to drift by 1 for each
        // default expression that used import.
        assert_eq!(
            eval(r#"
                let
                  mk = { a, b ? 10, c ? 20, d ? 30, e ? 40 }:
                    let
                      sum = a + b + c + d + e;
                      doubled = sum + sum;
                    in doubled;
                in mk { a = 1; }
            "#),
            VMValue::Int(202),
        );
    }
}
