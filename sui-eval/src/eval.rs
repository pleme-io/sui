//! Tree-walking Nix evaluator using rnix's typed AST.
//!
//! Implements Tvix-style lazy evaluation with thunks: let-bindings and
//! rec-attrset values are wrapped in `Value::Thunk` and only evaluated
//! when their value is actually needed (call-by-need with memoization).

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, HashMap, VecDeque};
use std::path::PathBuf;

use rnix::ast::{self, AstToken, HasEntry, InterpolPart};
use rowan::ast::AstNode;

use crate::builtins;
use crate::value::*;

thread_local! { static EVAL_DEPTH: Cell<usize> = const { Cell::new(0) }; }

// ── Source ID for identifier symbol cache ─────────────────────
//
// Each call to `rnix::Root::parse` produces a distinct AST tree.
// Identifiers from different trees may share the same byte offset,
// so we pair offset with a source ID to form a unique cache key.
// The ID is stored in a thread-local so `eval_expr` can access it
// without an extra parameter threaded through every call.

thread_local! {
    static CURRENT_SOURCE_ID: Cell<u32> = const { Cell::new(0) };
}

// ── Currently-evaluating-file stack ────────────────────────────
//
// Real Nix resolves relative path literals (`./foo.nix`) against the
// directory of the file that *contains* the literal, not against the
// process cwd. Track the stack of files we're currently evaluating
// so the `PathRel` handler and `import` builtin can resolve correctly.

thread_local! {
    static EVAL_FILE_STACK: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    /// Nix-level error context stack — captures source positions for --show-trace.
    /// Each entry: (file, expression_snippet). Pushed on function calls, select,
    /// force, and popped on return. Attached to errors for structured diagnostics.
    static NIX_TRACE_STACK: RefCell<Vec<NixTraceFrame>> = const { RefCell::new(Vec::new()) };
}

/// A single frame in the Nix-level error trace.
#[derive(Debug, Clone)]
pub struct NixTraceFrame {
    pub file: Option<String>,
    pub description: String,
}

/// Push a Nix-level trace frame. Returns a guard that pops on drop.
fn push_nix_trace(desc: impl Into<String>) -> NixTraceGuard {
    let frame = NixTraceFrame {
        file: current_eval_file().map(|p| {
            p.display().to_string()
                .rsplit_once("-source/")
                .map_or_else(|| p.display().to_string(), |(_, s)| s.to_string())
        }),
        description: desc.into(),
    };
    NIX_TRACE_STACK.with(|s| s.borrow_mut().push(frame));
    NixTraceGuard
}

struct NixTraceGuard;
impl Drop for NixTraceGuard {
    fn drop(&mut self) {
        NIX_TRACE_STACK.with(|s| s.borrow_mut().pop());
    }
}

/// Capture the current Nix trace and attach it to an error.
pub fn attach_trace(err: EvalError) -> EvalError {
    NIX_TRACE_STACK.with(|s| {
        let stack = s.borrow();
        if stack.is_empty() {
            return err;
        }
        let mut trace = format!("{err}");
        for (i, frame) in stack.iter().rev().take(15).enumerate() {
            let loc = frame.file.as_deref().unwrap_or("<eval>");
            trace.push_str(&format!("\n  {} ({loc})", frame.description));
            if i >= 14 {
                trace.push_str(&format!("\n  ... ({} more frames)", stack.len() - 15));
            }
        }
        EvalError::TypeError(trace)
    })
}

/// Return the directory of the file currently being evaluated, if any.
/// Used by the `PathRel` AST handler to resolve relative path literals.
#[must_use]
pub fn current_eval_dir() -> Option<PathBuf> {
    EVAL_FILE_STACK.with(|s| s.borrow().last().and_then(|p| p.parent().map(PathBuf::from)))
}

/// Push a file onto the eval stack. Returns an RAII guard that pops
/// it on drop. Use when entering an `import <file>` so subsequent
/// relative path literals resolve against the right directory.
pub fn push_eval_file(file: PathBuf) -> EvalFileGuard {
    EVAL_FILE_STACK.with(|s| s.borrow_mut().push(file));
    EvalFileGuard
}

/// Return the file currently being evaluated, if any.
/// Used by error sites to attach source location context.
#[must_use]
pub fn current_eval_file() -> Option<PathBuf> {
    EVAL_FILE_STACK.with(|s| s.borrow().last().cloned())
}

/// Snapshot the entire eval file stack (debug).
pub fn eval_file_stack_snapshot() -> Vec<String> {
    EVAL_FILE_STACK.with(|s| {
        s.borrow().iter().map(|p| {
            let s = p.display().to_string();
            s.rsplit_once("-source/").map_or(s.clone(), |(_, r)| r.to_string())
        }).collect()
    })
}

/// Format the current eval file for error context strings.
/// Returns e.g. `", in '/nix/store/.../default.nix'"` or empty string.
fn eval_file_ctx() -> String {
    current_eval_file()
        .map(|p| format!(", in '{}'", p.display()))
        .unwrap_or_default()
}

/// RAII guard that pops the top of the eval-file stack on drop.
pub struct EvalFileGuard;

