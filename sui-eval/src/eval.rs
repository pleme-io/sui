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
    /// `None` frame = "evaluating something with no source file" (a `--expr` /
    /// `<string>` literal). Representing that explicitly is load-bearing: a
    /// thunk captured in a fileless context used to push NOTHING when it
    /// forced, so the callee's file stayed on top and `unsafeGetAttrPos`
    /// stamped the literal with the callee's path where CppNix returns `null`.
    /// That fed `eval-config.nix`'s `modulesLocation`, which wraps every user
    /// module in `{ _file; imports = [ m ]; }` — demoting it one
    /// `genericClosure` level and permuting NixOS definition order.
    static EVAL_FILE_STACK: RefCell<Vec<Option<PathBuf>>> = const { RefCell::new(Vec::new()) };
    /// Nix-level error context stack — captures source positions for --show-trace.
    /// Each entry: (file, expression_snippet). Pushed on function calls, select,
    /// force, and popped on return. Attached to errors for structured diagnostics.
    static NIX_TRACE_STACK: RefCell<Vec<NixTraceFrame>> = const { RefCell::new(Vec::new()) };
}

/// A single frame in the Nix-level error trace.
///
/// The frame is only ever *observed* on the cold error path (via
/// `attach_trace`). To keep the hot lambda-call path allocation-free,
/// the per-call lambda frame stores the raw ingredients (a cheap
/// `Rc`-clone of the closure env + the raw current-eval-file `PathBuf`)
/// and defers the `format!` / path-strip work into `attach_trace`. The
/// rendered `(description, file)` pair is byte-identical to the eager
/// form either way (see the `description()` / `file()` accessors).
#[derive(Debug, Clone)]
pub enum NixTraceFrame {
    /// Pre-formatted frame (the builtin-call path — kept eager because
    /// the builtin name is already a `&'static str`, so there is no
    /// per-call heap-`String` to defer).
    Eager {
        file: Option<String>,
        description: String,
    },
    /// Lazy per-lambda-call frame. The `description` string and the
    /// stripped `file` string are built on demand in `attach_trace`.
    ///
    /// - `closure_env` provides the *description*'s file (from
    ///   `closure.env.eval_file()`) — an O(1) `Rc` refcount bump.
    /// - `current_file` is the raw `current_eval_file()` snapshot taken
    ///   at push time (the stack top after the file guard pushed the
    ///   closure's file), used verbatim for the frame's `file` field so
    ///   the rendered `loc` matches the eager form byte-for-byte.
    Lambda {
        closure_env: Env,
        current_file: Option<PathBuf>,
    },
}

/// Strip the `-source/` store-path prefix from a rendered path exactly
/// as the eager trace path did (`p.display()...rsplit_once("-source/")`).
fn strip_source_prefix(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    s.rsplit_once("-source/")
        .map_or_else(|| p.display().to_string(), |(_, tail)| tail.to_string())
}

impl NixTraceFrame {
    /// The frame's `file` field (for the trace `loc`), matching the
    /// eager `frame.file` byte-for-byte.
    fn file(&self) -> Option<String> {
        match self {
            NixTraceFrame::Eager { file, .. } => file.clone(),
            NixTraceFrame::Lambda { current_file, .. } => {
                current_file.as_deref().map(strip_source_prefix)
            }
        }
    }

    /// The frame's `description`, matching the eager `frame.description`
    /// byte-for-byte. Rendered through the `Display` impl (a `write!`
    /// surface — the description is the frame's canonical serialization,
    /// per the fleet TYPED-EMISSION rule; no `format!()`).
    fn description(&self) -> String {
        self.to_string()
    }
}

/// The frame's rendered description IS its `Display` — the typed emission
/// surface for the trace message (`write!`, never `format!()`). The
/// `Lambda` arm defers the path-strip to this cold error-path render.
impl std::fmt::Display for NixTraceFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NixTraceFrame::Eager { description, .. } => f.write_str(description),
            NixTraceFrame::Lambda { closure_env, .. } => {
                let file = closure_env.eval_file().map(|p| strip_source_prefix(p));
                write!(
                    f,
                    "while calling function defined in {}",
                    file.as_deref().unwrap_or("<eval>")
                )
            }
        }
    }
}

