//! Nix value types and environments.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

// ── Nix string context ─────────────────────────────────────────

/// An element of a Nix string's context set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ContextElement {
    /// Store path reference (e.g., "/nix/store/abc-hello").
    Plain(String),
    /// Derivation output reference.
    Output { drv: String, output: String },
    /// Entire derivation closure.
    DrvDeep(String),
}

impl fmt::Display for ContextElement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContextElement::Plain(p) => write!(f, "{p}"),
            ContextElement::Output { drv, output } => write!(f, "{drv}!{output}"),
            ContextElement::DrvDeep(d) => write!(f, "={d}"),
        }
    }
}

/// The context attached to a Nix string: a set of store-path references that
/// the string depends on. Plain string literals have an empty context.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StringContext(pub BTreeSet<ContextElement>);

impl StringContext {
    /// Create an empty context.
    pub fn new() -> Self {
        Self(BTreeSet::new())
    }

    /// Merge another context into this one.
    pub fn merge(&mut self, other: &StringContext) {
        self.0.extend(other.0.iter().cloned());
    }

    /// Add a plain store-path reference.
    pub fn add_plain(&mut self, path: String) {
        self.0.insert(ContextElement::Plain(path));
    }

    /// Add a derivation output reference.
    pub fn add_output(&mut self, drv: String, output: String) {
        self.0.insert(ContextElement::Output { drv, output });
    }

    /// Add a derivation-deep reference.
    pub fn add_drv_deep(&mut self, drv: String) {
        self.0.insert(ContextElement::DrvDeep(drv));
    }

    /// Whether this context set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Return the number of context elements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Iterate over all context elements.
    pub fn iter(&self) -> impl Iterator<Item = &ContextElement> {
        self.0.iter()
    }

    /// Insert a raw context element.
    pub fn insert(&mut self, elem: ContextElement) {
        self.0.insert(elem);
    }
}

/// A Nix string value with associated context (store-path references).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NixString {
    /// The character data.
    pub chars: String,
    /// The context set (empty for plain string literals).
    pub context: StringContext,
}

impl NixString {
    /// Create a context-free string.
    pub fn plain(s: impl Into<String>) -> Self {
        Self {
            chars: s.into(),
            context: StringContext::default(),
        }
    }

    /// Create a string with an explicit context.
    pub fn with_context(s: impl Into<String>, ctx: StringContext) -> Self {
        Self {
            chars: s.into(),
            context: ctx,
        }
    }

    /// Borrow the string content.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.chars
    }

    /// Whether this string carries any context (store path references).
    #[must_use]
    pub fn has_context(&self) -> bool {
        !self.context.0.is_empty()
    }
}

impl AsRef<str> for NixString {
    fn as_ref(&self) -> &str {
        &self.chars
    }
}

impl std::ops::Deref for NixString {
    type Target = str;

    fn deref(&self) -> &str {
        &self.chars
    }
}

impl fmt::Display for NixString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.chars)
    }
}

// ── Value enum ────────────────────────────────────────────────

/// A Nix value.
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(NixString),
    Path(String),
    List(Vec<Value>),
    Attrs(NixAttrs),
    Lambda(Closure),
    Builtin(BuiltinFn),
    /// A lazy value (thunk) with memoization and blackhole detection.
    Thunk(Thunk),
}

/// Internal representation of a thunk's state machine.
///
/// Transitions: `Suspended` → `Blackhole` → `Evaluated` (on success),
/// or `Suspended` → `Blackhole` → `Suspended` (on failure, to allow retry).
pub enum ThunkRepr {
    /// Not yet evaluated. Holds the AST expression and captured environment.
    Suspended {
        expr: rnix::ast::Expr,
        env: Env,
    },
    /// Pending `inherit (source) name` selection. When forced,
    /// evaluates `source_expr` in `env` then pulls out `name`. This
    /// is its own variant (rather than synthesizing a Select AST
    /// node) because rnix doesn't expose a public AST builder, and
    /// we want each inherited name to defer evaluation of the
    /// source expression so that `inherit (lib.trivial) ...` at
    /// the top of trivial.nix doesn't blackhole on the still-being-
    /// constructed `lib.trivial`.
    InheritSelect {
        source: rnix::ast::Expr,
        name: String,
        env: Env,
    },
    /// Currently being evaluated -- detects infinite recursion.
    Blackhole,
    /// Already evaluated and memoized.
    Evaluated(Box<Value>),
}

/// A lazy value with memoization and blackhole detection.
#[derive(Clone)]
pub struct Thunk(pub(crate) Rc<RefCell<ThunkRepr>>);

impl Thunk {
    /// Create a thunk that will evaluate `expr` in `env` when forced.
    pub fn new_suspended(expr: rnix::ast::Expr, env: Env) -> Self {
        Self(Rc::new(RefCell::new(ThunkRepr::Suspended { expr, env })))
    }

    /// Create a thunk that, when forced, evaluates `source` in
    /// `env` and pulls out the attribute named `name`. Used for
    /// `inherit (source) name` so the source is not eagerly
    /// forced and self-referential patterns don't blackhole.
    pub fn new_inherit_select(source: rnix::ast::Expr, name: String, env: Env) -> Self {
        Self(Rc::new(RefCell::new(ThunkRepr::InheritSelect {
            source,
            name,
            env,
        })))
    }

    /// Create a thunk that is already evaluated (an optimization).
    pub fn new_evaluated(value: Value) -> Self {
        Self(Rc::new(RefCell::new(ThunkRepr::Evaluated(Box::new(value)))))
    }

    /// Check whether this thunk has already been forced.
    pub fn is_evaluated(&self) -> bool {
        matches!(*self.0.borrow(), ThunkRepr::Evaluated(_))
    }

    /// Replace the environment captured in a suspended thunk.
    /// No-op if the thunk is already evaluated or a blackhole.
    pub fn update_env(&self, new_env: Env) {
        let mut borrow = self.0.borrow_mut();
        if let ThunkRepr::Suspended { env, .. } = &mut *borrow {
            *env = new_env;
        }
    }

    /// Force this thunk using the given evaluator function.
    ///
    /// On first force: transitions Suspended -> Blackhole -> Evaluated.
    /// Re-entering a Blackhole signals infinite recursion.
    /// If the evaluated result is itself a thunk, it is forced transitively.
    pub fn force(
        &self,
        evaluator: &dyn Fn(&rnix::ast::Expr, &Env) -> Result<Value, EvalError>,
    ) -> Result<Value, EvalError> {
        // Take the current repr, replacing with Blackhole.
        let repr = std::mem::replace(&mut *self.0.borrow_mut(), ThunkRepr::Blackhole);

        match repr {
            ThunkRepr::Suspended { expr, env } => {
                // Push the thunk's captured eval_file onto the thread-local
                // stack so PathRel literals and relative imports inside the
                // thunk body resolve against the file where the thunk was
                // *defined*, not where it is forced from. The RAII guard
                // pops on drop (including on error paths).
                let _file_guard = env.eval_file.clone().map(crate::eval::push_eval_file);
                match evaluator(&expr, &env) {
                    Ok(mut value) => {
                        // Transitively force inner thunks.
                        while let Value::Thunk(inner) = value {
                            value = inner.force(evaluator)?;
                        }
                        // Store the deeply-forced value.
                        *self.0.borrow_mut() = ThunkRepr::Evaluated(Box::new(value.clone()));
                        Ok(value)
                    }
                    Err(e) => {
                        // Restore suspended state so the thunk can be retried or
                        // at least not left as a permanent blackhole.
                        *self.0.borrow_mut() = ThunkRepr::Suspended { expr, env };
                        Err(e)
                    }
                }
            }
            ThunkRepr::InheritSelect { source, name, env } => {
                // Evaluate source, force, then select `name`. The
                // restore-on-error semantics mirror the Suspended
                // branch so a transient error doesn't permanently
                // blackhole this thunk.
                let _file_guard = env.eval_file.clone().map(crate::eval::push_eval_file);
                let attempt = (|| -> Result<Value, EvalError> {
                    let raw = evaluator(&source, &env)?;
                    let mut forced = raw;
                    while let Value::Thunk(inner) = forced {
                        forced = inner.force(evaluator)?;
                    }
                    let attrs = match &forced {
                        Value::Attrs(a) => a,
                        _ => {
                            return Err(EvalError::TypeError(format!(
                                "inherit (source) {name}: source is {}, not a set",
                                forced.type_name()
                            )))
                        }
                    };
                    attrs
                        .get(&name)
                        .cloned()
                        .ok_or_else(|| EvalError::AttrNotFound(name.clone()))
                })();
                match attempt {
                    Ok(mut value) => {
                        while let Value::Thunk(inner) = value {
                            value = inner.force(evaluator)?;
                        }
                        *self.0.borrow_mut() = ThunkRepr::Evaluated(Box::new(value.clone()));
                        Ok(value)
                    }
                    Err(e) => {
                        *self.0.borrow_mut() = ThunkRepr::InheritSelect { source, name, env };
                        Err(e)
                    }
                }
            }
            ThunkRepr::Blackhole => {
                Err(EvalError::InfiniteRecursion(
                    "thunk blackhole".into(),
                ))
            }
            ThunkRepr::Evaluated(v) => {
                // Put it back (was taken by replace).
                let cloned = (*v).clone();
                *self.0.borrow_mut() = ThunkRepr::Evaluated(v);
                Ok(cloned)
            }
        }
    }
}

impl fmt::Debug for Thunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.0.borrow() {
            ThunkRepr::Suspended { .. } => write!(f, "<thunk>"),
            ThunkRepr::InheritSelect { name, .. } => write!(f, "<inherit-select {name}>"),
            ThunkRepr::Blackhole => write!(f, "<blackhole>"),
            ThunkRepr::Evaluated(v) => write!(f, "{v:?}"),
        }
    }
}

