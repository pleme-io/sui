//! L3 slice 2 — eval-through-IR for the **pure expression subset**.
//!
//! Evaluates a lowered [`Program`] directly — no rowan re-walk on the eval
//! path. The semantic oracle is the tree-walker (`sui-eval`, `--no-vm`): the
//! companion differential (`tests/eval_differential.rs`) byte-compares the
//! rendered result of both engines over the parity corpus, the render-harness
//! supplement, a closed-value seed, and property-generated expressions.
//!
//! # Why mirror types instead of `sui_eval::Value`
//!
//! No Cargo cycle blocks depending on `sui-eval` (nothing depends on
//! `sui-ir`) — the test harness DOES depend on it (dev-dependency). What
//! blocks *reusing* `sui_eval::Value` inside the IR engine is representational
//! coupling, stated plainly: `ThunkRepr::Suspended` holds an
//! `rnix::ast::Expr` and `Closure` holds `rnix::ast::Param` +
//! `rnix::ast::Expr` — a rowan AST node is the suspension/closure body, which
//! is exactly the representation this slice exists to avoid re-walking. So
//! this module defines the minimal mirror types ([`IrValue`], [`IrEnv`],
//! [`IrThunk`]) whose suspensions are `(Rc<Program>, ExprId, IrEnv)`.
//! Byte-parity is enforced at the *rendered result* level by the
//! differential, not at the value-representation level.
//!
//! # Scope (the honest subset)
//!
//! Handled: Int / Float / Bool / Null / Str (interpolation over pure parts) /
//! Uri / Ident / LetIn / Lambda / Apply / BinOp (all) / UnaryOp / IfElse /
//! List / AttrSet (non-rec + rec) / Select (+ `or`) / HasAttr / With /
//! Assert / Paren / Inherit (+ `inherit (from)`), plus — since slice 3 —
//! **`Path` literals** (abs/rel/home, with interpolation, mirroring the
//! walker's `canon_abs`/`normalize`/eval-dir resolution via the
//! [`crate::path`] mirrors), **`import`** (file loading through the
//! lower-once [`crate::file_eval`] program cache, with typed
//! circular-import detection) and the **builtins bridge**
//! ([`crate::builtins`]) — the most-used pure builtins natively on
//! [`IrValue`], with every *unimplemented* walker builtin pre-seeded as a
//! typed [`IrEvalError::MissingBuiltin`] failed thunk.
//!
//! Since slice 4, `SearchPath` (`<name>`) resolves through NIX_PATH exactly
//! like the walker (a hit is a `Path`; a miss is a catchable
//! [`IrEvalError::Throw`]), and the pure builtin surface is
//! (near-)complete (see [`crate::builtins`]).
//!
//! Not handled — each returns a typed [`IrEvalError::Unsupported`], never a
//! silent wrong value: `LegacyLet`, `CurPos`, and copy-to-store path
//! coercion inside string interpolation (`"${./f}"` needs the store;
//! `toString ./f` — plain coercion — works).
//!
//! # Semantics mirrored from the tree-walker (not from nix)
//!
//! Where sui's tree-walker deliberately (or historically) diverges from
//! CppNix, this engine mirrors the TREE-WALKER, because parity-with-the-
//! oracle is the gate. Notable mirrored behaviors:
//!
//! * `&&` / `||` / `->` type-check only the LHS; the RHS is returned as-is
//!   (`false || 1` evaluates to `1`).
//! * String interpolation coerces like the walker's copy-to-store coercion:
//!   ints/floats/bools/null/lists all coerce (`"${1}"` → `"1"`).
//! * Self/mutually-recursive `let`/`rec` bindings force through a Promise
//!   cell seeded with `{ }` (the walker's M2.6 bridge) — deeper fixpoint
//!   re-entrance sees the partial. The DIRECT self-alias (`let x = x; in x`)
//!   still errors on both engines: the thunk memoizes to itself and the
//!   force chain's depth-100 cycle guard (mirrored from `force_value`)
//!   reports `InfiniteRecursion`.
//! * **Overlay-fixpoint promotion (slice 5).** A binding the syntactic
//!   classifier marked NON-recursive (its RHS never names itself — the
//!   `self:super:`/`callPackage`-across-files shape) forces through a hard
//!   `Blackhole`; when that same thunk is re-entered WHILE still on the force
//!   stack (a genuine self-fixpoint), it is retroactively PROMOTED to a
//!   Promise cell and the in-progress `{ }` partial is returned — so the
//!   fixpoint converges instead of erroring, mirroring `sui-eval`'s
//!   `value.rs::force_inner` Blackhole arm. Bounded by a concurrent-promotion
//!   nest cap (32) and a force-depth runaway backstop (500), both armed like
//!   the walker's.
//! * **Promise-body softening (slice 5).** While evaluating a Promise-state
//!   thunk's body (`IN_PROMISE_EVAL > 0`), and ONLY then, three error classes
//!   soften to `null` — an undefined identifier, calling a non-function, and a
//!   `with`-scoped bare-inherit miss — mirroring the walker's `in_promise_eval`
//!   softening. `eval_select` misses are deliberately NOT softened (the walker
//!   removed that at ROOT #4).
//! * Equality forces lazily and treats a failed inner force as `null`;
//!   lambdas compare by closure identity; builtins compare `false`.
//! * Integer overflow is a typed `Abort`; division by zero (int or float) is
//!   a typed error.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use rustc_hash::FxHashMap;
use sui_intern::{intern, resolve, Symbol};

pub use crate::builtins::IrBuiltin;
use crate::file_eval::{current_eval_dir, push_eval_file};
use crate::ir::{
    AttrName, BinOp, Binding, ExprId, Ir, Param, PathKind, PathPart, Program, StrPart, UnaryOp,
};

// ── overlay-fixpoint promotion state (the M2.6 rec-semantics mirror) ────────
//
// Slice 5 mirrors the tree-walker's Blackhole↔Promise recursive-binding
// machinery (`sui-eval/src/value.rs::force_inner` + the `IN_PROMISE_EVAL`
// softening in `eval.rs`). Three thread-local pieces, one-for-one with the
// walker, drive it:
//
//   * FORCE_STACK — the thunk identities currently mid-force (the walker's
//     `trace::FORCE_STACK`, restricted to the `thunk_id` the promotion needs).
//     A thunk is on the stack for the duration of its `force_step` body eval;
//     a re-entrant Blackhole force whose thunk is STILL on the stack is a
//     genuine self-fixpoint (`self:super:`/`callPackage` across files) that
//     the syntactic classifier missed — so it is PROMOTED to a Promise cell.
//   * IN_PROMISE_EVAL — "currently in a Promise-thunk body" depth. While > 0,
//     and ONLY then, three error classes soften to `null` (undefined ident,
//     non-function call, WithIdent miss) — the walker's `in_promise_eval`
//     softening. NOTE (mirror of the code's CURRENT behaviour): `eval_select`
//     misses are NOT softened here — the walker removed that (ROOT #4,
//     2026-07-11); the two over-forces it masked are fixed at their cause.
//   * PROMOTION_OCCURRED — latched true once any promotion fires, arming the
//     force-depth runaway backstop for the rest of the eval. Like the walker,
//     it is NEVER reset within the library (a fresh thread starts clean); the
//     differential runs recursion fixtures on spawned workers for isolation.

thread_local! {
    /// Thunk identities (`Rc::as_ptr`) currently being forced, innermost last.
    static FORCE_STACK: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    /// Depth of "currently evaluating a Promise-state thunk's body".
    static IN_PROMISE_EVAL: Cell<u32> = const { Cell::new(0) };
    /// Latched once an overlay-fixpoint promotion fires anywhere on this thread.
    static PROMOTION_OCCURRED: Cell<bool> = const { Cell::new(false) };
}

/// Concurrent-promotion nesting cap (walker `FIXPOINT_PROMOTE_NEST_CAP`). A
/// converging fixpoint (`libxcrypt`) bottoms out in ≤ ~18 concurrent
/// promotions; above 2× that we STOP promoting and fall through to
/// `InfiniteRecursion` (which `x.y or default` recovers, exactly like nix).
const FIXPOINT_PROMOTE_NEST_CAP: u32 = 32;

/// Force-stack-depth runaway backstop, armed only after a promotion has fired
/// (walker `PROMOTION_RUNAWAY_FORCE_DEPTH`). A converging fixpoint bottoms out
/// at a force depth of a few dozen; a non-converging promoted partial recurses
/// without bound — this converts that runaway into a recoverable
/// `InfiniteRecursion` before the native stack aborts.
const PROMOTION_RUNAWAY_FORCE_DEPTH: usize = 500;

/// Whether we are currently inside a Promise-thunk body (walker
/// `in_promise_eval`): the three softening sites consult this.
fn in_promise_eval() -> bool {
    IN_PROMISE_EVAL.with(|c| c.get() > 0)
}

/// Whether an overlay-fixpoint promotion has fired on this thread (arms the
/// runaway backstop).
fn promotion_occurred() -> bool {
    PROMOTION_OCCURRED.with(|c| c.get())
}

/// Is `thunk_id` currently on the force stack? (walker `force_stack_contains`)
/// A live Blackhole re-entry whose thunk is still forcing is the promotion
/// trigger.
fn force_stack_contains(thunk_id: usize) -> bool {
    FORCE_STACK.with(|s| s.borrow().iter().any(|&id| id == thunk_id))
}

/// Current force-stack depth (walker `trace::current_force_depth`).
fn force_stack_depth() -> usize {
    FORCE_STACK.with(|s| s.borrow().len())
}

/// RAII push of a thunk id onto the force stack; pops on drop (every exit
/// path, error included) — the mirror of the walker's matched
/// `push_force`/`pop_force`.
struct ForceStackGuard;

impl Drop for ForceStackGuard {
    fn drop(&mut self) {
        FORCE_STACK.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

fn push_force_frame(thunk_id: usize) -> ForceStackGuard {
    FORCE_STACK.with(|s| s.borrow_mut().push(thunk_id));
    ForceStackGuard
}


// ── errors ────────────────────────────────────────────────────────────────

/// Typed evaluation error for the IR engine. Variants mirror the CLASSES of
/// `sui_eval::EvalError` the pure subset can produce; messages are not
/// byte-mirrored (the differential compares rendered VALUES byte-for-byte
/// and errors by class only).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IrEvalError {
    #[error("undefined variable '{0}'")]
    UndefinedVar(String),
    #[error("type error: {0}")]
    TypeError(String),
    #[error("expected {expected}, got {got}")]
    TypeMismatch {
        expected: &'static str,
        got: &'static str,
    },
    #[error("attribute '{0}' not found")]
    AttrNotFound(String),
    #[error("assertion failed")]
    AssertionFailed,
    #[error("division by zero")]
    DivisionByZero,
    #[error("infinite recursion encountered")]
    InfiniteRecursion,
    #[error("evaluation aborted: {0}")]
    Abort(String),
    /// A CATCHABLE `builtins.throw` (and a search-path miss, which the
    /// walker also raises as a throw) — the one error class `tryEval`
    /// catches alongside [`IrEvalError::AssertionFailed`]. Distinct from
    /// [`IrEvalError::Abort`] (uncatchable) so `tryEval` mirrors CppNix.
    #[error("throw: {0}")]
    Throw(String),
    #[error("construct not supported by the pure-subset IR evaluator: {0}")]
    Unsupported(&'static str),
    /// A builtin the walker provides but this engine has not implemented —
    /// pre-seeded as a failed thunk in the `builtins` attrset so the gap is
    /// typed, never a wrong value or a bare `AttrNotFound`.
    #[error("builtin not implemented by the pure-subset IR evaluator: {0}")]
    MissingBuiltin(String),
    /// Circular `import` chain (typed where the walker would recurse until
    /// the stack dies). Carries the `a -> b -> a` chain.
    #[error("circular import: {0}")]
    ImportCycle(String),
    /// File-system failure while loading an imported file.
    #[error("{context}: {message}")]
    Io { context: String, message: String },
    /// Parse (or lower) failure of an imported file.
    #[error("parse error: {0}")]
    Parse(String),
}

// ── values ────────────────────────────────────────────────────────────────

/// Attribute set payload — a sorted map so iteration order is the walker's
/// `sorted_entries()` order (lexicographic by key string) by construction.
pub type IrAttrs = std::collections::BTreeMap<String, IrValue>;

/// A lambda closure over the flat IR: the body is an [`ExprId`] into the
/// owned [`Program`], never an AST node.
#[derive(Debug)]
pub struct IrClosure {
    pub prog: Rc<Program>,
    pub param: Param,
    pub body: ExprId,
    pub env: IrEnv,
}

/// One element of a Nix string's context — the mirror of the walker's
/// `sui_eval::value::ContextElement` (a store-path reference a string
/// depends on). String context is how a derivation output flows into a
/// consuming derivation's `inputDrvs` / `inputSrcs`, and therefore into its
/// drvPath: dropping it silently diverges the drv hash from nix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrContextElem {
    /// Store-path reference (e.g. `/nix/store/abc-hello`).
    Plain(String),
    /// Derivation output reference (`drv!output`) → `inputDrvs[drv] += output`.
    Output { drv: String, output: String },
    /// Entire derivation closure (`=drv`) — carried by a `.drvPath` string;
    /// NOT consumed into `inputDrvs`/`inputSrcs` (mirrors the walker).
    DrvDeep(String),
}

/// The context attached to a Nix string — the mirror of the walker's
/// `sui_eval::value::StringContext`. A **dedup `Vec`** (NOT a set), same as
/// the walker: most strings carry 0–2 elements where linear search beats tree
/// overhead, and — critically — the fold into `inputDrvs`/`inputSrcs` (which
/// then sorts) makes element *order* irrelevant to the drvPath, so a Vec is
/// byte-faithful. `add_plain`/`add_output`/`add_drv_deep`/`merge` mirror the
/// walker's dedup-insert semantics one-for-one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IrStringContext(Vec<IrContextElem>);

impl IrStringContext {
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Merge another context into this one (dedup union).
    pub fn merge(&mut self, other: &IrStringContext) {
        for elem in &other.0 {
            if !self.0.contains(elem) {
                self.0.push(elem.clone());
            }
        }
    }

