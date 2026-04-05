//! Nix value types and environments.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// A Nix value.
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Path(String),
    List(Vec<Value>),
    Attrs(NixAttrs),
    Lambda(Closure),
    Builtin(BuiltinFn),
}

/// A Nix attribute set.
#[derive(Debug, Clone)]
pub struct NixAttrs(pub BTreeMap<String, Value>);

impl NixAttrs {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn insert(&mut self, key: String, value: Value) {
        self.0.insert(key, value);
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Merge two attrsets (right overrides left, like `//`).
    pub fn update(&self, other: &NixAttrs) -> NixAttrs {
        let mut result = self.0.clone();
        for (k, v) in &other.0 {
            result.insert(k.clone(), v.clone());
        }
        NixAttrs(result)
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

/// A builtin function.
///
/// Not `Send`/`Sync` because `Value` contains rnix AST nodes (rowan `SyntaxNode`)
/// which use `NonNull` internally. The evaluator is single-threaded.
#[derive(Clone)]
pub struct BuiltinFn {
    pub name: &'static str,
    pub func: Arc<dyn Fn(&[Value]) -> Result<Value, EvalError>>,
}

impl fmt::Debug for BuiltinFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<builtin {}>", self.name)
    }
}

/// Evaluation environment — lexical scope chain.
#[derive(Debug, Clone)]
pub struct Env {
    bindings: BTreeMap<String, Value>,
    parent: Option<Arc<Env>>,
    /// Dynamic scope from `with` expressions.
    with_scope: Option<Arc<NixAttrs>>,
}

impl Env {
    pub fn new() -> Self {
        Self {
            bindings: BTreeMap::new(),
            parent: None,
            with_scope: None,
        }
    }

    pub fn child(&self) -> Self {
        Self {
            bindings: BTreeMap::new(),
            parent: Some(Arc::new(self.clone())),
            with_scope: None,
        }
    }

    pub fn with_scope(mut self, attrs: NixAttrs) -> Self {
        self.with_scope = Some(Arc::new(attrs));
        self
    }

    pub fn bind(&mut self, name: String, value: Value) {
        self.bindings.insert(name, value);
    }

    pub fn lookup(&self, name: &str) -> Option<Value> {
        // Check local bindings first
        if let Some(v) = self.bindings.get(name) {
            return Some(v.clone());
        }
        // Check parent BEFORE `with` scope — lexical scope takes precedence.
        // This matches Nix semantics: `with` only provides names not already
        // bound in the lexical scope chain.
        if let Some(ref parent) = self.parent {
            if let Some(v) = parent.lookup(name) {
                return Some(v);
            }
        }
        // `with` scope is the last resort before giving up.
        if let Some(ref attrs) = self.with_scope {
            if let Some(v) = attrs.get(name) {
                return Some(v.clone());
            }
        }
        None
    }
}

/// Evaluation errors.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("undefined variable: {0}")]
    UndefinedVar(String),
    #[error("type error: {0}")]
    TypeError(String),
    #[error("attribute not found: {0}")]
    AttrNotFound(String),
    #[error("assertion failed")]
    AssertionFailed,
    #[error("division by zero")]
    DivisionByZero,
    #[error("not yet implemented: {0}")]
    NotImplemented(String),
    #[error("parse error: {0}")]
    ParseError(String),
}

impl Value {
    /// Convert a value to JSON for API output.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(n) => serde_json::json!(n),
            Value::Float(f) => serde_json::json!(f),
            Value::String(s) => serde_json::Value::String(s.clone()),
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
        }
    }

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
        }
    }

    pub fn as_bool(&self) -> Result<bool, EvalError> {
        match self {
            Value::Bool(b) => Ok(*b),
            _ => Err(EvalError::TypeError(format!("expected bool, got {}", self.type_name()))),
        }
    }

    pub fn as_int(&self) -> Result<i64, EvalError> {
        match self {
            Value::Int(n) => Ok(*n),
            _ => Err(EvalError::TypeError(format!("expected int, got {}", self.type_name()))),
        }
    }

    pub fn as_string(&self) -> Result<&str, EvalError> {
        match self {
            Value::String(s) => Ok(s),
            _ => Err(EvalError::TypeError(format!("expected string, got {}", self.type_name()))),
        }
    }

    pub fn as_attrs(&self) -> Result<&NixAttrs, EvalError> {
        match self {
            Value::Attrs(a) => Ok(a),
            _ => Err(EvalError::TypeError(format!("expected set, got {}", self.type_name()))),
        }
    }

    pub fn as_list(&self) -> Result<&[Value], EvalError> {
        match self {
            Value::List(l) => Ok(l),
            _ => Err(EvalError::TypeError(format!("expected list, got {}", self.type_name()))),
        }
    }

    /// Coerce a numeric value to float.
    pub fn as_float(&self) -> Result<f64, EvalError> {
        match self {
            Value::Float(f) => Ok(*f),
            Value::Int(n) => Ok(*n as f64),
            _ => Err(EvalError::TypeError(format!("expected number, got {}", self.type_name()))),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => (*a as f64) == *b,
            (Value::String(a), Value::String(b)) => a == b,
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
            Value::String(s) => write!(f, "\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
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
            Value::String("hello".to_string()).to_json(),
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
    fn type_name_string() { assert_eq!(Value::String("".to_string()).type_name(), "string"); }

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
        assert!(Value::String("true".to_string()).as_bool().is_err());
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
        assert!(Value::String("x".to_string()).as_float().is_err());
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
        assert_ne!(Value::Int(1), Value::String("1".to_string()));
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
        assert_eq!(format!("{}", Value::String("hi".to_string())), "\"hi\"");
    }

    #[test]
    fn display_string_with_escapes() {
        let v = Value::String("a\"b\\c".to_string());
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
}