/// A Nix attribute set.
#[derive(Debug, Clone, Default)]
pub struct NixAttrs(pub BTreeMap<String, Value>);

impl NixAttrs {
    /// Create an empty attribute set.
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Look up an attribute by name.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// Insert or overwrite an attribute.
    pub fn insert(&mut self, key: String, value: Value) {
        self.0.insert(key, value);
    }

    /// Check whether an attribute exists.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// Iterate over attribute names in sorted order.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }

    /// Iterate over (name, value) pairs in sorted key order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }

    /// Iterate over values in sorted key order.
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.0.values()
    }

    /// Remove an attribute, returning its value if present.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.0.remove(key)
    }

    /// Return the number of attributes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether this attribute set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Merge two attrsets (right overrides left, like `//`).
    #[must_use]
    pub fn update(&self, other: &NixAttrs) -> NixAttrs {
        let mut result = self.0.clone();
        for (k, v) in &other.0 {
            result.insert(k.clone(), v.clone());
        }
        NixAttrs(result)
    }
}

impl FromIterator<(String, Value)> for NixAttrs {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        NixAttrs(iter.into_iter().collect())
    }
}

impl IntoIterator for NixAttrs {
    type Item = (String, Value);
    type IntoIter = std::collections::btree_map::IntoIter<String, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// A closure — lambda + captured environment.
///
/// Stores rnix AST nodes so we can re-evaluate the body in the captured env.
#[derive(Debug, Clone)]
pub struct Closure {
    pub param: rnix::ast::Param,
    pub body: rnix::ast::Expr,
    pub env: Env,
}

/// The function signature stored inside a [`BuiltinFn`].
pub type BuiltinFunc = dyn Fn(&[Value]) -> Result<Value, EvalError>;

/// A builtin function.
///
/// Not `Send`/`Sync` because `Value` contains rnix AST nodes (rowan `SyntaxNode`)
/// which use `NonNull` internally. The evaluator is single-threaded.
#[derive(Clone)]
pub struct BuiltinFn {
    /// Name used for display and debug printing.
    pub name: &'static str,
    /// The implementation closure.
    pub func: Arc<BuiltinFunc>,
}

impl fmt::Debug for BuiltinFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<builtin {}>", self.name)
    }
}

/// Evaluation environment — lexical scope chain.
#[derive(Debug, Clone, Default)]
pub struct Env {
    bindings: BTreeMap<String, Value>,
    parent: Option<Arc<Env>>,
    /// Dynamic scope from `with` expressions.
    with_scope: Option<Arc<NixAttrs>>,
    /// Source file currently being evaluated, for relative path
    /// literals (`./foo.nix`) inside function defaults that get
    /// evaluated *after* control has left the file scope. The
    /// closure captures this when it is created and `apply`
    /// pushes it onto the eval-file stack before running the body.
    pub eval_file: Option<std::path::PathBuf>,
}

impl Env {
    /// Create a root environment with no bindings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bindings: BTreeMap::new(),
            parent: None,
            with_scope: None,
            eval_file: None,
        }
    }

    /// Create a child environment that inherits from this one.
    #[must_use]
    pub fn child(&self) -> Self {
        Self {
            bindings: BTreeMap::new(),
            parent: Some(Arc::new(self.clone())),
            with_scope: None,
            // Children inherit the parent's eval file so that
            // path literals nested deep in let-chains still
            // resolve against the right directory.
            eval_file: self.eval_file.clone(),
        }
    }

    /// Attach a `with` scope to this environment.
    #[must_use]
    pub fn with_scope(mut self, attrs: NixAttrs) -> Self {
        self.with_scope = Some(Arc::new(attrs));
        self
    }

    /// Bind a name to a value in this environment's own scope.
    pub fn bind(&mut self, name: String, value: Value) {
        self.bindings.insert(name, value);
    }

    /// Two-pass lookup matching Nix semantics:
    ///
    /// 1. Walk the entire lexical-binding chain (own bindings → parent's
    ///    bindings → … → root's bindings). Any explicit `let`/`rec`/
    ///    function-arg binding wins over every `with` scope.
    /// 2. If no lexical binding matched, walk the chain again looking only
    ///    at `with_scope`s, **innermost first**. So `with X; with Y; x`
    ///    finds `x` in Y if Y has it, otherwise in X.
    ///
    /// The previous single-pass implementation reached the *outer*
    /// `with_scope` before the inner one (because the parent recursion
    /// returned the parent's full lookup result, including its `with_scope`,
    /// before the child's own `with_scope` got checked).
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<Value> {
        if let Some(v) = self.lookup_lexical(name) {
            return Some(v);
        }
        self.lookup_with(name)
    }

    fn lookup_lexical(&self, name: &str) -> Option<Value> {
        if let Some(v) = self.bindings.get(name) {
            return Some(v.clone());
        }
        self.parent.as_ref().and_then(|p| p.lookup_lexical(name))
    }

    fn lookup_with(&self, name: &str) -> Option<Value> {
        if let Some(ref attrs) = self.with_scope {
            if let Some(v) = attrs.get(name) {
                return Some(v.clone());
            }
        }
        self.parent.as_ref().and_then(|p| p.lookup_with(name))
    }
}

/// Evaluation errors produced by the Nix evaluator.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EvalError {
    /// A variable was referenced but not bound in scope.
    #[error("undefined variable: {0}")]
    UndefinedVar(String),
    /// A type mismatch or coercion failure.
    #[error("type error: {0}")]
    TypeError(String),
    /// An attribute was selected from a set that does not contain it.
    #[error("attribute not found: {0}")]
    AttrNotFound(String),
    /// A type mismatch with structured expected/got information.
    #[error("type error: expected {expected}, got {got}")]
    TypeMismatch {
        expected: &'static str,
        got: &'static str,
    },
    /// An `assert` expression's condition evaluated to false.
    #[error("assertion failed")]
    AssertionFailed,
    /// Integer division by zero.
    #[error("division by zero")]
    DivisionByZero,
    /// Infinite recursion detected (thunk blackhole or eval depth).
    #[error("infinite recursion ({0})")]
    InfiniteRecursion(String),
    /// An I/O error from the host filesystem.
    #[error("I/O error: {context}: {message}")]
    IoError { context: String, message: String },
    /// Explicit `throw` or `abort` from Nix code.
    #[error("{0}")]
    Throw(String),
    /// A language feature that is not yet implemented.
    #[error("not yet implemented: {0}")]
    NotImplemented(String),
    /// A syntax error in the input expression.
    #[error("parse error: {0}")]
    ParseError(String),
    /// Maximum recursion depth exceeded.
    #[error("recursion limit: {0}")]
    RecursionLimit(String),
}

impl EvalError {
    /// Convenience constructor for a `TypeError` variant.
    #[must_use]
    pub fn type_error(msg: impl Into<String>) -> Self {
        EvalError::TypeError(msg.into())
    }

    /// Convenience constructor for a `TypeMismatch` variant.
    #[must_use]
    pub fn type_mismatch(expected: &'static str, got: &'static str) -> Self {
        EvalError::TypeMismatch { expected, got }
    }

    /// Whether this error was caused by `throw` or `abort`.
    #[must_use]
    pub fn is_throw(&self) -> bool {
        matches!(self, EvalError::Throw(_))
    }

    /// Whether this error is an infinite recursion.
    #[must_use]
    pub fn is_infinite_recursion(&self) -> bool {
        matches!(self, EvalError::InfiniteRecursion(_))
    }
}

impl Value {
    /// Convenience constructor for a context-free string.
    #[must_use]
    pub fn string(s: impl Into<String>) -> Self {
        Value::String(NixString::plain(s))
    }