    /// Add a plain store-path reference (dedup).
    pub fn add_plain(&mut self, path: impl Into<String>) {
        let elem = IrContextElem::Plain(path.into());
        if !self.0.contains(&elem) {
            self.0.push(elem);
        }
    }

    /// Add a derivation-output reference (dedup).
    pub fn add_output(&mut self, drv: impl Into<String>, output: impl Into<String>) {
        let elem = IrContextElem::Output {
            drv: drv.into(),
            output: output.into(),
        };
        if !self.0.contains(&elem) {
            self.0.push(elem);
        }
    }

    /// Add a derivation-deep reference (dedup).
    pub fn add_drv_deep(&mut self, drv: impl Into<String>) {
        let elem = IrContextElem::DrvDeep(drv.into());
        if !self.0.contains(&elem) {
            self.0.push(elem);
        }
    }

    /// Insert a raw element (dedup) — used by `unsafeDiscardOutputDependency`
    /// / `addDrvOutputDependencies` mirrors.
    pub fn insert(&mut self, elem: IrContextElem) {
        if !self.0.contains(&elem) {
            self.0.push(elem);
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &IrContextElem> {
        self.0.iter()
    }
}

/// A Nix value produced by the IR engine — the minimal mirror of
/// `sui_eval::Value` for the pure subset (see module docs for why a mirror).
#[derive(Debug, Clone, Default)]
pub enum IrValue {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A string + its (usually empty) context. Context is boxed behind
    /// `Option<Rc<…>>` so the overwhelmingly-common context-free string pays
    /// zero allocation and `IrValue` stays lean (the A/B force path is
    /// unchanged for plain strings). Equality/ordering/render all ignore the
    /// context — nix `==` and rendering are context-blind — so the second
    /// field is matched `_` everywhere except the derivation fold + the
    /// `hasContext`/`getContext`/`unsafeDiscard*` builtins.
    Str(Rc<String>, Option<Rc<IrStringContext>>),
    /// A path value — the resolved path string (the walker's
    /// `Value::Path` mirror; renders raw, unquoted).
    Path(Rc<String>),
    List(Rc<Vec<IrValue>>),
    Attrs(Rc<IrAttrs>),
    Lambda(Rc<IrClosure>),
    /// A builtin with its captured arguments so far (uniform partial
    /// application — see [`crate::builtins`]).
    Builtin(IrBuiltin, Rc<Vec<IrValue>>),
    Thunk(IrThunk),
}

impl IrValue {
    /// A context-free string (the common case — a literal, an int coercion,
    /// a pure-builtin result). No allocation for the (absent) context.
    #[must_use]
    pub fn string(s: impl Into<String>) -> Self {
        IrValue::Str(Rc::new(s.into()), None)
    }

    /// A string carrying string context — the constructor a derivation's
    /// `.drvPath` / `.outPath` (and any coercion that merges context) uses.
    /// An empty context collapses back to the context-free representation.
    #[must_use]
    pub fn string_with_context(s: impl Into<String>, ctx: IrStringContext) -> Self {
        let boxed = if ctx.is_empty() {
            None
        } else {
            Some(Rc::new(ctx))
        };
        IrValue::Str(Rc::new(s.into()), boxed)
    }

    /// The string's context if it carries any (borrowed through the `Rc`).
    #[must_use]
    pub fn str_context(&self) -> Option<&IrStringContext> {
        match self {
            IrValue::Str(_, Some(c)) => Some(c),
            _ => None,
        }
    }

    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            IrValue::Null => "null",
            IrValue::Bool(_) => "bool",
            IrValue::Int(_) => "int",
            IrValue::Float(_) => "float",
            IrValue::Str(..) => "string",
            IrValue::Path(_) => "path",
            IrValue::List(_) => "list",
            IrValue::Attrs(_) => "set",
            IrValue::Lambda(_) | IrValue::Builtin(..) => "lambda",
            IrValue::Thunk(_) => "thunk",
        }
    }

    /// Force to weak head normal form: chase the thunk chain until the
    /// outermost value is not a thunk. List elements / attr values stay lazy.
    ///
    /// Mirrors the walker's `force_value` chain guard: a thunk chain deeper
    /// than 100 steps (a cycle like `let x = x; in x`, whose thunk memoizes
    /// to itself, or a runaway lazy wrap) is a typed `InfiniteRecursion` —
    /// each `force_step` memoizes its possibly-still-lazy result WITHOUT
    /// deep-forcing, exactly like `ThunkRepr::Evaluated`.
    pub fn force(&self) -> Result<IrValue, IrEvalError> {
        let mut v = self.clone();
        for _ in 0..100 {
            match v {
                IrValue::Thunk(t) => v = t.force_step()?,
                other => return Ok(other),
            }
        }
        Err(IrEvalError::InfiniteRecursion)
    }

    /// Strict boolean accessor on a forced value.
    pub fn as_bool(&self) -> Result<bool, IrEvalError> {
        match self {
            IrValue::Bool(b) => Ok(*b),
            other => Err(IrEvalError::TypeMismatch {
                expected: "bool",
                got: other.type_name(),
            }),
        }
    }
}

// ── thunks ────────────────────────────────────────────────────────────────

/// Thunk lifecycle state — the mirror of the walker's `ThunkRepr` restricted
/// to what the pure subset needs.
enum IrThunkState {
    /// Not yet evaluated: an expression id + captured environment.
    Suspended {
        prog: Rc<Program>,
        expr: ExprId,
        env: IrEnv,
        /// Self/mutually-recursive binding — force through a Promise cell
        /// (re-entrance sees the partial value) instead of a Blackhole.
        recursive: bool,
    },
    /// `inherit (source) name` — force the shared source, select `name`.
    InheritSelect { source: IrThunk, name: String },
    /// Deferred bare `inherit name;` under a `with` scope: resolve at force
    /// time against the captured env (the walker's `WithIdent`).
    WithIdent { name: String, env: IrEnv },
    /// Deferred dynamic-tail attrpath (`{ head.${e} = v; }`): on force,
    /// build the nested attrset from the tail in the captured env (the
    /// walker's `build_deferred_tail_attr`).
    DeferredTail {
        prog: Rc<Program>,
        tail: Vec<AttrName>,
        value: ExprId,
        env: IrEnv,
    },
    /// Deferred application — apply `func` to `args` in order on force
    /// (the walker's `Thunk::new_native` in `map` / `mapAttrs`).
    NativeApply { func: IrValue, args: Vec<IrValue> },
    /// Being forced (non-recursive) — re-entrance is infinite recursion.
    Blackhole,
    /// Being forced (recursive) — re-entrance yields the partial cell.
    Promise(Rc<RefCell<IrValue>>),
    /// Memoized success.
    Evaluated(IrValue),
    /// Memoized failure — re-raised on every subsequent force.
    Failed(IrEvalError),
}

/// A lazy IR value with memoization + blackhole detection.
#[derive(Clone)]
pub struct IrThunk(Rc<RefCell<IrThunkState>>);

impl std::fmt::Debug for IrThunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.0.borrow() {
            IrThunkState::Suspended { expr, .. } => write!(f, "<thunk {expr:?}>"),
            IrThunkState::InheritSelect { name, .. } => write!(f, "<inherit {name}>"),
            IrThunkState::WithIdent { name, .. } => write!(f, "<with-ident {name}>"),
            IrThunkState::DeferredTail { .. } => write!(f, "<deferred-tail>"),
            IrThunkState::NativeApply { .. } => write!(f, "<native-apply>"),
            IrThunkState::Blackhole => write!(f, "<blackhole>"),
            IrThunkState::Promise(_) => write!(f, "<promise>"),
            IrThunkState::Evaluated(v) => write!(f, "<evaluated {v:?}>"),
            IrThunkState::Failed(e) => write!(f, "<failed {e}>"),
        }
    }
}

impl IrThunk {
    #[must_use]
    pub fn suspended(prog: Rc<Program>, expr: ExprId, env: IrEnv) -> Self {
        Self(Rc::new(RefCell::new(IrThunkState::Suspended {
            prog,
            expr,
            env,
            recursive: false,
        })))
    }

    #[must_use]
    pub fn suspended_recursive(prog: Rc<Program>, expr: ExprId, env: IrEnv) -> Self {
        Self(Rc::new(RefCell::new(IrThunkState::Suspended {
            prog,
            expr,
            env,
            recursive: true,
        })))
    }

    fn inherit_select(source: IrThunk, name: String) -> Self {
        Self(Rc::new(RefCell::new(IrThunkState::InheritSelect {
            source,
            name,
        })))
    }

    fn with_ident(name: String, env: IrEnv) -> Self {
        Self(Rc::new(RefCell::new(IrThunkState::WithIdent { name, env })))
    }

    fn deferred_tail(prog: Rc<Program>, tail: Vec<AttrName>, value: ExprId, env: IrEnv) -> Self {
        Self(Rc::new(RefCell::new(IrThunkState::DeferredTail {
            prog,
            tail,
            value,
            env,
        })))
    }

    pub(crate) fn native_apply(func: IrValue, args: Vec<IrValue>) -> Self {
        Self(Rc::new(RefCell::new(IrThunkState::NativeApply {
            func,
            args,
        })))
    }

    /// A thunk that is BORN failed — every force re-raises `err`. Used to
    /// pre-seed unimplemented builtins as typed gaps.
    #[must_use]
    pub fn failed(err: IrEvalError) -> Self {
        Self(Rc::new(RefCell::new(IrThunkState::Failed(err))))
    }

    /// Two-phase binding: replace the captured env of a still-suspended
    /// thunk (and of an `InheritSelect`'s source chain head) so recursive
    /// `let`/`rec` scopes see every sibling. Mirrors `Thunk::update_env`.
    pub fn update_env(&self, new_env: &IrEnv) {
        let mut state = self.0.borrow_mut();
        match &mut *state {
            IrThunkState::Suspended { env, .. } => *env = new_env.clone(),
            IrThunkState::InheritSelect { source, .. } => {
                let source = source.clone();
                drop(state);
                source.update_env(new_env);
            }
            _ => {}
        }
    }

