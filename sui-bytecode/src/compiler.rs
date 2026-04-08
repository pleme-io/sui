//! AST-to-bytecode compiler.
//!
//! Walks the rnix typed AST and emits a [`Chunk`] of bytecode
//! instructions. The compiler manages local variable resolution via
//! a scope stack and emits appropriate `GetLocal`/`SetLocal` instructions.

use std::rc::Rc;

use rnix::ast::{self, AstToken, HasEntry, InterpolPart};
use rowan::ast::AstNode;

use crate::chunk::Chunk;
use crate::error::CompileError;
use crate::opcode::OpCode;
use crate::value::{VMClosure, VMValue};

/// A local variable in the current scope.
#[derive(Debug, Clone)]
struct Local {
    /// The variable name.
    name: String,
    /// Scope depth (0 = outermost).
    depth: u32,
}

/// A let-binding entry (for the two-pass compilation).
enum LetBinding {
    /// A regular `name = expr;` binding.
    Value(ast::Expr),
    /// A bare `inherit name;` from the enclosing scope.
    Inherit,
}

/// The bytecode compiler.
///
/// Compiles a single expression (which may contain nested lambdas)
/// into a top-level [`Chunk`]. Nested lambdas produce sub-chunks
/// stored in the constant pool.
pub struct Compiler {
    /// The chunk being compiled into.
    chunk: Chunk,
    /// Local variable stack (simulates the runtime value stack layout).
    locals: Vec<Local>,
    /// Current scope depth.
    scope_depth: u32,
    /// Current source line for error reporting.
    current_line: u32,
}

impl Compiler {
    /// Create a new compiler.
    fn new() -> Self {
        Self {
            chunk: Chunk::new(),
            locals: Vec::new(),
            scope_depth: 0,
            current_line: 0,
        }
    }

    /// Compile a Nix expression string into bytecode.
    pub fn compile(input: &str) -> Result<Chunk, CompileError> {
        let parse = rnix::Root::parse(input);
        if !parse.errors().is_empty() {
            let msgs: Vec<String> = parse.errors().iter().map(|e| e.to_string()).collect();
            return Err(CompileError::ParseError(msgs.join("; ")));
        }
        let root = parse.tree();
        let expr = root
            .expr()
            .ok_or_else(|| CompileError::ParseError("empty expression".to_string()))?;
        Self::compile_expr_to_chunk(&expr)
    }

    /// Compile an rnix AST expression into a chunk.
    fn compile_expr_to_chunk(expr: &ast::Expr) -> Result<Chunk, CompileError> {
        let mut compiler = Self::new();
        compiler.compile_expr(expr)?;
        compiler.emit(OpCode::Return);
        Ok(compiler.chunk)
    }

    // ── Expression dispatch ────────────────────────────────────

    fn compile_expr(&mut self, expr: &ast::Expr) -> Result<(), CompileError> {
        self.current_line = line_of(expr);
        match expr {
            ast::Expr::Literal(lit) => self.compile_literal(lit),
            ast::Expr::Str(s) => self.compile_str(s),
            ast::Expr::Ident(id) => self.compile_ident(id),
            ast::Expr::LetIn(letin) => self.compile_let(letin),
            ast::Expr::AttrSet(set) => self.compile_attrset(set),
            ast::Expr::Select(sel) => self.compile_select(sel),
            ast::Expr::HasAttr(ha) => self.compile_has_attr(ha),
            ast::Expr::IfElse(ie) => self.compile_if(ie),
            ast::Expr::Lambda(lam) => self.compile_lambda(lam),
            ast::Expr::Apply(app) => self.compile_apply(app),
            ast::Expr::BinOp(op) => self.compile_binop(op),
            ast::Expr::UnaryOp(op) => self.compile_unary(op),
            ast::Expr::Assert(a) => self.compile_assert(a),
            ast::Expr::List(l) => self.compile_list(l),
            ast::Expr::Paren(p) => {
                let inner = p
                    .expr()
                    .ok_or_else(|| CompileError::MissingNode("paren expr".to_string()))?;
                self.compile_expr(&inner)
            }
            ast::Expr::Root(r) => {
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
                self.emit_constant(VMValue::Path(text))
            }
            ast::Expr::PathHome(p) => {
                let text = p.syntax().text().to_string();
                self.emit_constant(VMValue::Path(text))
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
                // Look up in locals.
                if let Some(idx) = self.resolve_local(&name) {
                    self.emit(OpCode::GetLocal);
                    self.emit_u16(idx);
                    Ok(())
                } else {
                    // In Phase 1, unresolved variables are an error.
                    // Phase 2 will add upvalue/with-scope resolution.
                    Err(CompileError::Unsupported(format!(
                        "unresolved variable: {name} (upvalues/with not yet implemented)"
                    )))
                }
            }
        }
    }

