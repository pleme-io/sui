//! AST-to-bytecode compiler.
//!
//! Walks the rnix typed AST and emits a [`Chunk`] of bytecode
//! instructions. The compiler manages local variable resolution via
//! a scope stack and emits appropriate `GetLocal`/`SetLocal` instructions.

use std::cell::RefCell;
use std::rc::Rc;

use rnix::ast::{self, AstToken, HasEntry, InterpolPart};
use rowan::ast::AstNode;

use crate::chunk::Chunk;
use crate::error::CompileError;
use crate::intern::Interner;
use crate::opcode::OpCode;
use crate::value::{VMClosure, VMValue};

/// A local variable in the current scope.
#[derive(Debug, Clone)]
struct Local {
    /// The variable name.
    name: String,
    /// Scope depth (0 = outermost).
    depth: u32,
    /// Whether this local has been captured as an upvalue by a nested function.
    is_captured: bool,
    /// The actual stack slot (relative to frame base) where this local lives.
    /// This may differ from the locals vector index when anonymous values
    /// are on the stack between locals (e.g., partial application results
    /// between a function parameter and let-binding locals).
    slot: u16,
}

/// An upvalue descriptor: tells a closure how to capture a variable.
#[derive(Debug, Clone, Copy)]
struct UpvalueDesc {
    /// If true, the upvalue captures a local from the immediately enclosing compiler.
    /// If false, it captures an upvalue from the enclosing compiler's upvalue list.
    is_local: bool,
    /// The index: either a local slot (if `is_local`) or an upvalue index.
    index: u16,
}

/// The bytecode compiler.
///
/// Compiles a single expression (which may contain nested lambdas)
/// into a top-level [`Chunk`]. Nested lambdas produce sub-chunks
/// stored in the constant pool.
///
/// The compiler maintains a shared [`Interner`] that is also passed
/// to the VM for attribute key resolution.
pub struct Compiler {
    /// The chunk being compiled into.
    chunk: Chunk,
    /// Local variable stack (simulates the runtime value stack layout).
    locals: Vec<Local>,
    /// Upvalue descriptors for this compiler (function scope).
    upvalues: Vec<UpvalueDesc>,
    /// Current scope depth.
    scope_depth: u32,
    /// Current source line for error reporting.
    current_line: u32,
    /// Shared string interner for attribute names and identifiers.
    interner: Rc<RefCell<Interner>>,
    /// Reference to the enclosing (parent) compiler, for upvalue resolution.
    enclosing: Option<*mut Compiler>,
    /// Whether this compiler has any `with` scopes active (used for variable resolution).
    with_depth: u32,
    /// Base directory for resolving relative paths (set when compiling imported files).
    base_dir: Option<std::path::PathBuf>,
    /// Tracks the current stack depth relative to frame base.
    /// Incremented on push/emit operations, decremented on pop.
    /// Used to assign correct stack slots to local variables when
    /// anonymous values (partial application results, etc.) sit on the
    /// stack between named locals.
    stack_depth: u16,
    /// Shared source text for lazy thunk compilation.
    /// When set, thunks can store source spans instead of eagerly compiling.
    source_text: Option<Rc<String>>,
    /// Whether the current expression is in tail position (eligible for
    /// tail-call optimization). Set to `true` in lambda bodies, if-else
    /// branches, and assert bodies. `compile_apply` checks this to emit
    /// `TailCall` instead of `Call`.
    tail_position: bool,
    /// Stack slots of with-scope values stored as hidden locals.
    /// When inside `with ns; body`, the namespace is Dup'd and stored as
    /// a hidden local so thunks compiled inside the body can capture it as
    /// an upvalue. At thunk force time, the thunk body emits
    /// `GetUpvalue + PushWith` to restore the with-scope context.
    with_scope_locals: Vec<u16>,
}

impl Compiler {
    /// Create a new compiler with a fresh interner.
    fn new() -> Self {
        Self {
            chunk: Chunk::new(),
            locals: Vec::new(),
            upvalues: Vec::new(),
            scope_depth: 0,
            current_line: 0,
            interner: Rc::new(RefCell::new(Interner::new())),
            enclosing: None,
            with_depth: 0,
            base_dir: None,
            stack_depth: 0,
            source_text: None,
            tail_position: false,
            with_scope_locals: Vec::new(),
        }
    }

    /// Create a new compiler sharing an existing interner.
    fn with_interner(interner: Rc<RefCell<Interner>>) -> Self {
        Self {
            chunk: Chunk::new(),
            locals: Vec::new(),
            upvalues: Vec::new(),
            scope_depth: 0,
            current_line: 0,
            interner,
            enclosing: None,
            with_depth: 0,
            base_dir: None,
            stack_depth: 0,
            source_text: None,
            tail_position: false,
            with_scope_locals: Vec::new(),
        }
    }

    /// Compile a Nix expression string into bytecode and an interner,
    /// resolving relative paths against the given base directory.
    pub fn compile_with_base_dir(
        input: &str,
        base_dir: std::path::PathBuf,
    ) -> Result<(Chunk, Interner), CompileError> {
        let parse = rnix::Root::parse(input);
        if !parse.errors().is_empty() {
            let msgs: Vec<String> = parse.errors().iter().map(|e| e.to_string()).collect();
            return Err(CompileError::ParseError(msgs.join("; ")));
        }
        let root = parse.tree();
        let expr = root
            .expr()
            .ok_or_else(|| CompileError::ParseError("empty expression".to_string()))?;
        let mut compiler = Self::new();
        compiler.base_dir = Some(base_dir);
        compiler.compile_expr(&expr)?;
        compiler.emit(OpCode::Return);
        let interner = match Rc::try_unwrap(compiler.interner) {
            Ok(cell) => cell.into_inner(),
            Err(rc) => (*rc).borrow().clone(),
        };
        Ok((compiler.chunk, interner))
    }

    /// Compile using a shared interner and base directory.
    /// Used when importing files from within the VM so that symbol IDs
    /// are consistent with the VM's interner.
    pub fn compile_with_shared_interner(
        input: &str,
        base_dir: std::path::PathBuf,
        interner: Rc<RefCell<Interner>>,
    ) -> Result<Chunk, CompileError> {
        let parse = rnix::Root::parse(input);
        if !parse.errors().is_empty() {
            let msgs: Vec<String> = parse.errors().iter().map(|e| e.to_string()).collect();
            return Err(CompileError::ParseError(msgs.join("; ")));
        }
        let root = parse.tree();
        let expr = root
            .expr()
            .ok_or_else(|| CompileError::ParseError("empty expression".to_string()))?;
        let mut compiler = Self::with_interner(interner);
        compiler.base_dir = Some(base_dir);
        compiler.source_text = Some(Rc::new(input.to_string()));
        compiler.compile_expr(&expr)?;
        compiler.emit(OpCode::Return);
        Ok(compiler.chunk)
    }

    /// Compile a standalone expression string (used for lazy thunk compilation).
    /// The expression is parsed and compiled fresh with the given interner and base directory.
    pub fn compile_expression(
        input: &str,
        base_dir: &std::path::Path,
        interner: Rc<RefCell<Interner>>,
    ) -> Result<Chunk, CompileError> {
        let parse = rnix::Root::parse(input);
        if !parse.errors().is_empty() {
            let msgs: Vec<String> = parse.errors().iter().map(|e| e.to_string()).collect();
            return Err(CompileError::ParseError(msgs.join("; ")));
        }
        let root = parse.tree();
        let expr = root
            .expr()
            .ok_or_else(|| CompileError::ParseError("empty expression".to_string()))?;
        let mut compiler = Self::with_interner(interner);
        compiler.base_dir = Some(base_dir.to_path_buf());
        compiler.compile_expr(&expr)?;
        compiler.emit(OpCode::Return);
        Ok(compiler.chunk)
    }

    /// Compile a Nix expression string into bytecode and an interner.
    pub fn compile(input: &str) -> Result<(Chunk, Interner), CompileError> {
        let parse = rnix::Root::parse(input);
        if !parse.errors().is_empty() {
            let msgs: Vec<String> = parse.errors().iter().map(|e| e.to_string()).collect();
            return Err(CompileError::ParseError(msgs.join("; ")));
        }
        let root = parse.tree();
        let expr = root
            .expr()
            .ok_or_else(|| CompileError::ParseError("empty expression".to_string()))?;
        let mut compiler = Self::new();
        compiler.compile_expr(&expr)?;
        compiler.emit(OpCode::Return);
        let interner = match Rc::try_unwrap(compiler.interner) {
            Ok(cell) => cell.into_inner(),
            Err(rc) => (*rc).borrow().clone(),
        };
        Ok((compiler.chunk, interner))
    }

    // ── Constant folding ────────────────────────────────────────

