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

/// A let-binding entry (for the two-pass compilation).
enum LetBinding {
    /// A regular `name = expr;` binding.
    Value(ast::Expr),
    /// A bare `inherit name;` from the enclosing scope.
    Inherit,
    /// An `inherit (source) name;` — copies from source expression.
    InheritFrom(ast::Expr, String),
}

/// A rec attrset binding entry.
enum RecAttrBinding {
    /// A regular `name = expr;` binding.
    Value(ast::Expr),
    /// A bare `inherit name;` from the enclosing scope.
    Inherit,
    /// An `inherit (source) name;`.
    InheritFrom(ast::Expr, String),
    /// Dotted bindings grouped under this top-level key.
    Dotted(Vec<(Vec<String>, ast::Expr)>),
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
                    // Defer to runtime: emit Throw so tryEval can catch it.
                    // Matches CppNix: search path failure is a throw, not a parse error.
                    self.emit_constant(VMValue::String(format!(
                        "search path '{text}' not in NIX_PATH"
                    )))?;
                    self.emit(OpCode::Throw);
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
        self.begin_scope();

        // Collect all binding names and value expressions first so we
        // can allocate all local slots before compiling any values
        // (enabling mutual references between let-bindings).
        let mut bindings: Vec<(String, LetBinding)> = Vec::new();

        for entry in letin.entries() {
            match entry {
                ast::Entry::AttrpathValue(ref apv) => {
                    let attrpath = apv.attrpath().ok_or_else(|| {
                        CompileError::MissingNode("binding attrpath".to_string())
                    })?;
                    let keys: Vec<_> = attrpath.attrs().collect();
                    if keys.len() != 1 {
                        return Err(CompileError::Unsupported(
                            "dotted let bindings".to_string(),
                        ));
                    }
                    let key = static_attr_name(&keys[0])?;
                    let value_expr = apv.value().ok_or_else(|| {
                        CompileError::MissingNode("binding value".to_string())
                    })?;
                    bindings.push((key, LetBinding::Value(value_expr)));
                }
                ast::Entry::Inherit(ref inherit) => {
                    if let Some(from) = inherit.from() {
                        let source_expr = from.expr().ok_or_else(|| {
                            CompileError::MissingNode("inherit from expr".to_string())
                        })?;
                        for attr in inherit.attrs() {
                            let name = static_attr_name(&attr)?;
                            bindings.push((name.clone(), LetBinding::InheritFrom(source_expr.clone(), name)));
                        }
                    } else {
                        for attr in inherit.attrs() {
                            let name = static_attr_name(&attr)?;
                            bindings.push((name, LetBinding::Inherit));
                        }
                    }
                }
            }
        }

        // Static cycle detection: check for `name = name;` patterns.
        {
            let pairs: Vec<(String, &ast::Expr)> = bindings
                .iter()
                .filter_map(|(name, binding)| match binding {
                    LetBinding::Value(expr) => Some((name.clone(), expr as &ast::Expr)),
                    _ => None,
                })
                .collect();
            for warning in detect_trivial_cycles(&pairs) {
                eprintln!("{warning}");
            }
        }

        let binding_count = u16::try_from(bindings.len())
            .map_err(|_| CompileError::TooManyLocals)?;

        // Phase 1: Push Null placeholders and register local slots.
        for (name, _) in &bindings {
            self.emit(OpCode::Null); // emit() tracks stack_depth
            self.add_local(name.clone())?;
        }

        // Phase 2: Compile each binding's value and store into its slot.
        // Two-pass thunk approach for lazy let-bindings:
        //   Pass A: Create thunks (0 upvalues), store in slots.
        //   Pass B: Patch each thunk's upvalues (siblings now exist).
        let mut thunk_slots: Vec<(u16, Vec<UpvalueDesc>)> = Vec::new();