    // ── Let/in ─────────────────────────────────────────────────

    fn compile_let(&mut self, letin: &ast::LetIn) -> Result<(), CompileError> {
        self.begin_scope();

        // Collect all binding names and value expressions first so we
        // can allocate all local slots before compiling any values
        // (enabling mutual references between let-bindings in Phase 2).
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
                    if inherit.from().is_some() {
                        return Err(CompileError::Unsupported(
                            "inherit (source) in let".to_string(),
                        ));
                    }
                    for attr in inherit.attrs() {
                        let name = static_attr_name(&attr)?;
                        bindings.push((name, LetBinding::Inherit));
                    }
                }
            }
        }

        let binding_count = u16::try_from(bindings.len())
            .map_err(|_| CompileError::TooManyLocals)?;

        // Phase 1: Push Null placeholders and register local slots.
        for (name, _) in &bindings {
            self.emit(OpCode::Null);
            self.add_local(name.clone())?;
        }

        // Phase 2: Compile each binding's value and store into its slot.
        for (name, binding) in &bindings {
            let slot = self.find_local_slot(name);
            match binding {
                LetBinding::Value(expr) => {
                    self.compile_expr(expr)?;
                    self.emit(OpCode::SetLocal);
                    self.emit_u16(slot);
                    self.emit(OpCode::Pop); // SetLocal peeks; discard the copy.
                }
                LetBinding::Inherit => {
                    // Temporarily hide this local so lookup finds the outer one.
                    let saved_depth = self.locals[slot as usize].depth;
                    self.locals[slot as usize].depth = u32::MAX;
                    if let Some(outer_slot) = self.resolve_local(name) {
                        self.emit(OpCode::GetLocal);
                        self.emit_u16(outer_slot);
                    } else {
                        self.locals[slot as usize].depth = saved_depth;
                        return Err(CompileError::Unsupported(format!(
                            "inherit: cannot resolve '{name}' in enclosing scope"
                        )));
                    }
                    self.locals[slot as usize].depth = saved_depth;
                    self.emit(OpCode::SetLocal);
                    self.emit_u16(slot);
                    self.emit(OpCode::Pop);
                }
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

    // ── Attribute sets ─────────────────────────────────────────

    fn compile_attrset(&mut self, set: &ast::AttrSet) -> Result<(), CompileError> {
        if set.rec_token().is_some() {
            return Err(CompileError::Unsupported("rec attrset".to_string()));
        }

        let mut count: u16 = 0;
        for entry in set.entries() {
            match entry {
                ast::Entry::AttrpathValue(ref apv) => {
                    let attrpath = apv.attrpath().ok_or_else(|| {
                        CompileError::MissingNode("attrset attrpath".to_string())
                    })?;
                    let keys: Vec<_> = attrpath.attrs().collect();
                    if keys.len() != 1 {
                        return Err(CompileError::Unsupported(
                            "dotted attrset keys".to_string(),
                        ));
                    }
                    let key = static_attr_name(&keys[0])?;
                    let value_expr = apv.value().ok_or_else(|| {
                        CompileError::MissingNode("attrset value".to_string())
                    })?;
                    // Push value first, then key (VM pops key then value).
                    self.compile_expr(&value_expr)?;
                    self.emit_constant(VMValue::String(key))?;
                    count += 1;
                }
                ast::Entry::Inherit(ref inherit) => {
                    if inherit.from().is_some() {
                        return Err(CompileError::Unsupported(
                            "inherit (source) in attrset".to_string(),
                        ));
                    }
                    for attr in inherit.attrs() {
                        let name = static_attr_name(&attr)?;
                        // Value: look up in current scope.
                        if let Some(slot) = self.resolve_local(&name) {
                            self.emit(OpCode::GetLocal);
                            self.emit_u16(slot);
                        } else {
                            return Err(CompileError::Unsupported(format!(
                                "inherit: cannot resolve '{name}'"
                            )));
                        }
                        // Key.
                        self.emit_constant(VMValue::String(name))?;
                        count += 1;
                    }
                }
            }
        }

        self.emit(OpCode::MakeAttrs);
        self.emit_u16(count);
        Ok(())
    }

    // ── Select (attrset.key) ───────────────────────────────────

    fn compile_select(&mut self, sel: &ast::Select) -> Result<(), CompileError> {
        let base = sel
            .expr()
            .ok_or_else(|| CompileError::MissingNode("select base".to_string()))?;
        let attrpath = sel
            .attrpath()
            .ok_or_else(|| CompileError::MissingNode("select attrpath".to_string()))?;

        let segments: Vec<_> = attrpath.attrs().collect();

        if let Some(default_expr) = sel.default_expr() {
            // `expr.key or default` — use SelectOrDefault for the last segment.
            // For multi-segment paths with a default, only the final segment
            // gets the or-default treatment; intermediate segments use GetAttr.
            self.compile_expr(&base)?;
            for (i, attr) in segments.iter().enumerate() {
                let key = static_attr_name(attr)?;
                let key_idx = self.chunk.add_constant(VMValue::String(key))?;
                if i == segments.len() - 1 {
                    // Last segment: compile default, push it, then SelectOrDefault.
                    self.compile_expr(&default_expr)?;
                    self.emit(OpCode::SelectOrDefault);
                    self.emit_u16(key_idx);
                } else {
                    self.emit(OpCode::GetAttr);
                    self.emit_u16(key_idx);
                }
            }
        } else {
            self.compile_expr(&base)?;
            for attr in &segments {
                let key = static_attr_name(attr)?;
                let key_idx = self.chunk.add_constant(VMValue::String(key))?;
                self.emit(OpCode::GetAttr);
                self.emit_u16(key_idx);
            }
        }

        Ok(())
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

        // For single-segment: compile base, then HasAttr.
        // For multi-segment: we need to check each segment in sequence,
        // short-circuiting to false if any intermediate value is not an attrset
        // or doesn't contain the key. For Phase 1, support single-segment only.
        if segments.len() != 1 {
            return Err(CompileError::Unsupported(
                "multi-segment hasattr".to_string(),
            ));
        }

        self.compile_expr(&base)?;
        let key = static_attr_name(&segments[0])?;
        let key_idx = self.chunk.add_constant(VMValue::String(key))?;
        self.emit(OpCode::HasAttr);
        self.emit_u16(key_idx);
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

        // Compile condition.
        self.compile_expr(&cond)?;
        // Jump to else if false.
        let else_jump = self.emit_jump(OpCode::JumpIfFalse);
        // Compile then branch.
        self.compile_expr(&then_body)?;
        // Jump past else.
        let end_jump = self.emit_jump(OpCode::Jump);
        // Patch else jump.
        self.patch_jump(else_jump)?;
        // Compile else branch.
        self.compile_expr(&else_body)?;
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

        // Compile the function body as a separate chunk.
        let mut func_compiler = Compiler::new();
        func_compiler.scope_depth = 1; // function body is its own scope

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
                    field_names.push((fname, default));
                }

                // Push local slots for each pattern field.
                for (fname, _) in &field_names {
                    func_compiler.emit(OpCode::Null);
                    func_compiler.add_local(fname.clone())?;
                }

                // Extract each field from slot 0 (the arg attrset).
                for (i, (fname, default)) in field_names.iter().enumerate() {
                    let key_idx =
                        func_compiler.chunk.add_constant(VMValue::String(fname.clone()))?;
                    if let Some(default_expr) = default {
                        // Use SelectOrDefault.
                        func_compiler.emit(OpCode::GetLocal);
                        func_compiler.emit_u16(0); // arg attrset at slot 0
                        func_compiler.compile_expr(default_expr)?;
                        func_compiler.emit(OpCode::SelectOrDefault);
                        func_compiler.emit_u16(key_idx);
                    } else {
                        // Use GetAttr (will error if missing).
                        func_compiler.emit(OpCode::GetLocal);
                        func_compiler.emit_u16(0); // arg attrset at slot 0
                        func_compiler.emit(OpCode::GetAttr);
                        func_compiler.emit_u16(key_idx);
                    }
                    // Store into the field's local slot.
                    // Slot 0 is the arg, then fields start at slot 1
                    // (or slot 1 if there's an @-binding occupying slot 0).
                    let field_slot = func_compiler.find_local_slot(fname);
                    func_compiler.emit(OpCode::SetLocal);
                    func_compiler.emit_u16(field_slot);
                    let _ = i; // suppress unused
                }

                (1, bind_name)
            }
        };

        // Compile the body inside the function compiler.
        func_compiler.compile_expr(&body)?;
        func_compiler.emit(OpCode::Return);

        // Store the compiled function as a constant in the outer chunk.
        let closure = VMValue::Closure(VMClosure {
            chunk: Rc::new(func_compiler.chunk),
            upvalues: Vec::new(), // Phase 2: upvalue capture
            arity,
            name,
        });
        self.emit_constant(closure)
    }

    // ── Apply (function call) ──────────────────────────────────

    fn compile_apply(&mut self, app: &ast::Apply) -> Result<(), CompileError> {
        let func = app
            .lambda()
            .ok_or_else(|| CompileError::MissingNode("apply function".to_string()))?;
        let arg = app
            .argument()
            .ok_or_else(|| CompileError::MissingNode("apply argument".to_string()))?;
        // Push function, then argument, then Call.
        self.compile_expr(&func)?;
        self.compile_expr(&arg)?;
        self.emit(OpCode::Call);
        Ok(())
    }

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
                self.compile_expr(&rhs)?;
                let end_jump = self.emit_jump(OpCode::Jump);
                self.patch_jump(false_jump)?;
                self.emit(OpCode::False);
                self.patch_jump(end_jump)?;
            }
            // Short-circuit: || compiles as if/then/else
            ast::BinOpKind::Or => {
                self.compile_expr(&lhs)?;
                let true_jump = self.emit_jump(OpCode::JumpIfTrue);
                self.compile_expr(&rhs)?;
                let end_jump = self.emit_jump(OpCode::Jump);
                self.patch_jump(true_jump)?;
                self.emit(OpCode::True);
                self.patch_jump(end_jump)?;
            }
            // Short-circuit: -> is !a || b, so if lhs is false => true
            ast::BinOpKind::Implication => {
                self.compile_expr(&lhs)?;
                let false_jump = self.emit_jump(OpCode::JumpIfFalse);
                self.compile_expr(&rhs)?;
                let end_jump = self.emit_jump(OpCode::Jump);
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

    // ── Assert ─────────────────────────────────────────────────

    fn compile_assert(&mut self, assert: &ast::Assert) -> Result<(), CompileError> {
        let cond = assert
            .condition()
            .ok_or_else(|| CompileError::MissingNode("assert condition".to_string()))?;
        let body = assert
            .body()
            .ok_or_else(|| CompileError::MissingNode("assert body".to_string()))?;
        self.compile_expr(&cond)?;
        self.emit(OpCode::Assert);
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
        Ok(())
    }

    // ── Emission helpers ───────────────────────────────────────

    fn emit(&mut self, op: OpCode) {
        self.chunk.write_op(op, self.current_line);
    }

    fn emit_u16(&mut self, value: u16) {
        self.chunk.write_u16(value, self.current_line);
    }

    fn emit_constant(&mut self, value: VMValue) -> Result<(), CompileError> {
        let idx = self.chunk.add_constant(value)?;
        self.emit(OpCode::Constant);
        self.emit_u16(idx);
        Ok(())
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
            let base_slot = self.locals.len() as u16 - binding_count;
            self.emit(OpCode::SetLocal);
            self.emit_u16(base_slot);
            for _ in 0..binding_count {
                self.emit(OpCode::Pop);
            }
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
        let slot = self.locals.len() as u16;
        self.locals.push(Local {
            name,
            depth: self.scope_depth,
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

    /// Find the slot of a local by name (must exist).
    fn find_local_slot(&self, name: &str) -> u16 {
        self.resolve_local(name)
            .unwrap_or_else(|| panic!("local '{name}' not found"))
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

/// Extract a static attribute name (identifier, not dynamic/interpolated).
fn static_attr_name(attr: &ast::Attr) -> Result<String, CompileError> {
    match attr {
        ast::Attr::Ident(ident) => Ok(ident_text(ident)),
        ast::Attr::Dynamic(_) | ast::Attr::Str(_) => Err(CompileError::Unsupported(
            "dynamic or string attribute keys".to_string(),
        )),
    }
}

/// Get the source line number for an expression (approximate).
fn line_of(expr: &ast::Expr) -> u32 {
    // rnix doesn't directly expose line numbers; use the text offset
    // as an approximation. A real implementation would map offset→line.
    let offset = AstNode::syntax(expr).text_range().start();
    // Use offset as a rough line proxy.
    u32::from(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(input: &str) -> Chunk {
        Compiler::compile(input).unwrap_or_else(|e| panic!("compile failed for '{input}': {e}"))
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
        assert_eq!(chunk.code[0], OpCode::True as u8);
    }

    #[test]
    fn compile_bool_false() {
        let chunk = compile("false");
        assert_eq!(chunk.code[0], OpCode::False as u8);
    }

    #[test]
    fn compile_null() {
        let chunk = compile("null");
        assert_eq!(chunk.code[0], OpCode::Null as u8);
    }

    #[test]
    fn compile_string() {
        let chunk = compile(r#""hello""#);
        assert_eq!(chunk.constants[0], VMValue::String("hello".to_string()));
    }

    #[test]
    fn compile_addition() {
        let chunk = compile("1 + 2");
        // Should have: Constant(1), Constant(2), Add, Return
        assert!(chunk.code.contains(&(OpCode::Add as u8)));
    }

    #[test]
    fn compile_if_else() {
        let chunk = compile("if true then 1 else 2");
        assert!(chunk.code.contains(&(OpCode::JumpIfFalse as u8)));
        assert!(chunk.code.contains(&(OpCode::Jump as u8)));
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
        assert!(chunk.code.contains(&(OpCode::Negate as u8)));
    }

    #[test]
    fn compile_not() {
        let chunk = compile("!true");
        assert!(chunk.code.contains(&(OpCode::Not as u8)));
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
        assert!(chunk.code.contains(&(OpCode::Less as u8)));
    }

    #[test]
    fn compile_equality() {
        let chunk = compile("1 == 1");
        assert!(chunk.code.contains(&(OpCode::Equal as u8)));
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
        assert!(chunk.code.contains(&(OpCode::JumpIfFalse as u8)));
    }

    #[test]
    fn compile_or_short_circuit() {
        let chunk = compile("false || true");
        assert!(chunk.code.contains(&(OpCode::JumpIfTrue as u8)));
    }

    #[test]
    fn compile_has_attr() {
        let chunk = compile("{ a = 1; } ? a");
        assert!(chunk.code.contains(&(OpCode::HasAttr as u8)));
    }

    #[test]
    fn compile_select_or_default() {
        let chunk = compile("{ a = 1; }.b or 0");
        assert!(chunk.code.contains(&(OpCode::SelectOrDefault as u8)));
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
}