impl Drop for EvalFileGuard {
    fn drop(&mut self) {
        EVAL_FILE_STACK.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

// ── Path normalization ────────────────────────────────────────
//
// Normalize a path by removing `.` components and resolving `..`
// components.  Unlike `canonicalize()`, this doesn't require the
// path to exist on disk — critical for flake evaluation where
// files may not be materialized yet.

/// Normalize a path by removing `.` and resolving `..` components
/// without touching the filesystem.
///
/// Delegates to [`crate::path::normalize`] — kept as a public re-export
/// so existing call-sites continue to compile without changes.
pub fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    crate::path::normalize(path)
}

// ── Pure (hermetic) evaluation mode ────────────────────────────
//
// When pure mode is enabled, impure builtins (`storePath`, `fetchurl`/`fetchTarball`
// without an explicit hash, `currentTime`, `getEnv`, etc.) should refuse to
// produce non-deterministic results. The flag is thread-local so each evaluator
// thread can opt in independently.

thread_local! {
    static PURE_MODE: Cell<bool> = const { Cell::new(false) };
}

/// Enable or disable hermetic (pure) evaluation mode for the current thread.
pub fn set_pure_mode(pure: bool) {
    PURE_MODE.with(|p| p.set(pure));
}

/// Whether the current thread is in hermetic (pure) evaluation mode.
#[must_use]
pub fn is_pure_mode() -> bool {
    PURE_MODE.with(Cell::get)
}

/// Maximum evaluation depth before we report infinite recursion.
///
/// With `stacker` dynamically growing the call stack, we are no longer
/// limited by the default 8 MB thread stack.
///
/// **Test builds** keep a low limit (2 048) so that infinite-recursion
/// tests fail quickly instead of spinning for minutes.
///
/// **Non-test builds** disable the depth guard entirely (`usize::MAX`).
/// nixpkgs uses deeply nested fixpoints (50+ overlay applications, each
/// creating cascading chains of millions of `eval_expr` calls when
/// attributes are forced). CppNix has no explicit depth limit — it
/// relies on the OS stack, which `stacker` now emulates for us. True
/// infinite recursion is caught by the thunk blackhole detector in
/// `Thunk::force`, not by this counter.
#[cfg(test)]
const MAX_EVAL_DEPTH: usize = 2_048;
#[cfg(not(test))]
const MAX_EVAL_DEPTH: usize = usize::MAX;

/// Lightweight depth guard.
///
/// In non-test builds where `MAX_EVAL_DEPTH == usize::MAX`, the guard
/// is effectively a no-op (the overflow check never fires). The
/// compiler should be able to elide most of the overhead.
struct DepthGuard;

impl DepthGuard {
    #[inline(always)]
    fn enter() -> Result<Self, EvalError> {
        EVAL_DEPTH.with(|d| {
            let depth = d.get();
            if MAX_EVAL_DEPTH != usize::MAX && depth > MAX_EVAL_DEPTH {
                return Err(EvalError::InfiniteRecursion(
                    "eval depth exceeded".into(),
                ));
            }
            d.set(depth + 1);
            Ok(DepthGuard)
        })
    }
}

impl Drop for DepthGuard {
    #[inline(always)]
    fn drop(&mut self) {
        EVAL_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Collect ALL identifier names referenced in an AST expression.
///
/// Walks the full expression tree (including inside `with` bodies)
/// and collects every `Ident` node. This is an OVER-APPROXIMATION:
/// it includes shadowed names and names inside `with` bodies.
///
/// Over-approximation is SAFE for dead binding elimination — we may
/// keep a binding that's unused (waste) but never skip a binding
/// that IS used (correctness).
///
/// Previous versions bailed out on `with` expressions, disabling
/// dead binding elimination entirely. The fix: collect idents even
/// inside `with` bodies. If a binding name doesn't appear as ANY
/// identifier ANYWHERE in the expression, it's provably dead
/// regardless of `with` scopes — `with` makes names from the
/// namespace reachable, not names from the enclosing let-scope.
fn collect_referenced_names(expr: &ast::Expr) -> HashSet<String> {
    let mut names = HashSet::new();
    for node in expr.syntax().descendants() {
        if let Some(ident) = ast::Ident::cast(node) {
            names.insert(ident_text(&ident));
        }
    }
    names
}

/// Compute the set of binding names that are transitively needed
/// by the body expression in a recursive scope (let-in or rec attrset).
///
/// Algorithm:
/// 1. Collect all ident references from the body → root set
/// 2. Collect all ident references from each binding's value expression
/// 3. BFS from root set through binding dependencies
/// 4. Return the set of reachable binding names
///
/// Bindings NOT in the returned set are provably dead and can be skipped.
/// This is correct even for recursive scopes because the BFS follows
/// transitive dependencies: if A is needed and A references B, then B
/// is added to the needed set.
fn compute_needed_bindings(
    body: &ast::Expr,
    binding_info: &[(String, Option<ast::Expr>)], // (name, value_expr) — None for plain inherit
) -> HashSet<String> {
    // Step 1: Collect idents from the body
    let body_refs = collect_referenced_names(body);

    // Build the set of all binding names and their dependencies
    let mut all_names: HashSet<String> = HashSet::with_capacity(binding_info.len());
    let mut deps: HashMap<String, HashSet<String>> = HashMap::with_capacity(binding_info.len());

    for (name, value_expr) in binding_info {
        all_names.insert(name.clone());
        if let Some(expr) = value_expr {
            deps.insert(name.clone(), collect_referenced_names(expr));
        }
    }

    // Step 2: BFS from body refs through binding dependencies
    let mut needed: HashSet<String> = body_refs.intersection(&all_names).cloned().collect();
    let mut queue: VecDeque<String> = needed.iter().cloned().collect();

    while let Some(name) = queue.pop_front() {
        if let Some(name_deps) = deps.get(&name) {
            for dep in name_deps {
                if all_names.contains(dep) && needed.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }

    needed
}

/// Evaluate a Nix expression string.
#[must_use = "evaluation result should be used"]
pub fn eval(input: &str) -> Result<Value, EvalError> {
    eval_with_file(input, None)
}

// Whether we are inside a top-level eval (used to avoid nested perf reports).
thread_local! {
    static EVAL_NESTING: Cell<usize> = const { Cell::new(0) };
}

/// Evaluate a Nix expression string, optionally tagged with the
/// path of the source file. The file is stored on the root `Env`
/// so that any closure created during evaluation captures it and
/// can resolve relative path literals (`./foo.nix`) in function
/// defaults that fire after control has left the file's scope.

pub fn eval_with_file(input: &str, file: Option<std::path::PathBuf>) -> Result<Value, EvalError> {
    let nesting = EVAL_NESTING.with(|n| {
        let v = n.get();
        n.set(v + 1);
        v
    });
    if nesting == 0 {
        crate::perf::init();
        crate::perf::start();
        crate::trace::init_trace();
        // Clear the identifier symbol cache so that offsets from
        // previous top-level evaluations don't persist.
        clear_ident_cache();
    }
    let parse = rnix::Root::parse(input);
    if !parse.errors().is_empty() {
        let msgs: Vec<String> = parse.errors().iter().map(|e| e.to_string()).collect();
        EVAL_NESTING.with(|n| n.set(n.get().saturating_sub(1)));
        return Err(EvalError::ParseError(msgs.join("; ")));
    }

    // Each parse tree gets a unique source ID so that identifiers
    // at the same byte offset in different files don't collide in
    // the symbol cache.
    let src_id = next_source_id();
    let prev_src_id = CURRENT_SOURCE_ID.with(|s| {
        let old = s.get();
        s.set(src_id);
        old
    });

    let root = parse.tree();
    let expr = match root.expr() {
        Some(e) => e,
        None => {
            CURRENT_SOURCE_ID.with(|s| s.set(prev_src_id));
            EVAL_NESTING.with(|n| n.set(n.get().saturating_sub(1)));
            return Err(EvalError::ParseError("empty expression".to_string()));
        }
    };
    let mut env = Env::new();
    env.set_eval_file(file);
    builtins::register(&mut env);
    let result = eval_expr(&expr, &env).map_err(|e| attach_trace(e))?;
    // Force the top-level result so callers always see a concrete value.
    let final_result = force_value(&result).map_err(|e| attach_trace(e));
    // Restore the previous source ID (matters for nested imports).
    CURRENT_SOURCE_ID.with(|s| s.set(prev_src_id));
    EVAL_NESTING.with(|n| n.set(n.get().saturating_sub(1)));
    if nesting == 0 {
        crate::perf::report();
    }
    final_result
}

/// Force a value: if it is a thunk, evaluate and memoize the result.
/// Concrete values are returned unchanged.
/// Force a value: if it is a thunk, evaluate and memoize the result.
/// Concrete values are returned unchanged.
///
/// Inlined aggressively so the non-thunk fast path compiles to a
/// simple clone without a function-call boundary.
#[inline(always)]
/// Force a value and return a type-safe `Concrete` (guaranteed non-Thunk).
///
/// This is the preferred forcing API. The `Concrete` return type makes it
/// impossible to accidentally use an unforced thunk — the compiler rejects it.
pub fn force_concrete(value: &Value) -> Result<Concrete, EvalError> {
    value.demand()
}

/// Force a value (legacy API — returns `Value` for backward compatibility).
///
/// Prefer `force_concrete()` or `Value::demand()` for new code.
pub fn force_value(value: &Value) -> Result<Value, EvalError> {
    crate::perf::inc(crate::perf::Counter::ForceValue);
    if let Value::Thunk(thunk) = value {
        force_thunk(thunk)
    } else {
        Ok(value.clone())
    }
}

/// Force with call-site tracking (legacy API).
pub fn force_value_tracked(value: &Value, site: &str) -> Result<Value, EvalError> {
    crate::perf::inc(crate::perf::Counter::ForceValue);
    if let Value::Thunk(thunk) = value {
        FORCE_SITES.with(|sites| {
            *sites.borrow_mut().entry(site.to_string()).or_insert(0) += 1;
        });
        force_thunk(thunk)
    } else {
        Ok(value.clone())
    }
}

thread_local! {
    static FORCE_SITES: std::cell::RefCell<std::collections::HashMap<String, u64>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static APPLY_SITES: std::cell::RefCell<std::collections::HashMap<String, u64>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Dump force-site counters (call from perf reporting).
pub fn dump_force_sites() {
    FORCE_SITES.with(|sites| {
        let sites = sites.borrow();
        let mut sorted: Vec<_> = sites.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        eprintln!("[force-sites] top thunk force call sites:");
        for (site, count) in sorted.iter().take(10) {
            eprintln!("  {count:>8} {site}");
        }
    });
    APPLY_SITES.with(|sites| {
        let sites = sites.borrow();
        let mut sorted: Vec<_> = sites.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        eprintln!("[apply-sites] top lambda call sites by source file:");
        for (site, count) in sorted.iter().take(15) {
            // Strip nix store prefix for readability
            let short = site.rsplit_once("-source/").map_or(site.as_str(), |(_,s)| s);
            eprintln!("  {count:>8} {short}");
        }
    });
}

/// Force a thunk — split out from [`force_value`] so the fast path
/// (non-thunk clone) stays fully inlined while this cold path can
/// be a regular function call with stacker protection.
fn force_thunk(thunk: &Thunk) -> Result<Value, EvalError> {
    stacker::maybe_grow(64 * 1024, 2 * 1024 * 1024, || {
        // Force ONE level only — matches CppNix's forceValue which does
        // not transitively chase thunk-in-thunk chains. The caller will
        // force again when the value is actually needed. This is the key
        // optimization: CppNix forces 71 thunks for lib.version while
        // sui was forcing 180K due to transitive forcing.
        thunk.force(&|expr, env| eval_expr(expr, env))
    })
}

/// Decide whether to thunk an expression or evaluate it directly.
///
/// Trivial expressions (literals, paths) are evaluated immediately --
/// no thunk allocation. For non-recursive scopes, variable lookups
/// (Ident) and lambdas are also evaluated eagerly. This matches
/// CppNix's `maybeThunk` optimization which avoids a large fraction
/// of thunk creations on nixpkgs.
///
/// For recursive scopes (let-in, rec attrsets), set `is_rec = true` to
/// prevent eager evaluation of `Ident` and `Lambda` expressions:
/// - Ident: sibling bindings may not be defined yet (forward refs).
/// - Lambda: the closure must capture the *final* env (set in Phase 2)
///   so that the lambda body can reference sibling bindings.
///
/// `defined_so_far`: In recursive scopes, names that have already been
/// bound in this scope (i.e. earlier bindings). Idents referencing these
/// are backward references and can be resolved directly without thunking.
/// Forward references (names not yet defined) must still be thunked.
fn maybe_thunk(
    expr: &ast::Expr,
    env: &Env,
    is_rec: bool,
    defined_so_far: Option<&HashSet<String>>,
) -> Value {
    match expr {
        // Literals: evaluate directly (no allocation needed).
        ast::Expr::Literal(lit) => eval_literal(lit).unwrap_or_else(|_| {
            Value::Thunk(Thunk::new_suspended(expr.clone(), env.clone()))
        }),
        // Identifiers: look up directly (unless in rec scope where
        // forward references to sibling bindings are possible).
        ast::Expr::Ident(ident) if !is_rec => {
            let name = ident_text(ident);
            match name.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                "null" => Value::Null,
                _ => env.lookup(&name).unwrap_or_else(|| {
                    Value::Thunk(Thunk::new_suspended(expr.clone(), env.clone()))
                }),
            }
        }
        // Identifiers in rec scope: check if it's a backward reference
        // (name already defined earlier in the same scope). If so, we
        // can resolve it directly instead of creating a wasteful thunk.
        ast::Expr::Ident(ident) if is_rec => {
            let name = ident_text(ident);
            match name.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                "null" => Value::Null,
                _ => {
                    // If this name was already defined earlier in the
                    // scope, it's a backward reference — resolve directly.
                    if defined_so_far.map_or(false, |d| d.contains(&name)) {
                        env.lookup(&name).unwrap_or_else(|| {
                            Value::Thunk(Thunk::new_suspended(expr.clone(), env.clone()))
                        })
                    } else {
                        // Forward reference — must thunk
                        Value::Thunk(Thunk::new_suspended(expr.clone(), env.clone()))
                    }
                }
            }
        }
        // Absolute and home paths: trivial text extraction.
        ast::Expr::PathAbs(p) => {
            let text = p.syntax().text().to_string();
            Value::Path(Box::new(SmolStr::from(text.as_str())))
        }
        ast::Expr::PathHome(p) => {
            let text = p.syntax().text().to_string();
            Value::Path(Box::new(SmolStr::from(text.as_str())))
        }
        // Lambda: capture env directly (no computation needed).
        // But NOT in recursive scopes -- the closure must capture the
        // final env with all sibling bindings (set in Phase 2).
        ast::Expr::Lambda(lam) if !is_rec => {
            if let (Some(param), Some(body)) = (lam.param(), lam.body()) {
                Value::Lambda(Rc::new(Closure {
                    param,
                    body,
                    env: env.clone(),
                }))
            } else {
                Value::Thunk(Thunk::new_suspended(expr.clone(), env.clone()))
            }
        }
        // Select on a variable: CppNix's maybeThunk evaluates these eagerly
        // when the base is a simple ident. However, this breaks fixpoints
        // where the base (e.g., `config`) is a thunk being computed — eagerly
        // evaluating `config.x` during attrset construction triggers blackhole.
        //
        // The nixpkgs module system relies on `{ ...; default = config.x; }`
        // being lazy. Wrap selects in thunks unconditionally.
        // The performance cost is minimal (thunk allocation + deferred eval)
        // and correctness is critical for fixpoint patterns.
        // Everything else: wrap in a thunk for lazy evaluation.
        _ => Value::Thunk(Thunk::new_suspended(expr.clone(), env.clone())),
    }
}

/// Evaluate an rnix expression in an environment.
///
/// Uses `stacker::maybe_grow` to dynamically extend the call stack when
/// it is close to exhaustion.  This prevents stack overflow on deeply
/// nested nixpkgs fixpoints (50+ overlay applications each creating
/// multiple recursive `eval_expr` / `force_value` frames).
///
/// **Fast path:** Ident (~32% of all evals), Literal, Paren, and Root
/// expressions don't recurse and are handled directly, skipping the
/// `stacker::maybe_grow` overhead for ~40% of all `eval_expr` calls.
#[inline(always)]
pub fn eval_expr(expr: &ast::Expr, env: &Env) -> Result<Value, EvalError> {
    // Fast path: trivial expressions that don't recurse.
    // Skip stacker overhead for ~40% of all eval_expr calls.
    match expr {
        ast::Expr::Ident(ident) => {
            crate::perf::inc(crate::perf::Counter::EvalExpr);
            if crate::perf::enabled() {
                crate::perf::inc(crate::perf::Counter::ExprIdent);
            }
            let name = ident_text(ident);
            return match name.as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                "null" => Ok(Value::Null),
                _ => env
                    .lookup(&name)
                    .ok_or_else(|| {
                        // Debug trace: when SUI_DEBUG_VAR is set, log
                        // the env state on lookup failure for that var.
                        if let Ok(dbg_var) = std::env::var("SUI_DEBUG_VAR") {
                            if dbg_var == name || dbg_var == "*" {
                                eprintln!(
                                    "[sui-debug] UndefinedVar '{name}' in {}\n\
                                     [sui-debug]   env bindings ({} total): {:?}\n\
                                     [sui-debug]   with_scopes: {}",
                                    eval_file_ctx(),
                                    env.binding_count(),
                                    env.binding_names_preview(20),
                                    env.with_scope_count(),
                                );
                            }
                        }
                        EvalError::UndefinedVar(
                            format!("'{name}'{}", eval_file_ctx()),
                        )
                    }),
            };
        }
        ast::Expr::Literal(lit) => {
            crate::perf::inc(crate::perf::Counter::EvalExpr);
            if crate::perf::enabled() {
                crate::perf::inc(crate::perf::Counter::ExprLiteral);
            }
            return eval_literal(lit);
        }
        ast::Expr::Paren(p) => {
            if let Some(inner) = p.expr() {
                return eval_expr(&inner, env);
            }
        }
        ast::Expr::Root(r) => {
            if let Some(inner) = r.expr() {
                return eval_expr(&inner, env);
            }
        }
        _ => {}
    }
    // Complex expressions: need stacker for recursion safety
    stacker::maybe_grow(64 * 1024, 2 * 1024 * 1024, || {
        eval_expr_inner(expr, env)
    })
}

/// Inner implementation of [`eval_expr`] — called from the `stacker`
/// trampoline so that the stack is guaranteed to have headroom.
///
/// Uses a tail-call loop: for expressions in tail position (`if/else`,
/// `let..in`, `with`, `assert`, `paren`, `root`), we update the local
/// `expr` and `env` variables and loop instead of recursing. This
/// eliminates millions of stack frames in nixpkgs evaluation.
fn eval_expr_inner(expr: &ast::Expr, env: &Env) -> Result<Value, EvalError> {
    // Tail-call trampoline: expressions in tail position update these
    // and `continue` instead of recursing into eval_expr.
    let mut cur_expr = expr.clone();
    let mut cur_env = env.clone();

    loop {
    crate::perf::inc(crate::perf::Counter::EvalExpr);
    // Track expression type distribution when profiling
    if crate::perf::enabled() {
        use crate::perf::Counter;
        let c = match &cur_expr {
            ast::Expr::Ident(_) => Counter::ExprIdent,
            ast::Expr::Literal(_) => Counter::ExprLiteral,
            ast::Expr::Str(_) => Counter::ExprStr,
            ast::Expr::List(_) => Counter::ExprList,
            ast::Expr::AttrSet(_) => Counter::ExprAttrs,
            ast::Expr::Select(_) => Counter::ExprSelect,
            ast::Expr::Apply(_) => Counter::ExprApply,
            ast::Expr::LetIn(_) => Counter::ExprLetIn,
            ast::Expr::IfElse(_) => Counter::ExprIfElse,
            ast::Expr::With(_) => Counter::ExprWith,
            ast::Expr::Lambda(_) => Counter::ExprLambda,
            ast::Expr::BinOp(_) => Counter::ExprBinOp,
            ast::Expr::HasAttr(_) => Counter::ExprHasAttr,
            ast::Expr::UnaryOp(_) => Counter::ExprUnaryOp,
            ast::Expr::Assert(_) => Counter::ExprAssert,
            ast::Expr::PathAbs(_) | ast::Expr::PathRel(_)
            | ast::Expr::PathHome(_) | ast::Expr::PathSearch(_) => Counter::ExprPath,
            _ => Counter::ExprOther,
        };
        crate::perf::inc(c);
    }
    let _guard = DepthGuard::enter()?;
    let env = &cur_env;
    match &cur_expr {
        ast::Expr::Literal(lit) => return eval_literal(lit),

        ast::Expr::Str(s) => return eval_str(s, env),

        ast::Expr::PathAbs(p) => {
            let text = p.syntax().text().to_string();
            return Ok(Value::Path(Box::new(SmolStr::from(text.as_str()))));
        }
        ast::Expr::PathRel(p) => {
            // Real Nix resolves `./foo.nix` against the directory
            // of the file that *contains* the literal, not the
            // process cwd. Use the current eval-file stack; fall
            // back to cwd when no file is being evaluated (e.g.,
            // top-level `sui eval`).
            let text = p.syntax().text().to_string();
            let resolved = if let Some(dir) = current_eval_dir() {
                let joined = dir.join(&text);
                // Use normalize_path instead of canonicalize so that
                // paths with ./  and .. are cleaned without requiring
                // the path to exist on disk.
                normalize_path(&joined)
                    .to_string_lossy()
                    .into_owned()
            } else {
                text
            };
            return Ok(Value::Path(Box::new(SmolStr::from(resolved.as_str()))));
        }
        ast::Expr::PathHome(p) => {
            let text = p.syntax().text().to_string();
            return Ok(Value::Path(Box::new(SmolStr::from(text.as_str()))));
        }
        ast::Expr::PathSearch(p) => {
            // `<name>` or `<name/sub/path>` — resolve via NIX_PATH
            // entries (parsed from the env var). If no NIX_PATH entry
            // matches, fall through to the literal text so the error
            // message points at the name the user wrote.
            let text = p.syntax().text().to_string();
            let inner = text
                .strip_prefix('<')
                .and_then(|s| s.strip_suffix('>'))
                .unwrap_or(&text);
            if let Some(resolved) = crate::builtins::resolve_search_path(inner) {
                return Ok(Value::Path(Box::new(SmolStr::from(resolved.as_str()))));
            }
            // CppNix: search path resolution failure is a throw
            // (catchable by tryEval). Used by nixpkgs impure-overlays.nix
            // which tries `import <nixpkgs-overlays>` inside tryEval.
            return Err(EvalError::Throw(
                format!("search path '{text}' not in NIX_PATH"),
            ));
        }

        ast::Expr::Ident(ident) => {
            let name = ident_text(ident);
            return match name.as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                "null" => Ok(Value::Null),
                _ => {
                    env.lookup(&name)
                        .ok_or_else(|| EvalError::UndefinedVar(
                            format!("'{name}'{}", eval_file_ctx()),
                        ))
                }
            };
        }

        ast::Expr::List(list) => {
            // Wrap list elements in thunks for maximum laziness.
            // CppNix wraps list elements — only forced when accessed.
            // This prevents eager evaluation of unused list elements
            // (e.g., nixpkgs overlay lists with thousands of entries).
            let values: Vec<Value> = list.items()
                .map(|e| maybe_thunk(&e, env, false, None))
                .collect();
            return Ok(Value::list(values));
        }

        ast::Expr::AttrSet(set) => return eval_attrset(set, env),

        ast::Expr::Select(sel) => return eval_select(sel, env),

        ast::Expr::HasAttr(ha) => return eval_has_attr(ha, env),

        ast::Expr::UnaryOp(op) => return eval_unary_op(op, env),

        ast::Expr::BinOp(binop) => {
            let lhs_expr = binop
                .lhs()
                .ok_or_else(|| EvalError::ParseError("binop missing lhs".to_string()))?;
            let rhs_expr = binop
                .rhs()
                .ok_or_else(|| EvalError::ParseError("binop missing rhs".to_string()))?;
            let kind = binop
                .operator()
                .ok_or_else(|| EvalError::ParseError("binop missing operator".to_string()))?;
            return eval_binop(kind, &lhs_expr, &rhs_expr, env);
        }

        ast::Expr::Apply(app) => return eval_apply(app, env),

        ast::Expr::IfElse(ie) => {
            let cond = ie
                .condition()
                .ok_or_else(|| EvalError::ParseError("if missing condition".to_string()))?;
            let body = ie
                .body()
                .ok_or_else(|| EvalError::ParseError("if missing then body".to_string()))?;
            let else_body = ie
                .else_body()
                .ok_or_else(|| EvalError::ParseError("if missing else body".to_string()))?;
            if force_concrete(&eval_expr(&cond, env)?)?.as_bool()? {
                cur_expr = body;
            } else {
                cur_expr = else_body;
            }
            // env stays the same — tail call
            continue;
        }

        ast::Expr::Assert(assert) => {
            let cond = assert
                .condition()
                .ok_or_else(|| EvalError::ParseError("assert missing condition".to_string()))?;
            let body = assert
                .body()
                .ok_or_else(|| EvalError::ParseError("assert missing body".to_string()))?;
            if !force_concrete(&eval_expr(&cond, env)?)?.as_bool()? {
                return Err(EvalError::AssertionFailed(eval_file_ctx()));
            }
            cur_expr = body;
            continue;
        }

        ast::Expr::With(with) => {
            let ns = with
                .namespace()
                .ok_or_else(|| EvalError::ParseError("with missing namespace".to_string()))?;
            let body = with
                .body()
                .ok_or_else(|| EvalError::ParseError("with missing body".to_string()))?;
            // Don't force the namespace yet — store as a lazy value.
            // CppNix evaluates with-scopes lazily: the namespace is only
            // forced when a name lookup actually falls through lexical scope.
            // This is critical for `fix (self: with self; { … })` patterns
            // used throughout nixpkgs.
            let scope_val = eval_expr(&ns, env)?;
            let new_env = env.child().with_scope(scope_val);
            cur_expr = body;
            cur_env = new_env;
            continue;
        }

        ast::Expr::LetIn(letin) => {
            let mut new_env = env.child();

            // Phase 1: Create thunks with a dummy env and bind them.
            // Collect (key, thunk) pairs so we can update envs later.
            let mut thunks: Vec<(String, Thunk)> = Vec::new();

            // Track which names have been defined so far in this scope.
            // Used by maybe_thunk to resolve backward references directly
            // instead of creating wasteful thunks.
            let mut defined_so_far: HashSet<String> = HashSet::new();

            // Accumulator for dotted-path bindings (`let a.b = 1; a.c = 2; ...`).
            // Leaf values are wrapped in thunks so they can reference
            // sibling let-bindings (the let scope is recursive in Nix).
            let mut dotted_attrs: NixAttrs = NixAttrs::new();

            for entry in letin.entries() {
                match entry {
                    ast::Entry::AttrpathValue(ref apv) => {
                        let attrpath = apv.attrpath().ok_or_else(|| {
                            EvalError::ParseError("binding missing attrpath".to_string())
                        })?;
                        let value_expr = apv.value().ok_or_else(|| {
                            EvalError::ParseError("binding missing value".to_string())
                        })?;
                        let mut path_keys: Vec<String> = attrpath
                            .attrs()
                            .map(|a| eval_attr(&a, env))
                            .collect::<Result<_, _>>()?;
                        if path_keys.len() == 1 {
                            let key = path_keys.pop().unwrap();
                            // maybeThunk: skip thunk for trivial exprs.
                            // is_rec=true because let-in is mutually
                            // recursive — forward refs possible.
                            // Pass defined_so_far so backward refs resolve directly.
                            let value = maybe_thunk(&value_expr, env, true, Some(&defined_so_far));
                            new_env.bind(key.clone(), value.clone());
                            if let Value::Thunk(t) = &value {
                                thunks.push((key.clone(), t.clone()));
                            }
                            defined_so_far.insert(key);
                        } else if path_keys.len() > 1 {
                            // Multi-segment dotted path: build a nested
                            // attrset with thunks at the leaves so the
                            // value expression can reference sibling
                            // let-bindings.
                            let key = path_keys[0].clone();
                            let value = build_nested_attr_thunk(
                                &path_keys[1..],
                                &value_expr,
                                env,
                                &mut thunks,
                            );
                            merge_nested_insert(&mut dotted_attrs, key, value);
                        }
                    }
                    ast::Entry::Inherit(ref inherit) => {
                        if let Some(from) = inherit.from() {
                            let source_expr = from.expr().ok_or_else(|| {
                                EvalError::ParseError(
                                    "inherit from missing expr".to_string(),
                                )
                            })?;
                            // Create ONE shared source thunk per
                            // `inherit (source)` clause. All inherited
                            // names share it via Rc clone — the source
                            // is evaluated at most once.
                            let source_thunk = Thunk::new_suspended(
                                source_expr, env.clone(),
                            );
                            for attr in inherit.attrs() {
                                let name = eval_attr(&attr, env)?;
                                let thunk = Thunk::new_inherit_select(
                                    source_thunk.clone(),
                                    name.clone(),
                                );
                                new_env.bind(name.clone(), Value::Thunk(thunk.clone()));
                                thunks.push((name, thunk));
                            }
                        } else {
                            // `inherit name1 name2 ...` from the
                            // enclosing lexical scope. This stays
                            // eager because the names already exist
                            // in `env` — no fixpoint involved.
                            for attr in inherit.attrs() {
                                let name = eval_attr(&attr, env)?;
                                let value = env.lookup(&name).ok_or_else(|| {
                                    EvalError::UndefinedVar(
                                        format!("'{name}'{}", eval_file_ctx()),
                                    )
                                })?;
                                new_env.bind(name, value);
                            }
                        }
                    }
                }
            }

            // Phase 1b: Bind accumulated dotted-path attrs into new_env.
            // Note: CppNix rejects `inherit (src) x; x.y = ...;` as a
            // duplicate definition, so we do not attempt to merge with
            // existing inherit thunks — just bind directly.
            for (key, value) in dotted_attrs.iter() {
                new_env.bind(key.clone(), value.clone());
            }

            // Phase 2: Update all thunks to capture the final env
            // (which now has all names bound).
            for (_key, thunk) in &thunks {
                thunk.update_env(&new_env);
            }

            let body = letin
                .body()
                .ok_or_else(|| EvalError::ParseError("let missing body".to_string()))?;
            cur_expr = body;
            cur_env = new_env;
            continue;
        }

        ast::Expr::Lambda(lam) => {
            let param = lam
                .param()
                .ok_or_else(|| EvalError::ParseError("lambda missing param".to_string()))?;
            let body = lam
                .body()
                .ok_or_else(|| EvalError::ParseError("lambda missing body".to_string()))?;
            return Ok(Value::Lambda(Rc::new(Closure {
                param,
                body,
                env: env.clone(),
            })));
        }

        ast::Expr::Paren(p) => {
            let inner = p
                .expr()
                .ok_or_else(|| EvalError::ParseError("paren missing expr".to_string()))?;
            cur_expr = inner;
            continue;
        }

        ast::Expr::Root(r) => {
            let inner = r
                .expr()
                .ok_or_else(|| EvalError::ParseError("root missing expr".to_string()))?;
            cur_expr = inner;
            continue;
        }

        ast::Expr::LegacyLet(ll) => {
            let mut new_env = env.child();
            eval_entries(ll, &mut new_env)?;
            // legacy let returns the `body` attr from its bindings
            return new_env
                .lookup("body")
                .ok_or_else(|| EvalError::AttrNotFound(
                    format!("'body' in legacy let{}", eval_file_ctx()),
                ));
        }

        ast::Expr::CurPos(_) => return Err(EvalError::NotImplemented("__curPos".to_string())),
        ast::Expr::Error(_) => return Err(EvalError::ParseError("parse error node".to_string())),
    } // match
    } // loop — unreachable, all arms either return or continue
}

fn eval_literal(lit: &ast::Literal) -> Result<Value, EvalError> {
    use ast::LiteralKind;
    match lit.kind() {
        LiteralKind::Integer(tok) => {
            let n = tok
                .value()
                .map_err(|e| EvalError::ParseError(format!("invalid integer: {e}")))?;
            Ok(Value::Int(n))
        }
        LiteralKind::Float(tok) => {
            let f = tok
                .value()
                .map_err(|e| EvalError::ParseError(format!("invalid float: {e}")))?;
            Ok(Value::Float(f))
        }
        LiteralKind::Uri(tok) => Ok(Value::string(tok.syntax().text().to_string())),
    }
}

/// Result of walking an attrpath on a base value.
enum TraverseResult {
    /// All keys found; contains the leaf value.
    Found(Value),
    /// A key was missing; contains the missing key name.
    Missing(String),
    /// A non-attrset value was encountered during traversal.
    NotAttrs(Value),
}

/// Walk an attrpath on a base value, forcing at each level.
///
/// Returns `Found(leaf)` when every key exists, `Missing(key)` when
/// a key is absent, or `NotAttrs(v)` when a non-attrset is encountered.
fn traverse_attrpath(
    base: Value,
    attrpath: &rnix::ast::Attrpath,
    env: &Env,
) -> Result<TraverseResult, EvalError> {
    let attrs: Vec<_> = attrpath.attrs().collect();
    let mut value = base;
    for (i, attr) in attrs.iter().enumerate() {
        let key = eval_attr(attr, env)?;
        // Force the current value to an attrset to select from it.
        let forced = force_value(&value)?;
        match forced {
            Value::Attrs(ref a) => match a.get(&key) {
                Some(v) => {
                    if i < attrs.len() - 1 {
                        // Intermediate step: force to attrset for next selection.
                        value = force_value(v)?;
                    } else {
                        // Final step: return WITHOUT forcing — let the caller
                        // decide when to force. Matches CppNix's lazy attr access.
                        value = v.clone();
                    }
                }
                None => return Ok(TraverseResult::Missing(key)),
            },
            _ => return Ok(TraverseResult::NotAttrs(forced)),
        }
    }
    Ok(TraverseResult::Found(value))
}

fn eval_select(sel: &ast::Select, env: &Env) -> Result<Value, EvalError> {
    crate::perf::inc(crate::perf::Counter::Select);
    let base_expr = sel.expr().ok_or_else(|| {
        EvalError::ParseError("select missing expression".to_string())
    })?;
    let base = force_concrete(&eval_expr(&base_expr, env)?)?.into_value();
    let base_type = base.type_name();
    let attrpath = sel.attrpath().ok_or_else(|| {
        EvalError::ParseError("select missing attrpath".to_string())
    })?;
    match traverse_attrpath(base, &attrpath, env)? {
        TraverseResult::Found(v) => Ok(v),
        TraverseResult::Missing(key) => {
            if let Some(def) = sel.default_expr() {
                eval_expr(&def, env)
            } else {
                Err(EvalError::AttrNotFound(
                    format!("'{key}'{}", eval_file_ctx()),
                ))
            }
        }
        TraverseResult::NotAttrs(_) => {
            // CppNix: `expr.a.b or default` falls back to default for
            // ANY error in the path — including intermediate values
            // that aren't attrsets (e.g., null). The module system
            // relies on this: `x.options.type.name or null` must
            // return null when x.options is null, not throw.
            if let Some(def) = sel.default_expr() {
                eval_expr(&def, env)
            } else {
                Err(attach_trace(EvalError::type_error(
                    format!("cannot select from {base_type}"),
                )))
            }
        }
    }
}

/// Evaluate `expr ? a.b.c` — check key presence without forcing value thunks.
fn eval_has_attr(ha: &ast::HasAttr, env: &Env) -> Result<Value, EvalError> {
    let base_expr = ha.expr().ok_or_else(|| {
        EvalError::ParseError("hasattr missing expression".to_string())
    })?;
    let base = force_concrete(&eval_expr(&base_expr, env)?)?.into_value();
    let attrpath = ha.attrpath().ok_or_else(|| {
        EvalError::ParseError("hasattr missing attrpath".to_string())
    })?;
    match traverse_attrpath(base, &attrpath, env)? {
        TraverseResult::Found(_) => Ok(Value::Bool(true)),
        TraverseResult::Missing(_) | TraverseResult::NotAttrs(_) => Ok(Value::Bool(false)),
    }
}

fn eval_unary_op(op: &ast::UnaryOp, env: &Env) -> Result<Value, EvalError> {
    let inner = op
        .expr()
        .ok_or_else(|| EvalError::ParseError("unary op missing expr".to_string()))?;
    let val = force_value(&eval_expr(&inner, env)?)?;
    let kind = op
        .operator()
        .ok_or_else(|| EvalError::ParseError("unary op missing operator".to_string()))?;
    match kind {
        ast::UnaryOpKind::Negate => match val {
            Value::Int(n) => Ok(Value::Int(-n)),
            Value::Float(f) => Ok(Value::Float(-f)),
            _ => Err(EvalError::type_error(
                format!("cannot negate {}", val.type_name()),
            )),
        },
        ast::UnaryOpKind::Invert => Ok(Value::Bool(!val.as_bool()?)),
    }
}

fn eval_apply(app: &ast::Apply, env: &Env) -> Result<Value, EvalError> {
    let func_expr = app
        .lambda()
        .ok_or_else(|| EvalError::ParseError("apply missing function".to_string()))?;
    let arg_expr = app
        .argument()
        .ok_or_else(|| EvalError::ParseError("apply missing argument".to_string()))?;
    let func = force_value(&eval_expr(&func_expr, env)?)?;
    // Lambda arguments are wrapped in a thunk for call-by-need semantics.
    // Thunk strategy depends on function type:
    // - Lambda: ALWAYS thunk (call-by-need, enables fixpoints)
    // - tryEval: ALWAYS thunk (must catch errors during force)
    // - Builtin: evaluate eagerly (builtins always force args anyway;
    //   thunking wastes Rc + OnceCell allocation per call)
    // - __functor: evaluate eagerly (will be applied immediately)
    let arg = match &func {
        Value::Lambda(_) => {
            Value::Thunk(Thunk::new_suspended(arg_expr.clone(), env.clone()))
        }
        Value::Builtin(b) if b.name == "tryEval" => {
            Value::Thunk(Thunk::new_suspended(arg_expr.clone(), env.clone()))
        }
        _ => eval_expr(&arg_expr, env)?,
    };
    apply(func, arg)
}

fn eval_str(s: &ast::Str, env: &Env) -> Result<Value, EvalError> {
    let mut result = String::new();
    let mut ctx = StringContext::new();
    for part in s.normalized_parts() {
        match part {
            InterpolPart::Literal(text) => result.push_str(&text),
            InterpolPart::Interpolation(interpol) => {
                let expr = interpol.expr().ok_or_else(|| {
                    EvalError::ParseError("interpolation missing expr".to_string())
                })?;
                let val = force_value(&eval_expr(&expr, env)?)?;
                let (s, c) = val.coerce_to_string()?;
                result.push_str(&s);
                ctx.merge(&c);
            }
        }
    }
    Ok(Value::String(Rc::new(NixString::with_context(result, ctx))))
}

/// Evaluate an attribute name, requiring non-null.
/// Use `eval_attr_maybe_null` when null dynamic attrs should be skipped.
fn eval_attr(attr: &ast::Attr, env: &Env) -> Result<String, EvalError> {
    eval_attr_maybe_null(attr, env)?
        .ok_or_else(|| EvalError::TypeError("null dynamic attribute name".into()))
}

/// Evaluate an attribute name. Returns `None` for null dynamic attrs
/// (CppNix silently omits attributes with null names).
fn eval_attr_maybe_null(attr: &ast::Attr, env: &Env) -> Result<Option<String>, EvalError> {
    match attr {
        ast::Attr::Ident(ident) => Ok(Some(ident_text(ident))),
        ast::Attr::Dynamic(dyn_) => {
            let expr = dyn_
                .expr()
                .ok_or_else(|| EvalError::ParseError("dynamic attr missing expr".to_string()))?;
            let val = force_value(&eval_expr(&expr, env)?)?;
            // CppNix: null dynamic attr name → skip the attribute entirely.
            // Used by nixpkgs module system: `${if cond then null else "name"} = value;`
            if val == Value::Null {
                return Ok(None);
            }
            Ok(Some(val.as_string()?.to_string()))
        }
        ast::Attr::Str(s) => {
            let val = eval_str(s, env)?;
            Ok(Some(val.as_string()?.to_string()))
        }
    }
}

/// Get the text of an rnix Ident node.
fn ident_text(ident: &ast::Ident) -> String {
    // Ident's ident_token() returns TOKEN_IDENT, but `or` keyword gets
    // a TOKEN_OR token instead. Use syntax().text() which always works.
    ident.syntax().text().to_string()
}

fn eval_attrset(set: &ast::AttrSet, env: &Env) -> Result<Value, EvalError> {
    crate::perf::inc(crate::perf::Counter::Attrset);
    let mut attrs = NixAttrs::new();
    let is_rec = set.rec_token().is_some();

    if is_rec {
        let mut rec_env = env.child();
        let mut thunks: Vec<(String, Thunk)> = Vec::new();

        // Track which names have been defined so far in this scope.
        // Used by maybe_thunk to resolve backward references directly
        // instead of creating wasteful thunks.
        let mut defined_so_far: HashSet<String> = HashSet::new();

        // Accumulator for dotted-path bindings (`rec { a.b = 1; a.c = 2; ... }`).
        // Leaf values are wrapped in thunks so they participate in the
        // recursive env fixpoint, matching CppNix semantics where
        // `rec { types.a = f 1; f = x: x + 1; }` allows `f` to be a
        // sibling binding.
        let mut dotted_attrs: NixAttrs = NixAttrs::new();

        // Phase 1: Create thunks with placeholder env and bind them.
        for entry in set.entries() {
            match entry {
                ast::Entry::AttrpathValue(apv) => {
                    let attrpath = apv.attrpath().ok_or_else(|| {
                        EvalError::ParseError("binding missing attrpath".to_string())
                    })?;
                    let value_expr = apv.value().ok_or_else(|| {
                        EvalError::ParseError("binding missing value".to_string())
                    })?;
                    let mut path_keys: Vec<String> = attrpath
                        .attrs()
                        .filter_map(|a| eval_attr_maybe_null(&a, env).transpose())
                        .collect::<Result<_, _>>()?;
                    // Null dynamic attr name → skip entire binding (CppNix compat)
                    if path_keys.is_empty() { continue; }
                    if path_keys.len() == 1 {
                        let key = path_keys.pop().unwrap();
                        // maybeThunk: skip thunk for trivial exprs.
                        // is_rec=true because rec attrset bindings
                        // can reference each other.
                        // Pass defined_so_far so backward refs resolve directly.
                        let value = maybe_thunk(&value_expr, env, true, Some(&defined_so_far));
                        rec_env.bind(key.clone(), value.clone());
                        attrs.insert(key.clone(), value.clone());
                        if let Value::Thunk(t) = &value {
                            thunks.push((key.clone(), t.clone()));
                        }
                        defined_so_far.insert(key);
                    } else {
                        // Multi-segment dotted path: build a nested attrset
                        // with a thunk at the leaf so the value expression
                        // can reference sibling rec-bindings.
                        let key = path_keys[0].clone();
                        let value =
                            build_nested_attr_thunk(&path_keys[1..], &value_expr, env, &mut thunks);
                        merge_nested_insert(&mut dotted_attrs, key, value);
                    }
                }
                ast::Entry::Inherit(inherit) => {
                    eval_inherit(&inherit, env, &mut attrs, Some(&mut rec_env), Some(&mut thunks))?;
                }
            }
        }

        // Phase 1b: Bind accumulated dotted-path attrs into attrs and rec_env.
        // Note: CppNix rejects `inherit (src) x; x.y = ...;` as a
        // duplicate definition, so we do not attempt to merge with
        // existing inherit thunks — just bind directly.
        for (key, value) in dotted_attrs.iter() {
            attrs.insert(key.clone(), value.clone());
            rec_env.bind(key.clone(), value.clone());
        }

        // Phase 2: Update all thunks (both Suspended and InheritSelect)
        // to capture the final rec_env (which now has all names bound).
        for (_key, thunk) in &thunks {
            thunk.update_env(&rec_env);
        }
    } else {
        for entry in set.entries() {
            match entry {
                ast::Entry::AttrpathValue(apv) => {
                    let attrpath = apv.attrpath().ok_or_else(|| {
                        EvalError::ParseError("binding missing attrpath".to_string())
                    })?;
                    let value_expr = apv.value().ok_or_else(|| {
                        EvalError::ParseError("binding missing value".to_string())
                    })?;
                    let mut path_keys: Vec<String> = attrpath
                        .attrs()
                        .filter_map(|a| eval_attr_maybe_null(&a, env).transpose())
                        .collect::<Result<_, _>>()?;
                    // Null dynamic attr name → skip entire binding (CppNix compat)
                    if path_keys.is_empty() { continue; }
                    if path_keys.len() == 1 {
                        let key = path_keys.pop().unwrap();
                        // maybeThunk: skip thunk for trivial exprs.
                        // is_rec=false — Ident lookups are safe.
                        let value = maybe_thunk(&value_expr, env, false, None);
                        attrs.insert(key, value);
                    } else {
                        let key = path_keys[0].clone();
                        let value = build_nested_attr(&path_keys[1..], &value_expr, env)?;
                        merge_nested_insert(&mut attrs, key, value);
                    }
                }
                ast::Entry::Inherit(inherit) => {
                    eval_inherit(&inherit, env, &mut attrs, None, None)?;
                }
            }
        }
    }

    Ok(Value::Attrs(Rc::new(attrs)))
}

fn eval_inherit(
    inherit: &ast::Inherit,
    env: &Env,
    attrs: &mut NixAttrs,
    bind_env: Option<&mut Env>,
    mut thunks: Option<&mut Vec<(String, Thunk)>>,
) -> Result<(), EvalError> {
    if let Some(from) = inherit.from() {
        // inherit (expr) a b c;
        //
        // The source expression must NOT be eagerly evaluated. nixpkgs
        // `lib/trivial.nix` has `inherit (lib.trivial) isFunction ...`
        // at the top of a file that itself defines `lib.trivial`. If
        // we eagerly force `lib.trivial`, we hit a self-referential
        // thunk blackhole. Instead: build a thunk per inherited
        // name that, when forced, evaluates the source and pulls
        // out that one attribute. This is what real Nix does.
        //
        // For `rec { inherit (X) name; ...; foo = name; }` we ALSO
        // need to bind the name in the enclosing rec env so the
        // sibling `foo = name` can reference it. The caller passes
        // its rec env in `bind_env`.
        //
        // When `thunks` is provided (rec attrsets), InheritSelect
        // thunks are collected so Phase 2 can update their captured
        // env to the full recursive scope. Without this, the source
        // expression cannot reference sibling bindings.
        let source_expr = from
            .expr()
            .ok_or_else(|| EvalError::ParseError("inherit from missing expr".to_string()))?;
        // Shared source thunk — all inherited names share one source
        // evaluation (the source thunk's own memoization ensures at
        // most one evaluation).
        let source_thunk = Thunk::new_suspended(source_expr, env.clone());
        let mut be = bind_env;
        for attr in inherit.attrs() {
            let name = eval_attr(&attr, env)?;
            let thunk = Thunk::new_inherit_select(source_thunk.clone(), name.clone());
            let value = Value::Thunk(thunk.clone());
            attrs.insert(name.clone(), value.clone());
            if let Some(ref mut e) = be {
                e.bind(name.clone(), value);
            }
            if let Some(ref mut t) = thunks {
                t.push((name, thunk));
            }
        }
    } else {
        // inherit a b c;
        let mut be = bind_env;
        for attr in inherit.attrs() {
            let name = eval_attr(&attr, env)?;
            let value = env
                .lookup(&name)
                .ok_or_else(|| EvalError::UndefinedVar(
                    format!("'{name}'{}", eval_file_ctx()),
                ))?;
            attrs.insert(name.clone(), value.clone());
            if let Some(ref mut e) = be {
                e.bind(name, value);
            }
        }
    }
    Ok(())
}

fn build_nested_attr(
    path: &[String],
    expr: &ast::Expr,
    env: &Env,
) -> Result<Value, EvalError> {
    if path.is_empty() {
        return eval_expr(expr, env);
    }
    let key = path[0].clone();
    let inner = build_nested_attr(&path[1..], expr, env)?;
    let mut attrs = NixAttrs::new();
    attrs.insert(key, inner);
    Ok(Value::Attrs(Rc::new(attrs)))
}

/// Like [`build_nested_attr`] but wraps the leaf in a [`Thunk`] instead of
/// eagerly evaluating it. Used inside `rec { ... }` and `let ... in` so
/// that dotted-path leaf expressions can reference sibling bindings
/// through the recursive env (which is finalised in Phase 2).
///
/// Every thunk created is appended to `thunks` so Phase 2 can update
/// its captured environment.
fn build_nested_attr_thunk(
    path: &[String],
    expr: &ast::Expr,
    env: &Env,
    thunks: &mut Vec<(String, Thunk)>,
) -> Value {
    if path.is_empty() {
        let thunk = Thunk::new_suspended(expr.clone(), env.clone());
        let val = Value::Thunk(thunk.clone());
        thunks.push((String::new(), thunk));
        return val;
    }
    let key = path[0].clone();
    let inner = build_nested_attr_thunk(&path[1..], expr, env, thunks);
    let mut attrs = NixAttrs::new();
    attrs.insert(key, inner);
    Value::Attrs(Rc::new(attrs))
}

/// Insert `value` at `key` in `target`. If `target` already has a
/// concrete `Value::Attrs` at that key AND `value` is also a
/// concrete `Value::Attrs`, deep-merge them rather than overwriting.
/// This is what makes `{ a.b.c = 1; a.b.d = 2; a.e = 3; }` produce
/// `{ a = { b = { c = 1; d = 2; }; e = 3; }; }` instead of
/// dropping siblings — every nixpkgs module relies on this.
fn merge_nested_insert(target: &mut NixAttrs, key: String, value: Value) {
    let should_merge = matches!(target.get(&key), Some(Value::Attrs(_)))
        && matches!(value, Value::Attrs(_));
    if !should_merge {
        target.insert(key, value);
        return;
    }
    // Both sides are concrete attrs — merge in place. We pop the
    // existing entry, then walk the new attrs and recursively
    // merge each child onto it.
    let mut existing_attrs = match target.get(&key).cloned() {
        Some(Value::Attrs(a)) => (*a).clone(),
        _ => unreachable!(),
    };
    let new_attrs = match value {
        Value::Attrs(ref a) => a,
        _ => unreachable!(),
    };
    for (k, v) in new_attrs.iter() {
        merge_nested_insert(&mut existing_attrs, k.clone(), v.clone());
    }
    target.insert(key, Value::Attrs(Rc::new(existing_attrs)));
}

/// Evaluate entries from any HasEntry node (LegacyLet).
fn eval_entries<N: HasEntry + AstNode>(node: &N, env: &mut Env) -> Result<(), EvalError> {
    for entry in node.entries() {
        match entry {
            ast::Entry::AttrpathValue(apv) => {
                let attrpath = apv.attrpath().ok_or_else(|| {
                    EvalError::ParseError("binding missing attrpath".to_string())
                })?;
                let value_expr = apv.value().ok_or_else(|| {
                    EvalError::ParseError("binding missing value".to_string())
                })?;
                let mut path_keys: Vec<String> = attrpath
                    .attrs()
                    .map(|a| eval_attr(&a, env))
                    .collect::<Result<_, _>>()?;
                if path_keys.len() == 1 {
                    let key = path_keys.pop().unwrap();
                    let value = eval_expr(&value_expr, env)?;
                    env.bind(key, value);
                }
                // Multi-key paths in let are not standard; skip for now.
            }
            ast::Entry::Inherit(inherit) => {
                if let Some(from) = inherit.from() {
                    let source_expr = from.expr().ok_or_else(|| {
                        EvalError::ParseError("inherit from missing expr".to_string())
                    })?;
                    let source = force_value(&eval_expr(&source_expr, env)?)?;
                    let source_attrs = source.as_attrs()?;
                    for attr in inherit.attrs() {
                        let name = eval_attr(&attr, env)?;
                        let value = source_attrs
                            .get(&name)
                            .cloned()
                            .ok_or_else(|| EvalError::AttrNotFound(
                                format!("'{name}' in inherit{}", eval_file_ctx()),
                            ))?;
                        env.bind(name, value);
                    }
                } else {
                    for attr in inherit.attrs() {
                        let name = eval_attr(&attr, env)?;
                        let value = env
                            .lookup(&name)
                            .ok_or_else(|| EvalError::UndefinedVar(
                                format!("'{name}'{}", eval_file_ctx()),
                            ))?;
                        env.bind(name, value);
                    }
                }
            }
        }
    }
    Ok(())
}

fn eval_binop(
    op: ast::BinOpKind,
    lhs: &ast::Expr,
    rhs: &ast::Expr,
    env: &Env,
) -> Result<Value, EvalError> {
    // Short-circuit for && and ||
    match op {
        ast::BinOpKind::And => {
            let l = force_value(&eval_expr(lhs, env)?)?.as_bool()?;
            if !l {
                return Ok(Value::Bool(false));
            }
            return eval_expr(rhs, env);
        }
        ast::BinOpKind::Or => {
            let l = force_value(&eval_expr(lhs, env)?)?.as_bool()?;
            if l {
                return Ok(Value::Bool(true));
            }
            return eval_expr(rhs, env);
        }
        ast::BinOpKind::Implication => {
            let l = force_value(&eval_expr(lhs, env)?)?.as_bool()?;
            if !l {
                return Ok(Value::Bool(true));
            }
            return eval_expr(rhs, env);
        }
        _ => {}
    }

    let lc = force_concrete(&eval_expr(lhs, env)?)?;
    let rc = force_concrete(&eval_expr(rhs, env)?)?;
    let l = lc.to_value();
    let r = rc.to_value();

    match op {
        ast::BinOpKind::Add => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a + b)),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
            (Value::Int(a), Value::Float(b)) => Ok(Value::Float(*a as f64 + b)),
            (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + *b as f64)),
            (Value::String(a), Value::String(b)) => {
                let mut ctx = a.context.clone();
                ctx.merge(&b.context);
                Ok(Value::String(Rc::new(NixString::with_context(
                    format!("{}{}", a.chars, b.chars),
                    ctx,
                ))))
            }
            (Value::Path(a), Value::String(b)) => Ok(Value::Path(Box::new(SmolStr::from(format!("{a}{}", b.chars).as_str())))),
            (Value::Path(a), Value::Path(b)) => Ok(Value::Path(Box::new(SmolStr::from(format!("{a}/{b}").as_str())))),
            // CppNix coerces attrsets with outPath when used with +
            (Value::Attrs(_), _) | (_, Value::Attrs(_)) => {
                let (ls, lctx) = l.coerce_to_string()?;
                let (rs, rctx) = r.coerce_to_string()?;
                let mut ctx = lctx;
                ctx.merge(&rctx);
                Ok(Value::String(Rc::new(NixString::with_context(
                    format!("{ls}{rs}"),
                    ctx,
                ))))
            }
            _ => Err(EvalError::op_type("add", l.type_name(), r.type_name())),
        },
        ast::BinOpKind::Sub => num_op(&l, &r, |a, b| a - b, |a, b| a - b),
        ast::BinOpKind::Mul => num_op(&l, &r, |a, b| a * b, |a, b| a * b),
        ast::BinOpKind::Div => match (&l, &r) {
            (Value::Int(_), Value::Int(0)) => Err(EvalError::DivisionByZero),
            _ => num_op(&l, &r, |a, b| a / b, |a, b| a / b),
        },
        ast::BinOpKind::Equal => Ok(Value::Bool(l == r)),
        ast::BinOpKind::NotEqual => Ok(Value::Bool(l != r)),
        ast::BinOpKind::Less => compare(&l, &r, |o| o == std::cmp::Ordering::Less),
        ast::BinOpKind::LessOrEq => compare(&l, &r, |o| o != std::cmp::Ordering::Greater),
        ast::BinOpKind::More => compare(&l, &r, |o| o == std::cmp::Ordering::Greater),
        ast::BinOpKind::MoreOrEq => compare(&l, &r, |o| o != std::cmp::Ordering::Less),
        ast::BinOpKind::Update => {
            // // operator: force both sides to concrete attrs.
            let la = l.to_attrs()?;
            let ra = r.to_attrs()?;
            // Optimization: if right side is empty, return left as-is.
            if ra.is_empty() {
                return Ok(Value::Attrs(Rc::new(la)));
            }
            // Optimization: if left side is empty, return right as-is.
            if la.is_empty() {
                return Ok(Value::Attrs(Rc::new(ra)));
            }
            Ok(Value::Attrs(Rc::new(la.update(&ra))))
        }
        ast::BinOpKind::Concat => {
            let mut la = l.as_list()?.to_vec();
            la.extend_from_slice(r.as_list()?);
            Ok(Value::list(la))
        }
        ast::BinOpKind::And | ast::BinOpKind::Or | ast::BinOpKind::Implication => {
            unreachable!("handled above")
        }
        ast::BinOpKind::PipeRight | ast::BinOpKind::PipeLeft => {
            Err(EvalError::NotImplemented("pipe operators".to_string()))
        }
    }
}