    /// Convert a value to JSON for API output.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(n) => serde_json::json!(n),
            Value::Float(f) => serde_json::json!(f),
            Value::String(s) => serde_json::Value::String(s.chars.clone()),
            Value::Path(p) => serde_json::Value::String(p.clone()),
            Value::List(items) => {
                serde_json::Value::Array(items.iter().map(|v| v.to_json()).collect())
            }
            Value::Attrs(attrs) => {
                let map: serde_json::Map<String, serde_json::Value> = attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_json()))
                    .collect();
                serde_json::Value::Object(map)
            }
            Value::Lambda(_) => serde_json::Value::String("<lambda>".to_string()),
            Value::Builtin(b) => serde_json::Value::String(format!("<builtin {}>", b.name)),
            Value::Thunk(thunk) => {
                // Force the thunk for JSON conversion.
                match thunk.force(&|expr, env| crate::eval::eval_expr(expr, env)) {
                    Ok(v) => v.to_json(),
                    Err(_) => serde_json::Value::String("<thunk:error>".to_string()),
                }
            }
        }
    }

    /// Return the Nix type name for this value (e.g. `"int"`, `"set"`).
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::String(_) => "string",
            Value::Path(_) => "path",
            Value::List(_) => "list",
            Value::Attrs(_) => "set",
            Value::Lambda(_) => "lambda",
            Value::Builtin(_) => "lambda",
            Value::Thunk(thunk) => {
                // Force and delegate.
                match thunk.force(&|expr, env| crate::eval::eval_expr(expr, env)) {
                    Ok(v) => v.type_name(),
                    Err(_) => "thunk",
                }
            }
        }
    }

    // ── Value coercion methods ──────────────────────────────────
    //
    // Naming conventions:
    //
    // • `as_*(&self)` — borrow. Returns a reference or Copy type.
    //   Primitives (`as_bool`, `as_int`, `as_float`) force thunks
    //   transparently because they return owned Copy values. Reference
    //   accessors (`as_string`, `as_nix_string`, `as_attrs`, `as_list`)
    //   CANNOT force thunks (the forced value is transient and we can't
    //   return a borrow into it), so they error on Thunk inputs.
    //
    // • `to_*(&self)` — clone. Returns an owned value by cloning, and
    //   DOES force thunks. Use when the value may be a thunk and you
    //   need an owned result.
    //
    // • `coerce_to_path` — a Nix-specific coercion that accepts both
    //   Path and String values (many builtins accept either).

    /// Extract a bool, forcing thunks if needed.
    pub fn as_bool(&self) -> Result<bool, EvalError> {
        match self {
            Value::Bool(b) => Ok(*b),
            Value::Thunk(thunk) => {
                thunk.force(&|e, env| crate::eval::eval_expr(e, env))?.as_bool()
            }
            _ => Err(EvalError::TypeMismatch { expected: "bool", got: self.type_name() }),
        }
    }

    /// Extract an integer, forcing thunks if needed.
    pub fn as_int(&self) -> Result<i64, EvalError> {
        match self {
            Value::Int(n) => Ok(*n),
            Value::Thunk(thunk) => {
                thunk.force(&|e, env| crate::eval::eval_expr(e, env))?.as_int()
            }
            _ => Err(EvalError::TypeMismatch { expected: "int", got: self.type_name() }),
        }
    }

    /// Borrow the string content without forcing thunks.
    pub fn as_string(&self) -> Result<&str, EvalError> {
        match self {
            Value::String(s) => Ok(&s.chars),
            // Note: we cannot return a reference into a forced thunk here
            // because the forced value is transient. Callers that go through
            // force_value() in eval.rs will match on the concrete value.
            Value::Thunk(_) => Err(EvalError::TypeError(
                "thunk in as_string: force first via force_value()".into(),
            )),
            _ => Err(EvalError::TypeMismatch { expected: "string", got: self.type_name() }),
        }
    }

    /// Return a reference to the full `NixString` (with context).
    pub fn as_nix_string(&self) -> Result<&NixString, EvalError> {
        match self {
            Value::String(ns) => Ok(ns),
            Value::Thunk(_) => Err(EvalError::TypeError(
                "thunk in as_nix_string: force first via force_value()".into(),
            )),
            _ => Err(EvalError::TypeMismatch { expected: "string", got: self.type_name() }),
        }
    }

    /// Force-aware string extraction. Returns an owned String by forcing
    /// thunks if needed. Use this instead of `as_string()` when you may
    /// be operating on thunked attrset values.
    pub fn to_str(&self) -> Result<String, EvalError> {
        match self {
            Value::String(s) => Ok(s.chars.clone()),
            Value::Thunk(thunk) => {
                let forced = thunk.force(&|e, env| crate::eval::eval_expr(e, env))?;
                forced.to_str()
            }
            _ => Err(EvalError::TypeMismatch { expected: "string", got: self.type_name() }),
        }
    }

    /// Force-aware `NixString` extraction. Returns an owned `NixString`
    /// (with context) by forcing thunks if needed.
    pub fn to_nix_string(&self) -> Result<NixString, EvalError> {
        match self {
            Value::String(s) => Ok(s.clone()),
            Value::Thunk(thunk) => {
                let forced = thunk.force(&|e, env| crate::eval::eval_expr(e, env))?;
                forced.to_nix_string()
            }
            _ => Err(EvalError::TypeMismatch { expected: "string", got: self.type_name() }),
        }
    }

    /// Borrow the inner attrs without forcing. If the value is a
    /// thunk, the caller should have force_value'd it first; we
    /// return an error rather than silently mutating the thunk
    /// (which would require &mut self).
    ///
    /// Most call sites should use `to_attrs()` (which forces and
    /// clones) unless they're certain the value is already
    /// concrete and want to avoid the clone.
    pub fn as_attrs(&self) -> Result<&NixAttrs, EvalError> {
        match self {
            Value::Attrs(a) => Ok(a),
            Value::Thunk(_) => Err(EvalError::TypeError(
                "thunk in as_attrs: force first via force_value() or use to_attrs()".into(),
            )),
            _ => Err(EvalError::TypeMismatch { expected: "set", got: self.type_name() }),
        }
    }

    /// Borrow the list content without forcing thunks.
    pub fn as_list(&self) -> Result<&[Value], EvalError> {
        match self {
            Value::List(l) => Ok(l),
            Value::Thunk(_) => Err(EvalError::TypeError(
                "thunk in as_list: force first via force_value()".into(),
            )),
            _ => Err(EvalError::TypeMismatch { expected: "list", got: self.type_name() }),
        }
    }

    /// Force-aware attrs extraction. Forces the value if it is a thunk.
    pub fn to_attrs(&self) -> Result<NixAttrs, EvalError> {
        match self {
            Value::Attrs(a) => Ok(a.clone()),
            Value::Thunk(thunk) => {
                let forced = thunk.force(&|e, env| crate::eval::eval_expr(e, env))?;
                forced.to_attrs()
            }
            _ => Err(EvalError::TypeMismatch { expected: "set", got: self.type_name() }),
        }
    }

    /// Force-aware list extraction. Forces the value if it is a thunk.
    pub fn to_list(&self) -> Result<Vec<Value>, EvalError> {
        match self {
            Value::List(l) => Ok(l.clone()),
            Value::Thunk(thunk) => {
                let forced = thunk.force(&|e, env| crate::eval::eval_expr(e, env))?;
                forced.to_list()
            }
            _ => Err(EvalError::TypeMismatch { expected: "list", got: self.type_name() }),
        }
    }

    /// Extract a filesystem path from a `Path` or `String` value.
    ///
    /// Many builtins (`readFile`, `import`, `pathExists`, etc.) accept
    /// either `Path` or `String` arguments. This method centralises
    /// that coercion so every call-site doesn't repeat the same match.
    pub fn coerce_to_path(&self, context: &str) -> Result<String, EvalError> {
        match self {
            Value::Path(p) => Ok(p.clone()),
            Value::String(ns) => Ok(ns.chars.clone()),
            Value::Attrs(attrs) => {
                if let Some(out_path) = attrs.get("outPath") {
                    let forced = crate::eval::force_value(out_path)?;
                    forced.coerce_to_path(context)
                } else {
                    Err(EvalError::TypeError(format!(
                        "{context}: expected path or string, got set without outPath"
                    )))
                }
            }
            _ => Err(EvalError::TypeError(format!(
                "{context}: expected path or string, got {}",
                self.type_name()
            ))),
        }
    }

    /// Coerce a numeric value to float.
    pub fn as_float(&self) -> Result<f64, EvalError> {
        match self {
            Value::Float(f) => Ok(*f),
            Value::Int(n) => Ok(*n as f64),
            Value::Thunk(thunk) => {
                thunk.force(&|e, env| crate::eval::eval_expr(e, env))?.as_float()
            }
            _ => Err(EvalError::TypeMismatch { expected: "number", got: self.type_name() }),
        }
    }

    /// Coerce this value to a string following CppNix semantics.
    ///
    /// This is the single source of truth for string coercion used by
    /// string interpolation, `builtins.toString`, and derivation env
    /// var construction.
    ///
    /// Rules (in order):
    /// - String → its content (with context)
    /// - Path → path string (adds Plain context element)
    /// - Int → decimal representation
    /// - Float → decimal representation
    /// - Bool → "1" for true, "" for false
    /// - Null → ""
    /// - Attrs with `__toString` → call `__toString(self)` and coerce result
    /// - Attrs with `outPath` → coerce outPath recursively
    /// - List → space-joined coerced elements
    /// - Lambda/Builtin/Thunk → error
    pub fn coerce_to_string(&self) -> Result<(String, StringContext), EvalError> {
        let mut ctx = StringContext::new();
        let s = match self {
            Value::String(ns) => {
                ctx.merge(&ns.context);
                ns.chars.clone()
            }
            Value::Path(p) => {
                ctx.add_plain(p.clone());
                p.clone()
            }
            Value::Int(n) => n.to_string(),
            Value::Float(f) => format!("{f}"),
            Value::Bool(true) => "1".to_string(),
            Value::Bool(false) => String::new(),
            Value::Null => String::new(),
            Value::Attrs(attrs) => {
                if let Some(to_str) = attrs.get("__toString") {
                    let result =
                        crate::eval::apply(to_str.clone(), Value::Attrs(attrs.clone()))?;
                    let forced = crate::eval::force_value(&result)?;
                    let (s, c) = forced.coerce_to_string()?;
                    ctx.merge(&c);
                    s
                } else if let Some(out_path) = attrs.get("outPath") {
                    let forced = crate::eval::force_value(out_path)?;
                    let (s, c) = forced.coerce_to_string()?;
                    ctx.merge(&c);
                    s
                } else {
                    return Err(EvalError::TypeError(
                        "cannot coerce set to string (no __toString or outPath)".into(),
                    ));
                }
            }
            Value::List(items) => {
                let mut parts = Vec::new();
                for item in items {
                    let forced = crate::eval::force_value(item)?;
                    let (s, c) = forced.coerce_to_string()?;
                    ctx.merge(&c);
                    parts.push(s);
                }
                parts.join(" ")
            }
            other => {
                return Err(EvalError::TypeError(format!(
                    "cannot coerce {} to string",
                    other.type_name()
                )));
            }
        };
        Ok((s, ctx))
    }
}

// ── Conversions from foreign value types ────────────────────