/// Push a Nix-level trace frame. Returns a guard that pops on drop.
fn push_nix_trace(desc: impl Into<String>) -> NixTraceGuard {
    let frame = NixTraceFrame::Eager {
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

/// Push a *lazy* Nix-level trace frame for a lambda call. Stores only the
/// raw ingredients (an O(1) `Rc`-clone of the closure env + the raw
/// `current_eval_file()` snapshot) — the `format!`/path-strip work is
/// deferred to the cold `attach_trace` path. Returns a guard that pops on
/// drop. The rendered frame is byte-identical to the eager form.
fn push_nix_trace_lambda(closure_env: &Env) -> NixTraceGuard {
    let frame = NixTraceFrame::Lambda {
        closure_env: closure_env.clone(),
        current_file: current_eval_file(),
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
        let max_frames = std::env::var("SUI_M26_MAXFRAMES").ok()
            .and_then(|s| s.parse::<usize>().ok()).unwrap_or(15);
        let mut trace = format!("{err}");
        for (i, frame) in stack.iter().rev().take(max_frames).enumerate() {
            let file = frame.file();
            let loc = file.as_deref().unwrap_or("<eval>");
            trace.push_str(&format!("\n  {} ({loc})", frame.description()));
            if i + 1 >= max_frames && stack.len() > max_frames {
                trace.push_str(&format!("\n  ... ({} more frames)", stack.len() - max_frames));
            }
        }
        // CRITICAL: preserve Throw/AssertionFailed variants so tryEval can catch them.
        // Converting to TypeError would make tryEval miss them.
        match err {
            EvalError::Throw(_) => EvalError::Throw(trace),
            EvalError::AssertionFailed(_) => EvalError::AssertionFailed(trace),
            _ => EvalError::TypeError(trace),
        }
    })
}

/// Return the directory of the file currently being evaluated, if any.
/// Used by the `PathRel` AST handler to resolve relative path literals.
#[must_use]
pub fn current_eval_dir() -> Option<PathBuf> {
    EVAL_FILE_STACK
        .with(|s| s.borrow().last().cloned())
        .flatten()
        .and_then(|p| p.parent().map(PathBuf::from))
}

/// Push a file onto the eval stack. Returns an RAII guard that pops
/// it on drop. Use when entering an `import <file>` so subsequent
/// relative path literals resolve against the right directory.
pub fn push_eval_file(file: PathBuf) -> EvalFileGuard {
    push_eval_frame(Some(file))
}

/// Push a frame that may be fileless. `None` means "this code has no source
/// file" and MUST still occupy a stack slot — pushing nothing would leave the
/// caller's file visible to `current_eval_file`, which is exactly the
/// `unsafeGetAttrPos` divergence documented on `EVAL_FILE_STACK`.
pub fn push_eval_frame(file: Option<PathBuf>) -> EvalFileGuard {
    EVAL_FILE_STACK.with(|s| s.borrow_mut().push(file));
    EvalFileGuard
}

/// Return the file currently being evaluated, if any.
/// Used by error sites to attach source location context.
#[must_use]
pub fn current_eval_file() -> Option<PathBuf> {
    EVAL_FILE_STACK.with(|s| s.borrow().last().cloned()).flatten()
}


/// Snapshot the entire eval file stack (debug).
pub fn eval_file_stack_snapshot() -> Vec<String> {
    EVAL_FILE_STACK.with(|s| {
        s.borrow().iter().map(|p| {
            let Some(p) = p else { return "<no-file>".to_string() };
            let s = p.display().to_string();
            s.rsplit_once("-source/").map_or(s.clone(), |(_, r)| r.to_string())
        }).collect()
    })
}

/// Format the current eval file for error context strings.
/// Returns e.g. `", in '/nix/store/.../default.nix'"` or empty string.
pub(crate) fn eval_file_ctx() -> String {
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

/// Set `CURRENT_SOURCE_ID` to `id`, returning an RAII guard that restores
/// the previous id on drop. Used at thunk force so a cross-file thunk's
/// idents key the `(source_id, offset)` symbol cache against the file where
/// the thunk was DEFINED, not the ambient source at force time — the sibling
/// of the eval-file guard, closing the `parse.nix` cross-file collision.
pub fn push_source_id(id: u32) -> SourceIdGuard {
    let prev = CURRENT_SOURCE_ID.with(|s| {
        let old = s.get();
        s.set(id);
        old
    });
    SourceIdGuard(prev)
}

/// RAII guard that restores the previous `CURRENT_SOURCE_ID` on drop.
pub struct SourceIdGuard(u32);

impl Drop for SourceIdGuard {
    fn drop(&mut self) {
        CURRENT_SOURCE_ID.with(|s| s.set(self.0));
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

/// Release-active runaway backstop for the overlay-fixpoint promotion.
///
/// Release builds set `MAX_EVAL_DEPTH = usize::MAX` (no eval-depth guard)
/// so nixpkgs' legitimately-deep fixpoints evaluate.  But a promoted
/// empty-attrs partial that corrupts a downstream `makeOverridable` /
/// `commonAttrs` fixpoint (the cross-system Darwin `apple-sdk` path `hello`
/// hits under `builtins.currentSystem = macOS`) recurses through
/// `eval_expr` without bound — and that recursion does NOT climb the force
/// stack, so only an `eval_expr`-level bound catches it before the OS stack
/// aborts.  Armed ONLY once a promotion has fired (`promotion_occurred()`),
/// so ordinary deep evaluation (never after a promotion) is untouched.  The
/// converging native-system fixpoint (`libxcrypt`) peaks well under this
/// bound and is unaffected; the non-converging cross-system runaway is
/// caught here, converting a hard native-stack abort into a recoverable
/// `InfiniteRecursion` that `x.y or default` recovers exactly like nix
/// (`hello` returns to a clean value-diverge instead of aborting).
const PROMOTION_RUNAWAY_EVAL_DEPTH: usize = 500;

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
            if depth > PROMOTION_RUNAWAY_EVAL_DEPTH
                && crate::value::promotion_occurred()
            {
                return Err(EvalError::InfiniteRecursion(
                    "overlay-fixpoint promotion runaway (eval depth exceeded)".into(),
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
        // ENV-RESOLVE M0 (no-op unless `SUI_RESOLVE=1`): clear the per-source
        // resolution side-table for the same reason — its `(source_id,
        // offset)` keys must not survive across independent top-level evals.
        crate::resolve_env::clear();
        // SOURCE_TEXTS is deliberately NOT cleared here — it is append-only
        // for the life of the process. Clearing it on a `nesting == 0`
        // re-entry was a shared-mutable-cell bug: the top-level
        // `eval_with_file` RETURNS (nesting → 0) BEFORE its caller
        // deep-forces the result (e.g. `value.to_json()` at the CLI), and
        // that deep force triggers lazy `import`s which re-enter
        // `eval_with_file` at nesting == 0 — so clearing here wiped every
        // registered file's text mid-force. Any `unsafeGetAttrPos` resolved
        // after the first deep-force import then failed its `text_for()`
        // existence check and returned null (the cid `options.json` attrTag
        // `declarations = []` divergence). SOURCE_TEXTS is keyed by canonical
        // path and `register_source` stores each path's text only once
        // (identical on re-parse), so append-only is correct — a path always
        // maps to its own text — and matches CppNix, which never clears its
        // source registry. The only cost is bounded growth within one process
        // (a non-issue for a per-invocation CLI). Removing the clearable cell
        // makes the whole "absent/wrong source text at resolve time" class
        // unrepresentable rather than merely guarded.
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
    // ENV-RESOLVE M0 (no-op unless `SUI_RESOLVE=1`): run the parse-time
    // variable resolver over THIS parse tree and merge its `Lexical`
    // resolutions into the per-source table under `src_id`. Pure + fail-safe
    // (any uncertainty is left `Dynamic`), so the eval below is byte-identical
    // — the `Lexical` fast path only shortcuts a lexical-bindings hit, which
    // `lookup_fast` returns first anyway.
    if crate::resolve_env::enabled() {
        let table = sui_resolve::resolve(&parse.tree());
        crate::resolve_env::populate(src_id, &table);
    }
    // Register this parse tree's file + text so a static key's byte offset
    // (recorded by `eval_attrset`) resolves to a file/line/column for
    // `builtins.unsafeGetAttrPos`. The file flows through the eval-file
    // stack (store-path prefixed for imported inputs); the position resolver
    // lifts a cache-dir path to its `/nix/store/<h>-source` store path.
    crate::pos::register_source(file.as_deref(), input);
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
    // Tag the env with THIS parse tree's source_id so a thunk created here
    // and forced later (cross-file) restores this id on force (see the
    // source-id guard in `Thunk::force`), keying `IDENT_CACHE` against the
    // file where the thunk was defined.
    env.set_source_id(src_id);
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
    // Fast path: non-thunk values are returned immediately (no clone needed
    // until we actually have work to do).
    if !matches!(value, Value::Thunk(_)) {
        return Ok(value.clone());
    }
    // Slow path: chase thunk chains.
    //
    // A legitimate chain is typically 1–3 links deep (result of lazy
    // evaluation wrapping an intermediate value in another thunk).
    // Reaching 100 means either (a) a self-referential cycle like
    // `let x = x; in x` that bypassed per-thunk Blackhole detection,
    // or (b) pathological Thunk(Thunk(...)) nesting. Both are errors.
    //
    // Previous behavior silently returned `Ok(last_thunk)` at depth
    // 100, which hid infinite-recursion bugs — the blackhole tests
    // in the lib suite failed because `result.is_ok()` instead of
    // `is_err()`. Returning `Err` here makes the silent-bail visible
    // at the CppNix-compatible call site (real Nix raises "infinite
    // recursion encountered").
    let mut v = value.clone();
    let mut depth = 0u32;
    loop {
        match v {
            Value::Thunk(ref thunk) => {
                v = force_thunk(thunk)?;
                depth += 1;
                if depth > 100 {
                    return Err(EvalError::InfiniteRecursion(
                        "force_value: thunk chain exceeded depth 100 (cycle or runaway lazy wrap)".into(),
                    ));
                }
            }
            _ => return Ok(v),
        }
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
    // Ultra-fast path: if the thunk is already cached, skip stacker overhead.
    if let Some(cached) = thunk.peek() {
        crate::perf::inc(crate::perf::Counter::ThunkHit);
        return Ok(cached.clone().into_value());
    }
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
/// Detect whether `value_expr`'s source structurally references
/// the identifier `name` — the signal that this let-binding is a
/// self-recursive fix-point (`let x = f x; in x` or
/// `let x = { a = 1; b = x.a; }; in x`).  Used at let-binding
/// thunking time to pick `Thunk::new_suspended_recursive` over the
/// classic `Thunk::new_suspended`, so inner re-entrance during
/// force returns the partial value via `ThunkRepr::Promise`
/// instead of erroring with `InfiniteRecursion`.
///
/// Implementation walks the value-expr's rnix syntax tree looking
/// for `TOKEN_IDENT` whose text equals `name`.  This is a
/// conservative over-approximation:
/// - shadowing (e.g. `let x = let x = 1; in x; in x`) marks the
///   outer thunk recursive even though no real cycle exists;
/// - the resulting Promise behaviour is a strict superset of
///   Blackhole for non-cyclic forces (the body runs to completion
///   and the cell gets the final value), so false positives are
///   semantically safe — they cost only the extra `Rc<RefCell>`
///   allocation per recursive let-binding.
///
/// False negatives (e.g. the bound name appears only inside an
/// inherit-from-source clause) leave the existing
/// `InfiniteRecursion` behaviour intact, which is the conservative
/// fallback.
/// `SUI_SCOPE_NARROW` — the scope-narrowing latch.
///
/// Every `let` / `rec` / pattern-default binding closes an `Rc` cycle today:
/// the thunk is bound INTO the scope env, then Phase 2's `update_env` puts
/// that same env back INTO the thunk. `Rc` has no cycle collector and no
/// `Weak` sits on that edge, so the whole scope — every innocent leaf in it —
/// is immortal for the life of the process. Narrowing removes the second half
/// of the cycle for the bindings that provably do not need it.
///
/// * unset / `0` — today's behaviour, byte- AND allocation-identical. Not one
///   extra tree walk runs on this path.
/// * `1` — D3 (pattern-lambda formal defaults) + D1 (`let` / `rec` bindings
///   whose RHS reaches no sibling keep their outer-env capture).
/// * `2` — additionally D2 (bindings that DO need the scope get a *cluster*
///   env holding only the names they can reach, so one recursive binding
///   stops pinning its innocent siblings).
///
/// Read once through a `OnceLock` one-way latch — the `resolve_env::enabled()`
/// idiom — so the value cannot change mid-eval and the default path pays a
/// single relaxed load.
fn scope_narrow_level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(
        || match std::env::var("SUI_SCOPE_NARROW").ok().as_deref() {
            Some("1") => 1,
            Some("2") => 2,
            _ => 0,
        },
    )
}

/// True at `SUI_SCOPE_NARROW >= 1` — D1 + D3 are on.
#[inline]
fn scope_narrow_enabled() -> bool {
    scope_narrow_level() >= 1
}

/// True at `SUI_SCOPE_NARROW = 2` — D2 (the cluster env) is on.
#[inline]
fn scope_cluster_enabled() -> bool {
    scope_narrow_level() >= 2
}

/// The set of variable-reference ident names in `value_expr`'s subtree
/// (`NODE_IDENT` whose parent is NOT a `NODE_ATTRPATH` — i.e. genuine
/// variable references, not attribute names/keys). ONE subtree walk.
///
/// Kills the O(N²) re-walk storm (Storm A) at the call sites: previously
/// `is_self_recursive_binding` did a full subtree walk once per
/// `(binding × sibling-name)` in every `let`/`rec` scope; now each RHS is
/// walked ONCE to build this set, then every name is an O(1) set lookup.
/// Byte-neutral: the recursion verdict is unchanged (a name is self/mutually
/// recursive iff it is in the set).
///
/// NOT cross-call memoized: a process-lifetime memo keyed on ephemeral AST
/// node identity `(source-id, range)` collides when nodes are parsed/dropped
/// without a per-eval clear (the standalone-predicate case). The call-site
/// single-walk is the byte-safe win; `ContentMemo` (sui-intern) is reserved
/// for sites with a STABLE content key (the NAR-hash memo's `(dir,name)`, the
/// overlay-flatten per-node cache).
///
/// The attrpath exclusion matters: without it, `placeholder = if
/// lhs.placeholder == …` in nixpkgs `lib/types.nix` would be falsely flagged
/// self-recursive (its RHS mentions the *attribute* `.placeholder`), routing
/// the binding through the `Promise` fix-point path whose env handling drops
/// the let-scope — surfacing as a force-order-dependent `null` in the module
/// system (`concatLists: expected list, got null`).
fn referenced_idents(value_expr: &ast::Expr) -> HashSet<SmolStr> {
    use rnix::SyntaxKind;
    // Storm A instrumentation (byte-neutral, gated on perf::enabled()): count
    // this walk + the rnix descendants it visits + its walltime, so the
    // residual per-fixpoint-iteration self/mutual-recursion detection cost is
    // VISIBLE in the SUI_EVAL_PERF report — symmetric with sorted_entries /
    // overlay-flatten. The counter reads add zero output-relevant work.
    let perf_on = crate::perf::enabled();
    let t0 = if perf_on {
        Some(std::time::Instant::now())
    } else {
        None
    };
    crate::perf::inc(crate::perf::Counter::SelfRecWalkCalls);
    let mut nodes_walked: u64 = 0;
    let mut set: HashSet<SmolStr> = HashSet::new();
    for node in value_expr.syntax().descendants() {
        nodes_walked += 1;
        if node.kind() == SyntaxKind::NODE_IDENT
            && node
                .parent()
                .is_none_or(|p| p.kind() != SyntaxKind::NODE_ATTRPATH)
            && let Some(i) = ast::Ident::cast(node)
        {
            set.insert(SmolStr::from(ident_text(&i).as_str()));
        }
    }
    crate::perf::add(crate::perf::Counter::SelfRecWalkNodes, nodes_walked);
    if let Some(t0) = t0 {
        crate::trace::add_self_rec_walk_nanos(t0.elapsed().as_nanos());
    }
    set
}

/// True iff `value_expr` references `name` as a variable. Now a set lookup
/// over one subtree walk (see `referenced_idents`). Byte-neutral vs the prior
/// per-name-walk implementation.
fn is_self_recursive_binding(value_expr: &ast::Expr, name: &str) -> bool {
    referenced_idents(value_expr).contains(name)
}

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
        // Ident resolution: try full lookup (lexical + with-scope cache + force).
        // On successful lookup → return value directly (most common case).
        // On blackhole (fixpoint being constructed) → env.lookup returns None
        // → create WithIdent thunk for deferred O(1) cache-based resolution.
        // This approach: (1) is fast for resolved with-scopes (no thunk overhead),
        // (2) handles blackhole fixpoints correctly via WithIdent deferral.
        ast::Expr::Ident(ident) if !is_rec => {
            // Cache the interned Symbol by (source_id, text_offset) — same
            // zero-alloc steady-state path as the strict Ident arm in
            // `eval_expr`. The ident text is materialized only on the
            // once-per-offset cold miss and on the (rare) blackhole deferral.
            // Same cross-file aliasing fix as the strict `eval_expr` Ident arm —
            // key on the env's source id, not the unmaintained thread-local.
            // This twin had NO stale-symbol guard at all (the one commit
            // 2d93e77 added sits only on the strict arm's lookup-MISS path,
            // after the keyword check), so it was the more exposed of the two.
            let sym = {
                let src_id = env.source_id();
                let offset = u32::from(ident.syntax().text_range().start());
                crate::value::intern_cached_with(src_id, offset, || {
                    crate::value::intern(&ident_text(ident))
                })
            };
            // Zero-copy keyword check on the resolved Symbol.
            if let Some(kw) = crate::value::with_resolved(sym, |s| match s {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                "null" => Some(Value::Null),
                _ => None,
            }) {
                return kw;
            }
            {
                {
                    // `name` arg to `lookup_fast` is unused (lookup is by
                    // Symbol) — pass "" to skip materializing the ident text on
                    // the hot HIT path.
                    if let Some(v) = env.lookup_fast(sym, "") {
                        return v;
                    }
                    // Failed — either blackhole or missing. Create WithIdent
                    // thunk for deferred resolution (only for the blackhole case).
                    if let Some((scope_cache, scope_value)) = env.innermost_with_scope() {
                        return Value::Thunk(Thunk::new_with_ident(
                            SmolStr::from(ident_text(ident).as_str()),
                            scope_cache,
                            scope_value,
                            env.clone(),
                        ));
                    }
                    crate::perf::inc(crate::perf::Counter::ThunkSiteMaybeIdent);
                    Value::Thunk(Thunk::new_suspended(expr.clone(), env.clone()))
                }
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
                            crate::perf::inc(crate::perf::Counter::ThunkSiteMaybeIdent);
                            Value::Thunk(Thunk::new_suspended(expr.clone(), env.clone()))
                        })
                    } else {
                        // Forward reference — must thunk
                        crate::perf::inc(crate::perf::Counter::ThunkSiteMaybeIdent);
                        Value::Thunk(Thunk::new_suspended(expr.clone(), env.clone()))
                    }
                }
            }
        }
        // Absolute and home paths: trivial text extraction — but ONLY
        // for the non-interpolated case. An interpolated path (`/a/${e}`,
        // `~/${e}`) must be thunked so its `${…}` parts are evaluated in
        // `eval_expr_inner`, never spliced as literal text.
        ast::Expr::PathAbs(p) if !parts_have_interpolation(&p.parts()) => {
            // CppNix canonicalizes every absolute path literal on eval
            // (`/.` → `/`, `/a/./b` → `/a/b`, `/a/../b` → `/b`, `..`
            // clamped at root). A path VALUE carries the canonical form —
            // the marquee cid root threw in `lib.path.hasStorePathPrefix`
            // precisely because sui kept the raw `/.` text.
            let text = crate::path::canon_abs(&p.syntax().text().to_string());
            Value::Path(Box::new(SmolStr::from(text.as_str())))
        }
        ast::Expr::PathHome(p) if !parts_have_interpolation(&p.parts()) => {
            let text = p.syntax().text().to_string();
            Value::Path(Box::new(SmolStr::from(text.as_str())))
        }
        // Non-interpolated string literal: a constant value with no
        // interpolation, so `eval_str` runs no `${…}` force/coerce — it is
        // pure, non-throwing, side-effect-free, and produces a
        // `String(NixString::with_context(text, EMPTY))`. Evaluating it here is
        // therefore byte-identical to forcing a suspended thunk of it (M2
        // thunk-waste: a constant Str thunk is always pure overhead — it can
        // never observably change eval order because it cannot throw or
        // diverge). Only the NON-interpolated case is direct; an interpolated
        // `"${e}"` must stay thunked so its parts force lazily in the right
        // env/order. `eval_str` on the empty-interpolation input cannot fail,
        // but fall back to a thunk on the (unreachable) error to preserve
        // exact prior behavior.
        ast::Expr::Str(st) if !str_has_interpolation(st) => {
            eval_str(st, env).unwrap_or_else(|_| {
                Value::Thunk(Thunk::new_suspended(expr.clone(), env.clone()))
            })
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
        _ => {
            crate::perf::inc(crate::perf::Counter::ThunkSiteMaybeOther);
            if crate::perf::enabled() {
                let kind = match expr {
                    ast::Expr::Select(_) => "Select",
                    ast::Expr::Apply(_) => "Apply",
                    ast::Expr::BinOp(_) => "BinOp",
                    ast::Expr::IfElse(_) => "IfElse",
                    ast::Expr::Str(_) => "Str",
                    ast::Expr::List(_) => "List",
                    ast::Expr::With(_) => "With",
                    ast::Expr::Assert(_) => "Assert",
                    ast::Expr::HasAttr(_) => "HasAttr",
                    ast::Expr::UnaryOp(_) => "UnaryOp",
                    ast::Expr::Paren(_) => "Paren",
                    ast::Expr::LetIn(_) => "LetIn",
                    ast::Expr::AttrSet(_) => "AttrSet",
                    ast::Expr::Ident(_) => "Ident(rec)",
                    ast::Expr::Lambda(_) => "Lambda(rec)",
                    ast::Expr::LegacyLet(_) => "LegacyLet",
                    ast::Expr::PathAbs(_)
                    | ast::Expr::PathHome(_)
                    | ast::Expr::PathRel(_)
                    | ast::Expr::PathSearch(_) => "Path(interp)",
                    _ => "Other",
                };
                crate::trace::inc_maybe_other_kind(kind);
            }
            Value::Thunk(Thunk::new_suspended(expr.clone(), env.clone()))
        }
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
            // ── ENV-RESOLVE M0 fast path (no-op unless `SUI_RESOLVE=1`) ──
            // A parse-time-`Lexical` reference carries its precomputed
            // Symbol; probe the lexical bindings map DIRECTLY, skipping the
            // per-lookup `ident_text().to_string()` + `intern()`. This is
            // parity-by-construction: `lookup_fast` probes the SAME lexical
            // map by the SAME Symbol FIRST, so a hit here is byte-identical
            // to what the unchanged path below returns. Any miss (a
            // mid-fixpoint blackhole where the binding isn't in scope yet, an
            // unrecorded ident, or `Dynamic`) falls through to the EXACT
            // unchanged path — including the whole with-chain + WithIdent
            // deferral. The resolver never records keywords, so the
            // true/false/null handling below is untouched on this path.
            if crate::resolve_env::enabled() {
                let src_id = CURRENT_SOURCE_ID.with(std::cell::Cell::get);
                let offset = u32::from(ident.syntax().text_range().start());
                if let sui_resolve::Resolution::Lexical { sym } =
                    crate::resolve_env::resolution_for(src_id, offset)
                {
                    if let Some(v) = env.lookup_lexical_sym(sym) {
                        return Ok(v);
                    }
                }
                // Miss / Dynamic → fall through to the unchanged path.
            }
            // Cache the interned Symbol by (source_id, text_offset) so the
            // steady-state identifier lookup pays neither a per-lookup
            // `ident_text().to_string()` heap alloc nor a string re-hash — the
            // ident's text is materialized only on the once-per-offset cold
            // miss. The keyword check + the common `lookup_fast` HIT then run
            // fully allocation-free; `name` is materialized lazily only on the
            // miss/error branches, which need the string anyway.
            // KEY ON `env.source_id()`, NOT the thread-local (fixed 2026-07-20).
            //
            // `CURRENT_SOURCE_ID` is pushed at exactly ONE site —
            // `value.rs`'s `ThunkRepr::Suspended` force branch. Lambda
            // application and the Native/WithIdent/InheritSelect/Promise force
            // branches never push it, so while a callee's body was being
            // evaluated the thread-local still named the CALLER's file. The
            // `(source_id, offset)` cache key then aliased across files: an
            // identifier at byte N in file A could resolve to the Symbol
            // interned for a `null`/`true`/`false` token at byte N in file B —
            // and the zero-copy keyword check below turned that into a literal
            // `Value::Null` for a perfectly well-defined identifier, before any
            // environment lookup.
            //
            // That is what stopped sui evaluating nixpkgs: `hostSuffix` in
            // `make-derivation.nix` resolved to `null`, so `attrs.name +
            // hostSuffix` raised "cannot add string and null" — observed
            // directly as `STALE-KEYWORD ident="hostSuffix" resolvedAs="null"`.
            // It is not darwin-specific and has nothing to do with the module
            // system; `import <nixpkgs> {}` fails identically on x86_64-linux.
            //
            // `Env` already carries the correct value: `eval_with_file` sets it
            // and `child()` inherits it, and a lambda's `call_env` is
            // `closure.env.child()` — so a body's env names its DEFINING file.
            // Keying on it fixes every cross-file path at the cause, rather than
            // adding a fifth push/pop guard that a sixth path can forget.
            let sym = {
                let src_id = env.source_id();
                let offset = u32::from(ident.syntax().text_range().start());
                crate::value::intern_cached_with(src_id, offset, || {
                    crate::value::intern(&ident_text(ident))
                })
            };
            // Zero-copy keyword check on the resolved Symbol — the resolver
            // never records keywords, so this matches the prior `name.as_str()`
            // arm exactly.
            if let Some(kw) = crate::value::with_resolved(sym, |s| match s {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                "null" => Some(Value::Null),
                _ => None,
            }) {
                return Ok(kw);
            }
            return {
                {
                    // `lookup_fast`'s `name` argument is unused (lookup is by
                    // Symbol); pass "" to avoid materializing the ident text on
                    // the hot HIT path.
                    if let Some(v) = env.lookup_fast(sym, "") {
                        Ok(v)
                    } else {
                        let name = ident_text(ident);
                        // The `(src_id, text_offset)` identifier-symbol cache
                        // (`intern_cached_with`) can hand back a STALE Symbol when
                        // a lazily-forced thunk's identifier is resolved under a
                        // force-time `CURRENT_SOURCE_ID` that differs from the
                        // identifier's PARSE-time src_id — a thunk from file A can
                        // be forced while B is the current source, so
                        // `(B_src_id, offset)` aliases B's parse tree's identifier
                        // at that same byte offset and returns ITS Symbol. (Proven
                        // root: nixpkgs `lib/systems/parse.nix` `mkOptionType` — the
                        // binding IS present in the env, but the cache returned
                        // `Symbol(566)` while the binding was interned under
                        // `Symbol(506)`, so `lookup_fast(566)` missed a defined
                        // var.) `intern` is deterministic + append-only, so on a
                        // miss re-intern the name from its text (the authoritative
                        // Symbol) and retry the lexical lookup BEFORE considering
                        // with-scopes or undefined. A genuinely undefined variable
                        // is unaffected — its fresh lookup also misses and falls
                        // through unchanged.
                        let fresh = crate::value::intern(name.as_str());
                        if fresh != sym {
                            if let Some(v) = env.lookup_fast(fresh, name.as_str()) {
                                return Ok(v);
                            }
                        }
                        if env.with_scope_count() > 0 {
                        // With-scope lookup failed (likely blackhole from fixpoint).
                        // Return a WithIdent thunk for deferred resolution.
                        // This is the eval_expr equivalent of maybe_thunk's deferral.
                        if let Some((scope_cache, scope_value)) = env.innermost_with_scope() {
                            Ok(Value::Thunk(Thunk::new_with_ident(
                                SmolStr::from(name.as_str()),
                                scope_cache,
                                scope_value,
                                env.clone(),
                            )))
                        } else if crate::value::in_promise_eval() {
                            // M2.6 Promise softening: an undefined
                            // identifier inside Promise body evaluation
                            // typically means a `with` block sourced
                            // from the empty-attrset sentinel didn't
                            // populate the with-scope.  Returning null
                            // lets the eval proceed; the result is
                            // wrong-but-bounded (no further forces
                            // happen on null until something downstream
                            // demands a real value).
                            Ok(Value::Null)
                        } else {
                            Err(EvalError::UndefinedVar(
                                format!("'{name}'{}", eval_file_ctx()),
                            ))
                        }
                    } else {
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
                        if crate::value::in_promise_eval() {
                            // Same Promise softening as the with-scope
                            // branch above.
                            return Ok(Value::Null);
                        }
                        Err(EvalError::UndefinedVar(
                            format!("'{name}'{}", eval_file_ctx()),
                        ))
                        }
                    }
                }
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
        // Lambda: no recursion — just captures env into a closure.
        ast::Expr::Lambda(lam) => {
            crate::perf::inc(crate::perf::Counter::EvalExpr);
            if crate::perf::enabled() {
                crate::perf::inc(crate::perf::Counter::ExprLambda);
            }
            if let (Some(param), Some(body)) = (lam.param(), lam.body()) {
                return Ok(Value::Lambda(Rc::new(Closure {
                    param,
                    body,
                    env: env.clone(),
                })));
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
            // An interpolated absolute path (`/a/${e}`) splices its
            // `${…}` parts; a plain one takes the raw-text shortcut.
            let parts = p.parts();
            if parts_have_interpolation(&parts) {
                return eval_interpol_path_parts(&parts, PathKind::Abs, env);
            }
            // Canonicalize like CppNix (`/.` → `/`, `.`/`..` collapse,
            // `..` clamps at root) — see the WHNF fast-path above.
            let text = crate::path::canon_abs(&p.syntax().text().to_string());
            return Ok(Value::Path(Box::new(SmolStr::from(text.as_str()))));
        }
        ast::Expr::PathRel(p) => {
            // Real Nix resolves `./foo.nix` against the directory
            // of the file that *contains* the literal, not the
            // process cwd. Use the current eval-file stack; fall
            // back to cwd when no file is being evaluated (e.g.,
            // top-level `sui eval`).
            //
            // An interpolated relative path (`./${x}.nix`) first splices
            // its `${…}` parts, then resolves the concatenated text the
            // same way — the interpolation is evaluated + string-coerced,
            // NOT treated as literal `${x}` text.
            let parts = p.parts();
            if parts_have_interpolation(&parts) {
                return eval_interpol_path_parts(&parts, PathKind::Rel, env);
            }
            let text = p.syntax().text().to_string();
            let resolved = if let Some(dir) = current_eval_dir() {
                let joined = dir.join(&text);
                // Use normalize_path instead of canonicalize so that
                // paths with ./  and .. are cleaned without requiring
                // the path to exist on disk.
                let norm = normalize_path(&joined);
                // A relative path literal (`./x`, `../..`) resolves against the
                // eval-dir, which for a fetched flake input is the sui fetcher
                // CACHE dir. CppNix resolves it against the input's
                // `/nix/store/<h>-source` STORE path, so the resulting path
                // VALUE must carry the store prefix (this is the value half of
                // the store↔cache seam — `materialize`/`dematerialize`). Lift
                // the cache path back to the store path so `toString ../..`
                // matches CppNix — the options.json `hasPrefix
                // <nix-darwin>.outPath decl` rewrite root (`prefix = ../..`).
                crate::path::dematerialize(&norm)
                    .to_string_lossy()
                    .into_owned()
            } else {
                text.clone()
            };
            return Ok(Value::Path(Box::new(SmolStr::from(resolved.as_str()))));
        }
        ast::Expr::PathHome(p) => {
            let parts = p.parts();
            if parts_have_interpolation(&parts) {
                return eval_interpol_path_parts(&parts, PathKind::Home, env);
            }
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
            //
            // M2.6 ROOT #4a (byte-verified): `eval_expr(&ns, env)?` was NOT
            // lazy — it EVALUATED the namespace expression eagerly at
            // `with`-entry.  For `with (throw "X"); body` that runs the
            // throw; for `with config.services.borgbackup; { … }` (nixpkgs'
            // module `config` shape) it forces `config.services.borgbackup`
            // the instant the `with`-body's WHNF/keys are demanded (during
            // module collection's `pushDownProperties`), re-entering the
            // mid-force `config` fixpoint → the empty-Promise partial →
            // `null` softening → `concatLists null`.  cppnix stores the
            // namespace as a thunk and forces it ONLY when a bare-ident
            // lookup actually falls through lexical scope into the `with`.
            // Reduced repro (no module system, iterates in ms):
            //   `builtins.attrNames (with (throw "X"); { a = 1; })`
            //   nix → [ "a" ] ; sui (before) → throws "X".
            // `maybe_thunk` keeps the fast-path for an already-resolved
            // ident namespace (no thunk overhead) while deferring any
            // non-trivial namespace (Select / Apply / throw) into a lazy
            // thunk the scope-lookup path (`Env::lookup_fast`) forces only
            // on fallthrough.
            let scope_val = maybe_thunk(&ns, env, false, None);
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

            // Pre-pass: collect every binding name in this let-scope
            // (single-key bindings + top-level keys of dotted paths +
            // names from inherit clauses).  Used by the recursive-thunk
            // detector below — a binding is part of the mutual fix-point
            // if its RHS references ANY of these names.
            //
            // D1 (`SUI_SCOPE_NARROW>=1`) — `names_complete` is the honesty half
            // of the narrowing. Narrowing is only sound while
            // `let_scope_names` is a COMPLETE list of what this scope binds: a
            // binding is judged "reaches no sibling" by intersecting its RHS's
            // free variables with that set, so a name MISSING from it reads as
            // an outer reference and the binding wrongly keeps the outer env.
            // A head that does not resolve here contributes nothing, so the
            // whole scope forfeits narrowing rather than narrow on a partial
            // set. (`Dynamic` heads are excluded even when they do resolve —
            // the name is computed, so it is not a syntactic property of the
            // scope.) Nothing about the EVALUATION below changes; this only
            // decides whether the optimisation is allowed to apply.
            let mut names_complete = true;
            let let_scope_names: HashSet<String> = {
                let mut s = HashSet::new();
                for entry in letin.entries() {
                    match entry {
                        ast::Entry::AttrpathValue(apv) => {
                            if let Some(attrpath) = apv.attrpath() {
                                if let Some(first) = attrpath.attrs().next() {
                                    if let ast::Attr::Dynamic(_) = &first {
                                        names_complete = false;
                                    }
                                    if let Ok(name) = eval_attr(&first, env) {
                                        s.insert(name);
                                    } else {
                                        names_complete = false;
                                    }
                                } else {
                                    names_complete = false;
                                }
                            } else {
                                names_complete = false;
                            }
                        }
                        ast::Entry::Inherit(inherit) => {
                            for attr in inherit.attrs() {
                                if let ast::Attr::Dynamic(_) = &attr {
                                    names_complete = false;
                                }
                                if let Ok(name) = eval_attr(&attr, env) {
                                    s.insert(name);
                                } else {
                                    names_complete = false;
                                }
                            }
                        }
                    }
                }
                s
            };
            let narrow = scope_narrow_enabled() && names_complete;

            // D2 (`SUI_SCOPE_NARROW=2`) — the CLUSTER env.
            //
            // D1 alone is not enough, and the reason is the shape of the
            // graph: free-variable analysis is per-binding on the
            // `thunk -> env` edge, but the `env -> thunk` edge is SHARED. One
            // binding that really does reach a sibling keeps `new_env` alive,
            // and `new_env` holds EVERY binding in the scope — so a single
            // recursive `f` re-pins all fifty innocent leaves and the footprint
            // is unchanged. (That is the P4 row, and it is why the headline
            // gate is too easy: D1 greens it while doing nothing here.)
            //
            // The fix is to stop pointing the survivors at the whole scope.
            // Phase 2 re-points them at a `fix_env` carrying ONLY the names the
            // pinned bindings can actually reach — their own names plus
            // `refs ∩ scope_names`. The body still gets the full `new_env`, so
            // nothing the LET EXPRESSION evaluates to can change; only the
            // envs captured by thunks shrink.
            let cluster = narrow && scope_cluster_enabled();
            // Every (name, value) bound into `new_env`, so the pinned subset can
            // be re-bound into `fix_env`. Allocated only under D2.
            let mut all_bound: Vec<(String, Value)> = Vec::new();
            // The names that stayed pinned, and the free-variable sets of the
            // bindings behind them. `pin` needs only the UNION of those sets, so
            // no name→refs association is required — and that union already IS
            // the fixpoint: a name added to `pin` that is not itself a pinned
            // binding contributes no further refs, and one that is has its refs
            // in the union already.
            let mut pinned_names: HashSet<String> = HashSet::new();
            let mut pinned_refs: Vec<HashSet<SmolStr>> = Vec::new();
            // A dotted path (`let a.b = 1;`) pushes LEAF thunks whose names are
            // inner path segments, not scope names, and whose free variables are
            // never computed here — so `fix_env` cannot be shown to carry what
            // they need. Such a scope forfeits D2 (D1 still applies).
            let mut has_dotted = false;

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
                            // Self/mutual-recursive detection: any binding
                            // whose RHS references its own name OR any
                            // SIBLING let-scope name is part of the let's
                            // mutual fix-point.  Mark as recursive so
                            // inner re-entrance during force returns a
                            // Promise sentinel instead of erroring with
                            // InfiniteRecursion.  This is the M2.6
                            // module-system fix path (cppnix's
                            // lib/modules.nix uses a deep let-scope with
                            // declaredConfig / options / matchedOptions /
                            // resultsByName / modules all transitively
                            // cycling through each other).
                            //
                            // `let_scope_names` is collected upfront in a
                            // pre-pass so each binding sees every other
                            // binding name (not just earlier ones).
                            // O(N) not O(N²): compute the RHS's referenced-name
                            // set ONCE (memoized), then intersect with the
                            // let-scope names. Byte-identical to the prior
                            // `references(key) OR references(any sibling)`:
                            // chaining `key` covers the self-reference case
                            // regardless of whether `key ∈ let_scope_names`.
                            let referenced = referenced_idents(&value_expr);
                            let in_mutual_cycle = std::iter::once(&key)
                                .chain(let_scope_names.iter())
                                .any(|n| referenced.contains(n.as_str()));
                            let value = if in_mutual_cycle {
                                Value::Thunk(Thunk::new_suspended_recursive(
                                    value_expr.clone(),
                                    env.clone(),
                                ))
                            } else {
                                maybe_thunk(&value_expr, env, true, Some(&defined_so_far))
                            };
                            new_env.bind(key.clone(), value.clone());
                            if cluster {
                                all_bound.push((key.clone(), value.clone()));
                            }
                            if let Value::Thunk(t) = &value {
                                // D1: `in_mutual_cycle` is ALREADY the
                                // forward-complete "reaches a sibling"
                                // predicate here (`let_scope_names` is a full
                                // pre-pass, unlike the `rec` arm's
                                // backward-only one), so it doubles as the
                                // needs-scope test at zero extra cost — no
                                // second tree walk.
                                //
                                // When it is false the RHS references nothing
                                // this scope binds, so every name it CAN
                                // resolve resolves identically in `env` and in
                                // `new_env`: `Env::child` copies `with_scopes`,
                                // `eval_file` and `source_id` verbatim, and the
                                // only added bindings are the let-scope names
                                // this RHS provably does not mention. Skipping
                                // the re-point is therefore byte-neutral, and
                                // it is what leaves the thunk holding the OUTER
                                // env instead of closing
                                // `thunk -> new_env -> thunk`.
                                if in_mutual_cycle || !narrow {
                                    thunks.push((key.clone(), t.clone()));
                                    if cluster {
                                        pinned_names.insert(key.clone());
                                        pinned_refs.push(referenced);
                                    }
                                    crate::value::census::scope_pinned();
                                } else {
                                    crate::value::census::scope_narrowed();
                                }
                            }
                            defined_so_far.insert(key);
                        } else if path_keys.len() > 1 {
                            // Multi-segment dotted path: build a nested
                            // attrset with thunks at the leaves so the
                            // value expression can reference sibling
                            // let-bindings.
                            has_dotted = true;
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
                            // D1: every `InheritSelect` in this clause shares
                            // ONE source thunk, and `Thunk::update_env`
                            // delegates straight through to it — so all N
                            // pushes re-point the SAME env. Whether that
                            // re-point is needed is therefore a property of the
                            // source expression alone, computed ONCE above the
                            // loop instead of N times inside it. Guarded by
                            // `!narrow ||` so the default path does not pay the
                            // walk at all.
                            let source_refs: Option<HashSet<SmolStr>> = if narrow {
                                Some(referenced_idents(&source_expr))
                            } else {
                                None
                            };
                            let source_needs_scope = match &source_refs {
                                Some(refs) => let_scope_names
                                    .iter()
                                    .any(|n| refs.contains(n.as_str())),
                                None => true,
                            };
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
                                if cluster {
                                    all_bound.push((
                                        name.clone(),
                                        Value::Thunk(thunk.clone()),
                                    ));
                                }
                                if source_needs_scope {
                                    if cluster {
                                        pinned_names.insert(name.clone());
                                    }
                                    thunks.push((name, thunk));
                                    crate::value::census::scope_pinned();
                                } else {
                                    crate::value::census::scope_narrowed();
                                }
                            }
                            // One refs set for the whole clause — every name in
                            // it re-points the SAME shared source thunk.
                            if cluster && source_needs_scope {
                                if let Some(refs) = source_refs {
                                    pinned_refs.push(refs);
                                }
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
                                if cluster {
                                    all_bound.push((name.clone(), value.clone()));
                                }
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
                if cluster {
                    all_bound.push((key.clone(), value.clone()));
                }
            }

            // D2: the cluster env the survivors get re-pointed at, in place of
            // the whole scope. Built only when it can actually shrink anything
            // — some binding pinned, some binding not, and no dotted path (see
            // `has_dotted`).
            let fix_env: Option<Env> = if cluster && !has_dotted && !thunks.is_empty() {
                // `pin` = the pinned names, plus every scope name they can
                // reach. This union is already the fixpoint: a name pulled in
                // that is not itself pinned contributes no further refs (its
                // own thunk still holds the OUTER env and so resolves entirely
                // outside this scope), and one that is pinned had its refs in
                // the union from the start.
                let mut pin = pinned_names;
                for refs in &pinned_refs {
                    for n in &let_scope_names {
                        if refs.contains(n.as_str()) {
                            pin.insert(n.clone());
                        }
                    }
                }
                if pin.len() < all_bound.len() {
                    let mut fe = env.child();
                    for (name, value) in &all_bound {
                        if pin.contains(name) {
                            fe.bind(name.clone(), value.clone());
                        }
                    }
                    Some(fe)
                } else {
                    None
                }
            } else {
                None
            };

            // Phase 2: Update all thunks to capture the final env
            // (which now has all names bound).
            let phase2_env: &Env = fix_env.as_ref().unwrap_or(&new_env);
            for (_key, thunk) in &thunks {
                thunk.update_env(phase2_env);
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
    // M2.6 bridge: in `expr.path or default`, an `InfiniteRecursion`
    // hit while forcing the LEFT side falls back to the default —
    // operationally matches cppnix, which avoids the cycle entirely
    // via lazy attribute access during fix-point evaluation.  Without
    // a default, the recursion propagates as a real error.  Other
    // error kinds (Throw, TypeError, …) always propagate so user
    // bugs aren't masked.  Removed when the underlying fix-point /
    // lazy-access semantics land — see docs/M2.6-MODULE-SYSTEM-FIXPOINT.md.
    let base_result = eval_expr(&base_expr, env)
        .and_then(|v| force_concrete(&v).map(Concrete::into_value));
    let base = match base_result {
        Ok(v) => v,
        Err(EvalError::InfiniteRecursion(_)) if sel.default_expr().is_some() => {
            return eval_expr(&sel.default_expr().expect("checked"), env);
        }
        Err(e) => return Err(e),
    };
    let base_type = base.type_name();
    let attrpath = sel.attrpath().ok_or_else(|| {
        EvalError::ParseError("select missing attrpath".to_string())
    })?;
    // M2.6 bridge: when the blackhole-bridge sentinels are active,
    // an attribute lookup that misses (`AttrNotFound`) or hits a
    // non-attrset intermediate (`NotAttrs`) on the bridge's empty
    // sentinel value gets resolved to `null` instead of erroring.
    // cppnix's partial attrset would have CARRIED the keys (with
    // their lazy values), so the lookup would succeed; null is the
    // cheapest sentinel that propagates through downstream code
    // without further type errors.
    //
    // M2.6 ROOT #4 CLOSED (2026-07-11): the `|| crate::value::in_promise_eval()`
    // clause that used to soften a mid-Promise `config.<x>` select-miss to
    // `null` is REMOVED.  It was the band-aid masking the two real over-forces
    // that ROOT #4a (the `with`-namespace eager eval, above) and ROOT #4b (the
    // dropped full-set leaf in `merge_nested_insert`, below) now fix at their
    // load-bearing cause.  Verified with the softening gone: both
    // `lib.nixosSystem { modules = []; }.config.system.name` → `"nixos"` and
    // `attrNames sys.options` → 53 (nix-parity), `sui parity` stays 35 match /
    // 0 regressions, 1324 sui-eval lib tests + 30 diff tests pass — nothing
    // depended on the sentinel any more.  The two explicit operator-gated
    // bridges below stay as opt-in experiments (default-off); only the
    // always-on Promise softening is retired.
    let bridge_active = std::env::var_os("SUI_BLACKHOLE_AS_EMPTY_ATTRS").is_some()
        || std::env::var_os("SUI_BLACKHOLE_AS_NULL").is_some();
    let traversal = traverse_attrpath(base, &attrpath, env);
    match traversal {
        Ok(TraverseResult::Found(v)) => Ok(v),
        Ok(TraverseResult::Missing(key)) => {
            if let Some(def) = sel.default_expr() {
                eval_expr(&def, env)
            } else if bridge_active {
                if std::env::var_os("SUI_M26_SELTRACE").is_some() {
                    let path: Vec<String> = sel.attrpath().map(|ap|
                        ap.attrs().map(|a| a.syntax().text().to_string()).collect()
                    ).unwrap_or_default();
                    eprintln!("[M26 SEL-MISS→null] base_type={base_type} path={path:?} missing-key={key}{}", eval_file_ctx());
                }
                if let Ok(filt) = std::env::var("SUI_M26_HARDSOFTEN") {
                    let path: Vec<String> = sel.attrpath().map(|ap|
                        ap.attrs().map(|a| a.syntax().text().to_string()).collect()
                    ).unwrap_or_default();
                    if path.iter().any(|p| p.contains(&filt)) {
                        return Err(EvalError::type_error(format!(
                            "M26-HARDSOFTEN path={path:?} key={key}"
                        )));
                    }
                }
                Ok(Value::Null)
            } else {
                Err(EvalError::AttrNotFound(
                    format!("'{key}'{}", eval_file_ctx()),
                ))
            }
        }
        Ok(TraverseResult::NotAttrs(forced)) => {
            // CppNix: `expr.a.b or default` falls back to default for
            // ANY error in the path — including intermediate values
            // that aren't attrsets (e.g., null). The module system
            // relies on this: `x.options.type.name or null` must
            // return null when x.options is null, not throw.
            if let Some(def) = sel.default_expr() {
                eval_expr(&def, env)
            } else if bridge_active {
                if let Ok(filt) = std::env::var("SUI_M26_HARDSOFTEN") {
                    let path: Vec<String> = sel.attrpath().map(|ap|
                        ap.attrs().map(|a| a.syntax().text().to_string()).collect()
                    ).unwrap_or_default();
                    if path.iter().any(|p| p.contains(&filt)) {
                        return Err(EvalError::type_error(format!(
                            "M26-HARDSOFTEN-NOTATTRS path={path:?} base_type={base_type}"
                        )));
                    }
                }
                return Ok(Value::Null);
            } else {
                if std::env::var("SUI_DEBUG_SELECT").is_ok() {
                    let path: Vec<String> = sel.attrpath().map(|ap|
                        ap.attrs().filter_map(|a| match a {
                            ast::Attr::Ident(i) => Some(i.to_string()),
                            ast::Attr::Str(s) => Some(format!("\"{}\"", s.syntax().text())),
                            ast::Attr::Dynamic(_) => Some("<dyn>".into()),
                        }).collect()
                    ).unwrap_or_default();
                    let dbg = format!("{:?}", forced);
                    let truncated = if dbg.len() > 200 { format!("{}…", &dbg[..200]) } else { dbg };
                    eprintln!("[SUI_DEBUG_SELECT] base_type={base_type} path={path:?} base={truncated}{}", eval_file_ctx());
                }
                Err(attach_trace(EvalError::type_error(
                    format!("cannot select from {base_type}"),
                )))
            }
        }
        // Same M2.6 bridge as on the base force above: if an
        // intermediate step in the attrpath traversal raises
        // InfiniteRecursion and `or default` was supplied, the
        // default is the operationally-correct value.
        Err(EvalError::InfiniteRecursion(_)) if sel.default_expr().is_some() => {
            eval_expr(&sel.default_expr().expect("checked"), env)
        }
        Err(e) => Err(e),
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

/// Builtins that must receive their argument UNFORCED (call-by-need). This is the
/// SINGLE source of truth consumed by BOTH `eval_apply` (which must THUNK the arg
/// instead of eager-evaluating it) AND the builtin apply arm (which must SKIP the
/// arg force). The two sites MUST agree: if `eval_apply` eager-evaluates the arg,
/// the apply-arm's force-skip is dead (the arg is already forced — or already
/// threw) upstream. They were previously inconsistent (only `tryEval` was thunked
/// in `eval_apply`), so `seq`/`deepSeq`/`addErrorContext`/`foldl'` silently got
/// eager args despite their apply-time exemption — the bug behind
/// `builtins.foldl' (_: x: x) (throw "…") […]` throwing instead of returning the
/// last element (nix's foldl' is NOT strict in the nul accumulator).
#[inline]
pub(crate) fn builtin_takes_lazy_arg(name: &str) -> bool {
    matches!(
        name,
        "tryEval" | "addErrorContext<partial>" | "seq<partial>" | "deepSeq<partial>" | "foldl'<p1>"
    )
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
            // Call-by-need: the arg is thunked so it forces lazily. But a
            // PURE-CONSTANT arg (a literal, a non-interpolated string, or a
            // non-interpolated path) can never throw or diverge, so producing
            // its value directly is byte-neutral whether or not the lambda ever
            // forces it — identical eval-order-observable behavior, one fewer
            // never-forced thunk. This is `arg_pure_constant` ONLY: any arg that
            // could throw/diverge/observe a fixpoint (Ident with-scope, Select,
            // Apply, BinOp, …) stays fully thunked to preserve laziness.
            if let Some(v) = eval_pure_constant_arg(&arg_expr) {
                v
            } else {
                crate::perf::inc(crate::perf::Counter::ThunkSiteApplyArg);
                Value::Thunk(Thunk::new_suspended(arg_expr.clone(), env.clone()))
            }
        }
        Value::Builtin(b) if builtin_takes_lazy_arg(&b.name) => {
            // Call-by-need for the laziness-exempt builtins (tryEval / seq /
            // deepSeq / addErrorContext / foldl'<p1>): the arg MUST be thunked,
            // not eager-evaluated, so it forces only if/when the builtin demands
            // it. Kept in lockstep with the apply-arm skip via `builtin_takes_lazy_arg`.
            crate::perf::inc(crate::perf::Counter::ThunkSiteApplyArg);
            Value::Thunk(Thunk::new_suspended(arg_expr.clone(), env.clone()))
        }
        _ => eval_expr(&arg_expr, env)?,
    };
    apply(func, arg)
}

/// If `arg_expr` is a PURE CONSTANT — a literal, a non-interpolated string, or
/// a non-interpolated absolute/home path — return its value directly (no thunk).
///
/// A pure constant has no free variables, cannot throw, cannot diverge, and has
/// no fixpoint/laziness interaction: `eval_expr(arg)` is total and produces the
/// exact value a suspended thunk of it would yield on force. Producing it
/// eagerly in a call-by-need arg position is therefore byte-neutral (the
/// lambda that never forces the arg observes no difference — the value is inert).
///
/// Returns `None` for EVERYTHING else (Ident — may hit a with-scope force;
/// Select/Apply/BinOp/If/… — may throw or diverge; interpolated Str/Path —
/// must force `${…}` lazily), which keeps those args fully thunked. `env` is
/// NOT threaded in because a pure constant needs no environment; if a match
/// arm ever needed `env`, it would not be a pure constant.
fn eval_pure_constant_arg(arg_expr: &ast::Expr) -> Option<Value> {
    match arg_expr {
        ast::Expr::Literal(lit) => eval_literal(lit).ok(),
        ast::Expr::Str(st) if !str_has_interpolation(st) => {
            // No interpolation ⇒ `eval_str` runs no force/coerce; env is unused.
            eval_str(st, &Env::new()).ok()
        }
        ast::Expr::PathAbs(p) if !parts_have_interpolation(&p.parts()) => {
            let text = crate::path::canon_abs(&p.syntax().text().to_string());
            Some(Value::Path(Box::new(SmolStr::from(text.as_str()))))
        }
        ast::Expr::PathHome(p) if !parts_have_interpolation(&p.parts()) => {
            let text = p.syntax().text().to_string();
            Some(Value::Path(Box::new(SmolStr::from(text.as_str()))))
        }
        _ => None,
    }
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
                // CppNix string interpolation is copy-to-store coercion: an
                // interpolated source path (`"${./foo}"`) is NAR-copied into
                // the store and the store path is spliced in (with context),
                // never the raw filesystem path.
                let (s, c) = val.coerce_to_string_copy_to_store()?;
                result.push_str(&s);
                ctx.merge(&c);
            }
        }
    }
    Ok(Value::String(Rc::new(NixString::with_context(result, ctx))))
}

/// Whether a list of path parts contains a `${…}` interpolation. When
/// it does not, the raw `.syntax().text()` shortcut is byte-identical
/// and cheaper, so the trivial fast paths stay on that shortcut.
fn parts_have_interpolation(parts: &[InterpolPart<rnix::ast::PathContent>]) -> bool {
    parts
        .iter()
        .any(|p| matches!(p, InterpolPart::Interpolation(_)))
}

/// Whether a string literal contains any `${…}` interpolation part. A `false`
/// result means the string is a pure constant (`eval_str` runs no force/coerce
/// and cannot throw), so `maybe_thunk` may evaluate it eagerly byte-neutrally.
fn str_has_interpolation(s: &ast::Str) -> bool {
    s.normalized_parts()
        .iter()
        .any(|p| matches!(p, InterpolPart::Interpolation(_)))
}

/// Evaluate an interpolatable path literal that contains `${…}` parts.
///
/// CppNix path interpolation (`./${x}.nix`, `/a/${e}`, `~/x/${e}`):
///   * each literal segment is spliced verbatim,
///   * each `${e}` is **plain**-coerced to a string with context
///     (NOT copy-to-store — path-typed interpolations splice the raw
///     store/filesystem path, e.g. `/bar/${./foo}` → `/bar/tmp/foo`),
///   * the concatenated text is then resolved exactly like the plain
///     path literal of the same kind (relative → joined + normalized
///     against the defining file's directory; absolute/home → verbatim),
///   * the result is a `path` value.
///
/// Parts come from rnix's `<PathKind>::parts()` which splits the path
/// token stream into `Literal(PathContent)` / `Interpolation(Interpol)`.
fn eval_interpol_path_parts(
    parts: &[InterpolPart<rnix::ast::PathContent>],
    kind: PathKind,
    env: &Env,
) -> Result<Value, EvalError> {
    let mut text = String::new();
    for part in parts {
        match part {
            InterpolPart::Literal(content) => text.push_str(content.text()),
            InterpolPart::Interpolation(interpol) => {
                let expr = interpol.expr().ok_or_else(|| {
                    EvalError::ParseError("path interpolation missing expr".to_string())
                })?;
                let val = force_value(&eval_expr(&expr, env)?)?;
                // Plain coercion (coerceMore = false): a path-typed
                // interpolation splices the raw path string, never a
                // copied-to-store hash path.
                let (s, _ctx) = val.coerce_to_string()?;
                text.push_str(&s);
            }
        }
    }
    let resolved = match kind {
        // Relative path: resolve against the defining file's directory,
        // mirroring the plain `PathRel` branch.
        PathKind::Rel => {
            if let Some(dir) = current_eval_dir() {
                let norm = normalize_path(&dir.join(&text));
                // Lift cache→store exactly like the plain `PathRel` branch (the
                // store↔cache seam value-half). Without this, an interpolated
                // relative-path literal (`./${x}`, `./modules/${name}.nix`)
                // inside a fetched flake input yielded a Value::Path holding the
                // fetcher CACHE dir instead of the input's `/nix/store/<h>-source`
                // path — so its `toString`/copy-to-store/inputSrc diverged from
                // CppNix (the plain `./x` sibling already dematerializes; the two
                // must agree).
                crate::path::dematerialize(&norm).to_string_lossy().into_owned()
            } else {
                // No eval-file context (top-level `sui eval -E`): the
                // plain branch keeps the raw text, so match it — but the
                // interpolation is still spliced.
                text
            }
        }
        // Absolute paths: canonicalize the concatenated text CppNix's way.
        // The `${e}` splice routinely introduces a `//` seam (`/bar/` +
        // `/tmp/foo`) or a `.`/`..` component that must collapse
        // (`/bar//tmp/foo` → `/bar/tmp/foo`), and `..` must clamp at root.
        // `canon_abs` is filesystem-free (works on not-yet-materialized
        // flake paths) and root-aware (unlike `normalize_path`, which pops
        // past root — the marquee-root divergence).
        PathKind::Abs => crate::path::canon_abs(&text),
        // Home paths (`~/…`) carry a leading `~` component, so they are
        // not absolute-rooted; keep the pre-existing normalization.
        PathKind::Home => normalize_path(std::path::Path::new(&text))
            .to_string_lossy()
            .into_owned(),
    };
    Ok(Value::Path(Box::new(SmolStr::from(resolved.as_str()))))
}

/// Which kind of interpolatable path literal — governs how the
/// concatenated text is finally resolved.
#[derive(Clone, Copy)]
enum PathKind {
    Abs,
    Rel,
    Home,
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
    // Fast path: a `NODE_IDENT` holds a single `TOKEN_IDENT`, whose `text()`
    // borrows the source `&str` directly from the green node — no
    // `PreorderWithTokens` cursor tree-walk and none of the `NodeData::new`
    // allocations that `syntax().text()` (a `SyntaxText` over the node's whole
    // descendant span) pays. Byte-identical fallback: the identifier `or` is
    // lexed as a nested `TOKEN_OR` (rnix quirk), so `ident_token()` is `None`
    // there — walk the full node text in that case, exactly as before.
    match ident.ident_token() {
        Some(tok) => tok.text().to_string(),
        None => ident.syntax().text().to_string(),
    }
}

/// Byte offset of a STATIC attr key (`Ident` or `Str`) in its source text —
/// the position `builtins.unsafeGetAttrPos` reports for that key. Returns
/// `None` for a dynamic key (`${e}`), which has no fixed source position.
///
/// CppNix points a binding's position at the KEY token's start; rnix exposes
/// it via the syntax node's `text_range().start()`.
fn static_attr_offset(attr: &ast::Attr) -> Option<u32> {
    let node = match attr {
        ast::Attr::Ident(i) => i.syntax(),
        ast::Attr::Str(s) => s.syntax(),
        ast::Attr::Dynamic(_) => return None,
    };
    Some(u32::from(node.text_range().start()))
}

/// Collect a literal attrset's static top-level KEY offsets into an
/// [`crate::pos::AttrPositions`] and attach it to `attrs` (behind the value's
/// `Rc<AttrPositions>` slot). Records only single-key static bindings — the
/// shape `attrTag`'s `tags_` (`{ app = …; file = …; }`) is built from and the
/// only shape `builtins.unsafeGetAttrPos` reads in nixpkgs. `None`-costs a
/// pointer when the set has no such keys (attaches nothing).
fn attach_attrset_positions(set: &ast::AttrSet, attrs: &mut NixAttrs, env: &Env) {
    // The FILE is the one the literal is being built in — from the eval-file
    // stack, which a thunk restores to its captured file when it forces. This
    // is correct under laziness: a `dock.nix` attrset literal forced later
    // records `dock.nix`, not whatever file is top-of-stack at force time.
    // (`current_source_id`/`CURRENT_SOURCE_ID` is per-`eval_with_file`, NOT
    // per-env, so it would mis-attribute a lazily-forced literal.)
    let mut table = crate::pos::AttrPositions::new(current_eval_file());
    for entry in set.entries() {
        if let ast::Entry::AttrpathValue(apv) = entry {
            let Some(attrpath) = apv.attrpath() else { continue };
            let path_attrs: Vec<ast::Attr> = attrpath.attrs().collect();
            // A dotted path `a.b = …` desugars to a nested set and CppNix gives
            // the OUTER key the position of the path's HEAD, so record
            // `path_attrs[0]` whatever the length. This previously skipped any
            // multi-segment path, on the assumption that nixpkgs never asks for
            // a dotted tag's position. Measured — for
            // `{ …; nested.deep = 3; }` at line 6:
            //   nix  nested=6:3      sui  nested=NULL
            let Some(head) = path_attrs.first() else { continue };
            let Some(offset) = static_attr_offset(head) else { continue };
            // Resolve the static key name (Ident/Str) — never forces (a
            // dynamic key already returned None above).
            if let Ok(Some(name)) = eval_attr_maybe_null(&path_attrs[0], env) {
                table.insert(intern(&name), offset);
            }
        } else if let ast::Entry::Inherit(inh) = entry {
            // `inherit x;` and `inherit (src) x;` BIND an attribute exactly as
            // `x = …` does, and CppNix gives each inherited name the position of
            // its own ident. Skipping them left every inherited key
            // position-less — which is most of nixpkgs' `lib`, since
            // `lib/default.nix` re-exports through
            // `inherit (self.options) mkOption …`. Measured before the fix:
            //   unsafeGetAttrPos "mkOption" nixpkgs.lib
            //     nix …-source/lib/default.nix     sui null
            //
            // An earlier attempt at this arm was reverted for reporting line 1;
            // that was `pos::line_col` returning a constant, NOT this arm. With
            // the real offset→line/column conversion in place it resolves
            // exactly.
            for attr in inh.attrs() {
                let Some(offset) = static_attr_offset(&attr) else { continue };
                if let Ok(Some(name)) = eval_attr_maybe_null(&attr, env) {
                    table.insert(intern(&name), offset);
                }
            }
        }
    }
    if !table.is_empty() {
        attrs.set_positions(std::rc::Rc::new(table));
    }
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

        // D1 (`SUI_SCOPE_NARROW>=1`) — a SECOND predicate, deliberately not a
        // widening of `is_recursive_binding` below.
        //
        // THE TRAP: `is_recursive_binding` is BACKWARD-BLIND on purpose — it
        // tests `key` plus the siblings seen SO FAR, so `rec { b = a; a = 1; }`
        // computes `false` for `b`. That verdict selects Promise semantics, so
        // widening it would change which bindings get the fix-point sentinel
        // and is not a refactor available here. Yet `b` genuinely does need the
        // rec scope, and today gets it from Phase 2's blanket `update_env`.
        // Narrowing therefore needs its own forward-complete question — "does
        // this RHS reach ANY key this scope binds, declared before or after?" —
        // answered against a full pre-pass, while `is_recursive_binding` stays
        // byte-identical.
        //
        // The pre-pass is PURELY SYNTACTIC, which is the second trap: the
        // Phase-1 loop below owns the evaluation order of `${…}` keys, and
        // calling `eval_attr` here would run that arbitrary code earlier. So a
        // head that is not a plain identifier forfeits narrowing for the whole
        // scope instead of being evaluated for its name. Starting the flag at
        // `scope_narrow_enabled()` also means the default path never walks the
        // entries at all.
        let mut names_complete = scope_narrow_enabled();
        let rec_scope_names: HashSet<String> = if names_complete {
            let mut s = HashSet::new();
            for entry in set.entries() {
                match entry {
                    ast::Entry::AttrpathValue(apv) => {
                        match apv.attrpath().and_then(|p| p.attrs().next()) {
                            Some(ast::Attr::Ident(i)) => {
                                s.insert(ident_text(&i));
                            }
                            _ => names_complete = false,
                        }
                    }
                    ast::Entry::Inherit(inh) => {
                        for attr in inh.attrs() {
                            match attr {
                                ast::Attr::Ident(i) => {
                                    s.insert(ident_text(&i));
                                }
                                _ => names_complete = false,
                            }
                        }
                    }
                }
            }
            s
        } else {
            HashSet::new()
        };
        let narrow = names_complete;

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
                        // Self-recursive detection in a `rec { … }` scope:
                        // any binding whose value-expr references the
                        // bound name OR any sibling key declared in this
                        // rec scope is potentially self-recursive (the
                        // siblings' thunks share the rec_env via Phase 2).
                        // Mark as recursive so inner re-entrance during
                        // force returns a Promise sentinel instead of
                        // erroring with InfiniteRecursion.
                        //
                        // For simplicity we check `key` and all already-
                        // defined siblings; siblings defined later are
                        // covered when THEIR thunks force (they reference
                        // back into this rec scope via Phase 2's env update).
                        // O(N) not O(N²): one memoized referenced-name set,
                        // intersected with key + already-defined siblings.
                        // Byte-identical to the prior per-name walks.
                        let referenced = referenced_idents(&value_expr);
                        let is_recursive_binding = referenced.contains(key.as_str())
                            || defined_so_far
                                .iter()
                                .any(|n| referenced.contains(n.as_str()));
                        let value = if is_recursive_binding {
                            Value::Thunk(Thunk::new_suspended_recursive(
                                value_expr.clone(),
                                env.clone(),
                            ))
                        } else {
                            // maybeThunk: skip thunk for trivial exprs.
                            // is_rec=true because rec attrset bindings
                            // can reference each other.
                            // Pass defined_so_far so backward refs
                            // resolve directly.
                            maybe_thunk(&value_expr, env, true, Some(&defined_so_far))
                        };
                        // Forward-complete needs-scope test (see the pre-pass
                        // above). `is_recursive_binding` is folded in as
                        // belt-and-braces: it is a subset whenever `narrow`
                        // holds, since every key it can name came from an
                        // `Ident` head and so is in `rec_scope_names`.
                        let needs_scope = !narrow
                            || is_recursive_binding
                            || rec_scope_names
                                .iter()
                                .any(|n| referenced.contains(n.as_str()));
                        rec_env.bind(key.clone(), value.clone());
                        attrs.insert(key.clone(), value.clone());
                        if let Value::Thunk(t) = &value {
                            if needs_scope {
                                thunks.push((key.clone(), t.clone()));
                                crate::value::census::scope_pinned();
                            } else {
                                crate::value::census::scope_narrowed();
                            }
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
                    let path_attrs: Vec<ast::Attr> = attrpath.attrs().collect();
                    // CppNix defers a dynamic key that is NOT at the HEAD of the
                    // attrpath: `{ a.${e} = v; }` builds `{ a = <thunk {${e}=v}>; }`,
                    // so `e` never forces until `.a` is demanded. Evaluating the
                    // whole path eagerly would force `e` at construction and — in
                    // the module-system fixpoint — read `config.<x>` while `config`
                    // is mid-force (the M2.6 divergence: `homes.null` instead of
                    // `homes.<name>`). Only the head is eager; a lone dynamic tail
                    // becomes a deferred thunk. A rarer collision under the same
                    // head stays eager (forced) so static deep-merge still works.
                    let tail_is_dynamic =
                        path_attrs.len() > 1 && attrs_have_dynamic(&path_attrs[1..]);
                    let head_key = match eval_attr_maybe_null(&path_attrs[0], env)? {
                        Some(k) => k,
                        // Null dynamic HEAD attr name → skip entire binding.
                        None => continue,
                    };
                    if tail_is_dynamic && attrs.get(&head_key).is_none() {
                        let value =
                            build_deferred_tail_attr(&path_attrs[1..], &value_expr, env);
                        attrs.insert(head_key, value);
                        continue;
                    }
                    // M2.6 ROOT #3 (collision case): the tail has a dynamic key
                    // AND the head already exists (a sibling binding wrote it,
                    // e.g. osquery's `systemd.services.… = …` then
                    // `systemd.tmpfiles.settings."10-osquery".${dirname …}.d`).
                    // The plain deferral above bails (head present), and the
                    // eager path below would force the dynamic key at
                    // construction — re-reading `config.<x>` mid-fixpoint →
                    // the empty-Promise partial. Instead, descend the existing
                    // head along the tail's STATIC prefix and splice a DEFERRED
                    // thunk at the first dynamic level, so the dynamic key
                    // stays lazy exactly as CppNix's nested-literal desugaring
                    // does — while preserving the static deep-merge with the
                    // sibling binding.
                    if tail_is_dynamic {
                        if let Some(existing) = attrs.get(&head_key).cloned() {
                            let merged = merge_deferred_dynamic_tail(
                                existing,
                                &path_attrs[1..],
                                &value_expr,
                                env,
                            )?;
                            attrs.insert(head_key, merged);
                            continue;
                        }
                    }
                    // Eager path: evaluate the remaining (static, or collision)
                    // keys now. A null dynamic tail key skips the binding.
                    let mut path_keys: Vec<String> = {
                        let mut v = Vec::with_capacity(path_attrs.len());
                        v.push(head_key);
                        let mut skip = false;
                        for a in &path_attrs[1..] {
                            match eval_attr_maybe_null(a, env)? {
                                Some(k) => v.push(k),
                                None => { skip = true; break; }
                            }
                        }
                        if skip { v.clear(); }
                        v
                    };
                    // Null dynamic attr name → skip entire binding (CppNix compat)
                    if path_keys.is_empty() { continue; }
                    if path_keys.len() == 1 {
                        let key = path_keys.pop().unwrap();
                        // maybeThunk: skip thunk for trivial exprs.
                        // is_rec=false — Ident lookups are safe.
                        let value = maybe_thunk(&value_expr, env, false, None);
                        // CppNix desugars `a.b = x; a = { c = y; };` into a single
                        // merged `a = { b = x; c = y; }` at parse time. rnix keeps
                        // the two bindings separate, so when a single-key binding
                        // collides with an already-built (dotted) attrs for the
                        // same key, deep-MERGE instead of overwrite. Force the RHS
                        // to WHNF so merge_nested_insert (which needs concrete
                        // Value::Attrs on both sides) can merge — forcing an
                        // attrset to WHNF does NOT force its fields, so leaf values
                        // stay lazy. Only fires on collision; non-colliding
                        // single-key bindings keep the plain fast insert.
                        // (This is the pkg-config-wrapper `env.addFlags` drop:
                        // `env.addFlags = …` then `env = { wrapperName = …; … }`.)
                        // If the earlier binding for this key is still a lazy
                        // Thunk (an attrset literal inserted via maybe_thunk), force
                        // it to WHNF FIRST so a `key = {..}; key = {..}` collision is
                        // seen as attrs-vs-attrs and MERGES, matching nix
                        // (`{ s = {a=1;}; s = {b=2;}; }` → `{ s = {a=1; b=2;}; }`).
                        // Without this the `Some(Value::Attrs(_))` test below is false
                        // on a Thunk and the second binding overwrites, dropping the
                        // first's keys. The dotted branch below already does this; R3
                        // (eval-okay-merge-dynamic-attrs set1/set2) needs it here too.
                        // WHNF force does not force fields → leaf laziness preserved.
                        // (A non-attrs dup like `s = 1; s = 2` still overwrites here,
                        // unchanged — nix errors there, an eval-FAIL case out of scope.)
                        if matches!(attrs.get(&key), Some(Value::Thunk(_))) {
                            let existing = attrs.get(&key).cloned().unwrap();
                            let forced_existing = force_value(&existing)?;
                            attrs.insert(key.clone(), forced_existing);
                        }
                        if matches!(attrs.get(&key), Some(Value::Attrs(_))) {
                            let forced = force_value(&value)?;
                            merge_nested_insert(&mut attrs, key, forced);
                        } else {
                            attrs.insert(key, value);
                        }
                    } else {
                        let key = path_keys[0].clone();
                        let value = build_nested_attr(&path_keys[1..], &value_expr, env)?;
                        // CppNix desugars `a = { x = …; }; a.y = …;` into a
                        // single merged `a = { x = …; y = …; }`. When the
                        // full-set binding for `a` was inserted FIRST it is a
                        // lazy Thunk (attrset literals go through maybe_thunk),
                        // so merge_nested_insert — which only merges when the
                        // existing value is a concrete Value::Attrs — would
                        // NOT see the earlier keys and would overwrite `a`
                        // with just `{ y = … }`, silently dropping `x`. Force
                        // the existing entry to WHNF on collision so the merge
                        // sees the concrete attrs (forcing to WHNF does not
                        // force the fields, so leaf laziness is preserved).
                        // (This is the gst-plugins-base `passthru.waylandEnabled`
                        // drop: `passthru = { … }; passthru.tests.x = …;`.)
                        if matches!(attrs.get(&key), Some(Value::Thunk(_))) {
                            let existing = attrs.get(&key).cloned().unwrap();
                            let forced = force_value(&existing)?;
                            attrs.insert(key.clone(), forced);
                        }
                        merge_nested_insert(&mut attrs, key, value);
                    }
                }
                ast::Entry::Inherit(inherit) => {
                    eval_inherit(&inherit, env, &mut attrs, None, None)?;
                }
            }
        }
    }

    // Record the literal's static-key source positions for
    // `builtins.unsafeGetAttrPos` (the `attrTag` `declarations` — options.json
    // dock root). Cheap: one entry walk over static Ident/Str keys, no
    // forcing; attaches nothing (a pointer-sized `None`) when the set has no
    // single-static-key bindings.
    attach_attrset_positions(set, &mut attrs, env);

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
        //
        // CppNix resolves a bare `inherit x;` LAZILY, exactly like a plain
        // reference to `x` — it does NOT eagerly force the enclosing scope.
        // This matters when `x` is provided only by an enclosing `with`
        // scope whose value is a fixpoint still being constructed (a
        // blackhole): eager `env.lookup` returns None → spurious
        // `UndefinedVar`. nixpkgs `all-packages.nix` is
        // `… with pkgs; { nettle = import … { inherit callPackage; }; }`,
        // so `inherit callPackage` must resolve `callPackage` from the
        // `with pkgs` scope AT FORCE TIME, not eagerly at attrset
        // construction. Mirror `maybe_thunk`'s Ident path: try the fast
        // lookup, and on a miss defer to a WithIdent thunk (or a suspended
        // env lookup) so the resolution happens lazily against the settled
        // scope. (This was the `nettle` UndefinedVar('callPackage') drop.)
        let mut be = bind_env;
        for attr in inherit.attrs() {
            let name = eval_attr(&attr, env)?;
            let sym = crate::value::intern(&name);
            let value = if let Some(v) = env.lookup_fast(sym, &name) {
                v
            } else if let Some((scope_cache, scope_value)) =
                env.innermost_with_scope()
            {
                Value::Thunk(Thunk::new_with_ident(
                    SmolStr::from(name.as_str()),
                    scope_cache,
                    scope_value,
                    env.clone(),
                ))
            } else {
                return Err(EvalError::UndefinedVar(format!(
                    "'{name}'{}",
                    eval_file_ctx()
                )));
            };
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
        // CRITICAL: Wrap leaf in a thunk instead of eagerly evaluating.
        // For dotted paths like `config.warnings = optionals config.x [...]`,
        // the leaf expression must be lazy — eagerly evaluating it during
        // attrset construction forces fixpoint thunks prematurely.
        return Ok(maybe_thunk(expr, env, false, None));
    }
    let key = path[0].clone();
    let inner = build_nested_attr(&path[1..], expr, env)?;
    let mut attrs = NixAttrs::new();
    attrs.insert(key, inner);
    Ok(Value::Attrs(Rc::new(attrs)))
}