    /// One force step: run this thunk's suspension and memoize the result
    /// (success or failure) WITHOUT deep-forcing it — the result may itself
    /// be a thunk (`Evaluated(Thunk …)`), which [`IrValue::force`]'s
    /// depth-guarded chain chases. This mirrors the walker split between
    /// `force_thunk` (one step, memoized) and `force_value` (the chain).
    pub fn force_step(&self) -> Result<IrValue, IrEvalError> {
        // Fast path + state transition.
        enum Todo {
            Eval {
                prog: Rc<Program>,
                expr: ExprId,
                env: IrEnv,
                promise: Option<Rc<RefCell<IrValue>>>,
            },
            Inherit {
                source: IrThunk,
                name: String,
            },
            WithIdent {
                name: String,
                env: IrEnv,
            },
            DeferredTail {
                prog: Rc<Program>,
                tail: Vec<AttrName>,
                value: ExprId,
                env: IrEnv,
            },
            NativeApply {
                func: IrValue,
                args: Vec<IrValue>,
            },
        }
        // Stable identity of THIS thunk (mirror of the walker's
        // `Rc::as_ptr(&self.0) as usize`): the force-stack membership key the
        // overlay-fixpoint promotion tests, and the frame pushed while the
        // body evaluates.
        let thunk_id = Rc::as_ptr(&self.0) as usize;
        let todo = {
            let mut state = self.0.borrow_mut();
            let todo = match &*state {
                IrThunkState::Evaluated(v) => return Ok(v.clone()),
                IrThunkState::Failed(e) => return Err(e.clone()),
                IrThunkState::Blackhole => {
                    // OVERLAY-FIXPOINT SEMANTIC PROMOTION (mirror of the walker's
                    // `value.rs::force_inner` Blackhole arm, default-ON).
                    //
                    // A re-entered Blackhole whose SAME thunk is still mid-force
                    // is a genuine self-fixpoint (`self:super:`/`callPackage`
                    // threading across files) that the syntactic classifier
                    // (`referenced_idents`) missed — so it installed a hard
                    // Blackhole where nix exposes the not-yet-complete value.
                    // Retroactively PROMOTE it to a real Promise cell and return
                    // the in-progress empty-attrs partial: the outer body then
                    // populates the cell on completion (`became_promise` below),
                    // so inner Rc clones converge and the repr transitions to
                    // Evaluated. Bounded by the concurrent-promotion nest cap;
                    // a genuinely non-terminating cycle keeps re-entering the
                    // empty partial and is stopped by the runaway backstop /
                    // force-chain guard, exactly like the walker.
                    if force_stack_contains(thunk_id)
                        && IN_PROMISE_EVAL.with(|c| c.get()) < FIXPOINT_PROMOTE_NEST_CAP
                    {
                        let cell = Rc::new(RefCell::new(IrValue::Attrs(Rc::new(IrAttrs::new()))));
                        *state = IrThunkState::Promise(cell.clone());
                        IN_PROMISE_EVAL.with(|c| c.set(c.get() + 1));
                        PROMOTION_OCCURRED.with(|c| c.set(true));
                        return Ok(cell.borrow().clone());
                    }
                    return Err(IrEvalError::InfiniteRecursion);
                }
                IrThunkState::Promise(cell) => return Ok(cell.borrow().clone()),
                IrThunkState::Suspended {
                    prog,
                    expr,
                    env,
                    recursive,
                } => Todo::Eval {
                    prog: prog.clone(),
                    expr: *expr,
                    env: env.clone(),
                    promise: if *recursive {
                        // Walker seeds the Promise cell with an empty
                        // attrset — the cheapest partial that propagates.
                        Some(Rc::new(RefCell::new(IrValue::Attrs(Rc::new(
                            IrAttrs::new(),
                        )))))
                    } else {
                        None
                    },
                },
                IrThunkState::InheritSelect { source, name } => Todo::Inherit {
                    source: source.clone(),
                    name: name.clone(),
                },
                IrThunkState::WithIdent { name, env } => Todo::WithIdent {
                    name: name.clone(),
                    env: env.clone(),
                },
                IrThunkState::DeferredTail {
                    prog,
                    tail,
                    value,
                    env,
                } => Todo::DeferredTail {
                    prog: prog.clone(),
                    tail: tail.clone(),
                    value: *value,
                    env: env.clone(),
                },
                IrThunkState::NativeApply { func, args } => Todo::NativeApply {
                    func: func.clone(),
                    args: args.clone(),
                },
            };
            *state = match &todo {
                Todo::Eval {
                    promise: Some(cell),
                    ..
                } => IrThunkState::Promise(cell.clone()),
                _ => IrThunkState::Blackhole,
            };
            todo
        };
        let result = match todo {
            Todo::Eval {
                prog,
                expr,
                env,
                promise,
            } => {
                let is_promise = promise.is_some();
                // Push this thunk's frame for the duration of its body eval —
                // the promotion check in a re-entrant Blackhole force reads it.
                // The guard pops on every exit (error included).
                let _force_guard = push_force_frame(thunk_id);
                // Runaway backstop (force-stack depth), armed only once a
                // promotion has fired anywhere on this thread. A converging
                // fixpoint bottoms out shallow; a non-converging promoted
                // partial recurses without bound — this converts that into a
                // recoverable `InfiniteRecursion` before the native stack
                // aborts. (The eval-depth twin the walker also runs is a typed
                // KnownGap here — see slice write-up; the nest cap + this
                // force-depth backstop catch every fixture-reachable runaway.)
                if promotion_occurred() && force_stack_depth() > PROMOTION_RUNAWAY_FORCE_DEPTH {
                    Err(IrEvalError::InfiniteRecursion)
                } else {
                    // Mirror the walker's thunk force: re-enter the DEFINING
                    // file's context (the captured env's file) so relative path
                    // literals inside a late-forced thunk resolve against the
                    // file that wrote them, not whoever forced them.
                    let _file_guard = env.eval_file().map(|f| push_eval_file((*f).clone()));
                    // M2.6 Promise scope: bump `IN_PROMISE_EVAL` for the body of
                    // a construction-recursive (Promise) thunk so the three
                    // softening sites (undefined ident / non-function call /
                    // WithIdent miss) treat the sentinel partial's fallout as
                    // `null` rather than erroring. Balanced right after.
                    if is_promise {
                        IN_PROMISE_EVAL.with(|c| c.set(c.get() + 1));
                    }
                    // NO deep force here — the (possibly lazy) result is
                    // memoized as-is and the caller's force chain chases it.
                    let r = eval_ir(&prog, expr, &env);
                    if is_promise {
                        IN_PROMISE_EVAL.with(|c| c.set(c.get().saturating_sub(1)));
                    }
                    // A Blackhole (non-recursive at construction) may have been
                    // PROMOTED to Promise mid-body by a same-thunk fixpoint
                    // re-entry (the Blackhole arm above). That promotion bumped
                    // `IN_PROMISE_EVAL` once; balance it here, and populate its
                    // cell exactly like a construction-time Promise.
                    // Mutually exclusive with `is_promise` (a construction-time
                    // Promise never re-enters the Blackhole arm).
                    let became_promise =
                        !is_promise && matches!(&*self.0.borrow(), IrThunkState::Promise(_));
                    if became_promise {
                        IN_PROMISE_EVAL.with(|c| c.set(c.get().saturating_sub(1)));
                    }
                    // M2.6 Promise update: populate the cell with the final
                    // value BEFORE the outer store flips the repr to Evaluated,
                    // so any inner Rc clones that already read the empty partial
                    // converge on their next force.
                    if let Ok(v) = &r {
                        if is_promise {
                            if let Some(cell) = &promise {
                                *cell.borrow_mut() = v.clone();
                            }
                        } else if became_promise {
                            if let IrThunkState::Promise(cell) = &*self.0.borrow() {
                                *cell.borrow_mut() = v.clone();
                            }
                        }
                    }
                    r
                }
            }
            Todo::Inherit { source, name } => {
                // The SOURCE is chain-forced to an attrset; the selected
                // value is returned unforced.
                match IrValue::Thunk(source).force()? {
                    IrValue::Attrs(attrs) => attrs
                        .get(&name)
                        .cloned()
                        .ok_or(IrEvalError::AttrNotFound(name)),
                    other => Err(IrEvalError::TypeMismatch {
                        expected: "set",
                        got: other.type_name(),
                    }),
                }
            }
            Todo::WithIdent { name, env } => match env.lookup(&name) {
                Some(v) => Ok(v),
                // M2.6 Promise softening (mirror of the walker's WithIdent arm,
                // `value.rs:1944`): a bare-inherit name unresolved inside a
                // Promise body — typically a `with` scope sourced from the
                // empty-attrs sentinel that never populated — softens to `null`.
                None if in_promise_eval() => Ok(IrValue::Null),
                None => Err(IrEvalError::UndefinedVar(name)),
            },
            Todo::DeferredTail {
                prog,
                tail,
                value,
                env,
            } => build_tail_attrs(&prog, &tail, value, &env),
            Todo::NativeApply { func, args } => {
                let mut result = Ok(func);
                for arg in args {
                    result = result.and_then(|f| apply(f, arg));
                }
                result
            }
        };
        let mut state = self.0.borrow_mut();
        match &result {
            Ok(v) => *state = IrThunkState::Evaluated(v.clone()),
            Err(e) => *state = IrThunkState::Failed(e.clone()),
        }
        result
    }
}

// ── environment ───────────────────────────────────────────────────────────

/// One `with` scope: the (lazy) namespace value + a shared cache of its
/// forced attrset, mirroring the walker's `WithScope`.
#[derive(Clone)]
struct IrWithScope {
    value: IrValue,
    cached: Rc<RefCell<Option<Rc<IrAttrs>>>>,
}

#[derive(Default)]
struct IrEnvInner {
    bindings: FxHashMap<Symbol, IrValue>,
    /// Innermost LAST (lookup iterates in reverse).
    with_scopes: Vec<IrWithScope>,
    /// The source file this env's evaluation belongs to (the walker's
    /// `Env::eval_file` mirror) — restored on lambda apply + thunk force
    /// so relative paths resolve against their defining file.
    eval_file: Option<Rc<PathBuf>>,
}

/// Evaluation environment — the mirror of `sui_eval::value::Env`: a flat
/// binding map + a `with`-scope stack behind an `Rc` (clone = refcount bump;
/// `bind` is copy-on-write via `Rc::make_mut`-style cloning).
#[derive(Clone, Default)]
pub struct IrEnv(Rc<IrEnvInner>);

impl std::fmt::Debug for IrEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "IrEnv({} bindings, {} with-scopes)",
            self.0.bindings.len(),
            self.0.with_scopes.len()
        )
    }
}

impl IrEnv {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The base environment for the pure subset: the builtins bridge
    /// (`builtins` + the walker's `DEFAULT_SCOPE` bare names).
    /// (`true`/`false`/`null` are handled at `Ident` eval, like the walker.)
    #[must_use]
    pub fn with_pure_builtins() -> Self {
        crate::builtins::base_env()
    }

    /// Child environment (inherits bindings + with-scopes + eval file).
    #[must_use]
    pub fn child(&self) -> Self {
        Self(Rc::new(IrEnvInner {
            bindings: self.0.bindings.clone(),
            with_scopes: self.0.with_scopes.clone(),
            eval_file: self.0.eval_file.clone(),
        }))
    }

    /// Attach a `with` scope (innermost position).
    #[must_use]
    pub fn with_scope(&self, value: IrValue) -> Self {
        let mut inner = IrEnvInner {
            bindings: self.0.bindings.clone(),
            with_scopes: self.0.with_scopes.clone(),
            eval_file: self.0.eval_file.clone(),
        };
        inner.with_scopes.push(IrWithScope {
            value,
            cached: Rc::new(RefCell::new(None)),
        });
        Self(Rc::new(inner))
    }

    /// Tag this env with its source file (the walker's `set_eval_file`).
    pub fn set_eval_file(&mut self, file: Option<Rc<PathBuf>>) {
        match Rc::get_mut(&mut self.0) {
            Some(inner) => inner.eval_file = file,
            None => {
                let inner = IrEnvInner {
                    bindings: self.0.bindings.clone(),
                    with_scopes: self.0.with_scopes.clone(),
                    eval_file: file,
                };
                self.0 = Rc::new(inner);
            }
        }
    }

    /// The source file this env belongs to, if any.
    #[must_use]
    pub fn eval_file(&self) -> Option<Rc<PathBuf>> {
        self.0.eval_file.clone()
    }

    pub fn bind_sym(&mut self, sym: Symbol, value: IrValue) {
        match Rc::get_mut(&mut self.0) {
            Some(inner) => {
                inner.bindings.insert(sym, value);
            }
            None => {
                let mut inner = IrEnvInner {
                    bindings: self.0.bindings.clone(),
                    with_scopes: self.0.with_scopes.clone(),
                    eval_file: self.0.eval_file.clone(),
                };
                inner.bindings.insert(sym, value);
                self.0 = Rc::new(inner);
            }
        }
    }