        for (name, binding) in &bindings {
            let local_idx = self.resolve_local(name).unwrap();
            let slot = self.locals[local_idx as usize].slot;
            match binding {
                LetBinding::Value(expr) => {
                    // In let bindings (which are recursive in Nix), lambdas
                    // must not be inlined as trivial — same issue as rec
                    // attrsets: MakeClosure captures upvalues eagerly, but
                    // sibling bindings (especially dotted) may not yet exist.
                    if Self::is_trivial_value_for_rec(expr) {
                        self.compile_expr(expr)?;
                    } else {
                        let uv_descs = self.compile_thunk_deferred(expr)?;
                        if !uv_descs.is_empty() {
                            thunk_slots.push((slot, uv_descs));
                        }
                    }
                    self.emit(OpCode::SetLocal);
                    self.emit_u16(slot);
                    self.emit(OpCode::Pop);
                }
                LetBinding::Inherit => {
                    // Temporarily hide this local so lookup finds the outer one.
                    let saved_depth = self.locals[local_idx as usize].depth;
                    self.locals[local_idx as usize].depth = u32::MAX;
                    if let Some(outer_idx) = self.resolve_local(name) {
                        self.emit(OpCode::GetLocal);
                        self.emit_u16(self.local_stack_slot(outer_idx));
                    } else if let Some(uv_idx) = self.resolve_upvalue(name) {
                        self.emit(OpCode::GetUpvalue);
                        self.emit_u16(uv_idx as u16);
                    } else if self.has_with_scope() {
                        let name_idx = self.chunk.add_constant(VMValue::String(name.clone()))?;
                        self.emit(OpCode::LookupWith);
                        self.emit_u16(name_idx);
                    } else {
                        self.locals[local_idx as usize].depth = saved_depth;
                        return Err(CompileError::Unsupported(format!(
                            "inherit: cannot resolve '{name}' in enclosing scope"
                        )));
                    }
                    self.locals[local_idx as usize].depth = saved_depth;
                    self.emit(OpCode::SetLocal);
                    self.emit_u16(slot);
                    self.emit(OpCode::Pop);
                }
                LetBinding::InheritFrom(source_expr, attr_name) => {
                    // Wrap inherit-from in a thunk to avoid forcing the
                    // source expression at let-binding time (critical for
                    // fixpoint patterns like nixpkgs lib's inherit (lib.trivial)).
                    let uv_descs = self.compile_inherit_from_thunk_deferred(source_expr, attr_name)?;
                    if !uv_descs.is_empty() {
                        thunk_slots.push((slot, uv_descs));
                    }
                    self.emit(OpCode::SetLocal);
                    self.emit_u16(slot);
                    self.emit(OpCode::Pop);
                }
            }
        }

        // Pass B: Patch thunk upvalues now that all siblings exist in slots.
        for (slot, uv_descs) in &thunk_slots {
            self.emit(OpCode::PatchThunkUpvalues);
            self.emit_u16(*slot);
            self.emit_u16(uv_descs.len() as u16);
            for uv in uv_descs {
                self.chunk.write_byte(if uv.is_local { 1 } else { 0 }, self.current_line);
                self.emit_u16(uv.index);
            }
        }

        // Compile the body expression. Its result lands on top of the
        // local variable slots on the stack.
        let body = letin
            .body()
            .ok_or_else(|| CompileError::MissingNode("let body".to_string()))?;
        self.compile_expr(&body)?;

        // Clean up: move the body result down past the locals, then pop them.
        self.end_scope(binding_count);

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
    fn compile_thunk_deferred(&mut self, expr: &ast::Expr) -> Result<Vec<UpvalueDesc>, CompileError> {
        let mut tc = Compiler::with_interner(Rc::clone(&self.interner));
        tc.scope_depth = 1;
        tc.enclosing = Some(self as *mut Compiler);
        tc.with_depth = 0;
        tc.base_dir = self.base_dir.clone();
        let with_count = self.emit_with_scope_preamble(&mut tc);
        for _ in 0..with_count { tc.emit(OpCode::PopWith); }
        tc.emit(OpCode::Return);
        let uv_descs: Vec<UpvalueDesc> = tc.upvalues.clone();
        let closure = VMValue::Closure(VMClosure {
            chunk: Rc::new(tc.chunk), upvalues: Vec::new(), arity: 0, name: None, formals: Vec::new(),
        });
        let idx = self.chunk.add_constant(closure)?;
        self.emit(OpCode::MakeThunk);
        self.stack_depth += 1; // MakeThunk pushes one thunk
        self.emit_u16(idx);
        self.emit_u16(0); // 0 upvalues, patched later
        Ok(uv_descs)
    }

    /// Compile a deferred thunk for `inherit (source) name;` in let bindings.
    /// Like `compile_thunk_deferred`, but emits source + GetAttr(name) + Return.
    fn compile_inherit_from_thunk_deferred(
        &mut self,
        source_expr: &ast::Expr,
        attr_name: &str,
    ) -> Result<Vec<UpvalueDesc>, CompileError> {
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
        self.emit_u16(0); // 0 upvalues, patched later
        Ok(uv_descs)
    }