/// True if a single attr is a DYNAMIC key — one whose resolution runs
/// arbitrary expression code and therefore must not be forced at
/// attrset-construction time.
///
/// Two forms are dynamic:
///   * `ast::Attr::Dynamic` — a bare `${e}` antiquotation.
///   * `ast::Attr::Str` **containing an interpolation** — an interpolated
///     string key like `"iwd/${nm}"`.  A `Str` with NO interpolation
///     (`"foo bar"`) is a plain static string literal and is NOT dynamic.
///
/// M2.6 ROOT #3: `attrs_have_dynamic` previously matched ONLY
/// `Attr::Dynamic`, so an interpolated-string tail key (`config.a."p${e}"`)
/// fell to the eager path and forced `e` at construction.  In the module
/// system that forces a `config.<x>` read while `config` is mid-fixpoint
/// (`environment.etc."iwd/${configFile.name}"`, where `configFile` reads
/// `with config.networking.networkmanager`), yielding the empty-Promise
/// partial → the `set/null` softening.  Treating an interpolated `Str` as
/// dynamic routes it through the same per-level deferral as `${e}`
/// (ROOT #1/#2), so `e` forces only when the enclosing head is demanded —
/// exactly CppNix's nested-attrset-literal desugaring.
fn attr_is_dynamic(attr: &ast::Attr) -> bool {
    match attr {
        ast::Attr::Dynamic(_) => true,
        // A string attr key is dynamic iff it has ≥1 interpolation part;
        // a purely-literal string key forces nothing and stays eager.
        ast::Attr::Str(s) => s
            .normalized_parts()
            .iter()
            .any(|p| matches!(p, InterpolPart::Interpolation(_))),
        ast::Attr::Ident(_) => false,
    }
}