    pub fn bind(&mut self, name: &str, value: IrValue) {
        self.bind_sym(intern(name), value);
    }

    /// Lexical-only lookup.
    #[must_use]
    pub fn lookup_lexical(&self, sym: Symbol) -> Option<IrValue> {
        self.0.bindings.get(&sym).cloned()
    }

    /// Whether any `with` scope is attached (the walker's
    /// `innermost_with_scope().is_some()` probe used by bare `inherit`).
    #[must_use]
    pub fn has_with_scope(&self) -> bool {
        !self.0.with_scopes.is_empty()
    }

    /// Full lookup: lexical first, then `with` scopes innermost-first.
    /// A scope that fails to force, or forces to a non-attrset, is skipped —
    /// exactly the walker's `lookup_fast` error-swallowing.
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<IrValue> {
        let sym = intern(name);
        if let Some(v) = self.0.bindings.get(&sym) {
            return Some(v.clone());
        }
        for scope in self.0.with_scopes.iter().rev() {
            let cached = scope.cached.borrow().clone();
            let attrs = if let Some(attrs) = cached {
                attrs
            } else {
                match scope.value.force() {
                    Ok(IrValue::Attrs(attrs)) => {
                        *scope.cached.borrow_mut() = Some(attrs.clone());
                        attrs
                    }
                    // Non-attrset or failed force: skip this scope.
                    _ => continue,
                }
            };
            if let Some(v) = attrs.get(name) {
                return Some(v.clone());
            }
        }
        None
    }
}

// ── the evaluator ─────────────────────────────────────────────────────────

/// Evaluate expression `id` of `prog` in `env`. Returns a possibly-lazy
/// [`IrValue`] (WHNF discipline mirrors the tree-walker: containers hold
/// thunks; callers force what they inspect).
pub fn eval_ir(prog: &Rc<Program>, id: ExprId, env: &IrEnv) -> Result<IrValue, IrEvalError> {
    match prog.expr(id) {
        Ir::Int(n) => Ok(IrValue::Int(*n)),
        Ir::Float(f) => Ok(IrValue::Float(*f)),
        Ir::Uri(u) => Ok(IrValue::string(u.clone())),
        Ir::Ident(sym) => {
            let name = resolve(*sym);
            match name.as_str() {
                "true" => Ok(IrValue::Bool(true)),
                "false" => Ok(IrValue::Bool(false)),
                "null" => Ok(IrValue::Null),
                _ => match env.lookup(&name) {
                    Some(v) => Ok(v),
                    // M2.6 Promise softening (mirror of the walker's Ident arms,
                    // `eval.rs:1117-1151`): an undefined identifier inside a
                    // Promise body — the sentinel partial failed to populate a
                    // `with`/lexical binding — softens to `null` so the fixpoint
                    // proceeds. Scoped strictly to Promise-body evaluation; plain
                    // code keeps the hard `UndefinedVar`.
                    None if in_promise_eval() => Ok(IrValue::Null),
                    None => Err(IrEvalError::UndefinedVar(name)),
                },
            }
        }
        Ir::Str(parts) => {
            let (s, ctx) = eval_str_parts_ctx(prog, parts, env)?;
            Ok(IrValue::string_with_context(s, ctx))
        }
        Ir::Path { kind, parts } => eval_path_parts(prog, *kind, parts, env),
        // `<name>` / `<name/sub>` — resolve via NIX_PATH exactly like the
        // walker's `PathSearch` arm: a hit is a `Path` value; a miss is a
        // CATCHABLE `Throw` (so `tryEval (import <x>)` mirrors CppNix). rnix
        // stores the token WITH its angle brackets, so strip them before
        // resolving (the walker does the same); the throw message keeps the
        // raw `<name>` wording, matching CppNix.
        Ir::SearchPath(raw) => {
            let name = raw
                .strip_prefix('<')
                .and_then(|s| s.strip_suffix('>'))
                .unwrap_or(raw);
            match crate::path::resolve_search_path(name) {
                Some(resolved) => Ok(IrValue::Path(Rc::new(resolved))),
                None => Err(IrEvalError::Throw(format!(
                    "search path '{raw}' not in NIX_PATH"
                ))),
            }
        }
        Ir::CurPos => Err(IrEvalError::Unsupported("__curPos")),
        Ir::LegacyLet { .. } => Err(IrEvalError::Unsupported("legacy-let")),
        Ir::Paren(inner) => eval_ir(prog, *inner, env),
        Ir::List(items) => Ok(IrValue::List(Rc::new(
            items
                .iter()
                .map(|item| IrValue::Thunk(IrThunk::suspended(prog.clone(), *item, env.clone())))
                .collect(),
        ))),
        Ir::Lambda { param, body } => Ok(IrValue::Lambda(Rc::new(IrClosure {
            prog: prog.clone(),
            param: param.clone(),
            body: *body,
            env: env.clone(),
        }))),
        Ir::Apply { func, arg } => {
            let f = eval_ir(prog, *func, env)?.force()?;
            // Call-by-need: ALWAYS thunk the argument (the walker does the
            // same); `apply` forces it for builtins that want a forced arg.
            let arg_value = IrValue::Thunk(IrThunk::suspended(prog.clone(), *arg, env.clone()));
            apply(f, arg_value)
        }
        Ir::IfElse {
            condition,
            then_body,
            else_body,
        } => {
            if eval_ir(prog, *condition, env)?.force()?.as_bool()? {
                eval_ir(prog, *then_body, env)
            } else {
                eval_ir(prog, *else_body, env)
            }
        }
        Ir::Assert { condition, body } => {
            if eval_ir(prog, *condition, env)?.force()?.as_bool()? {
                eval_ir(prog, *body, env)
            } else {
                Err(IrEvalError::AssertionFailed)
            }
        }
        Ir::With { namespace, body } => {
            // Namespace stays lazy — forced only when an ident lookup falls
            // through lexical scope (the walker's M2.6 ROOT #4a fix).
            let scope = IrValue::Thunk(IrThunk::suspended(prog.clone(), *namespace, env.clone()));
            let new_env = env.child().with_scope(scope);
            eval_ir(prog, *body, &new_env)
        }
        Ir::UnaryOp { op, expr } => {
            let v = eval_ir(prog, *expr, env)?.force()?;
            match op {
                UnaryOp::Negate => match v {
                    IrValue::Int(n) => Ok(IrValue::Int(-n)),
                    IrValue::Float(f) => Ok(IrValue::Float(-f)),
                    other => Err(IrEvalError::TypeError(format!(
                        "cannot negate {}",
                        other.type_name()
                    ))),
                },
                UnaryOp::Invert => Ok(IrValue::Bool(!v.as_bool()?)),
            }
        }
        Ir::BinOp { op, lhs, rhs } => eval_binop(prog, *op, *lhs, *rhs, env),
        Ir::Select {
            subject,
            path,
            or_default,
        } => eval_select(prog, *subject, path, *or_default, env),
        Ir::HasAttr { subject, path } => {
            let base = eval_ir(prog, *subject, env)?.force()?;
            match traverse_attrpath(prog, base, path, env)? {
                Traverse::Found(_) => Ok(IrValue::Bool(true)),
                Traverse::Missing(_) | Traverse::NotAttrs => Ok(IrValue::Bool(false)),
            }
        }
        Ir::LetIn { bindings, body } => {
            let new_env = eval_let_bindings(prog, bindings, env)?;
            eval_ir(prog, *body, &new_env)
        }
        Ir::AttrSet { rec, bindings } => eval_attrset(prog, *rec, bindings, env),
    }
}

// ── strings + attr names ──────────────────────────────────────────────────

/// Evaluate normalized string parts to the concatenated content. Mirrors
/// the walker's `eval_str`: interpolations force + **copy-to-store**
/// coerce — for every value the pure subset can hold, that coincides with
/// plain coercion EXCEPT path values, which the walker NAR-copies into the
/// store; that store reach is a typed gap here
/// (`Unsupported("path-copy-to-store")`).
fn eval_str_parts(
    prog: &Rc<Program>,
    parts: &[StrPart],
    env: &IrEnv,
) -> Result<String, IrEvalError> {
    eval_str_parts_ctx(prog, parts, env).map(|(s, _)| s)
}

/// Context-tracking string interpolation — the mirror of the walker's
/// interpolation, which merges every interpolated part's context into the
/// resulting string. An interpolated `${derivation}` coerces to its `.outPath`
/// (carrying an `Output{drv,output}` element); that element rides the produced
/// string so a consuming derivation can rediscover the dependency edge and
/// populate `inputDrvs`. Splices use copy-to-store coercion (`true`), exactly
/// like the walker's string interpolation.
fn eval_str_parts_ctx(
    prog: &Rc<Program>,
    parts: &[StrPart],
    env: &IrEnv,
) -> Result<(String, IrStringContext), IrEvalError> {
    let mut out = String::new();
    let mut ctx = IrStringContext::new();
    for part in parts {
        match part {
            StrPart::Literal(text) => out.push_str(text),
            StrPart::Interp(e) => {
                let v = eval_ir(prog, *e, env)?.force()?;
                let (s, c) = coerce_to_string_ctx(&v, true)?;
                out.push_str(&s);
                ctx.merge(&c);
            }
        }
    }
    Ok((out, ctx))
}

/// Evaluate a path literal (plain or interpolated), mirroring the walker's
/// `PathAbs`/`PathRel`/`PathHome` arms + `eval_interpol_path_parts`:
/// interpolations splice with PLAIN coercion (a path-typed splice inserts
/// the raw path), then the concatenated text resolves by kind —
/// absolute → `canon_abs`; relative → joined + normalized against the
/// current eval dir (raw text when there is none, e.g. top-level
/// expression eval); home → raw text when plain, `normalize` when
/// interpolated (the walker's exact asymmetry).
fn eval_path_parts(
    prog: &Rc<Program>,
    kind: PathKind,
    parts: &[PathPart],
    env: &IrEnv,
) -> Result<IrValue, IrEvalError> {
    let has_interp = parts.iter().any(|p| matches!(p, PathPart::Interp(_)));
    let mut text = String::new();
    for part in parts {
        match part {
            PathPart::Literal(t) => text.push_str(t),
            PathPart::Interp(e) => {
                let v = eval_ir(prog, *e, env)?.force()?;
                text.push_str(&coerce_to_string_impl(&v, false)?);
            }
        }
    }
    let resolved = match kind {
        PathKind::Abs => crate::path::canon_abs(&text),
        PathKind::Rel => match current_eval_dir() {
            Some(dir) => crate::path::normalize(&dir.join(&text))
                .to_string_lossy()
                .into_owned(),
            None => text,
        },
        PathKind::Home => {
            if has_interp {
                crate::path::normalize(std::path::Path::new(&text))
                    .to_string_lossy()
                    .into_owned()
            } else {
                text
            }
        }
    };
    Ok(IrValue::Path(Rc::new(resolved)))
}

/// The walker's PLAIN string coercion (`Value::coerce_to_string`) — used
/// by `toString`, path interpolation, `concatStringsSep`,
/// `replaceStrings`, and `+` attrs coercion. A path splices its raw
/// string.
pub(crate) fn coerce_to_string_plain(v: &IrValue) -> Result<String, IrEvalError> {
    coerce_to_string_impl(v, false)
}

/// String-only façade over [`coerce_to_string_ctx`] for the many callers that
/// don't need the context (`toString`, `concatStringsSep`, `replaceStrings`,
/// `+` attrs coercion, path interpolation). Same behavior as before this
/// slice; the context is computed and discarded.
fn coerce_to_string_impl(v: &IrValue, copy_to_store: bool) -> Result<String, IrEvalError> {
    coerce_to_string_ctx(v, copy_to_store).map(|(s, _)| s)
}