    /// Try to evaluate an expression as a compile-time constant.
    /// Returns `Some(VMValue)` if the expression can be fully evaluated
    /// at compile time, `None` otherwise.
    fn try_eval_const(expr: &ast::Expr) -> Option<VMValue> {
        match expr {
            ast::Expr::Literal(lit) => Self::try_eval_literal(lit),
            ast::Expr::Paren(p) => Self::try_eval_const(&p.expr()?),
            ast::Expr::UnaryOp(op) => Self::try_fold_unary(op),
            ast::Expr::BinOp(binop) => Self::try_fold_binop(binop),
            ast::Expr::IfElse(ie) => Self::try_fold_if(ie),
            ast::Expr::Ident(id) => {
                let name = ident_text(id);
                match name.as_str() {
                    "true" => Some(VMValue::Bool(true)),
                    "false" => Some(VMValue::Bool(false)),
                    "null" => Some(VMValue::Null),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Try to evaluate a literal as a constant.
    fn try_eval_literal(lit: &ast::Literal) -> Option<VMValue> {
        match lit.kind() {
            ast::LiteralKind::Integer(tok) => {
                Some(VMValue::Int(tok.value().ok()?))
            }
            ast::LiteralKind::Float(tok) => {
                Some(VMValue::Float(tok.value().ok()?))
            }
            ast::LiteralKind::Uri(_) => None,
        }
    }

    /// Try to fold a unary operation on constants.
    fn try_fold_unary(op: &ast::UnaryOp) -> Option<VMValue> {
        let inner = Self::try_eval_const(&op.expr()?)?;
        let kind = op.operator()?;
        match kind {
            ast::UnaryOpKind::Negate => match inner {
                VMValue::Int(n) => Some(VMValue::Int(-n)),
                VMValue::Float(f) => Some(VMValue::Float(-f)),
                _ => None,
            },
            ast::UnaryOpKind::Invert => match inner {
                VMValue::Bool(b) => Some(VMValue::Bool(!b)),
                _ => None,
            },
        }
    }

    /// Try to fold a binary operation where both sides are constants.
    fn try_fold_binop(binop: &ast::BinOp) -> Option<VMValue> {
        let lhs = Self::try_eval_const(&binop.lhs()?)?;
        let rhs = Self::try_eval_const(&binop.rhs()?)?;
        let op = binop.operator()?;

        match op {
            ast::BinOpKind::Add => match (&lhs, &rhs) {
                (VMValue::Int(a), VMValue::Int(b)) => Some(VMValue::Int(a + b)),
                (VMValue::Float(a), VMValue::Float(b)) => Some(VMValue::Float(a + b)),
                (VMValue::Int(a), VMValue::Float(b)) => Some(VMValue::Float(*a as f64 + b)),
                (VMValue::Float(a), VMValue::Int(b)) => Some(VMValue::Float(a + *b as f64)),
                (VMValue::String(a), VMValue::String(b)) => {
                    Some(VMValue::String(format!("{a}{b}")))
                }
                _ => None,
            },
            ast::BinOpKind::Sub => match (&lhs, &rhs) {
                (VMValue::Int(a), VMValue::Int(b)) => Some(VMValue::Int(a - b)),
                (VMValue::Float(a), VMValue::Float(b)) => Some(VMValue::Float(a - b)),
                (VMValue::Int(a), VMValue::Float(b)) => Some(VMValue::Float(*a as f64 - b)),
                (VMValue::Float(a), VMValue::Int(b)) => Some(VMValue::Float(a - *b as f64)),
                _ => None,
            },
            ast::BinOpKind::Mul => match (&lhs, &rhs) {
                (VMValue::Int(a), VMValue::Int(b)) => Some(VMValue::Int(a * b)),
                (VMValue::Float(a), VMValue::Float(b)) => Some(VMValue::Float(a * b)),
                (VMValue::Int(a), VMValue::Float(b)) => Some(VMValue::Float(*a as f64 * b)),
                (VMValue::Float(a), VMValue::Int(b)) => Some(VMValue::Float(a * *b as f64)),
                _ => None,
            },
            ast::BinOpKind::Div => match (&lhs, &rhs) {
                (VMValue::Int(_), VMValue::Int(0)) => None, // don't fold div by zero
                (VMValue::Int(a), VMValue::Int(b)) => Some(VMValue::Int(a / b)),
                (VMValue::Float(a), VMValue::Float(b)) => Some(VMValue::Float(a / b)),
                (VMValue::Int(a), VMValue::Float(b)) => Some(VMValue::Float(*a as f64 / b)),
                (VMValue::Float(a), VMValue::Int(b)) => Some(VMValue::Float(a / *b as f64)),
                _ => None,
            },
            ast::BinOpKind::Equal => Some(VMValue::Bool(Self::const_eq(&lhs, &rhs))),
            ast::BinOpKind::NotEqual => Some(VMValue::Bool(!Self::const_eq(&lhs, &rhs))),
            ast::BinOpKind::Less => Self::const_cmp(&lhs, &rhs)
                .map(|o| VMValue::Bool(o == std::cmp::Ordering::Less)),
            ast::BinOpKind::LessOrEq => Self::const_cmp(&lhs, &rhs)
                .map(|o| VMValue::Bool(o != std::cmp::Ordering::Greater)),
            ast::BinOpKind::More => Self::const_cmp(&lhs, &rhs)
                .map(|o| VMValue::Bool(o == std::cmp::Ordering::Greater)),
            ast::BinOpKind::MoreOrEq => Self::const_cmp(&lhs, &rhs)
                .map(|o| VMValue::Bool(o != std::cmp::Ordering::Less)),
            ast::BinOpKind::And => match (&lhs, &rhs) {
                (VMValue::Bool(a), VMValue::Bool(b)) => Some(VMValue::Bool(*a && *b)),
                _ => None,
            },
            ast::BinOpKind::Or => match (&lhs, &rhs) {
                (VMValue::Bool(a), VMValue::Bool(b)) => Some(VMValue::Bool(*a || *b)),
                _ => None,
            },
            ast::BinOpKind::Implication => match (&lhs, &rhs) {
                (VMValue::Bool(a), VMValue::Bool(b)) => Some(VMValue::Bool(!a || *b)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Try to fold `if cond then a else b` when the condition is constant.
    fn try_fold_if(ie: &ast::IfElse) -> Option<VMValue> {
        let cond = Self::try_eval_const(&ie.condition()?)?;
        match cond {
            VMValue::Bool(true) => Self::try_eval_const(&ie.body()?),
            VMValue::Bool(false) => Self::try_eval_const(&ie.else_body()?),
            _ => None,
        }
    }

    /// Compile-time equality check.
    fn const_eq(a: &VMValue, b: &VMValue) -> bool {
        match (a, b) {
            (VMValue::Null, VMValue::Null) => true,
            (VMValue::Bool(a), VMValue::Bool(b)) => a == b,
            (VMValue::Int(a), VMValue::Int(b)) => a == b,
            (VMValue::Float(a), VMValue::Float(b)) => a == b,
            (VMValue::Int(a), VMValue::Float(b)) | (VMValue::Float(b), VMValue::Int(a)) => {
                (*a as f64) == *b
            }
            (VMValue::String(a), VMValue::String(b)) => a == b,
            _ => false,
        }
    }

    /// Compile-time comparison.
    fn const_cmp(a: &VMValue, b: &VMValue) -> Option<std::cmp::Ordering> {
        match (a, b) {
            (VMValue::Int(a), VMValue::Int(b)) => Some(a.cmp(b)),
            (VMValue::Float(a), VMValue::Float(b)) => a.partial_cmp(b),
            (VMValue::Int(a), VMValue::Float(b)) => (*a as f64).partial_cmp(b),
            (VMValue::Float(a), VMValue::Int(b)) => a.partial_cmp(&(*b as f64)),
            (VMValue::String(a), VMValue::String(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    // ── Expression dispatch ────────────────────────────────────

    fn compile_expr(&mut self, expr: &ast::Expr) -> Result<(), CompileError> {
        self.current_line = line_of(expr);

        // Try constant folding first — if the expression can be fully
        // evaluated at compile time, emit a single Constant instruction.
        if let Some(folded) = Self::try_eval_const(expr) {
            return self.emit_constant(folded);
        }

        // Save and clear tail_position. Specific branches that propagate
        // tail position (IfElse, Assert, Paren, Root, Apply) will restore
        // it themselves. All other branches compile subexpressions with
        // tail_position = false, which is the correct default.
        let tail = self.tail_position;
        self.tail_position = false;

        match expr {
            ast::Expr::Literal(lit) => self.compile_literal(lit),
            ast::Expr::Str(s) => self.compile_str(s),
            ast::Expr::Ident(id) => self.compile_ident(id),
            ast::Expr::LetIn(letin) => self.compile_let(letin),
            ast::Expr::AttrSet(set) => self.compile_attrset(set),
            ast::Expr::Select(sel) => self.compile_select(sel),
            ast::Expr::HasAttr(ha) => self.compile_has_attr(ha),
            ast::Expr::IfElse(ie) => {
                self.tail_position = tail;
                self.compile_if(ie)
            }
            ast::Expr::Lambda(lam) => self.compile_lambda(lam),
            ast::Expr::Apply(app) => {
                self.tail_position = tail;
                self.compile_apply(app)
            }
            ast::Expr::BinOp(op) => self.compile_binop(op),
            ast::Expr::UnaryOp(op) => self.compile_unary(op),
            ast::Expr::With(w) => self.compile_with(w),
            ast::Expr::Assert(a) => {
                self.tail_position = tail;
                self.compile_assert(a)
            }
            ast::Expr::List(l) => self.compile_list(l),
            ast::Expr::Paren(p) => {
                self.tail_position = tail;
                let inner = p
                    .expr()
                    .ok_or_else(|| CompileError::MissingNode("paren expr".to_string()))?;
                self.compile_expr(&inner)
            }
            ast::Expr::Root(r) => {
                self.tail_position = tail;
                let inner = r
                    .expr()
                    .ok_or_else(|| CompileError::MissingNode("root expr".to_string()))?;
                self.compile_expr(&inner)
            }
            ast::Expr::PathAbs(p) => {
                let text = p.syntax().text().to_string();
                self.emit_constant(VMValue::Path(text))
            }
            ast::Expr::PathRel(p) => {
                let text = p.syntax().text().to_string();
                // Resolve relative paths against base_dir when available,
                // or propagate from enclosing compiler.
                let resolved = self.resolve_relative_path(&text);
                self.emit_constant(VMValue::Path(resolved))
            }
            ast::Expr::PathHome(p) => {
                let text = p.syntax().text().to_string();
                self.emit_constant(VMValue::Path(text))
            }
            ast::Expr::PathSearch(p) => {
                let text = p.syntax().text().to_string();
                let inner = text
                    .strip_prefix('<')
                    .and_then(|s| s.strip_suffix('>'))
                    .unwrap_or(&text);
                if let Some(resolved) = resolve_search_path(inner) {
                    self.emit_constant(VMValue::Path(resolved))
                } else {
                    // Wrap the throw in a THUNK so it only fires when forced.
                    // This matches CppNix: unresolvable search paths are deferred
                    // and caught by tryEval at force-time, not at eval-time.
                    let msg = format!("search path '{text}' not in NIX_PATH");
                    let mut tc = Compiler::with_interner(Rc::clone(&self.interner));
                    tc.scope_depth = 1;
                    tc.base_dir = self.base_dir.clone();
                    tc.emit_constant(VMValue::String(msg))?;
                    tc.emit(OpCode::Throw);
                    tc.emit(OpCode::Return);
                    let closure = VMValue::Closure(VMClosure {
                        chunk: Rc::new(tc.chunk),
                        upvalues: Vec::new(),
                        arity: 0,
                        name: None,
                        formals: Vec::new(),
                    });
                    let idx = self.chunk.add_constant(closure)?;
                    self.emit(OpCode::MakeThunk);
                    self.stack_depth += 1;
                    self.emit_u16(idx);
                    self.emit_u16(0); // 0 upvalues
                    Ok(())
                }
            }
            ast::Expr::LegacyLet(ll) => {
                // Legacy let is like: let { x = 1; body = x; }
                // which is equivalent to: rec { x = 1; body = x; }.body
                // Compile as a recursive attrset, then select "body"
                self.compile_legacy_let(&ll)
            }
            ast::Expr::CurPos(_) => {
                // __curPos is a debug feature; emit null to avoid CompileError.
                self.emit_constant(VMValue::Null)
            }
            other => Err(CompileError::Unsupported(format!("{other:?}"))),
        }
    }

    // ── Literals ───────────────────────────────────────────────

    fn compile_literal(&mut self, lit: &ast::Literal) -> Result<(), CompileError> {
        match lit.kind() {
            ast::LiteralKind::Integer(tok) => {
                let n = tok.value().map_err(|e| {
                    CompileError::ParseError(format!("invalid integer: {e}"))
                })?;
                self.emit_constant(VMValue::Int(n))
            }
            ast::LiteralKind::Float(tok) => {
                let f = tok.value().map_err(|e| {
                    CompileError::ParseError(format!("invalid float: {e}"))
                })?;
                self.emit_constant(VMValue::Float(f))
            }
            ast::LiteralKind::Uri(tok) => {
                let s = tok.syntax().text().to_string();
                self.emit_constant(VMValue::String(s))
            }
        }
    }

    // ── Strings ────────────────────────────────────────────────

    fn compile_str(&mut self, s: &ast::Str) -> Result<(), CompileError> {
        let parts: Vec<_> = s.normalized_parts().into_iter().collect();

        // Optimize: single literal part (no interpolation) becomes a constant.
        if parts.len() == 1 {
            if let InterpolPart::Literal(text) = &parts[0] {
                return self.emit_constant(VMValue::String(String::from(text.as_str())));
            }
        }

        // General case: compile each part, then Interpolate.
        let mut count: u16 = 0;
        for part in &parts {
            match part {
                InterpolPart::Literal(text) => {
                    self.emit_constant(VMValue::String(text.to_string()))?;
                    count += 1;
                }
                InterpolPart::Interpolation(interp) => {
                    let expr = interp
                        .expr()
                        .ok_or_else(|| CompileError::MissingNode("interpolation expr".to_string()))?;
                    self.compile_expr(&expr)?;
                    count += 1;
                }
            }
        }

        if count == 0 {
            // Empty string.
            self.emit_constant(VMValue::String(String::new()))
        } else if count == 1 {
            // Already on stack from the single part above.
            Ok(())
        } else {
            self.emit(OpCode::Interpolate);
            self.emit_u16(count);
            // Interpolate pops count parts, pushes 1 string.
            self.stack_depth = self.stack_depth.saturating_sub(count) + 1;
            Ok(())
        }
    }

    // ── Identifiers (variable lookup) ──────────────────────────

    fn compile_ident(&mut self, ident: &ast::Ident) -> Result<(), CompileError> {
        let name = ident_text(ident);
        match name.as_str() {
            "true" => {
                self.emit(OpCode::True);
                Ok(())
            }
            "false" => {
                self.emit(OpCode::False);
                Ok(())
            }
            "null" => {
                self.emit(OpCode::Null);
                Ok(())
            }
            _ => {
                // 1. Look up in locals.
                if let Some(idx) = self.resolve_local(&name) {
                    self.emit(OpCode::GetLocal);
                    self.emit_u16(self.local_stack_slot(idx));
                    return Ok(());
                }
                // 2. Look up in upvalues (captures from enclosing scopes).
                if let Some(idx) = self.resolve_upvalue(&name) {
                    self.emit(OpCode::GetUpvalue);
                    self.emit_u16(idx as u16);
                    return Ok(());
                }
                // 3. `builtins` is a global — push the builtins attrset.
                if name == "builtins" {
                    self.emit(OpCode::PushBuiltins);
                    return Ok(());
                }
                // 4. Global builtins available without `builtins.` prefix.
                //    In Nix, these are automatically in scope.
                if is_global_builtin(&name) {
                    self.emit(OpCode::PushBuiltins);
                    let key_idx = self.add_attr_key(name)?;
                    self.emit(OpCode::GetAttr);
                    self.emit_u16(key_idx);
                    return Ok(());
                }
                // 5. Look up in with-scope (dynamic scope).
                if self.has_with_scope() {
                    let name_idx = self.chunk.add_constant(VMValue::String(name))?;
                    self.emit(OpCode::LookupWith);
                    self.emit_u16(name_idx);
                    return Ok(());
                }
                Err(CompileError::Unsupported(format!(
                    "unresolved variable: {name}"
                )))
            }
        }
    }

    // ── Let/in ─────────────────────────────────────────────────

    fn compile_let(&mut self, letin: &ast::LetIn) -> Result<(), CompileError> {
        // ★ A `let` is a RECURSIVE binder, so the plan is built with
        // `recursive = true`. That is what gives `let` dotted bindings and
        // duplicate-key merging, both of which the hand-rolled loop this
        // replaces either lost or refused:
        //
        //   let a = {b=1;}; a = {c=2;}; in a    was {"c":2}   nix {"b":1,"c":2}
        //   let a = {b=1;}; a.c = 2;    in a    was a hard `Unsupported("dotted
        //                                       let bindings")` refusal
        let plan = sui_normalize::plan_for_group_total(letin, true).map_err(reject)?;

        // A dynamic key cannot name a local slot, and nix rejects
        // `let ${k} = 1; in k` at parse time for exactly that reason. Refused,
        // never dropped.
        if !plan.dynamics.is_empty() {
            return Err(CompileError::Unsupported(
                "dynamic attribute in a let binding".to_string(),
            ));
        }

        let body = letin
            .body()
            .ok_or_else(|| CompileError::MissingNode("let body".to_string()))?;
        self.begin_scope();
        let local_count = self.bind_plan_group_locals(&plan)?;
        // The body's result lands on top of the local slots.
        self.compile_expr(&body)?;
        self.end_scope(local_count);
        Ok(())
    }

    /// Check if an expression is trivial (compile eagerly, no thunk needed).
    fn is_trivial_value(expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::Literal(_) => true,
            ast::Expr::Str(s) => {
                for part in s.normalized_parts() {
                    if !matches!(part, InterpolPart::Literal(_)) {
                        return false;
                    }
                }
                true
            }
            ast::Expr::Ident(id) => {
                let name = ident_text(id);
                matches!(name.as_str(), "true" | "false" | "null")
            }
            ast::Expr::Lambda(_) => true,
            ast::Expr::Paren(p) => p.expr().map_or(false, |inner| Self::is_trivial_value(&inner)),
            ast::Expr::List(list) => list.items().next().is_none(),
            ast::Expr::AttrSet(set) => set.rec_token().is_none() && set.entries().next().is_none(),
            _ => false,
        }
    }

    /// Like `is_trivial_value`, but for use in rec attrsets.
    /// Lambdas are NOT trivial in rec context because `MakeClosure` captures
    /// upvalues at emission time.  If a lambda captures a sibling binding
    /// (especially a dotted entry appended after non-dotted bindings), the
    /// sibling's slot may still hold the null placeholder, producing a silent
    /// wrong result.  Wrapping the lambda in a deferred thunk postpones
    /// `MakeClosure` until the value is accessed, by which time all siblings
    /// have been populated via `PatchThunkUpvalues`.
    fn is_trivial_value_for_rec(expr: &ast::Expr) -> bool {
        match expr {
            // Lambdas can capture rec-scoped variables — never inline in rec.
            ast::Expr::Lambda(_) => false,
            ast::Expr::Paren(p) => p.expr().map_or(false, |inner| Self::is_trivial_value_for_rec(&inner)),
            _ => Self::is_trivial_value(expr),
        }
    }

    /// Compile a thunk with 0 upvalues (deferred patching via PatchThunkUpvalues).
    /// The shared body of every deferred thunk: a child compiler parented to
    /// this one, the `with`-scope preamble, `body`, the matching `PopWith`s, a
    /// `Return`, and the `MakeThunk` whose upvalue count the caller patches
    /// later via `PatchThunkUpvalues`.
    ///
    /// ★ Extracted because this preamble/epilogue was written out THREE times
    /// verbatim (expression, `inherit (src) name`, nested attrset) and the plan
    /// path needs a fourth. Three identical copies of an eight-line protocol
    /// where the only difference is one middle line is a helper that was not
    /// written; a fourth copy would be the point at which a `with`-scope fix
    /// starts landing in three places out of four.
    ///
    /// `body` receives the CHILD compiler. Nothing it needs comes from `self`,
    /// which is what makes the closure form work at all — the parent is
    /// reachable from the child only through the raw `enclosing` pointer, and
    /// that is set up here.
    fn compile_deferred_thunk<F>(&mut self, body: F) -> Result<Vec<UpvalueDesc>, CompileError>
    where
        F: FnOnce(&mut Compiler) -> Result<(), CompileError>,
    {
        let mut tc = Compiler::with_interner(Rc::clone(&self.interner));
        tc.scope_depth = 1;
        tc.enclosing = Some(self as *mut Compiler);
        tc.with_depth = 0;
        tc.base_dir = self.base_dir.clone();
        let with_count = self.emit_with_scope_preamble(&mut tc);
        body(&mut tc)?;
        for _ in 0..with_count {
            tc.emit(OpCode::PopWith);
        }
        tc.emit(OpCode::Return);
        let uv_descs: Vec<UpvalueDesc> = tc.upvalues.clone();
        let closure = VMValue::Closure(VMClosure {
            chunk: Rc::new(tc.chunk),
            upvalues: Vec::new(),
            arity: 0,
            name: None,
            formals: Vec::new(),
        });
        let idx = self.chunk.add_constant(closure)?;
        self.emit(OpCode::MakeThunk);
        self.stack_depth += 1; // MakeThunk pushes one thunk
        self.emit_u16(idx);
        self.emit_u16(0); // 0 upvalues, patched later
        Ok(uv_descs)
    }

    fn compile_thunk_deferred(&mut self, expr: &ast::Expr) -> Result<Vec<UpvalueDesc>, CompileError> {
        self.compile_deferred_thunk(|tc| tc.compile_expr(expr))
    }

    /// Compile a function argument with call-by-need semantics.
    fn compile_arg_maybe_thunk(&mut self, arg: &ast::Expr) -> Result<(), CompileError> {
        if Self::is_trivial_arg(arg) {
            self.compile_expr(arg)
        } else {
            self.compile_thunk_immediate(arg)
        }
    }

    fn is_trivial_arg(expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::Literal(_) | ast::Expr::Ident(_)
            | ast::Expr::PathAbs(_) | ast::Expr::PathRel(_)
            | ast::Expr::PathHome(_) | ast::Expr::Lambda(_) => true,
            // Paren: check inner expression
            ast::Expr::Paren(p) => p.expr().map_or(false, |inner| Self::is_trivial_arg(&inner)),
            // Str without interpolation is trivial
            ast::Expr::Str(s) => s.normalized_parts().iter().all(|p| matches!(p, InterpolPart::Literal(_))),
            _ => false,
        }
    }

    /// Compile a deferred thunk for `inherit (source) name;` in let bindings.
    /// Like `compile_thunk_deferred`, but emits source + GetAttr(name) + Return.
    fn compile_inherit_from_thunk_deferred(
        &mut self,
        source_expr: &ast::Expr,
        attr_name: &str,
    ) -> Result<Vec<UpvalueDesc>, CompileError> {
        self.compile_deferred_thunk(|tc| {
            tc.compile_expr(source_expr)?;
            let key_idx = tc.add_attr_key(attr_name.to_string())?;
            tc.emit(OpCode::GetAttr);
            tc.emit_u16(key_idx);
            Ok(())
        })
    }

    /// Emit with-scope preamble in a child compiler: for each with-scope
    /// local in the parent, capture it as an upvalue and emit
    /// `GetUpvalue + PushWith` at the start of the thunk body.
    /// Returns the count of with-scopes pushed (caller must emit PopWith for each).
    fn emit_with_scope_preamble(&mut self, tc: &mut Compiler) -> usize {
        let slots: Vec<u16> = self.with_scope_locals.clone();
        for &slot in &slots {
            // Find the local index for this slot in the parent.
            let local_idx = self.locals.iter().rposition(|l| l.slot == slot);
            if let Some(idx) = local_idx {
                self.locals[idx].is_captured = true;
                if let Ok(uv_idx) = tc.add_upvalue(true, slot) {
                    tc.emit(OpCode::GetUpvalue);
                    tc.emit_u16(uv_idx as u16);
                    tc.emit(OpCode::PushWith);
                    tc.with_depth += 1;
                }
            }
        }
        slots.len()
    }

    /// Compile a thunk with upvalues captured immediately (for non-rec attrsets).
    ///
    /// When the compiler has source text available and the expression has no
    /// free variables (no locals, no upvalues, no with-scopes), emit a
    /// `MakeLazyThunk` that defers compilation until the thunk is forced.
    /// Otherwise, fall through to the eager compilation path.
    fn compile_thunk_immediate(&mut self, expr: &ast::Expr) -> Result<(), CompileError> {
        // Try lazy thunk: only when source text is available and there are
        // no variables in scope that the expression could reference.
        if let Some(ref source) = self.source_text {
            if self.locals.is_empty() && self.with_depth == 0 && self.upvalues.is_empty() {
                let range = AstNode::syntax(expr).text_range();
                let offset: usize = range.start().into();
                let length: usize = range.len().into();
                let base_dir_str = self.base_dir
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();

                // Store source text and base_dir in the constant pool.
                let src_idx = self.chunk.add_constant(VMValue::String((**source).clone()))?;
                let dir_idx = self.chunk.add_constant(VMValue::String(base_dir_str))?;

                self.emit(OpCode::MakeLazyThunk);
                self.stack_depth += 1;
                self.emit_u16(src_idx);
                self.chunk.write_u32(offset as u32, self.current_line);
                self.chunk.write_u32(length as u32, self.current_line);
                self.emit_u16(dir_idx);
                self.emit_u16(0); // 0 upvalues
                return Ok(());
            }
        }

        // Eager path: compile the thunk body now.
        let mut tc = Compiler::with_interner(Rc::clone(&self.interner));
        tc.scope_depth = 1;
        tc.enclosing = Some(self as *mut Compiler);
        tc.with_depth = 0; // Reset: thunk body restores with-scopes via upvalues
        tc.base_dir = self.base_dir.clone();

        // Capture with-scope locals from parent as upvalues in thunk body.
        // Emit PushWith at thunk body start to restore with-scope context.
        let with_count = self.emit_with_scope_preamble(&mut tc);

        tc.compile_expr(expr)?;

        // Pop with-scopes in reverse.
        for _ in 0..with_count {
            tc.emit(OpCode::PopWith);
        }

        tc.emit(OpCode::Return);
        let uv_descs: Vec<UpvalueDesc> = tc.upvalues.clone();
        let closure = VMValue::Closure(VMClosure {
            chunk: Rc::new(tc.chunk), upvalues: Vec::new(), arity: 0, name: None, formals: Vec::new(),
        });
        let idx = self.chunk.add_constant(closure)?;
        self.emit(OpCode::MakeThunk);
        self.stack_depth += 1; // MakeThunk pushes one thunk
        self.emit_u16(idx);
        self.emit_u16(uv_descs.len() as u16);
        for uv in &uv_descs {
            self.chunk.write_byte(if uv.is_local { 1 } else { 0 }, self.current_line);
            self.emit_u16(uv.index);
        }
        Ok(())
    }

    /// Compile `inherit (source) name;` as a lazy thunk.
    /// The thunk evaluates `source` and then does `GetAttr(name)` when forced.
    fn compile_inherit_from_thunk(
        &mut self,
        source_expr: &ast::Expr,
        attr_name: &str,
    ) -> Result<(), CompileError> {
        let mut tc = Compiler::with_interner(Rc::clone(&self.interner));
        tc.scope_depth = 1;
        tc.enclosing = Some(self as *mut Compiler);
        tc.with_depth = 0;
        tc.base_dir = self.base_dir.clone();
        let with_count = self.emit_with_scope_preamble(&mut tc);
        tc.compile_expr(source_expr)?;
        let key_idx = tc.add_attr_key(attr_name.to_string())?;
        tc.emit(OpCode::GetAttr);
        tc.emit_u16(key_idx);
        for _ in 0..with_count { tc.emit(OpCode::PopWith); }
        tc.emit(OpCode::Return);
        let uv_descs: Vec<UpvalueDesc> = tc.upvalues.clone();
        let closure = VMValue::Closure(VMClosure {
            chunk: Rc::new(tc.chunk),
            upvalues: Vec::new(),
            arity: 0, formals: Vec::new(),
            name: None,
        });
        let idx = self.chunk.add_constant(closure)?;
        self.emit(OpCode::MakeThunk);
        self.stack_depth += 1; // MakeThunk pushes one thunk
        self.emit_u16(idx);
        self.emit_u16(uv_descs.len() as u16);
        for uv in &uv_descs {
            self.chunk
                .write_byte(if uv.is_local { 1 } else { 0 }, self.current_line);
            self.emit_u16(uv.index);
        }
        Ok(())
    }

    // ── Attribute sets ─────────────────────────────────────────

    fn compile_attrset(&mut self, set: &ast::AttrSet) -> Result<(), CompileError> {
        let rec = set.rec_token().is_some();

        // ★ EVERY group goes through the plan — `plan_for_group_total`, not the
        // gated `plan_for_group`. The gate returns `None` for groups whose
        // ordinary path is already correct, which is what a consumer KEEPING
        // that path wants; the five entry buckets that used to live here were
        // the defect, so they are gone and there is nothing to gate for.
        //
        // A `NormalizeError` is a group nix itself rejects, and it is now
        // RETURNED rather than swallowed into a fallback. That is the whole of
        // the rejection tier on this engine.
        let plan = sui_normalize::plan_for_group_total(set, rec).map_err(reject)?;
        self.compile_plan_group(&plan)
    }

    // ── plan-driven binding groups ────────────────────────────────────────
    //
    // nix decides duplicate-key merge-vs-overwrite at PARSE time from SYNTAX,
    // as a destructive splice into the FIRST-declared node whose `rec` flag
    // governs and into whose scope the second side is re-scoped.
    // `sui-normalize` performs that splice; these functions only emit it.
    //
    // ★ What this replaces, and why it was wrong. `compile_attrset` sorted
    // entries into FIVE buckets (flat / dotted / inherit / dynamic /
    // dynamic-dotted) drained by five sequential emission loops. A key
    // reaching two buckets — `{ a = {b=1;}; a.c = 2; }` puts `a` in both flat
    // and dotted — was therefore emitted TWICE, and `MakeAttrs` kept one of
    // them. Not a merge, a coin toss with a fixed outcome: measured, the VM
    // answered `{"a":{"b":1}}` where nix says `{"a":{"b":1,"c":2}}`, at exit 0.
    //
    // A plan's postcondition is that no static name repeats, so there is no
    // merge to perform and no collision to resolve. `MakeAttrs` needs no
    // change, and in particular its pop order must NOT be reversed: it pops
    // LIFO and `BTreeMap::insert`s, so the FIRST-emitted pair wins. Flipping
    // that turns first-wins into last-wins, and nix's rule is neither — it is
    // a splice, which is why this had to be fixed in the compiler and not in
    // the opcode.

    /// Emit one binding group from a [`GroupPlan`], leaving one attrset on the
    /// stack.
    fn compile_plan_group(&mut self, plan: &sui_normalize::GroupPlan) -> Result<(), CompileError> {
        if plan.recursive {
            self.compile_plan_group_rec(plan)
        } else {
            self.compile_plan_group_flat(plan)
        }
    }

    /// A non-recursive group: every value is emitted in the ENCLOSING scope.
    fn compile_plan_group_flat(
        &mut self,
        plan: &sui_normalize::GroupPlan,
    ) -> Result<(), CompileError> {
        let mut count: u16 = 0;
        for b in &plan.statics {
            let name = sui_intern::resolve(b.name);
            self.emit_plan_binding(&b.binding, &name, plan)?;
            self.emit_constant(VMValue::String(name))?;
            count += 1;
        }
        count += self.emit_plan_dynamics(plan)?;
        self.emit(OpCode::MakeAttrs);
        self.emit_u16(count);
        // MakeAttrs pops 2*count (value+key pairs) and pushes 1 attrset.
        self.stack_depth = self.stack_depth.saturating_sub(2 * count) + 1;
        Ok(())
    }

    /// One binding's VALUE, for a non-recursive group.
    fn emit_plan_binding(
        &mut self,
        binding: &sui_normalize::Binding,
        name: &str,
        plan: &sui_normalize::GroupPlan,
    ) -> Result<(), CompileError> {
        use sui_normalize::Binding;
        match binding {
            Binding::Leaf(expr) => {
                if Self::is_trivial_value(expr) {
                    self.compile_expr(expr)
                } else {
                    self.compile_thunk_immediate(expr)
                }
            }
            // A nested group — a merged literal, or one a dotted path invented.
            // Emitted in place with lazy leaves, which is what
            // `compile_nested_attrset` did for the dotted bucket; the
            // difference is that this one can be `rec` and can hold any
            // binding kind, neither of which a `(path, value)` list can say.
            Binding::Group(sub) => self.compile_plan_group(sub),
            // `inherit x` resolves in the ENCLOSING scope, never the group's
            // own rec scope — that is what makes it shadow rather than
            // self-reference, and why it can never merge.
            Binding::Inherit => self.emit_variable_load(name),
            Binding::InheritFrom { from } => {
                let src = plan.inherit_froms.get(*from).ok_or_else(|| {
                    CompileError::Unsupported(format!(
                        "inherit-from index {from} out of range for '{name}'"
                    ))
                })?;
                let src = src.clone();
                self.compile_inherit_from_thunk(&src, name)
            }
        }
    }

    /// `${e}` keys that did not constant-fold, emitted AFTER every static key
    /// and in source order — nix's ordering, and the reason a dynamic key can
    /// never take part in the parse-time merge.
    fn emit_plan_dynamics(
        &mut self,
        plan: &sui_normalize::GroupPlan,
    ) -> Result<u16, CompileError> {
        use sui_normalize::Binding;
        let mut count: u16 = 0;
        for d in &plan.dynamics {
            match &d.value {
                Binding::Leaf(expr) => {
                    if Self::is_trivial_value(expr) {
                        self.compile_expr(expr)?;
                    } else {
                        self.compile_thunk_immediate(expr)?;
                    }
                }
                Binding::Group(sub) => self.compile_plan_group(sub)?,
                // Refused rather than guessed: an inherited name is resolved
                // BY that name, and a dynamic key has no name until run time.
                // nix rejects `inherit` under a dynamic key at parse time, so
                // this is unreachable from source — but a silent wrong answer
                // here would be indistinguishable from a correct one.
                Binding::Inherit | Binding::InheritFrom { .. } => {
                    return Err(CompileError::Unsupported(
                        "an inherited binding cannot have a dynamic key".to_string(),
                    ))
                }
            }
            self.compile_expr(&d.key)?;
            count += 1;
        }
        Ok(count)
    }

    /// A recursive group: each binding gets a local slot, values are compiled
    /// as DEFERRED thunks, and `PatchThunkUpvalues` re-points them once every
    /// sibling exists.
    ///
    /// The two-phase shape is `compile_rec_attrset`'s and is preserved
    /// deliberately — it is what makes a lambda that captures a later sibling
    /// work, since `MakeClosure` captures upvalues at emission time and the
    /// slot is still null then. What changes is only WHAT is emitted: the plan
    /// has already collapsed duplicate names, so `add_local` can no longer be
    /// called twice with the same name (which produced two locals, of which
    /// `resolve_local` found one).
    fn compile_plan_group_rec(
        &mut self,
        plan: &sui_normalize::GroupPlan,
    ) -> Result<(), CompileError> {
        self.begin_scope();
        let local_count = self.bind_plan_group_locals(plan)?;

        // Build the attrset from the locals.
        let mut count = local_count;
        for b in &plan.statics {
            let name = sui_intern::resolve(b.name);
            let slot = self.find_local_slot(&name);
            self.emit(OpCode::GetLocal);
            self.emit_u16(slot);
            self.emit_constant(VMValue::String(name))?;
        }
        // Dynamic keys resolve in the group's OWN scope, so they are emitted
        // here, while the locals are still live.
        count += self.emit_plan_dynamics(plan)?;

        self.emit(OpCode::MakeAttrs);
        self.emit_u16(count);
        self.stack_depth = self.stack_depth.saturating_sub(2 * count) + 1;

        // Move the attrset down past the locals.
        self.end_scope(local_count);
        Ok(())
    }

    /// Bind a recursive group's names into fresh locals, leaving the scope
    /// OPEN — the caller owns `begin_scope`/`end_scope` and decides what to
    /// leave on top: an attrset (`rec { … }`), a body (`let … in e`), or one
    /// selected member (legacy `let { … body = e; }`). Returns the local count
    /// the caller must pass to `end_scope`.
    ///
    /// That three-way split is exactly why this is separate: all three are the
    /// same recursive binder over the same plan and differ only in the last
    /// two instructions, and before the plan they were three hand-maintained
    /// copies of the two-phase protocol that had already drifted — `let`
    /// refused dotted bindings outright (`Unsupported("dotted let bindings")`)
    /// while `rec` supported them.
    fn bind_plan_group_locals(
        &mut self,
        plan: &sui_normalize::GroupPlan,
    ) -> Result<u16, CompileError> {
        use sui_normalize::Binding;

        let local_count =
            u16::try_from(plan.statics.len()).map_err(|_| CompileError::TooManyLocals)?;

        // Phase 1: allocate a local slot per name, null-initialised.
        for b in &plan.statics {
            self.emit(OpCode::Null); // emit() tracks stack_depth
            self.add_local(sui_intern::resolve(b.name))?;
        }

        // Phase 2: compile each value into its slot.
        let mut thunk_slots: Vec<(u16, Vec<UpvalueDesc>)> = Vec::new();
        for b in &plan.statics {
            let name = sui_intern::resolve(b.name);
            let local_idx = self
                .resolve_local(&name)
                .ok_or_else(|| CompileError::Unsupported(format!("rec local '{name}' vanished")))?;
            let slot = self.locals[local_idx as usize].slot;
            match &b.binding {
                Binding::Leaf(expr) => {
                    if Self::is_trivial_value_for_rec(expr) {
                        self.compile_expr(expr)?;
                    } else {
                        let uv = self.compile_thunk_deferred(expr)?;
                        if !uv.is_empty() {
                            thunk_slots.push((slot, uv));
                        }
                    }
                }
                Binding::Group(sub) => {
                    // Deferred, like the dotted bucket was: a leaf inside the
                    // sub-group may reference a rec sibling, which is only
                    // populated after `PatchThunkUpvalues` runs.
                    let sub = sub.clone();
                    let uv = self.compile_deferred_thunk(|tc| tc.compile_plan_group(&sub))?;
                    if !uv.is_empty() {
                        thunk_slots.push((slot, uv));
                    }
                }
                Binding::Inherit => {
                    // Hide this local so the lookup finds the OUTER binding of
                    // the same name — an inherit shadows, it does not recurse.
                    let saved_depth = self.locals[local_idx as usize].depth;
                    self.locals[local_idx as usize].depth = u32::MAX;
                    self.emit_variable_load_restore(&name, local_idx, saved_depth)?;
                    self.locals[local_idx as usize].depth = saved_depth;
                }
                Binding::InheritFrom { from } => {
                    let src = plan
                        .inherit_froms
                        .get(*from)
                        .ok_or_else(|| {
                            CompileError::Unsupported(format!(
                                "inherit-from index {from} out of range for '{name}'"
                            ))
                        })?
                        .clone();
                    let uv = self.compile_inherit_from_thunk_deferred(&src, &name)?;
                    if !uv.is_empty() {
                        thunk_slots.push((slot, uv));
                    }
                }
            }
            self.emit(OpCode::SetLocal);
            self.emit_u16(slot);
            self.emit(OpCode::Pop);
        }

        // Phase 2b: patch thunk upvalues now that every sibling exists.
        for (slot, uv_descs) in &thunk_slots {
            self.emit(OpCode::PatchThunkUpvalues);
            self.emit_u16(*slot);
            self.emit_u16(u16::try_from(uv_descs.len()).map_err(|_| CompileError::TooManyLocals)?);
            for uv in uv_descs {
                self.chunk
                    .write_byte(u8::from(uv.is_local), self.current_line);
                self.emit_u16(uv.index);
            }
        }

        Ok(local_count)
    }

    /// Compile a legacy let expression (`let { x = 1; body = x; }`).
    ///
    /// This is equivalent to `(rec { x = 1; body = x; }).body`.
    /// The entries are recursive (like `rec { ... }`), and the result
    /// is the `body` attribute.
    fn compile_legacy_let(&mut self, ll: &ast::LegacyLet) -> Result<(), CompileError> {
        // `let { … }` IS `(rec { … }).body`, so it is the same recursive binder
        // over the same plan, selecting one member instead of building an
        // attrset. Same measured defect it fixes:
        //
        //   let { a = {b=1;}; a.c = 2; body = a; }   was {"c":2}
        //                                            nix {"b":1,"c":2}
        let plan = sui_normalize::plan_for_group_total(ll, true).map_err(reject)?;
        if !plan.dynamics.is_empty() {
            return Err(CompileError::Unsupported(
                "dynamic attribute in a legacy let".to_string(),
            ));
        }
        let body_sym = sui_intern::intern("body");
        if !plan.statics.iter().any(|b| b.name == body_sym) {
            return Err(CompileError::Unsupported(
                "legacy let without a 'body' attribute".to_string(),
            ));
        }

        self.begin_scope();
        let local_count = self.bind_plan_group_locals(&plan)?;
        let slot = self.find_local_slot("body");
        self.emit(OpCode::GetLocal);
        self.emit_u16(slot);
        self.end_scope(local_count);
        Ok(())
    }

    /// Emit a variable load for a name (local, upvalue, or with-scope).
    fn emit_variable_load(&mut self, name: &str) -> Result<(), CompileError> {
        if let Some(idx) = self.resolve_local(name) {
            self.emit(OpCode::GetLocal);
            self.emit_u16(self.local_stack_slot(idx));
        } else if let Some(uv_idx) = self.resolve_upvalue(name) {
            self.emit(OpCode::GetUpvalue);
            self.emit_u16(uv_idx as u16);
        } else if self.has_with_scope() {
            let name_idx = self.chunk.add_constant(VMValue::String(name.to_string()))?;
            self.emit(OpCode::LookupWith);
            self.emit_u16(name_idx);
        } else {
            return Err(CompileError::Unsupported(format!(
                "inherit: cannot resolve '{name}'"
            )));
        }
        Ok(())
    }

    /// Emit variable load, restoring local depth on error.
    /// `local_idx` is the index into `self.locals` (for error recovery).
    fn emit_variable_load_restore(
        &mut self,
        name: &str,
        local_idx: u16,
        saved_depth: u32,
    ) -> Result<(), CompileError> {
        if let Some(outer_idx) = self.resolve_local(name) {
            self.emit(OpCode::GetLocal);
            self.emit_u16(self.local_stack_slot(outer_idx));
        } else if let Some(uv_idx) = self.resolve_upvalue(name) {
            self.emit(OpCode::GetUpvalue);
            self.emit_u16(uv_idx as u16);
        } else if self.has_with_scope() {
            let name_idx = self.chunk.add_constant(VMValue::String(name.to_string()))?;
            self.emit(OpCode::LookupWith);
            self.emit_u16(name_idx);
        } else {
            self.locals[local_idx as usize].depth = saved_depth;
            return Err(CompileError::Unsupported(format!(
                "inherit: cannot resolve '{name}' in enclosing scope"
            )));
        }
        Ok(())
    }

    // ── Select (attrset.key) ───────────────────────────────────

    /// Try to resolve an expression as a local variable slot.
    fn try_resolve_as_local(&self, expr: &ast::Expr) -> Option<u16> {
        if let ast::Expr::Ident(id) = expr {
            let name = ident_text(id);
            let idx = self.resolve_local(&name)?;
            Some(self.local_stack_slot(idx))
        } else {
            None
        }
    }

    fn compile_select(&mut self, sel: &ast::Select) -> Result<(), CompileError> {
        let base = sel
            .expr()
            .ok_or_else(|| CompileError::MissingNode("select base".to_string()))?;
        let attrpath = sel
            .attrpath()
            .ok_or_else(|| CompileError::MissingNode("select attrpath".to_string()))?;

        let segments: Vec<_> = attrpath.attrs().collect();

        if let Some(default_expr) = sel.default_expr() {
            // `expr.a.b.c or default` — if ANY segment is missing (or the
            // intermediate value is not an attrset), evaluate the default.
            //
            // Strategy: for each segment (including non-last), check with
            // HasAttr before accessing.  On miss, jump to a shared default
            // path.  HasAttr returns false for non-attrset values, so this
            // also handles the "not an attrset" case.
            //
            // Stack invariant: at each segment, exactly one value (the
            // current attrset being traversed) sits on top.
            //
            //   compile_expr(&base)        ; [val]
            //   for each segment:
            //     Dup                       ; [val, val]
            //     HasAttr key               ; [val, bool]
            //     JumpIfFalse miss          ; [val]
            //     GetAttr key               ; [next_val]
            //   (last segment's GetAttr produces the result)
            //   Jump end
            //   miss:
            //   Pop                         ; []  (discard partial val)
            //   <compile default>           ; [default_val]
            //   end:
            self.compile_expr(&base)?;
            let depth_before = self.stack_depth; // D (one extra value: base)
            let mut miss_jumps: Vec<usize> = Vec::new();
            for (_i, attr) in segments.iter().enumerate() {
                if let Ok(key) = static_attr_name(attr) {
                    let key_idx = self.add_attr_key(key)?;
                    self.emit(OpCode::Dup);             // [val, val]
                    self.emit(OpCode::HasAttr);         // [val, bool]
                    self.emit_u16(key_idx);
                    miss_jumps.push(self.emit_jump(OpCode::JumpIfFalse)); // [val]
                    self.emit(OpCode::GetAttr);         // [next_val]
                    self.emit_u16(key_idx);
                } else {
                    self.emit(OpCode::Dup);             // [val, val]
                    self.compile_dynamic_attr_key(attr)?; // [val, val, key]
                    self.emit(OpCode::DynHasAttr);      // [val, bool]
                    miss_jumps.push(self.emit_jump(OpCode::JumpIfFalse)); // [val]
                    self.compile_dynamic_attr_key(attr)?; // [val, key]
                    self.emit(OpCode::DynGetAttr);      // [next_val]
                }
            }
            // All segments succeeded — result is on stack.
            // Stack depth here = depth_before (each Dup+HasAttr+JumpIfFalse+GetAttr is net 0).
            let end_jump = self.emit_jump(OpCode::Jump);
            // miss path: one value on stack (the partial traversal value)
            for mj in miss_jumps {
                self.patch_jump(mj)?;
            }
            // Reset stack depth to depth_before (we have the partial value on stack)
            self.stack_depth = depth_before;
            self.emit(OpCode::Pop);                    // depth_before - 1
            self.compile_expr(&default_expr)?;         // depth_before (default_val)
            self.patch_jump(end_jump)?;
            // Both paths leave exactly one result on stack: depth = depth_before
        } else {
            // Superinstruction: if base is a local and first segment is static,
            // use GetLocalAttr for the first access (saves one dispatch).
            let local_slot = self.try_resolve_as_local(&base);

            for (i, attr) in segments.iter().enumerate() {
                if let Ok(key) = static_attr_name(attr) {
                    let key_idx = self.add_attr_key(key)?;

                    if i == 0 {
                        if let Some(slot) = local_slot {
                            // Fused GetLocal + GetAttr.
                            self.emit(OpCode::GetLocalAttr);
                            self.emit_u16(slot);
                            self.emit_u16(key_idx);
                        } else {
                            self.compile_expr(&base)?;
                            self.emit(OpCode::GetAttr);
                            self.emit_u16(key_idx);
                        }
                    } else {
                        self.emit(OpCode::GetAttr);
                        self.emit_u16(key_idx);
                    }
                } else {
                    // Dynamic segment: compile base if needed, then key, then DynGetAttr.
                    if i == 0 {
                        self.compile_expr(&base)?;
                    }
                    self.compile_dynamic_attr_key(attr)?;
                    self.emit(OpCode::DynGetAttr);
                }
            }
        }

        Ok(())
    }

    /// Compile a dynamic attribute key (interpolated string or dynamic expr).
    fn compile_dynamic_attr_key(&mut self, attr: &ast::Attr) -> Result<(), CompileError> {
        match attr {
            ast::Attr::Dynamic(d) => {
                let expr = d.expr().ok_or_else(|| {
                    CompileError::MissingNode("dynamic attr key expr".to_string())
                })?;
                self.compile_expr(&expr)
            }
            ast::Attr::Str(s) => {
                let key_expr = ast::Expr::Str(s.clone());
                self.compile_expr(&key_expr)
            }
            ast::Attr::Ident(ident) => {
                self.emit_constant(VMValue::String(ident_text(ident)))
            }
        }
    }

    // ── HasAttr (expr ? key) ───────────────────────────────────

    fn compile_has_attr(&mut self, ha: &ast::HasAttr) -> Result<(), CompileError> {
        let base = ha
            .expr()
            .ok_or_else(|| CompileError::MissingNode("hasattr base".to_string()))?;
        let attrpath = ha
            .attrpath()
            .ok_or_else(|| CompileError::MissingNode("hasattr attrpath".to_string()))?;

        let segments: Vec<_> = attrpath.attrs().collect();

        if segments.len() == 1 {
            // Single-segment: compile base, then HasAttr or DynHasAttr.
            self.compile_expr(&base)?;
            if let Ok(key) = static_attr_name(&segments[0]) {
                let key_idx = self.add_attr_key(key)?;
                self.emit(OpCode::HasAttr);
                self.emit_u16(key_idx);
            } else {
                self.compile_dynamic_attr_key(&segments[0])?;
                self.emit(OpCode::DynHasAttr);
            }
            return Ok(());
        }

        // Multi-segment hasattr: `a ? x.y.z`
        // Compiled as a chain of HasAttr checks with short-circuit jumps.
        // For each segment except the last, we check HasAttr and GetAttr
        // to drill into the nested attrset.
        //
        // The base expression is re-evaluated for each intermediate step,
        // which is correct because Nix is pure and the compiler wraps
        // non-trivial expressions in thunks.
        let mut false_jumps: Vec<usize> = Vec::new();
        // Save stack depth before first segment — all short-circuit
        // targets must converge to (depth_before + 1).
        let depth_before = self.stack_depth;

        for (i, seg) in segments.iter().enumerate() {
            // Build the prefix path: base.seg0.seg1...seg(i-1)
            self.compile_expr(&base)?;
            for prev_seg in &segments[..i] {
                if let Ok(prev_key) = static_attr_name(prev_seg) {
                    let prev_idx = self.add_attr_key(prev_key)?;
                    self.emit(OpCode::GetAttr);
                    self.emit_u16(prev_idx);
                } else {
                    self.compile_dynamic_attr_key(prev_seg)?;
                    self.emit(OpCode::DynGetAttr);
                }
            }
            if let Ok(key) = static_attr_name(seg) {
                let key_idx = self.add_attr_key(key)?;
                self.emit(OpCode::HasAttr);
                self.emit_u16(key_idx);
            } else {
                self.compile_dynamic_attr_key(seg)?;
                self.emit(OpCode::DynHasAttr);
            }

            // For all segments except the last, short-circuit on false.
            if i < segments.len() - 1 {
                false_jumps.push(self.emit_jump(OpCode::JumpIfFalse));
                // Reset depth for next iteration — each JumpIfFalse pops
                // the condition, and at the false target the stack is at
                // depth_before (no result pushed yet). The next segment
                // starts fresh from depth_before.
                self.stack_depth = depth_before;
            }
        }

        // Jump over the false path.
        let done_jump = self.emit_jump(OpCode::Jump);

        // False path: push false for any short-circuit jump.
        // All false_jumps target here, where stack is at depth_before.
        self.stack_depth = depth_before;
        for fj in false_jumps {
            self.patch_jump(fj)?;
        }
        self.emit(OpCode::False);
        // Now stack_depth = depth_before + 1 (same as the true path).

        self.patch_jump(done_jump)?;
        Ok(())
    }

    // ── If/then/else ───────────────────────────────────────────

    fn compile_if(&mut self, ie: &ast::IfElse) -> Result<(), CompileError> {
        let cond = ie
            .condition()
            .ok_or_else(|| CompileError::MissingNode("if condition".to_string()))?;
        let then_body = ie
            .body()
            .ok_or_else(|| CompileError::MissingNode("if then".to_string()))?;
        let else_body = ie
            .else_body()
            .ok_or_else(|| CompileError::MissingNode("if else".to_string()))?;

        // Save tail position — both branches inherit it.
        let tail = self.tail_position;

        // Compile condition (not in tail position).
        self.tail_position = false;
        self.compile_expr(&cond)?;
        // Jump to else if false.
        let else_jump = self.emit_jump(OpCode::JumpIfFalse);
        // After JumpIfFalse, the condition is popped. Save the depth here —
        // this is the stack depth at which both branches start.
        let depth_at_branch = self.stack_depth;
        // Compile then branch (tail position propagated).
        self.tail_position = tail;
        self.compile_expr(&then_body)?;
        // Jump past else.
        let end_jump = self.emit_jump(OpCode::Jump);
        // Patch else jump. Reset stack_depth to the branch start —
        // the else branch starts with the same stack as the then branch.
        self.stack_depth = depth_at_branch;
        self.patch_jump(else_jump)?;
        // Compile else branch (tail position propagated).
        self.tail_position = tail;
        self.compile_expr(&else_body)?;
        // Both branches push exactly one result value, so stack_depth
        // is now depth_at_branch + 1 (correct for the merge point).
        // Patch end jump.
        self.patch_jump(end_jump)?;
        Ok(())
    }

    // ── Lambda ─────────────────────────────────────────────────

    fn compile_lambda(&mut self, lam: &ast::Lambda) -> Result<(), CompileError> {
        let param = lam
            .param()
            .ok_or_else(|| CompileError::MissingNode("lambda param".to_string()))?;
        let body = lam
            .body()
            .ok_or_else(|| CompileError::MissingNode("lambda body".to_string()))?;

        // Compile the function body as a separate chunk (sharing the interner).
        let mut func_compiler = Compiler::with_interner(Rc::clone(&self.interner));
        func_compiler.scope_depth = 1; // function body is its own scope
        // Link to enclosing compiler for upvalue resolution.
        func_compiler.enclosing = Some(self as *mut Compiler);
        // Propagate base directory for relative path resolution.
        func_compiler.base_dir = self.base_dir.clone();
        // The function argument will be at slot 0 (pushed by VM Call handler).
        func_compiler.stack_depth = 1;

        let mut formals_metadata: Vec<(String, bool)> = Vec::new();
        let (arity, name) = match &param {
            ast::Param::IdentParam(ip) => {
                let ident = ip
                    .ident()
                    .ok_or_else(|| CompileError::MissingNode("lambda ident".to_string()))?;
                let name = ident_text(&ident);
                // The argument occupies slot 0 in the function's local stack.
                func_compiler.add_local(name.clone())?;
                (1, Some(name))
            }
            ast::Param::Pattern(pat) => {
                // Pattern destructuring: { a, b, c ? default }
                // The entire argument attrset occupies slot 0.
                // Then we extract individual bindings.
                let bind_name = pat
                    .pat_bind()
                    .and_then(|pb| pb.ident())
                    .map(|id| ident_text(&id));

                if let Some(ref bname) = bind_name {
                    func_compiler.add_local(bname.clone())?;
                } else {
                    // Anonymous slot 0 for the argument attrset.
                    func_compiler.add_local("__arg".to_string())?;
                }

                // For each pattern entry, extract the field from the arg.
                let mut field_names: Vec<(String, Option<ast::Expr>)> = Vec::new();
                for entry in pat.pat_entries() {
                    let ident = entry
                        .ident()
                        .ok_or_else(|| CompileError::MissingNode("pattern entry ident".to_string()))?;
                    let fname = ident_text(&ident);
                    let default = entry.default();
                    formals_metadata.push((fname.clone(), default.is_some()));
                    field_names.push((fname, default));
                }

                // Push local slots for each pattern field.
                for (fname, _) in &field_names {
                    func_compiler.emit(OpCode::Null); // emit() tracks stack_depth
                    func_compiler.add_local(fname.clone())?;
                }

                // Extract each field from slot 0 (the arg attrset).
                for (i, (fname, default)) in field_names.iter().enumerate() {
                    let key_idx = func_compiler.add_attr_key(fname.clone())?;
                    if let Some(default_expr) = default {
                        // Lazy default: only evaluate default_expr when the
                        // key is absent from the argument attrset AND the
                        // parameter is actually forced.  Nix semantics require
                        // defaults to be fully lazy — they must not be forced
                        // at function entry even when the key is missing.
                        //
                        // Emit:
                        //   GetLocal 0        ; push arg attrset
                        //   HasAttr key_idx   ; bool: key present?
                        //   JumpIfFalse L1    ; key missing → default path
                        //   GetLocal 0        ; key present → fetch value
                        //   GetAttr key_idx
                        //   Jump L2
                        // L1:
                        //   MakeThunk(default) ; wrap in thunk — only forced on use
                        // L2:
                        //   ; result on stack
                        func_compiler.emit(OpCode::GetLocal);
                        func_compiler.emit_u16(0); // arg attrset at slot 0
                        func_compiler.emit(OpCode::HasAttr);
                        func_compiler.emit_u16(key_idx);
                        let else_jump = func_compiler.emit_jump(OpCode::JumpIfFalse);
                        // After JumpIfFalse pops the bool, save depth.
                        let depth_at_branch = func_compiler.stack_depth;
                        // Key exists — get the value.
                        func_compiler.emit(OpCode::GetLocal);
                        func_compiler.emit_u16(0);
                        func_compiler.emit(OpCode::GetAttr);
                        func_compiler.emit_u16(key_idx);
                        let end_jump = func_compiler.emit_jump(OpCode::Jump);
                        // Key missing — wrap default in a thunk (lazy).
                        func_compiler.stack_depth = depth_at_branch;
                        func_compiler.patch_jump(else_jump)?;
                        func_compiler.compile_thunk_immediate(default_expr)?;
                        // Both branches leave exactly one value on the stack.
                        func_compiler.patch_jump(end_jump)?;
                    } else {
                        // Use GetAttr (will error if missing).
                        func_compiler.emit(OpCode::GetLocal);
                        func_compiler.emit_u16(0); // arg attrset at slot 0
                        func_compiler.emit(OpCode::GetAttr);
                        func_compiler.emit_u16(key_idx);
                    }
                    // Store into the field's local slot and pop the value from the stack.
                    let field_slot = func_compiler.find_local_slot(fname);
                    func_compiler.emit(OpCode::SetLocal);
                    func_compiler.emit_u16(field_slot);
                    func_compiler.emit(OpCode::Pop);
                    let _ = i; // suppress unused
                }

                (1, bind_name)
            }
        };

        // Compile the body inside the function compiler.
        // The lambda body is in tail position — any direct call can be a tail call.
        func_compiler.tail_position = true;
        func_compiler.compile_expr(&body)?;
        func_compiler.emit(OpCode::Return);

        // Collect upvalue descriptors from the function compiler.
        let upvalue_count = func_compiler.upvalues.len();
        let upvalue_descs: Vec<UpvalueDesc> = func_compiler.upvalues.clone();

        // Store the compiled function as a constant in the outer chunk.
        let closure = VMValue::Closure(VMClosure {
            chunk: Rc::new(func_compiler.chunk),
            upvalues: Vec::new(), // populated at runtime by MakeClosure
            arity,
            name,
            formals: formals_metadata,
        });

        if upvalue_count == 0 {
            // No upvalues: simple constant closure.
            self.emit_constant(closure)
        } else {
            // Emit MakeClosure with upvalue descriptors.
            let idx = self.chunk.add_constant(closure)?;
            self.emit(OpCode::MakeClosure);
            self.stack_depth += 1; // MakeClosure pushes the closure
            self.emit_u16(idx);
            // Emit upvalue count as u16.
            self.emit_u16(upvalue_count as u16);
            // For each upvalue: is_local (u8) + index (u16).
            for uv in &upvalue_descs {
                self.chunk.write_byte(if uv.is_local { 1 } else { 0 }, self.current_line);
                self.emit_u16(uv.index);
            }
            Ok(())
        }
    }

    // ── Apply (function call) ──────────────────────────────────

    fn compile_apply(&mut self, app: &ast::Apply) -> Result<(), CompileError> {
        let func = app
            .lambda()
            .ok_or_else(|| CompileError::MissingNode("apply function".to_string()))?;
        let arg = app
            .argument()
            .ok_or_else(|| CompileError::MissingNode("apply argument".to_string()))?;

        // Save tail position — arguments and function are NOT in tail position.
        let tail = self.tail_position;
        self.tail_position = false;

        // Special form: `import <path>` compiles to path + Import opcode.
        if let ast::Expr::Ident(ref id) = func {
            let name = ident_text(id);
            if name == "import" {
                self.compile_expr(&arg)?;
                self.emit(OpCode::Import);
                return Ok(());
            }
        }

        // Choose Call vs TailCall based on whether this apply is in tail position.
        let call_op = if tail { OpCode::TailCall } else { OpCode::Call };

        // Superinstruction: if the function is a local variable, use
        // GetLocalCall to save one dispatch cycle (only for non-tail calls;
        // tail calls use the standard TailCall opcode which handles frame reuse).
        if !tail {
            if let Some(slot) = self.try_resolve_as_local(&func) {
                self.compile_arg_maybe_thunk(&arg)?;
                self.emit(OpCode::GetLocalCall);
                self.emit_u16(slot);
                return Ok(());
            }
        }

        // Normal: push function, then argument, then Call/TailCall.
        self.compile_expr(&func)?;
        self.compile_arg_maybe_thunk(&arg)?;
        self.emit(call_op);
        Ok(())
    }

    /// Compile a function argument with call-by-need semantics.
    /// Trivial expressions (literals, idents, paths, lambdas) are inlined.
    /// Non-trivial expressions are wrapped in thunks for lazy evaluation.
    /// This matches CppNix's maybeThunk for function arguments.

    // ── Binary operations ──────────────────────────────────────

    fn compile_binop(&mut self, binop: &ast::BinOp) -> Result<(), CompileError> {
        let lhs = binop
            .lhs()
            .ok_or_else(|| CompileError::MissingNode("binop lhs".to_string()))?;
        let rhs = binop
            .rhs()
            .ok_or_else(|| CompileError::MissingNode("binop rhs".to_string()))?;
        let op = binop
            .operator()
            .ok_or_else(|| CompileError::MissingNode("binop operator".to_string()))?;

        match op {
            // Short-circuit: && compiles as if/then/else
            ast::BinOpKind::And => {
                self.compile_expr(&lhs)?;
                let false_jump = self.emit_jump(OpCode::JumpIfFalse);
                // After JumpIfFalse pops lhs, save depth at branch start.
                let depth_at_branch = self.stack_depth;
                self.compile_expr(&rhs)?;
                let end_jump = self.emit_jump(OpCode::Jump);
                // Reset to branch-start depth for the false path.
                self.stack_depth = depth_at_branch;
                self.patch_jump(false_jump)?;
                self.emit(OpCode::False);
                self.patch_jump(end_jump)?;
            }
            // Short-circuit: || compiles as if/then/else
            ast::BinOpKind::Or => {
                self.compile_expr(&lhs)?;
                let true_jump = self.emit_jump(OpCode::JumpIfTrue);
                // After JumpIfTrue pops lhs, save depth at branch start.
                let depth_at_branch = self.stack_depth;
                self.compile_expr(&rhs)?;
                let end_jump = self.emit_jump(OpCode::Jump);
                // Reset to branch-start depth for the true path.
                self.stack_depth = depth_at_branch;
                self.patch_jump(true_jump)?;
                self.emit(OpCode::True);
                self.patch_jump(end_jump)?;
            }
            // Short-circuit: -> is !a || b, so if lhs is false => true
            ast::BinOpKind::Implication => {
                self.compile_expr(&lhs)?;
                let false_jump = self.emit_jump(OpCode::JumpIfFalse);
                // After JumpIfFalse pops lhs, save depth at branch start.
                let depth_at_branch = self.stack_depth;
                self.compile_expr(&rhs)?;
                let end_jump = self.emit_jump(OpCode::Jump);
                // Reset to branch-start depth for the false path.
                self.stack_depth = depth_at_branch;
                self.patch_jump(false_jump)?;
                self.emit(OpCode::True);
                self.patch_jump(end_jump)?;
            }
            // Non-short-circuit: compile both sides, then emit opcode.
            _ => {
                self.compile_expr(&lhs)?;
                self.compile_expr(&rhs)?;
                match op {
                    ast::BinOpKind::Add => self.emit(OpCode::Add),
                    ast::BinOpKind::Sub => self.emit(OpCode::Sub),
                    ast::BinOpKind::Mul => self.emit(OpCode::Mul),
                    ast::BinOpKind::Div => self.emit(OpCode::Div),
                    ast::BinOpKind::Equal => self.emit(OpCode::Equal),
                    ast::BinOpKind::NotEqual => self.emit(OpCode::NotEqual),
                    ast::BinOpKind::Less => self.emit(OpCode::Less),
                    ast::BinOpKind::LessOrEq => self.emit(OpCode::LessEqual),
                    ast::BinOpKind::More => self.emit(OpCode::Greater),
                    ast::BinOpKind::MoreOrEq => self.emit(OpCode::GreaterEqual),
                    ast::BinOpKind::Update => self.emit(OpCode::UpdateAttrs),
                    ast::BinOpKind::Concat => self.emit(OpCode::Concat),
                    ast::BinOpKind::And
                    | ast::BinOpKind::Or
                    | ast::BinOpKind::Implication => unreachable!(),
                    ast::BinOpKind::PipeRight | ast::BinOpKind::PipeLeft => {
                        return Err(CompileError::Unsupported("pipe operators".to_string()));
                    }
                }
            }
        }
        Ok(())
    }

    // ── Unary operations ───────────────────────────────────────

    fn compile_unary(&mut self, op: &ast::UnaryOp) -> Result<(), CompileError> {
        let inner = op
            .expr()
            .ok_or_else(|| CompileError::MissingNode("unary expr".to_string()))?;
        let kind = op
            .operator()
            .ok_or_else(|| CompileError::MissingNode("unary operator".to_string()))?;
        self.compile_expr(&inner)?;
        match kind {
            ast::UnaryOpKind::Negate => self.emit(OpCode::Negate),
            ast::UnaryOpKind::Invert => self.emit(OpCode::Not),
        }
        Ok(())
    }

    // ── With ───────────────────────────────────────────────────

    fn compile_with(&mut self, with: &ast::With) -> Result<(), CompileError> {
        let ns = with
            .namespace()
            .ok_or_else(|| CompileError::MissingNode("with namespace".to_string()))?;
        let body = with
            .body()
            .ok_or_else(|| CompileError::MissingNode("with body".to_string()))?;

        // Compile the namespace expression.
        self.compile_expr(&ns)?;

        // Dup: one copy goes to PushWith (consumed), the other stays as a
        // hidden local so thunks inside the body can capture it as an upvalue.
        // Net stack effect of Dup (+1) + PushWith (-1) = 0.
        self.emit(OpCode::Dup);
        self.emit(OpCode::PushWith);

        // Register the remaining copy as a hidden local.
        let slot = self.add_local("__with_scope".to_string())?;
        self.with_scope_locals.push(slot);
        self.with_depth += 1;

        // Compile the body.
        self.compile_expr(&body)?;

        // Pop the with-scope.
        self.emit(OpCode::PopWith);
        self.with_depth -= 1;
        self.with_scope_locals.pop();

        // Clean up hidden local: body result is TOS, hidden local is below.
        // Stack: [..., __with_scope, body_result]
        // Swap them so body_result survives after Pop.
        // Use SetLocal to overwrite the hidden local with body_result,
        // then Pop to remove the duplicate TOS.
        self.emit(OpCode::SetLocal);
        self.emit_u16(slot);
        self.emit(OpCode::Pop);
        // Adjust: one slot removed (the hidden local is now body_result).
        self.stack_depth = slot + 1;
        self.locals.pop();

        Ok(())
    }

    // ── Assert ─────────────────────────────────────────────────

    fn compile_assert(&mut self, assert: &ast::Assert) -> Result<(), CompileError> {
        let cond = assert
            .condition()
            .ok_or_else(|| CompileError::MissingNode("assert condition".to_string()))?;
        let body = assert
            .body()
            .ok_or_else(|| CompileError::MissingNode("assert body".to_string()))?;
        // Save tail position — the body inherits it, the condition does not.
        let tail = self.tail_position;
        self.tail_position = false;
        self.compile_expr(&cond)?;
        self.emit(OpCode::Assert);
        // The assert body is in tail position if the assert itself is.
        self.tail_position = tail;
        self.compile_expr(&body)?;
        Ok(())
    }

    // ── Lists ──────────────────────────────────────────────────

    fn compile_list(&mut self, list: &ast::List) -> Result<(), CompileError> {
        let items: Vec<_> = list.items().collect();
        let count = u16::try_from(items.len())
            .map_err(|_| CompileError::Unsupported("list too large".to_string()))?;
        for item in &items {
            self.compile_expr(item)?;
        }
        self.emit(OpCode::MakeList);
        self.emit_u16(count);
        // MakeList pops count elements, pushes 1 list.
        self.stack_depth = self.stack_depth.saturating_sub(count) + 1;
        Ok(())
    }

    // ── Emission helpers ───────────────────────────────────────

    fn emit(&mut self, op: OpCode) {
        self.chunk.write_op(op, self.current_line);
        // Track stack depth for correct local-variable slot assignment.
        match op {
            // Push one value
            OpCode::Null | OpCode::True | OpCode::False
            | OpCode::GetLocal | OpCode::GetUpvalue
            | OpCode::PushBuiltins | OpCode::LookupWith => {
                self.stack_depth += 1;
            }
            // Dup: push a copy of TOS (net +1)
            OpCode::Dup => {
                self.stack_depth += 1;
            }
            // Pop one value
            OpCode::Pop | OpCode::PushWith
            | OpCode::Assert | OpCode::Throw | OpCode::Return => {
                self.stack_depth = self.stack_depth.saturating_sub(1);
            }
            // Pop 2, push 1 (net -1)
            OpCode::Add | OpCode::Sub | OpCode::Mul | OpCode::Div
            | OpCode::Equal | OpCode::NotEqual | OpCode::Less
            | OpCode::Greater | OpCode::LessEqual | OpCode::GreaterEqual
            | OpCode::And | OpCode::Or | OpCode::Implication
            | OpCode::Concat | OpCode::UpdateAttrs
            | OpCode::Call | OpCode::TailCall | OpCode::DynGetAttr | OpCode::DynHasAttr => {
                self.stack_depth = self.stack_depth.saturating_sub(1);
            }
            // Pop 1, push 1 (net 0)
            OpCode::Negate | OpCode::Not | OpCode::Force
            | OpCode::GetAttr | OpCode::HasAttr
            | OpCode::Import => {}
            // SetLocal: no stack change (writes to slot)
            OpCode::SetLocal | OpCode::SetUpvalue => {}
            // PopWith: removes from with-scope stack, not value stack
            OpCode::PopWith => {}
            // Jump: no stack change
            OpCode::Jump => {}
            // JumpIfFalse/JumpIfTrue: pop condition
            OpCode::JumpIfFalse | OpCode::JumpIfTrue => {
                self.stack_depth = self.stack_depth.saturating_sub(1);
            }
            // SelectOrDefault: pop 2 (default + attrset), push 1 (net -1)
            OpCode::SelectOrDefault => {
                self.stack_depth = self.stack_depth.saturating_sub(1);
            }
            // DynSelectOrDefault: pop 3 (default + key + attrset), push 1 (net -2)
            OpCode::DynSelectOrDefault => {
                self.stack_depth = self.stack_depth.saturating_sub(2);
            }
            // GetLocalAttr: push 1 (fused GetLocal+GetAttr: push local, get attr = net +1)
            OpCode::GetLocalAttr => {
                self.stack_depth += 1;
            }
            // GetLocalCall: pop 1 arg, get local, call (push local then pop 2 push 1 = net -1 from the arg)
            OpCode::GetLocalCall => {
                self.stack_depth = self.stack_depth.saturating_sub(1);
            }
            // CallBuiltin: handled in emit_u16 for arg count
            OpCode::CallBuiltin => {
                self.stack_depth = self.stack_depth.saturating_sub(1);
            }
            // Complex opcodes with inline operands: handled by callers
            // MakeAttrs: pops 2*count, pushes 1 (handled by caller)
            // MakeList: pops count, pushes 1 (handled by caller)
            // MakeClosure: pushes 1 (handled by caller)
            // MakeThunk: pushes 1 (handled by caller)
            // Interpolate: pops count, pushes 1 (handled by caller)
            // PatchThunkUpvalues: no stack change
            OpCode::Constant | OpCode::MakeAttrs | OpCode::MakeList
            | OpCode::MakeClosure | OpCode::MakeThunk | OpCode::MakeLazyThunk
            | OpCode::Interpolate | OpCode::PatchThunkUpvalues => {}
        }
    }


    fn emit_u16(&mut self, value: u16) {
        self.chunk.write_u16(value, self.current_line);
    }

    fn emit_constant(&mut self, value: VMValue) -> Result<(), CompileError> {
        let idx = self.chunk.add_constant(value)?;
        self.emit(OpCode::Constant);
        self.stack_depth += 1; // Constant pushes one value
        self.emit_u16(idx);
        Ok(())
    }

    /// Add a string constant for an attribute key and pre-intern its symbol.
    ///
    /// The pre-interned symbol is stored in `chunk.key_symbols` so the VM
    /// can skip the `intern()` call on every `GetAttr`/`HasAttr` dispatch.
    fn add_attr_key(&mut self, key: String) -> Result<u16, CompileError> {
        let sym = self.interner.borrow_mut().intern(&key);
        self.chunk.add_key_constant(VMValue::String(key), sym)
    }

    /// Emit a jump instruction with a placeholder target.
    /// Returns the offset of the placeholder (to be patched later).
    fn emit_jump(&mut self, op: OpCode) -> usize {
        self.emit(op);
        let offset = self.chunk.len();
        self.emit_u16(0xFFFF); // placeholder
        offset
    }

    /// Patch a previously emitted jump to point to the current position.
    fn patch_jump(&mut self, placeholder_offset: usize) -> Result<(), CompileError> {
        let target = self.chunk.len();
        let target_u16 = u16::try_from(target).map_err(|_| CompileError::JumpOverflow)?;
        self.chunk.patch_u16(placeholder_offset, target_u16);
        Ok(())
    }

    // ── Scope management ───────────────────────────────────────

    fn begin_scope(&mut self) {
        self.scope_depth += 1;
    }

    fn end_scope(&mut self, binding_count: u16) {
        // We need to preserve the top-of-stack (the body result) and
        // remove the local variable slots below it. Strategy:
        // Store the result in a temporary position, pop locals, restore.
        // Since we know exactly how many locals to pop, we emit Pop
        // instructions after moving the result.
        //
        // The value stack looks like: [... locals... body_result]
        // We need to get it to: [... body_result]
        //
        // We use SetLocal to the first local's slot to stash the body result,
        // then pop the remaining locals, then the stashed value is in the right place.
        //
        // Actually, a simpler approach: we know the body result is on top.
        // We pop N locals from under it. Since we can't do that directly,
        // we use a series of operations:
        // For N locals to pop, we need to move the result down.
        // The most straightforward: use a "swap-and-pop" sequence.
        //
        // Simplest correct approach for now: emit Pop for each local
        // *under* the result. We do this by emitting SetLocal to slot 0
        // of the scope (to stash the result), popping N-1, then GetLocal 0.
        // Actually that clobbers the first local.
        //
        // Even simpler: the VM can interpret end_scope specially, or we
        // can stash in a way that doesn't conflict. For Phase 1, since
        // the VM knows the locals, we'll use a direct approach:
        //
        // The result is on the stack top. Below it are `binding_count` locals.
        // We want to discard those locals but keep the result.
        // Emit: for each local (except we preserve the result on top),
        // we swap the result down and pop the old top.
        //
        // But we don't have a Swap opcode. Let's just do:
        // 1. The locals were at known stack positions.
        // 2. The body result is above them.
        // 3. After removing all locals from self.locals, the VM Pop
        //    instructions will maintain the stack.
        //
        // For correctness: we need the body result on top and locals gone.
        // Plan: emit nothing for the locals themselves (they'll be implicitly
        // dead). Instead, note: the VM stack still has them. We need to
        // actually remove them.
        //
        // Correct plan for Phase 1:
        // The stack is: [... (locals) (body_result)]
        // We need: [... (body_result)]
        // We can store body_result into the first local's slot,
        // then pop (binding_count - 1) times, and the first local slot
        // now holds the result.
        //
        // Wait, we need to be more careful. The locals are at specific
        // absolute positions. After the body result, the stack is:
        //
        // stack_base + 0: local_0
        // stack_base + 1: local_1
        // ...
        // stack_base + N-1: local_N-1
        // stack_base + N: body_result  <-- top
        //
        // We want the stack to be: [... body_result] at stack_base.
        // So: set slot (stack_base + 0) = body_result, then pop N times.
        // That gives us: [body_result] at stack_base. But we popped N,
        // and there are N+1 entries (N locals + result), so we pop N items
        // leaving 1.
        //
        // Hmm, SetLocal doesn't pop. It just writes. So after SetLocal(base+0),
        // the stack is: [result local_1 ... local_N-1 body_result]
        // Then pop N times: [result]
        // Perfect.

        if binding_count > 0 {
            // Use the first local's actual stack slot (not locals vector index)
            // to correctly handle cases where anonymous values sit on the
            // stack between the frame base and the scope's locals.
            let first_local_idx = self.locals.len() - binding_count as usize;
            let base_slot = self.locals[first_local_idx].slot;
            self.emit(OpCode::SetLocal);
            self.emit_u16(base_slot);
            for _ in 0..binding_count {
                self.emit(OpCode::Pop);
            }
            // Update stack_depth: we removed binding_count stack entries
            // but the body result now sits at base_slot.
            self.stack_depth = base_slot + 1;
        }

        // Remove locals from the compiler's tracking.
        while let Some(local) = self.locals.last() {
            if local.depth < self.scope_depth {
                break;
            }
            self.locals.pop();
        }
        self.scope_depth -= 1;
    }

    /// Add a local variable to the current scope. Returns its stack slot.
    fn add_local(&mut self, name: String) -> Result<u16, CompileError> {
        if self.locals.len() >= u16::MAX as usize {
            return Err(CompileError::TooManyLocals);
        }
        // The local's stack slot is the current stack_depth minus 1,
        // because the value (e.g. Null placeholder) was already pushed
        // onto the stack before add_local is called.
        let slot = self.stack_depth - 1;
        self.locals.push(Local {
            name,
            depth: self.scope_depth,
            is_captured: false,
            slot,
        });
        Ok(slot)
    }

    /// Resolve a local variable by name, returning its stack slot index.
    /// Searches from innermost scope outward.
    fn resolve_local(&self, name: &str) -> Option<u16> {
        for (i, local) in self.locals.iter().enumerate().rev() {
            if local.name == name && local.depth != u32::MAX {
                return Some(i as u16);
            }
        }
        None
    }

    /// Get the actual VM stack slot for a local at the given locals-vector index.
    fn local_stack_slot(&self, locals_idx: u16) -> u16 {
        self.locals[locals_idx as usize].slot
    }

    /// Find the VM stack slot of a local by name (must exist).
    /// Returns the actual stack position (relative to frame base),
    /// which may differ from the locals-vector index.
    fn find_local_slot(&self, name: &str) -> u16 {
        let idx = self.resolve_local(name)
            .unwrap_or_else(|| panic!("local '{name}' not found"));
        self.locals[idx as usize].slot
    }

    /// Find the VM stack slot of a local by name, returning `None` if not found.
    fn find_local_slot_opt(&self, name: &str) -> Option<u16> {
        self.resolve_local(name)
            .map(|idx| self.locals[idx as usize].slot)
    }

    /// Add an upvalue to this compiler's upvalue list.
    /// Returns the upvalue index. Deduplicates: if the same upvalue
    /// (same is_local + index) already exists, returns its index.
    fn add_upvalue(&mut self, is_local: bool, index: u16) -> Result<u8, CompileError> {
        // Check for existing identical upvalue.
        for (i, uv) in self.upvalues.iter().enumerate() {
            if uv.is_local == is_local && uv.index == index {
                return Ok(i as u8);
            }
        }
        if self.upvalues.len() >= 256 {
            return Err(CompileError::Unsupported("too many upvalues (max 256)".to_string()));
        }
        let idx = self.upvalues.len() as u8;
        self.upvalues.push(UpvalueDesc { is_local, index });
        Ok(idx)
    }

    /// Resolve a variable as an upvalue by walking the enclosing compiler chain.
    /// Uses Lua 5.x-style upvalue resolution: if the variable is a local in
    /// the enclosing scope, capture it directly. If it's an upvalue in the
    /// enclosing scope, capture that upvalue.
    fn resolve_upvalue(&mut self, name: &str) -> Option<u8> {
        let enclosing_ptr = self.enclosing?;
        // SAFETY: The enclosing compiler is on the stack and outlives this call.
        // We only use raw pointers to avoid Rust's borrow checker issues with
        // the recursive compiler hierarchy, which is purely compile-time.
        let enclosing = unsafe { &mut *enclosing_ptr };

        // Try to find as a local in the enclosing scope.
        if let Some(local_idx) = enclosing.resolve_local(name) {
            enclosing.locals[local_idx as usize].is_captured = true;
            // Store the actual stack slot (not locals index) for the VM.
            let stack_slot = enclosing.locals[local_idx as usize].slot;
            return Some(self.add_upvalue(true, stack_slot).ok()?);
        }

        // Try to find as an upvalue in the enclosing scope (recursive).
        if let Some(uv_idx) = enclosing.resolve_upvalue(name) {
            return Some(self.add_upvalue(false, uv_idx as u16).ok()?);
        }

        // No need to propagate with_depth here — has_with_scope()
        // in compile_ident already walks the enclosing chain to find
        // with-scopes transitively. Setting with_depth as a side effect
        // would poison all subsequent identifier lookups in this compiler,
        // causing names that should be upvalues to be emitted as LookupWith.
        None
    }

    /// Check if this compiler or any enclosing compiler has an active with-scope.
    fn has_with_scope(&self) -> bool {
        if self.with_depth > 0 {
            return true;
        }
        if let Some(enclosing_ptr) = self.enclosing {
            let enclosing = unsafe { &*enclosing_ptr };
            return enclosing.has_with_scope();
        }
        false
    }

    /// Resolve a relative path against the base directory.
    /// Walks the enclosing compiler chain to find a base_dir.
    fn resolve_relative_path(&self, rel_path: &str) -> String {
        if let Some(ref base) = self.base_dir {
            return base.join(rel_path).to_string_lossy().to_string();
        }
        if let Some(enclosing_ptr) = self.enclosing {
            let enclosing = unsafe { &*enclosing_ptr };
            return enclosing.resolve_relative_path(rel_path);
        }
        rel_path.to_string()
    }
}

// ── Helper functions ───────────────────────────────────────────

/// Extract the text of an ident node.
fn ident_text(ident: &ast::Ident) -> String {
    ident
        .ident_token()
        .map(|t| t.text().to_string())
        .unwrap_or_default()
}

/// Extract a static attribute name (identifier or plain string literal).
/// Rejects dynamic/interpolated keys.
/// A `sui-normalize` rejection, as a compile error.
///
/// ★ `ParseError`, matching the tree-walker's `EvalError::ParseError` for the
/// same input, and for the same measured reason: nix refuses a duplicate
/// attribute during PARSING — `nix-instantiate --parse` on `{ a = 1; a = 2; }`
/// fails and never evaluates. Filing it as `Unsupported` would say "the
/// compiler cannot do this", which is a claim about sui; it is a claim about
/// the program.
///
/// The message carries the attribute PATH, not nix's `«string»:1:3` position
/// or caret block — that needs a source-span formatter sui does not have.
fn reject(e: sui_normalize::NormalizeError) -> CompileError {
    CompileError::ParseError(e.to_string())
}

fn static_attr_name(attr: &ast::Attr) -> Result<String, CompileError> {
    match attr {
        ast::Attr::Ident(ident) => Ok(ident_text(ident)),
        ast::Attr::Str(s) => {
            // Handle plain string keys like { "key-with-dashes" = value; }
            let parts: Vec<_> = s.normalized_parts().into_iter().collect();
            if parts.len() == 1 {
                if let InterpolPart::Literal(text) = &parts[0] {
                    return Ok(text.to_string());
                }
            }
            Err(CompileError::Unsupported(
                "interpolated string attribute keys".to_string(),
            ))
        }
        ast::Attr::Dynamic(_) => Err(CompileError::Unsupported(
            "dynamic attribute keys".to_string(),
        )),
    }
}

/// Check if a name is a Nix global builtin (available without `builtins.` prefix).
///
/// ★ THIS LIST IS MEASURED, NOT REMEMBERED — and it is deliberately SHORT.
///
/// It is consulted at step 4 of `compile_ident`, i.e. ABOVE the `with`-scope
/// lookup at step 5. That ordering is correct — in CppNix the base environment
/// is the outermost LEXICAL scope, and `with` is only consulted when a name
/// fails to resolve lexically — which means every name listed here SHADOWS a
/// `with`. So a name that is NOT actually global must not appear, or the VM
/// silently answers with its own builtin where nix answers with the `with`.
///
/// This list previously carried 49 names against nix's real 23. Measured
/// 2026-08-17 against nix 2.31.5, one `nix eval --impure --expr '<name>'`
/// probe per attribute of `builtins.attrNames builtins` (118 names): exactly
/// 23 resolve in the global scope, the other 95 raise `undefined variable`.
/// `true` / `false` / `null` are three of the 23 and are handled earlier in
/// `compile_ident` as literals; `builtins` is a fourth and is handled at step
/// 3 — leaving the 19 below.
///
/// Two divergence shapes the 30 dropped names caused, both silent:
///
/// ```text
///   with { isFunction = x: "LIB"; }; isFunction 1  nix/walker "LIB"  VM false
///   with { typeOf     = x: "LIB"; }; typeOf 1      nix/walker "LIB"  VM "int"
/// ```
///
/// This is nixpkgs-shaped: `with lib;` is everywhere, and `lib.isFunction` /
/// `lib.functionArgs` are functor-aware REDEFINITIONS of the same-named
/// builtins. Second order: nix ERRORS on a bare `typeOf`, so the VM answering
/// it swallowed a genuine undefined-variable bug.
///
/// To re-measure: `nix eval --impure --expr '<name>'` for each name; exit 0
/// means global, `undefined variable` means not.
/// Names Nix resolves as bare identifiers, and which therefore may NOT be
/// shadowed by a `with`.
///
/// This used to be a hand-written `matches!` of 19 names — one of THREE
/// hand-maintained copies, which had already drifted: this list carried
/// `break` and the tree-walker's and `sui-ir`'s did not, so
/// `with { break = "LIB"; }; break` evaluated to `"LIB"` on the walker while
/// nix and this engine both say `false`. Measured against nix 2.31.5,
/// `break` is a real global (`builtins.typeOf break` → `lambda`), so this
/// engine was right and the other two were wrong.
///
/// `true`/`false`/`null` and `builtins` are deliberately absent: they are
/// [`sui_compat::scope::STRUCTURAL_GLOBALS`], handled earlier in
/// `compile_ident` as literals and as the attrset itself, which is why this
/// predicate covers 19 names where the walker's scope list covers 21.
fn is_global_builtin(name: &str) -> bool {
    sui_compat::scope::CALLABLE_GLOBALS.contains(&name)
}

/// Get the source line number for an expression (approximate).
fn line_of(expr: &ast::Expr) -> u32 {
    // rnix doesn't directly expose line numbers; use the text offset
    // as an approximation. A real implementation would map offset→line.
    let offset = AstNode::syntax(expr).text_range().start();
    // Use offset as a rough line proxy.
    u32::from(offset)
}

/// Detect trivial self-referential cycles in let/rec bindings.
///
/// Checks whether any binding `name = name;` directly references itself
/// via a bare identifier. This is always an infinite recursion in `rec`
/// blocks and usually one in `let` blocks (since the binding shadows
/// any outer definition of the same name).
///
/// Returns a list of warning messages for each detected cycle.
fn detect_trivial_cycles(bindings: &[(String, &ast::Expr)]) -> Vec<String> {
    let mut warnings = Vec::new();
    for (name, expr) in bindings {
        if let ast::Expr::Ident(id) = expr {
            if id
                .ident_token()
                .map(|t| t.text() == name.as_str())
                .unwrap_or(false)
            {
                warnings.push(format!("warning: `{name}` directly references itself"));
            }
        }
    }
    warnings
}

/// Parse a `NIX_PATH` env var value into `(prefix, path)` pairs.
///
/// The format is `prefix1=path1:prefix2=path2:...`. An entry with
/// no `=` is treated as having an empty prefix (CppNix-compatible).
/// Empty entries are skipped.
fn parse_nix_path(s: &str) -> Vec<(String, String)> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(':')
        .filter(|e| !e.is_empty())
        .map(|entry| match entry.split_once('=') {
            Some((prefix, path)) => (prefix.to_string(), path.to_string()),
            None => (String::new(), entry.to_string()),
        })
        .collect()
}

/// Resolve a `<name>` search-path token to an absolute filesystem
/// path by walking the entries parsed from `NIX_PATH`.
fn resolve_search_path(name: &str) -> Option<String> {
    let nix_path = std::env::var("NIX_PATH").ok()?;
    for (prefix, path) in parse_nix_path(&nix_path) {
        if !prefix.is_empty() && name == prefix {
            if std::path::Path::new(&path).exists() {
                return Some(path);
            }
            continue;
        }
        if !prefix.is_empty() {
            let needle = format!("{prefix}/");
            if let Some(rest) = name.strip_prefix(&needle) {
                let full = format!("{path}/{rest}");
                if std::path::Path::new(&full).exists() {
                    return Some(full);
                }
                continue;
            }
        }
        if prefix.is_empty() {
            let full = format!("{path}/{name}");
            if std::path::Path::new(&full).exists() {
                return Some(full);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(input: &str) -> Chunk {
        let (chunk, _interner) =
            Compiler::compile(input).unwrap_or_else(|e| panic!("compile failed for '{input}': {e}"));
        chunk
    }

    /// ★ A duplicate dotted path must return a typed error, NOT panic.
    ///
    /// `{ a.b = 1; a.b = 2; }` recursed until the remaining path was empty and
    /// then indexed `path[0]`:
    ///
    /// ```text
    /// thread 'sui-vm-eval' panicked at compiler.rs:1616:32:
    /// index out of bounds: the len is 0 but the index is 0
    /// ```
    ///
    /// The panic fired on the VM's own thread, where the CLI's
    /// whole-expression fallback caught the dead thread and returned the
    /// tree-walker's answer with **exit 0** — so a compiler crash presented as
    /// a clean success and was reachable from a five-token expression. Only
    /// `SUI_VM_STRICT=1` exposed it. That is why this is a test and not just a
    /// bounds fix: the failure mode was indistinguishable from working.
    ///
    /// CppNix rejects this input outright, so refusing to compile it is
    /// correct — and as of 2026-08-18 the refusal comes from the right place.
    /// This comment used to end "until the AST normalizer rejects it at
    /// parse"; that interim is over. The message is now `sui-normalize`'s
    /// (`attribute 'a.b' already defined`) rather than the deleted nested-path
    /// builder's ("defined more than once"), and it names the FULL dotted
    /// path, which is what nix names too.
    #[test]
    fn duplicate_dotted_path_errors_instead_of_panicking() {
        for (src, path) in [
            ("{ a.b = 1; a.b = 2; }", "a.b"),
            ("{ a.b.c = 1; a.b.c = 2; }", "a.b.c"),
            ("{ a.b.c.d = 1; a.b.c.d = 2; }", "a.b.c.d"),
        ] {
            let err = Compiler::compile(src)
                .err()
                .unwrap_or_else(|| panic!("{src} compiled; it must be refused, not accepted"));
            let msg = err.to_string();
            assert!(
                msg.contains(&format!("attribute '{path}' already defined")),
                "{src}: expected a duplicate-attribute refusal naming '{path}', got: {msg}"
            );
        }
    }

    /// CALIBRATION for the row above. Legal nested paths — including a merge
    /// of a dotted path with a sibling — must STILL compile. A "fix" that
    /// rejected any repeated first component would satisfy the test above
    /// while breaking ordinary nix.
    #[test]
    fn legal_nested_paths_still_compile() {
        for src in [
            "{ a.b = 1; a.c = 2; }",
            "{ a.b.c = 1; a.b.d = 2; }",
            "{ a.b = 1; a = { c = 2; }; }",
            "{ x.y.z = 1; }",
            "{ a = { b = 1; }; }",
        ] {
            assert!(
                Compiler::compile(src).is_ok(),
                "{src} must still compile — it is legal nix"
            );
        }
    }

    #[test]
    fn compile_integer() {
        let chunk = compile("42");
        assert!(!chunk.code.is_empty());
        assert_eq!(chunk.constants.len(), 1);
        assert_eq!(chunk.constants[0], VMValue::Int(42));
    }

    #[test]
    fn compile_float() {
        let chunk = compile("3.14");
        assert_eq!(chunk.constants[0], VMValue::Float(3.14));
    }

    #[test]
    fn compile_bool_true() {
        let chunk = compile("true");
        // Constant-folded: true becomes Constant(Bool(true)), Return.
        assert_eq!(chunk.code[0], OpCode::Constant as u8);
        assert_eq!(chunk.constants[0], VMValue::Bool(true));
    }

    #[test]
    fn compile_bool_false() {
        let chunk = compile("false");
        // Constant-folded: false becomes Constant(Bool(false)), Return.
        assert_eq!(chunk.code[0], OpCode::Constant as u8);
        assert_eq!(chunk.constants[0], VMValue::Bool(false));
    }

    #[test]
    fn compile_null() {
        let chunk = compile("null");
        // Constant-folded: null becomes Constant(Null), Return.
        assert_eq!(chunk.code[0], OpCode::Constant as u8);
        assert_eq!(chunk.constants[0], VMValue::Null);
    }

    #[test]
    fn compile_string() {
        let chunk = compile(r#""hello""#);
        assert_eq!(chunk.constants[0], VMValue::String("hello".to_string()));
    }

    #[test]
    fn compile_addition() {
        let chunk = compile("1 + 2");
        // Constant-folded: 1 + 2 becomes Constant(3), Return.
        assert_eq!(chunk.constants[0], VMValue::Int(3));
        assert!(!chunk.code.contains(&(OpCode::Add as u8)));
    }

    #[test]
    fn compile_addition_non_foldable() {
        // When variables are involved, no folding occurs.
        let chunk = compile("let x = 1; in x + 2");
        assert!(chunk.code.contains(&(OpCode::Add as u8)));
    }

    #[test]
    fn compile_if_else() {
        let chunk = compile("if true then 1 else 2");
        // Constant-folded: `if true then 1 else 2` becomes Constant(1), Return.
        assert_eq!(chunk.constants[0], VMValue::Int(1));
        assert!(!chunk.code.contains(&(OpCode::JumpIfFalse as u8)));
    }

    #[test]
    fn compile_if_else_non_foldable() {
        // When condition is not constant, no folding occurs.
        let chunk = compile("let b = true; in if b then 1 else 2");
        assert!(chunk.code.contains(&(OpCode::JumpIfFalse as u8)));
    }

    #[test]
    fn compile_list() {
        let chunk = compile("[1 2 3]");
        assert!(chunk.code.contains(&(OpCode::MakeList as u8)));
    }

    #[test]
    fn compile_attrset() {
        let chunk = compile("{ a = 1; b = 2; }");
        assert!(chunk.code.contains(&(OpCode::MakeAttrs as u8)));
    }

    #[test]
    fn compile_select() {
        let chunk = compile("{ a = 1; }.a");
        assert!(chunk.code.contains(&(OpCode::GetAttr as u8)));
    }

    #[test]
    fn compile_lambda() {
        let chunk = compile("x: x + 1");
        // The lambda body is stored as a closure constant.
        assert!(chunk.constants.iter().any(|c| matches!(c, VMValue::Closure(_))));
    }

    #[test]
    fn compile_negate() {
        let chunk = compile("-42");
        // Constant-folded: -42 becomes Constant(Int(-42)), Return.
        assert_eq!(chunk.constants[0], VMValue::Int(-42));
        assert!(!chunk.code.contains(&(OpCode::Negate as u8)));
    }

    #[test]
    fn compile_negate_non_foldable() {
        let chunk = compile("let x = 42; in -x");
        assert!(chunk.code.contains(&(OpCode::Negate as u8)));
    }

    #[test]
    fn compile_not() {
        let chunk = compile("!true");
        // Constant-folded: !true becomes Constant(Bool(false)), Return.
        assert_eq!(chunk.constants[0], VMValue::Bool(false));
        assert!(!chunk.code.contains(&(OpCode::Not as u8)));
    }

    #[test]
    fn compile_assert() {
        let chunk = compile("assert true; 42");
        assert!(chunk.code.contains(&(OpCode::Assert as u8)));
    }

    #[test]
    fn compile_let_in() {
        let chunk = compile("let x = 1; y = 2; in x + y");
        assert!(chunk.code.contains(&(OpCode::GetLocal as u8)));
    }

    #[test]
    fn compile_parse_error() {
        let result = Compiler::compile("let in");
        assert!(result.is_err());
    }

    #[test]
    fn compile_comparison() {
        let chunk = compile("1 < 2");
        // Constant-folded.
        assert_eq!(chunk.constants[0], VMValue::Bool(true));
    }

    #[test]
    fn compile_equality() {
        let chunk = compile("1 == 1");
        // Constant-folded.
        assert_eq!(chunk.constants[0], VMValue::Bool(true));
    }

    #[test]
    fn compile_update_attrs() {
        let chunk = compile("{ a = 1; } // { b = 2; }");
        assert!(chunk.code.contains(&(OpCode::UpdateAttrs as u8)));
    }

    #[test]
    fn compile_list_concat() {
        let chunk = compile("[1] ++ [2]");
        assert!(chunk.code.contains(&(OpCode::Concat as u8)));
    }

    #[test]
    fn compile_and_short_circuit() {
        let chunk = compile("true && false");
        // Constant-folded.
        assert_eq!(chunk.constants[0], VMValue::Bool(false));
    }

    #[test]
    fn compile_and_short_circuit_non_foldable() {
        let chunk = compile("let a = true; in a && false");
        assert!(chunk.code.contains(&(OpCode::JumpIfFalse as u8)));
    }

    #[test]
    fn compile_or_short_circuit() {
        let chunk = compile("false || true");
        // Constant-folded.
        assert_eq!(chunk.constants[0], VMValue::Bool(true));
    }

    #[test]
    fn compile_or_short_circuit_non_foldable() {
        let chunk = compile("let a = false; in a || true");
        assert!(chunk.code.contains(&(OpCode::JumpIfTrue as u8)));
    }

    #[test]
    fn compile_has_attr() {
        let chunk = compile("{ a = 1; } ? a");
        assert!(chunk.code.contains(&(OpCode::HasAttr as u8)));
    }

    #[test]
    fn compile_select_or_default() {
        // `or default` now uses jump-based control flow:
        // Dup + HasAttr + JumpIfFalse(miss) + GetAttr + Jump(end) + Pop + default
        let chunk = compile("{ a = 1; }.b or 0");
        assert!(chunk.code.contains(&(OpCode::Dup as u8)));
        assert!(chunk.code.contains(&(OpCode::HasAttr as u8)));
        assert!(chunk.code.contains(&(OpCode::JumpIfFalse as u8)));
        assert!(chunk.code.contains(&(OpCode::GetAttr as u8)));
    }

    #[test]
    fn compile_dyn_select_or_default() {
        // Dynamic `or default` now uses jump-based control flow:
        // Dup + DynHasAttr + JumpIfFalse(miss) + DynGetAttr + Jump(end) + Pop + default
        let chunk = compile(r#"let x = "a"; in { a = 1; }.${ x } or 0"#);
        assert!(chunk.code.contains(&(OpCode::Dup as u8)));
        assert!(chunk.code.contains(&(OpCode::DynHasAttr as u8)));
        assert!(chunk.code.contains(&(OpCode::JumpIfFalse as u8)));
        // The hit path uses DynGetAttr to actually select the value.
        assert!(chunk.code.contains(&(OpCode::DynGetAttr as u8)));
    }

    #[test]
    fn compile_multi_segment_select_or_default() {
        // `a.b.c or default` — all segments should use HasAttr+JumpIfFalse
        let chunk = compile("{ a = { b = 1; }; }.a.b.c or 0");
        // Each segment emits Dup + HasAttr + JumpIfFalse + GetAttr
        let has_attr_count = chunk.code.iter().filter(|&&b| b == OpCode::HasAttr as u8).count();
        assert!(has_attr_count >= 3, "expected >= 3 HasAttr ops for 3 segments, got {has_attr_count}");
    }

    #[test]
    fn compile_pattern_lambda() {
        let chunk = compile("{ a, b }: a + b");
        assert!(chunk.constants.iter().any(|c| matches!(c, VMValue::Closure(_))));
    }

    #[test]
    fn compile_string_interpolation() {
        let chunk = compile(r#"let x = "world"; in "hello ${x}""#);
        // Should contain Interpolate opcode.
        assert!(chunk.code.contains(&(OpCode::Interpolate as u8)));
    }

    // ── Static cycle detection ──────────────────────────────

    #[test]
    fn detect_trivial_self_reference() {
        let root = rnix::Root::parse("x");
        let expr = root.tree().expr().unwrap();
        let bindings = vec![("x".to_string(), &expr)];
        let warnings = detect_trivial_cycles(&bindings);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("directly references itself"));
    }

    #[test]
    fn detect_no_false_positive() {
        let root = rnix::Root::parse("y");
        let expr = root.tree().expr().unwrap();
        let bindings = vec![("x".to_string(), &expr)];
        let warnings = detect_trivial_cycles(&bindings);
        assert!(warnings.is_empty());
    }

    #[test]
    fn detect_non_ident_no_warning() {
        let root = rnix::Root::parse("1 + 2");
        let expr = root.tree().expr().unwrap();
        let bindings = vec![("x".to_string(), &expr)];
        let warnings = detect_trivial_cycles(&bindings);
        assert!(warnings.is_empty());
    }

    #[test]
    fn detect_trivial_cycles_multiple() {
        let root_x = rnix::Root::parse("x");
        let expr_x = root_x.tree().expr().unwrap();
        let root_y = rnix::Root::parse("y");
        let expr_y = root_y.tree().expr().unwrap();
        let root_z = rnix::Root::parse("1");
        let expr_z = root_z.tree().expr().unwrap();
        let bindings = vec![
            ("x".to_string(), &expr_x),
            ("y".to_string(), &expr_y),
            ("z".to_string(), &expr_z),
        ];
        let warnings = detect_trivial_cycles(&bindings);
        assert_eq!(warnings.len(), 2);
    }

    // -- PathSearch tests -----------------------------------------------

    /// Serializes every test that touches `NIX_PATH`.
    ///
    /// ── ★ THE "SAFETY" COMMENT WAS THE BUG ────────────────────────────
    /// These tests carried `// SAFETY: test runs single-threaded; no
    /// concurrent env access` above their `set_var`. libtest runs tests in
    /// PARALLEL by default, so that justification was false and the three
    /// NIX_PATH tests raced each other: one would `remove_var` while another
    /// was mid-compile, and the loser saw either no NIX_PATH or the other's
    /// value. Measured on the full workspace run: 1 failing suite in 2,
    /// naming `path_search_compiles_with_matching_nix_path` and
    /// `path_search_with_sub_path`.
    ///
    /// An env var is process-global; the only fix is to make the access
    /// exclusive. This is the same shape as two other flakes found in this
    /// fleet today (a `HOME` override and a shared scratch-file path), which
    /// is why it is worth naming rather than just silencing.
    fn nix_path_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn path_search_compiles_with_matching_nix_path() {
        let _nix_path = nix_path_lock();
        // Set NIX_PATH to a directory containing a target, then compile
        // a search-path expression.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mypkg");
        std::fs::create_dir(&target).unwrap();
        // Set NIX_PATH with prefix=path format.
        let nix_path_val = format!("mypkg={}", target.display());
        // SAFETY: `nix_path_lock` above makes this access exclusive.
        unsafe { std::env::set_var("NIX_PATH", &nix_path_val) };
        let result = Compiler::compile("<mypkg>");
        unsafe { std::env::remove_var("NIX_PATH") };
        assert!(result.is_ok(), "expected compile success, got: {result:?}");
        let (chunk, _) = result.unwrap();
        // The resolved path should be in the constant pool.
        assert!(
            chunk.constants.iter().any(|c| matches!(c, VMValue::Path(p) if p == &target.display().to_string())),
            "expected path constant for {:?}, got: {:?}",
            target.display(),
            chunk.constants,
        );
    }

    #[test]
    fn path_search_fails_when_nix_path_no_match() {
        let _nix_path = nix_path_lock();
        // Set NIX_PATH to something that doesn't match.
        // SAFETY: `nix_path_lock` above makes this access exclusive.
        unsafe { std::env::set_var("NIX_PATH", "other=/nonexistent") };
        let result = Compiler::compile("<nosuchpkg>");
        unsafe { std::env::remove_var("NIX_PATH") };

        // ── ★ AN UNRESOLVABLE SEARCH PATH IS DEFERRED, NOT A COMPILE ERROR ──
        // This asserted `is_err()`, which the compiler deliberately stopped
        // doing: an unresolvable `<…>` is now compiled to a THUNK that throws
        // when forced, "to match CppNix: unresolvable search paths are
        // deferred and caught by tryEval at force-time" (see the emit site).
        // The test pinned the behaviour the change was made to remove, so it
        // has failed ever since — invisibly, because a Linux-only compile
        // error in `build_levels` kept the test gate from ever running.
        //
        // Asserting `is_ok()` ALONE would be vacuous: it passes just as well
        // if the compiler silently resolved `<nosuchpkg>` to some wrong path.
        // So the deferral itself is what gets checked — a closure carrying the
        // throw message reaches the constant pool, exactly as the sibling test
        // above checks for a resolved `Path` constant.
        assert!(
            result.is_ok(),
            "an unresolvable search path is deferred to force-time, not a \
             compile error; got: {result:?}"
        );
        let (chunk, _) = result.unwrap();
        assert!(
            chunk.constants.iter().any(|c| matches!(c, VMValue::Closure(_))),
            "expected a deferred-throw closure in the constant pool, got: {:?}",
            chunk.constants,
        );
    }

    #[test]
    fn path_search_with_sub_path() {
        let _nix_path = nix_path_lock();
        // Test `<nixpkgs/lib>` style — prefix match with sub-path.
        let dir = tempfile::tempdir().unwrap();
        let nixpkgs = dir.path().join("nixpkgs-src");
        let lib_dir = nixpkgs.join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        let nix_path_val = format!("nixpkgs={}", nixpkgs.display());
        // SAFETY: `nix_path_lock` above makes this access exclusive.
        unsafe { std::env::set_var("NIX_PATH", &nix_path_val) };
        let result = Compiler::compile("<nixpkgs/lib>");
        unsafe { std::env::remove_var("NIX_PATH") };
        assert!(result.is_ok(), "expected compile success for sub-path, got: {result:?}");
        let (chunk, _) = result.unwrap();
        let expected_path = lib_dir.display().to_string();
        assert!(
            chunk.constants.iter().any(|c| matches!(c, VMValue::Path(p) if p == &expected_path)),
            "expected path constant for {expected_path}, got: {:?}",
            chunk.constants,
        );
    }

    // -- TailCall detection tests ---------------------------------------

    #[test]
    fn lambda_body_apply_emits_tail_call() {
        // A call in the body of a lambda should emit TailCall.
        let chunk = compile("x: x 1");
        // The outer chunk contains a closure constant; the closure chunk
        // should contain TailCall.
        let closure_chunk = chunk
            .constants
            .iter()
            .find_map(|c| match c {
                VMValue::Closure(cl) => Some(&cl.chunk),
                _ => None,
            })
            .expect("expected a closure constant");
        assert!(
            closure_chunk.code.contains(&(OpCode::TailCall as u8)),
            "lambda body call should emit TailCall, bytecode: {:?}",
            closure_chunk.code,
        );
    }

    #[test]
    fn if_then_apply_emits_tail_call() {
        // A call in the then-branch of an if in a lambda body should be TailCall.
        let chunk = compile("x: if true then x 1 else 0");
        let closure_chunk = chunk
            .constants
            .iter()
            .find_map(|c| match c {
                VMValue::Closure(cl) => Some(&cl.chunk),
                _ => None,
            })
            .expect("expected a closure constant");
        assert!(
            closure_chunk.code.contains(&(OpCode::TailCall as u8)),
            "if-then call should emit TailCall, bytecode: {:?}",
            closure_chunk.code,
        );
    }

    #[test]
    fn if_else_apply_emits_tail_call() {
        // A call in the else-branch of an if in a lambda body should be TailCall.
        let chunk = compile("x: if false then 0 else x 1");
        let closure_chunk = chunk
            .constants
            .iter()
            .find_map(|c| match c {
                VMValue::Closure(cl) => Some(&cl.chunk),
                _ => None,
            })
            .expect("expected a closure constant");
        assert!(
            closure_chunk.code.contains(&(OpCode::TailCall as u8)),
            "if-else call should emit TailCall, bytecode: {:?}",
            closure_chunk.code,
        );
    }

    #[test]
    fn non_tail_apply_emits_regular_call() {
        // A call that is NOT in tail position (e.g. argument to another
        // function) should emit Call, not TailCall.
        let chunk = compile("let f = x: x; in f (f 1)");
        // The top-level chunk should contain Call (for `f (f 1)`).
        // The inner `f 1` is an argument, not tail position.
        assert!(
            chunk.code.contains(&(OpCode::Call as u8))
                || chunk.code.contains(&(OpCode::GetLocalCall as u8)),
            "non-tail call should emit Call or GetLocalCall, bytecode: {:?}",
            chunk.code,
        );
    }

    #[test]
    fn assert_body_apply_emits_tail_call() {
        // A call in the body of an assert inside a lambda should be TailCall.
        let chunk = compile("f: assert true; f 1");
        let closure_chunk = chunk
            .constants
            .iter()
            .find_map(|c| match c {
                VMValue::Closure(cl) => Some(&cl.chunk),
                _ => None,
            })
            .expect("expected a closure constant");
        assert!(
            closure_chunk.code.contains(&(OpCode::TailCall as u8)),
            "assert body call should emit TailCall, bytecode: {:?}",
            closure_chunk.code,
        );
    }

    // -- Multi-segment HasAttr tests ------------------------------------

    #[test]
    fn multi_segment_hasattr_compiles() {
        // `{ a.b = 1; } ? a` should compile and use HasAttr.
        let chunk = compile("{ a = { b = 1; }; } ? a");
        assert!(chunk.code.contains(&(OpCode::HasAttr as u8)));
    }

    #[test]
    fn single_segment_hasattr_still_works() {
        // Single-segment ? should still work.
        let chunk = compile("{ x = 1; } ? x");
        assert!(chunk.code.contains(&(OpCode::HasAttr as u8)));
    }

    #[test]
    fn multi_segment_hasattr_deep_path() {
        // `{ a = { b = 1; }; } ? a.b` — multi-segment hasattr should compile.
        let chunk = compile("{ a = { b = 1; }; } ? a.b");
        // Should contain HasAttr (used for each segment).
        assert!(chunk.code.contains(&(OpCode::HasAttr as u8)));
    }
}