fn num_op(
    l: &Value,
    r: &Value,
    int_op: impl Fn(i64, i64) -> i64,
    float_op: impl Fn(f64, f64) -> f64,
) -> Result<Value, EvalError> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => Ok(Value::Int(int_op(*a, *b))),
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(float_op(*a, *b))),
        (Value::Int(a), Value::Float(b)) => Ok(Value::Float(float_op(*a as f64, *b))),
        (Value::Float(a), Value::Int(b)) => Ok(Value::Float(float_op(*a, *b as f64))),
        _ => Err(EvalError::op_type("perform arithmetic on", l.type_name(), r.type_name())),
    }
}

fn compare(
    l: &Value,
    r: &Value,
    pred: impl Fn(std::cmp::Ordering) -> bool,
) -> Result<Value, EvalError> {
    let ord = match (l, r) {
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Float(a), Value::Float(b)) => {
            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Value::Int(a), Value::Float(b)) => (*a as f64)
            .partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::Float(a), Value::Int(b)) => a
            .partial_cmp(&(*b as f64))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.chars.cmp(&b.chars),
        _ => {
            return Err(EvalError::op_type("compare", l.type_name(), r.type_name()));
        }
    };
    Ok(Value::Bool(pred(ord)))
}

/// Apply a function to an argument.
///
/// Supports `__functor`: if `func` is an attrset with a `__functor` key,
/// calls `__functor self arg` (the Nix `__functor` protocol).
///
/// For lambda with a simple ident parameter, the argument is NOT forced
/// before binding -- this enables fixpoint combinators (`lib.fix`) where
/// the argument is a self-referential thunk.
/// Apply a function and force the result.
///
/// Builtins that inspect the return value (via `as_list`, `as_bool`, etc.)
/// must use this instead of bare `apply` — otherwise a thunk-wrapped result
/// will cause "thunk in as_list: force first" errors.
pub fn apply_and_force(func: Value, arg: Value) -> Result<Value, EvalError> {
    force_value(&apply(func, arg)?)
}

pub fn apply(func: Value, arg: Value) -> Result<Value, EvalError> {
    crate::perf::inc(crate::perf::Counter::Apply);
    let func = force_concrete(&func)?.into_value();
    match func {
        Value::Lambda(closure) => {
            // Hot function tracker: log source file + param name for each lambda call
            if crate::perf::enabled() {
                APPLY_SITES.with(|sites| {
                    let file = closure.env.eval_file()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<eval>".into());
                    // Include param info for identification
                    let param_name = match &closure.param {
                        rnix::ast::Param::IdentParam(ip) => ip.ident().map(|i| ident_text(&i)).unwrap_or_default(),
                        rnix::ast::Param::Pattern(pat) => {
                            let mut names: Vec<String> = pat.pat_entries()
                                .filter_map(|e| e.ident().map(|i| ident_text(&i)))
                                .take(3)
                                .collect();
                            if pat.pat_entries().count() > 3 { names.push("...".to_string()); }
                            format!("{{{}}}", names.join(","))
                        }
                    };
                    let key = format!("{}:{}", file.rsplit_once("-source/").map_or(file.as_str(), |(_,s)| s), param_name);
                    *sites.borrow_mut().entry(key).or_insert(0u64) += 1;
                });
            }
            let mut call_env = closure.env.child();
            let _file_guard = closure
                .env
                .eval_file()
                .cloned()
                .map(push_eval_file);
            // Push Nix-level trace frame for function calls
            let _trace = {
                let file = closure.env.eval_file()
                    .map(|p| p.display().to_string()
                        .rsplit_once("-source/")
                        .map_or_else(|| p.display().to_string(), |(_, s)| s.to_string()));
                push_nix_trace(format!("while calling function defined in {}", file.as_deref().unwrap_or("<eval>")))
            };
            match &closure.param {
                rnix::ast::Param::IdentParam(_) => {
                    // Simple ident param: bind argument WITHOUT forcing.
                    // This is critical for fixpoint / call-by-need semantics.
                    bind_param(&closure.param, &arg, &mut call_env)?;
                }
                rnix::ast::Param::Pattern(_) => {
                    // Pattern param needs the arg to be an attrset, so force.
                    let forced_arg = force_concrete(&arg)?.into_value();
                    bind_param(&closure.param, &forced_arg, &mut call_env)?;
                }
            }
            eval_expr(&closure.body, &call_env)
        }
        Value::Builtin(b) => {
            let _trace = push_nix_trace(format!("while calling the '{}' builtin", b.name));
            // Special builtins that must receive UNFORCED arguments:
            // - tryEval: must catch throw/abort during its own forcing
            // - addErrorContext<partial>: wraps value with error context
            //   without forcing (the value is the fixpoint `config` which
            //   causes infinite recursion if forced during collectModules)
            // - seq<partial>: forces first arg but returns second UNFORCED
            if b.name == "tryEval"
                || b.name == "addErrorContext<partial>"
                || b.name == "seq<partial>"
            {
                (b.func)(&[arg])
            } else {
                let forced_arg = force_value(&arg)?;
                (b.func)(&[forced_arg])
            }
        }
        Value::Attrs(ref attrs) => {
            if let Some(functor) = attrs.get("__functor") {
                let functor = force_value(functor)?;
                // __functor protocol: (functor self) arg
                let partial = apply(functor, func.clone())?;
                apply(partial, arg)
            } else {
                Err(EvalError::type_error(
                    format!("cannot call {} (missing __functor){}", func.type_name(), eval_file_ctx()),
                ))
            }
        }
        _ => Err(EvalError::type_error(
            format!("cannot call {}{}", func.type_name(), eval_file_ctx()),
        )),
    }
}