/// The walker's `coerce_to_string_impl`, context-preserving. `copy_to_store`
/// mirrors the mode split. This is the SINGLE context sink — every string that
/// acquires context (a `${derivation}` output ref, a plain-mode path's own
/// store-path ref) does so here, exactly as the walker does, so an interpolated
/// derivation output flows its `Output{drv,output}` edge into the consumer.
///
/// The one deliberate divergence is a typed gap, not a silent one:
/// `copy_to_store` of a **`Path` value** (`src = ./.`) NAR-copies the tree into
/// the store in the walker; the pure IR engine has no store, so it stays
/// `Unsupported("path-copy-to-store")` (a `src = ./.` derivation is a reported
/// gap, not a wrong answer). Plain-mode path coercion mirrors the walker's
/// `add_plain(raw)`.
pub(crate) fn coerce_to_string_ctx(
    v: &IrValue,
    copy_to_store: bool,
) -> Result<(String, IrStringContext), IrEvalError> {
    let mut ctx = IrStringContext::new();
    let s = match v {
        IrValue::Str(s, c) => {
            if let Some(c) = c {
                ctx.merge(c);
            }
            (**s).clone()
        }
        IrValue::Path(p) => {
            if copy_to_store {
                // CppNix copy-to-store coercion (mirror of the walker's
                // `value.rs` Path arm): resolve to canonical-absolute, require
                // it exists, NAR-hash the tree, reference the store path. The
                // walker's flake `-source` redirect (`materialize` /
                // `source_name_for_read_dir`) is omitted — a NIX_PATH nixpkgs
                // is not a fetched flake input, so `materialize` is identity
                // and the name is the real basename; the store path is
                // NAR-content-addressed, so byte-identical given the same tree.
                let raw: &str = p;
                let pb = std::path::Path::new(raw);
                let abs = if pb.is_absolute() {
                    pb.to_path_buf()
                } else if let Some(dir) = current_eval_dir() {
                    dir.join(pb)
                } else {
                    std::env::current_dir()
                        .map_err(|e| IrEvalError::Io {
                            context: {
                                let mut s = String::from("copy-to-store coercion of ");
                                s.push_str(raw);
                                s
                            },
                            message: e.to_string(),
                        })?
                        .join(pb)
                };
                let canon = abs.canonicalize().map_err(|_| {
                    IrEvalError::TypeError(format!("path '{}' does not exist", abs.display()))
                })?;
                let name = canon
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "source".to_string());
                let src = sui_compat::source::nar_hash_source_tree(&canon, &name)
                    .map_err(|e| {
                        IrEvalError::TypeError(format!(
                            "copy-to-store coercion of '{}': {e}",
                            canon.display()
                        ))
                    })?;
                ctx.add_plain(src.store_path.clone());
                src.store_path
            } else {
                // Walker plain-mode: the raw path IS its own context element.
                ctx.add_plain((**p).clone());
                (**p).clone()
            }
        }
        IrValue::Int(n) => n.to_string(),
        IrValue::Float(f) => format!("{f:.6}"),
        IrValue::Bool(true) => "1".to_string(),
        IrValue::Bool(false) | IrValue::Null => String::new(),
        IrValue::Attrs(attrs) => {
            if let Some(to_str) = attrs.get("__toString") {
                let r = apply(to_str.force()?, IrValue::Attrs(attrs.clone()))?.force()?;
                let (s, c) = coerce_to_string_ctx(&r, copy_to_store)?;
                ctx.merge(&c);
                s
            } else if let Some(out_path) = attrs.get("outPath") {
                let (s, c) = coerce_to_string_ctx(&out_path.force()?, copy_to_store)?;
                ctx.merge(&c);
                s
            } else {
                return Err(IrEvalError::TypeError(
                    "cannot coerce set to string (no __toString or outPath)".into(),
                ));
            }
        }
        IrValue::List(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items.iter() {
                let (s, c) = coerce_to_string_ctx(&item.force()?, copy_to_store)?;
                ctx.merge(&c);
                parts.push(s);
            }
            parts.join(" ")
        }
        IrValue::Thunk(_) => {
            let (s, c) = coerce_to_string_ctx(&v.force()?, copy_to_store)?;
            ctx.merge(&c);
            s
        }
        other => {
            return Err(IrEvalError::TypeError(format!(
                "cannot coerce {} to string",
                other.type_name()
            )))
        }
    };
    Ok((s, ctx))
}

/// Evaluate an attr name to a key string; `None` = null dynamic key (CppNix
/// skips the binding). Mirrors `eval_attr_maybe_null`: dynamic keys are
/// STRICT strings (no coercion), string keys go through interpolation.
fn eval_attr_maybe_null(
    prog: &Rc<Program>,
    attr: &AttrName,
    env: &IrEnv,
) -> Result<Option<String>, IrEvalError> {
    match attr {
        AttrName::Ident(sym) => Ok(Some(resolve(*sym))),
        AttrName::Str(parts) => Ok(Some(eval_str_parts(prog, parts, env)?)),
        AttrName::Dynamic(e) => {
            let v = eval_ir(prog, *e, env)?.force()?;
            match v {
                IrValue::Null => Ok(None),
                IrValue::Str(s, _) => Ok(Some((*s).clone())),
                other => Err(IrEvalError::TypeMismatch {
                    expected: "string",
                    got: other.type_name(),
                }),
            }
        }
    }
}

/// Strict attr-name evaluation (`let` paths, select/has-attr paths): a null
/// dynamic key is a type error, mirroring the walker's `eval_attr`.
fn eval_attr(prog: &Rc<Program>, attr: &AttrName, env: &IrEnv) -> Result<String, IrEvalError> {
    eval_attr_maybe_null(prog, attr, env)?
        .ok_or_else(|| IrEvalError::TypeError("null dynamic attribute name".into()))
}

// ── select / has-attr ─────────────────────────────────────────────────────

enum Traverse {
    Found(IrValue),
    Missing(String),
    NotAttrs,
}

fn traverse_attrpath(
    prog: &Rc<Program>,
    base: IrValue,
    path: &[AttrName],
    env: &IrEnv,
) -> Result<Traverse, IrEvalError> {
    let mut value = base;
    for (i, attr) in path.iter().enumerate() {
        let key = eval_attr(prog, attr, env)?;
        let forced = value.force()?;
        match forced {
            IrValue::Attrs(attrs) => match attrs.get(&key) {
                Some(v) => {
                    value = if i < path.len() - 1 {
                        v.force()?
                    } else {
                        v.clone()
                    };
                }
                None => return Ok(Traverse::Missing(key)),
            },
            _ => return Ok(Traverse::NotAttrs),
        }
    }
    Ok(Traverse::Found(value))
}

fn eval_select(
    prog: &Rc<Program>,
    subject: ExprId,
    path: &[AttrName],
    or_default: Option<ExprId>,
    env: &IrEnv,
) -> Result<IrValue, IrEvalError> {
    // Walker bridge: InfiniteRecursion while forcing the base falls back to
    // the default when one is present.
    let base = match eval_ir(prog, subject, env).and_then(|v| v.force()) {
        Ok(v) => v,
        Err(IrEvalError::InfiniteRecursion) if or_default.is_some() => {
            return eval_ir(prog, or_default.expect("checked"), env);
        }
        Err(e) => return Err(e),
    };
    let base_type = base.type_name();
    match traverse_attrpath(prog, base, path, env) {
        Ok(Traverse::Found(v)) => Ok(v),
        Ok(Traverse::Missing(key)) => match or_default {
            Some(def) => eval_ir(prog, def, env),
            None => Err(IrEvalError::AttrNotFound(key)),
        },
        Ok(Traverse::NotAttrs) => match or_default {
            Some(def) => eval_ir(prog, def, env),
            None => Err(IrEvalError::TypeError(format!(
                "cannot select from {base_type}"
            ))),
        },
        Err(IrEvalError::InfiniteRecursion) if or_default.is_some() => {
            eval_ir(prog, or_default.expect("checked"), env)
        }
        Err(e) => Err(e),
    }
}

// ── application ───────────────────────────────────────────────────────────

/// Apply a function value to an argument. Mirrors the walker's
/// `apply_inner`: lambdas bind call-by-need, pattern params force the arg,
/// `__functor` attrsets are supported, everything else is a type error.
pub fn apply(func: IrValue, arg: IrValue) -> Result<IrValue, IrEvalError> {
    let func = func.force()?;
    match func {
        IrValue::Lambda(closure) => {
            let mut call_env = closure.env.child();
            // Mirror the walker's `apply_inner`: re-enter the closure's
            // defining file so relative path literals in the body resolve
            // against it.
            let _file_guard = closure
                .env
                .eval_file()
                .map(|f| push_eval_file((*f).clone()));
            match &closure.param {
                Param::Ident(sym) => {
                    // Simple param: bind WITHOUT forcing (fixpoints).
                    call_env.bind_sym(*sym, arg);
                }
                Param::Pattern {
                    entries,
                    ellipsis,
                    bind,
                } => {
                    let forced = arg.force()?;
                    let IrValue::Attrs(attrs) = &forced else {
                        return Err(IrEvalError::TypeMismatch {
                            expected: "set",
                            got: forced.type_name(),
                        });
                    };
                    if let Some(bind_sym) = bind {
                        call_env.bind_sym(*bind_sym, forced.clone());
                    }
                    // Two-phase defaults: bind all formals first, then
                    // update default thunks to see the final env.
                    let mut default_thunks: Vec<IrThunk> = Vec::new();
                    for entry in entries {
                        let name = resolve(entry.name);
                        let value = if let Some(v) = attrs.get(&name) {
                            v.clone()
                        } else if let Some(default_expr) = entry.default {
                            let t = IrThunk::suspended(
                                closure.prog.clone(),
                                default_expr,
                                call_env.clone(),
                            );
                            default_thunks.push(t.clone());
                            IrValue::Thunk(t)
                        } else {
                            return Err(IrEvalError::TypeError(format!(
                                "missing argument '{name}'"
                            )));
                        };
                        call_env.bind_sym(entry.name, value);
                    }
                    for t in &default_thunks {
                        t.update_env(&call_env);
                    }
                    if !ellipsis {
                        let entry_names: HashSet<String> =
                            entries.iter().map(|e| resolve(e.name)).collect();
                        for key in attrs.keys() {
                            if !entry_names.contains(key) {
                                return Err(IrEvalError::TypeError(format!(
                                    "unexpected argument '{key}'"
                                )));
                            }
                        }
                    }
                }
            }
            eval_ir(&closure.prog, closure.body, &call_env)
        }
        IrValue::Builtin(kind, captured) => {
            // Mirror the walker: builtin args are chain-forced to WHNF
            // before the builtin runs — except the `seq<partial>` /
            // `deepSeq<partial>` stages, which receive the arg UNFORCED.
            let arg = if kind.wants_unforced_arg(captured.len()) {
                arg
            } else {
                arg.force()?
            };
            crate::builtins::apply_builtin(kind, &captured, arg)
        }
        IrValue::Attrs(attrs) => {
            if let Some(functor) = attrs.get("__functor") {
                let f = apply(functor.force()?, IrValue::Attrs(attrs.clone()))?;
                apply(f, arg)
            } else if in_promise_eval() {
                // M2.6 Promise softening (mirror of the walker's apply arm,
                // `eval.rs:3280-3285`): a functor-less attrset called as a
                // function inside a Promise body is the empty-attrs sentinel
                // landing where it doesn't belong — soften to `null`.
                Ok(IrValue::Null)
            } else {
                Err(IrEvalError::TypeError(
                    "attempt to call something which is not a function but a set".into(),
                ))
            }
        }
        // M2.6 Promise softening (mirror of `eval.rs:3292-3298`): calling
        // null/int/string/list/path as a function inside a Promise body is the
        // sentinel cascade — soften to `null` so the fixpoint continues.
        _ if in_promise_eval() => Ok(IrValue::Null),
        other => Err(IrEvalError::TypeError(format!(
            "attempt to call something which is not a function but a {}",
            other.type_name()
        ))),
    }
}