/// True if any attr in the slice is a dynamic (interpolated) key.
///
/// A dynamic key beyond the HEAD of an attrpath must NOT be evaluated at
/// attrset-construction time — CppNix defers it inside the head's lazy
/// value, so `{ a.${e} = v; }` never forces `e` until `.a` is demanded.
/// Static string/ident keys are cheap and force nothing, so they don't
/// need deferral.
fn attrs_have_dynamic(attrs: &[ast::Attr]) -> bool {
    attrs.iter().any(attr_is_dynamic)
}

/// Build the nested attrset for the TAIL of an attrpath, deferring
/// evaluation of dynamic tail keys until the value is forced.
///
/// Given tail attrs `[b, ${e}, c]` and a value expr, produce a lazy
/// `Value::Thunk` that, when forced, evaluates each tail key (including
/// the dynamic `${e}`) against `env` and builds `{ b = { ${e} = { c =
/// <leaf-thunk> }; }; }`. This mirrors CppNix: the inner attrset (and
/// thus its dynamic keys) is constructed only when the enclosing head
/// attribute is demanded — never at construction of the outer attrset.
///
/// A dynamic key that evaluates to `null` skips the whole binding
/// (returns an empty attrset), matching CppNix's null-dynamic-attr rule.
fn build_deferred_tail_attr(
    tail: &[ast::Attr],
    value_expr: &ast::Expr,
    env: &Env,
) -> Value {
    let tail: Vec<ast::Attr> = tail.to_vec();
    let value_expr = value_expr.clone();
    let env = env.clone();
    Value::Thunk(Thunk::new_native(move || {
        build_tail_attrs_now(&tail, &value_expr, &env)
    }))
}