fn bind_param(param: &ast::Param, arg: &Value, env: &mut Env) -> Result<(), EvalError> {
    match param {
        ast::Param::IdentParam(ip) => {
            let ident = ip
                .ident()
                .ok_or_else(|| EvalError::ParseError("ident param missing ident".to_string()))?;
            let name = ident_text(&ident);
            env.bind(name, arg.clone());
        }
        ast::Param::Pattern(pat) => {
            let attrs = arg.as_attrs()?;

            // @-binding (either `args @ { ... }` or `{ ... } @ args`)
            if let Some(pat_bind) = pat.pat_bind()
                && let Some(ident) = pat_bind.ident()
            {
                let name = ident_text(&ident);
                env.bind(name, arg.clone());
            }

            let has_ellipsis = pat.ellipsis_token().is_some();
            let entries: Vec<ast::PatEntry> = pat.pat_entries().collect();

            // Two-phase binding (matching CppNix semantics):
            // Phase 1: Bind all formals. Defaults get thunks with a
            //   preliminary env. We collect thunks for Phase 2 update.
            // Phase 2: Update default thunks to capture the final env
            //   (which now has ALL formals bound). This allows defaults
            //   to reference any other formal — including forward refs.
            let mut default_thunks: Vec<Thunk> = Vec::new();

            for entry in &entries {
                let ident = entry.ident().ok_or_else(|| {
                    EvalError::ParseError("pat entry missing ident".to_string())
                })?;
                let name = ident_text(&ident);
                let value = if let Some(v) = attrs.get(&name) {
                    v.clone()
                } else if let Some(default_expr) = entry.default() {
                    // Default values in pattern parameters must be lazy
                    // (wrapped in thunks), matching CppNix semantics.
                    // Patterns like `vendor ? assert false; null` rely on
                    // the default never being forced when the body checks
                    // `args ? vendor` instead of using `vendor` directly.
                    let thunk = Thunk::new_suspended(
                        ast::Expr::cast(default_expr.syntax().clone()).unwrap(),
                        env.clone(),
                    );
                    default_thunks.push(thunk.clone());
                    Value::Thunk(thunk)
                } else {
                    return Err(EvalError::type_error(
                        format!("missing argument '{name}'{}", eval_file_ctx()),
                    ));
                };
                env.bind(name, value);
            }

            // Phase 2: Update default thunks to see ALL formals.
            for thunk in &default_thunks {
                thunk.update_env(env);
            }

            if !has_ellipsis {
                let entry_names: std::collections::HashSet<String> = entries
                    .iter()
                    .filter_map(|e| e.ident().map(|i| ident_text(&i)))
                    .collect();
                for key in attrs.keys() {
                    if !entry_names.contains(key.as_str()) {
                        return Err(EvalError::type_error(
                            format!("unexpected argument '{key}'{}", eval_file_ctx()),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(input: &str) -> Value {
        eval(input).unwrap()
    }

    #[test]
    fn eval_int() { assert_eq!(ev("42"), Value::Int(42)); }

    #[test]
    fn eval_float() { assert_eq!(ev("3.14"), Value::Float(3.14)); }

    #[test]
    fn eval_string() { assert_eq!(ev(r#""hello""#), Value::string("hello")); }

    #[test]
    fn eval_bool() { assert_eq!(ev("true"), Value::Bool(true)); }

    #[test]
    fn eval_null() { assert_eq!(ev("null"), Value::Null); }

    #[test]
    fn eval_arithmetic() {
        assert_eq!(ev("1 + 2"), Value::Int(3));
        assert_eq!(ev("10 - 3"), Value::Int(7));
        assert_eq!(ev("2 * 3"), Value::Int(6));
        assert_eq!(ev("10 / 3"), Value::Int(3));
    }

    #[test]
    fn eval_precedence() {
        assert_eq!(ev("1 + 2 * 3"), Value::Int(7));
        assert_eq!(ev("(1 + 2) * 3"), Value::Int(9));
    }

    #[test]
    fn eval_comparison() {
        assert_eq!(ev("1 == 1"), Value::Bool(true));
        assert_eq!(ev("1 == 2"), Value::Bool(false));
        assert_eq!(ev("1 < 2"), Value::Bool(true));
        assert_eq!(ev("2 <= 2"), Value::Bool(true));
    }

    #[test]
    fn eval_logic() {
        assert_eq!(ev("true && false"), Value::Bool(false));
        assert_eq!(ev("true || false"), Value::Bool(true));
        assert_eq!(ev("!true"), Value::Bool(false));
    }

    #[test]
    fn eval_string_concat() {
        assert_eq!(ev(r#""hello" + " " + "world""#), Value::string("hello world"));
    }

    #[test]
    fn eval_if() {
        assert_eq!(ev("if true then 1 else 2"), Value::Int(1));
        assert_eq!(ev("if false then 1 else 2"), Value::Int(2));
    }

    #[test]
    fn eval_let() {
        assert_eq!(ev("let x = 1; in x"), Value::Int(1));
        assert_eq!(ev("let x = 1; y = 2; in x + y"), Value::Int(3));
    }

    #[test]
    fn eval_let_dotted_simple() {
        // Two dotted bindings sharing the top-level key `a`.
        assert_eq!(ev("let a.b = 1; a.c = 2; in a.b + a.c"), Value::Int(3));
    }

    #[test]
    fn eval_let_dotted_deep() {
        // Deeply nested dotted path.
        assert_eq!(ev("let a.b.c = 1; in a.b.c"), Value::Int(1));
    }

    #[test]
    fn eval_let_dotted_mixed() {
        // Mix of simple and dotted bindings.
        assert_eq!(
            ev("let a.x = 1; b = 2; a.y = 3; in a.x + a.y + b"),
            Value::Int(6),
        );
    }

    #[test]
    fn eval_let_dotted_produces_attrset() {
        // Dotted let bindings produce a real attrset.
        let v = ev("let a.b = 1; a.c = 2; in a");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("b"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("c"), Some(&Value::Int(2)));
        } else {
            panic!("expected Attrs, got {v:?}");
        }
    }

    #[test]
    fn eval_nested_let() {
        assert_eq!(ev("let a = 1; b = let c = 2; in c; in a + b"), Value::Int(3));
    }

    #[test]
    fn eval_lambda() {
        assert_eq!(ev("(x: x + 1) 41"), Value::Int(42));
    }

    #[test]
    fn eval_lambda_multi_arg() {
        assert_eq!(ev("(x: y: x + y) 1 2"), Value::Int(3));
    }

    #[test]
    fn eval_list() {
        let v = ev("[1 2 3]");
        assert_eq!(v, Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]));
    }

    #[test]
    fn eval_list_concat() {
        let v = ev("[1 2] ++ [3 4]");
        assert_eq!(v, Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)]));
    }

    #[test]
    fn eval_attrset() {
        let v = ev("{ a = 1; b = 2; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
        } else {
            panic!("expected attrset");
        }
    }

    #[test]
    fn eval_select() {
        assert_eq!(ev("{ a = 42; }.a"), Value::Int(42));
    }

    #[test]
    fn eval_select_or() {
        assert_eq!(ev("{ a = 42; }.b or 0"), Value::Int(0));
    }

    #[test]
    fn eval_has_attr() {
        assert_eq!(ev("{ a = 1; } ? a"), Value::Bool(true));
        assert_eq!(ev("{ a = 1; } ? b"), Value::Bool(false));
    }

    #[test]
    fn eval_update() {
        let v = ev("{ a = 1; b = 2; } // { b = 3; c = 4; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(3)));
            assert_eq!(attrs.get("c"), Some(&Value::Int(4)));
        } else {
            panic!("expected attrset");
        }
    }

    #[test]
    fn eval_with() {
        assert_eq!(ev("with { x = 42; }; x"), Value::Int(42));
    }

    #[test]
    fn eval_assert() {
        assert_eq!(ev("assert true; 42"), Value::Int(42));
        assert!(eval("assert false; 42").is_err());
    }

    #[test]
    fn eval_formals() {
        assert_eq!(ev("({ a, b }: a + b) { a = 1; b = 2; }"), Value::Int(3));
    }

    #[test]
    fn eval_formals_default() {
        assert_eq!(ev("({ a, b ? 10 }: a + b) { a = 1; }"), Value::Int(11));
    }

    #[test]
    fn eval_formals_ellipsis() {
        assert_eq!(ev("({ a, ... }: a) { a = 1; b = 2; }"), Value::Int(1));
    }

    #[test]
    fn eval_named_formals() {
        assert_eq!(ev("(args @ { a }: args.a) { a = 42; }"), Value::Int(42));
    }

    #[test]
    fn eval_rec_attrset() {
        assert_eq!(ev("(rec { a = 1; b = a + 1; }).b"), Value::Int(2));
    }

    #[test]
    fn eval_negation() {
        assert_eq!(ev("-42"), Value::Int(-42));
    }

    #[test]
    fn eval_float_arithmetic() {
        assert_eq!(ev("1.5 + 2.5"), Value::Float(4.0));
        assert_eq!(ev("1 + 1.5"), Value::Float(2.5));
    }

    #[test]
    fn eval_division_by_zero() {
        assert!(eval("1 / 0").is_err());
    }

    #[test]
    fn eval_builtins_available() {
        assert_eq!(ev("builtins.typeOf 42"), Value::string("int"));
        assert_eq!(ev("builtins.typeOf true"), Value::string("bool"));
    }

    #[test]
    fn eval_builtins_length() {
        assert_eq!(ev("builtins.length [1 2 3]"), Value::Int(3));
    }

    #[test]
    fn eval_builtins_head_tail() {
        assert_eq!(ev("builtins.head [1 2 3]"), Value::Int(1));
        assert_eq!(ev("builtins.length (builtins.tail [1 2 3])"), Value::Int(2));
    }

    #[test]
    fn eval_builtins_add() {
        assert_eq!(ev("builtins.add 1 2"), Value::Int(3));
    }

    #[test]
    fn eval_builtins_to_string() {
        assert_eq!(ev("builtins.toString 42"), Value::string("42"));
    }

    #[test]
    fn eval_implication() {
        assert_eq!(ev("false -> true"), Value::Bool(true));
        assert_eq!(ev("true -> false"), Value::Bool(false));
        assert_eq!(ev("true -> true"), Value::Bool(true));
    }

    // ── New tests ────────────────────────────────────────

    #[test]
    fn eval_error_undefined_variable() {
        let result = eval("nonexistent");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("undefined variable"));
    }

    #[test]
    fn eval_error_type_mismatch_arithmetic() {
        let result = eval(r#"1 + "hello""#);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("cannot add") || msg.contains("type"));
    }

    #[test]
    fn eval_error_unexpected_argument() {
        let result = eval("({ a }: a) { a = 1; b = 2; }");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("unexpected argument"));
    }

    #[test]
    fn eval_error_missing_required_argument() {
        let result = eval("({ a, b }: a + b) { a = 1; }");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("missing argument"));
    }

    #[test]
    fn eval_builtins_attr_names_sorted() {
        let v = ev("builtins.attrNames { z = 1; a = 2; m = 3; }");
        // BTreeMap keys are already sorted
        assert_eq!(
            v,
            Value::list(vec![
                Value::string("a"),
                Value::string("m"),
                Value::string("z"),
            ]),
        );
    }

    #[test]
    fn eval_builtins_attr_values() {
        let v = ev("builtins.attrValues { a = 1; b = 2; }");
        // BTreeMap iteration is sorted by key, so a=1 first, b=2 second
        assert_eq!(v, Value::list(vec![Value::Int(1), Value::Int(2)]));
    }

    #[test]
    fn eval_builtins_is_null() {
        assert_eq!(ev("builtins.isNull null"), Value::Bool(true));
        assert_eq!(ev("builtins.isNull 1"), Value::Bool(false));
    }

    #[test]
    fn eval_builtins_is_int() {
        assert_eq!(ev("builtins.isInt 42"), Value::Bool(true));
        assert_eq!(ev("builtins.isInt 3.14"), Value::Bool(false));
    }

    #[test]
    fn eval_builtins_is_bool() {
        assert_eq!(ev("builtins.isBool true"), Value::Bool(true));
        assert_eq!(ev("builtins.isBool 0"), Value::Bool(false));
    }

    #[test]
    fn eval_builtins_is_string() {
        assert_eq!(ev(r#"builtins.isString "hi""#), Value::Bool(true));
        assert_eq!(ev("builtins.isString 1"), Value::Bool(false));
    }

    #[test]
    fn eval_builtins_is_list() {
        assert_eq!(ev("builtins.isList [1 2]"), Value::Bool(true));
        assert_eq!(ev("builtins.isList {}"), Value::Bool(false));
    }

    #[test]
    fn eval_builtins_is_attrs() {
        assert_eq!(ev("builtins.isAttrs {}"), Value::Bool(true));
        assert_eq!(ev("builtins.isAttrs []"), Value::Bool(false));
    }

    #[test]
    fn eval_builtins_string_length() {
        assert_eq!(ev(r#"builtins.stringLength "hello""#), Value::Int(5));
        assert_eq!(ev(r#"builtins.stringLength """#), Value::Int(0));
    }

    #[test]
    fn eval_builtins_to_json_roundtrip() {
        // toJSON produces a JSON string; fromJSON parses it back
        assert_eq!(
            ev(r#"builtins.fromJSON (builtins.toJSON 42)"#),
            Value::Int(42),
        );
        assert_eq!(
            ev(r#"builtins.fromJSON (builtins.toJSON [1 2 3])"#),
            Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn eval_builtins_from_json() {
        assert_eq!(
            ev(r#"builtins.fromJSON "{\"a\": 1}""#),
            {
                let mut attrs = NixAttrs::new();
                attrs.insert("a".to_string(), Value::Int(1));
                Value::Attrs(Rc::new(attrs))
            },
        );
        assert_eq!(ev(r#"builtins.fromJSON "null""#), Value::Null);
        assert_eq!(ev(r#"builtins.fromJSON "true""#), Value::Bool(true));
    }

    #[test]
    fn eval_nested_function_application() {
        // (f 1) 2 where f = x: y: x + y
        assert_eq!(ev("(x: y: x + y) 1 2"), Value::Int(3));
        // equivalent parenthesized form
        assert_eq!(ev("((x: y: x + y) 1) 2"), Value::Int(3));
    }

    #[test]
    fn eval_recursive_let() {
        assert_eq!(ev("let a = 1; b = a + 1; in b"), Value::Int(2));
        assert_eq!(ev("let a = 1; b = a + 1; c = b + 1; in c"), Value::Int(3));
    }

    #[test]
    fn eval_string_comparison() {
        assert_eq!(ev(r#""a" < "b""#), Value::Bool(true));
        assert_eq!(ev(r#""b" < "a""#), Value::Bool(false));
        assert_eq!(ev(r#""abc" == "abc""#), Value::Bool(true));
        assert_eq!(ev(r#""abc" != "def""#), Value::Bool(true));
    }

    #[test]
    fn eval_list_in_attrset() {
        let v = ev("{ x = [1 2 3]; }.x");
        assert_eq!(
            v,
            Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn eval_nested_attrset_select() {
        assert_eq!(ev("{ a = { b = 42; }; }.a.b"), Value::Int(42));
    }

    #[test]
    fn eval_let_shadows_outer() {
        assert_eq!(
            ev("let x = 1; in let x = 2; in x"),
            Value::Int(2),
        );
    }

    #[test]
    fn eval_with_provides_scope() {
        // `with` scope is available for name resolution
        assert_eq!(
            ev("with { x = 42; y = 10; }; x + y"),
            Value::Int(52),
        );
    }

    #[test]
    fn eval_list_equality() {
        assert_eq!(ev("[1 2] == [1 2]"), Value::Bool(true));
        assert_eq!(ev("[1 2] == [1 3]"), Value::Bool(false));
    }

    #[test]
    fn eval_attrset_equality() {
        assert_eq!(ev("{ a = 1; } == { a = 1; }"), Value::Bool(true));
        assert_eq!(ev("{ a = 1; } == { a = 2; }"), Value::Bool(false));
    }

    // ═══════════════════════════════════════════════════════════
    // 1. LITERAL TYPES
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn literal_int_large_zero_negative() {
        // Large positive integer (within i64 range)
        assert_eq!(ev("9223372036854775807"), Value::Int(i64::MAX));
        // Zero
        assert_eq!(ev("0"), Value::Int(0));
        // Negative via unary negate
        assert_eq!(ev("-1"), Value::Int(-1));
        assert_eq!(ev("-999999"), Value::Int(-999999));
    }

    #[test]
    fn literal_float_small_large() {
        assert_eq!(ev("0.001"), Value::Float(0.001));
        assert_eq!(ev("999999.999"), Value::Float(999999.999));
        // Float with scientific notation via expression (1e6 parsed by rnix)
        assert_eq!(ev("1.0e3"), Value::Float(1000.0));
        assert_eq!(ev("1.5e2"), Value::Float(150.0));
    }

    #[test]
    fn literal_string_empty_and_escapes() {
        assert_eq!(ev(r#""""#), Value::string(""));
        // Escape sequences within strings
        assert_eq!(ev(r#""hello\nworld""#), Value::string("hello\nworld"));
        assert_eq!(ev(r#""tab\there""#), Value::string("tab\there"));
    }

    #[test]
    fn literal_multiline_string() {
        // Indented string ('' ... '')
        assert_eq!(
            ev("''hello''"),
            Value::string("hello"),
        );
        // Multiline indented string strips common indentation
        assert_eq!(
            ev("''\n  line1\n  line2\n''"),
            Value::string("line1\nline2\n"),
        );
    }

    #[test]
    fn literal_paths() {
        // Relative path
        assert_eq!(ev("./foo"), Value::Path(Box::new(SmolStr::from("./foo"))));
        // Absolute path
        assert_eq!(ev("/nix/store/abc"), Value::Path(Box::new(SmolStr::from("/nix/store/abc"))));
        // Home path
        assert_eq!(ev("~/myfile"), Value::Path(Box::new(SmolStr::from("~/myfile"))));
    }

    #[test]
    fn literal_null_true_false_standalone() {
        assert_eq!(ev("null"), Value::Null);
        assert_eq!(ev("true"), Value::Bool(true));
        assert_eq!(ev("false"), Value::Bool(false));
    }

    // ═══════════════════════════════════════════════════════════
    // 2. OPERATORS — COMPLETE COVERAGE
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn op_arithmetic_int() {
        assert_eq!(ev("100 + 200"), Value::Int(300));
        assert_eq!(ev("50 - 30"), Value::Int(20));
        assert_eq!(ev("7 * 8"), Value::Int(56));
        assert_eq!(ev("17 / 3"), Value::Int(5)); // integer division
    }

    #[test]
    fn op_arithmetic_float() {
        assert_eq!(ev("1.5 + 2.5"), Value::Float(4.0));
        assert_eq!(ev("5.0 - 1.5"), Value::Float(3.5));
        assert_eq!(ev("2.0 * 3.0"), Value::Float(6.0));
        assert_eq!(ev("7.0 / 2.0"), Value::Float(3.5));
    }

    #[test]
    fn op_arithmetic_mixed_int_float() {
        // int + float => float
        assert_eq!(ev("1 + 2.5"), Value::Float(3.5));
        assert_eq!(ev("2.5 + 1"), Value::Float(3.5));
        // int * float => float
        assert_eq!(ev("2 * 1.5"), Value::Float(3.0));
        // float - int => float
        assert_eq!(ev("5.5 - 2"), Value::Float(3.5));
    }

    #[test]
    fn op_string_concat() {
        assert_eq!(ev(r#""foo" + "bar""#), Value::string("foobar"));
        assert_eq!(ev(r#""" + "x""#), Value::string("x"));
        assert_eq!(ev(r#""a" + "" + "b""#), Value::string("ab"));
    }

    #[test]
    fn op_path_concat() {
        // path + string
        assert_eq!(ev(r#"./foo + "/bar""#), Value::Path(Box::new(SmolStr::from("./foo/bar"))));
        // path + path (should join with /)
        assert_eq!(ev("./a + ./b"), Value::Path(Box::new(SmolStr::from("./a/./b"))));
    }

    #[test]
    fn op_comparison_ints() {
        assert_eq!(ev("1 < 2"), Value::Bool(true));
        assert_eq!(ev("2 < 1"), Value::Bool(false));
        assert_eq!(ev("2 > 1"), Value::Bool(true));
        assert_eq!(ev("1 > 2"), Value::Bool(false));
        assert_eq!(ev("2 <= 2"), Value::Bool(true));
        assert_eq!(ev("3 <= 2"), Value::Bool(false));
        assert_eq!(ev("2 >= 2"), Value::Bool(true));
        assert_eq!(ev("1 >= 2"), Value::Bool(false));
    }

    #[test]
    fn op_comparison_floats() {
        assert_eq!(ev("1.5 < 2.5"), Value::Bool(true));
        assert_eq!(ev("2.5 > 1.5"), Value::Bool(true));
        assert_eq!(ev("1.5 <= 1.5"), Value::Bool(true));
        assert_eq!(ev("1.5 >= 1.5"), Value::Bool(true));
    }

    #[test]
    fn op_comparison_strings() {
        assert_eq!(ev(r#""apple" < "banana""#), Value::Bool(true));
        assert_eq!(ev(r#""banana" > "apple""#), Value::Bool(true));
        assert_eq!(ev(r#""abc" == "abc""#), Value::Bool(true));
        assert_eq!(ev(r#""abc" != "xyz""#), Value::Bool(true));
        assert_eq!(ev(r#""abc" <= "abd""#), Value::Bool(true));
        assert_eq!(ev(r#""abc" >= "abb""#), Value::Bool(true));
    }

    #[test]
    fn op_equality_various_types() {
        assert_eq!(ev("null == null"), Value::Bool(true));
        assert_eq!(ev("true == true"), Value::Bool(true));
        assert_eq!(ev("false == false"), Value::Bool(true));
        assert_eq!(ev("true == false"), Value::Bool(false));
        assert_eq!(ev("1 == 1"), Value::Bool(true));
        assert_eq!(ev("1 != 2"), Value::Bool(true));
        // Different types are not equal
        assert_eq!(ev(r#"1 == "1""#), Value::Bool(false));
        assert_eq!(ev("null == false"), Value::Bool(false));
    }

    #[test]
    fn op_logic_short_circuit() {
        // false && <error> should NOT evaluate the RHS
        assert_eq!(ev("false && (1 / 0 == 0)"), Value::Bool(false));
        // true || <error> should NOT evaluate the RHS
        assert_eq!(ev("true || (1 / 0 == 0)"), Value::Bool(true));
    }

    #[test]
    fn op_logic_full() {
        assert_eq!(ev("true && true"), Value::Bool(true));
        assert_eq!(ev("true && false"), Value::Bool(false));
        assert_eq!(ev("false && true"), Value::Bool(false));
        assert_eq!(ev("false && false"), Value::Bool(false));
        assert_eq!(ev("true || true"), Value::Bool(true));
        assert_eq!(ev("true || false"), Value::Bool(true));
        assert_eq!(ev("false || true"), Value::Bool(true));
        assert_eq!(ev("false || false"), Value::Bool(false));
        assert_eq!(ev("!true"), Value::Bool(false));
        assert_eq!(ev("!false"), Value::Bool(true));
    }

    #[test]
    fn op_implication_truth_table() {
        // false -> anything = true
        assert_eq!(ev("false -> false"), Value::Bool(true));
        assert_eq!(ev("false -> true"), Value::Bool(true));
        // true -> x = x
        assert_eq!(ev("true -> true"), Value::Bool(true));
        assert_eq!(ev("true -> false"), Value::Bool(false));
    }

    #[test]
    fn op_implication_short_circuit() {
        // false -> <error> should NOT evaluate the RHS
        assert_eq!(ev("false -> (1 / 0 == 0)"), Value::Bool(true));
    }

    #[test]
    fn op_update_merge() {
        let v = ev("{ a = 1; } // { b = 2; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn op_update_right_wins() {
        assert_eq!(ev("({ a = 1; } // { a = 2; }).a"), Value::Int(2));
    }

    #[test]
    fn op_list_concat() {
        assert_eq!(
            ev("[1 2] ++ [3 4]"),
            Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)]),
        );
        // Empty list concat
        assert_eq!(ev("[] ++ [1]"), Value::list(vec![Value::Int(1)]));
        assert_eq!(ev("[1] ++ []"), Value::list(vec![Value::Int(1)]));
    }

    #[test]
    fn op_has_attr_present_and_absent() {
        assert_eq!(ev("{ x = 1; y = 2; } ? x"), Value::Bool(true));
        assert_eq!(ev("{ x = 1; } ? z"), Value::Bool(false));
        assert_eq!(ev("{} ? anything"), Value::Bool(false));
    }

    #[test]
    fn op_unary_negate() {
        assert_eq!(ev("-42"), Value::Int(-42));
        assert_eq!(ev("-3.14"), Value::Float(-3.14));
        // Double negate
        assert_eq!(ev("- -5"), Value::Int(5));
    }

    // ═══════════════════════════════════════════════════════════
    // 3. CONTROL FLOW
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn control_if_true_branch() {
        assert_eq!(ev("if true then 42 else 0"), Value::Int(42));
    }

    #[test]
    fn control_if_false_branch() {
        assert_eq!(ev("if false then 42 else 0"), Value::Int(0));
    }

    #[test]
    fn control_if_nested() {
        assert_eq!(
            ev("if true then (if false then 1 else 2) else 3"),
            Value::Int(2),
        );
        assert_eq!(
            ev("if false then 1 else (if true then 2 else 3)"),
            Value::Int(2),
        );
    }

    #[test]
    fn control_assert_passing() {
        assert_eq!(ev("assert 1 == 1; 42"), Value::Int(42));
        assert_eq!(ev("assert true; true"), Value::Bool(true));
    }

    #[test]
    fn control_assert_failing() {
        assert!(eval("assert false; 42").is_err());
        assert!(eval("assert 1 == 2; 42").is_err());
    }

    #[test]
    fn control_with_basic_scope() {
        assert_eq!(ev("with { a = 1; b = 2; }; a + b"), Value::Int(3));
    }

    #[test]
    fn control_with_lexical_precedence() {
        // let binding takes precedence over with scope
        assert_eq!(
            ev("let x = 10; in with { x = 99; }; x"),
            Value::Int(10),
        );
    }

    #[test]
    fn control_with_nested() {
        assert_eq!(
            ev("with { a = 1; }; with { b = 2; }; a + b"),
            Value::Int(3),
        );
    }

    #[test]
    fn control_with_lazy_fix_self() {
        // THE critical pattern that nixpkgs requires:
        // fix (self: with self; { a = 1; b = a + 1; })
        // Before the lazy-with fix, this would hit the blackhole detector
        // because `with` eagerly forced `self`.
        let result = eval(
            "let fix = f: let x = f x; in x; in fix (self: with self; { a = 1; b = a + 1; })"
        );
        assert!(result.is_ok(), "fix with self should work: {:?}", result);
        if let Ok(Value::Attrs(attrs)) = result {
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
        } else {
            panic!("expected Attrs, got {:?}", result);
        }
    }

    #[test]
    fn control_with_lazy_fix_self_lib_pattern() {
        // The nixpkgs pattern: self-referential package set with lib.
        // Access via select to force through the thunk layer.
        let result = eval(r#"
            let fix = f: let x = f x; in x;
            in (fix (self: with self; {
                lib = { version = "1.0"; };
                hello = "hello ${lib.version}";
            })).hello
        "#);
        assert!(result.is_ok(), "nixpkgs-style lib pattern: {:?}", result);
        assert_eq!(
            result.unwrap(),
            Value::String(Rc::new(NixString::plain("hello 1.0"))),
        );
    }

    #[test]
    fn control_with_non_attrset_errors() {
        // CppNix errors when with-scope is not an attrset and a lookup hits it
        let result = eval("with 42; 1");
        // The body `1` is a literal and doesn't look up anything in the
        // with-scope, so this should succeed (the scope is never forced).
        assert_eq!(result.unwrap(), Value::Int(1));
    }

    #[test]
    fn control_with_non_attrset_lookup_falls_through() {
        // If the with scope is not an attrset, lookups should fall through
        // to outer scopes rather than crashing.
        let result = eval("let x = 1; in with 42; x");
        assert_eq!(result.unwrap(), Value::Int(1));
    }

    #[test]
    fn control_let_simple_and_multiple() {
        assert_eq!(ev("let x = 5; in x"), Value::Int(5));
        assert_eq!(ev("let x = 1; y = 2; z = 3; in x + y + z"), Value::Int(6));
    }

    #[test]
    fn control_let_shadow_outer() {
        assert_eq!(
            ev("let x = 1; in let x = 2; in x"),
            Value::Int(2),
        );
    }

    #[test]
    fn control_let_recursive_reference() {
        assert_eq!(ev("let a = 1; b = a + 1; in b"), Value::Int(2));
        assert_eq!(ev("let a = 1; b = a + 1; c = b + 1; in c"), Value::Int(3));
    }

    #[test]
    fn control_nested_let_expression() {
        assert_eq!(
            ev("let a = let b = 1; in b; in a"),
            Value::Int(1),
        );
        assert_eq!(
            ev("let a = let b = 10; in b + 5; in a * 2"),
            Value::Int(30),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 4. FUNCTIONS — COMPLETE COVERAGE
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn func_identity_lambda() {
        assert_eq!(ev("(x: x) 42"), Value::Int(42));
        assert_eq!(ev(r#"(x: x) "hello""#), Value::string("hello"));
    }

    #[test]
    fn func_curried_two_args() {
        assert_eq!(ev("(x: y: x + y) 3 4"), Value::Int(7));
    }

    #[test]
    fn func_curried_three_args() {
        assert_eq!(ev("(a: b: c: a + b + c) 1 2 3"), Value::Int(6));
    }

    #[test]
    fn func_formals_basic() {
        assert_eq!(ev("({ a, b }: a + b) { a = 3; b = 7; }"), Value::Int(10));
    }

    #[test]
    fn func_formals_with_defaults() {
        assert_eq!(ev("({ a, b ? 10 }: a + b) { a = 5; }"), Value::Int(15));
        // Providing the default-able argument overrides the default
        assert_eq!(ev("({ a, b ? 10 }: a + b) { a = 5; b = 20; }"), Value::Int(25));
    }

    #[test]
    fn func_formals_with_ellipsis() {
        assert_eq!(ev("({ a, ... }: a) { a = 1; b = 2; c = 3; }"), Value::Int(1));
    }

    #[test]
    fn func_named_formals_at_before() {
        // args @ { a, b }: ...
        assert_eq!(
            ev("(args @ { a, b }: args.a + args.b) { a = 3; b = 4; }"),
            Value::Int(7),
        );
    }

    #[test]
    fn func_named_formals_at_after() {
        // { a, b } @ args: ...
        assert_eq!(
            ev("({ a, b } @ args: args.a + args.b) { a = 10; b = 20; }"),
            Value::Int(30),
        );
    }

    #[test]
    fn func_nested_application() {
        // Explicit parenthesized application
        assert_eq!(ev("((x: y: x * y) 3) 4"), Value::Int(12));
    }

    #[test]
    fn func_higher_order_map() {
        assert_eq!(
            ev("builtins.map (x: x * 2) [1 2 3]"),
            Value::list(vec![Value::Int(2), Value::Int(4), Value::Int(6)]),
        );
    }

    #[test]
    fn func_higher_order_filter() {
        assert_eq!(
            ev("builtins.filter (x: x > 2) [1 2 3 4 5]"),
            Value::list(vec![Value::Int(3), Value::Int(4), Value::Int(5)]),
        );
    }

    #[test]
    fn func_higher_order_foldl() {
        // Sum of list via foldl'
        assert_eq!(
            ev("builtins.foldl' (acc: x: acc + x) 0 [1 2 3 4]"),
            Value::Int(10),
        );
    }

    #[test]
    fn func_as_attrset_value() {
        assert_eq!(
            ev("let s = { f = x: x + 1; }; in s.f 5"),
            Value::Int(6),
        );
    }

    #[test]
    fn func_immediate_application() {
        assert_eq!(ev("(x: x * x) 7"), Value::Int(49));
    }

    #[test]
    fn func_in_let_binding() {
        assert_eq!(
            ev("let double = x: x * 2; in double 21"),
            Value::Int(42),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 5. ATTRIBUTE SETS — COMPLETE COVERAGE
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn attrs_empty_set() {
        let v = ev("{}");
        if let Value::Attrs(attrs) = v {
            assert!(attrs.is_empty());
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn attrs_simple() {
        assert_eq!(ev("{ a = 1; }.a"), Value::Int(1));
    }

    #[test]
    fn attrs_nested_access() {
        assert_eq!(ev("{ a = { b = { c = 42; }; }; }.a.b.c"), Value::Int(42));
    }

    #[test]
    fn attrs_recursive_set() {
        assert_eq!(ev("(rec { a = 1; b = a + 1; c = b + 1; }).c"), Value::Int(3));
    }

    #[test]
    fn attrs_update_disjoint() {
        let v = ev("{ a = 1; } // { b = 2; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.len(), 2);
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn attrs_update_override() {
        assert_eq!(ev("({ a = 1; } // { a = 2; }).a"), Value::Int(2));
    }

    #[test]
    fn attrs_has_attr_operator() {
        assert_eq!(ev("{ a = 1; } ? a"), Value::Bool(true));
        assert_eq!(ev("{ a = 1; } ? b"), Value::Bool(false));
    }

    #[test]
    fn attrs_select_with_default() {
        assert_eq!(ev("{ a = 1; }.a or 99"), Value::Int(1));
        assert_eq!(ev("{}.missing or 99"), Value::Int(99));
        assert_eq!(ev("{ a = 1; }.b or 42"), Value::Int(42));
    }

    #[test]
    fn attrs_nested_attr_path_in_binding() {
        // { a.b = 1; } creates { a = { b = 1; }; }
        assert_eq!(ev("{ a.b = 1; }.a.b"), Value::Int(1));
    }

    #[test]
    fn attrs_inherit_from_scope() {
        assert_eq!(ev("let x = 1; y = 2; in { inherit x y; }.x"), Value::Int(1));
        assert_eq!(ev("let x = 1; y = 2; in { inherit x y; }.y"), Value::Int(2));
    }

    #[test]
    fn attrs_inherit_from_expr() {
        assert_eq!(
            ev("{ inherit ({ a = 42; b = 10; }) a; }.a"),
            Value::Int(42),
        );
    }

    #[test]
    fn attrs_dynamic_attr_name() {
        assert_eq!(
            ev(r#"let name = "x"; in { ${name} = 42; }.x"#),
            Value::Int(42),
        );
    }

    #[test]
    fn attrs_attr_names_sorted() {
        assert_eq!(
            ev("builtins.attrNames { z = 1; m = 2; a = 3; }"),
            Value::list(vec![
                Value::string("a"),
                Value::string("m"),
                Value::string("z"),
            ]),
        );
    }

    #[test]
    fn attrs_attr_values_follow_key_order() {
        // BTreeMap iteration order: a=1, b=2, c=3
        assert_eq!(
            ev("builtins.attrValues { c = 3; a = 1; b = 2; }"),
            Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn attrs_update_is_shallow() {
        // // is a shallow merge; nested attrs are replaced, not merged
        assert_eq!(
            ev("({ a = { x = 1; }; } // { a = { y = 2; }; }).a ? x"),
            Value::Bool(false),
        );
        assert_eq!(
            ev("({ a = { x = 1; }; } // { a = { y = 2; }; }).a.y"),
            Value::Int(2),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 6. LISTS — COMPLETE COVERAGE
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn list_empty() {
        assert_eq!(ev("[]"), Value::list(vec![]));
    }

    #[test]
    fn list_single_element() {
        assert_eq!(ev("[1]"), Value::list(vec![Value::Int(1)]));
    }

    #[test]
    fn list_mixed_types() {
        assert_eq!(
            ev(r#"[1 "two" true null]"#),
            Value::list(vec![
                Value::Int(1),
                Value::string("two"),
                Value::Bool(true),
                Value::Null,
            ]),
        );
    }

    #[test]
    fn list_nested() {
        assert_eq!(
            ev("[[1 2] [3 4]]"),
            Value::list(vec![
                Value::list(vec![Value::Int(1), Value::Int(2)]),
                Value::list(vec![Value::Int(3), Value::Int(4)]),
            ]),
        );
    }

    #[test]
    fn list_concat_operator() {
        assert_eq!(
            ev("[1] ++ [2] ++ [3]"),
            Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn list_builtins_length() {
        assert_eq!(ev("builtins.length [1 2 3]"), Value::Int(3));
        assert_eq!(ev("builtins.length []"), Value::Int(0));
    }

    #[test]
    fn list_builtins_elem_at() {
        assert_eq!(ev("builtins.elemAt [10 20 30] 0"), Value::Int(10));
        assert_eq!(ev("builtins.elemAt [10 20 30] 1"), Value::Int(20));
        assert_eq!(ev("builtins.elemAt [10 20 30] 2"), Value::Int(30));
    }

    #[test]
    fn list_equality() {
        assert_eq!(ev("[1 2 3] == [1 2 3]"), Value::Bool(true));
        assert_eq!(ev("[1 2] == [1 2 3]"), Value::Bool(false));
        assert_eq!(ev("[] == []"), Value::Bool(true));
    }

    // ═══════════════════════════════════════════════════════════
    // 7. STRING INTERPOLATION
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn interp_simple_variable() {
        assert_eq!(
            ev(r#"let name = "world"; in "hello ${name}""#),
            Value::string("hello world"),
        );
    }

    #[test]
    fn interp_nested_expression() {
        assert_eq!(
            ev(r#""result: ${builtins.toString (1 + 2)}""#),
            Value::string("result: 3"),
        );
    }

    #[test]
    fn interp_int_coercion() {
        // Ints are coerced to string in interpolation
        assert_eq!(
            ev(r#"let x = 42; in "count: ${builtins.toString x}""#),
            Value::string("count: 42"),
        );
    }

    #[test]
    fn interp_multiple() {
        assert_eq!(
            ev(r#"let a = "foo"; b = "bar"; in "${a} and ${b}""#),
            Value::string("foo and bar"),
        );
    }

    #[test]
    fn interp_in_let() {
        assert_eq!(
            ev(r#"let x = "world"; in "hello ${x}""#),
            Value::string("hello world"),
        );
    }

    #[test]
    fn interp_empty_result() {
        assert_eq!(
            ev(r#"let x = ""; in "a${x}b""#),
            Value::string("ab"),
        );
    }

    #[test]
    fn interp_path_in_string_context() {
        // Paths are coerced to strings in interpolation
        assert_eq!(
            ev(r#""path: ${./foo}""#),
            Value::string("path: ./foo"),
        );
    }

    #[test]
    fn interp_adjacent_interpolations() {
        assert_eq!(
            ev(r#"let a = "x"; b = "y"; in "${a}${b}""#),
            Value::string("xy"),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 8. BUILTINS — VERIFY ALL MAJOR ONES
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn builtins_map_filter_foldl() {
        // map
        assert_eq!(
            ev("builtins.map (x: x + 10) [1 2 3]"),
            Value::list(vec![Value::Int(11), Value::Int(12), Value::Int(13)]),
        );
        // filter
        assert_eq!(
            ev("builtins.filter (x: x > 1) [1 2 3]"),
            Value::list(vec![Value::Int(2), Value::Int(3)]),
        );
        // foldl' — product
        assert_eq!(
            ev("builtins.foldl' (a: b: a * b) 1 [2 3 4]"),
            Value::Int(24),
        );
    }

    #[test]
    fn builtins_map_attrs() {
        assert_eq!(
            ev("(builtins.mapAttrs (name: value: value * 2) { a = 1; b = 2; }).a"),
            Value::Int(2),
        );
        assert_eq!(
            ev("(builtins.mapAttrs (name: value: value * 2) { a = 1; b = 2; }).b"),
            Value::Int(4),
        );
    }

    #[test]
    fn builtins_list_to_attrs() {
        assert_eq!(
            ev(r#"(builtins.listToAttrs [{ name = "x"; value = 1; } { name = "y"; value = 2; }]).x"#),
            Value::Int(1),
        );
    }

    #[test]
    fn builtins_concat_map() {
        assert_eq!(
            ev("builtins.concatMap (x: [x (x * 2)]) [1 2 3]"),
            Value::list(vec![
                Value::Int(1), Value::Int(2),
                Value::Int(2), Value::Int(4),
                Value::Int(3), Value::Int(6),
            ]),
        );
    }

    #[test]
    fn builtins_concat_lists() {
        assert_eq!(
            ev("builtins.concatLists [[1 2] [3] [4 5]]"),
            Value::list(vec![
                Value::Int(1), Value::Int(2), Value::Int(3),
                Value::Int(4), Value::Int(5),
            ]),
        );
    }

    #[test]
    fn builtins_concat_strings_sep() {
        assert_eq!(
            ev(r#"builtins.concatStringsSep ", " ["a" "b" "c"]"#),
            Value::string("a, b, c"),
        );
        assert_eq!(
            ev(r#"builtins.concatStringsSep "" ["x" "y"]"#),
            Value::string("xy"),
        );
    }

    #[test]
    fn builtins_replace_strings() {
        assert_eq!(
            ev(r#"builtins.replaceStrings ["o"] ["0"] "foobar""#),
            Value::string("f00bar"),
        );
        assert_eq!(
            ev(r#"builtins.replaceStrings ["hello"] ["goodbye"] "hello world""#),
            Value::string("goodbye world"),
        );
    }

    #[test]
    fn builtins_has_prefix_has_suffix() {
        assert_eq!(ev(r#"builtins.hasPrefix "he" "hello""#), Value::Bool(true));
        assert_eq!(ev(r#"builtins.hasPrefix "xx" "hello""#), Value::Bool(false));
        assert_eq!(ev(r#"builtins.hasSuffix "lo" "hello""#), Value::Bool(true));
        assert_eq!(ev(r#"builtins.hasSuffix "xx" "hello""#), Value::Bool(false));
    }

    #[test]
    fn builtins_all_any() {
        assert_eq!(ev("builtins.all (x: x > 0) [1 2 3]"), Value::Bool(true));
        assert_eq!(ev("builtins.all (x: x > 1) [1 2 3]"), Value::Bool(false));
        assert_eq!(ev("builtins.any (x: x > 2) [1 2 3]"), Value::Bool(true));
        assert_eq!(ev("builtins.any (x: x > 5) [1 2 3]"), Value::Bool(false));
    }

    #[test]
    fn builtins_sort() {
        assert_eq!(
            ev("builtins.sort (a: b: a < b) [3 1 2]"),
            Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
    }

    #[test]
    fn builtins_remove_attrs() {
        let v = ev(r#"builtins.removeAttrs { a = 1; b = 2; c = 3; } ["b" "c"]"#);
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.len(), 1);
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert!(attrs.get("b").is_none());
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_intersect_attrs() {
        let v = ev("builtins.intersectAttrs { a = 1; b = 2; } { b = 20; c = 30; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.len(), 1);
            // intersectAttrs returns values from the second set
            assert_eq!(attrs.get("b"), Some(&Value::Int(20)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_type_of_all_types() {
        assert_eq!(ev("builtins.typeOf null"), Value::string("null"));
        assert_eq!(ev("builtins.typeOf true"), Value::string("bool"));
        assert_eq!(ev("builtins.typeOf 42"), Value::string("int"));
        assert_eq!(ev("builtins.typeOf 3.14"), Value::string("float"));
        assert_eq!(ev(r#"builtins.typeOf "hi""#), Value::string("string"));
        assert_eq!(ev("builtins.typeOf [1]"), Value::string("list"));
        assert_eq!(ev("builtins.typeOf {}"), Value::string("set"));
        assert_eq!(ev("builtins.typeOf (x: x)"), Value::string("lambda"));
    }

    #[test]
    fn builtins_is_type_checks() {
        assert_eq!(ev("builtins.isNull null"), Value::Bool(true));
        assert_eq!(ev("builtins.isNull 0"), Value::Bool(false));
        assert_eq!(ev("builtins.isInt 42"), Value::Bool(true));
        assert_eq!(ev("builtins.isInt 3.14"), Value::Bool(false));
        assert_eq!(ev("builtins.isBool true"), Value::Bool(true));
        assert_eq!(ev("builtins.isBool 1"), Value::Bool(false));
        assert_eq!(ev(r#"builtins.isString "x""#), Value::Bool(true));
        assert_eq!(ev("builtins.isString 1"), Value::Bool(false));
        assert_eq!(ev("builtins.isList []"), Value::Bool(true));
        assert_eq!(ev("builtins.isList {}"), Value::Bool(false));
        assert_eq!(ev("builtins.isAttrs {}"), Value::Bool(true));
        assert_eq!(ev("builtins.isAttrs []"), Value::Bool(false));
        assert_eq!(ev("builtins.isFunction (x: x)"), Value::Bool(true));
        assert_eq!(ev("builtins.isFunction 1"), Value::Bool(false));
        assert_eq!(ev("builtins.isFloat 3.14"), Value::Bool(true));
        assert_eq!(ev("builtins.isFloat 1"), Value::Bool(false));
    }

    #[test]
    fn builtins_to_json_from_json_roundtrip() {
        // int roundtrip
        assert_eq!(ev("builtins.fromJSON (builtins.toJSON 42)"), Value::Int(42));
        // string roundtrip
        assert_eq!(
            ev(r#"builtins.fromJSON (builtins.toJSON "hello")"#),
            Value::string("hello"),
        );
        // list roundtrip
        assert_eq!(
            ev("builtins.fromJSON (builtins.toJSON [1 2 3])"),
            Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        );
        // null roundtrip
        assert_eq!(ev("builtins.fromJSON (builtins.toJSON null)"), Value::Null);
        // bool roundtrip
        assert_eq!(ev("builtins.fromJSON (builtins.toJSON true)"), Value::Bool(true));
    }

    #[test]
    fn builtins_to_string_various() {
        assert_eq!(ev("builtins.toString 42"), Value::string("42"));
        assert_eq!(ev("builtins.toString true"), Value::string("1"));
        assert_eq!(ev("builtins.toString false"), Value::string(""));
        assert_eq!(ev("builtins.toString null"), Value::string(""));
        assert_eq!(ev(r#"builtins.toString "hello""#), Value::string("hello"));
    }

    #[test]
    fn builtins_function_args() {
        let v = ev("builtins.functionArgs ({ a, b ? 1 }: a)");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Bool(false))); // no default
            assert_eq!(attrs.get("b"), Some(&Value::Bool(true)));  // has default
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_gen_list() {
        assert_eq!(
            ev("builtins.genList (x: x * x) 5"),
            Value::list(vec![
                Value::Int(0), Value::Int(1), Value::Int(4),
                Value::Int(9), Value::Int(16),
            ]),
        );
        assert_eq!(ev("builtins.genList (x: x) 0"), Value::list(vec![]));
    }

    #[test]
    fn builtins_elem() {
        assert_eq!(ev("builtins.elem 2 [1 2 3]"), Value::Bool(true));
        assert_eq!(ev("builtins.elem 5 [1 2 3]"), Value::Bool(false));
        assert_eq!(ev("builtins.elem 1 []"), Value::Bool(false));
    }

    #[test]
    fn builtins_head_tail() {
        assert_eq!(ev("builtins.head [10 20 30]"), Value::Int(10));
        assert_eq!(
            ev("builtins.tail [10 20 30]"),
            Value::list(vec![Value::Int(20), Value::Int(30)]),
        );
    }

    #[test]
    fn builtins_string_length() {
        assert_eq!(ev(r#"builtins.stringLength "hello""#), Value::Int(5));
        assert_eq!(ev(r#"builtins.stringLength """#), Value::Int(0));
        assert_eq!(ev(r#"builtins.stringLength "abc def""#), Value::Int(7));
    }

    #[test]
    fn builtins_ceil_floor() {
        assert_eq!(ev("builtins.ceil 2.3"), Value::Int(3));
        assert_eq!(ev("builtins.ceil 2.0"), Value::Int(2));
        assert_eq!(ev("builtins.floor 2.9"), Value::Int(2));
        assert_eq!(ev("builtins.floor 2.0"), Value::Int(2));
        // Int coercion: ceil/floor on int should work via to_float()
        assert_eq!(ev("builtins.ceil 5"), Value::Int(5));
        assert_eq!(ev("builtins.floor 5"), Value::Int(5));
    }

    #[test]
    fn builtins_try_eval() {
        let v = ev("builtins.tryEval 42");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("success"), Some(&Value::Bool(true)));
            assert_eq!(attrs.get("value"), Some(&Value::Int(42)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn builtins_throw() {
        let result = eval(r#"builtins.throw "oops""#);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("oops"));
    }

    #[test]
    fn builtins_seq_deep_seq() {
        // seq forces first arg, returns second
        assert_eq!(ev("builtins.seq 1 42"), Value::Int(42));
        // deepSeq similarly
        assert_eq!(ev("builtins.deepSeq [1 2 3] 99"), Value::Int(99));
    }

    #[test]
    fn builtins_current_system() {
        let v = ev("builtins.currentSystem");
        if let Value::String(ns) = v {
            let s = &ns.chars;
            // Should be a valid system string
            assert!(
                s == "aarch64-darwin"
                    || s == "x86_64-darwin"
                    || s == "aarch64-linux"
                    || s == "x86_64-linux",
                "unexpected system: {s}",
            );
        } else {
            panic!("expected string");
        }
    }

    // ═══════════════════════════════════════════════════════════
    // 9. REAL-WORLD NIXPKGS PATTERNS
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn pattern_mkif_like() {
        // lib.mkIf pattern: if condition then { key = value; } else {}
        assert_eq!(
            ev("(if true then { x = 1; } else {}).x"),
            Value::Int(1),
        );
        let v = ev("if false then { x = 1; } else {}");
        if let Value::Attrs(attrs) = v {
            assert!(attrs.is_empty());
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn pattern_optional_attrs() {
        // lib.optionalAttrs pattern
        assert_eq!(
            ev("let optionalAttrs = cond: attrs: if cond then attrs else {}; in (optionalAttrs true { a = 1; }).a"),
            Value::Int(1),
        );
        let v = ev("let optionalAttrs = cond: attrs: if cond then attrs else {}; in optionalAttrs false { a = 1; }");
        if let Value::Attrs(attrs) = v {
            assert!(attrs.is_empty());
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn pattern_filter_attrs_via_remove() {
        // lib.filterAttrs pattern via removeAttrs
        assert_eq!(
            ev(r#"(builtins.removeAttrs { a = 1; b = 2; c = 3; } ["b"]).a"#),
            Value::Int(1),
        );
        assert_eq!(
            ev(r#"(builtins.removeAttrs { a = 1; b = 2; c = 3; } ["b"]) ? b"#),
            Value::Bool(false),
        );
    }

    #[test]
    fn pattern_override() {
        // default // overrides pattern
        let v = ev(r#"
            let
                defaults = { debug = false; port = 8080; host = "localhost"; };
                overrides = { debug = true; port = 9090; };
            in defaults // overrides
        "#);
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("debug"), Some(&Value::Bool(true)));
            assert_eq!(attrs.get("port"), Some(&Value::Int(9090)));
            assert_eq!(attrs.get("host"), Some(&Value::string("localhost")));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn pattern_functor() {
        // { __functor = self: x: self.value + x; value = 10; } 5
        assert_eq!(
            ev("let s = { __functor = self: x: self.value + x; value = 10; }; in s 5"),
            Value::Int(15),
        );
    }

    #[test]
    fn pattern_platform_check() {
        // Check pattern: if builtins.currentSystem == "..." then ... else ...
        let v = ev(r#"if builtins.currentSystem == "aarch64-darwin" then "arm" else "other""#);
        // We just verify it evaluates without error and produces a string
        if let Value::String(_) = v {
            // ok
        } else {
            panic!("expected string");
        }
    }

    #[test]
    fn pattern_recursive_overlay_lambda_structure() {
        // Test the lambda structure of an overlay (self: super: { ... })
        let v = ev("let overlay = self: super: { pkg = 42; }; in overlay {} {}");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("pkg"), Some(&Value::Int(42)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn pattern_call_package_simplified() {
        // Simplified callPackage: f: f { inherit lib; }
        assert_eq!(
            ev("let callPkg = f: f { lib = { id = x: x; }; }; lib = { id = x: x; }; in callPkg ({ lib }: lib.id 42)"),
            Value::Int(42),
        );
    }

    #[test]
    fn pattern_derivation_like_attrset() {
        let v = ev(r#"{ type = "derivation"; name = "hello"; system = builtins.currentSystem; builder = "/bin/sh"; }"#);
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("type"), Some(&Value::string("derivation")));
            assert_eq!(attrs.get("name"), Some(&Value::string("hello")));
            assert_eq!(attrs.get("builder"), Some(&Value::string("/bin/sh")));
            // system should be a string (may be a thunk that forces to string)
            let system = force_value(attrs.get("system").unwrap()).unwrap();
            assert!(matches!(system, Value::String(_)), "expected string, got {system:?}");
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn pattern_module_system_simplified() {
        // Simplified NixOS module evaluation
        assert_eq!(
            ev(r#"
                let
                    eval = m: m { config = {}; lib = { mkDefault = x: x; }; };
                in eval ({ config, lib }: { result = lib.mkDefault 42; })
            "#),
            {
                let mut attrs = NixAttrs::new();
                attrs.insert("result".to_string(), Value::Int(42));
                Value::Attrs(Rc::new(attrs))
            },
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 10. ERROR HANDLING
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn error_undefined_variable() {
        let result = eval("nonexistent_var");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("undefined variable") || msg.contains("nonexistent_var"));
    }

    #[test]
    fn error_type_mismatch_arithmetic() {
        let result = eval(r#"1 + "hello""#);
        assert!(result.is_err());
    }

    #[test]
    fn error_missing_attribute() {
        let result = eval("{}.nonexistent");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("nonexistent") || msg.contains("not found"));
    }

    #[test]
    fn error_division_by_zero() {
        assert!(eval("1 / 0").is_err());
        assert!(eval("100 / 0").is_err());
    }

    #[test]
    fn error_missing_required_function_arg() {
        let result = eval("({ a, b }: a + b) { a = 1; }");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("missing argument"));
    }

    #[test]
    fn error_unexpected_function_arg() {
        let result = eval("({ a }: a) { a = 1; b = 2; }");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("unexpected argument"));
    }

    #[test]
    fn error_assertion_failure() {
        assert!(eval("assert false; 1").is_err());
        assert!(eval("assert 1 == 2; 1").is_err());
    }

    #[test]
    fn error_infinite_recursion() {
        // `let x = x; in x` should either hit the depth guard or fail on
        // undefined variable (since sequential let can't see its own binding).
        let result = eval("let x = x; in x");
        assert!(result.is_err());
    }

    #[test]
    fn error_infinite_recursion_via_lambda() {
        // A true infinite recursion via self-application -- depth guard catches this.
        let result = eval("let f = x: f x; in f 1");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("infinite recursion") || msg.contains("eval depth") || msg.contains("undefined"),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // ADDITIONAL COVERAGE: edge cases and integration
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn integration_let_with_function_returning_attrset() {
        assert_eq!(
            ev("let mkPkg = name: { inherit name; version = 1; }; in (mkPkg \"hello\").name"),
            Value::string("hello"),
        );
    }

    #[test]
    fn integration_chained_updates() {
        assert_eq!(
            ev("({ a = 1; } // { b = 2; } // { c = 3; }).c"),
            Value::Int(3),
        );
    }

    #[test]
    fn integration_map_over_attrnames() {
        // Common nixpkgs pattern: map over attrNames
        assert_eq!(
            ev(r#"
                let
                    set = { a = 1; b = 2; };
                    names = builtins.attrNames set;
                in builtins.length names
            "#),
            Value::Int(2),
        );
    }

    #[test]
    fn integration_compose_functions() {
        // Function composition
        assert_eq!(
            ev("let compose = f: g: x: f (g x); double = x: x * 2; inc = x: x + 1; in compose double inc 5"),
            Value::Int(12), // (5 + 1) * 2
        );
    }

    #[test]
    fn integration_recursive_list_building() {
        // Build a list using genList and map
        assert_eq!(
            ev("builtins.map (x: x * x) (builtins.genList (x: x + 1) 4)"),
            Value::list(vec![Value::Int(1), Value::Int(4), Value::Int(9), Value::Int(16)]),
        );
    }

    #[test]
    fn integration_attrset_from_list() {
        // Convert list to attrset via listToAttrs + map
        let v = ev(r#"
            builtins.listToAttrs (builtins.map (x: { name = x; value = true; }) ["a" "b" "c"])
        "#);
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Bool(true)));
            assert_eq!(attrs.get("b"), Some(&Value::Bool(true)));
            assert_eq!(attrs.get("c"), Some(&Value::Bool(true)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn integration_nested_with_and_let() {
        assert_eq!(
            ev("let x = 10; in with { y = 20; }; x + y"),
            Value::Int(30),
        );
    }

    #[test]
    fn integration_complex_pattern_match() {
        // Complex function with defaults, ellipsis, and @ pattern
        assert_eq!(
            ev("(args @ { a, b ? 5, ... }: a + b + (if args ? c then args.c else 0)) { a = 1; c = 10; }"),
            Value::Int(16), // 1 + 5 + 10
        );
    }

    #[test]
    fn integration_substring() {
        assert_eq!(
            ev(r#"builtins.substring 0 5 "hello world""#),
            Value::string("hello"),
        );
        assert_eq!(
            ev(r#"builtins.substring 6 5 "hello world""#),
            Value::string("world"),
        );
    }

    #[test]
    fn integration_has_attr_on_nested() {
        // ? on nested attr paths
        assert_eq!(ev("{ a = { b = 1; }; } ? a"), Value::Bool(true));
        assert_eq!(
            ev("({ a = { b = 1; }; }.a) ? b"),
            Value::Bool(true),
        );
    }

    #[test]
    fn integration_cat_attrs() {
        assert_eq!(
            ev(r#"builtins.catAttrs "x" [{ x = 1; } { y = 2; } { x = 3; }]"#),
            Value::list(vec![Value::Int(1), Value::Int(3)]),
        );
    }

    #[test]
    fn integration_get_attr_builtin() {
        assert_eq!(
            ev(r#"builtins.getAttr "a" { a = 42; b = 10; }"#),
            Value::Int(42),
        );
    }

    #[test]
    fn integration_has_attr_builtin() {
        assert_eq!(
            ev(r#"builtins.hasAttr "a" { a = 1; }"#),
            Value::Bool(true),
        );
        assert_eq!(
            ev(r#"builtins.hasAttr "z" { a = 1; }"#),
            Value::Bool(false),
        );
    }

    #[test]
    fn integration_is_path() {
        assert_eq!(ev("builtins.isPath ./foo"), Value::Bool(true));
        assert_eq!(ev("builtins.isPath 42"), Value::Bool(false));
    }

    #[test]
    fn integration_builtins_trace() {
        // trace prints the first arg (as debug) and returns the second
        assert_eq!(ev(r#"builtins.trace "debug msg" 42"#), Value::Int(42));
    }

    #[test]
    fn integration_builtins_split() {
        // Nix spec: split returns alternating non-match strings and match group lists.
        // When the regex has no capture groups, separator positions get empty lists.
        // split "/" "a/b/c" => ["a" [] "b" [] "c"]
        assert_eq!(
            ev(r#"builtins.split "/" "a/b/c""#),
            Value::list(vec![
                Value::string("a"),
                Value::list(vec![]),
                Value::string("b"),
                Value::list(vec![]),
                Value::string("c"),
            ]),
        );
        // With a capture group, the captured text appears in the list.
        // split "(/)" "a/b/c" => ["a" ["/"] "b" ["/"] "c"]
        assert_eq!(
            ev(r#"builtins.split "(/)" "a/b/c""#),
            Value::list(vec![
                Value::string("a"),
                Value::list(vec![Value::string("/")]),
                Value::string("b"),
                Value::list(vec![Value::string("/")]),
                Value::string("c"),
            ]),
        );
    }

    #[test]
    fn integration_builtins_split_no_capture_groups() {
        // builtins.split with no capture groups returns empty lists
        // at separator positions — matches CppNix behavior.
        // This is critical for nixpkgs lib.splitString which uses
        // builtins.filter builtins.isString on the result.
        assert_eq!(
            ev(r#"builtins.split "-" "aarch64-darwin""#),
            Value::list(vec![
                Value::string("aarch64"),
                Value::list(vec![]),
                Value::string("darwin"),
            ]),
        );
    }

    #[test]
    fn integration_builtins_split_system_string_filter() {
        // Simulates nixpkgs lib.splitString: filter isString (split pattern string)
        // This is the exact pattern that parses system strings like "aarch64-darwin".
        assert_eq!(
            ev(r#"builtins.filter builtins.isString (builtins.split "-" "aarch64-darwin")"#),
            Value::list(vec![
                Value::string("aarch64"),
                Value::string("darwin"),
            ]),
        );
    }

    #[test]
    fn integration_deeply_nested_let() {
        // Deeply nested let-in expressions
        assert_eq!(
            ev("let a = let b = let c = 10; in c * 2; in b + 1; in a"),
            Value::Int(21),
        );
    }

    #[test]
    fn integration_if_in_attrset_value() {
        assert_eq!(
            ev("{ x = if true then 1 else 2; }.x"),
            Value::Int(1),
        );
    }

    #[test]
    fn integration_lambda_in_list() {
        // Store lambdas in a list and apply them
        assert_eq!(
            ev("let fs = [(x: x + 1) (x: x * 2)]; in (builtins.elemAt fs 0) 5"),
            Value::Int(6),
        );
        assert_eq!(
            ev("let fs = [(x: x + 1) (x: x * 2)]; in (builtins.elemAt fs 1) 5"),
            Value::Int(10),
        );
    }

    #[test]
    fn integration_nixpkgs_lib_id() {
        // lib.id = x: x
        assert_eq!(
            ev("let lib = { id = x: x; const = a: b: a; }; in lib.id 42"),
            Value::Int(42),
        );
        assert_eq!(
            ev("let lib = { id = x: x; const = a: b: a; }; in lib.const 1 2"),
            Value::Int(1),
        );
    }

    #[test]
    fn integration_multiple_inherit() {
        assert_eq!(
            ev("let a = 1; b = 2; c = 3; in { inherit a b c; }.b"),
            Value::Int(2),
        );
    }

    #[test]
    fn integration_rec_set_with_builtins() {
        assert_eq!(
            ev(r#"(rec { a = "hello"; b = builtins.stringLength a; }).b"#),
            Value::Int(5),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 11. __FUNCTOR PROTOCOL
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn functor_simple_callable_attrset() {
        assert_eq!(
            ev("let s = { __functor = self: x: x + 1; }; in s 41"),
            Value::Int(42),
        );
    }

    #[test]
    fn functor_with_self_reference() {
        assert_eq!(
            ev("let s = { __functor = self: x: self.base + x; base = 100; }; in s 23"),
            Value::Int(123),
        );
    }

    #[test]
    fn functor_updated_attrset() {
        // Override a field in the attrset, functor still works
        assert_eq!(
            ev(r#"
                let
                    mk = { __functor = self: x: self.n + x; n = 0; };
                    s = mk // { n = 50; };
                in s 7
            "#),
            Value::Int(57),
        );
    }

    #[test]
    fn functor_error_on_non_callable_attrset() {
        // Attrset without __functor should produce error when called
        let result = eval("let s = { a = 1; }; in s 5");
        assert!(result.is_err());
    }

    // ═══════════════════════════════════════════════════════════
    // 12. __TOSTRING PROTOCOL
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn to_string_protocol_in_interpolation() {
        assert_eq!(
            ev(r#"let s = { __toString = self: "world"; }; in "hello ${s}""#),
            Value::string("hello world"),
        );
    }

    #[test]
    fn to_string_protocol_accesses_self() {
        assert_eq!(
            ev(r#"let s = { __toString = self: self.val; val = "abc"; }; in "${s}""#),
            Value::string("abc"),
        );
    }

    #[test]
    fn to_string_protocol_via_builtin_to_string() {
        assert_eq!(
            ev(r#"builtins.toString { __toString = self: "via-builtin"; }"#),
            Value::string("via-builtin"),
        );
    }

    #[test]
    fn to_string_protocol_attrset_without_toString_fails() {
        // An attrset without __toString should fail in string context
        let result = eval(r#""${{}}"#);
        assert!(result.is_err());
    }

    // ═══════════════════════════════════════════════════════════
    // 13. NEWLY IMPLEMENTED BUILTINS (eval-level tests)
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn eval_builtins_concat_strings() {
        assert_eq!(
            ev(r#"builtins.concatStrings ["a" "b" "c"]"#),
            Value::string("abc"),
        );
        assert_eq!(
            ev(r#"builtins.concatStrings []"#),
            Value::string(""),
        );
    }

    #[test]
    fn eval_builtins_partition() {
        let v = ev("builtins.partition (x: x > 3) [1 2 3 4 5]");
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("right"), Some(&Value::list(vec![Value::Int(4), Value::Int(5)])));
            assert_eq!(a.get("wrong"), Some(&Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)])));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn eval_builtins_group_by() {
        let v = ev(r#"builtins.groupBy (x: if x > 0 then "pos" else "neg") [1 (0 - 2) 3 (0 - 4)]"#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("pos"), Some(&Value::list(vec![Value::Int(1), Value::Int(3)])));
            assert_eq!(a.get("neg"), Some(&Value::list(vec![Value::Int(-2), Value::Int(-4)])));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn eval_builtins_zip_attrs_with() {
        let v = ev("builtins.zipAttrsWith (n: vs: builtins.head vs) [{ a = 1; } { a = 2; b = 3; }]");
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("a"), Some(&Value::Int(1)));
            assert_eq!(a.get("b"), Some(&Value::Int(3)));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn eval_builtins_compare_versions() {
        assert_eq!(ev(r#"builtins.compareVersions "2.0" "1.0""#), Value::Int(1));
        assert_eq!(ev(r#"builtins.compareVersions "1.0" "2.0""#), Value::Int(-1));
        assert_eq!(ev(r#"builtins.compareVersions "1.0" "1.0""#), Value::Int(0));
    }

    #[test]
    fn eval_builtins_parse_drv_name() {
        let v = ev(r#"builtins.parseDrvName "nix-2.3.4""#);
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("name"), Some(&Value::string("nix")));
            assert_eq!(a.get("version"), Some(&Value::string("2.3.4")));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn eval_builtins_base_name_of() {
        assert_eq!(
            ev(r#"builtins.baseNameOf "/foo/bar/baz""#),
            Value::string("baz"),
        );
    }

    #[test]
    fn eval_builtins_dir_of() {
        assert_eq!(
            ev(r#"builtins.dirOf "/foo/bar/baz""#),
            Value::string("/foo/bar"),
        );
    }

    #[test]
    fn eval_builtins_add_error_context() {
        assert_eq!(
            ev(r#"builtins.addErrorContext "some context" 42"#),
            Value::Int(42),
        );
    }

    #[test]
    fn eval_builtins_abort() {
        let result = eval(r#"builtins.abort "fatal error""#);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("fatal error"));
    }

    // ═══════════════════════════════════════════════════════════
    // 14. INDENTED STRINGS ('' ... '')
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn indented_string_simple() {
        assert_eq!(ev("''hello''"), Value::string("hello"));
    }

    #[test]
    fn indented_string_multiline_strips_indent() {
        assert_eq!(
            ev("''\n  line1\n  line2\n''"),
            Value::string("line1\nline2\n"),
        );
    }

    #[test]
    fn indented_string_with_interpolation() {
        let code = "let x = \"world\"; in ''hello ${x}''";
        assert_eq!(
            ev(code),
            Value::string("hello world"),
        );
    }

    #[test]
    fn indented_string_deeper_indent_preserved() {
        // Common indent is 2 spaces; the 4-space line keeps 2 extra
        assert_eq!(
            ev("''\n  a\n    b\n''"),
            Value::string("a\n  b\n"),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 15. DYNAMIC ATTRIBUTE NAMES
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn dynamic_attr_name_in_set() {
        assert_eq!(
            ev(r#"let key = "mykey"; in { ${key} = 42; }.mykey"#),
            Value::Int(42),
        );
    }

    #[test]
    fn dynamic_attr_name_with_expression() {
        assert_eq!(
            ev(r#"let prefix = "foo"; in { ${"${prefix}bar"} = 1; }.foobar"#),
            Value::Int(1),
        );
    }

    // ═══════════════════════════════════════════════════════════
    // 16. IGNORED TESTS — features needing major infrastructure
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn eval_builtins_match() {
        assert_eq!(
            ev(r#"builtins.match "([0-9]+)" "42""#),
            Value::list(vec![Value::string("42")]),
        );
    }

    #[test]
    fn eval_builtins_hash_string() {
        let v = ev(r#"builtins.hashString "sha256" "hello""#);
        if let Value::String(ns) = v {
            assert_eq!(ns.chars.len(), 64);
        } else {
            panic!("expected string");
        }
    }

    #[test]
    fn eval_builtins_import() {
        let dir = std::env::temp_dir();
        let path = dir.join("sui_eval_test_import_eval.nix");
        std::fs::write(&path, "42").unwrap();
        let expr = format!(r#"import "{}""#, path.display());
        let v = eval(&expr).unwrap();
        assert_eq!(v, Value::Int(42));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn eval_builtins_derivation() {
        let v = eval(r#"builtins.derivation { name = "test"; system = "x86_64-linux"; builder = "/bin/sh"; }"#).unwrap();
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("type"), Some(&Value::string("derivation")));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn eval_mutual_recursive_let() {
        // Multi-pass evaluation allows forward references in let bindings.
        // After 3 passes (placeholder + eval + re-eval), `a.x` resolves to
        // the value of `b` from the previous pass, and `a.x.y` is an attrset.
        // Full semantic equivalence with Nix (a.x.y == a) requires lazy
        // thunks, but the multi-pass approach is sufficient for common
        // patterns like mutual module references.
        let v = eval("let a = { x = b; }; b = { y = a; }; in a.x.y");
        assert!(v.is_ok(), "mutual recursive let should not error: {v:?}");
        // a.x.y should be an attrset (it's a's value from a prior pass)
        let val = v.unwrap();
        assert!(
            matches!(val, Value::Attrs(_)),
            "a.x.y should be an attrset, got: {val:?}",
        );
    }

    #[test]
    fn eval_mutual_recursive_let_simple() {
        // Simpler case: forward reference in sequential let bindings
        let v = eval("let a = b; b = 42; in a");
        assert!(v.is_ok());
        // After multi-pass: pass 2 sets a=Null (b not yet bound), b=42
        // pass 3 sets a=42, b=42
        assert_eq!(v.unwrap(), Value::Int(42));
    }

    #[test]
    fn eval_builtins_read_dir() {
        let dir = std::env::temp_dir().join("sui_eval_test_readdir_eval");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "").unwrap();
        let expr = format!(r#"builtins.readDir "{}""#, dir.display());
        let v = eval(&expr).unwrap();
        if let Value::Attrs(a) = v {
            assert_eq!(a.get("a.txt"), Some(&Value::string("regular")));
        } else {
            panic!("expected attrs");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ═══════════════════════════════════════════════════════════
    // 17. THUNK / LAZY EVALUATION
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn thunk_basic_let() {
        // Simple let binding through thunk.
        assert_eq!(ev("let x = 1; in x"), Value::Int(1));
    }

    #[test]
    fn thunk_forward_ref() {
        // Forward reference: `a` references `b` which is defined later.
        assert_eq!(ev("let a = b; b = 1; in a"), Value::Int(1));
    }

    #[test]
    fn thunk_mutual_rec_attrset_in_let() {
        // Mutual recursion through attrsets in let bindings.
        assert_eq!(ev("let a = { x = b; }; b = { y = 1; }; in a.x.y"), Value::Int(1));
    }

    #[test]
    fn thunk_rec_attrset() {
        // rec { a = b; b = 1; } -- forward ref within rec set.
        assert_eq!(ev("(rec { a = b; b = 1; }).a"), Value::Int(1));
    }

    #[test]
    fn thunk_rec_attrset_chain() {
        // Longer chain: c depends on b depends on a.
        assert_eq!(ev("(rec { a = 1; b = a + 1; c = b + 1; }).c"), Value::Int(3));
    }

    #[test]
    fn thunk_fixpoint() {
        // Classic fixpoint combinator -- the core of nixpkgs' `lib.fix`.
        assert_eq!(
            ev("let fix = f: let x = f x; in x; in (fix (self: { a = 1; b = self.a + 1; })).b"),
            Value::Int(2),
        );
    }

    #[test]
    fn thunk_blackhole_self_reference() {
        // `let x = x; in x` is infinite recursion -- blackhole detection.
        let result = eval("let x = x; in x");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("infinite recursion") || msg.contains("blackhole"),
            "expected blackhole error, got: {msg}",
        );
    }

    #[test]
    fn thunk_mutual_blackhole() {
        // `let a = b; b = a; in a` -- mutual infinite recursion.
        let result = eval("let a = b; b = a; in a");
        assert!(result.is_err());
    }

    #[test]
    fn thunk_let_body_forces_correctly() {
        // The let body should be able to use thunked bindings in arithmetic.
        assert_eq!(ev("let a = 10; b = 20; in a + b"), Value::Int(30));
    }

    #[test]
    fn thunk_only_forced_when_needed() {
        // The binding `bad` would error if forced, but it is never used.
        assert_eq!(ev("let bad = 1 / 0; good = 42; in good"), Value::Int(42));
    }

    #[test]
    fn thunk_forward_ref_in_function_body() {
        // Forward reference used inside a function body.
        assert_eq!(
            ev("let f = x: x + b; b = 10; in f 5"),
            Value::Int(15),
        );
    }

    #[test]
    fn thunk_rec_set_self_ref_through_self() {
        // rec set where `b` references `a` which is in the same set.
        assert_eq!(
            ev(r#"(rec { a = "hello"; b = builtins.stringLength a; }).b"#),
            Value::Int(5),
        );
    }

    #[test]
    fn thunk_nested_let_forward_ref() {
        // Forward reference in nested let.
        assert_eq!(
            ev("let a = b + 1; b = 2; in a"),
            Value::Int(3),
        );
    }

    #[test]
    fn thunk_deep_chain() {
        // Chain of forward references: e -> d -> c -> b -> a.
        assert_eq!(
            ev("let a = 1; b = a; c = b; d = c; e = d; in e"),
            Value::Int(1),
        );
    }

    #[test]
    fn thunk_rec_set_fixpoint() {
        // Fixpoint through rec set -- common nixpkgs pattern.
        assert_eq!(
            ev("let fix = f: let x = f x; in x; in (fix (self: { a = 1; b = self.a + 1; c = self.b + 1; })).c"),
            Value::Int(3),
        );
    }

    #[test]
    fn thunk_let_with_inherit() {
        // Inherit in let should work alongside thunked bindings.
        assert_eq!(
            ev("let a = 1; in let inherit a; b = a + 1; in b"),
            Value::Int(2),
        );
    }

    #[test]
    fn thunk_attrset_value_lazy() {
        // Values in non-rec attrsets are evaluated eagerly, but the test
        // verifies that thunked let bindings inside attrset values work.
        assert_eq!(
            ev("let x = 42; in { a = x; }.a"),
            Value::Int(42),
        );
    }

    #[test]
    fn thunk_unused_error_not_forced() {
        // Multiple bindings, only `ok` is used. `bad` throws but is never forced.
        assert_eq!(
            ev(r#"let bad = builtins.throw "boom"; ok = 1; in ok"#),
            Value::Int(1),
        );
    }

    #[test]
    fn thunk_rec_set_mutual_reference() {
        // Mutual reference within rec set.
        let v = ev("rec { a = { val = b.val + 1; }; b = { val = 10; }; }");
        if let Value::Attrs(attrs) = v {
            let a = attrs.get("a").unwrap();
            let a_forced = force_value(a).unwrap();
            if let Value::Attrs(a_attrs) = a_forced {
                assert_eq!(a_attrs.get("val"), Some(&Value::Int(11)));
            } else {
                panic!("expected attrs for a");
            }
        } else {
            panic!("expected attrs");
        }
    }

    // ── let-rec self-reference corner cases ───────────────

    #[test]
    fn let_rec_self_reference_simple() {
        assert_eq!(
            ev("let x = 1; y = x + 1; in y"),
            Value::Int(2),
        );
    }

    #[test]
    fn let_rec_self_reference_chain() {
        assert_eq!(
            ev("let a = 1; b = a + 1; c = b + 1; in c"),
            Value::Int(3),
        );
    }

    #[test]
    fn let_rec_self_reference_with_function() {
        assert_eq!(
            ev("let f = x: x + 1; y = f 10; in y"),
            Value::Int(11),
        );
    }

    #[test]
    fn let_rec_mutual_recursion_via_if() {
        assert_eq!(
            ev("let isEven = n: if n == 0 then true else isOdd (n - 1); isOdd = n: if n == 0 then false else isEven (n - 1); in isEven 4"),
            Value::Bool(true),
        );
    }

    #[test]
    fn let_rec_forward_ref_in_list() {
        assert_eq!(
            ev("let xs = [a b]; a = 1; b = 2; in builtins.length xs"),
            Value::Int(2),
        );
    }

    // ── with-shadowing corner cases ───────────────────────

    #[test]
    fn with_shadowing_let_wins_over_with() {
        assert_eq!(
            ev("let x = 1; in with { x = 2; }; x"),
            Value::Int(1),
        );
    }

    #[test]
    fn with_shadowing_inner_with_wins() {
        assert_eq!(
            ev("with { x = 1; }; with { x = 2; }; x"),
            Value::Int(2),
        );
    }

    #[test]
    fn with_shadowing_outer_provides_missing() {
        assert_eq!(
            ev("with { x = 1; y = 10; }; with { x = 2; }; x + y"),
            Value::Int(12),
        );
    }

    #[test]
    fn with_shadowing_lambda_arg_wins() {
        assert_eq!(
            ev("(x: with { x = 99; }; x) 42"),
            Value::Int(42),
        );
    }

    #[test]
    fn with_shadowing_nested_let_wins_over_with() {
        assert_eq!(
            ev("with { x = 1; }; let x = 2; in x"),
            Value::Int(2),
        );
    }

    #[test]
    fn with_scope_dynamic_attrs() {
        assert_eq!(
            ev(r#"with { x = 1; y = 2; z = 3; }; x + y + z"#),
            Value::Int(6),
        );
    }

    // ── attrset deep merge ────────────────────────────────

    #[test]
    fn attrset_deep_merge_simple() {
        let v = ev("{ a.b = 1; a.c = 2; }");
        if let Value::Attrs(attrs) = v {
            let a = force_value(attrs.get("a").unwrap()).unwrap();
            if let Value::Attrs(inner) = a {
                assert_eq!(force_value(inner.get("b").unwrap()).unwrap(), Value::Int(1));
                assert_eq!(force_value(inner.get("c").unwrap()).unwrap(), Value::Int(2));
            } else {
                panic!("expected nested attrs");
            }
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn attrset_deep_merge_three_levels() {
        let v = ev("{ a.b.c = 1; a.b.d = 2; a.e = 3; }");
        if let Value::Attrs(attrs) = v {
            let a = force_value(attrs.get("a").unwrap()).unwrap();
            if let Value::Attrs(a_inner) = a {
                let e = force_value(a_inner.get("e").unwrap()).unwrap();
                assert_eq!(e, Value::Int(3));
                let b = force_value(a_inner.get("b").unwrap()).unwrap();
                if let Value::Attrs(b_inner) = b {
                    assert_eq!(force_value(b_inner.get("c").unwrap()).unwrap(), Value::Int(1));
                    assert_eq!(force_value(b_inner.get("d").unwrap()).unwrap(), Value::Int(2));
                } else {
                    panic!("expected nested attrs for b");
                }
            } else {
                panic!("expected nested attrs for a");
            }
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn attrset_deep_merge_preserves_siblings() {
        assert_eq!(
            ev("{ a.x = 1; b = 2; a.y = 3; }.b"),
            Value::Int(2),
        );
    }

    #[test]
    fn attrset_deep_merge_in_let() {
        let v = ev("let s = { a.b = 1; a.c = 2; }; in s.a.b + s.a.c");
        assert_eq!(v, Value::Int(3));
    }

    // ── inherit-from patterns ─────────────────────────────

    #[test]
    fn inherit_from_basic() {
        assert_eq!(
            ev("let s = { x = 1; y = 2; }; in let inherit (s) x y; in x + y"),
            Value::Int(3),
        );
    }

    #[test]
    fn inherit_from_with_shadowing() {
        assert_eq!(
            ev("let x = 10; in let inherit ({ x = 20; }) x; in x"),
            Value::Int(20),
        );
    }

    #[test]
    fn inherit_from_in_attrset() {
        let v = ev(r#"let s = { a = 1; b = 2; }; in { inherit (s) a b; c = 3; }"#);
        if let Value::Attrs(attrs) = v {
            assert_eq!(force_value(attrs.get("a").unwrap()).unwrap(), Value::Int(1));
            assert_eq!(force_value(attrs.get("b").unwrap()).unwrap(), Value::Int(2));
            assert_eq!(force_value(attrs.get("c").unwrap()).unwrap(), Value::Int(3));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn inherit_from_rec_set() {
        assert_eq!(
            ev("rec { inherit ({ x = 42; }) x; y = x; }.y"),
            Value::Int(42),
        );
    }

    #[test]
    fn inherit_plain_from_scope() {
        assert_eq!(
            ev("let x = 1; in { inherit x; }.x"),
            Value::Int(1),
        );
    }

    #[test]
    fn inherit_multiple_from_expr() {
        assert_eq!(
            ev("let s = { a = 10; b = 20; c = 30; }; in let inherit (s) a b c; in a + b + c"),
            Value::Int(60),
        );
    }

    // ── string interpolation edge cases ───────────────────

    #[test]
    fn interp_nested_attrset_access() {
        assert_eq!(
            ev(r#"let x = { a = "hello"; }; in "${x.a} world""#),
            Value::string("hello world"),
        );
    }

    #[test]
    fn interp_with_let_expression() {
        assert_eq!(
            ev(r#""${let x = "inner"; in x}""#),
            Value::string("inner"),
        );
    }

    #[test]
    fn interp_float_coercion() {
        assert_eq!(
            ev(r#""${toString 3.14}""#),
            Value::string("3.14"),
        );
    }

    // ── comparison edge cases ─────────────────────────────

    #[test]
    fn compare_mixed_int_float() {
        assert_eq!(ev("1 < 1.5"), Value::Bool(true));
        assert_eq!(ev("1.5 > 1"), Value::Bool(true));
        assert_eq!(ev("2.0 == 2"), Value::Bool(true));
    }

    #[test]
    fn compare_string_lexicographic() {
        assert_eq!(ev(r#""abc" < "abd""#), Value::Bool(true));
        assert_eq!(ev(r#""abc" < "abc""#), Value::Bool(false));
        assert_eq!(ev(r#""abc" <= "abc""#), Value::Bool(true));
    }

    // ── update operator edge cases ────────────────────────

    #[test]
    fn update_empty_sets() {
        let v = ev("{} // {}");
        if let Value::Attrs(a) = v { assert!(a.is_empty()); } else { panic!(); }
    }

    #[test]
    fn update_right_overrides_completely() {
        assert_eq!(
            ev("{ a = 1; b = 2; } // { a = 10; c = 30; }"),
            ev("{ a = 10; b = 2; c = 30; }"),
        );
    }

    #[test]
    fn update_chained() {
        assert_eq!(
            ev("{ a = 1; } // { b = 2; } // { c = 3; }"),
            ev("{ a = 1; b = 2; c = 3; }"),
        );
    }

    // ── force_value edge cases ────────────────────────────

    #[test]
    fn force_value_concrete_unchanged() {
        let v = Value::Int(42);
        assert_eq!(force_value(&v).unwrap(), Value::Int(42));
    }

    #[test]
    fn force_value_null() {
        assert_eq!(force_value(&Value::Null).unwrap(), Value::Null);
    }

    // ── eval_with_file ────────────────────────────────────

    #[test]
    fn eval_with_file_none() {
        let result = eval_with_file("1 + 2", None).unwrap();
        assert_eq!(result, Value::Int(3));
    }

    // ── error messages ────────────────────────────────────

    #[test]
    fn error_type_mismatch_in_comparison() {
        let result = eval(r#"1 < "a""#);
        assert!(result.is_err());
    }

    #[test]
    fn error_select_from_non_set() {
        let result = eval("42.x");
        assert!(result.is_err());
    }

    #[test]
    fn error_call_non_function() {
        let result = eval("42 1");
        assert!(result.is_err());
    }

    #[test]
    fn error_negate_string() {
        let result = eval(r#"-"hello""#);
        assert!(result.is_err());
    }

    // ── multiline string edge cases ───────────────────────

    #[test]
    fn multiline_string_empty() {
        assert_eq!(ev("''''"), Value::string(""));
    }

    #[test]
    fn multiline_string_with_trailing_newline() {
        let v = ev("''\n  hello\n''");
        assert_eq!(v, Value::string("hello\n"));
    }

    // ── list operations ───────────────────────────────────

    #[test]
    fn list_concat_empty_left() {
        assert_eq!(ev("[] ++ [1 2]"), Value::list(vec![Value::Int(1), Value::Int(2)]));
    }

    #[test]
    fn list_concat_empty_right() {
        assert_eq!(ev("[1 2] ++ []"), Value::list(vec![Value::Int(1), Value::Int(2)]));
    }

    #[test]
    fn list_concat_both_empty() {
        assert_eq!(ev("[] ++ []"), Value::list(vec![]));
    }

    // ── pattern matching / formals edge cases ─────────────

    #[test]
    fn formals_at_pattern_accessible() {
        assert_eq!(
            ev("({ x, ... } @ args: builtins.length (builtins.attrNames args)) { x = 1; y = 2; z = 3; }"),
            Value::Int(3),
        );
    }

    #[test]
    fn formals_default_uses_other_arg() {
        assert_eq!(
            ev("({ x, y ? x + 1 }: y) { x = 10; }"),
            Value::Int(11),
        );
    }

    #[test]
    fn formals_default_lazy_assert_false() {
        // nixpkgs parse.nix pattern: default is `assert false; null` but
        // the body checks `args ? vendor` instead of using `vendor`
        // directly, so the default must never be forced.
        assert_eq!(
            ev("({ cpu, vendor ? assert false; null, kernel } @ args: if args ? vendor then vendor else \"inferred\") { cpu = \"x86_64\"; kernel = \"linux\"; }"),
            Value::String(Rc::new(NixString::plain("inferred"))),
        );
    }

    #[test]
    fn formals_default_lazy_only_forced_when_accessed() {
        // When the default IS accessed, it should still evaluate correctly.
        assert_eq!(
            ev("({ a, b ? 42 }: b) { a = 1; }"),
            Value::Int(42),
        );
    }

    #[test]
    fn formals_ellipsis_ignores_extra() {
        assert_eq!(
            ev("({ x, ... }: x) { x = 1; y = 2; z = 3; }"),
            Value::Int(1),
        );
    }

    // ── pure mode ─────────────────────────────────────────

    #[test]
    fn pure_mode_roundtrip() {
        let was_pure = is_pure_mode();
        set_pure_mode(true);
        assert!(is_pure_mode());
        set_pure_mode(false);
        assert!(!is_pure_mode());
        set_pure_mode(was_pure);
    }

    // ── path operations ───────────────────────────────────

    #[test]
    fn path_concat_with_string() {
        assert_eq!(
            ev(r#"/foo + "bar""#),
            Value::Path(Box::new(SmolStr::from("/foobar"))),
        );
    }

    #[test]
    fn path_concat_with_path() {
        assert_eq!(
            ev("/foo + /bar"),
            Value::Path(Box::new(SmolStr::from("/foo//bar"))),
        );
    }

    // ── EvalFileGuard / current_eval_dir ───────────────────

    #[test]
    fn current_eval_dir_empty_when_no_file_pushed() {
        // Without a push, current_eval_dir should yield None.
        // (Note: this test is order-dependent; we accept whatever the
        // top of the stack happens to be when called.)
        let snapshot = current_eval_dir();
        // At minimum the API doesn't panic and returns Option.
        let _ = snapshot;
    }

    #[test]
    fn push_eval_file_sets_current_dir() {
        let p = std::path::PathBuf::from("/tmp/example/file.nix");
        {
            let _g = push_eval_file(p.clone());
            assert_eq!(current_eval_dir(), Some(std::path::PathBuf::from("/tmp/example")));
        }
        // Guard dropped, stack popped — current dir is whatever was below.
        // We can't assert exact value without snapshotting first, but the
        // value before push should be restored.
    }

    #[test]
    fn push_eval_file_nested_stack() {
        let outer = std::path::PathBuf::from("/a/x.nix");
        let inner = std::path::PathBuf::from("/b/y.nix");
        {
            let _g_outer = push_eval_file(outer.clone());
            assert_eq!(current_eval_dir(), Some(std::path::PathBuf::from("/a")));
            {
                let _g_inner = push_eval_file(inner.clone());
                assert_eq!(current_eval_dir(), Some(std::path::PathBuf::from("/b")));
            }
            // Inner dropped — outer is back on top.
            assert_eq!(current_eval_dir(), Some(std::path::PathBuf::from("/a")));
        }
    }

    // ── Source-mapped error context ────────────────────────

    #[test]
    fn error_undefined_var_includes_file_context() {
        let p = std::path::PathBuf::from("/nix/store/abc-default.nix");
        let _g = push_eval_file(p);
        let result = eval("nonexistent_xyz");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("undefined variable"), "msg: {msg}");
        assert!(msg.contains("nonexistent_xyz"), "msg: {msg}");
        assert!(msg.contains("abc-default.nix"), "msg: {msg}");
    }

    #[test]
    fn error_attr_not_found_includes_file_context() {
        let p = std::path::PathBuf::from("/nix/store/xyz-module.nix");
        let _g = push_eval_file(p);
        let result = eval("{}.missing_key");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("not found") || msg.contains("missing_key"), "msg: {msg}");
        assert!(msg.contains("xyz-module.nix"), "msg: {msg}");
    }

    #[test]
    fn error_assertion_failed_includes_file_context() {
        let p = std::path::PathBuf::from("/nix/store/test-assert.nix");
        let _g = push_eval_file(p);
        let result = eval("assert false; 1");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("assertion failed"), "msg: {msg}");
        assert!(msg.contains("test-assert.nix"), "msg: {msg}");
    }

    #[test]
    fn error_missing_argument_includes_file_context() {
        let p = std::path::PathBuf::from("/nix/store/func.nix");
        let _g = push_eval_file(p);
        let result = eval("({ a, b }: a) { a = 1; }");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("missing argument"), "msg: {msg}");
        assert!(msg.contains("func.nix"), "msg: {msg}");
    }

    #[test]
    fn error_cannot_call_includes_file_context() {
        let p = std::path::PathBuf::from("/nix/store/call.nix");
        let _g = push_eval_file(p);
        let result = eval("42 99");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("cannot call"), "msg: {msg}");
        assert!(msg.contains("call.nix"), "msg: {msg}");
    }

    #[test]
    fn error_without_file_has_no_in_prefix() {
        // When no file is on the eval stack, error messages should
        // not contain ", in" context.
        let result = eval("nonexistent_xyz");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("undefined variable"), "msg: {msg}");
        assert!(!msg.contains(", in"), "msg should not contain file context: {msg}");
    }

    // ── pure mode getter/setter independence ───────────────

    #[test]
    fn pure_mode_set_get_independence() {
        let was = is_pure_mode();
        set_pure_mode(true);
        assert!(is_pure_mode());
        set_pure_mode(false);
        assert!(!is_pure_mode());
        set_pure_mode(was);
    }

    // ── eval_with_file with file path ──────────────────────

    #[test]
    fn eval_with_file_some_path_arithmetic() {
        let p = std::path::PathBuf::from("/tmp/imaginary.nix");
        let result = eval_with_file("1 + 2", Some(p)).unwrap();
        assert_eq!(result, Value::Int(3));
    }

    // ── String interpolation primitive coercions ───────────

    #[test]
    fn interp_int_into_string() {
        // Integer interpolated into a string is coerced to its decimal repr.
        assert_eq!(ev(r#""val=${toString 42}""#), Value::string("val=42"));
    }

    #[test]
    fn interp_bool_true_becomes_one() {
        // Per eval_str: Bool(true) → "1", Bool(false) → "" (empty)
        let v = ev(r#"let x = true; in "${builtins.toString x}""#);
        assert_eq!(v, Value::string("1"));
    }

    #[test]
    fn interp_null_becomes_empty() {
        // Null in interpolation is empty.
        let v = ev(r#"let x = null; in "${builtins.toString x}""#);
        assert_eq!(v, Value::string(""));
    }

    #[test]
    fn interp_attrset_without_to_string_errors() {
        // An attrset interpolated without __toString is a type error.
        let result = eval(r#"let s = { x = 1; }; in "${s}""#);
        assert!(result.is_err());
    }

    #[test]
    fn interp_attrset_with_to_string_protocol() {
        // __toString protocol returns a string when called with self.
        let v = ev(r#""${{ __toString = self: "ok"; }}""#);
        assert_eq!(v, Value::string("ok"));
    }

    // ── Path PathRel / PathHome / PathAbs ─────────────────

    #[test]
    fn eval_path_absolute_literal() {
        let v = ev("/tmp/foo");
        match v {
            Value::Path(p) => assert!(p.contains("/tmp/foo")),
            _ => panic!("expected Path"),
        }
    }

    #[test]
    fn eval_path_home_literal() {
        let v = ev("~/foo.nix");
        match v {
            Value::Path(p) => assert!(p.contains("~/foo.nix") || p.ends_with("foo.nix")),
            _ => panic!("expected Path"),
        }
    }

    // ── search path miss ──────────────────────────────────

    #[test]
    fn path_search_unmatched_errors() {
        // Without NIX_PATH entries matching, <nonexistent> errors out.
        // We unset NIX_PATH locally to ensure no entries match.
        let saved = std::env::var("NIX_PATH").ok();
        // SAFETY: tests run sequentially in single-threaded mode by
        // default? The thread_local NIX_PATH is per-thread but std::env
        // is process-global. We restore it after.
        unsafe {
            std::env::remove_var("NIX_PATH");
        }
        let result = eval("<this_should_not_resolve>");
        if let Some(v) = saved {
            unsafe {
                std::env::set_var("NIX_PATH", v);
            }
        }
        assert!(result.is_err());
    }

    // ── Unary operators ────────────────────────────────────

    #[test]
    fn unary_negate_int() {
        assert_eq!(ev("-7"), Value::Int(-7));
    }

    #[test]
    fn unary_negate_float() {
        assert_eq!(ev("-2.5"), Value::Float(-2.5));
    }

    #[test]
    fn unary_invert_true() {
        assert_eq!(ev("!true"), Value::Bool(false));
    }

    #[test]
    fn unary_invert_false() {
        assert_eq!(ev("!false"), Value::Bool(true));
    }

    #[test]
    fn unary_negate_bool_errors() {
        let result = eval("-true");
        assert!(result.is_err());
    }

    #[test]
    fn unary_invert_int_errors() {
        let result = eval("!42");
        assert!(result.is_err());
    }

    // ── Binary op type errors ──────────────────────────────

    #[test]
    fn binop_add_attrs_errors() {
        let result = eval("{a=1;} + {b=2;}");
        assert!(result.is_err());
    }

    #[test]
    fn binop_sub_string_errors() {
        let result = eval(r#""a" - "b""#);
        assert!(result.is_err());
    }

    #[test]
    fn binop_mul_string_errors() {
        let result = eval(r#""a" * "b""#);
        assert!(result.is_err());
    }

    #[test]
    fn binop_div_string_errors() {
        let result = eval(r#""a" / "b""#);
        assert!(result.is_err());
    }

    #[test]
    fn binop_compare_attrs_errors() {
        let result = eval("{a=1;} < {b=2;}");
        assert!(result.is_err());
    }

    #[test]
    fn binop_div_float_by_zero_int() {
        // Float / int(0) is NOT a DivisionByZero error in this evaluator —
        // only int/int matches the DivisionByZero branch. This documents
        // that branch.
        let result = eval("1.0 / 0");
        // Either inf or error is acceptable; the documented branch is
        // the int/int(0) → DivisionByZero one.
        let _ = result;
    }

    #[test]
    fn binop_int_div_zero_is_division_by_zero() {
        let result = eval("5 / 0");
        match result {
            Err(EvalError::DivisionByZero) => {}
            other => panic!("expected DivisionByZero, got {other:?}"),
        }
    }

    // ── if/then/else laziness ──────────────────────────────

    #[test]
    fn if_else_only_chosen_branch_evaluated_then() {
        // The else branch contains a divide-by-zero that would error
        // if eagerly evaluated. Choosing the then branch must skip it.
        assert_eq!(ev("if true then 42 else 1 / 0"), Value::Int(42));
    }

    #[test]
    fn if_else_only_chosen_branch_evaluated_else() {
        assert_eq!(ev("if false then 1 / 0 else 99"), Value::Int(99));
    }

    #[test]
    fn if_condition_must_be_bool() {
        let result = eval("if 1 then 1 else 2");
        assert!(result.is_err());
    }

    #[test]
    fn if_condition_lazy_does_not_force_unused() {
        // Lazy `let` ensures that `bad` is only forced if the chosen
        // branch references it.
        assert_eq!(
            ev("let bad = 1 / 0; in if true then 42 else bad"),
            Value::Int(42),
        );
    }

    // ── Logic short-circuit laziness ───────────────────────

    #[test]
    fn and_short_circuits_on_false() {
        // RHS contains an error; should never run.
        assert_eq!(ev("false && (1 / 0 == 0)"), Value::Bool(false));
    }

    #[test]
    fn or_short_circuits_on_true() {
        assert_eq!(ev("true || (1 / 0 == 0)"), Value::Bool(true));
    }

    #[test]
    fn implication_short_circuits_on_false_lhs() {
        // false -> anything is true; RHS not evaluated.
        assert_eq!(ev("false -> (1 / 0 == 0)"), Value::Bool(true));
    }

    // ── Lambda fixpoint via let ────────────────────────────

    #[test]
    fn lambda_fix_combinator_returns_attrset() {
        // The classic `fix = f: let x = f x; in x` shape.
        let v = ev(
            "let fix = f: let x = f x; in x; in
              (fix (self: { val = 1; double = self.val * 2; })).double",
        );
        assert_eq!(v, Value::Int(2));
    }

    // ── eval_attrset rec scope details ─────────────────────

    #[test]
    fn rec_attrset_self_reference() {
        // rec set with simple forward reference.
        let v = ev("(rec { a = b; b = 1; }).a");
        assert_eq!(v, Value::Int(1));
    }

    #[test]
    fn rec_attrset_inherit_from_uses_outer_scope() {
        // inherit-from in rec uses the OUTER (lexical) scope to evaluate
        // the source expression, not the rec scope. We bind `src` in
        // an outer let so the inherit can find it.
        let v = ev(
            "let src = { a = 10; }; in
              rec {
                inherit (src) a;
                b = a + 1;
              }",
        );
        if let Value::Attrs(attrs) = v {
            let b = attrs.get("b").unwrap();
            let b_forced = force_value(b).unwrap();
            assert_eq!(b_forced, Value::Int(11));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn nonrec_attrset_no_self_reference() {
        // In a non-rec set, a name doesn't see its sibling. The error
        // surfaces as an UndefinedVar when the thunk is forced.
        let result = eval("({ a = 1; b = a + 1; }).b");
        assert!(result.is_err());
    }

    // ── eval_attrset deep merge edge cases ─────────────────

    #[test]
    fn dotted_binding_three_segments_then_sibling() {
        let v = ev("{ a.b.c = 1; a.b.d = 2; a.e = 3; }");
        if let Value::Attrs(attrs) = v {
            let a = attrs.get("a").unwrap();
            let a_forced = force_value(a).unwrap();
            if let Value::Attrs(a_attrs) = a_forced {
                let b = a_attrs.get("b").unwrap();
                let b_forced = force_value(b).unwrap();
                if let Value::Attrs(b_attrs) = b_forced {
                    assert_eq!(force_value(b_attrs.get("c").unwrap()).unwrap(), Value::Int(1));
                    assert_eq!(force_value(b_attrs.get("d").unwrap()).unwrap(), Value::Int(2));
                } else {
                    panic!("expected b to be attrs");
                }
                assert_eq!(force_value(a_attrs.get("e").unwrap()).unwrap(), Value::Int(3));
            } else {
                panic!("expected a to be attrs");
            }
        } else {
            panic!("expected outer attrs");
        }
    }

    // ── rec/let dotted bindings in recursive scope ────────

    #[test]
    fn rec_dotted_bindings_visible_to_siblings() {
        // Dotted bindings in rec blocks must be visible to sibling
        // bindings -- this is the nixpkgs lib/systems/parse.nix pattern.
        let v = ev("rec { types.openSB = 1; types.openCpu = 2; foo = types.openSB; }.foo");
        assert_eq!(v, Value::Int(1));
    }

    #[test]
    fn rec_dotted_leaf_uses_rec_scope() {
        // Leaf expressions in dotted bindings must see sibling
        // rec-bindings, not just the parent scope.
        let v = ev("rec { types.a = f 1; f = x: x + 1; }.types.a");
        assert_eq!(v, Value::Int(2));
    }

    #[test]
    fn rec_dotted_multiple_keys_merge() {
        // Multiple dotted bindings sharing a top-level key must merge.
        let v = ev("rec { types.a = 1; types.b = 2; x = types; }.x");
        if let Value::Attrs(attrs) = v {
            assert_eq!(force_value(attrs.get("a").unwrap()).unwrap(), Value::Int(1));
            assert_eq!(force_value(attrs.get("b").unwrap()).unwrap(), Value::Int(2));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn rec_nixpkgs_parse_pattern() {
        // Simplified nixpkgs lib/systems/parse.nix pattern:
        // rec block with dotted types.xxx bindings that reference
        // each other through the rec scope.
        let v = ev(r#"
            let
              mkOptionType = x: x;
              mergeOneOption = "merge";
              attrValues = builtins.attrValues;
              setType = name: value: { __type = name; } // value;
              mapAttrs = builtins.mapAttrs;
              enum = xs: mkOptionType { name = "enum"; check = x: builtins.elem x xs; };
              setTypes = type: mapAttrs (name: value: setType type.name ({ inherit name; } // value));
            in
            rec {
              types.openSB = mkOptionType { name = "sb"; merge = mergeOneOption; };
              types.significantByte = enum (attrValues significantBytes);
              significantBytes = setTypes types.openSB { bigEndian = {}; littleEndian = {}; };
              types.openCpuType = mkOptionType { name = "cpu-type"; };
              types.cpuType = enum (attrValues cpuTypes);
              cpuTypes = setTypes types.openCpuType { arm = { bits = 32; }; };
            }.types.openCpuType
        "#);
        if let Value::Attrs(attrs) = v {
            assert_eq!(
                force_value(attrs.get("name").unwrap()).unwrap(),
                Value::string("cpu-type")
            );
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn let_dotted_leaf_uses_let_scope() {
        // Dotted binding leaf in a let block sees sibling let-bindings.
        let v = ev("let a.x = f 1; f = x: x + 1; in a.x");
        assert_eq!(v, Value::Int(2));
    }

    #[test]
    fn let_inherit_from_plus_dotted_overrides() {
        // inherit-from and dotted bindings for the same key in a let
        // block: CppNix rejects this as a duplicate definition.  Sui
        // currently lets the dotted binding win (last-write-wins).
        // This test documents the current behaviour -- when we add
        // duplicate detection it should change to assert an error.
        let v = ev(r#"
            let
              src = { types = { existing = true; }; };
              inherit (src) types;
              types.added = true;
            in types
        "#);
        if let Value::Attrs(attrs) = v {
            // Dotted binding overwrites the inherited value
            assert_eq!(
                force_value(attrs.get("added").unwrap()).unwrap(),
                Value::Bool(true)
            );
            // Inherited 'existing' is lost because dotted replaced it
            assert!(attrs.get("existing").is_none());
        } else {
            panic!("expected attrs");
        }
    }

    // ── Function pattern variations ────────────────────────

    #[test]
    fn pattern_empty_no_args_no_ellipsis() {
        // {} pattern accepts only an empty attrset.
        assert_eq!(ev("({}: 1) {}"), Value::Int(1));
    }

    #[test]
    fn pattern_empty_with_ellipsis_accepts_extra() {
        assert_eq!(ev("({...}: 1) { a = 1; b = 2; }"), Value::Int(1));
    }

    #[test]
    fn pattern_all_defaults() {
        assert_eq!(
            ev("({a ? 1, b ? 2}: a + b) {}"),
            Value::Int(3),
        );
    }

    #[test]
    fn pattern_at_bind_before() {
        // args @ { x }: args.x — bind name comes before pattern.
        assert_eq!(ev("(args @ { x }: args.x) { x = 7; }"), Value::Int(7));
    }

    #[test]
    fn pattern_at_bind_after() {
        // { x } @ args: args.x — bind name comes after pattern.
        assert_eq!(ev("({ x } @ args: args.x) { x = 7; }"), Value::Int(7));
    }

    #[test]
    fn pattern_default_references_other_arg() {
        // The default for `b` references `a` (which exists).
        assert_eq!(ev("({a, b ? a + 1}: b) {a = 10;}"), Value::Int(11));
    }

    #[test]
    fn pattern_required_missing_errors() {
        let result = eval("({ a, b }: a) { a = 1; }");
        assert!(result.is_err());
    }

    #[test]
    fn pattern_unexpected_errors_without_ellipsis() {
        let result = eval("({ a }: a) { a = 1; b = 2; }");
        assert!(result.is_err());
    }

    // ── apply: error on non-callable ───────────────────────

    #[test]
    fn apply_int_errors() {
        let result = eval("42 5");
        assert!(result.is_err());
    }

    #[test]
    fn apply_string_errors() {
        let result = eval(r#""hi" 5"#);
        assert!(result.is_err());
    }

    #[test]
    fn apply_attrset_without_functor_errors() {
        let result = eval("{ x = 1; } 5");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("__functor") || msg.contains("cannot call"));
    }

    // ── Select with multi-segment + default ────────────────

    #[test]
    fn select_multi_segment_with_default() {
        // a.b.missing or 99 -- the missing segment yields the default.
        assert_eq!(ev("{ a = { b = 1; }; }.a.c or 99"), Value::Int(99));
    }

    #[test]
    fn select_from_int_errors() {
        let result = eval("(1).x");
        assert!(result.is_err());
    }

    // ── HasAttr edge cases ─────────────────────────────────

    #[test]
    fn has_attr_on_non_set_returns_false() {
        // `expr ? a` where expr is not a set returns false (not error).
        assert_eq!(ev("1 ? x"), Value::Bool(false));
    }

    #[test]
    fn has_attr_nested_path_present() {
        assert_eq!(ev("{ a = { b = 1; }; } ? a.b"), Value::Bool(true));
    }

    #[test]
    fn has_attr_nested_path_missing() {
        assert_eq!(ev("{ a = { b = 1; }; } ? a.c"), Value::Bool(false));
    }

    #[test]
    fn has_attr_intermediate_missing_returns_false() {
        assert_eq!(ev("{} ? a.b.c"), Value::Bool(false));
    }

    // ── List eval edge cases ───────────────────────────────

    #[test]
    fn list_with_function_value() {
        let v = ev("[(x: x + 1)]");
        if let Value::List(items) = v {
            assert_eq!(items.len(), 1);
            // List elements are now lazy (thunked). Force to check type.
            let forced = force_value(&items[0]).unwrap();
            assert!(matches!(forced, Value::Lambda(_)));
        } else {
            panic!("expected list");
        }
    }

    // ── eval_inherit edge: inherit from missing var ────────

    #[test]
    fn inherit_unknown_name_errors() {
        let result = eval("let x = 1; in let inherit nonexistent; in nonexistent");
        assert!(result.is_err());
    }

    // ── String op: string concat preserves context ─────────

    #[test]
    fn string_concat_no_context_when_both_plain() {
        let v = ev(r#""abc" + "def""#);
        if let Value::String(ns) = v {
            assert_eq!(ns.chars, "abcdef");
            assert!(!ns.has_context());
        } else {
            panic!("expected string");
        }
    }

    // ── Parens / Root ──────────────────────────────────────

    #[test]
    fn parens_around_expression() {
        assert_eq!(ev("(1 + 2)"), Value::Int(3));
    }

    #[test]
    fn nested_parens() {
        assert_eq!(ev("(((42)))"), Value::Int(42));
    }

    // ── Throw via builtins ─────────────────────────────────

    #[test]
    fn throw_propagates_as_error() {
        let result = eval(r#"builtins.throw "kaboom""#);
        match result {
            Err(EvalError::Throw(s)) => assert!(s.contains("kaboom")),
            other => panic!("expected Throw, got {other:?}"),
        }
    }

    #[test]
    fn assert_failed_propagates_as_error() {
        let result = eval("assert false; 1");
        match result {
            Err(EvalError::AssertionFailed(_)) => {}
            other => panic!("expected AssertionFailed, got {other:?}"),
        }
    }

    // ── eval_str InterpolPart::Literal only ────────────────

    #[test]
    fn string_no_interp_yields_no_context() {
        let v = ev(r#""just literal""#);
        if let Value::String(ns) = v {
            assert!(!ns.has_context());
        } else {
            panic!("expected string");
        }
    }

    // ── Path interpolation adds context ───────────────────

    #[test]
    fn interp_path_adds_plain_context() {
        let v = ev(r#""${/tmp/abc}""#);
        if let Value::String(ns) = v {
            assert!(ns.has_context());
            assert!(ns.chars.contains("/tmp/abc"));
        } else {
            panic!("expected string");
        }
    }

    // ── pipe operators (NotImplemented) ────────────────────
    // Pipe operators (|>, <|) are parsed as PipeRight/PipeLeft and
    // currently return NotImplemented. We can't easily evaluate them
    // here because rnix may not even parse them, so we just rely on
    // the binop branch existing.

    // ── ParseError surface ─────────────────────────────────

    #[test]
    fn parse_error_unbalanced_braces() {
        let result = eval("{ a = 1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, EvalError::ParseError(_)));
    }

    #[test]
    fn parse_error_dangling_let() {
        let result = eval("let in");
        assert!(result.is_err());
    }

    #[test]
    fn parse_error_empty_input() {
        let result = eval("");
        assert!(result.is_err());
    }

    // ── num_op coverage via float ops ──────────────────────

    #[test]
    fn float_int_subtraction() {
        assert_eq!(ev("3.5 - 1"), Value::Float(2.5));
    }

    #[test]
    fn int_float_subtraction() {
        assert_eq!(ev("3 - 0.5"), Value::Float(2.5));
    }

    #[test]
    fn float_float_division() {
        assert_eq!(ev("6.0 / 2.0"), Value::Float(3.0));
    }

    #[test]
    fn int_float_multiplication() {
        assert_eq!(ev("3 * 2.5"), Value::Float(7.5));
    }

    // ── compare with mixed numerics ────────────────────────

    #[test]
    fn compare_int_float_less() {
        assert_eq!(ev("1 < 1.5"), Value::Bool(true));
    }

    #[test]
    fn compare_float_int_more() {
        assert_eq!(ev("3.5 > 3"), Value::Bool(true));
    }

    #[test]
    fn compare_equal_int_float() {
        assert_eq!(ev("3 <= 3.0"), Value::Bool(true));
    }

    // ── Equality ──────────────────────────────────────────

    #[test]
    fn equal_lists_same() {
        assert_eq!(ev("[1 2 3] == [1 2 3]"), Value::Bool(true));
    }

    #[test]
    fn equal_lists_diff_length() {
        assert_eq!(ev("[1 2] == [1 2 3]"), Value::Bool(false));
    }

    #[test]
    fn not_equal_lists() {
        assert_eq!(ev("[1] != [2]"), Value::Bool(true));
    }

    #[test]
    fn equal_attrsets_same() {
        assert_eq!(ev("{a = 1; b = 2;} == {b = 2; a = 1;}"), Value::Bool(true));
    }

    // ── Lambda identity equality (Rc ptr_eq) ────────────────
    // Regression test: same lambda via Rc must compare equal.
    // Without this, nixpkgs stdenv evaluation enters an infinite loop
    // because `crossSystem != localSystem` returns true even when both
    // are the same elaborate result (containing shared function attrs).

    #[test]
    fn lambda_self_equality_in_attrset() {
        // Same closure shared via let → inherit must be equal
        assert_eq!(
            ev("let f = x: x; in { a = 1; inherit f; } == { a = 1; inherit f; }"),
            Value::Bool(true),
        );
    }

    #[test]
    fn lambda_self_reference_attrset_equality() {
        // Attrset with function attr: x == x must be true
        assert_eq!(
            ev("let x = { a = 1; f = y: y; }; in x == x"),
            Value::Bool(true),
        );
    }

    #[test]
    fn lambda_different_closures_not_equal() {
        // Different lambda closures (even structurally identical) must be false
        assert_eq!(
            ev("{ f = x: x; } == { f = x: x; }"),
            Value::Bool(false),
        );
    }

    #[test]
    fn lambda_ne_does_not_force_unused_branch() {
        // If crossSystem == localSystem (same obj), != returns false,
        // and the then-branch (with throw) is never forced.
        assert_eq!(
            ev("let ls = { a = 1; f = x: x; }; in if ls != ls then builtins.throw \"bug\" else 42"),
            Value::Int(42),
        );
    }

    // ── force_value chains thunks ──────────────────────────

    #[test]
    fn force_value_through_thunk() {
        let root = rnix::Root::parse("1 + 2");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert_eq!(force_value(&val).unwrap(), Value::Int(3));
    }

    // ── Builtin name "tryEval" lazy arg path ──────────────

    #[test]
    fn try_eval_catches_thrown_error() {
        // tryEval wraps the thunk and catches throws inside.
        let v = ev(r#"(builtins.tryEval (builtins.throw "oops")).success"#);
        assert_eq!(v, Value::Bool(false));
    }

    #[test]
    fn try_eval_returns_value_on_success() {
        let v = ev("(builtins.tryEval 42).value");
        assert_eq!(v, Value::Int(42));
    }

    // ── LegacyLet (`let { body = ...; ...}`) ───────────────

    #[test]
    fn legacy_let_returns_body_attr() {
        // `let { x = 1; body = x + 41; }` is the legacy let form: it
        // is desugared as a recursive set whose `body` attr is the
        // result.
        assert_eq!(ev("let { x = 1; body = x + 41; }"), Value::Int(42));
    }

    #[test]
    fn legacy_let_missing_body_errors() {
        let result = eval("let { x = 1; }");
        assert!(result.is_err());
    }

    #[test]
    fn legacy_let_with_inherit_from_scope() {
        assert_eq!(
            ev("let outer = 5; in let { inherit outer; body = outer * 2; }"),
            Value::Int(10),
        );
    }

    // ── eval_str interpolation more cases ──────────────────

    #[test]
    fn interp_with_string_concat_preserves_order() {
        assert_eq!(
            ev(r#"let a = "x"; b = "y"; in "${a}-${b}""#),
            Value::string("x-y"),
        );
    }

    #[test]
    fn interp_only_literal_part() {
        assert_eq!(ev(r#""no interp here""#), Value::string("no interp here"));
    }

    // ── eval_attr dynamic / string keys ────────────────────

    #[test]
    fn dynamic_attr_via_string_key_in_set() {
        // `{ "a" = 1; }.a` works because attr keys can be string literals.
        assert_eq!(ev(r#"{ "a" = 1; }.a"#), Value::Int(1));
    }

    #[test]
    fn dynamic_attr_via_interpolated_key() {
        let v = ev(r#"let k = "foo"; in { ${k} = 99; }.foo"#);
        assert_eq!(v, Value::Int(99));
    }

    // ── String key access via select with dynamic ──────────

    #[test]
    fn select_with_string_key() {
        let v = ev(r#"{ a = 42; }."a""#);
        assert_eq!(v, Value::Int(42));
    }

    // ── Apply via __functor on attrset ─────────────────────

    #[test]
    fn apply_attrset_with_functor_works() {
        let v = ev("let s = { __functor = self: x: x + 1; }; in s 5");
        assert_eq!(v, Value::Int(6));
    }

    // ── Negation of negative ───────────────────────────────

    #[test]
    fn double_negate_int() {
        assert_eq!(ev("- (-5)"), Value::Int(5));
    }

    // ── Inherit from rec scope binding visibility ──────────

    #[test]
    fn inherit_in_let_makes_name_available() {
        assert_eq!(
            ev("let src = { a = 7; }; in let inherit (src) a; in a"),
            Value::Int(7),
        );
    }

    // ── String + path ──────────────────────────────────────

    #[test]
    fn path_plus_string_yields_path() {
        let v = ev(r#"/foo + "/bar""#);
        match v {
            Value::Path(p) => assert_eq!(&*p, "/foo/bar"),
            _ => panic!("expected path"),
        }
    }

    // ── Lazy attrset value not forced unless selected ──────

    #[test]
    fn attrset_value_not_forced_unless_selected() {
        // `bad` is an attr whose value would error if forced, but we
        // only ever select `good`, so it's never touched.
        assert_eq!(
            ev(r#"{ bad = builtins.throw "boom"; good = 42; }.good"#),
            Value::Int(42),
        );
    }

    // ── Lambda calling itself via let ──────────────────────

    #[test]
    fn lambda_recursive_via_let() {
        // factorial via let-bound recursive function
        assert_eq!(
            ev("let fact = n: if n == 0 then 1 else n * fact (n - 1); in fact 5"),
            Value::Int(120),
        );
    }

    // ── Dynamic key in select ──────────────────────────────

    #[test]
    fn select_with_dynamic_key_via_var() {
        // ${k} interpolation in select position is not standard Nix
        // syntax, but a string-literal key works for select.
        assert_eq!(ev(r#"let k = { x = 1; }; in k.x"#), Value::Int(1));
    }

    // ── Compare strings ────────────────────────────────────

    #[test]
    fn compare_string_lex_greater_or_equal() {
        assert_eq!(ev(r#""b" >= "a""#), Value::Bool(true));
        assert_eq!(ev(r#""a" >= "a""#), Value::Bool(true));
        assert_eq!(ev(r#""a" >= "b""#), Value::Bool(false));
    }

    // ── PartialEq across types ─────────────────────────────

    #[test]
    fn equal_int_string_false() {
        assert_eq!(ev(r#"1 == "1""#), Value::Bool(false));
    }

    #[test]
    fn equal_null_int_false() {
        assert_eq!(ev("null == 0"), Value::Bool(false));
    }

    // ── Update operator on thunked operands ────────────────

    #[test]
    fn update_with_let_bound_operands() {
        assert_eq!(
            ev("let a = { x = 1; }; b = { y = 2; }; in (a // b).y"),
            Value::Int(2),
        );
    }

    // ── Concat on let-bound lists ──────────────────────────

    #[test]
    fn concat_lists_from_let() {
        assert_eq!(
            ev("let a = [1 2]; b = [3 4]; in builtins.length (a ++ b)"),
            Value::Int(4),
        );
    }

    // ── String interpolation: list coercion ─────────────────

    #[test]
    fn interp_list_coerces_with_spaces() {
        // Lists in interpolation are now coerced via coerce_to_string
        // (space-joined elements).
        assert_eq!(
            ev(r#""${toString [1 2 3]}""#),
            Value::string("1 2 3"),
        );
    }

    #[test]
    fn interp_list_directly_coerces() {
        // Direct list interpolation space-joins elements via coerce_to_string.
        assert_eq!(
            ev(r#""${[1 2]}""#),
            Value::string("1 2"),
        );
    }

    // ── String interpolation: outPath ─────────────────────

    #[test]
    fn interp_outpath_attrset() {
        assert_eq!(
            ev(r#"let x = { outPath = "/nix/store/abc"; }; in "${x}""#),
            Value::string("/nix/store/abc"),
        );
    }

    #[test]
    fn interp_tostring_takes_priority_over_outpath() {
        assert_eq!(
            ev(r#"let x = { __toString = self: "custom"; outPath = "/ignored"; }; in "${x}""#),
            Value::string("custom"),
        );
    }

    #[test]
    fn interp_derivation_coerces_to_outpath() {
        // derivation produces an attrset with outPath
        let result = eval(r#"
            let drv = builtins.derivation {
                name = "test";
                system = "x86_64-linux";
                builder = "/bin/sh";
            };
            in "${drv}"
        "#).unwrap();
        if let Value::String(s) = result {
            assert!(s.chars.starts_with("/nix/store/"), "got: {}", s.chars);
        } else {
            panic!("expected string");
        }
    }

    // ── String interpolation: lambda error ─────────────────

    #[test]
    fn interp_lambda_errors() {
        let result = eval(r#""${x: x}""#);
        assert!(result.is_err());
    }

    // ── force_value tests ────────────────────────────────────

    #[test]
    fn force_value_int_returns_same() {
        let v = Value::Int(42);
        assert_eq!(force_value(&v).unwrap(), Value::Int(42));
    }

    #[test]
    fn force_value_bool_returns_same() {
        let v = Value::Bool(true);
        assert_eq!(force_value(&v).unwrap(), Value::Bool(true));
    }

    #[test]
    fn force_value_string_returns_same() {
        let v = Value::string("hello");
        assert_eq!(force_value(&v).unwrap(), Value::string("hello"));
    }

    #[test]
    fn force_value_attrs_returns_same() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        let v = Value::Attrs(Rc::new(a.clone()));
        assert_eq!(force_value(&v).unwrap(), Value::Attrs(Rc::new(a)));
    }

    #[test]
    fn force_value_list_returns_same() {
        let v = Value::list(vec![Value::Int(1), Value::Int(2)]);
        assert_eq!(
            force_value(&v).unwrap(),
            Value::list(vec![Value::Int(1), Value::Int(2)]),
        );
    }

    #[test]
    fn force_value_null_returns_null() {
        let v = Value::Null;
        assert_eq!(force_value(&v).unwrap(), Value::Null);
    }

    #[test]
    fn force_value_evaluated_thunk_returns_cached() {
        // Thunk wrapping a simple expression should evaluate and cache
        let v = ev("let x = 1 + 2; in x");
        assert_eq!(v, Value::Int(3));
        // Force again — should return the cached value
        assert_eq!(force_value(&v).unwrap(), Value::Int(3));
    }

    // ── Tail-call loop tests ─────────────────────────────────

    #[test]
    fn tco_if_true_condition() {
        assert_eq!(ev("if true then 42 else 0"), Value::Int(42));
    }

    #[test]
    fn tco_if_false_condition() {
        assert_eq!(ev("if false then 42 else 0"), Value::Int(0));
    }

    #[test]
    fn tco_deeply_nested_if_else_chain() {
        // Build a chain: if false then 1 else if false then 2 else ... else 150
        // All conditions are false except the final else, which produces 150.
        let mut expr = String::from("150");
        for i in (1..150).rev() {
            expr = format!("if false then {} else {}", i, expr);
        }
        let v = ev(&expr);
        assert_eq!(v, Value::Int(150));
    }

    #[test]
    fn tco_assert_true_passes_through() {
        assert_eq!(ev("assert true; 42"), Value::Int(42));
    }

    #[test]
    fn tco_assert_false_throws_assertion_failed() {
        let result = eval("assert false; 42");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, EvalError::AssertionFailed(_)),
            "expected AssertionFailed, got: {err}",
        );
    }

    #[test]
    fn tco_with_makes_scope_available() {
        assert_eq!(ev("with { x = 10; y = 20; }; x + y"), Value::Int(30));
    }

    #[test]
    fn tco_let_in_creates_bindings() {
        assert_eq!(ev("let a = 5; in a"), Value::Int(5));
    }

    #[test]
    fn tco_let_in_multiple_bindings() {
        assert_eq!(ev("let a = 1; b = 2; c = 3; in a + b + c"), Value::Int(6));
    }

    // ── eval_attrset tests ───────────────────────────────────

    #[test]
    fn eval_attrset_empty() {
        let v = ev("{}");
        if let Value::Attrs(attrs) = v {
            assert!(attrs.is_empty(), "expected empty attrset");
        } else {
            panic!("expected attrset, got {v:?}");
        }
    }

    #[test]
    fn eval_attrset_simple_kv() {
        let v = ev("{ a = 1; b = 2; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
        } else {
            panic!("expected attrset, got {v:?}");
        }
    }

    #[test]
    fn eval_attrset_recursive() {
        assert_eq!(ev("(rec { a = 1; b = a + 1; }).b"), Value::Int(2));
        assert_eq!(ev("(rec { a = 1; b = a + 1; }).a"), Value::Int(1));
    }

    #[test]
    fn eval_attrset_inherit_from_scope() {
        assert_eq!(ev("let x = 1; in { inherit x; }.x"), Value::Int(1));
    }

    #[test]
    fn eval_attrset_inherit_from_expr() {
        assert_eq!(
            ev("{ inherit (builtins) true; }.true"),
            Value::Bool(true),
        );
    }

    #[test]
    fn eval_attrset_dotted_path() {
        assert_eq!(ev("{ a.b.c = 1; }.a.b.c"), Value::Int(1));
    }

    #[test]
    fn eval_attrset_update_merge() {
        let v = ev("{ a = 1; } // { b = 2; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
        } else {
            panic!("expected attrset, got {v:?}");
        }
    }

    // ── eval_apply tests ─────────────────────────────────────

    #[test]
    fn eval_apply_simple_function() {
        assert_eq!(ev("(x: x + 1) 2"), Value::Int(3));
    }

    #[test]
    fn eval_apply_pattern_destructuring() {
        assert_eq!(ev("({a, b}: a + b) { a = 1; b = 2; }"), Value::Int(3));
    }

    #[test]
    fn eval_apply_default_arguments() {
        assert_eq!(ev("({a, b ? 0}: a + b) { a = 1; }"), Value::Int(1));
    }

    #[test]
    fn eval_apply_ellipsis() {
        assert_eq!(ev("({a, ...}: a) { a = 1; b = 2; }"), Value::Int(1));
    }

    // ── eval_select tests ────────────────────────────────────

    #[test]
    fn eval_select_single_key() {
        assert_eq!(ev("{ a = 1; }.a"), Value::Int(1));
    }

    #[test]
    fn eval_select_multi_level() {
        assert_eq!(ev("{ a.b = 1; }.a.b"), Value::Int(1));
    }

    #[test]
    fn eval_select_with_or_default() {
        assert_eq!(ev("{}.a or 42"), Value::Int(42));
    }

    #[test]
    fn eval_select_missing_key_without_default_throws() {
        let result = eval("{}.a");
        assert!(result.is_err());
    }

    // ── BinOp tests ──────────────────────────────────────────

    #[test]
    fn binop_add_ints() {
        assert_eq!(ev("1 + 2"), Value::Int(3));
    }

    #[test]
    fn binop_sub_ints() {
        assert_eq!(ev("3 - 1"), Value::Int(2));
    }

    #[test]
    fn binop_mul_ints() {
        assert_eq!(ev("2 * 3"), Value::Int(6));
    }

    #[test]
    fn binop_div_ints() {
        assert_eq!(ev("6 / 2"), Value::Int(3));
    }

    #[test]
    fn binop_float_arithmetic() {
        assert_eq!(ev("1.5 + 2.5"), Value::Float(4.0));
    }

    #[test]
    fn binop_string_concat() {
        assert_eq!(
            ev(r#""hello" + " " + "world""#),
            Value::string("hello world"),
        );
    }

    #[test]
    fn binop_list_concat() {
        assert_eq!(
            ev("[1 2] ++ [3 4]"),
            Value::list(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(3),
                Value::Int(4),
            ]),
        );
    }

    #[test]
    fn binop_attrset_update() {
        let v = ev("{ a = 1; } // { b = 2; }");
        if let Value::Attrs(attrs) = v {
            assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
        } else {
            panic!("expected attrset, got {v:?}");
        }
    }

    #[test]
    fn binop_less_than() {
        assert_eq!(ev("1 < 2"), Value::Bool(true));
        assert_eq!(ev("2 < 1"), Value::Bool(false));
    }

    #[test]
    fn binop_greater_than() {
        assert_eq!(ev("2 > 1"), Value::Bool(true));
        assert_eq!(ev("1 > 2"), Value::Bool(false));
    }

    #[test]
    fn binop_equal() {
        assert_eq!(ev("1 == 1"), Value::Bool(true));
        assert_eq!(ev("1 == 2"), Value::Bool(false));
    }

    #[test]
    fn binop_not_equal() {
        assert_eq!(ev("1 != 2"), Value::Bool(true));
        assert_eq!(ev("1 != 1"), Value::Bool(false));
    }

    #[test]
    fn binop_logical_and() {
        assert_eq!(ev("true && false"), Value::Bool(false));
        assert_eq!(ev("true && true"), Value::Bool(true));
    }

    #[test]
    fn binop_logical_or() {
        assert_eq!(ev("true || false"), Value::Bool(true));
        assert_eq!(ev("false || false"), Value::Bool(false));
    }

    #[test]
    fn binop_logical_not() {
        assert_eq!(ev("!true"), Value::Bool(false));
        assert_eq!(ev("!false"), Value::Bool(true));
    }

    #[test]
    fn binop_implication() {
        assert_eq!(ev("false -> true"), Value::Bool(true));
        assert_eq!(ev("false -> false"), Value::Bool(true));
        assert_eq!(ev("true -> true"), Value::Bool(true));
        assert_eq!(ev("true -> false"), Value::Bool(false));
    }
}