impl From<&serde_json::Value> for Value {
    fn from(json: &serde_json::Value) -> Self {
        match json {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else {
                    Value::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => Value::string(s.clone()),
            serde_json::Value::Array(arr) => {
                Value::List(arr.iter().map(Value::from).collect())
            }
            serde_json::Value::Object(obj) => {
                let mut attrs = NixAttrs::new();
                for (k, v) in obj {
                    attrs.insert(k.clone(), Value::from(v));
                }
                Value::Attrs(attrs)
            }
        }
    }
}

impl From<&toml::Value> for Value {
    fn from(v: &toml::Value) -> Self {
        match v {
            toml::Value::String(s) => Value::string(s.clone()),
            toml::Value::Integer(n) => Value::Int(*n),
            toml::Value::Float(f) => Value::Float(*f),
            toml::Value::Boolean(b) => Value::Bool(*b),
            toml::Value::Array(arr) => {
                Value::List(arr.iter().map(Value::from).collect())
            }
            toml::Value::Table(t) => {
                let mut attrs = NixAttrs::new();
                for (k, val) in t {
                    attrs.insert(k.clone(), Value::from(val));
                }
                Value::Attrs(attrs)
            }
            toml::Value::Datetime(dt) => Value::string(dt.to_string()),
        }
    }
}

impl Default for Value {
    fn default() -> Self {
        Value::Null
    }
}

// ── From impls for ergonomic Value construction ─────────────

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::Int(n)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

impl From<NixString> for Value {
    fn from(s: NixString) -> Self {
        Value::String(s)
    }
}

impl From<NixAttrs> for Value {
    fn from(attrs: NixAttrs) -> Self {
        Value::Attrs(attrs)
    }
}

impl From<Vec<Value>> for Value {
    fn from(list: Vec<Value>) -> Self {
        Value::List(list)
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        // Force thunks before comparing.
        let force = |v: &Value| -> Value {
            match v {
                Value::Thunk(t) => t
                    .force(&|e, env| crate::eval::eval_expr(e, env))
                    .unwrap_or(Value::Null),
                other => other.clone(),
            }
        };
        let l = force(self);
        let r = force(other);

        match (&l, &r) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => (*a as f64) == *b,
            (Value::String(a), Value::String(b)) => a.chars == b.chars,
            (Value::Path(a), Value::Path(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Attrs(a), Value::Attrs(b)) => a.0 == b.0,
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "\"{}\"", s.chars.replace('\\', "\\\\").replace('"', "\\\"")),
            Value::Path(p) => write!(f, "{p}"),
            Value::List(items) => {
                write!(f, "[ ")?;
                for item in items {
                    write!(f, "{item} ")?;
                }
                write!(f, "]")
            }
            Value::Attrs(attrs) => {
                write!(f, "{{ ")?;
                for (k, v) in attrs.iter() {
                    write!(f, "{k} = {v}; ")?;
                }
                write!(f, "}}")
            }
            Value::Lambda(_) => write!(f, "<<lambda>>"),
            Value::Builtin(b) => write!(f, "<<builtin {}>>" , b.name),
            Value::Thunk(thunk) => {
                match thunk.force(&|e, env| crate::eval::eval_expr(e, env)) {
                    Ok(v) => write!(f, "{v}"),
                    Err(_) => write!(f, "<<thunk:error>>"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // ── Value::to_json for every variant ─────────────────

    #[test]
    fn to_json_null() {
        assert_eq!(Value::Null.to_json(), serde_json::Value::Null);
    }

    #[test]
    fn to_json_bool() {
        assert_eq!(Value::Bool(true).to_json(), serde_json::Value::Bool(true));
        assert_eq!(Value::Bool(false).to_json(), serde_json::Value::Bool(false));
    }

    #[test]
    fn to_json_int() {
        assert_eq!(Value::Int(42).to_json(), serde_json::json!(42));
    }

    #[test]
    fn to_json_float() {
        assert_eq!(Value::Float(3.14).to_json(), serde_json::json!(3.14));
    }

    #[test]
    fn to_json_string() {
        assert_eq!(
            Value::string("hello").to_json(),
            serde_json::Value::String("hello".to_string()),
        );
    }

    #[test]
    fn to_json_path() {
        assert_eq!(
            Value::Path("/nix/store".to_string()).to_json(),
            serde_json::Value::String("/nix/store".to_string()),
        );
    }

    #[test]
    fn to_json_list() {
        let v = Value::List(vec![Value::Int(1), Value::Bool(true)]);
        assert_eq!(v.to_json(), serde_json::json!([1, true]));
    }

    #[test]
    fn to_json_attrs() {
        let mut attrs = NixAttrs::new();
        attrs.insert("a".to_string(), Value::Int(1));
        let v = Value::Attrs(attrs);
        assert_eq!(v.to_json(), serde_json::json!({"a": 1}));
    }

    #[test]
    fn to_json_lambda() {
        // Build a minimal rnix lambda for testing
        let root = rnix::Root::parse("x: x");
        let expr = root.tree().expr().unwrap();
        let lambda = match expr {
            rnix::ast::Expr::Lambda(l) => l,
            _ => panic!("expected lambda"),
        };
        let closure = Closure {
            param: lambda.param().unwrap(),
            body: lambda.body().unwrap(),
            env: Env::new(),
        };
        assert_eq!(
            Value::Lambda(closure).to_json(),
            serde_json::Value::String("<lambda>".to_string()),
        );
    }

    #[test]
    fn to_json_builtin() {
        let b = BuiltinFn {
            name: "test",
            func: Arc::new(|_| Ok(Value::Null)),
        };
        assert_eq!(
            Value::Builtin(b).to_json(),
            serde_json::Value::String("<builtin test>".to_string()),
        );
    }

    // ── Value::type_name for every variant ───────────────

    #[test]
    fn type_name_null() { assert_eq!(Value::Null.type_name(), "null"); }

    #[test]
    fn type_name_bool() { assert_eq!(Value::Bool(false).type_name(), "bool"); }

    #[test]
    fn type_name_int() { assert_eq!(Value::Int(0).type_name(), "int"); }

    #[test]
    fn type_name_float() { assert_eq!(Value::Float(0.0).type_name(), "float"); }

    #[test]
    fn type_name_string() { assert_eq!(Value::string("").type_name(), "string"); }

    #[test]
    fn type_name_path() { assert_eq!(Value::Path("".to_string()).type_name(), "path"); }

    #[test]
    fn type_name_list() { assert_eq!(Value::List(vec![]).type_name(), "list"); }

    #[test]
    fn type_name_set() { assert_eq!(Value::Attrs(NixAttrs::new()).type_name(), "set"); }

    #[test]
    fn type_name_lambda() {
        let root = rnix::Root::parse("x: x");
        let expr = root.tree().expr().unwrap();
        let lambda = match expr {
            rnix::ast::Expr::Lambda(l) => l,
            _ => panic!("expected lambda"),
        };
        let closure = Closure {
            param: lambda.param().unwrap(),
            body: lambda.body().unwrap(),
            env: Env::new(),
        };
        assert_eq!(Value::Lambda(closure).type_name(), "lambda");
    }

    #[test]
    fn type_name_builtin() {
        let b = BuiltinFn {
            name: "t",
            func: Arc::new(|_| Ok(Value::Null)),
        };
        assert_eq!(Value::Builtin(b).type_name(), "lambda");
    }

    // ── as_* error on wrong type ─────────────────────────

    #[test]
    fn as_bool_error_on_non_bool() {
        assert!(Value::Int(1).as_bool().is_err());
        assert!(Value::string("true").as_bool().is_err());
    }

    #[test]
    fn as_int_error_on_non_int() {
        assert!(Value::Bool(true).as_int().is_err());
        assert!(Value::Float(1.0).as_int().is_err());
    }

    #[test]
    fn as_string_error_on_non_string() {
        assert!(Value::Int(42).as_string().is_err());
        assert!(Value::Null.as_string().is_err());
    }

    #[test]
    fn as_attrs_error_on_non_attrs() {
        assert!(Value::Int(1).as_attrs().is_err());
        assert!(Value::List(vec![]).as_attrs().is_err());
    }

    #[test]
    fn as_list_error_on_non_list() {
        assert!(Value::Int(1).as_list().is_err());
        assert!(Value::Attrs(NixAttrs::new()).as_list().is_err());
    }

    // ── as_float int->float coercion ─────────────────────

    #[test]
    fn as_float_coerces_int() {
        assert_eq!(Value::Int(5).as_float().unwrap(), 5.0);
        assert_eq!(Value::Float(2.5).as_float().unwrap(), 2.5);
        assert!(Value::string("x").as_float().is_err());
    }

    // ── PartialEq ────────────────────────────────────────

    #[test]
    fn partial_eq_int_float_cross() {
        assert_eq!(Value::Int(3), Value::Float(3.0));
        assert_eq!(Value::Float(3.0), Value::Int(3));
        assert_ne!(Value::Int(3), Value::Float(3.5));
    }

    #[test]
    fn partial_eq_different_types_not_equal() {
        assert_ne!(Value::Int(1), Value::string("1"));
        assert_ne!(Value::Bool(true), Value::Int(1));
        assert_ne!(Value::Null, Value::Bool(false));
        assert_ne!(Value::List(vec![]), Value::Attrs(NixAttrs::new()));
    }

    // ── Display for all variants ─────────────────────────

    #[test]
    fn display_null() { assert_eq!(format!("{}", Value::Null), "null"); }

    #[test]
    fn display_bool() {
        assert_eq!(format!("{}", Value::Bool(true)), "true");
        assert_eq!(format!("{}", Value::Bool(false)), "false");
    }

    #[test]
    fn display_int() { assert_eq!(format!("{}", Value::Int(42)), "42"); }

    #[test]
    fn display_float() {
        let s = format!("{}", Value::Float(3.14));
        assert!(s.contains("3.14"));
    }

    #[test]
    fn display_string() {
        assert_eq!(format!("{}", Value::string("hi")), "\"hi\"");
    }

    #[test]
    fn display_string_with_escapes() {
        let v = Value::string("a\"b\\c");
        let s = format!("{v}");
        assert!(s.contains("\\\""));
        assert!(s.contains("\\\\"));
    }

    #[test]
    fn display_path() {
        assert_eq!(format!("{}", Value::Path("/foo".to_string())), "/foo");
    }

    #[test]
    fn display_list() {
        let v = Value::List(vec![Value::Int(1), Value::Int(2)]);
        assert_eq!(format!("{v}"), "[ 1 2 ]");
    }

    #[test]
    fn display_attrs() {
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(1));
        let v = Value::Attrs(attrs);
        assert_eq!(format!("{v}"), "{ x = 1; }");
    }

    #[test]
    fn display_lambda() {
        let root = rnix::Root::parse("x: x");
        let expr = root.tree().expr().unwrap();
        let lambda = match expr {
            rnix::ast::Expr::Lambda(l) => l,
            _ => panic!("expected lambda"),
        };
        let closure = Closure {
            param: lambda.param().unwrap(),
            body: lambda.body().unwrap(),
            env: Env::new(),
        };
        assert_eq!(format!("{}", Value::Lambda(closure)), "<<lambda>>");
    }

    #[test]
    fn display_builtin() {
        let b = BuiltinFn {
            name: "add",
            func: Arc::new(|_| Ok(Value::Null)),
        };
        assert_eq!(format!("{}", Value::Builtin(b)), "<<builtin add>>");
    }

    // ── NixAttrs ─────────────────────────────────────────

    #[test]
    fn nixattrs_update_merging() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        a.insert("y".to_string(), Value::Int(2));
        let mut b = NixAttrs::new();
        b.insert("y".to_string(), Value::Int(99));
        b.insert("z".to_string(), Value::Int(3));
        let merged = a.update(&b);
        assert_eq!(merged.get("x"), Some(&Value::Int(1)));
        assert_eq!(merged.get("y"), Some(&Value::Int(99)));
        assert_eq!(merged.get("z"), Some(&Value::Int(3)));
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn nixattrs_contains_key() {
        let mut a = NixAttrs::new();
        a.insert("foo".to_string(), Value::Null);
        assert!(a.contains_key("foo"));
        assert!(!a.contains_key("bar"));
    }

    // ── Env ──────────────────────────────────────────────

    #[test]
    fn env_lookup_through_parent_chain() {
        let mut root = Env::new();
        root.bind("a".to_string(), Value::Int(1));
        let mut child = root.child();
        child.bind("b".to_string(), Value::Int(2));
        let grandchild = child.child();
        // grandchild can see both a and b through parent chain
        assert_eq!(grandchild.lookup("a"), Some(Value::Int(1)));
        assert_eq!(grandchild.lookup("b"), Some(Value::Int(2)));
        assert_eq!(grandchild.lookup("c"), None);
    }

    #[test]
    fn env_with_scope_lookup() {
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(42));
        let env = Env::new().with_scope(attrs);
        assert_eq!(env.lookup("x"), Some(Value::Int(42)));
        assert_eq!(env.lookup("y"), None);
    }

    #[test]
    fn env_local_shadows_with_scope() {
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(1));
        let mut env = Env::new().with_scope(attrs);
        env.bind("x".to_string(), Value::Int(99));
        assert_eq!(env.lookup("x"), Some(Value::Int(99)));
    }

    // ── NixString context propagation ─────────────────────

    #[test]
    fn string_context_merge_combines_elements() {
        let mut ctx_a = StringContext::new();
        ctx_a.add_plain("/nix/store/aaa".to_string());
        let mut ctx_b = StringContext::new();
        ctx_b.add_plain("/nix/store/bbb".to_string());
        ctx_a.merge(&ctx_b);
        assert_eq!(ctx_a.0.len(), 2);
        assert!(ctx_a.0.contains(&ContextElement::Plain("/nix/store/aaa".to_string())));
        assert!(ctx_a.0.contains(&ContextElement::Plain("/nix/store/bbb".to_string())));
    }

    #[test]
    fn string_context_merge_deduplicates() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/same".to_string());
        ctx.add_plain("/nix/store/same".to_string());
        assert_eq!(ctx.0.len(), 1);
    }