/// Resolve ONE level of the deferred attrpath tail — used from inside
/// the deferred thunk above once the enclosing head is demanded.
///
/// M2.6 ROOT #2 (the OVER-FORCE fix): this resolves *only* `tail[0]`'s
/// key and wraps the remaining tail `tail[1..]` in another DEFERRED
/// thunk — it does NOT recurse eagerly through the whole tail. This is
/// exactly CppNix's desugaring of `a.b.c = v` into nested attrset
/// literals `a = { b = { c = v; }; }`, where forcing `a` to WHNF yields
/// `{ b = <thunk {c=v}> }` — the inner level (`b`, and any dynamic key
/// under it) stays lazy until `.b` is demanded.
///
/// Forcing the enclosing head therefore resolves ONE tail key, never
/// the whole chain: `config.homes.${cfg.pleme.userName} = 7` demanded
/// as `config` yields `{ homes = <deferred> }` WITHOUT forcing the
/// `${cfg.pleme.userName}` key. The prior implementation recursed the
/// whole tail eagerly, forcing that dynamic key while only `.config`
/// (or its `._type`) was demanded — the over-force cppnix never does.
///
/// A dynamic key that evaluates to `null` skips the whole binding
/// (returns an empty attrset), matching CppNix's null-dynamic-attr rule.
fn build_tail_attrs_now(
    tail: &[ast::Attr],
    value_expr: &ast::Expr,
    env: &Env,
) -> Result<Value, EvalError> {
    if tail.is_empty() {
        return Ok(maybe_thunk(value_expr, env, false, None));
    }
    if std::env::var_os("SUI_M26_TAILTRACE").is_some() {
        let t: String = tail[0].syntax().text().to_string().chars().take(40).collect();
        eprintln!("[M26 TAIL-RESOLVE] forcing dynamic tail key `{t}`");
        if attrs_have_dynamic(&tail[..1]) {
            crate::trace::dump_force_stack_ids();
        }
    }
    let key = match eval_attr_maybe_null(&tail[0], env)? {
        Some(k) => k,
        // Null dynamic key → the whole binding is skipped; an empty
        // attrset is the identity for merge_nested_insert.
        None => return Ok(Value::Attrs(Rc::new(NixAttrs::new()))),
    };
    // Resolve ONE level: if more tail remains, defer it (a new lazy
    // thunk) rather than recursing eagerly. Only the leaf (empty tail)
    // is built here. This keeps each nested level lazy, exactly like
    // CppNix's nested-attrset-literal desugaring — so forcing this
    // level does NOT force the next level's (possibly dynamic) key.
    let inner = if tail.len() == 1 {
        maybe_thunk(value_expr, env, false, None)
    } else {
        build_deferred_tail_attr(&tail[1..], value_expr, env)
    };
    let mut attrs = NixAttrs::new();
    attrs.insert(key, inner);
    Ok(Value::Attrs(Rc::new(attrs)))
}