    /// Compile a deferred thunk for a dotted binding in rec attrsets.
    /// Like `compile_thunk_deferred`, but the thunk body is a nested attrset
    /// rather than a single expression.  Leaf values inside the nested attrset
    /// are individually wrapped in immediate thunks so that forcing the outer
    /// thunk doesn't eagerly evaluate all leaves (avoiding infinite recursion
    /// when dotted bindings cross-reference each other through rec siblings).
    fn compile_nested_attrset_thunk_deferred(
        &mut self,
        sub_bindings: &[(Vec<String>, ast::Expr)],
    ) -> Result<Vec<UpvalueDesc>, CompileError> {
        let mut tc = Compiler::with_interner(Rc::clone(&self.interner));
        tc.scope_depth = 1;
        tc.enclosing = Some(self as *mut Compiler);
        tc.with_depth = 0;
        tc.base_dir = self.base_dir.clone();
        let with_count = self.emit_with_scope_preamble(&mut tc);
        tc.compile_nested_attrset_lazy(sub_bindings)?;
        for _ in 0..with_count { tc.emit(OpCode::PopWith); }
        tc.emit(OpCode::Return);
        let uv_descs: Vec<UpvalueDesc> = tc.upvalues.clone();
        let closure = VMValue::Closure(VMClosure {
            chunk: Rc::new(tc.chunk), upvalues: Vec::new(), arity: 0, name: None, formals: Vec::new(),
        });
        let idx = self.chunk.add_constant(closure)?;
        self.emit(OpCode::MakeThunk);
        self.stack_depth += 1; // MakeThunk pushes one thunk
        self.emit_u16(idx);
        self.emit_u16(0); // 0 upvalues, patched later
        Ok(uv_descs)
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
        if set.rec_token().is_some() {
            return self.compile_rec_attrset(set);
        }

        // Collect all entries, handling dotted bindings by merging them.
        // We need to group dotted bindings by their top-level key.
        let mut flat_entries: Vec<(String, ast::Expr)> = Vec::new();
        let mut dotted_entries: std::collections::BTreeMap<String, Vec<(Vec<String>, ast::Expr)>> =
            std::collections::BTreeMap::new();
        let mut inherit_entries: Vec<(String, Option<ast::Expr>)> = Vec::new();
        let mut dynamic_entries: Vec<(ast::Expr, ast::Expr)> = Vec::new();
        let mut dynamic_dotted_entries: Vec<(ast::Attr, Vec<String>, ast::Expr)> = Vec::new();

        for entry in set.entries() {
            match entry {
                ast::Entry::AttrpathValue(ref apv) => {
                    let attrpath = apv.attrpath().ok_or_else(|| {
                        CompileError::MissingNode("attrset attrpath".to_string())
                    })?;
                    let keys: Vec<_> = attrpath.attrs().collect();
                    let value_expr = apv.value().ok_or_else(|| {
                        CompileError::MissingNode("attrset value".to_string())
                    })?;

                    if keys.len() == 1 {
                        // Check for dynamic key.
                        match &keys[0] {
                            ast::Attr::Dynamic(dyn_attr) => {
                                let key_expr = dyn_attr.expr().ok_or_else(|| {
                                    CompileError::MissingNode("dynamic attr key".to_string())
                                })?;
                                dynamic_entries.push((key_expr, value_expr));
                            }
                            ast::Attr::Str(s) => {
                                // Try to extract a plain string literal
                                // (e.g. `"1" = ...`). These are static keys
                                // and must be compiled like flat entries
                                // (with lazy thunk-wrapped values) to avoid
                                // eagerly evaluating throw expressions in
                                // unaccessed attrset branches.
                                if let Ok(key) = static_attr_name(&keys[0]) {
                                    flat_entries.push((key, value_expr));
                                } else {
                                    // Interpolated string key — truly dynamic.
                                    let key_expr = ast::Expr::Str(s.clone());
                                    dynamic_entries.push((key_expr, value_expr));
                                }
                            }
                            _ => {
                                let key = static_attr_name(&keys[0])?;
                                flat_entries.push((key, value_expr));
                            }
                        }
                    } else {
                        // Dotted binding: group by top-level key.
                        match static_attr_name(&keys[0]) {
                            Ok(top_key) => {
                                let rest_keys: Vec<String> = keys[1..]
                                    .iter()
                                    .map(static_attr_name)
                                    .collect::<Result<_, _>>()?;
                                dotted_entries
                                    .entry(top_key)
                                    .or_default()
                                    .push((rest_keys, value_expr));
                            }
                            Err(_) => {
                                // Dynamic top-level key in dotted path.
                                // Collect rest keys as static names for the
                                // nested attrset; push as a dynamic entry.
                                let rest_keys: Vec<String> = keys[1..]
                                    .iter()
                                    .map(static_attr_name)
                                    .collect::<Result<_, _>>()?;
                                // Store for later compilation as dynamic
                                // dotted entry (key_attr, rest_keys, value).
                                dynamic_dotted_entries.push((
                                    keys[0].clone(),
                                    rest_keys,
                                    value_expr,
                                ));
                            }
                        }
                    }
                }
                ast::Entry::Inherit(ref inherit) => {
                    let source_expr = inherit.from().and_then(|f| f.expr());
                    for attr in inherit.attrs() {
                        let name = static_attr_name(&attr)?;
                        inherit_entries.push((name, source_expr.clone()));
                    }
                }
            }
        }

        let mut count: u16 = 0;

        // Emit flat entries (lazy: wrap non-trivial values in thunks,
        // except inside with-scopes where thunks can't capture the
        // dynamic scope).
        for (key, value_expr) in &flat_entries {
            if Self::is_trivial_value(value_expr) {
                self.compile_expr(value_expr)?;
            } else {
                self.compile_thunk_immediate(value_expr)?;
            }
            self.emit_constant(VMValue::String(key.clone()))?;
            count += 1;
        }

        // Emit dotted entries as nested attrsets.
        for (top_key, sub_bindings) in &dotted_entries {
            self.compile_nested_attrset(sub_bindings)?;
            self.emit_constant(VMValue::String(top_key.clone()))?;
            count += 1;
        }

        // Emit inherit entries (lazy: wrap inherit-from in thunks to avoid
        // forcing the source expression at attrset construction time).
        for (name, source_expr) in &inherit_entries {
            if let Some(src) = source_expr {
                // inherit (source) name; — wrap in a thunk that evaluates
                // source.name lazily (critical for fixpoint patterns like
                // makeExtensible where the source references `self`).
                self.compile_inherit_from_thunk(src, name)?;
            } else {
                // inherit name; — look up in current scope.
                self.emit_variable_load(name)?;
            }
            self.emit_constant(VMValue::String(name.clone()))?;
            count += 1;
        }

        // Emit dynamic entries (lazy: wrap non-trivial values in thunks
        // to preserve Nix's lazy evaluation semantics).
        for (key_expr, value_expr) in &dynamic_entries {
            if Self::is_trivial_value(value_expr) {
                self.compile_expr(value_expr)?;
            } else {
                self.compile_thunk_immediate(value_expr)?;
            }
            self.compile_expr(key_expr)?;
            count += 1;
        }

        // Emit dynamic dotted entries: dynamic top-level key with static
        // nested path. Build the nested attrset from rest_keys, then emit
        // the dynamic key expression.
        for (key_attr, rest_keys, value_expr) in &dynamic_dotted_entries {
            // Build nested attrset: { rest_key1.rest_key2... = value; }
            self.compile_nested_attrset(&[(rest_keys.clone(), value_expr.clone())])?;
            // Compile the dynamic key expression.
            self.compile_dynamic_attr_key(key_attr)?;
            count += 1;
        }

        self.emit(OpCode::MakeAttrs);
        self.emit_u16(count);
        // MakeAttrs pops 2*count (value+key pairs) and pushes 1 attrset.
        self.stack_depth = self.stack_depth.saturating_sub(2 * count) + 1;

        // If there were both flat/dotted and we need to merge, the MakeAttrs
        // handles it by creating one set. Dotted entries that share top-level
        // keys with flat entries need merging. For now, dotted entries that
        // share keys with flat entries override. This matches Nix semantics
        // where the last definition wins (for simple cases).

        Ok(())
    }