// ── binary operators ──────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn eval_binop(
    prog: &Rc<Program>,
    op: BinOp,
    lhs: ExprId,
    rhs: ExprId,
    env: &IrEnv,
) -> Result<IrValue, IrEvalError> {
    // Short-circuit forms mirror the walker exactly: LHS is forced +
    // bool-checked, the RHS is returned UNCHECKED (`false || 1` → `1`).
    match op {
        BinOp::And => {
            let l = eval_ir(prog, lhs, env)?.force()?.as_bool()?;
            return if l {
                eval_ir(prog, rhs, env)
            } else {
                Ok(IrValue::Bool(false))
            };
        }
        BinOp::Or => {
            let l = eval_ir(prog, lhs, env)?.force()?.as_bool()?;
            return if l {
                Ok(IrValue::Bool(true))
            } else {
                eval_ir(prog, rhs, env)
            };
        }
        BinOp::Implication => {
            let l = eval_ir(prog, lhs, env)?.force()?.as_bool()?;
            return if l {
                eval_ir(prog, rhs, env)
            } else {
                Ok(IrValue::Bool(true))
            };
        }
        _ => {}
    }

    let l = eval_ir(prog, lhs, env)?.force()?;
    let r = eval_ir(prog, rhs, env)?.force()?;

    match op {
        BinOp::Add => match (&l, &r) {
            (IrValue::Int(a), IrValue::Int(b)) => a
                .checked_add(*b)
                .map(IrValue::Int)
                .ok_or_else(|| int_overflow("adding", *a, '+', *b)),
            (IrValue::Float(a), IrValue::Float(b)) => Ok(IrValue::Float(a + b)),
            (IrValue::Int(a), IrValue::Float(b)) => Ok(IrValue::Float(*a as f64 + b)),
            (IrValue::Float(a), IrValue::Int(b)) => Ok(IrValue::Float(a + *b as f64)),
            (IrValue::Str(a, ca), IrValue::Str(b, cb)) => {
                let mut s = String::with_capacity(a.len() + b.len());
                s.push_str(a);
                s.push_str(b);
                // Walker `+` on strings unions both operands' context
                // (eval.rs String+String), so `"${pkg}" + "/bin"` keeps the
                // dependency edge and a concatenated derivation attr still
                // populates `inputDrvs`.
                let mut ctx = IrStringContext::new();
                if let Some(ca) = ca {
                    ctx.merge(ca);
                }
                if let Some(cb) = cb {
                    ctx.merge(cb);
                }
                Ok(IrValue::string_with_context(s, ctx))
            }
            // Walker: path + string concatenates raw; path + path joins
            // with a `/`. (string + path is NOT matched there — it falls
            // through to the type error, mirrored here.)
            (IrValue::Path(a), IrValue::Str(b, _)) => {
                Ok(IrValue::Path(Rc::new(format_concat(a, b))))
            }
            (IrValue::Path(a), IrValue::Path(b)) => {
                let mut s = String::with_capacity(a.len() + b.len() + 1);
                s.push_str(a);
                s.push('/');
                s.push_str(b);
                Ok(IrValue::Path(Rc::new(s)))
            }
            // Walker: attrsets coerce (outPath / __toString) on either
            // side, PLAIN mode.
            (IrValue::Attrs(_), _) | (_, IrValue::Attrs(_)) => {
                let ls = coerce_to_string_plain(&l)?;
                let rs = coerce_to_string_plain(&r)?;
                Ok(IrValue::string(format_concat(&ls, &rs)))
            }
            _ => Err(op_type_error("add", &l, &r)),
        },
        BinOp::Sub => num_op(&l, &r, i64::checked_sub, |a, b| a - b, "subtracting", '-'),
        BinOp::Mul => num_op(&l, &r, i64::checked_mul, |a, b| a * b, "multiplying", '*'),
        BinOp::Div => {
            let rhs_is_zero = match &r {
                IrValue::Int(0) => true,
                IrValue::Float(f) => *f == 0.0,
                _ => false,
            };
            if rhs_is_zero {
                return Err(IrEvalError::DivisionByZero);
            }
            num_op(&l, &r, i64::checked_div, |a, b| a / b, "dividing", '/')
        }
        // `ir_eq_operator`, NOT `ir_eq` — the operator proves distinct cells.
        BinOp::Equal => Ok(IrValue::Bool(ir_eq_operator(&l, &r))),
        BinOp::NotEqual => Ok(IrValue::Bool(!ir_eq_operator(&l, &r))),
        BinOp::Less => compare(&l, &r, |o| o == std::cmp::Ordering::Less),
        BinOp::LessOrEq => compare(&l, &r, |o| o != std::cmp::Ordering::Greater),
        BinOp::More => compare(&l, &r, |o| o == std::cmp::Ordering::Greater),
        BinOp::MoreOrEq => compare(&l, &r, |o| o != std::cmp::Ordering::Less),
        BinOp::Update => {
            let IrValue::Attrs(la) = &l else {
                return Err(IrEvalError::TypeMismatch {
                    expected: "set",
                    got: l.type_name(),
                });
            };
            let IrValue::Attrs(ra) = &r else {
                return Err(IrEvalError::TypeMismatch {
                    expected: "set",
                    got: r.type_name(),
                });
            };
            let mut merged = (**la).clone();
            for (k, v) in ra.iter() {
                merged.insert(k.clone(), v.clone());
            }
            Ok(IrValue::Attrs(Rc::new(merged)))
        }
        BinOp::Concat => {
            let IrValue::List(la) = &l else {
                return Err(IrEvalError::TypeMismatch {
                    expected: "list",
                    got: l.type_name(),
                });
            };
            let IrValue::List(ra) = &r else {
                return Err(IrEvalError::TypeMismatch {
                    expected: "list",
                    got: r.type_name(),
                });
            };
            let mut out = Vec::with_capacity(la.len() + ra.len());
            out.extend(la.iter().cloned());
            out.extend(ra.iter().cloned());
            Ok(IrValue::List(Rc::new(out)))
        }
        BinOp::And | BinOp::Or | BinOp::Implication => unreachable!("handled above"),
        BinOp::PipeRight | BinOp::PipeLeft => Err(IrEvalError::Unsupported("pipe-operators")),
    }
}

/// Byte-identical to `format!("{ls}{rs}")` without the format machinery.
fn format_concat(ls: &str, rs: &str) -> String {
    let mut s = String::with_capacity(ls.len() + rs.len());
    s.push_str(ls);
    s.push_str(rs);
    s
}

fn int_overflow(verb: &str, a: i64, sym: char, b: i64) -> IrEvalError {
    IrEvalError::Abort(format!("integer overflow in {verb} {a} {sym} {b}"))
}

fn op_type_error(op: &str, l: &IrValue, r: &IrValue) -> IrEvalError {
    IrEvalError::TypeError(format!(
        "cannot {op} {} and {}",
        l.type_name(),
        r.type_name()
    ))
}

fn num_op(
    l: &IrValue,
    r: &IrValue,
    int_op: impl Fn(i64, i64) -> Option<i64>,
    float_op: impl Fn(f64, f64) -> f64,
    verb: &'static str,
    sym: char,
) -> Result<IrValue, IrEvalError> {
    match (l, r) {
        (IrValue::Int(a), IrValue::Int(b)) => int_op(*a, *b)
            .map(IrValue::Int)
            .ok_or_else(|| int_overflow(verb, *a, sym, *b)),
        (IrValue::Float(a), IrValue::Float(b)) => Ok(IrValue::Float(float_op(*a, *b))),
        (IrValue::Int(a), IrValue::Float(b)) => Ok(IrValue::Float(float_op(*a as f64, *b))),
        (IrValue::Float(a), IrValue::Int(b)) => Ok(IrValue::Float(float_op(*a, *b as f64))),
        _ => Err(op_type_error("perform arithmetic on", l, r)),
    }
}

fn compare(
    l: &IrValue,
    r: &IrValue,
    pred: impl Fn(std::cmp::Ordering) -> bool,
) -> Result<IrValue, IrEvalError> {
    let ord = match (l, r) {
        (IrValue::Int(a), IrValue::Int(b)) => a.cmp(b),
        (IrValue::Float(a), IrValue::Float(b)) => {
            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
        }
        (IrValue::Int(a), IrValue::Float(b)) => (*a as f64)
            .partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal),
        (IrValue::Float(a), IrValue::Int(b)) => a
            .partial_cmp(&(*b as f64))
            .unwrap_or(std::cmp::Ordering::Equal),
        (IrValue::Str(a, _), IrValue::Str(b, _)) => a.cmp(b),
        _ => return Err(op_type_error("compare", l, r)),
    };
    Ok(IrValue::Bool(pred(ord)))
}

// ── equality (mirrors `Concrete::PartialEq` + `Value::PartialEq`) ─────────

/// Deep equality with the walker's exact semantics: operands are demanded
/// (a failed inner force compares as `null`), int/float cross-compare,
/// strings by content, lists elementwise, attrsets by key/value with the
/// derivation `outPath` short-circuit, lambdas by closure identity,
/// builtins never equal.
#[must_use]
pub fn ir_eq(l: &IrValue, r: &IrValue) -> bool {
    let l = l.force().unwrap_or(IrValue::Null);
    let r = r.force().unwrap_or(IrValue::Null);
    match (&l, &r) {
        (IrValue::Null, IrValue::Null) => true,
        (IrValue::Bool(a), IrValue::Bool(b)) => a == b,
        (IrValue::Int(a), IrValue::Int(b)) => a == b,
        (IrValue::Float(a), IrValue::Float(b)) => a == b,
        (IrValue::Int(a), IrValue::Float(b)) | (IrValue::Float(b), IrValue::Int(a)) => {
            (*a as f64) == *b
        }
        (IrValue::Str(a, _), IrValue::Str(b, _)) => a == b,
        // Walker `Concrete::PartialEq`: paths compare by string; a path
        // never equals a string.
        (IrValue::Path(a), IrValue::Path(b)) => a == b,
        (IrValue::List(a), IrValue::List(b)) => {
            Rc::ptr_eq(a, b) || (a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| ir_eq(x, y)))
        }
        (IrValue::Attrs(a), IrValue::Attrs(b)) => {
            if Rc::ptr_eq(a, b) {
                return true;
            }
            // Derivation short-circuit: both `type == "derivation"` with an
            // `outPath` compare by outPath string only.
            if let (Some(pa), Some(pb)) = (derivation_out_path(a), derivation_out_path(b)) {
                return pa == pb;
            }
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| b.get(k).is_some_and(|w| ir_eq(v, w)))
        }
        // Load-bearing; see `ir_eq_operator` for why it stays and why the
        // `==` operator must not use it.
        (IrValue::Lambda(a), IrValue::Lambda(b)) => Rc::ptr_eq(a, b),
        _ => false,
    }
}

/// Nix `==` / `!=` at the OPERATOR — the IR mirror of
/// `sui_eval::value::eq_operator`, and it must move in lockstep with it
/// (`tests/eval_differential.rs` compares the two engines row by row and this
/// exact expression, `let f = x: x; in f == f`, is one of the rows).
///
/// CppNix answers `false` for two functions at the top level because
/// `ExprOpEq::eval` gives each operand its own stack `Value`, so the
/// pointer-identity hack at the head of `eqValues` cannot fire. Nested, the
/// two really are one `Value*`, so it does fire and `[f] == [f]` is `true`.
/// `ir_eq` keeps the nested relation; this reproduces the top-level one.
#[must_use]
pub fn ir_eq_operator(l: &IrValue, r: &IrValue) -> bool {
    if let (Ok(IrValue::Lambda(_)), Ok(IrValue::Lambda(_))) = (l.force(), r.force()) {
        return false;
    }
    ir_eq(l, r)
}

fn derivation_out_path(attrs: &IrAttrs) -> Option<String> {
    let ty = attrs.get("type")?.force().ok()?;
    let IrValue::Str(s, _) = ty else { return None };
    if &**s != "derivation" {
        return None;
    }
    let out = attrs.get("outPath")?.force().ok()?;
    match out {
        IrValue::Str(p, _) => Some((*p).clone()),
        _ => None,
    }
}

// ── let / attrset construction ────────────────────────────────────────────

/// Every identifier SYMBOL referenced anywhere in the subtree of `id` —
/// the IR mirror of the walker's `referenced_idents` over-approximation.
/// Attrpath keys are naturally excluded (they are `AttrName`, not `Ident`
/// nodes). Used only to decide Promise-vs-Blackhole force mode.
fn referenced_idents(prog: &Program, id: ExprId, out: &mut HashSet<Symbol>) {
    if let Ir::Ident(sym) = prog.expr(id) {
        out.insert(*sym);
    }
    for child in prog.children(id) {
        referenced_idents(prog, child, out);
    }
}