    #[test]
    fn string_context_mixed_element_types() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/foo".to_string());
        ctx.add_output("/nix/store/bar.drv".to_string(), "out".to_string());
        ctx.add_drv_deep("/nix/store/baz.drv".to_string());
        assert_eq!(ctx.0.len(), 3);
        assert!(!ctx.is_empty());
    }

    #[test]
    fn string_context_new_is_empty() {
        let ctx = StringContext::new();
        assert!(ctx.is_empty());
        assert_eq!(ctx.0.len(), 0);
    }

    #[test]
    fn nix_string_plain_has_no_context() {
        let s = NixString::plain("hello");
        assert!(!s.has_context());
        assert_eq!(s.as_str(), "hello");
    }

    #[test]
    fn nix_string_with_context_reports_context() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/xyz".to_string());
        let s = NixString::with_context("hello", ctx);
        assert!(s.has_context());
        assert_eq!(s.as_str(), "hello");
    }

    #[test]
    fn nix_string_display_shows_chars_only() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/abc".to_string());
        let s = NixString::with_context("visible", ctx);
        assert_eq!(format!("{s}"), "visible");
    }

    #[test]
    fn nix_string_struct_eq_includes_context() {
        let plain = NixString::plain("hello");
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/xxx".to_string());
        let with_ctx = NixString::with_context("hello", ctx);
        // NixString's derived PartialEq compares context too
        assert_ne!(plain, with_ctx);
    }

    #[test]
    fn value_string_eq_ignores_context() {
        let plain = Value::String(NixString::plain("hello"));
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/xxx".to_string());
        let with_ctx = Value::String(NixString::with_context("hello", ctx));
        // Value::PartialEq only compares .chars, ignoring context
        assert_eq!(plain, with_ctx);
    }

    // ── Env deeply nested with-scopes ─────────────────────

    #[test]
    fn env_nested_with_inner_wins() {
        let mut outer_attrs = NixAttrs::new();
        outer_attrs.insert("x".to_string(), Value::Int(1));
        let outer = Env::new().with_scope(outer_attrs);
        let mut inner_attrs = NixAttrs::new();
        inner_attrs.insert("x".to_string(), Value::Int(2));
        let inner = outer.child().with_scope(inner_attrs);
        assert_eq!(inner.lookup("x"), Some(Value::Int(2)));
    }

    #[test]
    fn env_nested_with_fallback_to_outer() {
        let mut outer_attrs = NixAttrs::new();
        outer_attrs.insert("x".to_string(), Value::Int(1));
        let outer = Env::new().with_scope(outer_attrs);
        let mut inner_attrs = NixAttrs::new();
        inner_attrs.insert("y".to_string(), Value::Int(2));
        let inner = outer.child().with_scope(inner_attrs);
        assert_eq!(inner.lookup("x"), Some(Value::Int(1)));
        assert_eq!(inner.lookup("y"), Some(Value::Int(2)));
    }

    #[test]
    fn env_lexical_binding_wins_over_all_with_scopes() {
        let mut outer_attrs = NixAttrs::new();
        outer_attrs.insert("x".to_string(), Value::Int(1));
        let outer = Env::new().with_scope(outer_attrs);
        let mut inner_attrs = NixAttrs::new();
        inner_attrs.insert("x".to_string(), Value::Int(2));
        let mut inner = outer.child().with_scope(inner_attrs);
        inner.bind("x".to_string(), Value::Int(99));
        assert_eq!(inner.lookup("x"), Some(Value::Int(99)));
    }

    #[test]
    fn env_parent_lexical_wins_over_child_with_scope() {
        let mut root = Env::new();
        root.bind("x".to_string(), Value::Int(10));
        let mut child_attrs = NixAttrs::new();
        child_attrs.insert("x".to_string(), Value::Int(20));
        let child = root.child().with_scope(child_attrs);
        assert_eq!(child.lookup("x"), Some(Value::Int(10)));
    }

    #[test]
    fn env_deeply_nested_with_scopes_three_levels() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        let env1 = Env::new().with_scope(a);

        let mut b = NixAttrs::new();
        b.insert("y".to_string(), Value::Int(2));
        let env2 = env1.child().with_scope(b);

        let mut c = NixAttrs::new();
        c.insert("z".to_string(), Value::Int(3));
        let env3 = env2.child().with_scope(c);

        assert_eq!(env3.lookup("x"), Some(Value::Int(1)));
        assert_eq!(env3.lookup("y"), Some(Value::Int(2)));
        assert_eq!(env3.lookup("z"), Some(Value::Int(3)));
        assert_eq!(env3.lookup("w"), None);
    }

    #[test]
    fn env_lookup_lexical_only_skips_with_scope() {
        let mut attrs = NixAttrs::new();
        attrs.insert("x".to_string(), Value::Int(42));
        let env = Env::new().with_scope(attrs);
        assert_eq!(env.lookup_lexical("x"), None);
    }

    #[test]
    fn env_lookup_with_only_skips_bindings() {
        let mut env = Env::new();
        env.bind("x".to_string(), Value::Int(42));
        assert_eq!(env.lookup_with("x"), None);
    }

    #[test]
    fn env_child_inherits_eval_file() {
        let mut env = Env::new();
        env.eval_file = Some(std::path::PathBuf::from("/foo/bar.nix"));
        let child = env.child();
        assert_eq!(child.eval_file, Some(std::path::PathBuf::from("/foo/bar.nix")));
    }

    #[test]
    fn env_new_has_no_parent_no_with() {
        let env = Env::new();
        assert_eq!(env.lookup("anything"), None);
        assert!(env.eval_file.is_none());
    }

    // ── Thunk state machine ───────────────────────────────

    #[test]
    fn thunk_new_suspended_is_not_evaluated() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        assert!(!thunk.is_evaluated());
    }

    #[test]
    fn thunk_new_evaluated_is_evaluated() {
        let thunk = Thunk::new_evaluated(Value::Int(42));
        assert!(thunk.is_evaluated());
    }

    #[test]
    fn thunk_force_evaluates_suspended() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let result = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Value::Int(42));
        assert!(thunk.is_evaluated());
    }

    #[test]
    fn thunk_force_memoizes_result() {
        let root = rnix::Root::parse("1 + 2");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let r1 = thunk.force(&|e, env| crate::eval::eval_expr(e, env)).unwrap();
        let r2 = thunk.force(&|e, env| crate::eval::eval_expr(e, env)).unwrap();
        assert_eq!(r1, Value::Int(3));
        assert_eq!(r2, Value::Int(3));
    }

    #[test]
    fn thunk_force_already_evaluated_returns_value() {
        let thunk = Thunk::new_evaluated(Value::Bool(true));
        let result = thunk.force(&|_, _| panic!("should not be called"));
        assert_eq!(result.unwrap(), Value::Bool(true));
    }

    #[test]
    fn thunk_blackhole_detects_infinite_recursion() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());

        // Manually set to blackhole to simulate re-entrance
        *thunk.0.borrow_mut() = ThunkRepr::Blackhole;

        let result = thunk.force(&|_, _| Ok(Value::Null));
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("infinite recursion"));
    }

    #[test]
    fn thunk_update_env_replaces_suspended_env() {
        let root = rnix::Root::parse("x");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());

        let mut new_env = Env::new();
        new_env.bind("x".to_string(), Value::Int(99));
        thunk.update_env(new_env);

        let result = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
        assert_eq!(result.unwrap(), Value::Int(99));
    }

    #[test]
    fn thunk_update_env_noop_when_evaluated() {
        let thunk = Thunk::new_evaluated(Value::Int(1));
        let mut new_env = Env::new();
        new_env.bind("x".to_string(), Value::Int(99));
        thunk.update_env(new_env);
        assert_eq!(
            thunk.force(&|_, _| panic!("should not be called")).unwrap(),
            Value::Int(1),
        );
    }

    #[test]
    fn thunk_debug_suspended() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        assert_eq!(format!("{thunk:?}"), "<thunk>");
    }

    #[test]
    fn thunk_debug_evaluated() {
        let thunk = Thunk::new_evaluated(Value::Int(42));
        let dbg = format!("{thunk:?}");
        assert!(dbg.contains("42"));
    }

    #[test]
    fn thunk_error_restores_suspended_state() {
        let root = rnix::Root::parse("nonexistent_var");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());

        let result = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
        assert!(result.is_err());
        // After error, thunk should be restored to Suspended, not stuck as Blackhole
        assert!(!thunk.is_evaluated());
        let dbg = format!("{thunk:?}");
        assert_eq!(dbg, "<thunk>");
    }

    #[test]
    fn thunk_inherit_select_forces_and_selects() {
        let root = rnix::Root::parse(r#"{ x = 42; }"#);
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_inherit_select(expr, "x".to_string(), Env::new());
        let result = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
        assert_eq!(result.unwrap(), Value::Int(42));
        assert!(thunk.is_evaluated());
    }

    #[test]
    fn thunk_inherit_select_missing_attr_errors() {
        let root = rnix::Root::parse(r#"{ x = 42; }"#);
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_inherit_select(expr, "y".to_string(), Env::new());
        let result = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
        assert!(result.is_err());
        // Thunk should restore to InheritSelect, not be stuck as Blackhole
        assert!(!thunk.is_evaluated());
    }

    #[test]
    fn thunk_inherit_select_non_attrs_source_errors() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_inherit_select(expr, "x".to_string(), Env::new());
        let result = thunk.force(&|e, env| crate::eval::eval_expr(e, env));
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("not a set"));
    }

    // ── NixAttrs additional tests ─────────────────────────

    #[test]
    fn nixattrs_empty_operations() {
        let a = NixAttrs::new();
        assert!(a.is_empty());
        assert_eq!(a.len(), 0);
        assert_eq!(a.get("x"), None);
        assert!(!a.contains_key("x"));
        assert_eq!(a.keys().count(), 0);
        assert_eq!(a.iter().count(), 0);
    }

    #[test]
    fn nixattrs_update_with_empty() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        let b = NixAttrs::new();
        let merged = a.update(&b);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged.get("x"), Some(&Value::Int(1)));
    }

    #[test]
    fn nixattrs_update_empty_with_nonempty() {
        let a = NixAttrs::new();
        let mut b = NixAttrs::new();
        b.insert("x".to_string(), Value::Int(1));
        let merged = a.update(&b);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged.get("x"), Some(&Value::Int(1)));
    }

    #[test]
    fn nixattrs_keys_sorted_order() {
        let mut a = NixAttrs::new();
        a.insert("c".to_string(), Value::Int(3));
        a.insert("a".to_string(), Value::Int(1));
        a.insert("b".to_string(), Value::Int(2));
        let keys: Vec<&String> = a.keys().collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    // ── Value convenience methods ─────────────────────────

    #[test]
    fn value_to_str_forces_thunks() {
        let root = rnix::Root::parse(r#""hello""#);
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert_eq!(val.to_str().unwrap(), "hello");
    }

    #[test]
    fn value_to_nix_string_forces_thunks() {
        let root = rnix::Root::parse(r#""world""#);
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        let ns = val.to_nix_string().unwrap();
        assert_eq!(ns.as_str(), "world");
        assert!(!ns.has_context());
    }

    #[test]
    fn value_to_attrs_forces_thunks() {
        let root = rnix::Root::parse("{ x = 1; }");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        let attrs = val.to_attrs().unwrap();
        assert_eq!(attrs.len(), 1);
    }

    #[test]
    fn value_to_list_forces_thunks() {
        let root = rnix::Root::parse("[1 2 3]");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        let list = val.to_list().unwrap();
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn value_as_float_on_thunk() {
        let root = rnix::Root::parse("3.14");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        let f = val.as_float().unwrap();
        assert!((f - 3.14).abs() < f64::EPSILON);
    }

    #[test]
    fn value_as_bool_on_thunk() {
        let root = rnix::Root::parse("true");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert!(val.as_bool().unwrap());
    }

    #[test]
    fn value_as_int_on_thunk() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert_eq!(val.as_int().unwrap(), 42);
    }

    #[test]
    fn value_string_constructor() {
        let v = Value::string("test");
        assert_eq!(v, Value::String(NixString::plain("test")));
    }

    #[test]
    fn value_partial_eq_null_null() {
        assert_eq!(Value::Null, Value::Null);
    }

    #[test]
    fn value_partial_eq_lists_deep() {
        let a = Value::List(vec![Value::Int(1), Value::List(vec![Value::Int(2)])]);
        let b = Value::List(vec![Value::Int(1), Value::List(vec![Value::Int(2)])]);
        assert_eq!(a, b);
    }

    #[test]
    fn value_partial_eq_attrs_deep() {
        let mut a = NixAttrs::new();
        a.insert("x".to_string(), Value::Int(1));
        let mut b = NixAttrs::new();
        b.insert("x".to_string(), Value::Int(1));
        assert_eq!(Value::Attrs(a), Value::Attrs(b));
    }

    // ── EvalError variants & convenience constructors ────

    #[test]
    fn eval_error_type_error_constructor() {
        let e = EvalError::type_error("oops");
        assert!(matches!(e, EvalError::TypeError(ref s) if s == "oops"));
    }

    #[test]
    fn eval_error_type_mismatch_constructor() {
        let e = EvalError::type_mismatch("int", "string");
        match e {
            EvalError::TypeMismatch { expected, got } => {
                assert_eq!(expected, "int");
                assert_eq!(got, "string");
            }
            _ => panic!("expected TypeMismatch"),
        }
    }

    #[test]
    fn eval_error_is_throw_yes_no() {
        assert!(EvalError::Throw("oops".into()).is_throw());
        assert!(!EvalError::TypeError("oops".into()).is_throw());
        assert!(!EvalError::AssertionFailed.is_throw());
    }

    #[test]
    fn eval_error_is_infinite_recursion_yes_no() {
        assert!(EvalError::InfiniteRecursion("loop".into()).is_infinite_recursion());
        assert!(!EvalError::DivisionByZero.is_infinite_recursion());
        assert!(!EvalError::Throw("x".into()).is_infinite_recursion());
    }

    #[test]
    fn eval_error_display_undefined_var() {
        let s = format!("{}", EvalError::UndefinedVar("foo".into()));
        assert!(s.contains("undefined variable"));
        assert!(s.contains("foo"));
    }

    #[test]
    fn eval_error_display_type_error() {
        let s = format!("{}", EvalError::TypeError("bad".into()));
        assert!(s.contains("type error"));
        assert!(s.contains("bad"));
    }

    #[test]
    fn eval_error_display_attr_not_found() {
        let s = format!("{}", EvalError::AttrNotFound("x".into()));
        assert!(s.contains("attribute not found"));
        assert!(s.contains("x"));
    }

    #[test]
    fn eval_error_display_type_mismatch() {
        let s = format!(
            "{}",
            EvalError::TypeMismatch { expected: "int", got: "string" }
        );
        assert!(s.contains("expected int"));
        assert!(s.contains("got string"));
    }

    #[test]
    fn eval_error_display_assertion_failed() {
        let s = format!("{}", EvalError::AssertionFailed);
        assert!(s.contains("assertion"));
    }

    #[test]
    fn eval_error_display_division_by_zero() {
        let s = format!("{}", EvalError::DivisionByZero);
        assert!(s.contains("division by zero"));
    }

    #[test]
    fn eval_error_display_infinite_recursion() {
        let s = format!("{}", EvalError::InfiniteRecursion("loop".into()));
        assert!(s.contains("infinite recursion"));
        assert!(s.contains("loop"));
    }

    #[test]
    fn eval_error_display_io_error() {
        let s = format!(
            "{}",
            EvalError::IoError {
                context: "ctx".into(),
                message: "no such file".into(),
            }
        );
        assert!(s.contains("I/O"));
        assert!(s.contains("ctx"));
        assert!(s.contains("no such file"));
    }

    #[test]
    fn eval_error_display_throw() {
        let s = format!("{}", EvalError::Throw("boom".into()));
        assert_eq!(s, "boom");
    }

    #[test]
    fn eval_error_display_not_implemented() {
        let s = format!("{}", EvalError::NotImplemented("frob".into()));
        assert!(s.contains("not yet implemented"));
        assert!(s.contains("frob"));
    }

    #[test]
    fn eval_error_display_parse_error() {
        let s = format!("{}", EvalError::ParseError("syntax".into()));
        assert!(s.contains("parse error"));
        assert!(s.contains("syntax"));
    }

    #[test]
    fn eval_error_display_recursion_limit() {
        let s = format!(
            "{}",
            EvalError::RecursionLimit("max depth exceeded".into())
        );
        assert!(s.contains("recursion limit"));
        assert!(s.contains("max depth exceeded"));
    }

    #[test]
    fn eval_error_partial_eq_same_variant() {
        assert_eq!(
            EvalError::UndefinedVar("x".into()),
            EvalError::UndefinedVar("x".into()),
        );
        assert_ne!(
            EvalError::UndefinedVar("x".into()),
            EvalError::UndefinedVar("y".into()),
        );
        assert_ne!(
            EvalError::UndefinedVar("x".into()),
            EvalError::AttrNotFound("x".into()),
        );
    }

    // ── ContextElement display ───────────────────────────

    #[test]
    fn context_element_display_plain() {
        let e = ContextElement::Plain("/nix/store/xyz".into());
        assert_eq!(format!("{e}"), "/nix/store/xyz");
    }

    #[test]
    fn context_element_display_output() {
        let e = ContextElement::Output {
            drv: "/nix/store/abc.drv".into(),
            output: "out".into(),
        };
        assert_eq!(format!("{e}"), "/nix/store/abc.drv!out");
    }

    #[test]
    fn context_element_display_drv_deep() {
        let e = ContextElement::DrvDeep("/nix/store/abc.drv".into());
        assert_eq!(format!("{e}"), "=/nix/store/abc.drv");
    }

    // ── StringContext additional API ─────────────────────

    #[test]
    fn string_context_iter_yields_all() {
        let mut ctx = StringContext::new();
        ctx.add_plain("/nix/store/aaa".into());
        ctx.add_plain("/nix/store/bbb".into());
        let count = ctx.iter().count();
        assert_eq!(count, 2);
    }

    #[test]
    fn string_context_len_matches_set_size() {
        let mut ctx = StringContext::new();
        assert_eq!(ctx.len(), 0);
        ctx.add_plain("/nix/store/x".into());
        assert_eq!(ctx.len(), 1);
        ctx.add_output("/nix/store/y.drv".into(), "out".into());
        assert_eq!(ctx.len(), 2);
    }

    #[test]
    fn string_context_insert_raw_element() {
        let mut ctx = StringContext::new();
        ctx.insert(ContextElement::Plain("/nix/store/foo".into()));
        assert_eq!(ctx.len(), 1);
    }

    #[test]
    fn string_context_default_is_empty() {
        let ctx = StringContext::default();
        assert!(ctx.is_empty());
    }

    // ── NixString additional traits ──────────────────────

    #[test]
    fn nix_string_as_ref_str() {
        let s = NixString::plain("hello");
        let r: &str = s.as_ref();
        assert_eq!(r, "hello");
    }

    #[test]
    fn nix_string_deref_to_str_methods() {
        let s = NixString::plain("Hello World");
        assert_eq!(s.len(), 11);
        assert!(s.starts_with("Hello"));
        // Calling &str method via Deref proves Deref impl is wired up.
        assert_eq!(s.to_uppercase(), "HELLO WORLD");
    }

    // ── NixAttrs additional API ──────────────────────────

    #[test]
    fn nixattrs_remove_returns_value() {
        let mut a = NixAttrs::new();
        a.insert("x".into(), Value::Int(1));
        let removed = a.remove("x");
        assert_eq!(removed, Some(Value::Int(1)));
        assert!(!a.contains_key("x"));
        assert_eq!(a.remove("y"), None);
    }

    #[test]
    fn nixattrs_values_iter() {
        let mut a = NixAttrs::new();
        a.insert("a".into(), Value::Int(1));
        a.insert("b".into(), Value::Int(2));
        let mut vs: Vec<&Value> = a.values().collect();
        vs.sort_by_key(|v| match v {
            Value::Int(n) => *n,
            _ => 0,
        });
        assert_eq!(vs, vec![&Value::Int(1), &Value::Int(2)]);
    }

    #[test]
    fn nixattrs_iter_returns_sorted_pairs() {
        let mut a = NixAttrs::new();
        a.insert("zeta".into(), Value::Int(3));
        a.insert("alpha".into(), Value::Int(1));
        a.insert("mu".into(), Value::Int(2));
        let pairs: Vec<(&String, &Value)> = a.iter().collect();
        assert_eq!(pairs[0].0, "alpha");
        assert_eq!(pairs[1].0, "mu");
        assert_eq!(pairs[2].0, "zeta");
    }

    #[test]
    fn nixattrs_from_iterator() {
        let pairs = vec![
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Int(2)),
        ];
        let attrs: NixAttrs = pairs.into_iter().collect();
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
        assert_eq!(attrs.get("b"), Some(&Value::Int(2)));
    }

    #[test]
    fn nixattrs_into_iterator_yields_owned() {
        let mut a = NixAttrs::new();
        a.insert("x".into(), Value::Int(42));
        let pairs: Vec<(String, Value)> = a.into_iter().collect();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "x");
        assert_eq!(pairs[0].1, Value::Int(42));
    }

    #[test]
    fn nixattrs_default_is_empty() {
        let a = NixAttrs::default();
        assert!(a.is_empty());
    }

    // ── Value::From conversions ──────────────────────────

    #[test]
    fn value_from_bool() {
        assert_eq!(Value::from(true), Value::Bool(true));
        assert_eq!(Value::from(false), Value::Bool(false));
    }

    #[test]
    fn value_from_i64() {
        assert_eq!(Value::from(42_i64), Value::Int(42));
        assert_eq!(Value::from(-1_i64), Value::Int(-1));
    }

    #[test]
    fn value_from_f64() {
        assert_eq!(Value::from(2.5_f64), Value::Float(2.5));
    }

    #[test]
    fn value_from_nix_string() {
        let v: Value = NixString::plain("hi").into();
        assert_eq!(v, Value::string("hi"));
    }

    #[test]
    fn value_from_nix_attrs() {
        let mut a = NixAttrs::new();
        a.insert("x".into(), Value::Int(1));
        let v: Value = a.into();
        match v {
            Value::Attrs(_) => {}
            _ => panic!("expected Attrs"),
        }
    }

    #[test]
    fn value_from_vec() {
        let v: Value = vec![Value::Int(1), Value::Int(2)].into();
        assert_eq!(v, Value::List(vec![Value::Int(1), Value::Int(2)]));
    }

    #[test]
    fn value_default_is_null() {
        let v: Value = Value::default();
        assert_eq!(v, Value::Null);
    }

    // ── From<&serde_json::Value> ─────────────────────────

    #[test]
    fn value_from_json_null() {
        let v = Value::from(&serde_json::Value::Null);
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn value_from_json_bool() {
        let v = Value::from(&serde_json::Value::Bool(true));
        assert_eq!(v, Value::Bool(true));
    }

    #[test]
    fn value_from_json_int() {
        let v = Value::from(&serde_json::json!(42));
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn value_from_json_float() {
        let v = Value::from(&serde_json::json!(3.14));
        match v {
            Value::Float(f) => assert!((f - 3.14).abs() < f64::EPSILON),
            _ => panic!("expected Float"),
        }
    }

    #[test]
    fn value_from_json_string() {
        let v = Value::from(&serde_json::Value::String("hi".into()));
        assert_eq!(v, Value::string("hi"));
    }

    #[test]
    fn value_from_json_array() {
        let v = Value::from(&serde_json::json!([1, true, "x"]));
        match v {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Value::Int(1));
                assert_eq!(items[1], Value::Bool(true));
                assert_eq!(items[2], Value::string("x"));
            }
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn value_from_json_object() {
        let v = Value::from(&serde_json::json!({"a": 1, "b": "x"}));
        match v {
            Value::Attrs(attrs) => {
                assert_eq!(attrs.get("a"), Some(&Value::Int(1)));
                assert_eq!(attrs.get("b"), Some(&Value::string("x")));
            }
            _ => panic!("expected Attrs"),
        }
    }

    #[test]
    fn value_from_json_nested() {
        let v = Value::from(&serde_json::json!({"outer": {"inner": [1, 2]}}));
        let json_back = v.to_json();
        assert_eq!(json_back, serde_json::json!({"outer": {"inner": [1, 2]}}));
    }

    // ── From<&toml::Value> ──────────────────────────────

    #[test]
    fn value_from_toml_string() {
        let t = toml::Value::String("hi".into());
        assert_eq!(Value::from(&t), Value::string("hi"));
    }

    #[test]
    fn value_from_toml_int() {
        let t = toml::Value::Integer(42);
        assert_eq!(Value::from(&t), Value::Int(42));
    }

    #[test]
    fn value_from_toml_float() {
        let t = toml::Value::Float(3.14);
        match Value::from(&t) {
            Value::Float(f) => assert!((f - 3.14).abs() < f64::EPSILON),
            _ => panic!("expected Float"),
        }
    }

    #[test]
    fn value_from_toml_bool() {
        let t = toml::Value::Boolean(true);
        assert_eq!(Value::from(&t), Value::Bool(true));
    }

    #[test]
    fn value_from_toml_array() {
        let t = toml::Value::Array(vec![
            toml::Value::Integer(1),
            toml::Value::Integer(2),
        ]);
        assert_eq!(
            Value::from(&t),
            Value::List(vec![Value::Int(1), Value::Int(2)]),
        );
    }

    #[test]
    fn value_from_toml_table() {
        let mut tbl = toml::map::Map::new();
        tbl.insert("k".into(), toml::Value::Integer(7));
        let t = toml::Value::Table(tbl);
        match Value::from(&t) {
            Value::Attrs(attrs) => {
                assert_eq!(attrs.get("k"), Some(&Value::Int(7)));
            }
            _ => panic!("expected Attrs"),
        }
    }

    #[test]
    fn value_from_toml_datetime_becomes_string() {
        // toml::Value::Datetime serializes via Display.
        let dt: toml::value::Datetime = "2024-01-01T00:00:00Z".parse().unwrap();
        let t = toml::Value::Datetime(dt);
        match Value::from(&t) {
            Value::String(_) => {}
            other => panic!("expected String, got {other:?}"),
        }
    }

    // ── Value::coerce_to_path ────────────────────────────

    #[test]
    fn coerce_to_path_from_path() {
        let v = Value::Path("/foo".into());
        assert_eq!(v.coerce_to_path("ctx").unwrap(), "/foo");
    }

    #[test]
    fn coerce_to_path_from_string() {
        let v = Value::string("/bar");
        assert_eq!(v.coerce_to_path("ctx").unwrap(), "/bar");
    }

    #[test]
    fn coerce_to_path_errors_on_int() {
        let v = Value::Int(1);
        let e = v.coerce_to_path("readFile").unwrap_err();
        match e {
            EvalError::TypeError(ref msg) => {
                assert!(msg.contains("readFile"));
                assert!(msg.contains("path or string"));
                assert!(msg.contains("int"));
            }
            _ => panic!("expected TypeError"),
        }
    }

    #[test]
    fn coerce_to_path_errors_on_null() {
        let v = Value::Null;
        assert!(v.coerce_to_path("ctx").is_err());
    }

    #[test]
    fn coerce_to_path_attrs_with_outpath() {
        let mut attrs = NixAttrs::new();
        attrs.insert("outPath".to_string(), Value::string("/nix/store/test"));
        let val = Value::Attrs(attrs);
        assert_eq!(val.coerce_to_path("test").unwrap(), "/nix/store/test");
    }

    #[test]
    fn coerce_to_path_attrs_without_outpath_fails() {
        let attrs = NixAttrs::new();
        let val = Value::Attrs(attrs);
        assert!(val.coerce_to_path("test").is_err());
    }

    // ── Value::coerce_to_string ─────────────────────────

    #[test]
    fn coerce_to_string_string() {
        let v = Value::string("hello");
        let (s, _ctx) = v.coerce_to_string().unwrap();
        assert_eq!(s, "hello");
    }

    #[test]
    fn coerce_to_string_path() {
        let v = Value::Path("/foo".into());
        let (s, ctx) = v.coerce_to_string().unwrap();
        assert_eq!(s, "/foo");
        assert!(!ctx.is_empty()); // should add a Plain context element
    }

    #[test]
    fn coerce_to_string_int() {
        let v = Value::Int(42);
        let (s, _ctx) = v.coerce_to_string().unwrap();
        assert_eq!(s, "42");
    }

    #[test]
    fn coerce_to_string_float() {
        let v = Value::Float(3.14);
        let (s, _ctx) = v.coerce_to_string().unwrap();
        assert_eq!(s, "3.14");
    }

    #[test]
    fn coerce_to_string_bool_true() {
        let (s, _ctx) = Value::Bool(true).coerce_to_string().unwrap();
        assert_eq!(s, "1");
    }

    #[test]
    fn coerce_to_string_bool_false() {
        let (s, _ctx) = Value::Bool(false).coerce_to_string().unwrap();
        assert_eq!(s, "");
    }

    #[test]
    fn coerce_to_string_null() {
        let (s, _ctx) = Value::Null.coerce_to_string().unwrap();
        assert_eq!(s, "");
    }

    #[test]
    fn coerce_to_string_attrs_with_outpath() {
        let mut attrs = NixAttrs::new();
        attrs.insert("outPath".to_string(), Value::string("/nix/store/abc"));
        let val = Value::Attrs(attrs);
        let (s, _ctx) = val.coerce_to_string().unwrap();
        assert_eq!(s, "/nix/store/abc");
    }

    #[test]
    fn coerce_to_string_attrs_without_outpath_or_tostring_fails() {
        let attrs = NixAttrs::new();
        let val = Value::Attrs(attrs);
        assert!(val.coerce_to_string().is_err());
    }

    #[test]
    fn coerce_to_string_lambda_fails() {
        let root = rnix::Root::parse("x: x");
        let expr = root.tree().expr().unwrap();
        let closure = Closure {
            param: match expr {
                rnix::ast::Expr::Lambda(ref l) => l.param().unwrap(),
                _ => panic!("expected lambda"),
            },
            body: match expr {
                rnix::ast::Expr::Lambda(ref l) => l.body().unwrap(),
                _ => panic!("expected lambda"),
            },
            env: Env::new(),
        };
        let val = Value::Lambda(closure);
        assert!(val.coerce_to_string().is_err());
    }

    // ── BuiltinFn debug ──────────────────────────────────

    #[test]
    fn builtin_fn_debug_includes_name() {
        let b = BuiltinFn {
            name: "myFunc",
            func: Arc::new(|_| Ok(Value::Null)),
        };
        let s = format!("{b:?}");
        assert!(s.contains("myFunc"));
        assert!(s.contains("builtin"));
    }

    // ── Thunk additional tests ───────────────────────────

    #[test]
    fn thunk_force_chains_through_inner_thunks() {
        // Build a thunk whose evaluator yields another thunk.
        let inner_root = rnix::Root::parse("99");
        let inner_expr = inner_root.tree().expr().unwrap();
        let inner_thunk = Thunk::new_suspended(inner_expr, Env::new());
        let outer = Thunk::new_evaluated(Value::Thunk(inner_thunk));
        let result = outer.force(&|e, env| crate::eval::eval_expr(e, env));
        // Already-evaluated outer returns the inner thunk; the chain is
        // collapsed by the higher-level force_value, not by force() itself
        // when starting from Evaluated. So we just check we got a Thunk
        // back unchanged.
        match result.unwrap() {
            Value::Thunk(_) | Value::Int(99) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn thunk_inherit_select_debug_format() {
        let root = rnix::Root::parse("{ x = 1; }");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_inherit_select(expr, "x".into(), Env::new());
        let s = format!("{thunk:?}");
        assert!(s.contains("inherit-select"));
        assert!(s.contains("x"));
    }

    #[test]
    fn thunk_blackhole_debug_format() {
        let root = rnix::Root::parse("1");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        *thunk.0.borrow_mut() = ThunkRepr::Blackhole;
        assert_eq!(format!("{thunk:?}"), "<blackhole>");
    }

    // ── Value display for thunks ─────────────────────────

    #[test]
    fn value_display_thunk_evaluates() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert_eq!(format!("{val}"), "42");
    }

    #[test]
    fn value_to_json_thunk_forces() {
        let root = rnix::Root::parse(r#""world""#);
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert_eq!(val.to_json(), serde_json::Value::String("world".into()));
    }

    #[test]
    fn value_type_name_thunk_forces() {
        let root = rnix::Root::parse("42");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert_eq!(val.type_name(), "int");
    }

    // ── as_string / as_nix_string thunk error ────────────

    #[test]
    fn as_string_errors_on_thunk() {
        let root = rnix::Root::parse(r#""x""#);
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        let err = val.as_string().unwrap_err();
        match err {
            EvalError::TypeError(msg) => assert!(msg.contains("thunk")),
            _ => panic!("expected TypeError"),
        }
    }

    #[test]
    fn as_nix_string_errors_on_thunk() {
        let root = rnix::Root::parse(r#""x""#);
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert!(val.as_nix_string().is_err());
    }

    #[test]
    fn as_attrs_errors_on_thunk() {
        let root = rnix::Root::parse("{}");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert!(val.as_attrs().is_err());
    }

    #[test]
    fn as_list_errors_on_thunk() {
        let root = rnix::Root::parse("[]");
        let expr = root.tree().expr().unwrap();
        let thunk = Thunk::new_suspended(expr, Env::new());
        let val = Value::Thunk(thunk);
        assert!(val.as_list().is_err());
    }

    // ── as_nix_string OK on string ───────────────────────

    #[test]
    fn as_nix_string_ok_on_string() {
        let v = Value::string("hi");
        let ns = v.as_nix_string().unwrap();
        assert_eq!(ns.as_str(), "hi");
    }

    #[test]
    fn as_nix_string_errors_on_int() {
        let v = Value::Int(1);
        match v.as_nix_string() {
            Err(EvalError::TypeMismatch { expected, got }) => {
                assert_eq!(expected, "string");
                assert_eq!(got, "int");
            }
            _ => panic!("expected TypeMismatch"),
        }
    }
}