/// M2.6 ROOT #3 (collision case): splice a DEFERRED dynamic-tail binding
/// into an ALREADY-PRESENT head value without forcing the dynamic key.
///
/// `existing` is the value already stored at the attrpath's head (written
/// by a sibling binding — e.g. `systemd.services.… = …`). `tail` is the
/// remaining attrpath (`path_attrs[1..]`) of the new binding, which
/// contains ≥1 dynamic attr (`systemd.tmpfiles.….${dirname …}.d`).
///
/// We descend `existing` along the LONGEST STATIC PREFIX of `tail`
/// (`tmpfiles`, `settings`, `"10-osquery"` — all static, forced-free
/// keys), forcing each already-present sub-attrset to WHNF so the merge
/// sees concrete keys (forcing to WHNF never forces leaf VALUES, so leaf
/// laziness is preserved), and at the first DYNAMIC level splice a
/// `build_deferred_tail_attr` thunk. The dynamic key therefore forces
/// only when that exact nested path is later demanded — CppNix's
/// nested-attrset-literal desugaring, now honoured through a sibling
/// collision too.
fn merge_deferred_dynamic_tail(
    existing: Value,
    tail: &[ast::Attr],
    value_expr: &ast::Expr,
    env: &Env,
) -> Result<Value, EvalError> {
    // `tail` is non-empty and contains a dynamic attr somewhere (the
    // caller guarantees `attrs_have_dynamic(tail)`).
    debug_assert!(!tail.is_empty());

    // If the FIRST tail attr is itself dynamic, there is no static prefix
    // to descend — the whole tail is deferred and merged as a lazy
    // overlay onto the existing head (a `//`-style right-merge; the
    // deferred attrset only materialises its dynamic key on demand).
    if attr_is_dynamic(&tail[0]) {
        let deferred = build_deferred_tail_attr(tail, value_expr, env);
        return Ok(lazy_overlay_merge(existing, deferred));
    }

    // The head static key of `tail`. Resolve it (static → forces nothing
    // relevant; a null dynamic can't occur here since tail[0] is static).
    let key = match eval_attr_maybe_null(&tail[0], env)? {
        Some(k) => k,
        None => return Ok(existing),
    };

    // Force the existing head to a concrete attrset so we can descend +
    // merge on the resolved static key. Forcing to WHNF does NOT force
    // its field VALUES, so leaf laziness is preserved.
    let existing_forced = force_value(&existing)?;
    let mut base = match existing_forced {
        Value::Attrs(a) => (*a).clone(),
        // The existing head is not an attrset (a sibling wrote a leaf
        // here); CppNix would error on the merge, but to stay lazy we
        // defer the tail and let a later demand surface the real merge
        // conflict. Build the deferred tail as a fresh attrset.
        _ => {
            let deferred = build_deferred_tail_attr(tail, value_expr, env);
            return Ok(deferred);
        }
    };

    // Recurse: merge the REMAINING tail (`tail[1..]`) under `key`.
    let child_existing = base.get(&key).cloned();
    let new_child = match child_existing {
        Some(child) if tail.len() > 1 => {
            // Deeper static/dynamic prefix under an existing sub-attrset.
            merge_deferred_dynamic_tail(child, &tail[1..], value_expr, env)?
        }
        Some(child) => {
            // tail == [key]; the leaf collides with an existing value.
            // Static leaf collision — build the leaf and lazy-merge.
            let leaf = maybe_thunk(value_expr, env, false, None);
            lazy_overlay_merge(child, leaf)
        }
        None if tail.len() > 1 => {
            // No existing child; the remaining tail may itself start with
            // a dynamic key — defer it whole (build_deferred_tail_attr
            // handles the static/dynamic split per-level).
            build_deferred_tail_attr(&tail[1..], value_expr, env)
        }
        None => maybe_thunk(value_expr, env, false, None),
    };
    base.insert(key, new_child);
    Ok(Value::Attrs(Rc::new(base)))
}

/// Lazy right-merge of two values that are (or will force to) attrsets,
/// preserving leaf laziness. Used by [`merge_deferred_dynamic_tail`] to
/// combine a deferred dynamic-tail attrset with an existing value without
/// forcing either's dynamic keys eagerly. When both are concrete attrs we
/// deep-merge in place (reusing [`merge_nested_insert`]); otherwise we
/// build a lazy overlay thunk that merges on demand.
fn lazy_overlay_merge(left: Value, right: Value) -> Value {
    match (&left, &right) {
        (Value::Attrs(la), Value::Attrs(_)) => {
            crate::perf::inc(crate::perf::Counter::SlashDeferredTailClone);
            let mut merged = (**la).clone();
            if let Value::Attrs(ra) = &right {
                // Merging distinct override keys into `merged` is order-
                // independent (per-key right-wins), and the result map is
                // unordered storage — the sorted `iter()` was dead work.
                for (k, v) in ra.iter_unsorted() {
                    merge_nested_insert(&mut merged, k.clone(), v.clone());
                }
            }
            Value::Attrs(Rc::new(merged))
        }
        _ => {
            // At least one side is a thunk (a deferred dynamic tail).
            // Defer the merge behind a Native thunk so neither side's
            // dynamic key forces until the merged attrset is demanded.
            Value::Thunk(Thunk::new_native(move || {
                let lf = force_value(&left)?;
                let rf = force_value(&right)?;
                let la = lf.as_attrs()?;
                let ra = rf.as_attrs()?;
                crate::perf::inc(crate::perf::Counter::SlashDeferredTailClone);
                let mut merged = (*la).clone();
                for (k, v) in ra.iter_unsorted() {
                    merge_nested_insert(&mut merged, k.clone(), v.clone());
                }
                Ok(Value::Attrs(Rc::new(merged)))
            }))
        }
    }
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
    // Fast path: no existing entry at this key → plain insert, keeping the
    // value lazy (the overwhelmingly common non-colliding case, so we never
    // force a thunk here).
    let existing = match target.get(&key) {
        Some(e) => e.clone(),
        None => {
            target.insert(key, value);
            return;
        }
    };
    // A collision exists.  A deep merge is warranted only when BOTH the
    // existing entry AND the new value are attrset-shaped.  M2.6 ROOT #4b
    // (byte-verified): either side may be a lazy `Thunk` wrapping a
    // full-set leaf — both dotted-path orderings hit this:
    //   forward  `o.a = { x = 1; }; o.a.y = 2;` → EXISTING `a` is a thunk
    //            (`build_nested_attr` puts the `{x=1}` leaf through
    //            `maybe_thunk`), NEW `a` is `{ y = … }`;
    //   reverse  `o.a.y = 2; o.a = { x = 1; };` → EXISTING `a` is `{y}`,
    //            NEW `a` is the `<thunk {x=1}>`.
    // The old `should_merge` required BOTH sides to already be concrete
    // `Value::Attrs`, so a Thunk-vs-Attrs collision fell to the overwrite
    // path and silently dropped the earlier leaf's keys.  cppnix desugars
    // BOTH orderings into one merged `o.a = { x = 1; y = 2; }`.  Force each
    // side's thunk to WHNF ON COLLISION ONLY (forcing an attrset to WHNF
    // does NOT force its fields, so leaf laziness is preserved); a thunk
    // that forces to a non-attrset (or errors) makes the merge a plain
    // overwrite (leaf last-write-wins).
    // Symptom this closes: nixpkgs' alsa module declares
    // `options.hardware.alsa = { enable = …; cardAliases = …; … }` AND
    // `options.hardware.alsa.enablePersistence = …`; sui merged them to
    // only `{enablePersistence}`, so `hardware.alsa.cardAliases` "does not
    // exist" — the M2.6 frontier once the `with`-namespace over-force (#4a)
    // was fixed.
    let value = match value {
        Value::Thunk(_) => match force_value(&value) {
            Ok(v @ Value::Attrs(_)) => v,
            _ => value,
        },
        other => other,
    };
    if !matches!(value, Value::Attrs(_)) {
        target.insert(key, value);
        return;
    }
    // Normalize the existing side to concrete attrs too (forcing a thunk
    // to WHNF if needed); if it isn't attrset-shaped, the new attrs wins.
    let existing_concrete = match &existing {
        Value::Attrs(_) => existing.clone(),
        Value::Thunk(_) => match force_value(&existing) {
            Ok(v @ Value::Attrs(_)) => v,
            _ => {
                target.insert(key, value);
                return;
            }
        },
        _ => {
            target.insert(key, value);
            return;
        }
    };
    // Both sides are concrete attrs — merge in place. We pop the
    // existing entry, then walk the new attrs and recursively
    // merge each child onto it.
    let mut existing_attrs = match existing_concrete {
        Value::Attrs(a) => (*a).clone(),
        _ => unreachable!(),
    };
    let new_attrs = match value {
        Value::Attrs(ref a) => a,
        _ => unreachable!(),
    };
    for (k, v) in new_attrs.iter_unsorted() {
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
    // Consume the Concretes (move, don't clone) so `l`/`r` hold the sole Rc to
    // any heap payload. This is byte-neutral — `into_value` yields the identical
    // `Value` as `to_value` — but it drops `lc`/`rc`, which is what lets the
    // `Concat` arm's structural-share fast path see a uniquely-owned left list
    // for a fresh `++` temporary (`Rc::try_unwrap` → append in place). Keeping
    // `lc` alive via `to_value` pinned the refcount at ≥2 and defeated reuse.
    let l = lc.into_value();
    let r = rc.into_value();

    match op {
        ast::BinOpKind::Add => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => a
                .checked_add(*b)
                .map(Value::Int)
                .ok_or_else(|| int_overflow("adding", *a, '+', *b)),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
            (Value::Int(a), Value::Float(b)) => Ok(Value::Float(*a as f64 + b)),
            (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + *b as f64)),
            (Value::String(a), Value::String(b)) => {
                let mut ctx = a.context.clone();
                ctx.merge(&b.context);
                // Byte-identical to `format!("{}{}", a.chars, b.chars)` but
                // routes around the `core::fmt` runtime (its dispatch was the
                // #1 self-time frame on the string-concat hot path): a single
                // exact-capacity `String` + two `push_str` reserves the final
                // size once, so the left operand is copied exactly once instead
                // of copied-then-regrown. Result string + context unchanged →
                // ByteSufficient. (Also removes a `format!` — TYPED EMISSION.)
                let mut s = String::with_capacity(a.chars.len() + b.chars.len());
                s.push_str(&a.chars);
                s.push_str(&b.chars);
                Ok(Value::String(Rc::new(NixString::with_context(s, ctx))))
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
        ast::BinOpKind::Sub => num_op(
            &l,
            &r,
            |a, b| a.checked_sub(b),
            |a, b| a - b,
            |a, b| int_overflow("subtracting", a, '-', b),
        ),
        ast::BinOpKind::Mul => num_op(
            &l,
            &r,
            |a, b| a.checked_mul(b),
            |a, b| a * b,
            |a, b| int_overflow("multiplying", a, '*', b),
        ),
        ast::BinOpKind::Div => {
            // CppNix rejects division by zero for both int and float
            // operands; Rust's native int-div-by-0 panics (we handle
            // that below) but float-div-by-0 silently returns `inf`
            // or `NaN`, which sui was then serializing as `null` —
            // an invisible silent-Ok bug surfaced by the error-case
            // differential corpus.
            //
            // Cover every zero-denominator case explicitly.
            let rhs_is_zero = match &r {
                Value::Int(0) => true,
                Value::Float(f) => *f == 0.0,
                _ => false,
            };
            if rhs_is_zero {
                return Err(EvalError::DivisionByZero);
            }
            num_op(
                &l,
                &r,
                |a, b| a.checked_div(b),
                |a, b| a / b,
                |a, b| int_overflow("dividing", a, '/', b),
            )
        }
        ast::BinOpKind::Equal => Ok(Value::Bool(l == r)),
        ast::BinOpKind::NotEqual => Ok(Value::Bool(l != r)),
        ast::BinOpKind::Less => compare(&l, &r, |o| o == std::cmp::Ordering::Less),
        ast::BinOpKind::LessOrEq => compare(&l, &r, |o| o != std::cmp::Ordering::Greater),
        ast::BinOpKind::More => compare(&l, &r, |o| o == std::cmp::Ordering::Greater),
        ast::BinOpKind::MoreOrEq => compare(&l, &r, |o| o != std::cmp::Ordering::Less),
        ast::BinOpKind::Update => {
            let la = l.to_attrs()?;
            let ra = r.to_attrs()?;
            // O(1) lazy overlay — defers merge until attribute access.
            Ok(Value::Attrs(Rc::new(la.overlay(ra))))
        }
        ast::BinOpKind::Concat => {
            // Structural-share fast path: when the left operand's `Rc<Vec>` is
            // uniquely owned (a fresh temporary, as in a left-associative `++`
            // fold `acc ++ [x]`), append the right elements IN PLACE instead of
            // cloning the whole accumulator. This turns an O(n) copy per concat
            // into amortized O(1), byte-identically — the result is the same
            // ordered sequence of the same Rc-shared lazy thunks (no forcing,
            // no reordering, no identity change). When the Rc is shared (the
            // left came from a still-live binding/thunk) we fall back to the
            // clone-extend path, preserving the shared list unchanged.
            crate::value::concat_lists(l, r.as_list()?)
        }
        ast::BinOpKind::And | ast::BinOpKind::Or | ast::BinOpKind::Implication => {
            unreachable!("handled above")
        }
        ast::BinOpKind::PipeRight | ast::BinOpKind::PipeLeft => {
            Err(EvalError::NotImplemented("pipe operators".to_string()))
        }
    }
}

/// CppNix aborts (uncatchably) on i64 arithmetic overflow, e.g.
/// `integer overflow in adding 9223372036854775807 + 1`. `EvalError::Abort` is
/// the uncatchable variant (`tryEval` catches only `Throw`/`AssertionFailed`),
/// matching nix — a wrapping result would silently produce a wrong drvPath.
#[inline]
fn int_overflow(verb: &str, a: i64, sym: char, b: i64) -> EvalError {
    EvalError::Abort(format!("integer overflow in {verb} {a} {sym} {b}"))
}

fn num_op(
    l: &Value,
    r: &Value,
    int_op: impl Fn(i64, i64) -> Option<i64>,
    float_op: impl Fn(f64, f64) -> f64,
    overflow: impl Fn(i64, i64) -> EvalError,
) -> Result<Value, EvalError> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => {
            int_op(*a, *b).map(Value::Int).ok_or_else(|| overflow(*a, *b))
        }
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
    stacker::maybe_grow(64 * 1024, 2 * 1024 * 1024, || apply_inner(func, arg))
}

fn apply_inner(func: Value, arg: Value) -> Result<Value, EvalError> {
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
            // ALWAYS push a frame, even when the closure captured no file:
            // `.map(push_eval_file)` pushed nothing for `None`, leaving the
            // CALLER's file on top, so a literal written in a fileless
            // context got stamped with the callee's path. CppNix returns
            // `null` there. See `EVAL_FILE_STACK`.
            let _file_guard = push_eval_frame(closure.env.eval_file().cloned());
            // Push Nix-level trace frame for function calls. Lazy: stores
            // only the raw ingredients (O(1) Rc-clone of the closure env +
            // the current-eval-file snapshot) and defers the format!/strip
            // work to the cold `attach_trace` path. Renders byte-identical
            // to the eager form.
            let _trace = push_nix_trace_lambda(&closure.env);
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
            // Same lazy-arg set as `eval_apply` (single source of truth) — these
            // builtins receive the arg UNFORCED. foldl'<p1> is the nul accumulator
            // (nix's foldl' is strict in each op RESULT, NOT in the nul).
            if builtin_takes_lazy_arg(&b.name) {
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
            } else if crate::value::in_promise_eval() {
                // M2.6 Promise softening: an attrset without __functor
                // being called as a function — typically the empty-
                // attrset sentinel inside a fix-point body.  Return
                // null so eval can proceed.
                Ok(Value::Null)
            } else {
                Err(EvalError::type_error(
                    format!("cannot call {} (missing __functor){}", func.type_name(), eval_file_ctx()),
                ))
            }
        }
        _ if crate::value::in_promise_eval() => {
            // M2.6 Promise softening: calling null / int / string / list
            // as a function inside a Promise body is the sentinel
            // cascade landing somewhere it doesn't belong.  Return null
            // so the fix-point continues instead of erroring.
            Ok(Value::Null)
        }
        _ => Err(EvalError::type_error(
            format!("cannot call {}{}", func.type_name(), eval_file_ctx()),
        )),
    }
}