/// Build the recursive `let` scope env. Mirrors the walker's LetIn arm:
/// two-phase thunk binding, dotted-path accumulation, eager bare inherit,
/// lazy `inherit (from)`.
fn eval_let_bindings(
    prog: &Rc<Program>,
    bindings: &[Binding],
    env: &IrEnv,
) -> Result<IrEnv, IrEvalError> {
    let mut new_env = env.child();
    let mut thunks: Vec<IrThunk> = Vec::new();
    let mut dotted_attrs = IrAttrs::new();

    // Pre-pass: every binding name in this let scope (head keys + inherit
    // names), errors ignored — used by the mutual-recursion detector.
    let mut let_scope_names: HashSet<String> = HashSet::new();
    for binding in bindings {
        match binding {
            Binding::Path { path, .. } => {
                if let Some(first) = path.first() {
                    if let Ok(Some(name)) = eval_attr_maybe_null(prog, first, env) {
                        let_scope_names.insert(name);
                    }
                }
            }
            Binding::Inherit { attrs, .. } => {
                for attr in attrs {
                    if let Ok(Some(name)) = eval_attr_maybe_null(prog, attr, env) {
                        let_scope_names.insert(name);
                    }
                }
            }
        }
    }

    for binding in bindings {
        match binding {
            Binding::Path { path, value } => {
                let mut path_keys = Vec::with_capacity(path.len());
                for attr in path {
                    path_keys.push(eval_attr(prog, attr, env)?);
                }
                if path_keys.len() == 1 {
                    let key = path_keys.pop().expect("len checked");
                    let mut referenced = HashSet::new();
                    referenced_idents(prog, *value, &mut referenced);
                    let in_mutual_cycle = std::iter::once(&key)
                        .chain(let_scope_names.iter())
                        .any(|n| referenced.contains(&intern(n)));
                    let thunk = if in_mutual_cycle {
                        IrThunk::suspended_recursive(prog.clone(), *value, env.clone())
                    } else {
                        IrThunk::suspended(prog.clone(), *value, env.clone())
                    };
                    thunks.push(thunk.clone());
                    new_env.bind(&key, IrValue::Thunk(thunk));
                } else {
                    let key = path_keys[0].clone();
                    let inner =
                        build_nested_attr_thunk(prog, &path_keys[1..], *value, env, &mut thunks);
                    merge_nested_insert(&mut dotted_attrs, key, inner)?;
                }
            }
            Binding::Inherit { from, attrs } => {
                if let Some(source_expr) = from {
                    let source_thunk =
                        IrThunk::suspended(prog.clone(), *source_expr, env.clone());
                    for attr in attrs {
                        let name = eval_attr(prog, attr, env)?;
                        let t = IrThunk::inherit_select(source_thunk.clone(), name.clone());
                        thunks.push(t.clone());
                        new_env.bind(&name, IrValue::Thunk(t));
                    }
                } else {
                    // Bare inherit in `let` is EAGER (walker: lookup or
                    // UndefinedVar at construction).
                    for attr in attrs {
                        let name = eval_attr(prog, attr, env)?;
                        let value = env
                            .lookup(&name)
                            .ok_or_else(|| IrEvalError::UndefinedVar(name.clone()))?;
                        new_env.bind(&name, value);
                    }
                }
            }
        }
    }

    for (key, value) in &dotted_attrs {
        new_env.bind(key, value.clone());
    }
    for thunk in &thunks {
        thunk.update_env(&new_env);
    }
    Ok(new_env)
}

/// Nested attrset with a (collected) thunk at the leaf — the walker's
/// `build_nested_attr_thunk` (leaf participates in the recursive fixpoint).
fn build_nested_attr_thunk(
    prog: &Rc<Program>,
    path: &[String],
    value: ExprId,
    env: &IrEnv,
    thunks: &mut Vec<IrThunk>,
) -> IrValue {
    if path.is_empty() {
        let t = IrThunk::suspended(prog.clone(), value, env.clone());
        thunks.push(t.clone());
        return IrValue::Thunk(t);
    }
    let inner = build_nested_attr_thunk(prog, &path[1..], value, env, thunks);
    let mut attrs = IrAttrs::new();
    attrs.insert(path[0].clone(), inner);
    IrValue::Attrs(Rc::new(attrs))
}

/// Nested attrset with a lazy (uncollected) leaf — the walker's
/// `build_nested_attr` used by non-rec attrsets.
fn build_nested_attr(prog: &Rc<Program>, path: &[String], value: ExprId, env: &IrEnv) -> IrValue {
    if path.is_empty() {
        return IrValue::Thunk(IrThunk::suspended(prog.clone(), value, env.clone()));
    }
    let inner = build_nested_attr(prog, &path[1..], value, env);
    let mut attrs = IrAttrs::new();
    attrs.insert(path[0].clone(), inner);
    IrValue::Attrs(Rc::new(attrs))
}

/// The walker's `merge_nested_insert`: deep-merge on collision when both
/// sides are attrset-shaped (forcing a colliding thunk to WHNF only), plain
/// last-write-wins otherwise.
fn merge_nested_insert(
    target: &mut IrAttrs,
    key: String,
    value: IrValue,
) -> Result<(), IrEvalError> {
    let Some(existing) = target.get(&key).cloned() else {
        target.insert(key, value);
        return Ok(());
    };
    // Normalize the NEW side: force a thunk to WHNF; keep as-is if the force
    // fails or yields a non-attrset (mirror: overwrite path).
    let value = match &value {
        IrValue::Thunk(_) => match value.force() {
            Ok(v @ IrValue::Attrs(_)) => v,
            _ => value,
        },
        _ => value,
    };
    if !matches!(value, IrValue::Attrs(_)) {
        target.insert(key, value);
        return Ok(());
    }
    let existing_concrete = match &existing {
        IrValue::Attrs(_) => existing.clone(),
        IrValue::Thunk(_) => match existing.force() {
            Ok(v @ IrValue::Attrs(_)) => v,
            _ => {
                target.insert(key, value);
                return Ok(());
            }
        },
        _ => {
            target.insert(key, value);
            return Ok(());
        }
    };
    let IrValue::Attrs(existing_rc) = existing_concrete else {
        unreachable!()
    };
    let IrValue::Attrs(new_rc) = value else {
        unreachable!()
    };
    let mut merged = (*existing_rc).clone();
    for (k, v) in new_rc.iter() {
        merge_nested_insert(&mut merged, k.clone(), v.clone())?;
    }
    target.insert(key, IrValue::Attrs(Rc::new(merged)));
    Ok(())
}

/// Force the deferred dynamic-tail attrpath: build `{ tail… = value }` in
/// the captured env (null dynamic keys skip, yielding `{ }` at that level).
fn build_tail_attrs(
    prog: &Rc<Program>,
    tail: &[AttrName],
    value: ExprId,
    env: &IrEnv,
) -> Result<IrValue, IrEvalError> {
    let Some((head, rest)) = tail.split_first() else {
        return eval_ir(prog, value, env).and_then(|v| v.force());
    };
    let mut attrs = IrAttrs::new();
    if let Some(key) = eval_attr_maybe_null(prog, head, env)? {
        let inner = if rest.is_empty() {
            IrValue::Thunk(IrThunk::suspended(prog.clone(), value, env.clone()))
        } else {
            IrValue::Thunk(IrThunk::deferred_tail(
                prog.clone(),
                rest.to_vec(),
                value,
                env.clone(),
            ))
        };
        attrs.insert(key, inner);
    }
    Ok(IrValue::Attrs(Rc::new(attrs)))
}

fn attrname_is_dynamic(attr: &AttrName) -> bool {
    match attr {
        AttrName::Ident(_) => false,
        AttrName::Dynamic(_) => true,
        AttrName::Str(parts) => parts.iter().any(|p| matches!(p, StrPart::Interp(_))),
    }
}

#[allow(clippy::too_many_lines)]
fn eval_attrset(
    prog: &Rc<Program>,
    rec: bool,
    bindings: &[Binding],
    env: &IrEnv,
) -> Result<IrValue, IrEvalError> {
    let mut attrs = IrAttrs::new();

    if rec {
        let mut rec_env = env.child();
        let mut thunks: Vec<IrThunk> = Vec::new();
        let mut defined_so_far: HashSet<String> = HashSet::new();
        let mut dotted_attrs = IrAttrs::new();

        for binding in bindings {
            match binding {
                Binding::Path { path, value } => {
                    let mut path_keys = Vec::with_capacity(path.len());
                    let mut skip = false;
                    for attr in path {
                        match eval_attr_maybe_null(prog, attr, env)? {
                            Some(k) => path_keys.push(k),
                            None => {
                                skip = true;
                                break;
                            }
                        }
                    }
                    if skip || path_keys.is_empty() {
                        continue;
                    }
                    if path_keys.len() == 1 {
                        let key = path_keys.pop().expect("len checked");
                        let mut referenced = HashSet::new();
                        referenced_idents(prog, *value, &mut referenced);
                        let is_recursive = referenced.contains(&intern(&key))
                            || defined_so_far.iter().any(|n| referenced.contains(&intern(n)));
                        let thunk = if is_recursive {
                            IrThunk::suspended_recursive(prog.clone(), *value, env.clone())
                        } else {
                            IrThunk::suspended(prog.clone(), *value, env.clone())
                        };
                        thunks.push(thunk.clone());
                        let v = IrValue::Thunk(thunk);
                        rec_env.bind(&key, v.clone());
                        attrs.insert(key.clone(), v);
                        defined_so_far.insert(key);
                    } else {
                        let key = path_keys[0].clone();
                        let inner = build_nested_attr_thunk(
                            prog,
                            &path_keys[1..],
                            *value,
                            env,
                            &mut thunks,
                        );
                        merge_nested_insert(&mut dotted_attrs, key, inner)?;
                    }
                }
                Binding::Inherit { from, attrs: names } => {
                    eval_inherit(
                        prog,
                        from.as_ref(),
                        names,
                        env,
                        &mut attrs,
                        Some(&mut rec_env),
                        Some(&mut thunks),
                    )?;
                }
            }
        }

        for (key, value) in &dotted_attrs {
            attrs.insert(key.clone(), value.clone());
            rec_env.bind(key, value.clone());
        }
        for thunk in &thunks {
            thunk.update_env(&rec_env);
        }
    } else {
        for binding in bindings {
            match binding {
                Binding::Path { path, value } => {
                    let tail_is_dynamic = path.len() > 1 && path[1..].iter().any(attrname_is_dynamic);
                    let Some(head_key) = eval_attr_maybe_null(prog, &path[0], env)? else {
                        continue;
                    };
                    if tail_is_dynamic && !attrs.contains_key(&head_key) {
                        // Deferred dynamic tail (walker's build_deferred_tail_attr).
                        let t = IrThunk::deferred_tail(
                            prog.clone(),
                            path[1..].to_vec(),
                            *value,
                            env.clone(),
                        );
                        attrs.insert(head_key, IrValue::Thunk(t));
                        continue;
                    }
                    if tail_is_dynamic {
                        // Collision under an existing head with a dynamic
                        // tail: force the deferred sub-attrs now and merge
                        // (outcome-equivalent to the walker's
                        // merge_deferred_dynamic_tail for the pure subset —
                        // the differential gates this).
                        let sub = build_tail_attrs(prog, &path[1..], *value, env)?;
                        merge_nested_insert(&mut attrs, head_key, sub)?;
                        continue;
                    }
                    // Static tail: evaluate remaining keys now (null skips).
                    let mut path_keys = vec![head_key];
                    let mut skip = false;
                    for attr in &path[1..] {
                        match eval_attr_maybe_null(prog, attr, env)? {
                            Some(k) => path_keys.push(k),
                            None => {
                                skip = true;
                                break;
                            }
                        }
                    }
                    if skip {
                        continue;
                    }
                    if path_keys.len() == 1 {
                        let key = path_keys.pop().expect("len checked");
                        let value =
                            IrValue::Thunk(IrThunk::suspended(prog.clone(), *value, env.clone()));
                        // Collision with an existing attrs → deep merge
                        // (force RHS to WHNF first), mirroring the walker.
                        if matches!(attrs.get(&key), Some(IrValue::Attrs(_))) {
                            let forced = value.force()?;
                            merge_nested_insert(&mut attrs, key, forced)?;
                        } else {
                            attrs.insert(key, value);
                        }
                    } else {
                        let key = path_keys[0].clone();
                        let value = build_nested_attr(prog, &path_keys[1..], *value, env);
                        // Existing full-set thunk at the head: force to WHNF
                        // so the merge sees concrete attrs.
                        if matches!(attrs.get(&key), Some(IrValue::Thunk(_))) {
                            let existing = attrs.get(&key).cloned().expect("just matched");
                            let forced = existing.force()?;
                            attrs.insert(key.clone(), forced);
                        }
                        merge_nested_insert(&mut attrs, key, value)?;
                    }
                }
                Binding::Inherit { from, attrs: names } => {
                    eval_inherit(prog, from.as_ref(), names, env, &mut attrs, None, None)?;
                }
            }
        }
    }

    Ok(IrValue::Attrs(Rc::new(attrs)))
}