    /// Compile a `rec { ... }` attrset.
    fn compile_rec_attrset(&mut self, set: &ast::AttrSet) -> Result<(), CompileError> {
        self.begin_scope();

        // Collect all binding names and their expressions.
        let mut bindings: Vec<(String, RecAttrBinding)> = Vec::new();
        let mut dotted_entries: std::collections::BTreeMap<String, Vec<(Vec<String>, ast::Expr)>> =
            std::collections::BTreeMap::new();

        for entry in set.entries() {
            match entry {
                ast::Entry::AttrpathValue(ref apv) => {
                    let attrpath = apv.attrpath().ok_or_else(|| {
                        CompileError::MissingNode("rec attrset attrpath".to_string())
                    })?;
                    let keys: Vec<_> = attrpath.attrs().collect();
                    let value_expr = apv.value().ok_or_else(|| {
                        CompileError::MissingNode("rec attrset value".to_string())
                    })?;
                    if keys.len() == 1 {
                        let key = static_attr_name(&keys[0])?;
                        bindings.push((key, RecAttrBinding::Value(value_expr)));
                    } else {
                        let top_key = static_attr_name(&keys[0])?;
                        let rest_keys: Vec<String> = keys[1..]
                            .iter()
                            .map(static_attr_name)
                            .collect::<Result<_, _>>()?;
                        dotted_entries
                            .entry(top_key)
                            .or_default()
                            .push((rest_keys, value_expr));
                    }
                }
                ast::Entry::Inherit(ref inherit) => {
                    if let Some(from) = inherit.from() {
                        let source_expr = from.expr().ok_or_else(|| {
                            CompileError::MissingNode("inherit from expr".to_string())
                        })?;
                        for attr in inherit.attrs() {
                            let name = static_attr_name(&attr)?;
                            bindings.push((name.clone(), RecAttrBinding::InheritFrom(source_expr.clone(), name)));
                        }
                    } else {
                        for attr in inherit.attrs() {
                            let name = static_attr_name(&attr)?;
                            bindings.push((name, RecAttrBinding::Inherit));
                        }
                    }
                }
            }
        }

        // Add dotted entries as bindings.
        for (top_key, sub) in &dotted_entries {
            bindings.push((top_key.clone(), RecAttrBinding::Dotted(sub.clone())));
        }

        // Static cycle detection: check for `name = name;` patterns in rec bindings.
        {
            let pairs: Vec<(String, &ast::Expr)> = bindings
                .iter()
                .filter_map(|(name, binding)| match binding {
                    RecAttrBinding::Value(expr) => Some((name.clone(), expr as &ast::Expr)),
                    _ => None,
                })
                .collect();
            for warning in detect_trivial_cycles(&pairs) {
                eprintln!("{warning}");
            }
        }

        let binding_count = u16::try_from(bindings.len())
            .map_err(|_| CompileError::TooManyLocals)?;

        // Phase 1: Allocate local slots with null placeholders.
        for (name, _) in &bindings {
            self.emit(OpCode::Null); // emit() tracks stack_depth
            self.add_local(name.clone())?;
        }

        // Phase 2: Compile each binding's value (lazy: use deferred thunks
        // so rec attrset values are only evaluated when accessed).
        let mut thunk_slots: Vec<(u16, Vec<UpvalueDesc>)> = Vec::new();

        for (name, binding) in &bindings {
            let local_idx = self.resolve_local(name).unwrap();
            let slot = self.locals[local_idx as usize].slot;
            match binding {
                RecAttrBinding::Value(expr) => {
                    // In rec attrsets, lambdas must NOT be treated as trivial
                    // because MakeClosure captures upvalues at emission time.
                    // If a lambda captures a sibling binding (especially a
                    // dotted entry, which is appended last), that slot may still
                    // be null.  Wrapping in a deferred thunk delays MakeClosure
                    // until the lambda is actually accessed, when all siblings
                    // are populated.
                    if Self::is_trivial_value_for_rec(expr) {
                        self.compile_expr(expr)?;
                    } else {
                        let uv_descs = self.compile_thunk_deferred(expr)?;
                        if !uv_descs.is_empty() {
                            thunk_slots.push((slot, uv_descs));
                        }
                    }
                }
                RecAttrBinding::Inherit => {
                    // Temporarily hide this local so lookup finds the outer one.
                    let saved_depth = self.locals[local_idx as usize].depth;
                    self.locals[local_idx as usize].depth = u32::MAX;
                    self.emit_variable_load_restore(name, local_idx, saved_depth)?;
                    self.locals[local_idx as usize].depth = saved_depth;
                }
                RecAttrBinding::InheritFrom(source_expr, attr_name) => {
                    // Wrap inherit-from in deferred thunks for laziness.
                    let uv_descs = self.compile_inherit_from_thunk_deferred(source_expr, attr_name)?;
                    if !uv_descs.is_empty() {
                        thunk_slots.push((slot, uv_descs));
                    }
                }
                RecAttrBinding::Dotted(sub_bindings) => {
                    // Wrap dotted bindings in deferred thunks so that leaf
                    // expressions referencing rec siblings are only evaluated
                    // after PatchThunkUpvalues has populated upvalues.
                    // Leaves inside the thunk are also made individually lazy
                    // to avoid eagerly forcing siblings (which would cause
                    // infinite recursion for cross-referencing dotted bindings).
                    let uv_descs = self.compile_nested_attrset_thunk_deferred(sub_bindings)?;
                    if !uv_descs.is_empty() {
                        thunk_slots.push((slot, uv_descs));
                    }
                }
            }
            self.emit(OpCode::SetLocal);
            self.emit_u16(slot);
            self.emit(OpCode::Pop);
        }

        // Phase 2b: Patch thunk upvalues now that all siblings exist.
        for (slot, uv_descs) in &thunk_slots {
            self.emit(OpCode::PatchThunkUpvalues);
            self.emit_u16(*slot);
            self.emit_u16(uv_descs.len() as u16);
            for uv in uv_descs {
                self.chunk.write_byte(if uv.is_local { 1 } else { 0 }, self.current_line);
                self.emit_u16(uv.index);
            }
        }

        // Build the attrset from the local variables.
        for (name, _) in &bindings {
            let slot = self.find_local_slot(name);
            self.emit(OpCode::GetLocal);
            self.emit_u16(slot);
            self.emit_constant(VMValue::String(name.clone()))?;
        }
        self.emit(OpCode::MakeAttrs);
        self.emit_u16(binding_count);
        // MakeAttrs pops 2*count and pushes 1.
        self.stack_depth = self.stack_depth.saturating_sub(2 * binding_count) + 1;

        // Clean up scope: move the attrset result down past the locals.
        self.end_scope(binding_count);

        Ok(())
    }