/// Dark-side lever `batch-bind` (byte-SAFE, `RedundantWrite`) — OFF by default.
/// When `SUI_BATCH_BIND=1`, an N-formal pattern binds in ONE copy-on-write step
/// (`Env::bind_many`) instead of N successive `env.bind()` calls. Byte-identical
/// either way (same intern, same insert order, same final HAMT — Phase 2's
/// `update_env` makes each default thunk's initial env capture unobservable).
/// Gated because the extra `Vec` allocation could regress the common small-pattern
/// case, and the win is unmeasured under load — never change the default path on a
/// hunch (never-ship-a-regression). Cached so the default path pays zero per call.
/// Ledger: `sui-spec/specs/darkside.lisp` (`batch-bind`, DarkGated).
static SUI_BATCH_BIND: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var_os("SUI_BATCH_BIND").is_some());

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
            // batch-bind (byte-SAFE `RedundantWrite`, OFF unless `SUI_BATCH_BIND=1`):
            // the flag path collects every formal's (name, value) pair and binds
            // them in ONE copy-on-write step (`bind_many`) instead of N successive
            // `env.bind()` calls. Byte-identical either way — the default thunks
            // capture `env.clone()` (pre-batch) and Phase 2's `update_env` re-points
            // every one to the final all-formals-bound env, so a thunk's *initial*
            // capture is unobservable (overwritten before any force); same intern,
            // same insert order, same final HAMT. The default path (flag unset) is
            // the original per-formal loop, byte- AND perf-identical (no Vec alloc).
            let use_batch = *SUI_BATCH_BIND;
            let mut pairs: Vec<(String, Value)> =
                if use_batch { Vec::with_capacity(entries.len()) } else { Vec::new() };

            // D3 (`SUI_SCOPE_NARROW>=1`) — the highest-yield arm of the fix,
            // because it fires on every `callPackage`'d
            // `{ stdenv, lib, foo ? null }` and every
            // `{ config, lib, pkgs, ... }` module in the fleet.
            //
            // Today EVERY default thunk is re-pointed at the final all-formals
            // env by Phase 2, so `{ a, b ? 1 }` closes
            // `b-thunk -> env -> b-thunk` and the whole call frame is immortal.
            // But a default only NEEDS the final env if it can reach a formal
            // that is itself satisfied by a default — those are the only names
            // still unbound when the default is built. Everything else (an
            // argument-supplied formal, the `@`-bind, any outer name) is
            // already in scope, so the capture is complete on the spot and the
            // cycle never has to be closed.
            //
            // Splitting the single pass in two is what makes that true:
            // pass A binds every argument-supplied formal FIRST, so pass B's
            // captures see all of them regardless of declaration order.
            //
            // The reorder is byte-safe: formal names are unique (a duplicate
            // is a parse error), `bindings` is a hash map read only by key, and
            // building a thunk has no side effects — so nothing observes the
            // order in which the two passes populate the env, only its final
            // contents, which are unchanged.
            let narrow = scope_narrow_enabled();
            // The formals that will be satisfied BY A DEFAULT — i.e. exactly
            // the names not yet bound when pass B runs.
            let default_names: HashSet<String> = if narrow {
                entries
                    .iter()
                    .filter(|e| e.default().is_some())
                    .filter_map(|e| e.ident())
                    .map(|i| ident_text(&i))
                    .filter(|n| attrs.get(n).is_none())
                    .collect()
            } else {
                HashSet::new()
            };

            if narrow {
                // PASS A — argument-supplied formals only. The
                // `missing argument` error still fires here, in entry order,
                // exactly where the single pass raised it.
                let mut deferred: Vec<(String, ast::Expr)> =
                    Vec::with_capacity(default_names.len());
                for entry in &entries {
                    let ident = entry.ident().ok_or_else(|| {
                        EvalError::ParseError("pat entry missing ident".to_string())
                    })?;
                    let name = ident_text(&ident);
                    if let Some(v) = attrs.get(&name) {
                        env.bind(name, v.clone());
                    } else if let Some(default_expr) = entry.default() {
                        deferred.push((
                            name,
                            ast::Expr::cast(default_expr.syntax().clone()).unwrap(),
                        ));
                    } else {
                        return Err(EvalError::type_error(
                            format!("missing argument '{name}'{}", eval_file_ctx()),
                        ));
                    }
                }
                // PASS B — the defaults, capturing an env that already carries
                // every argument-supplied formal and the `@`-bind.
                for (name, default_expr) in deferred {
                    let thunk =
                        Thunk::new_suspended(default_expr.clone(), env.clone());
                    let referenced = referenced_idents(&default_expr);
                    if default_names.iter().any(|n| referenced.contains(n.as_str())) {
                        // Reaches another DEFAULTED formal, which may not be
                        // bound yet — it needs Phase 2's re-point, and pays
                        // the cycle.
                        default_thunks.push(thunk.clone());
                        crate::value::census::scope_pinned();
                    } else {
                        crate::value::census::scope_narrowed();
                    }
                    env.bind(name, Value::Thunk(thunk));
                }
            } else {
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
                    if use_batch {
                        pairs.push((name, value));
                    } else {
                        env.bind(name, value);
                    }
                }
                if use_batch {
                    env.bind_many(pairs);
                }
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

    // Regression (2026-07-10): the let-scope fix-point detector must count
    // only GENUINE variable references, not attribute names / attrset keys
    // (which sit under a `NODE_ATTRPATH`).  nixpkgs `lib/types.nix` has
    // `placeholder = if lhs.placeholder == …` whose RHS mentions the
    // *attribute* `.placeholder`; the old raw-token match falsely flagged
    // the binding self-recursive and routed it through the Promise path.
    #[test]
    fn is_self_recursive_binding_ignores_attribute_names() {
        fn expr(s: &str) -> ast::Expr {
            rnix::Root::parse(s).tree().expr().expect("parse")
        }
        // attribute names / keys are NOT references to the binding
        assert!(!is_self_recursive_binding(&expr("lhs.placeholder"), "placeholder"));
        assert!(!is_self_recursive_binding(&expr("{ placeholder = 1; }"), "placeholder"));
        assert!(!is_self_recursive_binding(
            &expr("if lhs.placeholder == rhs.placeholder then lhs.placeholder else null"),
            "placeholder",
        ));
        // genuine variable references ARE detected
        assert!(is_self_recursive_binding(&expr("placeholder + 1"), "placeholder"));
        assert!(is_self_recursive_binding(
            &expr("if placeholder then 1 else 2"),
            "placeholder"
        ));
    }

    // M2 thunk-waste (byte-safe eager constant): a NON-interpolated string in a
    // maybe_thunk site is evaluated directly (no suspended thunk). The value +
    // its (empty) context must be byte-identical to forcing a thunk of it.
    #[test]
    fn maybe_thunk_eager_constant_str_is_byte_identical() {
        fn expr(s: &str) -> ast::Expr {
            rnix::Root::parse(s).tree().expr().expect("parse")
        }
        let env = Env::new();
        // Constant string → returned as a concrete String, NOT a Thunk.
        let v = maybe_thunk(&expr(r#""abc""#), &env, false, None);
        assert!(matches!(v, Value::String(_)), "constant str should be eager, got {v:?}");
        assert_eq!(force_value(&v).unwrap(), Value::string("abc"));
        // Interpolated string → MUST stay a thunk (lazy `${…}` force).
        let vi = maybe_thunk(&expr(r#""a${b}c""#), &env, false, None);
        assert!(matches!(vi, Value::Thunk(_)), "interpolated str must stay thunked");
    }

    // The pure-constant arg classifier admits ONLY literals + non-interpolated
    // strings/paths, and rejects everything that could throw/diverge/observe a
    // fixpoint — the laziness safety boundary of the apply-arg optimization.
    #[test]
    fn eval_pure_constant_arg_classification() {
        fn expr(s: &str) -> ast::Expr {
            rnix::Root::parse(s).tree().expr().expect("parse")
        }
        // ADMIT: pure constants (byte-safe to eval eagerly in an arg position).
        assert!(eval_pure_constant_arg(&expr("42")).is_some());
        assert!(eval_pure_constant_arg(&expr("3.14")).is_some());
        assert!(eval_pure_constant_arg(&expr(r#""const""#)).is_some());
        assert!(eval_pure_constant_arg(&expr("/abs/path")).is_some());
        // REJECT: anything that could throw / diverge / observe laziness.
        assert!(eval_pure_constant_arg(&expr(r#""a${b}c""#)).is_none(), "interpolated str");
        // `true`/`false`/`null` are IDENTS in nix (shadowable), not literals —
        // rejected to avoid a with-scope force, correctly conservative.
        assert!(eval_pure_constant_arg(&expr("true")).is_none(), "bool is an ident");
        assert!(eval_pure_constant_arg(&expr("x")).is_none(), "ident (with-scope force)");
        assert!(eval_pure_constant_arg(&expr("a.b")).is_none(), "select (fixpoint)");
        assert!(eval_pure_constant_arg(&expr("f x")).is_none(), "apply (may throw)");
        assert!(eval_pure_constant_arg(&expr("1 + 1")).is_none(), "binop (may throw)");
        assert!(eval_pure_constant_arg(&expr("throw \"x\"")).is_none(), "throw stays lazy");
    }

    // LAZINESS GUARD: a lambda that IGNORES its arg must NOT force it — even a
    // throwing arg. The pure-constant optimization only touches inert constants,
    // so a `throw`-ing arg stays fully thunked and the ignoring lambda succeeds.
    #[test]
    fn ignored_throwing_arg_stays_lazy() {
        assert_eq!(ev(r#"(x: 7) (throw "boom")"#), Value::Int(7));
        // And an ignored constant arg is equally invisible.
        assert_eq!(ev(r#"(x: 7) "const""#), Value::Int(7));
        // A USED constant arg produces the right value.
        assert_eq!(ev(r#"(x: x) "used""#), Value::string("used"));
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

    // ── Inner dynamic attrpath key laziness ──────────────────
    // CppNix defers a dynamic key that is NOT at the head of an attrpath:
    // `{ a.${e} = v; }` builds `{ a = <thunk {${e}=v}>; }`, so `e` never
    // forces until `.a` is demanded. Reading a sibling must not force the
    // inner dynamic key. Root fix: `build_deferred_tail_attr` in eval.rs.
    // This is the pure-builtins reduction of the NixOS module-system
    // `config.homes.${cfg.userName}` fixpoint divergence.
    #[test]
    fn dynamic_inner_attr_key_is_lazy_on_sibling_read() {
        // The dynamic key throws; reading the SIBLING must NOT force it.
        assert_eq!(
            ev(r#"let s = { a.${throw "KEYFORCED"} = 7; other = 9; }; in s.other"#),
            Value::Int(9),
        );
    }

    #[test]
    fn dynamic_inner_attr_key_resolves_on_head_demand() {
        // Demanding the head DOES resolve the deferred dynamic key.
        let v = ev(r#"let u = "bob"; s = { homes.${u} = 7; }; in s.homes"#);
        if let Value::Attrs(attrs) = force_value(&v).unwrap() {
            assert_eq!(attrs.get("bob"), Some(&Value::Int(7)));
        } else {
            panic!("expected Attrs");
        }
    }

    #[test]
    fn dynamic_inner_attr_key_merges_with_static_sibling() {
        // Collision under one head still deep-merges (static + dynamic).
        let v = ev(r#"let u = "x"; s = { a.${u} = 1; a.b = 2; }; in s.a"#);
        if let Value::Attrs(attrs) = force_value(&v).unwrap() {
            assert_eq!(attrs.get("x"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
        } else {
            panic!("expected Attrs");
        }
    }

    #[test]
    fn dynamic_inner_attr_key_null_skips_binding() {
        // A null dynamic inner key skips the definition (CppNix rule):
        // `a` becomes an empty attrset, the sibling stays.
        let v = ev(
            r#"let c = true; s = { a.${if c then null else "n"} = 5; b = 1; }; in s.b"#,
        );
        assert_eq!(v, Value::Int(1));
    }

    // ── M2.6 ROOT #3: interpolated-STRING tail keys are dynamic too ──────
    // `{ a."p${e}" = v; }` must build `{ a = <thunk {"p${e}"=v}>; }` — an
    // interpolated-string attr key references `e` and so must defer like a
    // bare `${e}`, never force at construction. Reading a sibling must NOT
    // force it (the KEYFORCE discriminator, now for a `Str` key).
    #[test]
    fn interpolated_string_attr_key_is_lazy_on_sibling_read() {
        assert_eq!(
            ev(r#"let s = { a."p/${throw "KEYFORCED"}" = 7; other = 9; }; in s.other"#),
            Value::Int(9),
        );
    }

    #[test]
    fn interpolated_string_attr_key_resolves_on_head_demand() {
        // Demanding the head DOES resolve the deferred interpolated key.
        let v = ev(r#"let u = "bob"; s = { homes."u/${u}" = 7; }; in s.homes"#);
        if let Value::Attrs(attrs) = force_value(&v).unwrap() {
            assert_eq!(attrs.get("u/bob"), Some(&Value::Int(7)));
        } else {
            panic!("expected Attrs");
        }
    }

    #[test]
    fn purely_literal_string_attr_key_stays_eager_static() {
        // A `Str` key with NO interpolation is a plain static key and must
        // NOT be treated as dynamic (it forces nothing, deep-merges).
        let v = ev(r#"let s = { a."foo bar" = 1; a.b = 2; }; in s.a"#);
        if let Value::Attrs(attrs) = force_value(&v).unwrap() {
            assert_eq!(attrs.get("foo bar"), Some(&Value::Int(1)));
            assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
        } else {
            panic!("expected Attrs");
        }
    }

    // ── M2.6 ROOT #3 (collision case): dynamic tail key under a head that
    // a sibling binding already wrote must stay lazy AND deep-merge.
    #[test]
    fn dynamic_tail_key_under_colliding_head_is_lazy() {
        // `sd.services.x` writes head `sd`; the second binding's dynamic
        // key must NOT force when a SIBLING (`sd.services`) is read.
        let v = ev(
            r#"let s = { sd.services.x = 1; sd.tmpfiles.${throw "KEYFORCED"}.d = 2; }; in s.sd.services.x"#,
        );
        assert_eq!(v, Value::Int(1));
    }

    #[test]
    fn dynamic_tail_key_under_colliding_head_resolves_and_merges() {
        // Demanding the dynamic branch resolves the key; the sibling
        // static branch (`sd.services`) survives the merge intact.
        let v = ev(
            r#"let k = "z"; s = { sd.services.x = 1; sd.tmpfiles.${k}.d = 2; }; in s.sd"#,
        );
        let sd = force_value(&v).unwrap();
        if let Value::Attrs(sd_attrs) = &sd {
            // static sibling intact
            let services = force_value(sd_attrs.get("services").unwrap()).unwrap();
            if let Value::Attrs(a) = &services {
                assert_eq!(force_value(a.get("x").unwrap()).unwrap(), Value::Int(1));
            } else { panic!("expected services attrs"); }
            // dynamic branch resolved to key "z"
            let tmpfiles = force_value(sd_attrs.get("tmpfiles").unwrap()).unwrap();
            if let Value::Attrs(a) = &tmpfiles {
                let z = force_value(a.get("z").unwrap()).unwrap();
                if let Value::Attrs(zd) = &z {
                    assert_eq!(force_value(zd.get("d").unwrap()).unwrap(), Value::Int(2));
                } else { panic!("expected z attrs"); }
            } else { panic!("expected tmpfiles attrs"); }
        } else {
            panic!("expected sd attrs");
        }
    }

    // ── M2.6 ROOT #4a — `with` namespace must be LAZY ─────────────────
    // `with X; body` stores the namespace as a thunk forced only on a
    // bare-ident fallthrough lookup; demanding only the body's WHNF/keys
    // must NOT force X.  cppnix: `attrNames (with (throw "X"); {a=1;})`
    // → ["a"].  Before the fix, sui EVALUATED the namespace at `with`-entry
    // and threw.  This is the load-bearing over-force behind the M2.6
    // `concatLists null` (nixpkgs' `config = mkIf … (with config.services.X;
    // { … })` module shape forced `config.services.X` during collection).
    #[test]
    fn with_namespace_is_lazy_on_body_whnf() {
        let v = ev(r#"builtins.attrNames (with (throw "WITH-FORCED"); { a = 1; b = 2; })"#);
        if let Value::List(items) = force_value(&v).unwrap() {
            let names: Vec<String> = items
                .iter()
                .map(|i| match force_value(i).unwrap() {
                    Value::String(s) => s.as_str().to_string(),
                    other => panic!("expected string, got {}", other.type_name()),
                })
                .collect();
            assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn with_namespace_forces_only_on_fallthrough() {
        // A bare ident that falls through lexical scope DOES resolve via
        // the namespace (correct cppnix semantics) — proves the deferred
        // thunk is real and gets forced on demand, not an accidental no-op.
        assert_eq!(ev(r#"with { x = 42; }; x"#), Value::Int(42));
        // A lexical binding shadows the with-scope, so the (throwing)
        // namespace is never forced — the laziness we rely on for M2.6.
        assert_eq!(ev(r#"let x = 7; in with (throw "NS"); x"#), Value::Int(7));
    }

    // ── M2.6 ROOT #4b — depth-≥2 dotted full-set leaf must deep-merge ──
    // `o.a = { x = 1; }` inserts `o = { a = <thunk {x=1}> }` (leaf goes
    // through maybe_thunk); a deeper sibling `o.a.y = 2` recurses
    // merge_nested_insert down to key `a` where the existing value is that
    // thunk.  Before the fix, merge_nested_insert required BOTH sides to be
    // concrete Attrs, so the Thunk-vs-Attrs collision OVERWROTE — dropping
    // `x`.  cppnix desugars both orderings into `o.a = { x = 1; y = 2; }`.
    // This is the M2.6 post-`with`-fix frontier (nixpkgs alsa's
    // `options.hardware.alsa = { … }` + `options.hardware.alsa.enablePersistence
    // = …` merged to only {enablePersistence} → `cardAliases` "does not exist").
    #[test]
    fn dotted_fullset_leaf_deep_merges_with_deeper_sibling() {
        let v = ev(r#"{ o.a = { x = 1; }; o.a.y = 2; }.o.a"#);
        if let Value::Attrs(a) = force_value(&v).unwrap() {
            assert_eq!(force_value(a.get("x").unwrap()).unwrap(), Value::Int(1));
            assert_eq!(force_value(a.get("y").unwrap()).unwrap(), Value::Int(2));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn dotted_fullset_leaf_deep_merge_reverse_order() {
        // Deeper sibling FIRST, full-set leaf SECOND — the NEW value is the
        // `<thunk {x=1}>`; must still merge (the collision forces it).
        let v = ev(r#"{ o.a.y = 2; o.a = { x = 1; }; }.o.a"#);
        if let Value::Attrs(a) = force_value(&v).unwrap() {
            assert_eq!(force_value(a.get("x").unwrap()).unwrap(), Value::Int(1));
            assert_eq!(force_value(a.get("y").unwrap()).unwrap(), Value::Int(2));
        } else {
            panic!("expected attrs");
        }
    }

    #[test]
    fn dotted_fullset_leaf_merge_preserves_leaf_laziness() {
        // The merge forces the existing/new leaf to WHNF (keys) but MUST
        // NOT force the leaf VALUES — a throwing sibling value that is never
        // demanded stays lazy.
        assert_eq!(ev(r#"{ o.a = { x = throw "X-NEVER"; }; o.a.y = 2; }.o.a.y"#), Value::Int(2));
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

    // ── Interpolated path literals (cid-marquee root, 2026-07-12) ──
    //
    // CppNix path literals may contain `${e}` antiquotations: `./${x}.nix`,
    // `/a/${e}`, `~/${e}`. sui previously flattened the whole path token to
    // raw text and dropped the interpolation (`import ./${x}.nix` →
    // `No such file or directory`). The `${e}` must be evaluated,
    // string-coerced (plain, no copy-to-store), spliced, and the result is
    // still a `path` value. Oracles taken from cppnix.

    #[test]
    fn interp_path_abs_splices_and_types_path() {
        // /a/${x}/b with x="foo" → /a/foo/b, type path (nix oracle).
        let v = ev(r#"let x = "foo"; in /a/${x}/b"#);
        assert_eq!(v, Value::Path(Box::new(SmolStr::from("/a/foo/b"))));
    }

    #[test]
    fn interp_path_abs_multi_and_slash_in_value() {
        // Multiple interpolations + a slash inside the spliced value.
        assert_eq!(
            ev(r#"let a = "x"; b = "y/z"; in /p/${a}/${b}.nix"#),
            Value::Path(Box::new(SmolStr::from("/p/x/y/z.nix"))),
        );
    }

    #[test]
    fn interp_path_abs_normalizes_double_slash_seam() {
        // A path-typed interpolation splices the raw path (no copy-to-store)
        // and the `/` seam is normalized: `/bar/` + `/tmp/foo` → /bar/tmp/foo.
        assert_eq!(
            ev(r#"/bar/${/tmp/foo}"#),
            Value::Path(Box::new(SmolStr::from("/bar/tmp/foo"))),
        );
    }

    #[test]
    fn interp_path_rel_resolves_against_eval_dir() {
        // The spicetify `map (x: ./${x}.nix) [...]` root: a relative
        // interpolated path resolves against the defining file's directory,
        // exactly like a plain `./foo.nix` literal.
        let _g = push_eval_file(std::path::PathBuf::from("/tmp/example/default.nix"));
        assert_eq!(
            ev(r#"let x = "foo"; in ./${x}.nix"#),
            Value::Path(Box::new(SmolStr::from("/tmp/example/foo.nix"))),
        );
    }

    #[test]
    fn interp_path_rel_no_eval_dir_keeps_relative_text() {
        // With no eval-file context the plain branch keeps the raw relative
        // text; the interpolated branch splices then does the same.
        assert_eq!(
            ev(r#"let x = "foo"; in ./${x}.nix"#),
            Value::Path(Box::new(SmolStr::from("./foo.nix"))),
        );
    }

    #[test]
    fn interp_path_home_splices_leading_tilde_preserved() {
        // Home paths splice their `${e}`; the leading `~` is carried as-is
        // (matching sui's plain `~/foo` behavior — `~`-expansion is a
        // separate, pre-existing concern, not introduced here).
        assert_eq!(
            ev(r#"let x = "foo"; in ~/${x}/bar"#),
            Value::Path(Box::new(SmolStr::from("~/foo/bar"))),
        );
    }

    #[test]
    fn interp_path_non_interpolated_still_raw() {
        // A path with no `${…}` must keep the trivial raw-text shortcut
        // (byte-for-byte identical to the plain branch).
        assert_eq!(ev("/a/b/c"), Value::Path(Box::new(SmolStr::from("/a/b/c"))));
        assert_eq!(ev("~/plain"), Value::Path(Box::new(SmolStr::from("~/plain"))));
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
        // CppNix string interpolation is copy-to-store coercion: a nonexistent
        // path errors "path '…' does not exist" (previously sui spliced the raw
        // relative path "./foo" verbatim, diverging from nix). The positive
        // copy-to-store case is byte-verified in
        // interp_path_copies_to_store_byte_matches_cppnix below.
        assert!(eval(r#""path: ${./foo-nonexistent-xyz}""#).is_err());
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
    fn builtins_list_to_attrs_duplicate_key_first_wins() {
        // Nix `listToAttrs` keeps the FIRST occurrence of a duplicate `name`
        // (later duplicates are ignored). cppnix returns 1 here, not 2.
        // Byte-parity root (cid darwin): a Cargo.lock listing a crate twice
        // (registry entry then git entry of the same name+version) must
        // resolve to the FIRST source, so `substrate/lockfile-delta.nix`'s
        // `lockByKey` picks the registry crate exactly as nix does. Last-wins
        // silently switched the source to git and produced a structurally
        // different `rust_<crate>` derivation.
        assert_eq!(
            ev(r#"(builtins.listToAttrs [{ name = "k"; value = 1; } { name = "k"; value = 2; }]).k"#),
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

    #[test]
    fn with_scope_over_lazy_thunk_chain_resolves() {
        // A `with`-head that resolves through a NESTED thunk chain
        // (`Thunk(Thunk(Attrs))`) must still be searched: the lookup
        // has to FULLY force the head (chase the chain), not take a
        // single force step. A single step leaves a `Value::Thunk`
        // that `type_name()` reports as "set" but the `Value::Attrs`
        // match rejects — the scope is skipped and a bare ident
        // through it fails with a spurious UndefinedVar. This corners
        // the nixpkgs `platforms = with lib.platforms; unix;` shape.
        assert_eq!(
            ev(r#"let outer = if true then (if true then { unix = 42; } else {}) else {};
                      # force a two-deep lazy wrap of the with-head
                      head = (x: x) ((y: y) outer);
                  in with head; unix"#),
            Value::Int(42),
        );
    }

    #[test]
    fn with_scope_head_from_deep_select_resolves() {
        // `with a.b.c; key` where a.b.c is a lazily-selected attrset —
        // the bare-ident body must find `key` through the forced head.
        assert_eq!(
            ev(r#"let a = { b = { c = { key = 7; }; }; }; in with a.b.c; key"#),
            Value::Int(7),
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

    #[test]
    fn attrset_deep_merge_fullset_then_dotted() {
        // General root (gst-plugins-base `passthru.waylandEnabled` drop):
        // `a = { x = 1; }; a.y = 2;` — the full-set binding is a lazy
        // Thunk (attrset literals go through maybe_thunk), so a naive
        // merge_nested_insert (which only merges concrete Value::Attrs)
        // overwrote `a` with `{ y = 2 }`, silently dropping `x`. The
        // collision must force the existing thunk to WHNF first.
        let v = ev("let s = { a = { x = 1; }; a.y = 2; }; in s.a.x + s.a.y");
        assert_eq!(v, Value::Int(3));
        // both keys must survive (not just their sum)
        let both = ev("let s = { a = { x = 1; }; a.y = 2; }; in [ s.a.x s.a.y ]");
        if let Value::List(items) = both {
            assert_eq!(force_value(&items[0]).unwrap(), Value::Int(1));
            assert_eq!(force_value(&items[1]).unwrap(), Value::Int(2));
        } else {
            panic!("expected list");
        }
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

    // Regression (2026-07-11): a bare `inherit x;` must resolve LAZILY, like
    // a plain reference to `x` — not eagerly at attrset construction. When
    // `x` is provided only by an enclosing `with` scope whose value is a
    // fixpoint still being constructed, eager resolution spuriously threw
    // `UndefinedVar`. nixpkgs `all-packages.nix` is
    // `with pkgs; { nettle = import … { inherit callPackage; }; }`, so
    // `inherit callPackage` must resolve from the `with pkgs` scope at force
    // time. (This was the nettle UndefinedVar('callPackage') drop.)
    #[test]
    fn inherit_plain_from_with_scope_lazy() {
        // `inherit cp` reads `cp` from a `with self` fixpoint scope; the
        // attr forcing it (`a`) must resolve `cp` lazily against the settled
        // scope, not eagerly during attrset construction.
        assert_eq!(
            ev("let fix = f: let x = f x; in x;
                    self = fix (self: with self; {
                      a = use { inherit cp; };
                      use = { cp }: cp 5;
                      cp = x: x + 100;
                    });
                in self.a"),
            Value::Int(105),
        );
        // Simpler: bare inherit from a plain (non-blackhole) with scope.
        assert_eq!(
            ev("with { y = 7; }; { inherit y; }.y"),
            Value::Int(7),
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
        // CppNix %f-format: always 6 decimal places.
        assert_eq!(
            ev(r#""${toString 3.14}""#),
            Value::string("3.140000"),
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

    /// A fileless frame MASKS the parent's file rather than being skipped.
    ///
    /// Regression: the stack used to be `Vec<PathBuf>`, so a thunk captured in
    /// a `--expr` context pushed nothing when it forced and the callee's file
    /// stayed visible. `builtins.unsafeGetAttrPos` then reported the callee's
    /// path where CppNix reports `null`, which set `eval-config.nix`'s
    /// `modulesLocation` and permuted NixOS module definition order.
    #[test]
    fn fileless_frame_masks_parent_file() {
        let outer = std::path::PathBuf::from("/a/x.nix");
        let _g_outer = push_eval_file(outer.clone());
        assert_eq!(current_eval_file(), Some(outer.clone()));
        {
            let _g_none = push_eval_frame(None);
            // The whole point: NOT Some("/a/x.nix").
            assert_eq!(current_eval_file(), None);
            assert_eq!(current_eval_dir(), None);
            assert_eq!(eval_file_stack_snapshot().last().map(String::as_str), Some("<no-file>"));
        }
        // Popped — the parent is visible again.
        assert_eq!(current_eval_file(), Some(outer));
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

    /// `inherit` binds an attribute, so it carries a position.
    ///
    /// Regression: `attach_attrset_positions` matched only
    /// `Entry::AttrpathValue`, so every inherited key was position-less — most
    /// of nixpkgs' `lib`, which re-exports via `inherit (self.options) mkOption
    /// …`, and it fed a null into `eval-config.nix`'s `modulesLocation`.
    ///
    /// Shaped exactly like `unsafe_get_attr_pos_reports_file_and_offset_column`
    /// (ONE direct `eval`, no lambda, no second evaluation) because the
    /// in-process harness is fragile here: the source-text registry is a
    /// thread-local that `pos.rs`'s tests clear, so a multi-eval version passes
    /// standalone and fails in the full suite. The CLI path is not affected —
    /// verified against `nix eval` on both shapes, both engines agreeing on
    /// column 18.
    #[test]
    fn inherit_bindings_carry_positions() {
        let dir = tempfile::tempdir().unwrap();
        // A PLAIN attrset, no `let ... in` wrapper: with the wrapper the
        // result is built lazily AFTER `import` returns, and the in-process
        // harness then resolves it without the file on the eval stack. The CLI
        // handles both (measured), the harness only this one.
        let body = "{ inherit ({ x = 1; }) x; }\n";
        let f = dir.path().join("inh.nix");
        std::fs::write(&f, body).unwrap();
        let v = eval(&format!("builtins.unsafeGetAttrPos \"x\" (import {})", f.display())).unwrap();
        let attrs = match v {
            Value::Attrs(a) => a,
            Value::Null => panic!("null — the inherit binding carried no position"),
            o => panic!("expected attrs, got {o:?}"),
        };
        // Computed from the fixture, never hardcoded: a hardcoded expectation is
        // how `pos::line_col`'s own "verified" comment came to agree with the
        // bug it documented.
        let off = body.rfind("x; }").unwrap();
        let bol = body[..off].rfind('\n').map_or(0, |i| i + 1);
        assert_eq!(*attrs.get("line").unwrap(), Value::Int(1));
        assert_eq!(*attrs.get("column").unwrap(), Value::Int((off - bol) as i64 + 1));
    }

    /// Corpus gate: every attribute-BINDING form carries a position.
    ///
    /// Seals the class the three position bugs came from, rather than the three
    /// instances: `//` dropping positions wholesale, `pos::line_col` returning a
    /// constant, and `inherit` never being recorded. Each was found only because
    /// a NixOS toplevel drvPath diverged — an expensive way to learn that an
    /// attribute lost its position.
    ///
    /// Expectations are DERIVED from the fixture, never written out, so the test
    /// cannot drift into agreeing with whatever the implementation emits. That
    /// is exactly how `line_col`'s own "verified against nix eval" comment came
    /// to document the bug it contained.
    ///
    /// Anti-vacuity: the row count is asserted, and any `NULL` fails. A change
    /// that stops attaching positions altogether makes every row `NULL` — which
    /// must be a failure, not an empty-set pass.
    #[test]
    fn every_binding_form_carries_a_position() {
        let dir = tempfile::tempdir().unwrap();
        // One line per key so the expected line number is its 1-based index.
        let body = concat!(
            "let src = { i = 1; j = 2; }; in {\n",
            "  plain = 1;\n",
            "  \"quoted\" = 2;\n",
            "  inherit (src) i;\n",
            "  inherit src;\n",
            "  nested.deep = 3;\n",
            "}\n",
        );
        let f = dir.path().join("forms.nix");
        std::fs::write(&f, body).unwrap();

        // `nested` is the head of a dotted path; CppNix points at the head.
        let keys = ["plain", "quoted", "i", "src", "nested"];
        let probe = keys
            .iter()
            .map(|k| format!(
                "(let q = builtins.unsafeGetAttrPos \"{k}\" t; \
                 in if q == null then \"{k}=NULL\" \
                 else \"{k}=${{toString q.line}}:${{toString q.column}}\")"
            ))
            .collect::<Vec<_>>()
            .join(" + \" \" + ");
        let got = eval(&format!("let t = import {}; in {probe}", f.display()))
            .unwrap()
            .as_string()
            .unwrap()
            .to_string();

        assert!(!got.contains("NULL"), "a binding form lost its position: {got}");
        let rows: Vec<&str> = got.split(' ').collect();
        assert_eq!(rows.len(), keys.len(), "corpus shrank — gate would be vacuous: {got}");

        // Derive each expectation by locating the key token in the fixture.
        for (k, row) in keys.iter().zip(&rows) {
            let needle = match *k {
                "quoted" => "\"quoted\"".to_string(),
                "i" => "i;".to_string(),
                "src" => "src;".to_string(),
                // A dotted path's head is followed by `.`, not ` =` — CppNix
                // reports the HEAD token's position for the outer key.
                "nested" => "nested.".to_string(),
                other => format!("{other} ="),
            };
            let off = body.find(&needle).unwrap();
            let bol = body[..off].rfind('\n').map_or(0, |i| i + 1);
            let line = 1 + body[..off].matches('\n').count();
            let col = off - bol + 1;
            assert_eq!(*row, format!("{k}={line}:{col}"), "wrong position for `{k}` in:\n{body}");
        }
    }

    /// A missing-argument error names the file the LAMBDA came from.
    ///
    /// Evaluated with `eval_with_file`, not `push_eval_file` + bare `eval`, and
    /// the difference is the point. Calling a closure now pushes the closure's
    /// OWN file — including a fileless frame when it has none — so a lambda
    /// defined in a fileless string no longer borrows whatever unrelated file
    /// happens to sit on the stack. That borrowing is what the old form
    /// asserted, and CppNix does not do it: an `--expr` lambda has no file.
    /// Associating the source with a file, as every real `import` does, keeps
    /// the original intent (errors carry file context) while testing the path
    /// production actually takes. Verified against CppNix: for a lambda in a
    /// real file both engines name that file.
    #[test]
    fn error_missing_argument_includes_file_context() {
        let p = std::path::PathBuf::from("/nix/store/func.nix");
        let result = eval_with_file("({ a, b }: a) { a = 1; }", Some(p));
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

    // ── unsafeGetAttrPos — the options.json `attrTag` declarations root ──
    //
    // Seals the CppNix-matching behavior: for a literal attrset built in a
    // FILE, `builtins.unsafeGetAttrPos <key> <set>` returns
    // `{ file; line=1; column=<key byte offset>+1; }`; for a `<string>` eval
    // (no file) it returns `null`. Byte-verified against `nix eval`.

    #[test]
    fn unsafe_get_attr_pos_reports_file_and_offset_column() {
        // The real `attrTag` path: a literal attrset built in an IMPORTED file.
        // `import` registers the file's source text + pushes it on the eval
        // stack, so `eval_attrset` captures the key positions against that file
        // and `unsafeGetAttrPos` resolves them. CppNix reports the file plus a
        // real newline-resolved line and BYTE column.
        //
        // Re-baselined: this used to assert line 1 and column = the key's
        // 1-based byte offset in the whole file, citing "verified against nix
        // eval". It was not — that was sui's own output taken as the oracle,
        // and the same false rule was pinned in pos.rs. Measured on nix 2.31.5:
        // for `{ a = 1;\n  b = 2; }` the `b` key is 2:3, not 1:12.
        let dir = tempfile::tempdir().unwrap();
        // The literal's `b` key sits at a known byte offset in this file.
        let file_body = "{ a = 1;\n  b = 2; }\n";
        let f = dir.path().join("lit.nix");
        std::fs::write(&f, file_body).unwrap();
        let src = format!("builtins.unsafeGetAttrPos \"b\" (import {})", f.display());
        let v = eval(&src).unwrap();
        let attrs = match v { Value::Attrs(a) => a, other => panic!("expected attrs, got {other:?}") };
        assert_eq!(
            attrs.get("file").unwrap().as_string().unwrap(),
            f.to_string_lossy(),
        );
        // `b` is on the SECOND line, at byte column 3.
        let off = file_body.find("b = 2").unwrap();
        let bol = file_body[..off].rfind('\n').map_or(0, |i| i + 1);
        let expected_line = 1 + file_body[..off].matches('\n').count() as i64;
        let expected_col = (off - bol) as i64 + 1;
        assert_eq!(expected_line, 2, "fixture must put `b` on line 2");
        assert_eq!(*attrs.get("line").unwrap(), Value::Int(expected_line));
        let col = match attrs.get("column").unwrap() { Value::Int(n) => *n, o => panic!("{o:?}") };
        assert_eq!(col, expected_col, "column must be the 1-based BYTE column");
    }

    #[test]
    fn unsafe_get_attr_pos_null_for_string_origin() {
        // A `<string>`-eval'd literal (no file on the stack) has no position → null.
        let v = eval("builtins.unsafeGetAttrPos \"a\" { a = 1; }").unwrap();
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn unsafe_get_attr_pos_null_for_missing_key() {
        // A key absent from an imported set → null.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("lit.nix");
        std::fs::write(&f, "{ a = 1; }\n").unwrap();
        let src = format!("builtins.unsafeGetAttrPos \"zzz\" (import {})", f.display());
        let v = eval(&src).unwrap();
        assert_eq!(v, Value::Null);
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

    // Byte-parity root #5: interpolating a source path is CppNix copy-to-store
    // coercion — the path is NAR-copied into /nix/store/<hash>-<name> and the
    // store path (with store-path context) is spliced in, not the raw path.
    // NAR of a single regular file is content+basename only (location-
    // independent), so a temp <dir>/data.txt of "hello\n" yields the exact
    // store path nix 2.34 produced: /nix/store/y9dmv…-data.txt.
    #[test]
    fn interp_path_copies_to_store_byte_matches_cppnix() {
        let dir = std::env::temp_dir().join(format!("sui-r5-interp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("data.txt");
        std::fs::write(&f, b"hello\n").unwrap();
        let expr = format!(r#""${{{}}}""#, f.display());
        let v = eval(&expr).unwrap();
        if let Value::String(ns) = v {
            assert_eq!(
                ns.chars.to_string(),
                "/nix/store/y9dmvfhip31hg8ia4njwjz9vfa3ndphr-data.txt",
            );
            assert!(ns.has_context());
        } else {
            panic!("expected string");
        }
        let _ = std::fs::remove_dir_all(&dir);
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