/// The walker's `eval_inherit`: `inherit (from) …` builds one shared source
/// thunk + per-name `InheritSelect` thunks; bare `inherit …` resolves
/// lexically, deferring to a `WithIdent` thunk when a `with` scope exists.
fn eval_inherit(
    prog: &Rc<Program>,
    from: Option<&ExprId>,
    names: &[AttrName],
    env: &IrEnv,
    attrs: &mut IrAttrs,
    mut bind_env: Option<&mut IrEnv>,
    mut thunks: Option<&mut Vec<IrThunk>>,
) -> Result<(), IrEvalError> {
    if let Some(source_expr) = from {
        let source_thunk = IrThunk::suspended(prog.clone(), *source_expr, env.clone());
        for attr in names {
            let name = eval_attr(prog, attr, env)?;
            let t = IrThunk::inherit_select(source_thunk.clone(), name.clone());
            let value = IrValue::Thunk(t.clone());
            attrs.insert(name.clone(), value.clone());
            if let Some(e) = bind_env.as_deref_mut() {
                e.bind(&name, value);
            }
            if let Some(ts) = thunks.as_deref_mut() {
                ts.push(t);
            }
        }
    } else {
        for attr in names {
            let name = eval_attr(prog, attr, env)?;
            // Walker order: full lookup (lexical + with-scopes) first; only
            // a MISS with a `with` scope present defers to a WithIdent thunk.
            let value = if let Some(v) = env.lookup(&name) {
                v
            } else if env.has_with_scope() {
                IrValue::Thunk(IrThunk::with_ident(name.clone(), env.clone()))
            } else {
                return Err(IrEvalError::UndefinedVar(name));
            };
            attrs.insert(name.clone(), value.clone());
            if let Some(e) = bind_env.as_deref_mut() {
                e.bind(&name, value);
            }
        }
    }
    Ok(())
}

// ── unit tests (IR-side only; the cross-engine differential lives in
//    tests/eval_differential.rs) ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lower_file;

    fn ev(src: &str) -> Result<IrValue, IrEvalError> {
        let prog = Rc::new(lower_file(src).expect("lowers"));
        let env = IrEnv::with_pure_builtins();
        eval_ir(&prog, prog.root, &env).and_then(|v| v.force())
    }

    fn ev_int(src: &str) -> i64 {
        match ev(src) {
            Ok(IrValue::Int(n)) => n,
            other => panic!("expected int for {src:?}, got {other:?}"),
        }
    }

    fn ev_str(src: &str) -> String {
        match ev(src) {
            Ok(IrValue::Str(s, _)) => (*s).clone(),
            other => panic!("expected string for {src:?}, got {other:?}"),
        }
    }

    #[test]
    fn literals() {
        assert_eq!(ev_int("42"), 42);
        assert!(matches!(ev("1.5"), Ok(IrValue::Float(f)) if (f - 1.5).abs() < f64::EPSILON));
        assert!(matches!(ev("true"), Ok(IrValue::Bool(true))));
        assert!(matches!(ev("null"), Ok(IrValue::Null)));
        assert_eq!(ev_str("\"hi\""), "hi");
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(ev_int("1 + 2 * 3"), 7);
        assert_eq!(ev_int("(1 + 2) * 3"), 9);
        assert!(matches!(ev("1 / 0"), Err(IrEvalError::DivisionByZero)));
        assert!(matches!(
            ev("9223372036854775807 + 1"),
            Err(IrEvalError::Abort(_))
        ));
    }

    #[test]
    fn short_circuit_mirrors_walker() {
        // The walker returns the RHS unchecked: `false || 1` is Int(1).
        assert_eq!(ev_int("false || 1"), 1);
        assert_eq!(ev_int("true && 1"), 1);
        assert_eq!(ev_int("true -> 1"), 1);
        assert!(matches!(ev("1 && true"), Err(IrEvalError::TypeMismatch { .. })));
    }

    #[test]
    fn let_lambda_apply() {
        assert_eq!(ev_int("let f = x: x + 1; in f 41"), 42);
        assert_eq!(ev_int("let f = { a ? 3 }: a; in f { }"), 3);
        assert_eq!(ev_int("(args @ { a, ... }: args.a) { a = 5; b = 6; }"), 5);
        // Unused broken binding never forces.
        assert_eq!(ev_int("let boom = 1 / 0; in 7"), 7);
    }

    #[test]
    fn attrsets_select_hasattr() {
        assert_eq!(ev_int("{ a = { b = 2; }; }.a.b"), 2);
        assert_eq!(ev_int("{ a = 1; }.b or 9"), 9);
        assert_eq!(ev_int("rec { a = b; b = 3; }.a"), 3);
        assert!(matches!(ev("{ a = 1; } ? a"), Ok(IrValue::Bool(true))));
        assert!(matches!(ev("1 ? a"), Ok(IrValue::Bool(false))));
        assert_eq!(ev_int("let s = { a.b = 1; a = { c = 2; }; }; in s.a.b + s.a.c"), 3);
    }

    #[test]
    fn with_and_inherit() {
        assert_eq!(ev_int("with { a = 1; }; a"), 1);
        assert_eq!(ev_int("let a = 2; in with { a = 1; }; a"), 2); // lexical wins
        assert_eq!(ev_int("let a = 4; s = { inherit a; }; in s.a"), 4);
        assert_eq!(ev_int("let k = { x = 8; }; in (rec { inherit (k) x; y = x; }).y"), 8);
    }

    #[test]
    fn string_interp_and_tostring() {
        assert_eq!(ev_str(r#"let x = "b"; in "a${x}c""#), "abc");
        assert_eq!(ev_str(r#""n=${toString 1}""#), "n=1");
        // Walker coercion quirks mirrored.
        assert_eq!(ev_str(r#""${1}""#), "1");
        assert_eq!(ev_str(r#""${1.5}""#), "1.500000");
    }

    #[test]
    fn self_alias_is_infinite_recursion_like_walker() {
        // The walker's force chain hits its depth guard on the direct
        // self-alias (the thunk memoizes to itself); mirror the class.
        assert!(matches!(
            ev("let x = x; in x"),
            Err(IrEvalError::InfiniteRecursion)
        ));
        // Mutual aliasing is the same cycle one step longer.
        assert!(matches!(
            ev("let a = b; b = a; in a"),
            Err(IrEvalError::InfiniteRecursion)
        ));
    }

    #[test]
    fn typed_gaps() {
        // Search paths now RESOLVE (slice 4): a name absent from NIX_PATH is
        // a catchable `Throw` (mirroring the walker), never `Unsupported`.
        // Use a name that cannot plausibly be in a NIX_PATH so the assertion
        // is host-stable.
        assert!(matches!(
            ev("<sui-ir-slice4-definitely-absent>"),
            Err(IrEvalError::Throw(_))
        ));
        assert!(matches!(
            ev("let { body = 1; }"),
            Err(IrEvalError::Unsupported("legacy-let"))
        ));
        assert!(matches!(
            ev("__curPos"),
            Err(IrEvalError::Unsupported("__curPos"))
        ));
        // `sort` is now IMPLEMENTED (slice 4) — it evaluates, no longer a gap.
        assert!(matches!(
            ev("builtins.sort (a: b: a < b) [ 2 1 ]"),
            Ok(IrValue::List(_))
        ));
        // `derivation` is IMPLEMENTED (slice 6): `derivation {}` now reaches
        // the impl and fails on the missing required `name` attr (an
        // `AttrNotFound`), NOT a `MissingBuiltin` gap. A well-formed leaf
        // derivation produces a `derivation`-typed attrset (see
        // `tests/derivation_milestone.rs` for the three-way drvPath match).
        assert!(matches!(
            ev("derivation { }"),
            Err(IrEvalError::AttrNotFound(n)) if n == "name"
        ));
        assert!(matches!(
            ev("(derivation { name = \"x\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }).type"),
            Ok(IrValue::Str(s, _)) if *s == "derivation"
        ));
        // Slice 7: `getEnv` is IMPLEMENTED (reads the process env like the
        // walker) — an unset var is the empty string on both engines.
        assert!(matches!(
            ev("builtins.getEnv \"SUI_IR_SURELY_UNSET_VAR_XYZ\""),
            Ok(IrValue::Str(s, _)) if s.is_empty()
        ));
        // Slice 7: copy-to-store path coercion (`"${./f}"`) is IMPLEMENTED
        // (NAR-hashes the source tree via `sui_compat::source`), so it is no
        // longer the `Unsupported("path-copy-to-store")` gap — it now attempts
        // the real store copy and, for a nonexistent path, errors byte-for-byte
        // like the walker (`tests/probe_hello`-verified against the walker).
        assert!(!matches!(
            ev(r#""${./x}""#),
            Err(IrEvalError::Unsupported("path-copy-to-store"))
        ));
        // TYPED KNOWN GAP (position-less IR value model): `unsafeGetAttrPos` on a
        // SOURCE-LITERAL attr returns `null` here, where the walker/CppNix return
        // the attr's `{file,line,column}`. Named + asserted, never silent; it
        // never enters a drvPath (a position byte is not part of derivation
        // hashing), so a real hello.drvPath is reachable through it. See the
        // `builtins::UnsafeGetAttrPos` arm.
        assert!(matches!(
            ev("builtins.unsafeGetAttrPos \"a\" { a = 1; }"),
            Ok(IrValue::Null)
        ));
    }

    #[test]
    fn paths_evaluate_like_the_walker() {
        // No eval-file context: relative + home stay raw; absolute
        // canonicalizes CppNix-style.
        assert!(matches!(ev("./x"), Ok(IrValue::Path(p)) if *p == "./x"));
        assert!(matches!(ev("~/dir/file"), Ok(IrValue::Path(p)) if *p == "~/dir/file"));
        assert!(matches!(ev("/foo/../bar"), Ok(IrValue::Path(p)) if *p == "/bar"));
        assert!(matches!(ev("/.."), Ok(IrValue::Path(p)) if *p == "/"));
        // Interpolation splices with PLAIN coercion; `//` seams collapse.
        assert!(
            matches!(ev("toString /bar/${/tmp/foo}"), Ok(IrValue::Str(s, _)) if *s == "/bar/tmp/foo")
        );
        assert_eq!(ev_str(r#"let x = "foo"; in toString /a/${x}/b"#), "/a/foo/b");
        // Path arithmetic mirrors the walker's Add arms.
        assert!(matches!(ev(r#"./x + "/y""#), Ok(IrValue::Path(p)) if *p == "./x/y"));
        assert!(matches!(ev("/a + /b"), Ok(IrValue::Path(p)) if *p == "/a//b"));
        assert!(matches!(ev(r#""s" + /a"#), Err(IrEvalError::TypeError(_))));
        // typeOf + equality.
        assert_eq!(ev_str("builtins.typeOf ./x"), "path");
        assert!(matches!(ev("/a/b == /a/b"), Ok(IrValue::Bool(true))));
        assert!(matches!(ev(r#"/a/b == "/a/b""#), Ok(IrValue::Bool(false))));
    }

    #[test]
    fn builtins_bridge_basics() {
        assert_eq!(ev_int("builtins.length [ 1 2 3 ]"), 3);
        assert_eq!(ev_str("builtins.concatStringsSep \"-\" [ \"a\" \"b\" ]"), "a-b");
        assert_eq!(ev_int("builtins.foldl' (a: b: a + b) 0 [ 1 2 3 ]"), 6);
        assert_eq!(ev_str("builtins.substring 1 2 \"abcd\""), "bc");
        assert!(matches!(ev("builtins ? sort"), Ok(IrValue::Bool(true))));
        // The walker's snapshot: `builtins.builtins` exists but does not
        // contain `builtins` itself.
        assert!(matches!(ev("builtins ? builtins"), Ok(IrValue::Bool(true))));
        assert!(matches!(ev("builtins.builtins ? builtins"), Ok(IrValue::Bool(false))));
    }

    #[test]
    fn map_is_lazy_and_correct() {
        let v = ev("map (x: x + 1) [ 1 2 3 ]").expect("maps");
        let IrValue::List(items) = v else { panic!("expected list") };
        let forced: Vec<i64> = items
            .iter()
            .map(|v| match v.force().expect("forces") {
                IrValue::Int(n) => n,
                other => panic!("expected int, got {other:?}"),
            })
            .collect();
        assert_eq!(forced, vec![2, 3, 4]);
    }
}