    /// Compile a legacy let expression (`let { x = 1; body = x; }`).
    ///
    /// This is equivalent to `(rec { x = 1; body = x; }).body`.
    /// The entries are recursive (like `rec { ... }`), and the result
    /// is the `body` attribute.
    fn compile_legacy_let(&mut self, ll: &ast::LegacyLet) -> Result<(), CompileError> {
        self.begin_scope();

        // Collect bindings — same logic as compile_rec_attrset but
        // operating on a LegacyLet node (which also implements HasEntry).
        let mut bindings: Vec<(String, RecAttrBinding)> = Vec::new();
        let mut dotted_entries: std::collections::BTreeMap<String, Vec<(Vec<String>, ast::Expr)>> =
            std::collections::BTreeMap::new();

        for entry in ll.entries() {
            match entry {
                ast::Entry::AttrpathValue(ref apv) => {
                    let attrpath = apv.attrpath().ok_or_else(|| {
                        CompileError::MissingNode("legacy let attrpath".to_string())
                    })?;
                    let keys: Vec<_> = attrpath.attrs().collect();
                    let value_expr = apv.value().ok_or_else(|| {
                        CompileError::MissingNode("legacy let value".to_string())
                    })?;
                    if keys.len() == 1 {
                        let key = static_attr_name(&keys[0])?;
                        bindings.push((key, RecAttrBinding::Value(value_expr)));
                    } else {
                        let top_key = static_attr_name(&keys[0])?;
                        let rest_keys: Vec<String> = keys[1..]
                            .iter()
                            .map(static_attr_name)
                            .collect::<Result<_, _>>()?;
                        dotted_entries
                            .entry(top_key)
                            .or_default()
                            .push((rest_keys, value_expr));
                    }
                }
                ast::Entry::Inherit(ref inherit) => {
                    if let Some(from) = inherit.from() {
                        let source_expr = from.expr().ok_or_else(|| {
                            CompileError::MissingNode("inherit from expr".to_string())
                        })?;
                        for attr in inherit.attrs() {
                            let name = static_attr_name(&attr)?;
                            bindings.push((name.clone(), RecAttrBinding::InheritFrom(source_expr.clone(), name)));
                        }
                    } else {
                        for attr in inherit.attrs() {
                            let name = static_attr_name(&attr)?;
                            bindings.push((name, RecAttrBinding::Inherit));
                        }
                    }
                }
            }
        }

        // Add dotted entries as bindings.
        for (top_key, sub) in &dotted_entries {
            bindings.push((top_key.clone(), RecAttrBinding::Dotted(sub.clone())));
        }

        let binding_count = u16::try_from(bindings.len())
            .map_err(|_| CompileError::TooManyLocals)?;

        // Phase 1: Allocate local slots with null placeholders.
        for (name, _) in &bindings {
            self.emit(OpCode::Null);
            self.add_local(name.clone())?;
        }

        // Phase 2: Compile each binding's value (lazy thunks for non-trivial).
        let mut thunk_slots: Vec<(u16, Vec<UpvalueDesc>)> = Vec::new();

        for (name, binding) in &bindings {
            let local_idx = self.resolve_local(name).unwrap();
            let slot = self.locals[local_idx as usize].slot;
            match binding {
                RecAttrBinding::Value(expr) => {
                    // Same rec-aware trivial check as compile_rec_attrset:
                    // lambdas must be deferred to avoid capturing null slots.
                    if Self::is_trivial_value_for_rec(expr) {
                        self.compile_expr(expr)?;
                    } else {
                        let uv_descs = self.compile_thunk_deferred(expr)?;
                        if !uv_descs.is_empty() {
                            thunk_slots.push((slot, uv_descs));
                        }
                    }
                }
                RecAttrBinding::Inherit => {
                    let saved_depth = self.locals[local_idx as usize].depth;
                    self.locals[local_idx as usize].depth = u32::MAX;
                    self.emit_variable_load_restore(name, local_idx, saved_depth)?;
                    self.locals[local_idx as usize].depth = saved_depth;
                }
                RecAttrBinding::InheritFrom(source_expr, attr_name) => {
                    let uv_descs = self.compile_inherit_from_thunk_deferred(source_expr, attr_name)?;
                    if !uv_descs.is_empty() {
                        thunk_slots.push((slot, uv_descs));
                    }
                }
                RecAttrBinding::Dotted(sub_bindings) => {
                    // Wrap dotted bindings in deferred thunks (same as rec attrset).
                    let uv_descs = self.compile_nested_attrset_thunk_deferred(sub_bindings)?;
                    if !uv_descs.is_empty() {
                        thunk_slots.push((slot, uv_descs));
                    }
                }
            }
            self.emit(OpCode::SetLocal);
            self.emit_u16(slot);
            self.emit(OpCode::Pop);
        }

        // Phase 2b: Patch thunk upvalues now that all siblings exist.
        for (slot, uv_descs) in &thunk_slots {
            self.emit(OpCode::PatchThunkUpvalues);
            self.emit_u16(*slot);
            self.emit_u16(uv_descs.len() as u16);
            for uv in uv_descs {
                self.chunk.write_byte(if uv.is_local { 1 } else { 0 }, self.current_line);
                self.emit_u16(uv.index);
            }
        }

        // Instead of building an attrset and selecting "body", directly
        // load the local named "body" — this avoids constructing the
        // intermediate attrset entirely.
        let body_slot = self.find_local_slot_opt("body").ok_or_else(|| {
            CompileError::MissingNode("legacy let missing 'body' binding".to_string())
        })?;
        self.emit(OpCode::GetLocal);
        self.emit_u16(body_slot);

        // Clean up scope: move the body value down past the locals.
        self.end_scope(binding_count);

        Ok(())
    }

    /// Compile a nested attrset from a list of (remaining-path, value) pairs.
    /// Used for dotted bindings like `{ a.b = 1; a.c = 2; }`.
    ///
    /// When `lazy_leaves` is true, non-trivial leaf values are wrapped in
    /// immediate thunks (for rec attrsets where leaves may reference siblings
    /// that aren't fully initialised until after `PatchThunkUpvalues` runs).
    fn compile_nested_attrset(
        &mut self,
        sub_bindings: &[(Vec<String>, ast::Expr)],
    ) -> Result<(), CompileError> {
        self.compile_nested_attrset_inner(sub_bindings, false)
    }

    fn compile_nested_attrset_lazy(
        &mut self,
        sub_bindings: &[(Vec<String>, ast::Expr)],
    ) -> Result<(), CompileError> {
        self.compile_nested_attrset_inner(sub_bindings, true)
    }

    fn compile_nested_attrset_inner(
        &mut self,
        sub_bindings: &[(Vec<String>, ast::Expr)],
        lazy_leaves: bool,
    ) -> Result<(), CompileError> {
        // Group by next key.
        let mut groups: std::collections::BTreeMap<String, Vec<(Vec<String>, ast::Expr)>> =
            std::collections::BTreeMap::new();

        for (path, expr) in sub_bindings {
            if path.len() == 1 {
                // Leaf binding.
                groups
                    .entry(path[0].clone())
                    .or_default()
                    .push((vec![], expr.clone()));
            } else {
                // Nested further.
                groups
                    .entry(path[0].clone())
                    .or_default()
                    .push((path[1..].to_vec(), expr.clone()));
            }
        }

        let mut count: u16 = 0;
        for (key, nested) in &groups {
            if nested.len() == 1 && nested[0].0.is_empty() {
                // Simple leaf.
                if lazy_leaves && !Self::is_trivial_value(&nested[0].1) {
                    self.compile_thunk_immediate(&nested[0].1)?;
                } else {
                    self.compile_expr(&nested[0].1)?;
                }
            } else {
                // Recurse for deeper nesting.
                self.compile_nested_attrset_inner(nested, lazy_leaves)?;
            }
            self.emit_constant(VMValue::String(key.clone()))?;
            count += 1;
        }

        self.emit(OpCode::MakeAttrs);
        self.emit_u16(count);
        self.stack_depth = self.stack_depth.saturating_sub(2 * count) + 1;
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
                self.compile_expr(&arg)?;
                self.emit(OpCode::GetLocalCall);
                self.emit_u16(slot);
                return Ok(());
            }
        }

        // Normal: push function, then argument, then Call/TailCall.
        self.compile_expr(&func)?;
        self.compile_expr(&arg)?;
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
fn is_global_builtin(name: &str) -> bool {
    matches!(
        name,
        "abort"
            | "baseNameOf"
            | "break"
            | "derivation"
            | "derivationStrict"
            | "dirOf"
            | "fetchGit"
            | "fetchMercurial"
            | "fetchTarball"
            | "fetchTree"
            | "fromTOML"
            | "import"
            | "isNull"
            | "map"
            | "placeholder"
            | "removeAttrs"
            | "scopedImport"
            | "throw"
            | "toString"
            | "trace"
            | "typeOf"
            | "seq"
            | "deepSeq"
            | "tryEval"
            | "genericClosure"
            | "addErrorContext"
            | "unsafeGetAttrPos"
            | "isPath"
            | "isFloat"
            | "isInt"
            | "isBool"
            | "isString"
            | "isList"
            | "isAttrs"
            | "isFunction"
            | "functionArgs"
            | "pathExists"
            | "readFile"
            | "readDir"
            | "toFile"
            | "toPath"
            | "fromJSON"
            | "toJSON"
            | "storeDir"
            | "nixVersion"
            | "nixPath"
            | "currentSystem"
            | "currentTime"
            | "langVersion"
    )
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

    #[test]
    fn path_search_compiles_with_matching_nix_path() {
        // Set NIX_PATH to a directory containing a target, then compile
        // a search-path expression.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mypkg");
        std::fs::create_dir(&target).unwrap();
        // Set NIX_PATH with prefix=path format.
        let nix_path_val = format!("mypkg={}", target.display());
        // SAFETY: test runs single-threaded; no concurrent env access.
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
        // Set NIX_PATH to something that doesn't match.
        // SAFETY: test runs single-threaded; no concurrent env access.
        unsafe { std::env::set_var("NIX_PATH", "other=/nonexistent") };
        let result = Compiler::compile("<nosuchpkg>");
        unsafe { std::env::remove_var("NIX_PATH") };
        assert!(result.is_err(), "expected compile error for missing search path");
    }

    #[test]
    fn path_search_with_sub_path() {
        // Test `<nixpkgs/lib>` style — prefix match with sub-path.
        let dir = tempfile::tempdir().unwrap();
        let nixpkgs = dir.path().join("nixpkgs-src");
        let lib_dir = nixpkgs.join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        let nix_path_val = format!("nixpkgs={}", nixpkgs.display());
        // SAFETY: test runs single-threaded; no concurrent env access.
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
